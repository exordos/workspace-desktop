//! Отправляет HTTPS-запросы к IAM Workspace и проверяет доступ к мессенджеру.
//! Ниже описаны запросы, разбор ответов и тесты форматов сервера.

use std::{
    collections::HashSet,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use reqwest::{
    StatusCode, Url,
    blocking::{Client, RequestBuilder, Response},
    header::{ACCEPT, HeaderValue},
    redirect::Policy,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::{IamClient, IamError, IamProject, IamToken},
    context::OwnerContext,
};

const TOKEN_PATH: &str = "/api/core/v1/iam/clients/default/actions/get_token/invoke";
const PROJECTS_PATH: &str = "/api/core/v1/iam/projects/";
const INTROSPECTION_PATH: &str = "/api/core/v1/iam/clients/default/actions/introspect";
const STREAMS_PATH: &str = "/api/workspace/v1/messenger/streams/";
const BASE_SCOPE: &str = "openid email profile";

/// Клиент Workspace IAM для входа, выбора проекта и продления сессии.
#[derive(Default)]
pub struct WorkspaceIamHttp {
    client: OnceLock<Client>, // Общий HTTP-клиент, который создаётся при первом запросе.
}

impl WorkspaceIamHttp {
    /// Создаёт адаптер IAM. HTTP-клиент появится при первом обращении.
    pub fn new() -> Self {
        Self::default()
    }

    /// Создаёт или возвращает общий HTTPS-клиент с тайм-аутами и запретом перенаправлений.
    fn client(&self) -> Result<&Client, IamError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let client = Client::builder()
            .https_only(true)
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| IamError::Temporary)?;
        Ok(self.client.get_or_init(|| client))
    }

    /// Проверяет, может ли токен читать список каналов выбранного проекта.
    pub fn verify_messenger(
        &self,
        context: &OwnerContext,
        access_token: &str,
    ) -> Result<(), IamError> {
        let response = send(self.messenger_request(context, access_token)?)?;
        if !response.status().is_success() {
            return Err(request_error(response.status()));
        }
        collection(response_json(response)?)?;
        Ok(())
    }

    /// Собирает запрос одного канала с токеном и идентификатором проекта.
    fn messenger_request(
        &self,
        context: &OwnerContext,
        access_token: &str,
    ) -> Result<RequestBuilder, IamError> {
        let mut endpoint = endpoint(&context.server, STREAMS_PATH)?;
        endpoint
            .query_pairs_mut()
            .append_pair("page_limit", "1")
            .append_pair("project_id", &context.project_id);
        Ok(self
            .client()?
            .get(endpoint)
            .header(ACCEPT, "application/json")
            .bearer_auth(access_token))
    }

    /// Собирает запрос первого токена по логину и паролю, при необходимости добавляя одноразовый код.
    fn login_request(
        &self,
        server: &str,
        login: &str,
        password: &str,
        otp: Option<&str>,
    ) -> Result<RequestBuilder, IamError> {
        let request = self
            .client()?
            .post(endpoint(server, TOKEN_PATH)?)
            .header(ACCEPT, "application/json")
            .json(&json!({
                "grant_type": "login+password",
                "login": login.trim(),
                "password": password,
                "scope": BASE_SCOPE,
                "ttl": 3600,
                "refresh_ttl": 30 * 24 * 60 * 60,
            }));
        if let Some(otp) = otp.map(str::trim).filter(|otp| !otp.is_empty()) {
            let mut value = HeaderValue::from_str(otp).map_err(|_| IamError::InvalidCredentials)?;
            value.set_sensitive(true);
            return Ok(request.header("X-OTP", value));
        }
        Ok(request)
    }

    /// Собирает запрос обновления токена. Если указан проект, добавляет его в область доступа.
    fn refresh_request(
        &self,
        server: &str,
        refresh_token: &str,
        project: Option<&str>,
    ) -> Result<RequestBuilder, IamError> {
        let mut body = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        });
        if let Some(project) = project {
            body["scope"] = json!(format!("{BASE_SCOPE} project:{project}"));
        }
        Ok(self
            .client()?
            .post(endpoint(server, TOKEN_PATH)?)
            .header(ACCEPT, "application/json")
            .json(&body))
    }

    /// Выполняет обмен токенов, определяет владельца и вычисляет оставшийся срок действия.
    fn grant(
        &self,
        server: &str,
        request: RequestBuilder,
        kind: Grant,
    ) -> Result<IamToken, IamError> {
        let response = send(request)?;
        let status = response.status();
        if !status.is_success() {
            let detail = if [400, 401, 403].contains(&status.as_u16()) {
                response
                    .text()
                    .ok()
                    .map(|body| serde_json::from_str(&body).unwrap_or(Value::String(body)))
            } else {
                None
            };
            return Err(grant_error(status, detail.as_ref(), kind));
        }

        let body: TokenBody = response_json(response)?;
        let received_at = SystemTime::now();
        let mut identity = token_identity(&body.access_token);
        if !is_nonempty(&body.access_token) {
            return Err(IamError::InvalidResponse);
        }
        if !identity.has_user() || kind == Grant::Refresh && identity.project_id.is_none() {
            let authenticated = self.introspect(server, &body.access_token)?;
            identity.merge_introspection(authenticated)?;
        }
        let expires_in = token_lifetime(&body, &identity, received_at, SystemTime::now())?;

        Ok(IamToken {
            access_token: body.access_token,
            refresh_token: nonempty(body.refresh_token),
            expires_in,
            user_id: identity.user_id().ok_or(IamError::InvalidResponse)?,
            project_id: identity.project_id,
            organization_id: identity.organization_id,
        })
    }

    /// Запрашивает у IAM подтверждённые данные о владельце и области действия токена.
    fn introspect(&self, server: &str, access_token: &str) -> Result<Introspection, IamError> {
        let request = self
            .client()?
            .get(endpoint(server, INTROSPECTION_PATH)?)
            .header(ACCEPT, "application/json")
            .bearer_auth(access_token);
        let response = send(request)?;
        if !response.status().is_success() {
            return Err(request_error(response.status()));
        }
        response_json(response)
    }
}

impl IamClient for WorkspaceIamHttp {
    /// Возвращает адрес сервера в едином виде, пригодном для запросов и сравнения.
    fn normalize_server(&self, server: &str) -> Result<String, IamError> {
        Ok(server_origin(server)?.origin().ascii_serialization())
    }

    /// Получает первый токен с помощью логина, пароля и, если нужен, одноразового кода.
    fn login(
        &self,
        server: &str,
        login: &str,
        password: &str,
        otp: Option<&str>,
    ) -> Result<IamToken, IamError> {
        let request = self.login_request(server, login, password, otp)?;
        self.grant(server, request, Grant::Login)
    }

    /// Читает доступные проекты. Ответ без содержимого означает пустой список.
    fn projects(&self, server: &str, access_token: &str) -> Result<Vec<IamProject>, IamError> {
        let response = send(
            self.client()?
                .get(endpoint(server, PROJECTS_PATH)?)
                .header(ACCEPT, "application/json")
                .bearer_auth(access_token),
        )?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(Vec::new());
        }
        if !response.status().is_success() {
            return Err(request_error(response.status()));
        }
        parse_projects(response_json(response)?)
    }

    /// Получает токен, привязанный к выбранному проекту.
    fn select_project(
        &self,
        server: &str,
        refresh_token: &str,
        project_id: &str,
    ) -> Result<IamToken, IamError> {
        let request = self.refresh_request(server, refresh_token, Some(project_id))?;
        self.grant(server, request, Grant::Refresh)
    }

    /// Продлевает токен без запроса новой области доступа.
    fn refresh(&self, server: &str, refresh_token: &str) -> Result<IamToken, IamError> {
        let request = self.refresh_request(server, refresh_token, None)?;
        self.grant(server, request, Grant::Refresh)
    }
}

/// Проверяет адрес и оставляет схему HTTPS, имя сервера и порт.
fn server_origin(server: &str) -> Result<Url, IamError> {
    let server = server.trim();
    if server.is_empty() || server.contains(|ch: char| ch.is_whitespace() || ch == '\\') {
        return Err(IamError::InvalidServer);
    }

    let absolute = match server.split_once("://") {
        Some((scheme, _)) if scheme.eq_ignore_ascii_case("https") => server.to_owned(),
        Some(_) => return Err(IamError::InvalidServer),
        None => format!("https://{server}"),
    };
    let authority = absolute
        .split_once("://")
        .ok_or(IamError::InvalidServer)?
        .1
        .split(['/', '#', '?'])
        .next()
        .ok_or(IamError::InvalidServer)?;
    if authority.is_empty() || authority.ends_with(':') || authority.contains('@') {
        return Err(IamError::InvalidServer);
    }

    let url = Url::parse(&absolute).map_err(|_| IamError::InvalidServer)?;
    if url.host().is_none() {
        return Err(IamError::InvalidServer);
    }
    Url::parse(&url.origin().ascii_serialization()).map_err(|_| IamError::InvalidServer)
}

/// Подставляет путь API в проверенный адрес сервера.
fn endpoint(server: &str, path: &str) -> Result<Url, IamError> {
    let mut url = server_origin(server)?;
    url.set_path(path);
    Ok(url)
}

/// Отправляет запрос и переводит ошибку транспорта или сборки запроса в ошибку IAM.
fn send(request: RequestBuilder) -> Result<Response, IamError> {
    request.send().map_err(|error| {
        if error.is_builder() {
            IamError::InvalidResponse
        } else {
            IamError::Temporary
        }
    })
}

/// Читает JSON и различает сбой чтения и неверный формат ответа.
fn response_json<T: serde::de::DeserializeOwned>(response: Response) -> Result<T, IamError> {
    response.json().map_err(|error| {
        if error.is_timeout() || error.is_body() {
            IamError::Temporary
        } else {
            IamError::InvalidResponse
        }
    })
}

/// Определяет общую ошибку запроса по HTTP-статусу.
fn request_error(status: StatusCode) -> IamError {
    if status.is_server_error() || [408, 429].contains(&status.as_u16()) {
        IamError::Temporary
    } else {
        IamError::InvalidResponse
    }
}

/// Вид запроса токена, от которого зависит трактовка ошибок IAM.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grant {
    Login,   // Первый токен по данным пользователя.
    Refresh, // Получение токена по токену обновления, в том числе при выборе проекта.
}

/// Определяет по статусу и деталям ответа, нужен ли одноразовый код, повтор или новый вход.
fn grant_error(status: StatusCode, detail: Option<&Value>, kind: Grant) -> IamError {
    const FIELDS: &[&str] = &[
        "error",
        "error_description",
        "error_code",
        "errorCode",
        "code",
        "message",
        "detail",
        "reason",
        "type",
    ];
    const TERMINAL_REFRESH: &[&str] = &[
        "invalid_grant",
        "invalid_token",
        "expired_token",
        "invalid_refresh_token",
        "refresh_token_expired",
        "refresh_token_invalid",
        "refresh_token_revoked",
        "revoked_token",
        "token_expired",
        "token_revoked",
        "revoked",
    ];

    let texts = detail.into_iter().flat_map(|value| {
        value.as_str().into_iter().chain(
            FIELDS
                .iter()
                .filter_map(move |field| value[*field].as_str()),
        )
    });
    let markers: Vec<String> = texts
        .map(|text| {
            let normalized = text
                .split(|ch: char| !ch.is_ascii_alphanumeric())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("_")
                .to_ascii_lowercase();
            format!("_{normalized}_")
        })
        .collect();
    let contains = |expected: &str| {
        let needle = format!("_{expected}_");
        markers.iter().any(|marker| marker.contains(&needle))
    };

    match (kind, status.as_u16()) {
        (Grant::Login, 400 | 401 | 403) if contains("otp") => IamError::OtpRequired,
        (Grant::Login, 401)
            if !contains("invalid_credentials")
                && !contains("invalid_grant")
                && !contains("invalid_password") =>
        {
            IamError::OtpRequired
        }
        (Grant::Login, 400 | 401 | 403) => IamError::InvalidCredentials,
        (Grant::Refresh, 400 | 401 | 403)
            if TERMINAL_REFRESH.iter().any(|marker| contains(marker)) =>
        {
            IamError::ReauthenticationRequired
        }
        (Grant::Refresh, 400 | 401 | 403) => IamError::Temporary,
        _ => request_error(status),
    }
}

/// Поля JSON-ответа IAM на запрос токена.
#[derive(Deserialize)]
struct TokenBody {
    access_token: String,          // Токен доступа, который сервер только что выдал.
    refresh_token: Option<String>, // Новый токен обновления, если сервер его прислал.
    expires_in: Option<u64>, // Срок действия в секундах от получения ответа, если сервер его указал.
}

/// Сведения о владельце и сроке действия из полезной нагрузки токена.
#[derive(Deserialize, Default)]
struct TokenIdentity {
    #[serde(alias = "userUuid")]
    user_uuid: Option<String>, // Основной идентификатор пользователя, если есть.
    sub: Option<String>, // Запасной идентификатор пользователя из поля subject.
    #[serde(alias = "projectId")]
    project_id: Option<String>, // Идентификатор проекта, если токен к нему привязан.
    #[serde(alias = "organizationId")]
    organization_id: Option<String>, // Идентификатор организации, если он указан в токене.
    exp: Option<u64>,    // Момент истечения в секундах с начала Unix-эпохи, если он указан.
}

impl TokenIdentity {
    /// Сообщает, есть ли хотя бы один идентификатор пользователя.
    fn has_user(&self) -> bool {
        self.user_uuid.is_some() || self.sub.is_some()
    }

    /// Возвращает основной идентификатор пользователя или запасной subject.
    fn user_id(&self) -> Option<String> {
        self.user_uuid.clone().or_else(|| self.sub.clone())
    }

    /// Дополняет данные токена сведениями от IAM. При расхождении возвращает ошибку.
    fn merge_introspection(&mut self, introspection: Introspection) -> Result<(), IamError> {
        let supplied_user = nonempty(Some(introspection.user_info.uuid));
        let supplied_project = nonempty(introspection.project_id);
        let supplied_org = nonempty(introspection.organization_id);
        if supplied_user.is_none()
            || self
                .user_id()
                .is_some_and(|user| Some(user) != supplied_user)
            || mismatch(&self.project_id, &supplied_project)
            || mismatch(&self.organization_id, &supplied_org)
        {
            return Err(IamError::InvalidResponse);
        }
        self.user_uuid = supplied_user;
        self.project_id = supplied_project.or(self.project_id.take());
        self.organization_id = supplied_org.or(self.organization_id.take());
        Ok(())
    }
}

/// Проверяет, что оба значения заданы и отличаются.
fn mismatch(left: &Option<String>, right: &Option<String>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}

/// Ответ IAM с подтверждёнными данными о владельце токена.
#[derive(Deserialize)]
struct Introspection {
    user_info: UserInfo,        // Данные пользователя, которому принадлежит токен.
    project_id: Option<String>, // Проект из ответа IAM, если он указан.
    organization_id: Option<String>, // Организация из ответа IAM, если она указана.
}

/// Часть ответа IAM с данными пользователя.
#[derive(Deserialize)]
struct UserInfo {
    uuid: String, // Идентификатор владельца токена.
}

/// Извлекает сведения из полезной нагрузки JWT. Если её нельзя прочитать, возвращает пустые данные.
fn token_identity(encoded: &str) -> TokenIdentity {
    // Из ответа IAM по HTTPS берём данные для локального контекста.
    // Право на каждый запрос по этому токену проверяет сервер.
    let Some(payload) = encoded.split('.').nth(1) else {
        return TokenIdentity::default();
    };
    let Ok(bytes) = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
    else {
        return TokenIdentity::default();
    };
    let Ok(mut identity) = serde_json::from_slice::<TokenIdentity>(&bytes) else {
        return TokenIdentity::default();
    };
    identity.user_uuid = nonempty(identity.user_uuid);
    identity.sub = nonempty(identity.sub);
    identity.project_id = nonempty(identity.project_id);
    identity.organization_id = nonempty(identity.organization_id);
    identity
}

/// Вычисляет оставшийся срок действия по JWT или ответу IAM. Истёкший токен отклоняет.
fn token_lifetime(
    body: &TokenBody,
    identity: &TokenIdentity,
    received_at: SystemTime,
    now: SystemTime,
) -> Result<Duration, IamError> {
    let expiry = match identity.exp {
        Some(exp) => UNIX_EPOCH.checked_add(Duration::from_secs(exp)),
        None => body
            .expires_in
            .and_then(|seconds| received_at.checked_add(Duration::from_secs(seconds))),
    };
    expiry
        .and_then(|expiry| expiry.duration_since(now).ok())
        .filter(|lifetime| !lifetime.is_zero())
        .ok_or(IamError::InvalidResponse)
}

/// Оставляет строку, только если она проходит проверку на заполненность.
fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|text| is_nonempty(text))
}

/// Проверяет, что строка непустая и без пробелов по краям.
fn is_nonempty(value: &str) -> bool {
    !value.is_empty() && value.trim() == value
}

/// Проект в том виде, в котором он приходит от IAM.
#[derive(Deserialize)]
struct ProjectBody {
    uuid: String,                       // Идентификатор проекта в ответе сервера.
    name: String,                       // Название проекта для пользователя.
    organization: Option<Organization>, // Организация проекта, если сервер передал её.
}

/// Два формата организации, которые может прислать IAM.
#[derive(Deserialize)]
#[serde(untagged)]
enum Organization {
    Id(String), // Идентификатор организации, присланный строкой.
    Object {
        uuid: Option<String>, // Идентификатор организации, если он указан в объекте.
    },
}

impl Organization {
    /// Достаёт идентификатор организации из строки или объекта.
    fn id(self) -> Option<String> {
        match self {
            Self::Id(id) => Some(id),
            Self::Object { uuid } => uuid,
        }
    }
}

/// Находит массив в ответе сервера, в том числе внутри известного поля-обёртки.
fn collection(value: Value) -> Result<Vec<Value>, IamError> {
    match value {
        Value::Array(values) => Ok(values),
        Value::Object(mut fields) => {
            for collection in ["items", "data", "results", "objects"] {
                if let Some(Value::Array(values)) = fields.remove(collection) {
                    return Ok(values);
                }
            }
            Err(IamError::InvalidResponse)
        }
        _ => Err(IamError::InvalidResponse),
    }
}

/// Читает проекты из ответа, пропуская неполные и повторяющиеся записи.
fn parse_projects(value: Value) -> Result<Vec<IamProject>, IamError> {
    let mut projects = Vec::<IamProject>::new();
    let mut ids = HashSet::new();
    for entry in collection(value)? {
        let Ok(project) = serde_json::from_value::<ProjectBody>(entry) else {
            continue;
        };
        if project.name.trim().is_empty()
            || !is_nonempty(&project.uuid)
            || !ids.insert(project.uuid.clone())
        {
            continue;
        }
        projects.push(IamProject {
            id: project.uuid,
            name: project.name,
            organization_id: nonempty(project.organization.and_then(Organization::id)),
        });
    }
    Ok(projects)
}

#[cfg(test)]
mod tests {
    use reqwest::{Method, blocking::Request, header::AUTHORIZATION};

    use super::*;
    use crate::context::AccountId;

    /// Читает JSON из подготовленного запроса для проверок теста.
    fn request_body(request: &Request) -> Value {
        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap()
    }

    /// Проверяет формат запросов входа, выбора проекта и обновления токена.
    #[test]
    fn login_and_project_exchange_follow_the_workspace_contract() {
        let iam = WorkspaceIamHttp::new();
        let request = iam
            .login_request(
                "WORKSPACE.example:443/api",
                " admin ",
                "password",
                Some("654321"),
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.method(), Method::POST);
        assert_eq!(
            request.url().as_str(),
            format!("https://workspace.example{TOKEN_PATH}")
        );
        assert_eq!(request.headers()["x-otp"], "654321");
        assert!(request.headers()["x-otp"].is_sensitive());
        assert_eq!(
            request_body(&request),
            json!({
                "grant_type": "login+password",
                "login": "admin",
                "password": "password",
                "scope": "openid email profile",
                "ttl": 3600,
                "refresh_ttl": 2_592_000,
            })
        );

        let initial_access = iam
            .refresh_request(
                "https://workspace.example",
                "initial-refresh",
                Some("project"),
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request_body(&initial_access),
            json!({
                "grant_type": "refresh_token",
                "refresh_token": "initial-refresh",
                "scope": "openid email profile project:project",
            })
        );

        let next_access = iam
            .refresh_request("https://workspace.example", "rotated-refresh", None)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request_body(&next_access),
            json!({"grant_type": "refresh_token", "refresh_token": "rotated-refresh"})
        );
    }

    /// Проверяет, что запрос к мессенджеру передаёт проект и помечает токен как секретный.
    #[test]
    fn messenger_probe_uses_context_project_and_sensitive_bearer_header() {
        let iam = WorkspaceIamHttp::new();
        let context = OwnerContext {
            account_id: AccountId("account".into()),
            server: "https://workspace.example".into(),
            organization_id: "organization".into(),
            project_id: "project&foreign=1".into(),
            user_id: "user".into(),
        };
        let request = iam
            .messenger_request(&context, "project-token")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.method(), Method::GET);
        assert_eq!(request.url().path(), STREAMS_PATH);
        assert_eq!(
            request.url().query_pairs().collect::<Vec<_>>(),
            [
                ("page_limit".into(), "1".into()),
                ("project_id".into(), context.project_id.into())
            ]
        );
        assert_eq!(request.headers()[AUTHORIZATION], "Bearer project-token");
        assert!(request.headers()[AUTHORIZATION].is_sensitive());
    }

    /// Проверяет нормализацию адреса и отказ от недопустимых адресов.
    #[test]
    fn only_valid_https_origins_are_used() {
        let origin =
            server_origin("  HTTPS://Workspace.Example:443/login?path=unrelated  ").unwrap();
        assert_eq!(origin.as_str(), "https://workspace.example/");
        assert_eq!(
            server_origin("[::1]:8443").unwrap().as_str(),
            "https://[::1]:8443/"
        );

        for invalid in [
            "",
            "http://workspace.example",
            "ftp://workspace.example",
            "https:///workspace.example",
            "https://@workspace.example",
            "https://admin:secret@workspace.example",
            "https://workspace.example\\@foreign.example",
            "https://work\nspace.example",
            "https://workspace.example:65536",
        ] {
            assert_eq!(
                server_origin(invalid),
                Err(IamError::InvalidServer),
                "{invalid:?}"
            );
        }
    }

    /// Проверяет, как статус и тело ответа влияют на ошибки входа и обновления.
    #[test]
    fn authentication_and_refresh_errors_are_classified_from_status_and_response() {
        assert_eq!(
            grant_error(
                StatusCode::UNAUTHORIZED,
                Some(
                    &json!({"error":"invalid_client","error_description":"The provided otp code is invalid"})
                ),
                Grant::Login,
            ),
            IamError::OtpRequired
        );
        assert_eq!(
            grant_error(
                StatusCode::UNAUTHORIZED,
                Some(&json!({"error":"invalid_grant","error_description":"Invalid credentials"})),
                Grant::Login,
            ),
            IamError::InvalidCredentials
        );
        assert_eq!(
            grant_error(
                StatusCode::BAD_REQUEST,
                Some(&json!({"error":"refresh_token_expired"})),
                Grant::Refresh,
            ),
            IamError::ReauthenticationRequired
        );
        assert_eq!(
            grant_error(StatusCode::UNAUTHORIZED, None, Grant::Refresh),
            IamError::Temporary
        );
        assert_eq!(
            grant_error(StatusCode::TOO_MANY_REQUESTS, None, Grant::Refresh),
            IamError::Temporary
        );
        assert_eq!(
            grant_error(
                StatusCode::SERVICE_UNAVAILABLE,
                Some(&json!({"error":"invalid_grant"})),
                Grant::Refresh,
            ),
            IamError::Temporary
        );
        assert_eq!(
            grant_error(StatusCode::TEMPORARY_REDIRECT, None, Grant::Login),
            IamError::InvalidResponse
        );
    }

    /// Проверяет, как данные JWT и ответ IAM определяют владельца и срок действия.
    #[test]
    fn issuer_claims_and_introspection_resolve_identity_and_expiry() {
        let claims = json!({"sub":"user", "organizationId":"org", "exp": 500});
        let token = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let body = TokenBody {
            access_token: token,
            refresh_token: None,
            expires_in: Some(3_600),
        };
        let mut identity = token_identity(&body.access_token);
        let received_at = UNIX_EPOCH + Duration::from_secs(325);
        let now = received_at + Duration::from_millis(5_500);
        assert_eq!(
            token_lifetime(&body, &identity, received_at, now),
            Ok(Duration::from_millis(169_500))
        );
        assert_eq!(
            token_lifetime(
                &body,
                &identity,
                received_at,
                UNIX_EPOCH + Duration::from_secs(500)
            ),
            Err(IamError::InvalidResponse),
        );

        let introspection = serde_json::from_value(json!({
            "user_info": {"uuid":"user"},
            "project_id":"project",
        }))
        .unwrap();
        identity.merge_introspection(introspection).unwrap();
        assert_eq!(identity.user_id().as_deref(), Some("user"));
        assert_eq!(identity.project_id.as_deref(), Some("project"));
        assert_eq!(identity.organization_id.as_deref(), Some("org"));

        let other_user = serde_json::from_value(json!({
            "user_info": {"uuid":"another-user"},
            "project_id":"project",
        }))
        .unwrap();
        assert_eq!(
            identity.merge_introspection(other_user),
            Err(IamError::InvalidResponse)
        );
    }

    /// Проверяет старый формат списка проектов и сохранение организации.
    #[test]
    fn supports_wrapped_projects_from_older_gateways_and_keeps_organization() {
        let projects = parse_projects(json!({
            "items": [
                {"uuid":"project","name":"Name","organization":{"uuid":"org"}},
                {"uuid":"project","name":"Duplicate","organization":{"uuid":"other-org"}},
                {"uuid":"invalid","name":""},
                {"uuid":"second","name":"Second","organization":{"name":"Older server"}}
            ]
        }))
        .unwrap();
        assert_eq!(
            projects,
            [
                IamProject {
                    id: "project".into(),
                    name: "Name".into(),
                    organization_id: Some("org".into())
                },
                IamProject {
                    id: "second".into(),
                    name: "Second".into(),
                    organization_id: None
                },
            ]
        );
    }

    /// Проверяет, что идентификатор организации можно прислать обычной строкой.
    #[test]
    fn accepts_projects_with_an_organization_id_as_a_string() {
        let projects = parse_projects(json!([
            {"uuid":"project","name":"Workspace","status":"active","organization":"org"}
        ]))
        .unwrap();

        assert_eq!(
            projects,
            [IamProject {
                id: "project".into(),
                name: "Workspace".into(),
                organization_id: Some("org".into()),
            }]
        );
    }
}
