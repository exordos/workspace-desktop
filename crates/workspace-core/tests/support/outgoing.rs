//! Проверяет отправку через настоящий Runtime и управляемые REST и WebSocket.
//! Сервер держит ответы POST, меняет доступ и воспроизводит неизвестный исход.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use reqwest::Url;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::tungstenite::Message as Frame;
use uuid::Uuid;
use workspace_core::{
    auth::SessionSnapshot,
    context::OwnerContext,
    messenger::{MessengerSource, send::DirectSendError},
    runtime::{Runtime, SourceError},
    storage::{
        Cursor, DeliveryStatus, HistoryChange, HistoryScope, MessagePlacement, PreparedSend,
        SendBlock, Storage,
    },
    sync::{Event, HistoryRevocation, Sync, SyncChange},
};

use super::{CA, CHAT, Fixture, HistoryApi, HttpsSocket, OTHER_TOPIC, TOPIC, serve, upgrade};

const A: Uuid = Uuid::from_u128(100);
const B: Uuid = Uuid::from_u128(200);
const NEXT_CHAT: Uuid = Uuid::from_u128(300);
const SCOPE: HistoryScope = HistoryScope {
    stream_id: CHAT,
    topic_id: None,
};

/// Варианты ответа о личном разговоре для проверки безопасного запрета.
#[derive(Clone, Copy, Default)]
enum Access {
    #[default]
    Plain, // Сервер явно разрешает открытый текст.
    Encrypted,      // Шифрование включено.
    NoEncryption,   // Сервер не прислал сведения о шифровании.
    NoTopic,        // Сервер не прислал тему по умолчанию.
    MalformedTopic, // Вместо UUID сервер прислал другое значение.
    ForeignProject, // Ответ содержит чужой проект.
    Denied,         // Личка больше не доступна.
}

/// POST, который дошёл до сервера. Тест выбирает, каким будет результат.
struct Post {
    bearer: String, // Токен аккаунта, чей запрос принял сервер.
    body: Value,    // Снимок запроса, который сверяется с очередью.
    reply: oneshot::Sender<Option<(&'static str, Value)>>, // None закрывает соединение без ответа.
}

/// Управляет ответами мессенджера и имитирует реальные точки готовности и догрузки.
pub(super) struct Api {
    access: Mutex<Access>,      // Состояние доступа, возвращаемое в GET стрима.
    default_topic: Mutex<Uuid>, // Значение, которое может измениться между сохранением и POST.
    temporary_topic_failure: AtomicBool, // Следующий GET темы вернёт временный сбой.
    persistent_topic_failure: AtomicBool, // Каждый GET первой темы вернёт временный сбой.
    ready: watch::Sender<bool>, // Пока false, WebSocket не сообщает о готовности к работе.
    events: broadcast::Sender<Value>, // События, отправляемые только B.
    posts: mpsc::UnboundedSender<Post>, // Все полученные POST доступны тесту.
    lookups: Mutex<HashMap<Uuid, Value>>, // Подтверждения, которые сервер возвращает через GET.
    history: Mutex<Vec<Value>>, // Строки, которые сервер возвращает в актуальной истории B.
}

impl Api {
    /// Обслуживает официальный цикл чтения и записи запросов одного владельца.
    pub(super) async fn reply(
        &self,
        mut socket: HttpsSocket,
        uri: Url,
        method: &str,
        headers: &str,
    ) {
        let header = |expected: &str| {
            headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case(expected).then(|| value.trim())
            })
        };
        let path = uri.path();
        if path.ends_with("/events/ws") {
            let protocol = header("sec-websocket-protocol").unwrap();
            self.events_socket(socket, headers, protocol.ends_with(&B.to_string()))
                .await;
            return;
        }
        let token = header("authorization")
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap();
        assert!([A.to_string(), B.to_string()].iter().any(|id| id == token));
        let (status, body) = if path.ends_with("/epoch/") {
            (
                "200 OK",
                json!({"epoch_generation":"test", "epoch_version":1}),
            )
        } else if path.ends_with("/events/") {
            ("200 OK", json!([]))
        } else if let Some((_, id)) = path.split_once("/folders/") {
            ("200 OK", json!({"uuid": id, "unread_count":0}))
        } else if path.ends_with(&format!("/streams/{CHAT}")) {
            self.stream_response(token)
        } else if path.ends_with(&format!("/streams/{NEXT_CHAT}")) {
            (
                "200 OK",
                json!({
                    "uuid":NEXT_CHAT, "project_id":"project", "user_uuid":token,
                    "private":true, "is_archived":false, "direct_user_uuid":Uuid::from_u128(400),
                    "notification_mode":"all_messages", "encryption":false, "default_topic_uuid":OTHER_TOPIC,
                }),
            )
        } else if path.ends_with(&format!("/stream_topics/{OTHER_TOPIC}")) {
            (
                "200 OK",
                json!({
                    "uuid":OTHER_TOPIC, "stream_uuid":NEXT_CHAT, "project_id":"project", "user_uuid":token,
                    "notification_mode":"default",
                }),
            )
        } else if path.ends_with(&format!("/stream_topics/{TOPIC}")) {
            if self.persistent_topic_failure.load(Relaxed)
                || self.temporary_topic_failure.swap(false, Relaxed)
            {
                ("503 Service Unavailable", Value::Null)
            } else {
                (
                    "200 OK",
                    json!({
                        "uuid":TOPIC, "stream_uuid":CHAT, "project_id":"project", "user_uuid":token,
                        "notification_mode":"default"
                    }),
                )
            }
        } else if path.ends_with("/messages/") && method == "POST" {
            let mut bytes = vec![0; header("content-length").unwrap().parse().unwrap()];
            socket.read_exact(&mut bytes).await.unwrap();
            let (reply, response) = oneshot::channel();
            self.posts
                .send(Post {
                    bearer: token.into(),
                    body: serde_json::from_slice(&bytes).unwrap(),
                    reply,
                })
                .ok()
                .unwrap();
            let Ok(Some(result)) = response.await else {
                return;
            };
            result
        } else if path.ends_with("/messages/") {
            let history = self.history.lock().unwrap();
            let messages = if token == B.to_string() {
                history.as_slice()
            } else {
                &[]
            };
            ("200 OK", json!(messages))
        } else if let Some((_, id)) = path.split_once("/messages/") {
            let id: Uuid = id.parse().unwrap();
            if let Some(message) = self.lookups.lock().unwrap().get(&id) {
                ("200 OK", message.clone())
            } else {
                ("404 Not Found", Value::Null)
            }
        } else {
            panic!("неожиданный запрос {path}");
        };
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        // Выход из аккаунта может закрыть клиент раньше задержанного ответа.
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    }

    /// Возвращает выбранную форму GET стрима или явный отказ.
    fn stream_response(&self, user: &str) -> (&'static str, Value) {
        let mut stream = json!({
            "uuid": CHAT, "user_uuid": user, "project_id": "project",
            "private": true, "is_archived": false, "direct_user_uuid": A,
            "notification_mode": "all_messages", "encryption": false,
            "default_topic_uuid": *self.default_topic.lock().unwrap(),
        });
        match *self.access.lock().unwrap() {
            Access::Plain => {}
            Access::Encrypted => stream["encryption"] = json!(true),
            Access::NoEncryption => stream["encryption"] = Value::Null,
            Access::NoTopic => stream["default_topic_uuid"] = Value::Null,
            Access::MalformedTopic => stream["default_topic_uuid"] = json!(42),
            Access::ForeignProject => stream["project_id"] = json!("foreign"),
            Access::Denied => return ("404 Not Found", Value::Null),
        }
        ("200 OK", stream)
    }

    /// Возвращает кадр готовности и позволяет тесту отправлять события на сокет B.
    async fn events_socket(&self, socket: HttpsSocket, headers: &str, recipient: bool) {
        let mut ready = self.ready.subscribe();
        let mut events = self.events.subscribe();
        let mut socket = upgrade(socket, headers).await;
        ready.wait_for(|ready| *ready).await.unwrap();
        if socket
            .send(Frame::text(
                json!({
                    "type": "ready", "epoch_generation": "test", "epoch_version": 1,
                })
                .to_string(),
            ))
            .await
            .is_err()
        {
            return;
        }
        loop {
            tokio::select! {
                message = socket.next() => {
                    if !matches!(message, Some(Ok(Frame::Ping(_) | Frame::Pong(_)))) { return; }
                    if socket.flush().await.is_err() { return; }
                }
                event = events.recv(), if recipient => {
                    let Ok(event) = event else { return; };
                    if socket.send(Frame::text(event.to_string())).await.is_err() { return; }
                }
            }
        }
    }

    /// Подтверждает сообщение по настоящему WS, включая повтор того же UUID.
    fn event(&self, version: u64, message: Value) {
        let mut payload = message;
        payload["kind"] = json!("message.created");
        self.events
            .send(json!({
                "schema_version": 1, "uuid": format!("event-{version}"),
                "epoch_generation": "test", "epoch_version": version,
                "project_id": "project", "user_uuid": B, "object_type": "message",
                "action": "created", "payload": payload,
            }))
            .unwrap();
    }
}

/// Тестовый стенд с двумя входами и SQLite на диске для перезапуска.
struct Test {
    api: Arc<Api>,                           // Управление ответами локального Workspace.
    requests: mpsc::UnboundedReceiver<Post>, // Наблюдение только за POST.
    fixture: Fixture,                        // Аккаунты A и B в общей авторизации.
    a: OwnerContext,                         // Второй активный аккаунт для проверки изоляции.
    b: OwnerContext,                         // Владелец откладываемых сообщений.
    directory: tempfile::TempDir,            // Временный каталог базы без постоянного профиля.
    source: Arc<MessengerSource>,            // Настоящий клиент с доверенным тестовым CA.
    server: JoinHandle<()>,                  // Сетевые обработчики завершаются после теста.
}

impl Test {
    /// Запускает HTTPS, создаёт два аккаунта и оставляет WebSocket в ожидании ready.
    async fn new() -> Self {
        let (posts, requests) = mpsc::unbounded_channel();
        let api = Arc::new(Api {
            access: Mutex::new(Access::Plain),
            default_topic: Mutex::new(TOPIC),
            temporary_topic_failure: AtomicBool::new(false),
            persistent_topic_failure: AtomicBool::new(false),
            ready: watch::channel(false).0,
            events: broadcast::channel(8).0,
            posts,
            lookups: Mutex::new(HashMap::new()),
            history: Mutex::new(Vec::new()),
        });
        let (address, server) = serve(Arc::new(HistoryApi {
            outgoing: Some(api.clone()),
            ..HistoryApi::default()
        }))
        .await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let a = fixture.login_at(&address, &A.to_string());
        let b = fixture.login_at(&address, &B.to_string());
        Self {
            api,
            requests,
            fixture,
            a,
            b,
            server,
            directory: tempfile::tempdir().unwrap(),
            source: Arc::new(
                MessengerSource::with_private_ca(
                    CertificateDer::from_pem_slice(CA).unwrap().as_ref(),
                )
                .unwrap(),
            ),
        }
    }

    /// Возвращает поколение сессии B.
    fn session(&self) -> SessionSnapshot {
        self.fixture
            .auth
            .snapshot()
            .into_iter()
            .find(|session| session.context == self.b)
            .unwrap()
    }

    /// Открывает ту же базу после перезапуска работы.
    fn open(&self) -> (Arc<Storage>, Arc<Sync>) {
        let storage =
            Arc::new(Storage::open(self.directory.path().join("outbox.sqlite3")).unwrap());
        let sync = Arc::new(Sync::new(self.fixture.auth.clone(), storage.clone()));
        (storage, sync)
    }

    /// Запускает очередь штатным Bootstrap рядом с живым потоком сообщений.
    fn run(&self, sync: Arc<Sync>) -> (oneshot::Sender<()>, JoinHandle<()>) {
        let runtime = Runtime::new(sync, vec![self.source.clone()]).unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            workspace_core::bootstrap::run(
                runtime,
                None,
                async {
                    let _ = stopped.await;
                },
                |_| Ok::<_, workspace_core::runtime::RuntimeError>(()),
            )
            .await
            .unwrap();
        });
        (stop, task)
    }

    /// Ставит сообщение в очередь B с выбранной исходной темой.
    fn prepare(&self, sync: &Sync, body: &str) -> PreparedSend {
        sync.prepare_send(&self.b, self.session().generation, CHAT, TOPIC, body.into())
            .unwrap()
    }

    /// Ждёт ровно первый POST снимка и сверяет исходный адрес и токен B.
    async fn post(&mut self, snapshot: &PreparedSend) -> Post {
        let request = timeout(Duration::from_secs(6), self.requests.recv())
            .await
            .expect("первый POST не пришёл")
            .unwrap();
        assert_eq!(request.bearer, B.to_string());
        assert_eq!(
            request.body,
            json!({
                "uuid": snapshot.canonical_id,
                "stream_uuid": snapshot.stream_id,
                "topic_uuid": snapshot.topic_id,
                "payload":{"kind":"markdown","content":snapshot.markdown}
            })
        );
        request
    }

    /// Ждёт конкретный статус снимка либо закрытие его строки.
    async fn wait_status(
        &self,
        sync: &Sync,
        snapshot: &PreparedSend,
        status: Option<DeliveryStatus>,
    ) {
        timeout(Duration::from_secs(5), async {
            loop {
                let found = sync
                    .outbox(
                        &self.b,
                        self.session().generation,
                        HistoryScope {
                            stream_id: snapshot.stream_id,
                            topic_id: Some(snapshot.topic_id),
                        },
                    )
                    .unwrap()
                    .into_iter()
                    .find(|entry| entry.snapshot.public_id == snapshot.public_id);
                if found.map(|entry| entry.status) == status {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("очередь не перешла в {status:?}"));
    }

    /// Убеждается, что за контрольный интервал не было POST.
    async fn no_post(&mut self, duration: Duration) {
        assert!(
            timeout(duration, self.requests.recv()).await.is_err(),
            "неожиданный POST"
        );
    }
}

impl Drop for Test {
    /// Закрывает тестовый сервер и принятые соединения.
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Строит нормальное подтверждение отправки от B.
fn delivered(snapshot: &PreparedSend) -> Value {
    json!({
        "uuid": snapshot.public_id, "stream_uuid": snapshot.stream_id, "topic_uuid": snapshot.topic_id,
        "project_id":"project", "user_uuid": B, "author_uuid": B,
        "created_at":"2026-01-01T12:00:00Z",
        "payload":{"kind":"markdown","content":snapshot.markdown}
    })
}

/// Проверяет явный адрес для пустого чата и безопасный отказ независимо от чтения.
#[tokio::test]
async fn direct_send_requires_explicit_unencrypted_destination() {
    let test = Test::new().await;
    let (_, sync) = test.open();
    let generation = test.session().generation;
    let resolve = || {
        test.source
            .resolve_direct_send(&sync, &test.b, generation, CHAT)
    };
    *test.api.default_topic.lock().unwrap() = OTHER_TOPIC;
    assert_eq!(resolve().await.unwrap(), OTHER_TOPIC);
    for (mode, expected) in [
        (Access::Encrypted, DirectSendError::EncryptionUnavailable),
        (Access::NoEncryption, DirectSendError::EncryptionUnavailable),
        (Access::NoTopic, DirectSendError::TopicUnavailable),
        (Access::MalformedTopic, DirectSendError::TopicUnavailable),
        (
            Access::ForeignProject,
            DirectSendError::Source(SourceError::InvalidResponse),
        ),
        (Access::Denied, DirectSendError::AccessLost),
    ] {
        *test.api.access.lock().unwrap() = mode;
        assert_eq!(
            resolve().await.unwrap_err().to_string(),
            expected.to_string()
        );
    }
    *test.api.access.lock().unwrap() = Access::MalformedTopic;
    let opening = test
        .source
        .open_latest_history(&sync, &test.b, generation, SCOPE)
        .await
        .unwrap();
    assert!(opening.refresh().await.unwrap().messages.is_empty());
    test.fixture.auth.logout(&test.b.account_id).unwrap();
    assert!(matches!(resolve().await, Err(DirectSendError::Auth(_))));
}

/// Проверяет запуск сохранённой очереди и единственную строку при обоих порядках подтверждения.
#[tokio::test]
async fn background_send_preserves_account_topic_and_confirmation_order() {
    let mut test = Test::new().await;
    let (_, before_restart) = test.open();
    let first = test.prepare(&before_restart, "HTTP раньше WS");
    drop(before_restart);
    // Не переадресуем сохранённое сообщение при изменении темы по умолчанию.
    *test.api.default_topic.lock().unwrap() = OTHER_TOPIC;
    test.api.temporary_topic_failure.store(true, Relaxed);
    let (storage, sync) = test.open();
    let generation = test.session().generation;
    let empty = test
        .source
        .open_latest_history(&sync, &test.b, generation, SCOPE)
        .await
        .unwrap()
        .refresh()
        .await
        .unwrap();
    let (stop, runtime) = test.run(sync.clone());
    test.no_post(Duration::from_millis(200)).await;
    test.api.ready.send_replace(true);

    let first_request = test.post(&first).await;
    test.wait_status(&sync, &first, Some(DeliveryStatus::Sending))
        .await;
    test.no_post(Duration::from_millis(200)).await;
    test.api.history.lock().unwrap().push(delivered(&first));
    first_request
        .reply
        .send(Some(("201 Created", delivered(&first))))
        .unwrap();
    test.wait_status(&sync, &first, None).await;
    test.api.event(2, delivered(&first));

    let second = test.prepare(&sync, "WS раньше HTTP");
    let second_request = test.post(&second).await;
    test.api.history.lock().unwrap().push(delivered(&second));
    test.api.event(3, delivered(&second));
    test.api.event(4, delivered(&second));
    test.wait_status(&sync, &second, None).await;
    second_request
        .reply
        .send(Some(("201 Created", delivered(&second))))
        .unwrap();
    let previous = sync
        .refresh_history_window(&test.b, generation, SCOPE, empty.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        previous.newer,
        workspace_core::storage::HistoryEdge::Unchecked
    );
    let history = test
        .source
        .load_latest_history(&sync, &test.b, generation, SCOPE)
        .await
        .unwrap();
    assert_eq!(
        history
            .messages
            .iter()
            .map(|message| message.id)
            .collect::<std::collections::HashSet<_>>(),
        [first.public_id, second.public_id].into_iter().collect()
    );
    assert_eq!(history.messages.len(), 2);
    assert_eq!(
        storage
            .message(&test.a, &second.public_id.to_string())
            .unwrap(),
        None
    );
    test.no_post(Duration::from_millis(300)).await;
    stop.send(()).unwrap();
    runtime.await.unwrap();
}

/// Ожидающая первая тема под 503 не мешает отправить следующее сообщение в другой чат.
#[tokio::test]
async fn unavailable_topic_does_not_delay_another_chat() {
    let mut test = Test::new().await;
    let (_, sync) = test.open();
    test.api.persistent_topic_failure.store(true, Relaxed);
    let waiting = test.prepare(&sync, "Тема отвечает 503");
    let ready = sync
        .prepare_send(
            &test.b,
            test.session().generation,
            NEXT_CHAT,
            OTHER_TOPIC,
            "Следующий чат принимает сообщение".into(),
        )
        .unwrap();
    let (stop, runtime) = test.run(sync.clone());
    test.api.ready.send_replace(true);

    let post = test.post(&ready).await;
    post.reply
        .send(Some(("201 Created", delivered(&ready))))
        .unwrap();
    test.wait_status(&sync, &ready, None).await;
    test.wait_status(&sync, &waiting, Some(DeliveryStatus::Waiting))
        .await;
    test.no_post(Duration::from_millis(1200)).await;
    test.wait_status(&sync, &waiting, Some(DeliveryStatus::Waiting))
        .await;
    stop.send(()).unwrap();
    runtime.await.unwrap();
}

/// Проверяет отмену позднего HTTP B после выхода при продолжающейся работе A.
#[tokio::test]
async fn logout_drops_late_confirmation_for_old_generation() {
    let mut test = Test::new().await;
    let (storage, sync) = test.open();
    let (stop, runtime) = test.run(sync.clone());
    test.api.ready.send_replace(true);
    let old_generation = test.session().generation;
    let snapshot = test.prepare(&sync, "Поздний ответ после выхода");
    let received = test.post(&snapshot).await;
    test.fixture.auth.logout(&test.b.account_id).unwrap();
    received
        .reply
        .send(Some(("201 Created", delivered(&snapshot))))
        .unwrap();
    assert_eq!(
        test.fixture.login_at(&test.b.server, &B.to_string()),
        test.b
    );
    assert_ne!(old_generation, test.session().generation);
    test.wait_status(&sync, &snapshot, Some(DeliveryStatus::Unknown))
        .await;
    assert_eq!(
        storage
            .message(&test.b, &snapshot.public_id.to_string())
            .unwrap(),
        None
    );
    assert!(
        test.fixture
            .auth
            .snapshot()
            .iter()
            .any(|session| session.context == test.a
                && session.status == workspace_core::auth::SessionStatus::Active)
    );
    test.no_post(Duration::from_millis(650)).await;
    stop.send(()).unwrap();
    runtime.await.unwrap();
}

/// Проверяет неизвестный исход через GET, после перезапуска и через событие.
#[tokio::test]
async fn unknown_delivery_reconciles_without_repeating_post() {
    let mut test = Test::new().await;
    let (_, sync) = test.open();
    test.api.ready.send_replace(true);
    let (stop, runtime) = test.run(sync.clone());
    let lookup = test.prepare(&sync, "Ответ потерялся, GET находит сообщение");
    let request = test.post(&lookup).await;
    test.api
        .lookups
        .lock()
        .unwrap()
        .insert(lookup.public_id, delivered(&lookup));
    request.reply.send(None).unwrap();
    test.wait_status(&sync, &lookup, Some(DeliveryStatus::Unknown))
        .await;
    test.wait_status(&sync, &lookup, None).await;

    let pushed = test.prepare(&sync, "Ответ потерялся, приходит событие");
    let request = test.post(&pushed).await;
    request.reply.send(None).unwrap();
    test.wait_status(&sync, &pushed, Some(DeliveryStatus::Unknown))
        .await;
    test.api.event(2, delivered(&pushed));
    test.wait_status(&sync, &pushed, None).await;

    let restart = test.prepare(&sync, "Перезапуск после захвата");
    let request = test.post(&restart).await;
    stop.send(()).unwrap();
    runtime.await.unwrap();
    drop(request);
    drop(sync);
    let (storage, sync) = test.open();
    let (stop, runtime) = test.run(sync.clone());
    test.wait_status(&sync, &restart, Some(DeliveryStatus::Unknown))
        .await;
    // В этот интервал GET ничего не видит. Отсутствие результата не разрешает POST.
    test.no_post(Duration::from_millis(750)).await;
    test.wait_status(&sync, &restart, Some(DeliveryStatus::Unknown))
        .await;
    test.api.event(3, delivered(&restart));
    test.wait_status(&sync, &restart, None).await;
    assert!(
        storage
            .message(&test.b, &restart.public_id.to_string())
            .unwrap()
            .is_some()
    );
    test.no_post(Duration::from_millis(400)).await;
    stop.send(()).unwrap();
    runtime.await.unwrap();
}

/// Проверяет отказ до захвата, окончательный HTTP-отказ и чужой автор в HTTP 201.
#[tokio::test]
async fn background_send_stops_on_access_loss_and_rejects_wrong_confirmation() {
    let mut test = Test::new().await;
    let (_, sync) = test.open();
    let (stop, runtime) = test.run(sync.clone());
    *test.api.access.lock().unwrap() = Access::Encrypted;
    let encrypted = test.prepare(&sync, "Шифрование включено после постановки");
    test.api.ready.send_replace(true);
    test.wait_status(
        &sync,
        &encrypted,
        Some(DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable)),
    )
    .await;
    *test.api.access.lock().unwrap() = Access::Denied;
    let revoked = test.prepare(&sync, "Отозванный адрес");
    test.wait_status(
        &sync,
        &revoked,
        Some(DeliveryStatus::Blocked(SendBlock::AccessLost)),
    )
    .await;
    test.no_post(Duration::from_millis(200)).await;

    *test.api.access.lock().unwrap() = Access::Plain;
    let rejected = test.prepare(&sync, "Однозначный отказ");
    test.post(&rejected)
        .await
        .reply
        .send(Some(("400 Bad Request", Value::Null)))
        .unwrap();
    test.wait_status(&sync, &rejected, Some(DeliveryStatus::Rejected(Some(400))))
        .await;

    let foreign = test.prepare(&sync, "Ответ чужого автора");
    let mut response = delivered(&foreign);
    response["author_uuid"] = json!(A);
    test.post(&foreign)
        .await
        .reply
        .send(Some(("201 Created", response)))
        .unwrap();
    test.wait_status(&sync, &foreign, Some(DeliveryStatus::Unknown))
        .await;
    test.no_post(Duration::from_millis(750)).await;
    stop.send(()).unwrap();
    runtime.await.unwrap();

    let (_, sync) = test.open();
    let (stop, runtime) = test.run(sync.clone());
    test.wait_status(&sync, &rejected, Some(DeliveryStatus::Rejected(Some(400))))
        .await;
    test.no_post(Duration::from_millis(350)).await;
    stop.send(()).unwrap();
    runtime.await.unwrap();
}

/// Принимает отзыв темы вместе с курсором и блокирует только совпавшую очередь.
#[test]
fn topic_revocation_prevents_first_claim_for_matching_scope() {
    let fixture = Fixture::new(|_, _| unreachable!());
    let a = fixture.login(&A.to_string());
    let b = fixture.login(&B.to_string());
    let generation = |owner: &OwnerContext| {
        fixture
            .auth
            .snapshot()
            .into_iter()
            .find(|session| session.context == *owner)
            .unwrap()
            .generation
    };
    let storage = Arc::new(Storage::in_memory().unwrap());
    let sync = Sync::new(fixture.auth.clone(), storage.clone());
    let queue = |owner: &OwnerContext, topic| {
        sync.prepare_send(owner, generation(owner), CHAT, topic, "Текст".into())
            .unwrap()
    };
    let a_pending = queue(&a, TOPIC);
    let blocked = queue(&b, TOPIC);
    let b_other = queue(&b, OTHER_TOPIC);
    let mut changes = sync.subscribe();
    let event = Event::HistoryChanged {
        cursor: Cursor {
            epoch_generation: "test".into(),
            epoch_version: 2,
        },
        change: HistoryChange::RemoveTopic(MessagePlacement {
            stream_id: CHAT.to_string(),
            topic_id: TOPIC.to_string(),
        }),
    };
    sync.apply(&b, generation(&b), "messenger", event.clone())
        .unwrap();
    assert_eq!(
        sync.cursor(&b, "messenger").unwrap().as_ref(),
        Some(event.cursor())
    );
    let update = changes.try_recv().unwrap();
    assert_eq!(update.context, b);
    assert_eq!(
        update.change,
        SyncChange::HistoryAccessRevoked(HistoryRevocation::Topic {
            stream_id: CHAT,
            topic_id: TOPIC,
        })
    );
    let visible = sync.outbox(&b, generation(&b), SCOPE).unwrap();
    assert_eq!(
        visible[0].status,
        DeliveryStatus::Blocked(SendBlock::AccessLost)
    );
    assert_eq!(storage.pending_sends(&a).unwrap(), vec![a_pending]);
    assert_eq!(storage.pending_sends(&b).unwrap(), vec![b_other]);
    assert_eq!(
        storage.claim_first_send(&b, blocked.canonical_id).unwrap(),
        None
    );

    sync.apply(&b, generation(&b), "messenger", event).unwrap();
    assert!(
        changes.try_recv().is_err(),
        "повтор позиции не публикует отзыв второй раз"
    );
}
