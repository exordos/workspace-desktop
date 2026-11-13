//! Подключает общий сценарий к выделенному тестовому Workspace.
//! Вход идёт через IAM, отправка через Messenger API, а клиент читает штатный журнал.

use std::{
    env,
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use reqwest::{Client, Url, redirect::Policy};
use serde_json::{Value, json};
use uuid::Uuid;
use workspace_core::auth::{Auth, Service};
use workspace_core::context::OwnerContext;
use workspace_core::iam_http::WorkspaceIamHttp;
use workspace_core::messenger::MessengerSource;

use super::{MemorySecrets, TestWorkspace};
use crate::platform::JsonAccountStore;

const STREAMS: &str = "/api/workspace/v1/messenger/streams/";
const MESSAGES: &str = "/api/workspace/v1/messenger/messages/";

/// Хранит параметры выбранного стенда и сессию для отправки сообщения.
pub(super) struct LiveWorkspace {
    server: Url,                // Явно выбранный HTTPS-адрес стенда.
    project: Uuid,              // Проект, в котором подготовлен личный чат.
    first: Credentials,         // Данные входа отправителя.
    second: Credentials,        // Данные входа получателя.
    http: Client,               // Клиент для поиска чата и отправки через API.
    chat: OnceLock<DirectChat>, // Данные чата после подготовки аккаунтов.
}

/// Данные входа одного тестового пользователя.
struct Credentials {
    login: String,    // Имя пользователя в IAM.
    password: String, // Пароль, полученный из окружения запуска.
}

/// Тестовый личный чат и авторизация отправителя.
struct DirectChat {
    auth: Auth,           // Авторизация, из которой берём токен доступа.
    sender: OwnerContext, // Владелец сессии для отправки сообщения.
    stream: Uuid,         // Идентификатор существующего личного чата.
}

impl LiveWorkspace {
    /// Читает параметры выделенного стенда при явном запуске сетевого теста.
    pub(super) fn from_environment() -> Self {
        let server = Url::parse(&required("WORKSPACE_TEST_URL"))
            .expect("WORKSPACE_TEST_URL должен содержать HTTPS-адрес");
        assert!(
            server.scheme() == "https"
                && server.host().is_some()
                && server.username().is_empty()
                && server.password().is_none()
                && server.path() == "/"
                && server.query().is_none()
                && server.fragment().is_none(),
            "WORKSPACE_TEST_URL должен быть базовым HTTPS-адресом тестового Workspace"
        );

        let first = Credentials {
            login: required("WORKSPACE_TEST_LOGIN_A"),
            password: required("WORKSPACE_TEST_PASSWORD_A"),
        };
        let second = Credentials {
            login: required("WORKSPACE_TEST_LOGIN_B"),
            password: required("WORKSPACE_TEST_PASSWORD_B"),
        };
        assert_ne!(
            first.login, second.login,
            "нужны два разных тестовых аккаунта"
        );
        Self {
            server,
            project: Uuid::parse_str(&required("WORKSPACE_TEST_PROJECT"))
                .expect("WORKSPACE_TEST_PROJECT должен содержать UUID проекта"),
            first,
            second,
            http: Client::builder()
                .https_only(true)
                .redirect(Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(8))
                .build()
                .expect("не удалось создать клиент тестового Workspace"),
            chat: OnceLock::new(),
        }
    }

    /// Собирает URL маршрута на выбранном стенде.
    fn endpoint(&self, path: &str) -> Url {
        let mut url = self.server.clone();
        url.set_path(path);
        url
    }
}

impl TestWorkspace for LiveWorkspace {
    /// Входит в оба аккаунта, находит их общий чат и готовит отдельную авторизацию для восстановления.
    async fn accounts(&self, profile: &Path) -> (Auth, OwnerContext, OwnerContext) {
        let profile = profile.to_owned();
        let server = self.server.origin().ascii_serialization();
        let project = self.project.to_string();
        let logins = [
            (self.first.login.clone(), self.first.password.clone()),
            (self.second.login.clone(), self.second.password.clone()),
        ];
        // IAM использует блокирующий HTTP. Вход выполняется вне потока Tokio.
        let (auth, restored, [sender, recipient]) = tokio::task::spawn_blocking(move || {
            let store = Arc::new(JsonAccountStore::open(&profile).unwrap());
            let secrets = Arc::new(MemorySecrets::default());
            let iam = Arc::new(WorkspaceIamHttp::new());
            let auth = Auth::new(iam.clone(), store.clone(), secrets.clone());
            let owners = logins.map(|(login, password)| {
                let attempt = auth
                    .begin_login(&server, &login, &password, None)
                    .unwrap_or_else(|error| panic!("вход тестового аккаунта {login}: {error}"));
                auth.complete_login(attempt.id, &project)
                    .unwrap_or_else(|error| panic!("выбор проекта для {login}: {error}"))
            });
            (auth, Auth::new(iam, store, secrets), owners)
        })
        .await
        .expect("подготовка тестовых аккаунтов прервалась");
        assert_ne!(
            sender.user_id, recipient.user_id,
            "IAM вернул одного пользователя для обоих аккаунтов"
        );

        let token = auth
            .access_token(&sender, Service::Messenger)
            .await
            .unwrap();
        let response = self
            .http
            .get(self.endpoint(STREAMS))
            .bearer_auth(token)
            .query(&[
                ("direct_user_uuid", &recipient.user_id),
                ("project_id", &sender.project_id),
            ])
            .send()
            .await
            .expect("не удалось запросить личные чаты")
            .error_for_status()
            .expect("Workspace отклонил запрос личных чатов");
        let streams: Vec<Value> = response.json().await.expect("ожидался список личных чатов");
        let stream = streams.iter().find(|stream| {
            stream["direct_user_uuid"].as_str() == Some(&recipient.user_id)
                && stream["project_id"].as_str() == Some(&sender.project_id)
                && stream["private"].as_bool() == Some(true)
                && stream["source_name"].as_str().unwrap_or("native") == "native"
                && !stream["encryption"].as_bool().unwrap_or(false)
                && !stream["is_archived"].as_bool().unwrap_or(false)
        }).expect("в проекте требуется существующий личный чат Workspace без шифрования между тестовыми аккаунтами");
        let stream = Uuid::parse_str(stream["uuid"].as_str().expect("чат без UUID"))
            .expect("неверный UUID личного чата");
        assert!(
            self.chat
                .set(DirectChat {
                    auth: restored.clone(),
                    sender: sender.clone(),
                    stream
                })
                .is_ok()
        );
        (restored, sender, recipient)
    }

    /// Подключает штатный транспорт с системным доверием к сертификатам.
    fn source(&self) -> MessengerSource {
        MessengerSource::new().unwrap()
    }

    /// Отправляет Markdown через API тестового Workspace и читает UUID созданного сообщения.
    async fn send_direct_message(&self, body: &str) -> (String, String) {
        let chat = self.chat.get().expect("тестовый чат ещё не подготовлен");
        let token = chat
            .auth
            .access_token(&chat.sender, Service::Messenger)
            .await
            .unwrap();
        let response = self
            .http
            .post(self.endpoint(MESSAGES))
            .bearer_auth(token)
            .json(&json!({
                "stream_uuid": chat.stream.to_string(),
                "payload": {"kind": "markdown", "content": body},
            }))
            .send()
            .await
            .expect("не удалось отправить сообщение в тестовый чат")
            .error_for_status()
            .expect("Workspace отклонил отправку сообщения");
        let result: Value = response
            .json()
            .await
            .expect("ожидался JSON созданного сообщения");
        let message = Uuid::parse_str(result["uuid"].as_str().expect("сообщение без UUID"))
            .expect("неверный UUID сообщения")
            .to_string();
        (chat.stream.to_string(), message)
    }
}

/// Читает обязательную переменную без вывода её значения в журнал.
fn required(name: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("задайте {name} для тестового Workspace"))
}
