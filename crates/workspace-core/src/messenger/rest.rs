//! Читает начальную позицию и страницы журнала мессенджера по HTTPS.
//! Здесь же разбирается общий формат событий, который используют HTTP и WebSocket.

use std::time::Duration;

use reqwest::{Client, RequestBuilder, StatusCode, Url, header::ACCEPT, redirect::Policy};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{
    context::OwnerContext,
    runtime::SourceError,
    storage::{Cursor, Message},
    sync::Event,
};

const EPOCH_PATH: &str = "/api/workspace/v1/epoch/";
const EVENTS_PATH: &str = "/api/workspace/v1/events/";
const PAGE_LIMIT: usize = 100;

/// HTTP-клиент для чтения журнала мессенджера.
pub(super) struct RestClient(Client); // Настроенный HTTP-клиент для запросов к журналу.

impl RestClient {
    /// Создаёт HTTPS-клиент с тайм-аутами и запретом перенаправлений.
    pub(super) fn new() -> Result<Self, SourceError> {
        let client = Client::builder()
            .https_only(true)
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| SourceError::Temporary)?;
        Ok(Self(client))
    }

    /// Получает текущую позицию журнала, с которой начинается первый запуск.
    pub(super) async fn current_cursor(
        &self,
        owner: &OwnerContext,
        access_token: &str,
    ) -> Result<Cursor, SourceError> {
        let epoch: Epoch = get_json(self.request(owner, access_token, EPOCH_PATH)?).await?;
        epoch.into_cursor()
    }

    /// Загружает и проверяет одну страницу событий строго после заданной позиции.
    pub(super) async fn page(
        &self,
        owner: &OwnerContext,
        access_token: &str,
        after: &Cursor,
    ) -> Result<Vec<Event>, SourceError> {
        let request = self.page_request(owner, access_token, after)?;
        let wire: Vec<Value> = get_json(request).await?;
        decode_page(wire, owner, after)
    }

    /// Добавляет к запросу метку журнала, нижнюю границу позиции и размер страницы.
    fn page_request(
        &self,
        owner: &OwnerContext,
        access_token: &str,
        after: &Cursor,
    ) -> Result<RequestBuilder, SourceError> {
        validate_cursor(after)?;
        Ok(self.request(owner, access_token, EVENTS_PATH)?.query(&[
            ("epoch_version>", after.epoch_version.to_string()),
            ("epoch_generation", after.epoch_generation.clone()),
            ("page_limit", PAGE_LIMIT.to_string()),
        ]))
    }

    /// Собирает GET-запрос к серверу владельца и добавляет токен доступа.
    fn request(
        &self,
        owner: &OwnerContext,
        access_token: &str,
        path: &str,
    ) -> Result<RequestBuilder, SourceError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(path);
        Ok(self
            .0
            .get(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(access_token))
    }
}

/// Проверяет, что адрес владельца содержит базовый HTTPS-адрес сервера.
pub(super) fn workspace_origin(owner: &OwnerContext) -> Result<Url, SourceError> {
    let url = Url::parse(&owner.server).map_err(|_| SourceError::InvalidResponse)?;
    if url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SourceError::InvalidResponse);
    }
    Ok(url)
}

/// Выполняет запрос, проверяет HTTP-статус и разбирает JSON.
async fn get_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, SourceError> {
    let response = request.send().await.map_err(request_error)?;
    check_status(response.status())?;
    response.json().await.map_err(request_error)
}

/// Переводит статус HTTP в ошибку источника, которую понимает синхронизация.
fn check_status(status: StatusCode) -> Result<(), SourceError> {
    match status {
        StatusCode::OK => Ok(()),
        StatusCode::GONE => Err(SourceError::CursorPruned),
        StatusCode::UNAUTHORIZED => Err(SourceError::Unauthorized),
        StatusCode::FORBIDDEN => Err(SourceError::Forbidden),
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => Err(SourceError::Temporary),
        status if status.is_server_error() => Err(SourceError::Temporary),
        _ => Err(SourceError::InvalidResponse),
    }
}

/// Различает неверный запрос или ответ и временный сбой транспорта.
fn request_error(error: reqwest::Error) -> SourceError {
    if error.is_decode() || error.is_builder() {
        SourceError::InvalidResponse
    } else {
        SourceError::Temporary
    }
}

/// Ответ сервера с текущей позицией в журнале.
#[derive(Deserialize)]
struct Epoch {
    epoch_generation: String, // Метка текущего журнала.
    epoch_version: u64,       // Последний номер события в журнале.
}

impl Epoch {
    /// Преобразует ответ сервера в позицию и проверяет допустимый диапазон.
    fn into_cursor(self) -> Result<Cursor, SourceError> {
        let cursor = Cursor {
            epoch_generation: self.epoch_generation,
            epoch_version: self.epoch_version,
        };
        validate_cursor(&cursor)?;
        Ok(cursor)
    }
}

/// Разбирает страницу и проверяет рост позиций. Пропуски между видимыми событиями допустимы.
fn decode_page(
    wire: Vec<Value>,
    owner: &OwnerContext,
    after: &Cursor,
) -> Result<Vec<Event>, SourceError> {
    let mut last_version = after.epoch_version;
    wire.into_iter()
        .map(|value| {
            let event = wire_event(value, owner, &after.epoch_generation)?;
            let version = event.cursor().epoch_version;
            if version <= last_version {
                return Err(SourceError::InvalidResponse);
            }
            last_version = version;
            Ok(event)
        })
        .collect()
}

/// Внешние поля события в JSON, полученном от сервера.
#[derive(Deserialize)]
struct WireEnvelope {
    schema_version: u64, // Версия формата события. Поддерживается версия 1.
    uuid: String,        // Идентификатор события в журнале.
    epoch_generation: Option<String>, // Метка журнала, если сервер прислал её в самом событии.
    epoch_version: u64,  // Номер события внутри журнала.
    project_id: String,  // Идентификатор проекта, которому принадлежит событие.
    user_uuid: String,   // Идентификатор пользователя, для которого выдано событие.
    object_type: String, // Тип объекта, например сообщение.
    action: String,      // Что произошло с объектом, например создание.
    payload: WirePayload, // Данные события и признак его вида.
}

/// Вложенные данные события из JSON.
#[derive(Deserialize)]
struct WirePayload {
    kind: String,               // Название события, например `message.created`.
    uuid: Option<String>,       // Идентификатор объекта. Для нового сообщения обязателен.
    project_id: Option<String>, // Идентификатор проекта, если сервер повторил его во вложенных данных.
    user_uuid: Option<String>, // Идентификатор пользователя, если сервер повторил его во вложенных данных.
    payload: Option<Value>, // Тело объекта. Для нового сообщения здесь лежат формат и содержимое.
}

/// Тело сообщения в ответе сервера.
#[derive(Deserialize)]
struct WireMessageBody {
    kind: String,           // Формат содержимого. Текст сохраняется для формата `markdown`.
    content: Option<Value>, // Содержимое сообщения. Для Markdown это обязательная строка.
}

/// Проверяет владельца и формат события. Возвращает новое сообщение или только позицию.
pub(super) fn wire_event(
    value: Value,
    owner: &OwnerContext,
    epoch: &str,
) -> Result<Event, SourceError> {
    let wire: WireEnvelope =
        serde_json::from_value(value).map_err(|_| SourceError::InvalidResponse)?;
    let cursor = Cursor {
        epoch_generation: epoch.to_owned(),
        epoch_version: wire.epoch_version,
    };
    validate_cursor(&cursor)?;
    if cursor.epoch_version == 0
        || wire.schema_version != 1
        || !nonempty(&wire.uuid)
        || !nonempty(&wire.object_type)
        || !nonempty(&wire.action)
        || !nonempty(&wire.payload.kind)
        || wire.project_id != owner.project_id
        || wire.user_uuid != owner.user_id
        || wire
            .epoch_generation
            .as_deref()
            .is_some_and(|value| value != epoch)
    {
        return Err(SourceError::InvalidResponse);
    }

    let message_created = wire.object_type == "message" && wire.action == "created";
    if message_created != (wire.payload.kind == "message.created") {
        return Err(SourceError::InvalidResponse);
    }
    if !message_created {
        return Ok(Event::Checkpoint(cursor));
    }

    let payload = wire.payload;
    let id = payload
        .uuid
        .filter(|id| nonempty(id))
        .ok_or(SourceError::InvalidResponse)?;
    if payload
        .project_id
        .is_some_and(|project| project != owner.project_id)
        || payload.user_uuid.is_some_and(|user| user != owner.user_id)
    {
        return Err(SourceError::InvalidResponse);
    }
    let body: WireMessageBody =
        serde_json::from_value(payload.payload.ok_or(SourceError::InvalidResponse)?)
            .map_err(|_| SourceError::InvalidResponse)?;
    if !nonempty(&body.kind) {
        return Err(SourceError::InvalidResponse);
    }
    if body.kind != "markdown" {
        return Ok(Event::Checkpoint(cursor));
    }
    let Some(Value::String(text)) = body.content else {
        return Err(SourceError::InvalidResponse);
    };
    Ok(Event::MessageCreated {
        cursor,
        message: Message { id, text },
    })
}

/// Проверяет метку журнала и то, что номер позиции помещается в SQLite.
pub(super) fn validate_cursor(cursor: &Cursor) -> Result<(), SourceError> {
    if !nonempty(&cursor.epoch_generation) || i64::try_from(cursor.epoch_version).is_err() {
        return Err(SourceError::InvalidResponse);
    }
    Ok(())
}

/// Проверяет, что строка непустая и без пробелов по краям.
fn nonempty(value: &str) -> bool {
    !value.is_empty() && value == value.trim()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::context::AccountId;

    /// Создаёт владельца данных для тестов журнала.
    fn owner() -> OwnerContext {
        OwnerContext {
            account_id: AccountId("local-account".into()),
            server: "https://example.test".into(),
            organization_id: "organization".into(),
            project_id: "project".into(),
            user_id: "recipient".into(),
        }
    }

    /// Создаёт позицию тестового журнала с заданным номером.
    fn cursor(version: u64) -> Cursor {
        Cursor {
            epoch_generation: "epoch".to_owned(),
            epoch_version: version,
        }
    }

    /// Создаёт JSON события о новом сообщении для теста.
    fn message(version: u64) -> Value {
        json!({
            "schema_version": 1,
            "uuid": "event-uuid",
            "epoch_version": version,
            "project_id": "project",
            "user_uuid": "recipient",
            "object_type": "message",
            "action": "created",
            "payload": {
                "kind": "message.created",
                "uuid": "message-uuid",
                "project_id": "project",
                "user_uuid": "recipient",
                "payload": {"kind": "markdown", "content": "hello"}
            }
        })
    }

    /// Проверяет, что запрос страницы передаёт токен и строгое ограничение по позиции.
    #[test]
    fn pages_request_strict_cursor_and_bearer_token() {
        let client = RestClient::new().unwrap();
        let request = client
            .page_request(&owner(), "test-token", &cursor(42))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.url().path(), EVENTS_PATH);
        assert_eq!(
            request.url().query_pairs().into_owned().collect::<Vec<_>>(),
            [
                ("epoch_version>".to_owned(), "42".to_owned()),
                ("epoch_generation".to_owned(), "epoch".to_owned()),
                ("page_limit".to_owned(), "100".to_owned())
            ]
        );
        let authorization = &request.headers()[reqwest::header::AUTHORIZATION];
        assert_eq!(authorization, "Bearer test-token");
        assert!(authorization.is_sensitive());
        assert_eq!(
            check_status(StatusCode::GONE),
            Err(SourceError::CursorPruned)
        );
    }

    /// Проверяет чтение событий и допустимость пропусков в их номерах.
    #[test]
    fn decodes_flat_events_and_allows_gaps_in_visible_versions() {
        let owner = owner();
        let mut unsupported = message(100);
        unsupported["action"] = json!("updated");
        unsupported["payload"]["kind"] = json!("message.updated");

        assert_eq!(
            decode_page(vec![message(20), unsupported], &owner, &cursor(3)).unwrap(),
            [
                Event::MessageCreated {
                    cursor: cursor(20),
                    message: Message {
                        id: "message-uuid".to_owned(),
                        text: "hello".to_owned(),
                    },
                },
                Event::Checkpoint(cursor(100))
            ]
        );
    }

    /// Проверяет, что неверное сообщение или чужие данные не продвигают позицию.
    #[test]
    fn refuses_malformed_created_messages_or_wrong_scope_before_advancing() {
        let owner = owner();
        let mut invalid = message(5);
        invalid["payload"]["payload"]["content"] = Value::Null;
        assert_eq!(
            decode_page(vec![message(4), invalid, message(6)], &owner, &cursor(3)),
            Err(SourceError::InvalidResponse)
        );

        let mut wrong_owner = message(4);
        wrong_owner["user_uuid"] = json!("other");
        assert_eq!(
            wire_event(wrong_owner, &owner, "epoch"),
            Err(SourceError::InvalidResponse)
        );
        let mut wrong_project = message(4);
        wrong_project["payload"]["project_id"] = json!("other");
        assert_eq!(
            wire_event(wrong_project, &owner, "epoch"),
            Err(SourceError::InvalidResponse)
        );
        assert_eq!(
            decode_page(vec![message(5), message(4)], &owner, &cursor(3)),
            Err(SourceError::InvalidResponse)
        );
        let mut future_schema = message(4);
        future_schema["schema_version"] = json!(2);
        assert_eq!(
            wire_event(future_schema, &owner, "epoch"),
            Err(SourceError::InvalidResponse)
        );
    }
}
