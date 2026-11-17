//! Проверяет путь от окна через каталог и очередь до локального HTTPS/WSS-сервера.

use std::{
    convert::Infallible,
    net::{Ipv4Addr, TcpListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
use tokio::sync::{broadcast, oneshot};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::derive_accept_key,
        protocol::{CloseFrame, Role, frame::coding::CloseCode},
    },
};
use uuid::Uuid;
use workspace_core::{
    auth::{
        AccountRecord, AccountStore, IamClient, IamError, IamProject, IamToken, SecretStore,
        StoreError,
    },
    context::OwnerContext,
    messenger::SOURCE_ID,
    storage::{Message as StoredMessage, Storage},
    sync::Event,
};

use super::*;
use crate::{
    gpui_client::{bind_keys, runtime::RuntimeEvents},
    platform::JsonAccountStore,
};

const USER: &str = "10000000-0000-4000-8000-000000000001";
const PEER: &str = "10000000-0000-4000-8000-000000000002";
const PROJECT: &str = "10000000-0000-4000-8000-000000000003";
const ORGANIZATION: &str = "10000000-0000-4000-8000-000000000004";
const STREAM: &str = "10000000-0000-4000-8000-000000000005";
const TOPIC: &str = "10000000-0000-4000-8000-000000000006";
const OLD_ROW: Uuid = Uuid::from_u128(71);
const FRESH_ROW: Uuid = Uuid::from_u128(72);
const PEER_ROW: Uuid = Uuid::from_u128(73);
const MARKER: &str = "Контрольная запись";
const MESSAGES: &str = "/api/workspace/v1/messenger/messages/";
const STREAMS: &str = "/api/workspace/v1/messenger/streams/";
const TOPICS: &str = "/api/workspace/v1/messenger/stream_topics/";
const CHAT_ROW: &str = "catalog-chat-10000000-0000-4000-8000-000000000005";
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
    fn refresh(&self, _: &str, refresh: &str) -> Result<IamToken, IamError> {
        let (user, token) = match refresh {
            "local-refresh" => (USER, "local-user"),
            "local-peer-refresh" => (PEER, "local-peer"),
            _ => unreachable!(),
        };
        Ok(IamToken {
            access_token: token.into(),
            refresh_token: None,
            expires_in: Duration::from_secs(3600),
            user_id: user.into(),
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
        Ok(match key.rsplit_once(':').map(|(_, nonce)| nonce) {
            Some("00000000-0000-0000-0000-000000000000") => Some("local-refresh".into()),
            Some(PEER) => Some("local-peer-refresh".into()),
            _ => None,
        })
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

/// Задерживает ровно первый запрос истории A, пока тест не вернётся в его разговор.
struct DelayedHistory {
    unblock: Mutex<Option<oneshot::Receiver<()>>>, // Канал удержания первого ответа A.
    requested: AtomicBool,                         // Сервер принял запрос первой истории A.
}

/// Переключает только журнал B на новую позицию или эпоху и доставляет живые события.
struct Recovery {
    changed_epoch: bool,  // True меняет эпоху, false обрезает прежнюю.
    rejected: AtomicBool, // После сброса REST отдаёт новую позицию и счётчик B.
    live: broadcast::Sender<RecoverySignal>, // Кадры для сокета B в порядке отправки.
}

/// Определяет, какой кадр должен получить уже подключённый B.
#[derive(Clone)]
enum RecoverySignal {
    RejectCursor, // Сервер больше не принимает прежнюю позицию.
    Event(Value), // Обычное событие журнала, переданное B.
}

impl Recovery {
    /// Даёт текущую начальную позицию и счётчик одного аккаунта.
    fn baseline(&self, user: &str) -> (&'static str, u64, u32) {
        if user == USER {
            return ("local", 1, 4);
        }
        if !self.rejected.load(Ordering::SeqCst) {
            return ("local", 1, 6);
        }
        if self.changed_epoch {
            ("next", 1, 7)
        } else {
            ("local", 50, 7)
        }
    }

    /// Сдвигает журнал B и просит его старый сокет отклонить курсор.
    fn reject(&self) {
        self.rejected.store(true, Ordering::SeqCst);
        assert!(self.live.send(RecoverySignal::RejectCursor).is_ok());
    }

    /// Передаёт B одно штатное событие текущей эпохи.
    fn event(&self, offset: u64, object: &str, action: &str, payload: Value) {
        let (epoch, base, _) = self.baseline(PEER);
        let event = json!({
            "schema_version":1, "uuid":Uuid::new_v4(),
            "epoch_generation":epoch, "epoch_version":base + offset,
            "project_id":PROJECT, "user_uuid":PEER,
            "object_type":object, "action":action, "payload":payload,
        });
        assert!(self.live.send(RecoverySignal::Event(event)).is_ok());
    }

    /// Передаёт B сообщение и следующий за ним новый счётчик непрочитанного.
    fn incoming(&self, id: Uuid, unread: u32) {
        self.event(1, "message", "created", json!({
            "kind":"message.created", "uuid":id, "source_name":"native",
            "is_own":false, "read":false, "author_uuid":USER, "stream_uuid":STREAM, "topic_uuid":TOPIC,
            "payload":{"kind":"markdown", "content":"Строка локального сценария"},
        }));
        self.event(
            2,
            "folder",
            "updated",
            json!({
                "kind":"folder.updated","uuid":Uuid::nil(),"unread_count":unread,
            }),
        );
    }
}

/// Записывает наблюдения сервера о готовности подключений и принятой отправке.
#[derive(Default)]
struct Received {
    sent: Mutex<Vec<Uuid>>, // Публичные UUID реально принятых POST в порядке поступления.
    ready: AtomicUsize,     // Сколько подключений подтвердили готовность через WSS.
    targets: AtomicUsize,   // Сколько проверок доступа к стриму вернул HTTPS.
    delayed: Option<DelayedHistory>, // Сценарий позднего ответа, если тест его включил.
    recovery: Option<Recovery>, // Сценарий сброса курсора, если тест его включил.
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
        Self::serve(Received::default())
    }

    /// Запускает сервер с отдельным сигналом выдачи первого ответа A.
    fn start_with_delayed_history() -> (Self, oneshot::Sender<()>) {
        let (release, unblock) = oneshot::channel();
        let server = Self::serve(Received {
            delayed: Some(DelayedHistory {
                unblock: Mutex::new(Some(unblock)),
                requested: AtomicBool::new(false),
            }),
            ..Received::default()
        });
        (server, release)
    }

    /// Запускает сервер с управляемым сбросом журнала B.
    fn start_with_recovery(changed_epoch: bool) -> Self {
        Self::serve(Received {
            recovery: Some(Recovery {
                changed_epoch,
                rejected: AtomicBool::new(false),
                live: broadcast::channel(16).0,
            }),
            ..Received::default()
        })
    }

    /// Подключает сценарий к обычным HTTPS/WSS-сокетам клиента.
    fn serve(state: Received) -> Self {
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = format!("https://{}", socket.local_addr().unwrap());
        socket.set_nonblocking(true).unwrap();
        let (stop, mut stopping) = oneshot::channel();
        let state = Arc::new(state);
        let received = state.clone();
        // Тестовые сертификаты доверены только клиенту локального сценария.
        let cert = CertificateDer::from_pem_slice(include_bytes!(
            "../../../../../tests/fixtures/tls/localhost.pem"
        ))
        .unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!(
            "../../../../../tests/fixtures/tls/localhost-key.pem"
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
        let protocol = request.headers()[header::SEC_WEBSOCKET_PROTOCOL]
            .to_str()
            .unwrap();
        let two_accounts = state.delayed.is_some() || state.recovery.is_some();
        let user = match protocol {
            "workspace.events.v1, bearer.local-user" => USER,
            "workspace.events.v1, bearer.local-peer" if two_accounts => PEER,
            _ => panic!("WSS должен использовать тестовый вход открытого профиля"),
        };
        let (epoch, base, _) = state
            .recovery
            .as_ref()
            .map_or(("local", 1, 0), |r| r.baseline(user));
        let mut live = None;
        let mut after = base;
        if let Some(recovery) = state.recovery.as_ref() {
            let cursor =
                query_value(&request, "last_epoch_version").and_then(|v| v.parse::<u64>().ok());
            if query_value(&request, "epoch_generation") != Some(epoch)
                || cursor.is_none_or(|version| version < base)
            {
                return json_response(StatusCode::GONE, json!({"error":"epoch_pruned"}));
            }
            after = cursor.unwrap();
            if user == PEER {
                live = Some((recovery.live.subscribe(), recovery.changed_epoch));
            }
        }
        let key = request.headers().get(header::SEC_WEBSOCKET_KEY).unwrap();
        let accept = derive_accept_key(key.as_bytes());
        let upgrade = hyper::upgrade::on(&mut request);
        tokio::spawn(async move {
            let socket = upgrade.await.unwrap();
            let mut ws =
                WebSocketStream::from_raw_socket(TokioIo::new(socket), Role::Server, None).await;
            let ready = json!({"type":"ready","epoch_generation":epoch,"epoch_version":after});
            ws.send(Message::text(ready.to_string())).await.unwrap();
            state.ready.fetch_add(1, Ordering::SeqCst);
            if let Some((mut live, changes_epoch)) = live {
                while let Ok(signal) = live.recv().await {
                    match signal {
                        RecoverySignal::RejectCursor => {
                            if changes_epoch {
                                let frame = json!({"error":"epoch_pruned","reason":"epoch_generation_changed","epoch_generation":"next"});
                                let _ = ws.send(Message::text(frame.to_string())).await;
                            } else {
                                let _ = ws
                                    .close(Some(CloseFrame {
                                        code: CloseCode::from(4410),
                                        reason: "epoch_pruned".into(),
                                    }))
                                    .await;
                            }
                            return;
                        }
                        RecoverySignal::Event(frame) => {
                            ws.send(Message::text(frame.to_string())).await.unwrap()
                        }
                    }
                }
            } else {
                futures::future::pending::<()>().await;
            }
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

    let (user, peer) = match request.headers()[header::AUTHORIZATION].to_str().unwrap() {
        "Bearer local-user" => (USER, PEER),
        "Bearer local-peer" if state.delayed.is_some() || state.recovery.is_some() => (PEER, USER),
        _ => panic!("HTTPS должен использовать тестовый вход открытого профиля"),
    };
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
        "uuid": STREAM, "project_id": PROJECT, "user_uuid": user, "name":"Тестовый чат",
        "private": true, "direct_user_uuid": peer, "is_archived": false,
        "default_topic_uuid": TOPIC, "encryption": false, "notification_mode":"all_messages",
    });
    let (epoch, base, unread) = state
        .recovery
        .as_ref()
        .map_or(("local", 1, 0), |r| r.baseline(user));
    let result = match path {
        STREAMS => json!([stream]),
        MESSAGES if state.delayed.is_some() => {
            let delayed = state.delayed.as_ref().unwrap();
            let id = if user == USER {
                let waiting = delayed.unblock.lock().unwrap().take();
                if let Some(waiting) = waiting {
                    delayed.requested.store(true, Ordering::SeqCst);
                    let _ = waiting.await;
                    OLD_ROW
                } else {
                    FRESH_ROW
                }
            } else {
                PEER_ROW
            };
            json!([{
                "uuid": id, "stream_uuid": STREAM, "topic_uuid": TOPIC,
                "user_uuid": user, "project_id": PROJECT, "author_uuid": peer,
                "created_at": "2026-11-17T10:00:00Z",
                "payload": {"kind":"markdown", "content":"Строка тестовой истории"},
            }])
        }
        MESSAGES => json!(state.sent.lock().unwrap().iter().map(|id| json!({
            "uuid": id, "stream_uuid": STREAM, "topic_uuid": TOPIC, "user_uuid": USER,
            "project_id": PROJECT, "author_uuid": USER, "created_at": "2026-11-17T10:00:00Z",
            "payload": {"kind": "markdown", "content": MARKER},
        })).collect::<Vec<_>>()),
        "/api/workspace/v1/events/" if state.recovery.is_some() => {
            let cursor =
                query_value(&request, "epoch_version%3E").and_then(|v| v.parse::<u64>().ok());
            if query_value(&request, "epoch_generation") != Some(epoch)
                || cursor.is_none_or(|version| version < base)
            {
                return json_response(StatusCode::GONE, json!({"error":"epoch_pruned"}));
            }
            json!([])
        }
        TOPICS | "/api/workspace/v1/events/" | "/api/workspace/v1/users/" => json!([]),
        "/api/workspace/v1/epoch/" => json!({"epoch_generation":epoch,"epoch_version":base}),
        path if path.ends_with(STREAM) => {
            state.targets.fetch_add(1, Ordering::SeqCst);
            stream
        }
        path if path.ends_with(TOPIC) => {
            json!({"uuid": TOPIC, "stream_uuid": STREAM, "project_id": PROJECT, "user_uuid": user, "notification_mode":"default"})
        }
        path if path.contains("/folders/") => json!({
            "uuid":"00000000-0000-0000-0000-000000000000", "user_uuid":user,
            "project_id":PROJECT, "unread_count":unread,
        }),
        _ => return json_response(StatusCode::NOT_FOUND, Value::Null),
    };
    json_response(StatusCode::OK, result)
}

/// Возвращает значение параметра курсора из запроса локального клиента.
fn query_value<'a>(request: &'a Request<Incoming>, name: &str) -> Option<&'a str> {
    request.uri().query()?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
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
    let point = ui
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Нет элемента для клика: {selector}"))
        .center();
    ui.simulate_click(point, Modifiers::none());
}

/// Создаёт запись одного входа с тем же ID, который проверяет восстановление профиля.
fn saved_account(server: &Server, user: &str, nonce: Uuid) -> AccountRecord {
    let identity = format!("{}\0{user}", server.address);
    let account_id = Uuid::new_v5(&Uuid::NAMESPACE_URL, identity.as_bytes());
    AccountRecord {
        context: OwnerContext {
            account_id: AccountId(account_id.to_string()),
            server: server.address.clone(),
            organization_id: ORGANIZATION.into(),
            project_id: PROJECT.into(),
            user_id: user.into(),
        },
        secret_key: format!("{account_id}:{nonce}"),
        signed_out: false,
        retired_secret_keys: vec![],
    }
}

/// Подключает рабочий поток к локальному серверу и отдельно сохранённому профилю.
fn start_runtime(
    profile: &std::path::Path,
    account: AccountRecord,
) -> (RuntimeClient, RuntimeEvents) {
    let ca =
        CertificateDer::from_pem_slice(include_bytes!["../../../../../tests/fixtures/tls/ca.pem"])
            .unwrap();
    RuntimeClient::start_for_test(
        profile.into(),
        account,
        Arc::new(LocalIam),
        Arc::new(LocalSecrets),
        ca.to_vec(),
    )
    .unwrap()
}

/// Сохранённый вход загружает каталог; мышь и ввод в рабочем окне приводят к одному POST.
#[gpui::test]
fn catalog_click_and_submission_reach_https_queue(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::start();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(780.), px(560.)));
    until(ui, "каталог и готовый WSS", |ui| {
        ui.debug_bounds(CHAT_ROW).is_some() && server.state.ready.load(Ordering::SeqCst) > 0
    });
    assert!(matches!(
        JsonAccountStore::open(profile.path()),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    click(ui, CHAT_ROW);
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

/// Находит реальную кнопку по текущему снимку аккаунтов, без предположения об их порядке.
fn click_account(client: &Entity<Client>, ui: &mut VisualTestContext, account: &AccountId) {
    let index = ui.read(|app| {
        client
            .read(app)
            .shell
            .accounts()
            .iter()
            .position(|row| &row.session.owner.account_id == account)
            .expect("В переключателе нет подготовленного аккаунта")
    });
    let selector = ["workspace-account-0", "workspace-account-1"][index];
    let button = ui
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Нет кнопки аккаунта с индексом {index}"));
    ui.simulate_click(button.center(), Modifiers::none());
    until(
        ui,
        "клик переключает аккаунт и подставляет его каталог",
        |ui| {
            ui.read(|app| {
                let shell = &client.read(app).shell;
                shell
                    .selected()
                    .is_some_and(|row| &row.owner.account_id == account)
                    && shell.selected_catalog().is_some()
            }) && ui.debug_bounds(CHAT_ROW).is_some()
        },
    );
}

/// Удерживает первый ответ A до возврата из B и проверяет видимые строки и поколение открытия.
#[gpui::test]
fn late_history_keeps_the_reopened_account_conversation(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let (server, release) = Server::start_with_delayed_history();
    let profile = tempfile::tempdir().unwrap();
    let a = saved_account(&server, USER, Uuid::nil());
    let b = saved_account(&server, PEER, PEER.parse().unwrap());
    JsonAccountStore::open(profile.path())
        .unwrap()
        .save(&b)
        .unwrap();
    let a_id = a.context.account_id.clone();
    let b_id = b.context.account_id;
    let (runtime, events) = start_runtime(profile.path(), a);
    let (client, ui) = cx.add_window_view(|_, cx| {
        Client::new(
            Options {
                active: Some(a_id.clone()),
                ..Options::default()
            },
            runtime.clone(),
            cx,
        )
    });
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(780.), px(560.)));
    until(
        ui,
        "оба сохранённых входа начали WSS, каталог A показан",
        |ui| server.state.ready.load(Ordering::SeqCst) >= 2 && ui.debug_bounds(CHAT_ROW).is_some(),
    );

    click(ui, CHAT_ROW);
    let delayed = server.state.delayed.as_ref().unwrap();
    until(
        ui,
        "сервер удерживает первый ответ истории A",
        |_| delayed.requested.load(Ordering::SeqCst),
    );
    // Тест держит старый экран живым: так можно дождаться, что ответ действительно дошёл до него.
    let first = ui.read(|app| client.read(app).conversation.as_ref().unwrap().clone());
    let first_address = ui.read(|app| first.read(app).address().clone());
    assert_eq!(
        first_address.route.owner.account_id, a_id,
        "первый разговор открыт в A"
    );

    click_account(&client, ui, &b_id);
    click(ui, CHAT_ROW);
    until(
        ui,
        "история B показала строку B",
        |ui| {
            ui.read(|app| {
                client.read(app).conversation.as_ref().is_some_and(|view| {
                    let view = view.read(app);
                    view.address().route.owner.account_id == b_id
                        && view.message_visible_for_test(PEER_ROW)
                })
            })
        },
    );

    click_account(&client, ui, &a_id);
    click(ui, CHAT_ROW);
    until(
        ui,
        "повторное открытие A показало свежую строку",
        |ui| {
            ui.read(|app| {
                client.read(app).conversation.as_ref().is_some_and(|view| {
                    let view = view.read(app);
                    view.address().route.owner.account_id == a_id
                        && view.message_visible_for_test(FRESH_ROW)
                })
            })
        },
    );
    let current = ui.read(|app| {
        client
            .read(app)
            .conversation
            .as_ref()
            .unwrap()
            .read(app)
            .address()
            .clone()
    });
    assert_eq!(
        current.route, first_address.route,
        "снова открыт тот же разговор A"
    );
    assert_eq!(
        current.generation, first_address.generation,
        "между открытиями вход A не менялся"
    );
    assert_ne!(
        current.revision, first_address.revision,
        "ревизия открытия A должна измениться"
    );
    assert!(
        !ui.read(|app| first.read(app).has_access()),
        "ответ первого открытия A ещё удерживается сервером"
    );

    release
        .send(())
        .expect("локальный сервер всё ещё ждёт выдачи ответа A");
    until(
        ui,
        "первый ответ A дошёл до прежнего экрана",
        |ui| ui.read(|app| first.read(app).has_access()),
    );
    ui.read(|app| {
        let client = client.read(app);
        let visible = client
            .conversation
            .as_ref()
            .expect("повторное открытие A должно остаться в окне")
            .read(app);
        assert_eq!(
            visible.address(),
            &current,
            "поздний ответ не должен изменить адрес экрана"
        );
        assert!(
            visible.message_visible_for_test(FRESH_ROW),
            "поздний ответ затмил свежую строку {FRESH_ROW}"
        );
        assert!(
            !visible.message_visible_for_test(OLD_ROW),
            "поздняя строка {OLD_ROW} попала в новое открытие A"
        );
    });
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean,
        "останавливаем фон после обоих ответов"
    );
}

/// Ждёт наблюдаемое состояние хранилища через штатную фоновую работу.
async fn wait_storage(step: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(TIMEOUT, async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Не дождались шага: {step}"));
}

/// Проверяет реальный отказ по WSS, новый курсор и продолжение без потери данных A.
#[tokio::test]
async fn local_wss_recovers_pruned_cursor_and_changed_epoch() {
    for changed_epoch in [false, true] {
        let server = Server::start_with_recovery(changed_epoch);
        let recovery = server.state.recovery.as_ref().unwrap();
        let profile = tempfile::tempdir().unwrap();
        let a = saved_account(&server, USER, Uuid::nil());
        let b = saved_account(&server, PEER, PEER.parse().unwrap());
        let owner_a = a.context.clone();
        let owner_b = b.context.clone();
        JsonAccountStore::open(profile.path())
            .unwrap()
            .save(&b)
            .unwrap();
        let (runtime, _events) = start_runtime(profile.path(), a);
        runtime.ready().await.unwrap();
        let storage = Storage::open(profile.path().join("messenger.sqlite3")).unwrap();
        wait_storage("оба исходных WSS и счётчики", || {
            server.state.ready.load(Ordering::SeqCst) == 2
                && storage.unread(&owner_a).unwrap() == Some(4)
                && storage.unread(&owner_b).unwrap() == Some(6)
        })
        .await;
        let original_a = owner_a.clone();
        runtime
            .request(move |services| async move {
                let generation = services
                    .auth
                    .snapshot()
                    .into_iter()
                    .find(|s| s.context == original_a)
                    .unwrap()
                    .generation;
                let mut cursor = services
                    .sync
                    .cursor(&original_a, SOURCE_ID)
                    .unwrap()
                    .unwrap();
                cursor.epoch_version += 1;
                services
                    .sync
                    .apply(
                        &original_a,
                        generation,
                        SOURCE_ID,
                        Event::MessageCreated {
                            cursor,
                            message: StoredMessage {
                                id: PEER_ROW.to_string(),
                                text: "Данные A".into(),
                            },
                            placement: None,
                            metadata: None,
                        },
                    )
                    .unwrap();
            })
            .await
            .unwrap();
        let cursor_a = storage.cursor(&owner_a, SOURCE_ID).unwrap();
        recovery.incoming(OLD_ROW, 7);
        wait_storage("сообщение B до сброса", || {
            storage
                .message(&owner_b, &OLD_ROW.to_string())
                .unwrap()
                .is_some()
                && storage.unread(&owner_b).unwrap() == Some(7)
        })
        .await;

        recovery.reject();
        let (epoch, base, _) = recovery.baseline(PEER);
        wait_storage(
            "B получил новую позицию и переподключился",
            || {
                server.state.ready.load(Ordering::SeqCst) >= 3
                    && storage
                        .cursor(&owner_b, SOURCE_ID)
                        .unwrap()
                        .is_some_and(|c| c.epoch_generation == epoch && c.epoch_version == base)
                    && storage
                        .message(&owner_b, &OLD_ROW.to_string())
                        .unwrap()
                        .is_none()
                    && storage.unread(&owner_b).unwrap() == Some(7)
            },
        )
        .await;

        recovery.incoming(FRESH_ROW, 8);
        wait_storage(
            "новое сообщение B записано после восстановления",
            || {
                storage
                    .message(&owner_b, &FRESH_ROW.to_string())
                    .unwrap()
                    .is_some()
                    && storage.unread(&owner_b).unwrap() == Some(8)
            },
        )
        .await;
        if changed_epoch {
            recovery.event(
                3,
                "message",
                "updated",
                json!({
                    "kind":"message.updated","uuid":FRESH_ROW,
                    "payload":{"kind":"markdown","content":"Обновлённая строка"},
                }),
            );
            wait_storage("правка B после смены эпохи", || {
                storage
                    .message(&owner_b, &FRESH_ROW.to_string())
                    .unwrap()
                    .is_some_and(|row| row.text == "Обновлённая строка")
            })
            .await;
            recovery.event(
                4,
                "message",
                "deleted",
                json!({"kind":"message.deleted","uuid":FRESH_ROW}),
            );
            wait_storage(
                "удаление B после смены эпохи",
                || {
                    storage
                        .message(&owner_b, &FRESH_ROW.to_string())
                        .unwrap()
                        .is_none()
                },
            )
            .await;
            recovery.event(5, "message", "created", json!({
                "kind":"message.created","uuid":OLD_ROW,"source_name":"native",
                "is_own":false,"read":false,"author_uuid":USER,"stream_uuid":STREAM,"topic_uuid":TOPIC,
                "payload":{"kind":"markdown","content":"Другая строка"},
            }));
            wait_storage(
                "другая строка B перед отзывом",
                || {
                    storage
                        .message(&owner_b, &OLD_ROW.to_string())
                        .unwrap()
                        .is_some()
                },
            )
            .await;
            recovery.event(
                6,
                "stream",
                "deleted",
                json!({"kind":"stream.deleted","uuid":STREAM}),
            );
            wait_storage(
                "отзыв доступа очищает историю B",
                || {
                    storage
                        .message(&owner_b, &OLD_ROW.to_string())
                        .unwrap()
                        .is_none()
                },
            )
            .await;
        }
        assert_eq!(storage.cursor(&owner_a, SOURCE_ID).unwrap(), cursor_a);
        assert_eq!(storage.unread(&owner_a).unwrap(), Some(4));
        assert!(
            storage
                .message(&owner_a, &PEER_ROW.to_string())
                .unwrap()
                .is_some()
        );
        assert_eq!(runtime.shutdown().await, RuntimeExit::Clean);
    }
}
