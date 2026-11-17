//! Проверяет путь от окна через каталог и очередь до локального HTTPS/WSS-сервера.

use std::{
    convert::Infallible,
    net::{Ipv4Addr, TcpListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures::SinkExt;
use gpui::{ClipboardItem, Modifiers, TestAppContext, VisualTestContext, px, size};
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    header,
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, handshake::derive_accept_key, protocol::Role},
};
use uuid::Uuid;
use workspace_core::{
    auth::{AccountRecord, IamClient, IamError, IamProject, IamToken, SecretStore, StoreError},
    context::OwnerContext,
};

use super::*;
use crate::{gpui_client::bind_keys, platform::JsonAccountStore};

const USER: &str = "10000000-0000-4000-8000-000000000001";
const PEER: &str = "10000000-0000-4000-8000-000000000002";
const PROJECT: &str = "10000000-0000-4000-8000-000000000003";
const ORGANIZATION: &str = "10000000-0000-4000-8000-000000000004";
const STREAM: &str = "10000000-0000-4000-8000-000000000005";
const TOPIC: &str = "10000000-0000-4000-8000-000000000006";
const MARKER: &str = "Контрольная запись";
const MESSAGES: &str = "/api/workspace/v1/messenger/messages/";
const STREAMS: &str = "/api/workspace/v1/messenger/streams/";
const TOPICS: &str = "/api/workspace/v1/messenger/stream_topics/";
const TIMEOUT: Duration = Duration::from_secs(5);

/// Восстанавливает синтетическую сессию без системных токенов.
struct LocalIam;

impl IamClient for LocalIam {
    /// Тестовый URL уже записан в профиле.
    fn normalize_server(&self, _: &str) -> Result<String, IamError> {
        unreachable!()
    }

    /// Для сохранённого профиля новый вход не нужен.
    fn login(&self, _: &str, _: &str, _: &str, _: Option<&str>) -> Result<IamToken, IamError> {
        unreachable!()
    }

    /// В сохранённом профиле проект уже выбран.
    fn projects(&self, _: &str, _: &str) -> Result<Vec<IamProject>, IamError> {
        unreachable!()
    }

    /// Повторный выбор проекта не участвует в этом сценарии.
    fn select_project(&self, _: &str, _: &str, _: &str) -> Result<IamToken, IamError> {
        unreachable!()
    }

    /// Возвращает полномочия одного тестового входа.
    fn refresh(&self, _: &str, _: &str) -> Result<IamToken, IamError> {
        Ok(IamToken {
            access_token: "local-user".into(),
            refresh_token: None,
            expires_in: Duration::from_secs(3600),
            user_id: USER.into(),
            project_id: Some(PROJECT.into()),
            organization_id: Some(ORGANIZATION.into()),
        })
    }
}

/// Поставляет готовое тестовое значение для одной записи профиля.
struct LocalSecrets;

impl SecretStore for LocalSecrets {
    /// Выдаёт маркер только синтетическому входу.
    fn load(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(key
            .ends_with(":00000000-0000-0000-0000-000000000000")
            .then(|| "local-refresh".to_owned()))
    }

    /// В этом прогоне IAM не меняет маркер входа.
    fn save(&self, _: &str, _: &str) -> Result<(), StoreError> {
        unreachable!()
    }

    /// В этом прогоне нет выхода из аккаунта.
    fn remove(&self, _: &str) -> Result<(), StoreError> {
        unreachable!()
    }
}

/// Записывает наблюдения сервера о готовности подключений и принятой отправке.
#[derive(Default)]
struct Received {
    sent: Mutex<Vec<Uuid>>, // Публичные UUID реально принятых POST в порядке поступления.
    ready: AtomicUsize,     // Сколько подключений подтвердили готовность через WSS.
    targets: AtomicUsize,   // Сколько проверок доступа к стриму вернул HTTPS.
}

/// Держит локальный сервер, пока существует сценарий.
struct Server {
    address: String,      // Обычный URL тестового профиля с временным портом.
    state: Arc<Received>, // Наблюдения тестового сервера, без журналирования текста.
    stop: Option<oneshot::Sender<()>>, // Завершает локальную сеть.
    thread: Option<std::thread::JoinHandle<()>>, // Ждёт закрытия сокетов.
}

impl Server {
    /// Открывает только loopback и запускает обычный HTTPS/WS на отдельном потоке.
    fn start() -> Self {
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = format!("https://{}", socket.local_addr().unwrap());
        socket.set_nonblocking(true).unwrap();
        let (stop, mut stopping) = oneshot::channel();
        let state = Arc::new(Received::default());
        let received = state.clone();
        // Эти сертификаты уже используются локальным сценарием Desktop.
        let cert = CertificateDer::from_pem_slice(include_bytes!(
            "../../shell/background_message/tls/localhost.pem"
        ))
        .unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!(
            "../../shell/background_message/tls/localhost-key.pem"
        ))
        .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
        let tls = TlsAcceptor::from(Arc::new(config));
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let socket = tokio::net::TcpListener::from_std(socket).unwrap();
                    loop {
                        tokio::select! {
                            _ = &mut stopping => break,
                            connection = socket.accept() => {
                                let (stream, _) = connection.unwrap();
                                let state = received.clone();
                                let tls = tls.clone();
                                tokio::spawn(async move {
                                    let Ok(socket) = tls.accept(stream).await else { return };
                                    let route = service_fn(move |request| {
                                        let state = state.clone();
                                        async move { Ok::<_, Infallible>(response(request, state).await) }
                                    });
                                    let _ = http1::Builder::new().serve_connection(TokioIo::new(socket), route).with_upgrades().await;
                                });
                            }
                        }
                    }
                });
        });
        Self {
            address,
            state,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Server {
    /// Закрывает локальное соединение и освобождает выделенный порт.
    fn drop(&mut self) {
        self.stop.take();
        let _ = self.thread.take().unwrap().join();
    }
}

/// Применяет реальные методы и заголовки протокола к синтетическому чату.
async fn response(mut request: Request<Incoming>, state: Arc<Received>) -> Response<Full<Bytes>> {
    let path = request.uri().path();
    if path == "/api/workspace/v1/events/ws" {
        assert_eq!(
            request
                .headers()
                .get(header::SEC_WEBSOCKET_PROTOCOL)
                .unwrap(),
            "workspace.events.v1, bearer.local-user"
        );
        let key = request.headers().get(header::SEC_WEBSOCKET_KEY).unwrap();
        let accept = derive_accept_key(key.as_bytes());
        let upgrade = hyper::upgrade::on(&mut request);
        tokio::spawn(async move {
            let socket = upgrade.await.unwrap();
            let mut ws =
                WebSocketStream::from_raw_socket(TokioIo::new(socket), Role::Server, None).await;
            let ready = json!({"type":"ready","epoch_generation":"local","epoch_version":1});
            ws.send(Message::text(ready.to_string())).await.unwrap();
            state.ready.fetch_add(1, Ordering::SeqCst);
            futures::future::pending::<()>().await;
            drop(ws);
        });
        return Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_PROTOCOL, "workspace.events.v1")
            .header(header::SEC_WEBSOCKET_ACCEPT, accept)
            .body(Full::default())
            .unwrap();
    }

    assert_eq!(
        request.headers().get(header::AUTHORIZATION).unwrap(),
        "Bearer local-user"
    );
    if path == MESSAGES && request.method() == Method::POST {
        let bytes = request.into_body().collect().await.unwrap().to_bytes();
        let post: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(post["stream_uuid"], STREAM);
        assert_eq!(post["topic_uuid"], TOPIC);
        assert_eq!(post["payload"]["content"], MARKER);
        let canonical = post["uuid"].as_str().unwrap();
        let id = Uuid::new_v5(&Uuid::parse_str(TOPIC).unwrap(), canonical.as_bytes());
        state.sent.lock().unwrap().push(id);
        return json_response(
            StatusCode::CREATED,
            json!({
                "uuid": id, "stream_uuid": STREAM, "topic_uuid": TOPIC, "user_uuid": USER,
                "project_id": PROJECT, "author_uuid": USER, "created_at": "2026-11-17T10:00:00Z",
                "payload": {"kind": "markdown", "content": MARKER},
            }),
        );
    }

    let stream = json!({
        "uuid": STREAM, "project_id": PROJECT, "user_uuid": USER, "name":"Тестовый чат",
        "private": true, "direct_user_uuid": PEER, "is_archived": false,
        "default_topic_uuid": TOPIC, "encryption": false, "notification_mode":"all_messages",
    });
    let result = match path {
        STREAMS => json!([stream]),
        MESSAGES => json!(state.sent.lock().unwrap().iter().map(|id| json!({
            "uuid": id, "stream_uuid": STREAM, "topic_uuid": TOPIC, "user_uuid": USER,
            "project_id": PROJECT, "author_uuid": USER, "created_at": "2026-11-17T10:00:00Z",
            "payload": {"kind": "markdown", "content": MARKER},
        })).collect::<Vec<_>>()),
        TOPICS | "/api/workspace/v1/events/" | "/api/workspace/v1/users/" => json!([]),
        "/api/workspace/v1/epoch/" => json!({"epoch_generation":"local","epoch_version":1}),
        path if path.ends_with(STREAM) => {
            state.targets.fetch_add(1, Ordering::SeqCst);
            stream
        }
        path if path.ends_with(TOPIC) => {
            json!({"uuid": TOPIC, "stream_uuid": STREAM, "project_id": PROJECT, "user_uuid": USER, "notification_mode":"default"})
        }
        path if path.contains("/folders/") => json!({
            "uuid":"00000000-0000-0000-0000-000000000000", "user_uuid":USER,
            "project_id":PROJECT, "unread_count":0,
        }),
        _ => return json_response(StatusCode::NOT_FOUND, Value::Null),
    };
    json_response(StatusCode::OK, result)
}

/// Возвращает REST-ответ с таким же типом результата, как у рабочего сервера.
fn json_response(status: StatusCode, json: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(json.to_string())))
        .unwrap()
}

/// Дожидается снаружи сети и снова даёт GPUI обработать доставленные задачи.
fn until(
    ui: &mut VisualTestContext,
    step: &str,
    mut ready: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        ui.run_until_parked();
        if ready(ui) {
            return;
        }
        assert!(Instant::now() < deadline, "Нет результата шага: {step}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Нажимает видимый элемент на том же тестовом окне.
fn click(ui: &mut VisualTestContext, selector: &'static str) {
    let point = ui.debug_bounds(selector).unwrap().center();
    ui.simulate_click(point, Modifiers::none());
}

/// Сохранённый вход загружает каталог; мышь и ввод в рабочем окне приводят к одному POST.
#[gpui::test]
fn catalog_click_and_submission_reach_https_queue(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::start();
    let profile = tempfile::tempdir().unwrap();
    let identity = format!("{}\0{USER}", server.address);
    let account_id = Uuid::new_v5(&Uuid::NAMESPACE_URL, identity.as_bytes());
    let setting = AccountRecord {
        context: OwnerContext {
            account_id: AccountId(account_id.to_string()),
            server: server.address.clone(),
            organization_id: ORGANIZATION.into(),
            project_id: PROJECT.into(),
            user_id: USER.into(),
        },
        secret_key: format!("{account_id}:{}", Uuid::nil()),
        signed_out: false,
        retired_secret_keys: vec![],
    };
    let ca =
        CertificateDer::from_pem_slice(include_bytes!["../../shell/background_message/tls/ca.pem"])
            .unwrap();
    let (runtime, events) = RuntimeClient::start_for_test(
        profile.path().into(),
        setting,
        Arc::new(LocalIam),
        Arc::new(LocalSecrets),
        ca.to_vec(),
    )
    .unwrap();
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(780.), px(560.)));
    const ROW: &str = "catalog-chat-10000000-0000-4000-8000-000000000005";
    until(ui, "каталог и готовый WSS", |ui| {
        ui.debug_bounds(ROW).is_some() && server.state.ready.load(Ordering::SeqCst) > 0
    });
    assert!(matches!(
        JsonAccountStore::open(profile.path()),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    click(ui, ROW);
    until(ui, "маршрут из каталога", |ui| {
        ui.read(|app| client.read(app).shell.conversation().is_some())
    });
    until(
        ui,
        "открытие чата и проверка темы",
        |ui| {
            ui.debug_bounds("workspace-composer-editor").is_some()
                && server.state.targets.load(Ordering::SeqCst) >= 2
        },
    );
    click(ui, "workspace-composer-editor");
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string(MARKER.to_owned())));
    ui.simulate_keystrokes("cmd-v");
    ui.run_until_parked();
    click(ui, "conversation-send");
    until(ui, "одна отправка по HTTPS", |_| {
        !server.state.sent.lock().unwrap().is_empty()
    });
    let accepted = server.state.sent.lock().unwrap()[0];
    let address = ui.read(|app| client.read(app).shell.conversation().unwrap().clone());
    until(
        ui,
        "подтверждённая история без записи ожидания",
        |_| {
            let address = address.clone();
            futures::executor::block_on(runtime.request(move |services| async move {
                let owner = &address.route.owner;
                let scope = address.route.scope;
                let generation = address.generation;
                let pending = services.sync.outbox(owner, generation, scope)?;
                let windows = services.sync.history_windows(owner, generation, scope)?;
                Ok::<_, workspace_core::sync::SyncError>(
                    pending.is_empty()
                        && windows
                            .iter()
                            .any(|window| window.messages.iter().any(|row| row.id == accepted)),
                )
            }))
            .is_ok_and(|result| result.unwrap_or(false))
        },
    );
    assert_eq!(server.state.sent.lock().unwrap().len(), 1);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
    assert!(JsonAccountStore::open(profile.path()).is_ok());
}
