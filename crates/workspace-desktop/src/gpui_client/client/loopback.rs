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
use gpui::{ClipboardItem, Focusable, Modifiers, TestAppContext, VisualTestContext, px, size};
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
    messenger::{SOURCE_ID, details::ConversationDetails},
    storage::{Message as StoredMessage, Storage},
    sync::Event,
};

use super::*;
use crate::{
    gpui_client::{bind_keys, contracts::ConversationAddress, runtime::RuntimeEvents},
    platform::JsonAccountStore,
};

use crate::gpui_client::right_panel::{PanelKey, Scope};

const USER: &str = "10000000-0000-4000-8000-000000000001";
const PEER: &str = "10000000-0000-4000-8000-000000000002";
const PROJECT: &str = "10000000-0000-4000-8000-000000000003";
const ORGANIZATION: &str = "10000000-0000-4000-8000-000000000004";
const STREAM: &str = "10000000-0000-4000-8000-000000000005";
const TOPIC: &str = "10000000-0000-4000-8000-000000000006";
const CHANNEL: &str = "10000000-0000-4000-8000-000000000007";
const CHANNEL_DEFAULT: &str = "10000000-0000-4000-8000-000000000008";
const CHANNEL_TOPIC: &str = "10000000-0000-4000-8000-000000000009";
const OLD_ROW: Uuid = Uuid::from_u128(71);
const FRESH_ROW: Uuid = Uuid::from_u128(72);
const PEER_ROW: Uuid = Uuid::from_u128(73);
const MARKER: &str =
    "Первая строка синтетического черновика.\nВторая строка для проверки роста.\nТретья строка.";
const MESSAGES: &str = "/api/workspace/v1/messenger/messages/";
const STREAMS: &str = "/api/workspace/v1/messenger/streams/";
const TOPICS: &str = "/api/workspace/v1/messenger/stream_topics/";
const FOLDERS: &str = "/api/workspace/v1/messenger/folders/";
const CHAT_ROW: &str = "catalog-chat-10000000-0000-4000-8000-000000000005";
const CHANNEL_ROW: &str = "catalog-chat-10000000-0000-4000-8000-000000000007";
const CHANNEL_TOPIC_ROW: &str =
    "catalog-topic-10000000-0000-4000-8000-000000000007-10000000-0000-4000-8000-000000000009";
const CHANNEL_HISTORY: Uuid = Uuid::from_u128(84);
const DETAILS_TRIGGER: &str = "conversation-details-trigger";
const DETAILS_PANEL: &str = "conversation-details-panel";
const DETAILS_FIRST_ROW: u128 = 200;
const DETAILS_LAST_ROW: u128 = 240;
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

/// Подтверждённый HTTP POST в тему с привязкой к тестовому аккаунту.
struct TopicPost {
    author: &'static str, // Вход из токена запроса.
    id: Uuid,             // Публичный UUID из канонического UUID и явной темы.
}

/// Состояние темы, управляемое тестом перед проверкой редактора или очереди.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum TopicGate {
    #[default]
    Open, // Канал допускает обычный текст, тема открыта.
    Closed,      // Тема закрылась.
    Encrypted,   // Канал требует защищённые сообщения.
    WrongStream, // Тема в ответе принадлежит другому стриму.
    Unknown,     // В ответе отсутствует признак is_done.
}

/// Наблюдения за попытками записи в явную тему канала.
#[derive(Default)]
struct TopicAcceptance {
    gate: Mutex<TopicGate>,      // Текущее состояние на локальном сервере.
    attempts: AtomicUsize,       // Все попытки POST, даже с неверным адресом.
    sent: Mutex<Vec<TopicPost>>, // Подтверждённые POST в тему.
}

/// Управляет ответами профиля и проверяет, какого пользователя запросил каждый вход.
#[derive(Default)]
struct DetailsFixture {
    users: Mutex<Vec<(&'static str, &'static str)>>, // Токен владельца и UUID собеседника.
    unblock: Mutex<Option<oneshot::Receiver<()>>>,   // Удерживает ровно один ответ профиля.
    denied_topic: AtomicBool, // Возвращает 403 для темы, которую уже показывает окно.
    omit_stream_user: AtomicBool, // Имитирует старый ответ, который не подтверждает владельца.
}

/// Записывает наблюдения сервера о готовности подключений и принятой отправке.
#[derive(Default)]
struct Received {
    sent: Mutex<Vec<Uuid>>, // Публичные UUID реально принятых POST в порядке поступления.
    ready: AtomicUsize,     // Сколько подключений подтвердили готовность через WSS.
    targets: AtomicUsize,   // Сколько проверок доступа к стриму вернул HTTPS.
    delayed: Option<DelayedHistory>, // Сценарий позднего ответа, если тест его включил.
    recovery: Option<Recovery>, // Сценарий сброса курсора, если тест его включил.
    topic: Option<TopicAcceptance>, // Канал с явной темой для UI-12.
    details: Option<DetailsFixture>, // Ответы для правой панели UI-13.
    folders: Mutex<HashMap<(&'static str, Uuid), Value>>, // Папки, созданные в тестовых входах.
    folder_posts: AtomicUsize, // Число запросов создания, дошедших через HTTPS.
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

    /// Запускает два сохранённых входа с доступом к тестовому каналу.
    fn start_with_topic() -> Self {
        Self::serve(Received {
            topic: Some(TopicAcceptance::default()),
            ..Received::default()
        })
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
        let two_accounts =
            state.delayed.is_some() || state.recovery.is_some() || state.topic.is_some();
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
        "Bearer local-peer"
            if state.delayed.is_some() || state.recovery.is_some() || state.topic.is_some() =>
        {
            (PEER, USER)
        }
        _ => panic!("HTTPS должен использовать тестовый вход открытого профиля"),
    };
    if path == FOLDERS && request.method() == Method::POST {
        let bytes = request.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let id: Uuid = serde_json::from_value(body["uuid"].clone()).unwrap();
        let mut folder = folder_row(id, body["title"].as_str().unwrap(), "created", 0);
        folder["background_color_value"] = body["background_color_value"].clone();
        state.folder_posts.fetch_add(1, Ordering::SeqCst);
        state
            .folders
            .lock()
            .unwrap()
            .insert((user, id), folder.clone());
        return json_response(StatusCode::CREATED, folder);
    }
    if path == MESSAGES && request.method() == Method::POST {
        let bytes = request.into_body().collect().await.unwrap().to_bytes();
        let post: Value = serde_json::from_slice(&bytes).unwrap();
        if let Some(acceptance) = &state.topic {
            acceptance.attempts.fetch_add(1, Ordering::SeqCst);
            assert_eq!(post["stream_uuid"], CHANNEL);
            assert_eq!(post["topic_uuid"], CHANNEL_TOPIC);
            assert_eq!(post["payload"]["content"], MARKER);
            let canonical = post["uuid"].as_str().unwrap();
            let id = Uuid::new_v5(
                &Uuid::parse_str(CHANNEL_TOPIC).unwrap(),
                canonical.as_bytes(),
            );
            acceptance
                .sent
                .lock()
                .unwrap()
                .push(TopicPost { author: user, id });
            return json_response(
                StatusCode::CREATED,
                topic_message(user, user, id, "2026-11-17T10:01:00Z", MARKER),
            );
        }
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

    if let Some(details) = &state.details {
        if path == format!("/api/workspace/v1/users/{peer}") {
            assert_eq!(request.method(), Method::GET);
            details.users.lock().unwrap().push((user, peer));
            let waiting = details.unblock.lock().unwrap().take();
            let name = if let Some(waiting) = waiting {
                let _ = waiting.await;
                "Устаревший профиль"
            } else if peer == PEER {
                "Собеседник A"
            } else {
                "Собеседник B"
            };
            return json_response(
                StatusCode::OK,
                json!({
                    "uuid": peer, "username":"peer", "first_name":name, "last_name":null,
                    "email":"peer@fixture.test", "status":"idle",
                }),
            );
        }
        if path == format!("{TOPICS}{CHANNEL_TOPIC}") && details.denied_topic.load(Ordering::SeqCst)
        {
            return json_response(StatusCode::FORBIDDEN, Value::Null);
        }
    }

    let mut stream = json!({
        "uuid": STREAM, "project_id": PROJECT, "user_uuid": user, "name":"Тестовый чат",
        "private": true, "direct_user_uuid": peer, "is_archived": false,
        "default_topic_uuid": TOPIC, "encryption": false, "notification_mode":"all_messages",
    });
    if state.details.as_ref().is_some_and(|details| {
        details.omit_stream_user.load(Ordering::SeqCst) && path == format!("{STREAMS}{STREAM}")
    }) {
        stream.as_object_mut().unwrap().remove("user_uuid");
    }
    let (epoch, base, unread) = state
        .recovery
        .as_ref()
        .map_or(("local", 1, 0), |r| r.baseline(user));
    let result = match path {
        STREAMS if state.topic.is_some() => json!([stream, topic_channel(user, false)]),
        STREAMS => json!([stream]),
        TOPICS if state.topic.is_some() => json!([channel_topic(user, TopicGate::Open)]),
        MESSAGES
            if state.topic.is_some() && query_value(&request, "stream_uuid") == Some(CHANNEL) =>
        {
            assert_eq!(query_value(&request, "topic_uuid"), Some(CHANNEL_TOPIC));
            let mut rows = vec![topic_message(
                user,
                peer,
                CHANNEL_HISTORY,
                "2026-11-17T10:00:00Z",
                "Строка синтетической темы",
            )];
            rows.extend(
                state
                    .topic
                    .as_ref()
                    .unwrap()
                    .sent
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|post| post.author == user)
                    .map(|post| topic_message(user, user, post.id, "2026-11-17T10:01:00Z", MARKER)),
            );
            json!(rows)
        }
        MESSAGES if state.details.is_some() => json!(
            (DETAILS_FIRST_ROW..DETAILS_LAST_ROW)
                .rev()
                .map(|number| json!({
                    "uuid":Uuid::from_u128(number), "stream_uuid":STREAM, "topic_uuid":TOPIC,
                    "user_uuid":user, "project_id":PROJECT, "author_uuid":if number % 2 == 0 { user } else { peer },
                    "created_at":format!("2026-11-17T10:00:{:02}Z", number - DETAILS_FIRST_ROW),
                    "payload":{
                        "kind":"markdown",
                        "content": "Тестовая строка разной длины. ".repeat(4 + (number as usize) % 5),
                    },
                }))
                .collect::<Vec<_>>()
        ),
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
        path if state.topic.is_some() && path.ends_with(CHANNEL) => {
            state.targets.fetch_add(1, Ordering::SeqCst);
            let gate = *state.topic.as_ref().unwrap().gate.lock().unwrap();
            topic_channel(user, gate == TopicGate::Encrypted)
        }
        path if state.topic.is_some() && path.ends_with(CHANNEL_TOPIC) => {
            let gate = *state.topic.as_ref().unwrap().gate.lock().unwrap();
            channel_topic(user, gate)
        }
        path if path.ends_with(STREAM) => {
            state.targets.fetch_add(1, Ordering::SeqCst);
            stream
        }
        path if path.ends_with(TOPIC) => {
            json!({"uuid": TOPIC, "stream_uuid": STREAM, "project_id": PROJECT, "user_uuid": user, "notification_mode":"default"})
        }
        FOLDERS => {
            let mut rows = vec![folder_row(Uuid::nil(), "Все чаты", "all", unread)];
            rows.extend(state.folders.lock().unwrap().iter()
                .filter(|((owner, _), _)| *owner == user)
                .map(|(_, folder)| folder.clone()));
            json!(rows)
        }
        path if path.starts_with(FOLDERS) => {
            let id: Uuid = path[FOLDERS.len()..].parse().unwrap();
            if id.is_nil() {
                folder_row(id, "Все чаты", "all", unread)
            } else if let Some(folder) = state.folders.lock().unwrap().get(&(user, id)).cloned() {
                folder
            } else {
                return json_response(StatusCode::NOT_FOUND, Value::Null);
            }
        }
        _ => return json_response(StatusCode::NOT_FOUND, Value::Null),
    };
    json_response(StatusCode::OK, result)
}

/// Готовит схему ответа папки, которую читает настоящий Core.
fn folder_row(id: Uuid, title: &str, kind: &str, unread: u32) -> Value {
    json!({
        "uuid": id, "title": title, "system_type": kind, "folder_items": [],
        "unread_count": unread, "created_at":"2026-11-18T12:00:00Z",
        "updated_at":"2026-11-18T12:00:00Z",
    })
}

/// Возвращает открытую конфигурацию канала с отдельной темой по умолчанию.
fn topic_channel(user: &str, encrypted: bool) -> Value {
    json!({
        "uuid": CHANNEL, "project_id": PROJECT, "user_uuid": user, "name": "Канал UI-12",
        "description":"Описание синтетического канала",
        "private": false, "is_archived": false, "default_topic_uuid": CHANNEL_DEFAULT,
        "encryption": encrypted, "notification_mode": "all_messages",
    })
}

/// Возвращает метаданные явной темы для чтения и повторной проверки перед отправкой.
fn channel_topic(user: &str, gate: TopicGate) -> Value {
    let stream = if gate == TopicGate::WrongStream {
        STREAM
    } else {
        CHANNEL
    };
    let mut row = json!({
        "uuid": CHANNEL_TOPIC, "stream_uuid": stream, "project_id": PROJECT,
        "user_uuid": user, "name": "Длинная тема", "notification_mode": "default",
    });
    if gate != TopicGate::Unknown {
        row["is_done"] = json!(gate == TopicGate::Closed);
    }
    row
}

/// Формирует подтверждённую строку истории явной темы.
fn topic_message(user: &str, author: &str, id: Uuid, created: &str, content: &str) -> Value {
    json!({
        "uuid": id, "stream_uuid": CHANNEL, "topic_uuid": CHANNEL_TOPIC,
        "user_uuid": user, "project_id": PROJECT, "author_uuid": author,
        "created_at": created, "payload": {"kind": "markdown", "content": content},
    })
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

/// Доходит клавишей Tab до поиска, включая новые кнопки папок в общем обходе.
fn tab_to_catalog_search(client: &Entity<Client>, ui: &mut VisualTestContext) {
    for _ in 0..24 {
        if ui.update(|window, app| {
            client
                .read(app)
                .sidebar
                .read(app)
                .tested_search_focused(window, app)
        }) {
            return;
        }
        ui.simulate_keystrokes("tab");
    }
    panic!("Tab не дошёл до поиска каталога");
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

/// Создаёт папку из рабочего окна и ждёт её возвращения через снимок Core.
#[gpui::test]
fn folder_creation_reaches_https_and_returns_through_the_core_snapshot(cx: &mut TestAppContext) {
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
    ui.simulate_resize(size(px(1000.), px(740.)));
    until(
        ui,
        "панель папок активного входа",
        |ui| {
            ui.debug_bounds("folder-rail-create").is_some()
                && server.state.ready.load(Ordering::SeqCst) > 0
        },
    );
    click(ui, "folder-rail-create");
    until(ui, "форма новой папки", |ui| {
        ui.debug_bounds("folder-dialog").is_some()
    });
    ui.simulate_input("Папка проверки");
    click(ui, "folder-dialog-submit");
    until(
        ui,
        "снимок созданной папки и закрытие формы",
        |ui| {
            let id = server
                .state
                .folders
                .lock()
                .unwrap()
                .keys()
                .next()
                .map(|(_, id)| *id);
            id.is_some_and(|id| {
                ui.read(|app| {
                    let sidebar = client.read(app).sidebar.read(app);
                    !sidebar.folder_dialog().read(app).is_open() && sidebar.tested_has_folder(id)
                })
            })
        },
    );
    assert_eq!(server.state.folder_posts.load(Ordering::SeqCst), 1);
    let folders = server.state.folders.lock().unwrap();
    let ((owner, _), folder) = folders.iter().next().unwrap();
    assert_eq!(*owner, USER);
    assert_eq!(folder["title"], "Папка проверки");
}

/// Открывает чат, проверяет редактор после сужения и сохраняет черновик при повторной попытке.
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
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(ui, "каталог и готовый WSS", |ui| {
        ui.debug_bounds(CHAT_ROW).is_some() && server.state.ready.load(Ordering::SeqCst) > 0
    });
    assert!(matches!(
        JsonAccountStore::open(profile.path()),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    // Идём от первой кнопки оболочки к меню, затем через поле поиска к строке.
    ui.update(|window, app| window.focus(&client.read(app).initial_focus));
    ui.simulate_keystrokes("tab tab enter");
    ui.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse("enter").unwrap(),
    });
    ui.run_until_parked();
    assert!(ui.read(|app| client.read(app).top_bar.read(app).menu_is_open()));
    ui.simulate_keystrokes("escape");
    tab_to_catalog_search(&client, ui);
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string("Тест".to_owned())));
    ui.simulate_keystrokes("cmd-v");
    ui.run_until_parked();
    assert!(ui.debug_bounds("catalog-clear").is_some());
    ui.simulate_keystrokes("down enter");
    ui.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse("enter").unwrap(),
    });
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
    let row_focus = ui.update(|window, app| window.focused(app).unwrap());
    let conversation = ui.read(|app| client.read(app).conversation.as_ref().unwrap().clone());
    ui.simulate_keystrokes("tab");
    ui.run_until_parked();
    assert_eq!(
        ui.update(|window, app| conversation.read(app).tested_details_focus(window, app)),
        Some("сведения")
    );
    ui.simulate_keystrokes("tab");
    ui.run_until_parked();
    let history_focus = ui.update(|window, app| window.focused(app).unwrap());
    assert_eq!(
        ui.update(|window, app| conversation.read(app).keyboard_focus(window, app)),
        Some("история")
    );
    assert_ne!(history_focus, row_focus);
    ui.simulate_keystrokes("tab");
    ui.run_until_parked();
    let editor_focus = ui.update(|window, app| window.focused(app).unwrap());
    assert_eq!(
        ui.update(|window, app| conversation.read(app).keyboard_focus(window, app)),
        Some("редактор")
    );
    assert_ne!(editor_focus, history_focus);
    ui.simulate_keystrokes("shift-tab");
    ui.run_until_parked();
    assert!(ui.update(|window, _| history_focus.is_focused(window)));
    ui.simulate_keystrokes("shift-tab");
    ui.run_until_parked();
    assert_eq!(
        ui.update(|window, app| conversation.read(app).tested_details_focus(window, app)),
        Some("сведения")
    );
    ui.simulate_keystrokes("shift-tab");
    ui.run_until_parked();
    assert!(ui.update(|window, _| row_focus.is_focused(window)));
    ui.simulate_keystrokes("tab tab tab");
    ui.run_until_parked();
    assert!(ui.update(|window, _| editor_focus.is_focused(window)));
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string(MARKER.to_owned())));
    ui.simulate_keystrokes("cmd-v");
    ui.run_until_parked();

    // Сужаем то же окно с черновиком и убеждаемся, что отправляется тот же текст.
    for (width, height, catalog_width) in [(1440., 900., 299.), (700., 600., 280.)] {
        ui.simulate_resize(size(px(width), px(height)));
        ui.run_until_parked();
        let catalog = ui.debug_bounds("workspace-sidebar-panel").unwrap();
        let history = ui.debug_bounds("workspace-conversation-panel").unwrap();
        assert_eq!(catalog.left(), px(76.));
        assert_eq!(catalog.size.width, px(catalog_width));
        assert_eq!(history.left(), catalog.right() + px(4.));
        assert_eq!(history.right(), px(width));
        assert!(ui.read(|app| client.read(app).conversation.as_ref() == Some(&conversation)));
        assert!(ui.debug_bounds("workspace-composer-editor").is_some());
    }

    let (composer, restore_save) = ui.update(|_, app| {
        conversation.update(app, |conversation, _| conversation.fail_queue_write())
    });
    let before = ui.debug_bounds("workspace-composer-editor").unwrap();
    assert!(
        before.size.height > px(86.),
        "В 700 px нужен черновик с несколькими переносами"
    );
    ui.simulate_keystrokes("enter");
    until(ui, "пояснение сбоя записи", |ui| {
        ui.debug_bounds("conversation-send-explanation").is_some()
    });
    assert!(server.state.sent.lock().unwrap().is_empty());
    let panel = ui.debug_bounds("workspace-conversation-panel").unwrap();
    assert_eq!(panel.bottom(), px(600.));
    for selector in [
        "conversation-composer",
        "conversation-composer-field",
        "workspace-composer-editor",
        "conversation-send",
        "conversation-send-explanation",
    ] {
        let bounds = ui.debug_bounds(selector).unwrap();
        assert!(
            bounds.top() >= panel.top()
                && bounds.bottom() <= panel.bottom()
                && bounds.left() >= panel.left()
                && bounds.right() <= panel.right(),
            "После ошибки {selector} выходит за границы разговора: {bounds:?}, панель {panel:?}"
        );
    }
    assert_eq!(
        ui.debug_bounds("workspace-composer-editor")
            .unwrap()
            .size
            .height,
        before.size.height
    );
    assert!(ui.update(|window, app| {
        let composer = composer.read(app);
        composer.draft(app) == MARKER && composer.editor().focus_handle(app).is_focused(window)
    }));

    ui.update(|_, app| conversation.update(app, |conversation, _| restore_save(conversation)));
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

/// Передаёт нажатие и отпускание Enter. Кнопки подтверждают действие на отпускании.
fn press_enter(ui: &mut VisualTestContext) {
    ui.simulate_keystrokes("enter");
    ui.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse("enter").unwrap(),
    });
    ui.run_until_parked();
}

/// Проверяет чтение явной темы и открытие редактора с доступной отправкой.
fn topic_ready(
    client: &Entity<Client>,
    ui: &mut VisualTestContext,
    step: &str,
) -> ConversationAddress {
    let topic = CHANNEL_TOPIC.parse().unwrap();
    until(ui, step, |ui| {
        ui.read(|app| {
            client
                .read(app)
                .conversation
                .as_ref()
                .is_some_and(|screen| {
                    let screen = screen.read(app);
                    screen.has_access()
                        && screen.tested_composer().is_some_and(|view| {
                            view.read(app).tested_send_state(app) == (Some(topic), false)
                        })
                })
        }) && ui
            .debug_bounds("history-message-00000000-0000-0000-0000-000000000054")
            .is_some()
            && ui.debug_bounds("workspace-composer-editor").is_some()
    });
    let address = ui.read(|app| client.read(app).shell.conversation().unwrap().clone());
    assert_eq!(address.route.scope.stream_id.to_string(), CHANNEL);
    assert_eq!(address.route.scope.topic_id, Some(topic));
    address
}

/// Ждёт ровно одну закреплённую строку по публичному UUID и пустую очередь.
fn wait_topic_confirmation(
    runtime: &RuntimeClient,
    ui: &mut VisualTestContext,
    address: &ConversationAddress,
    id: Uuid,
) {
    until(
        ui,
        "одно сообщение в истории и пустая очередь",
        |_| {
            let address = address.clone();
            futures::executor::block_on(runtime.request(move |services| async move {
                let owner = &address.route.owner;
                let generation = address.generation;
                let scope = address.route.scope;
                let pending = services.sync.outbox(owner, generation, scope)?;
                let windows = services.sync.history_windows(owner, generation, scope)?;
                let occurrences = windows
                    .iter()
                    .flat_map(|window| &window.messages)
                    .filter(|row| row.id == id)
                    .count();
                Ok::<_, workspace_core::sync::SyncError>(pending.is_empty() && occurrences == 1)
            }))
            .is_ok_and(|result| result.unwrap_or(false))
        },
    );
}

/// Содержимое тестовой панели уровня аккаунта.
struct AccountPanelFixture(&'static str); // Селектор видимого экрана.

impl gpui::Render for AccountPanelFixture {
    /// Помечает видимую панель в тестовом окне.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        let id = self.0;
        div().id(id).debug_selector(move || id.into())
    }
}

/// Открывает панель уровня аккаунта через тот же API, что и рабочие панели.
fn open_account_panel(client: &Entity<Client>, ui: &mut VisualTestContext, id: &'static str) {
    ui.update(|window, app| {
        let client = client.read(app);
        let key = PanelKey {
            scope: Scope::Account(client.shell.selected().unwrap().clone()),
            id: id.into(),
        };
        let opener = client.initial_focus.clone();
        let panel = client.right_panel.clone();
        panel.update(app, |panel, cx| {
            panel.open(
                key,
                "Тестовая панель",
                opener,
                |cx| cx.new(|_| AccountPanelFixture(id)),
                window,
                cx,
            );
        });
    });
    ui.run_until_parked();
}

/// Проверяет доступ к профилю, точную тему, закрытие и фокус через обычный Client.
#[gpui::test]
fn conversation_details_stay_with_the_verified_route(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        topic: Some(TopicAcceptance::default()),
        details: Some(DetailsFixture::default()),
        ..Received::default()
    });
    let details = server.state.details.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let a = saved_account(&server, USER, Uuid::nil());
    let b = saved_account(&server, PEER, PEER.parse().unwrap());
    let a_id = a.context.account_id.clone();
    let b_id = b.context.account_id.clone();
    JsonAccountStore::open(profile.path())
        .unwrap()
        .save(&b)
        .unwrap();
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
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(
        ui,
        "открыты оба синтетических входа",
        |ui| server.state.ready.load(Ordering::SeqCst) >= 2 && ui.debug_bounds(CHAT_ROW).is_some(),
    );

    click(ui, CHAT_ROW);
    until(ui, "личка доступна и видна", |ui| {
        ui.debug_bounds("workspace-composer-editor").is_some()
    });
    let first = ui.read(|app| client.read(app).conversation.as_ref().unwrap().clone());
    let composer = ui.read(|app| first.read(app).tested_composer().unwrap().clone());
    click(ui, "workspace-composer-editor");
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string(MARKER.into())));
    ui.simulate_keystrokes("cmd-v shift-tab pageup");
    ui.run_until_parked();
    assert_eq!(
        ui.update(|window, app| first.read(app).keyboard_focus(window, app)),
        Some("история")
    );
    let (anchor, before) = ui.read(|app| {
        let view = first.read(app);
        (DETAILS_FIRST_ROW + 1..DETAILS_LAST_ROW - 1)
            .filter_map(|number| {
                let id = Uuid::from_u128(number);
                Some((id, view.tested_details_row_y(id)?))
            })
            .min_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
            .expect("В середине истории нужна видимая опора")
    });

    details.omit_stream_user.store(true, Ordering::SeqCst);
    click(ui, DETAILS_TRIGGER);
    until(
        ui,
        "старый ответ стрима не подтверждает владельца",
        |ui| ui.debug_bounds("conversation-details-error").is_some(),
    );
    assert!(details.users.lock().unwrap().is_empty());
    details.omit_stream_user.store(false, Ordering::SeqCst);
    click(ui, "conversation-details-retry");
    until(
        ui,
        "аккаунт A видит проверенный профиль собеседника",
        |ui| ui.debug_bounds("conversation-details-peer").is_some(),
    );
    assert_eq!(details.users.lock().unwrap().as_slice(), &[(USER, PEER)]);
    ui.read(|app| {
        first.read(app).tested_details(app, |details| {
            let Some(ConversationDetails::Peer(profile)) = details else {
                panic!("Ожидался профиль собеседника");
            };
            assert_eq!(profile.display_name, "Собеседник A");
            assert_eq!(profile.email.as_deref(), Some("peer@fixture.test"));
            assert_eq!(
                profile.presence,
                Some(workspace_core::directory::ProfilePresence::Idle)
            );
        })
    });
    ui.read(|app| {
        let y = first
            .read(app)
            .tested_details_row_y(anchor)
            .expect("Опора видна рядом с панелью");
        assert!(
            (y - before).abs() <= 1.,
            "Опора {anchor} сдвинулась: {before} → {y}"
        );
    });
    let panel = ui.debug_bounds(DETAILS_PANEL).unwrap();
    assert_eq!(panel.size.width, px(315.));
    assert!(ui.debug_bounds("conversation-chat").unwrap().right() < panel.left());
    assert_eq!(
        ui.update(|window, app| first.read(app).tested_details_focus(window, app)),
        Some("закрыть")
    );
    open_account_panel(&client, ui, "test-account-main");
    open_account_panel(&client, ui, "test-account-settings");
    assert!(ui.debug_bounds("test-account-settings").is_some());
    assert!(!ui.read(|app| first.read(app).tested_details_is_open(app)));
    click(ui, "right-panel-back");
    click(ui, "right-panel-close");
    assert!(ui.read(|app| first.read(app).tested_details_is_open(app)));
    assert_eq!(details.users.lock().unwrap().as_slice(), &[(USER, PEER)]);
    assert_eq!(
        ui.update(|window, app| first.read(app).tested_details_focus(window, app)),
        Some("закрыть")
    );
    ui.simulate_keystrokes("escape");
    until(ui, "Escape закрыл панель", |ui| {
        !ui.read(|app| first.read(app).tested_details_is_open(app))
    });
    assert_eq!(
        ui.update(|window, app| first.read(app).tested_details_focus(window, app)),
        Some("сведения")
    );
    ui.read(|app| assert_eq!(composer.read(app).draft(app), MARKER));

    let (release, unblock) = oneshot::channel();
    *details.unblock.lock().unwrap() = Some(unblock);
    press_enter(ui);
    until(
        ui,
        "первый профиль после открытия задержан",
        |ui| {
            details.users.lock().unwrap().len() == 2
                && ui.debug_bounds("conversation-details-loading").is_some()
        },
    );
    ui.simulate_keystrokes("escape");
    ui.run_until_parked();
    press_enter(ui);
    until(
        ui,
        "тот же адрес запросил профиль заново",
        |ui| {
            details.users.lock().unwrap().len() == 3
                && ui.debug_bounds("conversation-details-peer").is_some()
        },
    );
    let _ = release.send(());
    ui.run_until_parked();
    ui.read(|app| {
        first.read(app).tested_details(app, |details| {
            assert!(matches!(
                details,
                Some(ConversationDetails::Peer(profile)) if profile.display_name == "Собеседник A"
            ));
        });
    });

    open_account_panel(&client, ui, "test-account-main");
    open_account_panel(&client, ui, "test-account-settings");
    click(ui, "right-panel-close-all");
    ui.read(|app| {
        assert!(!first.read(app).tested_details_is_open(app));
        first
            .read(app)
            .tested_details(app, |details| assert!(details.is_none()));
    });
    assert_eq!(
        ui.update(|window, app| first.read(app).tested_details_focus(window, app)),
        Some("сведения")
    );
    open_account_panel(&client, ui, "test-account-main");
    click_account(&client, ui, &b_id);
    assert!(ui.read(|app| client.read(app).conversation.is_none()));
    ui.read(|app| assert!(client.read(app).right_panel.read(app).is_empty()));
    click(ui, CHAT_ROW);
    until(
        ui,
        "личка B использует другой аккаунт",
        |ui| ui.debug_bounds("workspace-composer-editor").is_some(),
    );
    click(ui, DETAILS_TRIGGER);
    until(
        ui,
        "токен B запросил своего собеседника",
        |ui| {
            details.users.lock().unwrap().last() == Some(&(PEER, USER))
                && ui.debug_bounds("conversation-details-peer").is_some()
        },
    );
    open_account_panel(&client, ui, "test-account-main");
    ui.simulate_resize(size(px(700.), px(600.)));
    ui.run_until_parked();
    click(ui, CHANNEL_ROW);
    ui.run_until_parked();
    click(ui, CHANNEL_TOPIC_ROW);
    let _ = topic_ready(&client, ui, "в B открылась выбранная тема, панель закрыта");
    let topic = ui.read(|app| client.read(app).conversation.as_ref().unwrap().clone());
    ui.read(|app| {
        let client = client.read(app);
        let key = PanelKey {
            scope: Scope::Account(client.shell.selected().unwrap().clone()),
            id: "test-account-main".into(),
        };
        assert!(client.right_panel.read(app).is_active(&key));
    });
    let route_focus = ui.update(|window, app| window.focused(app));
    click(ui, "right-panel-close");
    ui.update(|window, _| {
        if let Some(focus) = route_focus {
            window.focus(&focus);
        }
    });
    ui.read(|app| assert!(client.read(app).right_panel.read(app).is_empty()));
    assert!(!ui.read(|app| topic.read(app).tested_details_is_open(app)));
    let chat_before = ui.debug_bounds("conversation-chat").unwrap();
    let composer_before = ui.debug_bounds("workspace-composer-editor").unwrap();
    for _ in 0..8 {
        ui.simulate_keystrokes("tab");
        if ui
            .update(|window, app| topic.read(app).tested_details_focus(window, app))
            .is_some()
        {
            break;
        }
    }
    assert_eq!(
        ui.update(|window, app| topic.read(app).tested_details_focus(window, app)),
        Some("сведения")
    );
    press_enter(ui);
    until(
        ui,
        "сведения относятся к выбранной, а не к теме по умолчанию",
        |ui| ui.debug_bounds("conversation-details-topic").is_some(),
    );
    ui.read(|app| {
        topic.read(app).tested_details(app, |details| {
            assert!(matches!(
                details,
                Some(ConversationDetails::Topic { channel, name, description: Some(description), done: Some(false) })
                    if channel == "Канал UI-12" && name == "Длинная тема" && description == "Описание синтетического канала"
            ));
        });
    });
    assert_eq!(ui.debug_bounds("conversation-chat").unwrap(), chat_before);
    assert_eq!(
        ui.debug_bounds("workspace-composer-editor").unwrap(),
        composer_before
    );
    let panel = ui.debug_bounds(DETAILS_PANEL).unwrap();
    assert_eq!(panel.right(), chat_before.right());
    assert!(panel.left() >= chat_before.left());
    ui.simulate_keystrokes("tab shift-tab");
    assert_eq!(
        ui.update(|window, app| topic.read(app).tested_details_focus(window, app)),
        Some("закрыть"),
    );
    click(ui, "conversation-details-close");
    assert!(!ui.read(|app| topic.read(app).tested_details_is_open(app)));

    details.denied_topic.store(true, Ordering::SeqCst);
    press_enter(ui);
    until(
        ui,
        "запрет сервера скрывает историю и панель",
        |ui| {
            ui.debug_bounds("history-error").is_some()
                && ui.read(|app| {
                    let topic = topic.read(app);
                    !topic.has_access() && !topic.tested_details_is_open(app)
                })
        },
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Проходит мышью и клавиатурой два входа, читает тему и проверяет доставку без дублей.
#[gpui::test]
fn explicit_topic_accepts_mouse_and_keyboard_in_each_account(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::start_with_topic();
    let acceptance = server.state.topic.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let a = saved_account(&server, USER, Uuid::nil());
    let b = saved_account(&server, PEER, PEER.parse().unwrap());
    JsonAccountStore::open(profile.path())
        .unwrap()
        .save(&b)
        .unwrap();
    let a_id = a.context.account_id.clone();
    let b_id = b.context.account_id.clone();
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
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(ui, "готовы два WSS и каталог", |ui| {
        server.state.ready.load(Ordering::SeqCst) >= 2 && ui.debug_bounds(CHANNEL_ROW).is_some()
    });

    click_account(&client, ui, &b_id);
    click(ui, CHANNEL_ROW);
    ui.run_until_parked();
    click(ui, CHANNEL_TOPIC_ROW);
    let mouse_address = topic_ready(&client, ui, "мышь открыла историю темы и редактор B");
    assert_eq!(mouse_address.route.owner.account_id, b_id);
    assert_eq!(
        ui.debug_bounds("workspace-conversation-panel")
            .unwrap()
            .right(),
        px(1440.)
    );
    click(ui, "workspace-composer-editor");
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string(MARKER.to_owned())));
    ui.simulate_keystrokes("cmd-v");
    ui.run_until_parked();
    click(ui, "conversation-send");
    until(ui, "единственный POST от B", |_| {
        !acceptance.sent.lock().unwrap().is_empty()
    });
    let mouse_id = {
        let posts = acceptance.sent.lock().unwrap();
        assert_eq!(posts[0].author, PEER);
        posts[0].id
    };
    wait_topic_confirmation(&runtime, ui, &mouse_address, mouse_id);

    ui.simulate_resize(size(px(700.), px(600.)));
    ui.update(|window, app| window.focus(&client.read(app).initial_focus));
    ui.simulate_keystrokes("tab tab tab");
    press_enter(ui);
    assert!(ui.read(|app| client.read(app).top_bar.read(app).menu_is_open()));
    let target_first = ui.read(|app| {
        client.read(app).shell.accounts()[0]
            .session
            .owner
            .account_id
            == a_id
    });
    ui.simulate_keystrokes(if target_first { "up" } else { "down" });
    press_enter(ui);
    until(ui, "клавиатура переключила A", |ui| {
        ui.read(|app| {
            client
                .read(app)
                .shell
                .selected()
                .is_some_and(|s| s.owner.account_id == a_id)
        }) && ui.debug_bounds(CHANNEL_ROW).is_some()
    });
    tab_to_catalog_search(&client, ui);
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string("Длинная тема".into())));
    ui.simulate_keystrokes("cmd-v");
    ui.run_until_parked();
    assert!(ui.debug_bounds("catalog-clear").is_some());
    assert!(ui.debug_bounds(CHANNEL_TOPIC_ROW).is_some());
    ui.simulate_keystrokes("down");
    ui.run_until_parked();
    press_enter(ui);
    until(
        ui,
        "Enter открыл маршрут явной темы A",
        |ui| {
            ui.read(|app| {
                client
                    .read(app)
                    .shell
                    .conversation()
                    .is_some_and(|address| {
                        address.route.owner.account_id == a_id
                            && address.route.scope.topic_id == Some(CHANNEL_TOPIC.parse().unwrap())
                    })
            })
        },
    );
    let key_address = topic_ready(&client, ui, "клавиатура открыла историю темы и редактор A");
    assert_eq!(key_address.route.owner.account_id, a_id);
    assert_eq!(
        ui.debug_bounds("workspace-conversation-panel")
            .unwrap()
            .right(),
        px(700.)
    );
    ui.simulate_keystrokes("tab");
    let conversation = ui.read(|app| client.read(app).conversation.as_ref().unwrap().clone());
    assert_eq!(
        ui.update(|window, app| conversation.read(app).tested_details_focus(window, app)),
        Some("сведения")
    );
    ui.simulate_keystrokes("tab");
    assert_eq!(
        ui.update(|window, app| conversation.read(app).keyboard_focus(window, app)),
        Some("история")
    );
    ui.simulate_keystrokes("tab");
    assert_eq!(
        ui.update(|window, app| conversation.read(app).keyboard_focus(window, app)),
        Some("редактор")
    );
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string(MARKER.to_owned())));
    ui.simulate_keystrokes("cmd-v");
    press_enter(ui);
    until(ui, "единственный POST от A", |_| {
        acceptance.sent.lock().unwrap().len() >= 2
    });
    let key_id = {
        let posts = acceptance.sent.lock().unwrap();
        assert_eq!(posts[1].author, USER);
        posts[1].id
    };
    assert_ne!(mouse_id, key_id);
    wait_topic_confirmation(&runtime, ui, &key_address, key_id);
    assert_eq!(acceptance.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Закрывает тему после открытия редактора и проверяет также отказ для сохранённых записей.
#[gpui::test]
fn explicit_topic_is_rechecked_before_queue_and_before_post(cx: &mut TestAppContext) {
    use workspace_core::storage::{DeliveryStatus, SendBlock};

    cx.update(bind_keys);
    let server = Server::start_with_topic();
    let acceptance = server.state.topic.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(700.), px(600.)));
    until(ui, "WSS и каталог", |ui| {
        server.state.ready.load(Ordering::SeqCst) > 0 && ui.debug_bounds(CHANNEL_ROW).is_some()
    });
    click(ui, CHANNEL_ROW);
    ui.run_until_parked();
    click(ui, CHANNEL_TOPIC_ROW);
    let address = topic_ready(&client, ui, "до закрытия видны история темы и редактор");
    *acceptance.gate.lock().unwrap() = TopicGate::Closed;
    click(ui, "workspace-composer-editor");
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string(MARKER.to_owned())));
    ui.simulate_keystrokes("cmd-v");
    press_enter(ui);
    until(
        ui,
        "пояснение отказа перед записью",
        |ui| {
            ui.read(|app| {
                let screen = client.read(app).conversation.as_ref().unwrap().read(app);
                let composer = screen.tested_composer().unwrap().read(app);
                composer.tested_send_state(app).1 && composer.draft(app) == MARKER
            }) && ui.debug_bounds("conversation-send-explanation").is_some()
        },
    );
    let route = address.clone();
    let entries = futures::executor::block_on(runtime.request(move |services| async move {
        services
            .sync
            .outbox(&route.route.owner, route.generation, route.route.scope)
    }))
    .unwrap()
    .unwrap();
    assert!(entries.is_empty());
    assert_eq!(acceptance.attempts.load(Ordering::SeqCst), 0);

    // Имитируем запись, созданную, когда контекст ещё разрешал отправку.
    for (gate, expected) in [
        (
            TopicGate::Closed,
            DeliveryStatus::Blocked(SendBlock::AccessLost),
        ),
        (
            TopicGate::Encrypted,
            DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable),
        ),
        (TopicGate::WrongStream, DeliveryStatus::Rejected(None)),
        (TopicGate::Unknown, DeliveryStatus::Rejected(None)),
    ] {
        *acceptance.gate.lock().unwrap() = gate;
        let route = address.clone();
        let id = futures::executor::block_on(runtime.request(move |services| async move {
            services
                .sync
                .prepare_send(
                    &route.route.owner,
                    route.generation,
                    route.route.scope.stream_id,
                    route.route.scope.topic_id.unwrap(),
                    MARKER.to_owned(),
                )
                .map(|record| record.public_id)
        }))
        .unwrap()
        .unwrap();
        until(
            ui,
            "очередь подтверждает отказ по этому UUID",
            |_| {
                let route = address.clone();
                futures::executor::block_on(runtime.request(move |services| async move {
                    let entries = services.sync.outbox(
                        &route.route.owner,
                        route.generation,
                        route.route.scope,
                    )?;
                    Ok::<_, workspace_core::sync::SyncError>(
                        entries.iter().any(|entry| {
                            entry.snapshot.public_id == id && entry.status == expected
                        }),
                    )
                }))
                .is_ok_and(|result| result.unwrap_or(false))
            },
        );
        assert_eq!(acceptance.attempts.load(Ordering::SeqCst), 0);
    }
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
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
