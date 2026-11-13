//! Запускает локальный HTTPS-сервер с журналом WebSocket и тестовыми аккаунтами.
//! Данные и конфигурация TLS принадлежат одному прогону.

use std::{
    convert::Infallible,
    net::Ipv4Addr,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};

use futures::SinkExt;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::{
    sync::broadcast,
    task::{JoinHandle, JoinSet},
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{
    Message as Frame,
    handshake::derive_accept_key,
    protocol::{CloseFrame, Role, frame::coding::CloseCode},
};
use uuid::Uuid;
use workspace_core::auth::{Auth, IamClient, IamError, IamProject, IamToken};
use workspace_core::context::OwnerContext;
use workspace_core::messenger::MessengerSource;

use super::{MemorySecrets, TestWorkspace};
use crate::platform::JsonAccountStore;

const FOREGROUND: &str = "10000000-0000-4000-8000-000000000001";
const BACKGROUND: &str = "10000000-0000-4000-8000-000000000002";
const PROJECT: &str = "10000000-0000-4000-8000-000000000003";
const ORGANIZATION: &str = "10000000-0000-4000-8000-000000000004";
const DIRECT: &str = "10000000-0000-4000-8000-000000000005";
const TOPIC: &str = "10000000-0000-4000-8000-000000000006";
const CHANNEL: &str = "10000000-0000-4000-8000-000000000007";
const ALL_CHATS: &str = "00000000-0000-0000-0000-000000000000";
const EPOCH: &str = "integration-test";
const NEXT_EPOCH: &str = "integration-test-next";
const INITIAL_POSITION: u64 = 10;
const INITIAL_FOREGROUND_UNREAD: u32 = 4;
const INITIAL_BACKGROUND_UNREAD: u32 = 29;
const WS_PROTOCOL: &str = "workspace.events.v1";
const WS_PATH: &str = "/api/workspace/v1/events/ws";

/// Управляет сервером и подготовленными сессиями тестовых аккаунтов.
pub(super) struct MockWorkspace {
    server: String,         // Адрес HTTPS и WSS для обоих аккаунтов.
    journal: Arc<Journal>,  // Данные, которые тест меняет при отправке сообщения.
    worker: JoinHandle<()>, // Приём сокетов до завершения сценария.
}

impl MockWorkspace {
    /// Возвращает ошибку для следующего запроса списка чатов аккаунта A.
    pub(super) fn fail_next_catalog(&self) {
        self.journal.fail_catalog.store(true, Ordering::Relaxed);
    }

    /// Добавляет изменение в журнал B и доставляет его через WSS.
    pub(super) fn send_history_change(&self, object: &str, action: &str, payload: Value) -> u64 {
        let mut entries = self.journal.entries.lock().unwrap();
        let (epoch, base, _) = self.journal.baseline(BACKGROUND);
        let version = base + entries.len() as u64 + 1;
        let change = event(epoch, version, object, action, payload);
        entries.push(change.clone());
        self.journal.live.send(change).unwrap();
        version
    }

    /// Делает позицию B непригодной и посылает сигнал через открытый WSS.
    pub(super) fn reject_cursor(&self, change_epoch: bool) {
        let mut entries = self.journal.entries.lock().unwrap();
        entries.clear();
        self.journal
            .recovery
            .store(if change_epoch { 2 } else { 1 }, Ordering::Relaxed);
        self.journal
            .live
            .send(json!({"test_reject_cursor": true, "change_epoch": change_epoch}))
            .unwrap();
    }

    /// Открывает только локальный порт и запускает обработку соединений.
    pub(super) async fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("не удалось открыть локальный порт мок-сервера");
        let server = format!("https://{}", listener.local_addr().unwrap());
        let journal = Arc::new(Journal::default());
        let journal_for_worker = journal.clone();
        let tls = localhost_tls();
        let worker = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.expect("мок-сервер не принял соединение");
                        let journal = journal_for_worker.clone();
                        let tls = tls.clone();
                        connections.spawn(async move {
                            // Клиент, который не доверяет тестовому сертификату, завершит TLS здесь.
                            if let Ok(socket) = tls.accept(socket).await {
                                let service = service_fn(move |request| {
                                    let journal = journal.clone();
                                    async move { Ok::<_, Infallible>(handle(request, journal)) }
                                });
                                let _ = http1::Builder::new()
                                    .serve_connection(TokioIo::new(socket), service)
                                    .with_upgrades()
                                    .await;
                            }
                        });
                    }
                    result = connections.join_next(), if !connections.is_empty() => {
                        result.unwrap().expect("обработчик мок-сервера завершился аварийно");
                    }
                }
            }
        });
        Self {
            server,
            journal,
            worker,
        }
    }
}

impl Drop for MockWorkspace {
    /// Прекращает принимать соединения, чтобы повторный прогон стартовал на чистом сервере.
    fn drop(&mut self) {
        self.worker.abort();
    }
}

impl TestWorkspace for MockWorkspace {
    /// Создаёт временные учётные записи через штатный код входа и выбора проекта.
    async fn accounts(&self, profile: &Path) -> (Auth, OwnerContext, OwnerContext) {
        let accounts = Arc::new(JsonAccountStore::open(profile).unwrap());
        let secrets = Arc::new(MemorySecrets::default());
        let auth = Auth::new(Arc::new(FakeIam), accounts.clone(), secrets.clone());
        let sign_in = |user| {
            let attempt = auth
                .begin_login(&self.server, user, "test-password", None)
                .unwrap();
            auth.complete_login(attempt.id, PROJECT).unwrap()
        };
        let foreground = sign_in(FOREGROUND);
        let background = sign_in(BACKGROUND);
        let restored = Auth::new(Arc::new(FakeIam), accounts, secrets);
        (restored, foreground, background)
    }

    /// Создаёт рабочий транспорт с доверием к тестовому центру сертификации.
    fn source(&self) -> MessengerSource {
        let ca = CertificateDer::from_pem_slice(include_bytes!("tls/ca.pem")).unwrap();
        MessengerSource::with_private_ca(ca.as_ref()).unwrap()
    }

    /// Записывает сообщение и новое значение счётчика в журнал получателя.
    async fn send_direct_message(&self, body: &str) -> (String, String) {
        (DIRECT.to_owned(), self.journal.send_direct_message(body))
    }
}

/// Выдаёт токены двух аккаунтов. Авторизация и синхронизация потребляют их обычным способом.
struct FakeIam;

impl FakeIam {
    /// Возвращает тестовый токен для известного участника чата.
    fn token(user: &str, project: Option<&str>) -> Result<IamToken, IamError> {
        if !matches!(user, FOREGROUND | BACKGROUND) {
            return Err(IamError::InvalidCredentials);
        }
        Ok(IamToken {
            access_token: user.to_owned(),
            refresh_token: Some(user.to_owned()),
            expires_in: Duration::from_secs(3600),
            user_id: user.to_owned(),
            project_id: project.map(str::to_owned),
            organization_id: Some(ORGANIZATION.to_owned()),
        })
    }
}

impl IamClient for FakeIam {
    /// Использует переданный адрес локального сервера.
    fn normalize_server(&self, server: &str) -> Result<String, IamError> {
        Ok(server.to_owned())
    }

    /// Возвращает исходный токен при входе тестового пользователя.
    fn login(&self, _: &str, login: &str, _: &str, _: Option<&str>) -> Result<IamToken, IamError> {
        Self::token(login, None)
    }

    /// Предлагает единственный проект тестового чата.
    fn projects(&self, _: &str, _: &str) -> Result<Vec<IamProject>, IamError> {
        Ok(vec![IamProject {
            id: PROJECT.to_owned(),
            name: "Проект для теста".to_owned(),
            organization_id: Some(ORGANIZATION.to_owned()),
        }])
    }

    /// Выдаёт токен для запросов в выбранном проекте.
    fn select_project(&self, _: &str, refresh: &str, project: &str) -> Result<IamToken, IamError> {
        Self::token(refresh, Some(project))
    }

    /// Восстанавливает токен уже выбранного проекта.
    fn refresh(&self, _: &str, refresh: &str) -> Result<IamToken, IamError> {
        Self::token(refresh, Some(PROJECT))
    }
}

/// Хранит события получателя и передаёт новые записи открытым сокетам.
struct Journal {
    entries: Mutex<Vec<Value>>,     // События получателя в порядке версий.
    live: broadcast::Sender<Value>, // Поток новых событий после установки сокета.
    recovery: AtomicU8,             // 0 — исходный журнал, 1 — обрезанный, 2 — новая эпоха для B.
    fail_catalog: AtomicBool,       // Нужно ли вернуть ошибку при следующей загрузке каталога A.
}

impl Default for Journal {
    /// Создаёт пустой журнал с каналом живой доставки.
    fn default() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            live: broadcast::channel(16).0,
            recovery: AtomicU8::new(0),
            fail_catalog: AtomicBool::new(false),
        }
    }
}

impl Journal {
    /// Возвращает метку, начальную позицию и снимок счётчика для пользователя.
    fn baseline(&self, user: &str) -> (&'static str, u64, u32) {
        if user == FOREGROUND {
            return (EPOCH, INITIAL_POSITION, INITIAL_FOREGROUND_UNREAD);
        }
        match self.recovery.load(Ordering::Relaxed) {
            1 => (EPOCH, 50, 7),
            2 => (NEXT_EPOCH, 1, 7),
            _ => (EPOCH, INITIAL_POSITION, INITIAL_BACKGROUND_UNREAD),
        }
    }

    /// Записывает Markdown и публикует событие папки после сообщения.
    fn send_direct_message(&self, text: &str) -> String {
        let mut entries = self.entries.lock().unwrap();
        let (epoch, base, unread) = self.baseline(BACKGROUND);
        let version = base + entries.len() as u64 + 1;
        let count = unread + folder_updates(&entries) + 1;
        let id = Uuid::new_v4().to_string();
        let message = event(
            epoch,
            version,
            "message",
            "created",
            json!({
                "kind": "message.created", "uuid": id, "source_name": "native",
                "is_own": false, "read": false, "author_uuid": FOREGROUND,
                "stream_uuid": DIRECT, "topic_uuid": TOPIC,
                "payload": {"kind": "markdown", "content": text},
            }),
        );
        let folder = event(
            epoch,
            version + 1,
            "folder",
            "updated",
            json!({
                "kind": "folder.updated", "uuid": ALL_CHATS, "unread_count": count,
            }),
        );
        entries.extend([message.clone(), folder.clone()]);
        self.live
            .send(message.clone())
            .expect("сокет получателя должен быть подключён");
        // Повтор приходит до обновления бейджа, поэтому основной сценарий увидит все его эффекты.
        self.live.send(message).unwrap();
        self.live.send(folder).unwrap();
        id
    }
}

/// Считает изменения счётчика, пропуская события истории.
fn folder_updates(entries: &[Value]) -> u32 {
    entries
        .iter()
        .filter(|event| event["object_type"] == "folder")
        .count() as u32
}

/// Формирует ответ в формате настоящего журнала Workspace.
fn event(epoch: &str, version: u64, object: &str, action: &str, payload: Value) -> Value {
    json!({
        "schema_version": 1, "uuid": Uuid::new_v4().to_string(),
        "epoch_generation": epoch, "epoch_version": version,
        "project_id": PROJECT, "user_uuid": BACKGROUND,
        "object_type": object, "action": action, "payload": payload,
    })
}

/// Настраивает сервер TLS сертификатом только для loopback.
fn localhost_tls() -> TlsAcceptor {
    // Ключ принадлежит только тестовому сертификату localhost и не используется приложением.
    let cert = CertificateDer::from_pem_slice(include_bytes!("tls/localhost.pem")).unwrap();
    let key = PrivateKeyDer::from_pem_slice(include_bytes!("tls/localhost-key.pem")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

/// Проверяет тестового пользователя и возвращает маршрут REST либо поднимает WebSocket.
fn handle(mut request: Request<Incoming>, journal: Arc<Journal>) -> Response<Full<Bytes>> {
    if request.uri().path() == WS_PATH {
        return upgrade(&mut request, journal);
    }

    let user = match request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    {
        Some(user @ (FOREGROUND | BACKGROUND)) => user,
        _ => return response(StatusCode::UNAUTHORIZED, Value::Null),
    };
    let entries = journal.entries.lock().unwrap();
    let (epoch, base, unread) = journal.baseline(user);
    let event_count = if user == BACKGROUND { entries.len() } else { 0 };
    let path = request.uri().path();
    let value = match path {
        "/api/workspace/v1/messenger/streams/" => {
            if user == FOREGROUND && journal.fail_catalog.swap(false, Ordering::Relaxed) {
                return response(StatusCode::SERVICE_UNAVAILABLE, Value::Null);
            }
            assert_eq!(query_value(request.uri(), "page_limit"), Some("100"));
            let name = if user == FOREGROUND {
                "Канал A"
            } else {
                "Канал B"
            };
            if query_value(request.uri(), "page_marker") == Some(DIRECT) {
                json!([{
                    "uuid": CHANNEL, "user_uuid": user, "project_id": PROJECT,
                    "name": name, "private": false, "is_archived": false,
                }])
            } else {
                let peer = if user == FOREGROUND {
                    BACKGROUND
                } else {
                    FOREGROUND
                };
                let mut page = response(
                    StatusCode::OK,
                    json!([{
                        "uuid": DIRECT, "user_uuid": user, "project_id": PROJECT,
                        "name": "Личный чат", "private": true, "is_archived": false, "direct_user_uuid": peer,
                    }]),
                );
                page.headers_mut()
                    .insert("X-Pagination-Marker", HeaderValue::from_static(DIRECT));
                return page;
            }
        }
        "/api/workspace/v1/messenger/stream_topics/" => json!([{
            "uuid": TOPIC, "stream_uuid": CHANNEL, "user_uuid": user, "project_id": PROJECT,
            "name": "Планы",
        }]),
        "/api/workspace/v1/users/" => json!([
            {"uuid": FOREGROUND, "username": "alice", "first_name": "Алиса"},
            {"uuid": BACKGROUND, "username": "bob", "first_name": "Борис"},
        ]),
        "/api/workspace/v1/epoch/" => json!({
            "epoch_generation": epoch, "epoch_version": base + event_count as u64,
        }),
        "/api/workspace/v1/events/" => {
            let Some(after) = position(request.uri(), "epoch_version%3E") else {
                return response(StatusCode::BAD_REQUEST, Value::Null);
            };
            if after < base || query_value(request.uri(), "epoch_generation") != Some(epoch) {
                return response(StatusCode::GONE, json!({"error": "epoch_pruned"}));
            }
            json!(
                entries
                    .iter()
                    .filter(|event| user == BACKGROUND
                        && event["epoch_version"].as_u64().unwrap() > after)
                    .collect::<Vec<_>>()
            )
        }
        "/api/workspace/v1/messenger/folders/00000000-0000-0000-0000-000000000000" => json!({
            "uuid": ALL_CHATS, "user_uuid": user, "project_id": PROJECT,
            "unread_count": unread + if user == BACKGROUND { folder_updates(&entries) } else { 0 },
        }),
        "/api/workspace/v1/messenger/streams/10000000-0000-4000-8000-000000000005" => json!({
            "uuid": DIRECT, "user_uuid": user, "project_id": PROJECT,
            "private": true, "is_archived": false, "direct_user_uuid": FOREGROUND,
            "notification_mode": "all_messages",
        }),
        "/api/workspace/v1/messenger/stream_topics/10000000-0000-4000-8000-000000000006" => json!({
            "uuid": TOPIC, "stream_uuid": DIRECT, "user_uuid": user, "project_id": PROJECT,
            "notification_mode": "default",
        }),
        _ => return response(StatusCode::NOT_FOUND, Value::Null),
    };
    response(StatusCode::OK, value)
}

/// Возвращает числовую позицию, которую клиент передал в строке запроса.
fn position(uri: &hyper::Uri, name: &str) -> Option<u64> {
    query_value(uri, name)?.parse().ok()
}

/// Возвращает значение параметра с простым именем и значением из теста.
fn query_value<'a>(uri: &'a hyper::Uri, name: &str) -> Option<&'a str> {
    uri.query()?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

/// Формирует JSON-ответ с указанным статусом.
fn response(status: StatusCode, value: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(value.to_string())))
        .unwrap()
}

/// Принимает протокол журнала и отправляет снимок позиции перед живыми событиями.
fn upgrade(request: &mut Request<Incoming>, journal: Arc<Journal>) -> Response<Full<Bytes>> {
    let protocol = request
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok());
    let user = match protocol.and_then(|value| value.split_once(", bearer.")) {
        Some((WS_PROTOCOL, user @ (FOREGROUND | BACKGROUND))) => user,
        _ => return response(StatusCode::UNAUTHORIZED, Value::Null),
    };
    let Some(key) = request.headers().get(header::SEC_WEBSOCKET_KEY) else {
        return response(StatusCode::BAD_REQUEST, Value::Null);
    };
    let Some(after) = position(request.uri(), "last_epoch_version") else {
        return response(StatusCode::BAD_REQUEST, Value::Null);
    };
    let is_recipient = user == BACKGROUND;
    let accept = derive_accept_key(key.as_bytes());
    let (mut live, backlog, epoch) = {
        let entries = journal.entries.lock().unwrap();
        let (epoch, base, _) = journal.baseline(user);
        if after < base || query_value(request.uri(), "epoch_generation") != Some(epoch) {
            return response(StatusCode::GONE, json!({"error": "epoch_pruned"}));
        }
        let backlog = entries
            .iter()
            .filter(|event| is_recipient && event["epoch_version"].as_u64().unwrap() > after)
            .cloned()
            .collect::<Vec<_>>();
        (journal.live.subscribe(), backlog, epoch)
    };
    let future = hyper::upgrade::on(request);
    tokio::spawn(async move {
        let Ok(io) = future.await else { return };
        let mut socket =
            WebSocketStream::from_raw_socket(TokioIo::new(io), Role::Server, None).await;
        let mut last = after;
        for event in backlog {
            last = event["epoch_version"].as_u64().unwrap();
            if socket.send(Frame::text(event.to_string())).await.is_err() {
                return;
            }
        }
        let ready = json!({"type": "ready", "epoch_generation": epoch, "epoch_version": last});
        if socket.send(Frame::text(ready.to_string())).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                event = live.recv() => {
                    let Ok(event) = event else { return };
                    if event["test_reject_cursor"] == true {
                        if is_recipient {
                            if event["change_epoch"] == true {
                                let _ = socket.send(Frame::text(json!({"error": "epoch_pruned", "reason": "epoch_generation_changed", "epoch_generation": NEXT_EPOCH}).to_string())).await;
                            } else {
                                let _ = socket.close(Some(CloseFrame { code: CloseCode::from(4410), reason: "epoch_pruned".into() })).await;
                            }
                            return;
                        }
                        continue;
                    }
                    if is_recipient && socket.send(Frame::text(event.to_string())).await.is_err() {
                        return;
                    }
                }
                frame = futures::StreamExt::next(&mut socket) => match frame {
                    Some(Ok(Frame::Ping(_))) => {
                        if socket.flush().await.is_err() { return; }
                    }
                    Some(Ok(Frame::Close(_)) | Err(_)) | None => return,
                    _ => {}
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, HeaderValue::from_static("upgrade"))
        .header(header::UPGRADE, HeaderValue::from_static("websocket"))
        .header(header::SEC_WEBSOCKET_ACCEPT, accept)
        .header(header::SEC_WEBSOCKET_PROTOCOL, WS_PROTOCOL)
        .body(Full::default())
        .unwrap()
}
