//! Проверяет отправку через настоящий Runtime и управляемые REST и WebSocket.
//! Сервер держит ответы POST, меняет доступ и воспроизводит неизвестный исход.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
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
    messenger::{
        MessengerSource,
        encrypted_send::EncryptedMessageError,
        encryption::{
            ChatKeyAddress, ChatKeyStore, EncryptionActivation, EncryptionError, EncryptionSession,
            GroupKey, HeldChatKey, KeyStoreError, MessageEncryption,
        },
        message_actions::MessageTarget,
        quote::{Source as QuoteSource, SourceCheckError},
        send::DirectSendError,
    },
    runtime::{Runtime, SourceError},
    storage::{
        Cursor, DeliveryStatus, HistoryChange, HistoryEdge, HistoryMessage, HistoryMetadata,
        HistoryPageEdges, HistoryPageOutcome, HistoryPageTarget, HistoryScope, HistoryTime,
        Message, MessageBody, MessagePlacement, PreparedSend, PreparedSendBody, SendBlock, Storage,
    },
    sync::{Event, HistoryRevocation, Sync, SyncChange},
};

use super::{CA, CHAT, Fixture, HistoryApi, HttpsSocket, OTHER_TOPIC, TOPIC, serve, upgrade};

const A: Uuid = Uuid::from_u128(100);
const B: Uuid = Uuid::from_u128(200);
const NEXT_CHAT: Uuid = Uuid::from_u128(300);
const CURRENT_KEY: Uuid = Uuid::from_u128(71);
const PREVIOUS_KEY: Uuid = Uuid::from_u128(72);
const TEST_DEVICE: Uuid = Uuid::from_u128(73);
const SCOPE: HistoryScope = HistoryScope {
    stream_id: CHAT,
    topic_id: None,
};

/// Варианты ответа о личном разговоре для проверки безопасного запрета.
#[derive(Clone, Copy, Default)]
enum Access {
    #[default]
    Plain, // Сервер явно разрешает открытый текст.
    NativeMarkdown,   // Обычный маршрут с подтверждённым нативным источником.
    ExternalMarkdown, // Буквальные маркеры остаются Markdown внешнего провайдера.
    Encrypted,        // Шифрование включено.
    NoCurrentKey,     // Шифрование включено, но сервер пропустил UUID поколения.
    NullCurrentKey,   // Шифрование включено, а UUID поколения явно пуст.
    NoEncryption,     // Сервер не прислал сведения о шифровании.
    NoTopic,          // Сервер не прислал тему по умолчанию.
    MalformedTopic,   // Вместо UUID сервер прислал другое значение.
    ForeignProject,   // Ответ содержит чужой проект.
    Denied,           // Личка больше не доступна.
}

/// POST, который дошёл до сервера. Тест выбирает, каким будет результат.
struct Post {
    bearer: String, // Токен аккаунта, чей запрос принял сервер.
    body: Value,    // Снимок запроса, который сверяется с очередью.
    reply: oneshot::Sender<Option<(&'static str, Value)>>, // None закрывает соединение без ответа.
}

/// Удерживает ответ точного GET после чтения неизменного снимка.
struct LookupGate {
    captured: oneshot::Sender<()>, // Сообщает тесту, что ответ готов и ждёт.
    release: oneshot::Receiver<()>, // Разрешает вернуть старый снимок.
}

/// Управляет ответами мессенджера и имитирует реальные точки готовности и догрузки.
pub(super) struct Api {
    access: Mutex<Access>,      // Состояние доступа, возвращаемое в GET стрима.
    default_topic: Mutex<Uuid>, // Значение, которое может измениться между сохранением и POST.
    temporary_topic_failure: AtomicBool, // Следующий GET темы вернёт временный сбой.
    persistent_topic_failure: AtomicBool, // Каждый GET первой темы вернёт временный сбой.
    ready: watch::Sender<bool>, // Пока false, WebSocket не сообщает о готовности к работе.
    events: broadcast::Sender<Value>, // События, отправляемые только B.
    page_events: Mutex<Vec<Value>>, // Сохранённые события B для REST-догрузки после обрыва.
    posts: mpsc::UnboundedSender<Post>, // Все полученные POST доступны тесту.
    lookups: Mutex<HashMap<Uuid, Value>>, // Подтверждения, которые сервер возвращает через GET.
    lookup_count: AtomicUsize,  // Число GET адресных сообщений.
    lookup_failure: AtomicBool, // Возвращает временный сбой в GET сообщения.
    lookup_gate: Mutex<Option<LookupGate>>, // Пауза после получения одного точного снимка.
    history: Mutex<Vec<Value>>, // Строки, которые сервер возвращает в актуальной истории B.
    meet_url: Mutex<Option<String>>, // Публичная настройка Jitsi локального сервера.
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
        let public_settings = path.ends_with("/server_settings");
        let token = if public_settings {
            assert!(header("authorization").is_none());
            ""
        } else {
            let token = header("authorization")
                .unwrap()
                .strip_prefix("Bearer ")
                .unwrap();
            assert!([A.to_string(), B.to_string()].iter().any(|id| id == token));
            token
        };
        let (status, body) = if public_settings {
            assert_eq!(method, "GET");
            (
                "200 OK",
                json!({"meet_url": *self.meet_url.lock().unwrap()}),
            )
        } else if path.ends_with("/epoch/") {
            (
                "200 OK",
                json!({"epoch_generation":"test", "epoch_version":1}),
            )
        } else if path.ends_with("/events/") {
            let after = uri
                .query_pairs()
                .find(|(name, _)| name == "epoch_version>")
                .and_then(|(_, value)| value.parse::<u64>().ok())
                .unwrap_or_default();
            let events = self.page_events.lock().unwrap();
            let rows: Vec<_> = events
                .iter()
                .filter(|event| {
                    token == B.to_string() && event["epoch_version"].as_u64().unwrap() > after
                })
                .collect();
            ("200 OK", json!(rows))
        } else if path.ends_with("/folders/")
            || path.ends_with("/streams/")
            || path.ends_with("/stream_topics/")
        {
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
                    "notification_mode":"all_messages", "encryption":false, "default_topic_uuid":OTHER_TOPIC, "source_name":"native",
                }),
            )
        } else if path.ends_with(&format!("/stream_topics/{OTHER_TOPIC}")) {
            let stream = if matches!(*self.access.lock().unwrap(), Access::Encrypted) {
                CHAT
            } else {
                NEXT_CHAT
            };
            (
                "200 OK",
                json!({
                    "uuid":OTHER_TOPIC, "stream_uuid":stream, "project_id":"project", "user_uuid":token,
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
        } else if (path.ends_with("/messages/") && method == "POST")
            || (method == "PUT"
                && path
                    .split_once("/messages/")
                    .is_some_and(|(_, id)| id.parse::<Uuid>().is_ok()))
        {
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
            self.lookup_count.fetch_add(1, Relaxed);
            if self.lookup_failure.load(Relaxed) {
                ("503 Service Unavailable", Value::Null)
            } else {
                let message = self.lookups.lock().unwrap().get(&id).cloned();
                let gate = self.lookup_gate.lock().unwrap().take();
                if let Some(gate) = gate {
                    gate.captured.send(()).unwrap();
                    gate.release.await.unwrap();
                }
                message.map_or(("404 Not Found", Value::Null), |message| {
                    ("200 OK", message)
                })
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
            Access::NativeMarkdown => stream["source_name"] = json!("native"),
            Access::ExternalMarkdown => stream["source_name"] = json!("zulip"),
            encrypted @ (Access::Encrypted | Access::NoCurrentKey | Access::NullCurrentKey) => {
                stream["encryption"] = json!(true);
                stream["source_name"] = json!("native");
                match encrypted {
                    Access::Encrypted => stream["current_encryption_key_uuid"] = json!(CURRENT_KEY),
                    Access::NullCurrentKey => stream["current_encryption_key_uuid"] = Value::Null,
                    _ => {}
                }
            }
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
        self.events.send(message_created(version, message)).unwrap();
    }
}

/// Проверяет буквальные маркеры внешнего провайдера и нативный допуск удаления черновика.
#[tokio::test]
async fn fragment_capability_separates_external_markers_and_native_draft_purge() {
    use workspace_core::storage::{DraftScope, DraftVersion, DraftWriteOutcome, PlaintextDraft};
    let mut test = Test::new().await;
    let (storage, sync) = test.open();
    let generation = test.session().generation;
    let owner = &test.b;
    let scope = DraftScope {
        stream_id: CHAT,
        topic_id: TOPIC,
    };
    let access = storage.prepare_draft_access(owner, scope).unwrap();
    let id = Uuid::new_v4();
    let draft = PlaintextDraft {
        id,
        version: 1,
        markdown: ":::буквальная разметка Zulip".into(),
        replies: None,
    };

    *test.api.access.lock().unwrap() = Access::ExternalMarkdown;
    let external = test
        .source
        .resolve_plain_send(&sync, owner, generation, SCOPE)
        .await
        .unwrap();
    assert!(!external.fragments_allowed() && external.literal_markers_allowed());
    assert_eq!(
        sync.save_external_draft(&external, &access, &draft)
            .unwrap(),
        DraftWriteOutcome::Saved
    );
    assert!(
        sync.protect_draft(&external, &access, DraftVersion { id, version: 2 })
            .is_err()
    );
    assert!(storage.read_plaintext_draft(&access, Some(id)).unwrap() == Some(draft.clone()));
    let pending = sync
        .prepare_external_send(&external, draft.markdown.clone(), None)
        .unwrap();
    let source = test.source.clone();
    let owner = owner.clone();
    let outgoing = pending.clone();
    let sent = tokio::spawn(async move {
        source
            .post_prepared_message(&owner, &B.to_string(), &outgoing)
            .await
    });
    let request = timeout(Duration::from_secs(3), test.requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.body["payload"]["content"], draft.markdown);
    request
        .reply
        .send(Some(("201 Created", delivered(&pending))))
        .unwrap();
    assert!(sent.await.unwrap().is_ok());

    *test.api.access.lock().unwrap() = Access::NativeMarkdown;
    let native = test
        .source
        .resolve_plain_send(&sync, &test.b, generation, SCOPE)
        .await
        .unwrap();
    assert!(native.fragments_allowed() && !native.literal_markers_allowed());
    assert!(
        test.source
            .post_prepared_message(&test.b, &B.to_string(), &pending)
            .await
            .is_err()
    );
    assert!(
        sync.protect_draft(&native, &access, DraftVersion { id, version: 2 })
            .unwrap()
    );
    assert!(
        storage
            .read_plaintext_draft(&access, Some(id))
            .unwrap()
            .is_none()
    );
}

/// Открывает правку обычной строки до появления первого фрагмента и отправляет только конверт.
#[tokio::test]
async fn own_plain_markdown_edit_can_add_first_fragment_and_observes_origin_revocation() {
    use workspace_core::messenger::encryption::fragments::fragment_links;
    let mut test = Test::new().await;
    let (_keys, activation) = test.enable_encryption();
    *test.api.access.lock().unwrap() = Access::NativeMarkdown;
    let (_storage, sync) = test.open();
    let session = test.session();
    let id = Uuid::new_v4();
    let expected = HistoryMessage {
        id,
        stream_id: CHAT,
        topic_id: TOPIC,
        body: MessageBody::Markdown("обычная исходная строка".into()),
        metadata: HistoryMetadata {
            author_id: Some(B),
            created_at: HistoryTime::parse("2026-01-01T12:00:00Z").unwrap(),
            updated_at: None,
        },
    };
    let guard = sync
        .prepare_history_page(
            &session.context,
            session.generation,
            SCOPE,
            HistoryPageTarget::New,
        )
        .unwrap()
        .unwrap();
    sync.save_history_page(
        guard,
        std::slice::from_ref(&expected),
        HistoryPageEdges::New {
            older: HistoryEdge::End,
            newer: HistoryEdge::End,
        },
    )
    .unwrap();
    let original = json!({
        "uuid": id, "stream_uuid": CHAT, "topic_uuid": TOPIC,
        "project_id":"project", "user_uuid":B, "author_uuid":B, "source_name":"native",
        "created_at":"2026-01-01T12:00:00Z", "payload":{"kind":"markdown","content":expected.body.markdown()}
    });
    test.api
        .lookups
        .lock()
        .unwrap()
        .insert(id, original.clone());
    let target = MessageTarget {
        scope: SCOPE,
        topic_id: TOPIC,
        message_id: id,
    };
    let edit = Arc::new(
        test.source
            .open_fragment_edit(&sync, &activation, target, &expected)
            .await
            .unwrap(),
    );
    assert_eq!(
        edit.with_markdown(str::to_owned).as_deref(),
        expected.body.markdown()
    );
    let source = test.source.clone();
    let editing = {
        let sync = sync.clone();
        let edit = edit.clone();
        tokio::spawn(async move {
            source
                .edit_fragment_message(&sync, &edit, "обычная :::приватная первая часть:::".into())
                .await
        })
    };
    let put = timeout(Duration::from_secs(3), test.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let sent = put.body["payload"]["content"].as_str().unwrap();
    assert!(!sent.contains("приватная"));
    let link = &fragment_links(sent)[0];
    assert_eq!(link.reference.stream_id(), CHAT);
    let mut edited = original;
    edited["payload"] = put.body["payload"].clone();
    edited["updated_at"] = json!("2026-01-01T12:01:00Z");
    put.reply.send(Some(("200 OK", edited))).unwrap();
    assert!(editing.await.unwrap().is_ok());

    for (number, stream) in [(1, NEXT_CHAT), (2, CHAT)] {
        sync.apply(
            &session.context,
            session.generation,
            workspace_core::messenger::SOURCE_ID,
            Event::HistoryChanged {
                cursor: Cursor {
                    epoch_generation: "test".into(),
                    epoch_version: number,
                },
                change: HistoryChange::RemoveStream(stream.to_string()),
            },
        )
        .unwrap();
        assert_eq!(edit.with_markdown(|_| ()).is_some(), stream != CHAT);
    }
}

/// Пересылает исходный конверт в другую тему и отзывает чтение по исходному чату.
#[tokio::test]
async fn protected_forward_posts_canonical_snapshot_and_tracks_the_origin() {
    use workspace_core::messenger::{
        SOURCE_ID,
        encryption::fragments::fragment_links,
        forward::{ProtectedForwardOrigin, forward_spans},
    };

    let mut test = Test::new().await;
    let (keys, activation) = test.enable_encryption();
    *test.api.access.lock().unwrap() = Access::NativeMarkdown;
    let (_, sync) = test.open();
    let session = test.session();
    let prepared = test
        .source
        .prepare_fragment_send(
            &sync,
            &activation,
            SCOPE,
            TOPIC,
            "Публично :::приватно из исходного чата:::".into(),
            true,
        )
        .await
        .unwrap();
    let PreparedSendBody::Markdown(source_snapshot) = prepared.body else {
        panic!("исходная строка должна быть смешанным Markdown");
    };
    let source_wire = json!({
        "uuid": prepared.public_id, "stream_uuid": CHAT, "topic_uuid": TOPIC,
        "project_id":"project", "user_uuid":B, "author_uuid":B, "source_name":"native",
        "created_at":"2026-01-01T12:00:00Z", "payload":{"kind":"markdown","content":source_snapshot}
    });
    test.api
        .lookups
        .lock()
        .unwrap()
        .insert(prepared.public_id, source_wire);
    let target = HistoryScope {
        stream_id: NEXT_CHAT,
        topic_id: None,
    };
    let origin = ProtectedForwardOrigin {
        scope: SCOPE,
        message_id: prepared.public_id,
        author: "Автор".into(),
        chat: "Исходный чат".into(),
    };
    let record = test
        .source
        .prepare_protected_forward(&sync, &session.context, session.generation, &origin, target)
        .await
        .unwrap();
    assert_eq!(
        (record.stream_id, record.topic_id),
        (NEXT_CHAT, OTHER_TOPIC)
    );
    let PreparedSendBody::Markdown(forward) = &record.body else {
        panic!("готовая пересылка должна быть обычным Markdown");
    };
    let forwarded = forward_spans(forward).pop().unwrap().reference.unwrap();
    assert_eq!(forwarded.snapshot, source_snapshot);
    assert!(!forwarded.snapshot.contains("приватно из исходного чата"));
    let reference = &fragment_links(&forwarded.snapshot)[0].reference;
    assert_eq!(reference.stream_id(), CHAT);
    let open = || async {
        keys.accept_fragment(
            test.source
                .prepare_linked_fragment(&sync, &activation, reference)
                .await
                .unwrap(),
        )
        .unwrap()
    };
    let first = open().await;
    assert_eq!(
        first.with_text(str::to_owned).as_deref(),
        Some("приватно из исходного чата")
    );
    let source = test.source.clone();
    let owner = session.context.clone();
    let sending = tokio::spawn(async move {
        source
            .post_prepared_message(&owner, &B.to_string(), &record)
            .await
    });
    let post = timeout(Duration::from_secs(3), test.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let forwarded_content = post.body["payload"]["content"].as_str().unwrap();
    assert_eq!(
        forward_spans(forwarded_content)[0]
            .reference
            .as_ref()
            .unwrap()
            .snapshot,
        source_snapshot
    );
    assert_eq!(post.body["stream_uuid"], json!(NEXT_CHAT));
    assert_eq!(post.body["topic_uuid"], json!(OTHER_TOPIC));
    let public_id = Uuid::new_v5(&OTHER_TOPIC, post.body["uuid"].as_str().unwrap().as_bytes());
    let echoed = json!({
        "uuid":public_id, "canonical_message_uuid":post.body["uuid"], "stream_uuid":NEXT_CHAT, "topic_uuid":OTHER_TOPIC,
        "project_id":"project", "user_uuid":B, "author_uuid":B, "source_name":"native",
        "created_at":"2026-01-01T12:01:00Z", "payload":post.body["payload"],
    });
    post.reply.send(Some(("201 Created", echoed))).unwrap();
    assert!(sending.await.unwrap().is_ok());

    for (number, stream) in [(1, NEXT_CHAT), (2, CHAT)] {
        sync.apply(
            &session.context,
            session.generation,
            SOURCE_ID,
            Event::HistoryChanged {
                cursor: Cursor {
                    epoch_generation: "test".into(),
                    epoch_version: number,
                },
                change: HistoryChange::RemoveStream(stream.to_string()),
            },
        )
        .unwrap();
        assert_eq!(first.with_text(|_| ()).is_some(), stream != CHAT);
    }
}

/// Оборачивает подтверждение в событие одного и того же формата для REST и WebSocket.
fn message_created(version: u64, mut payload: Value) -> Value {
    payload["kind"] = json!("message.created");
    json!({
        "schema_version": 1, "uuid": format!("event-{version}"),
        "epoch_generation": "test", "epoch_version": version,
        "project_id": "project", "user_uuid": B, "object_type": "message",
        "action": "created", "payload": payload,
    })
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
            page_events: Mutex::default(),
            posts,
            lookups: Mutex::new(HashMap::new()),
            lookup_count: AtomicUsize::new(0),
            lookup_failure: AtomicBool::new(false),
            lookup_gate: Mutex::default(),
            history: Mutex::new(Vec::new()),
            meet_url: Mutex::new(None),
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

    /// Подключает защищённый поток для B и локальное хранилище тестового устройства.
    fn enable_encryption(&mut self) -> (Arc<MessageEncryption>, EncryptionActivation) {
        *self.api.access.lock().unwrap() = Access::Encrypted;
        let keys = Arc::new(MessageEncryption::new(Arc::new(SendKeys::default())));
        self.source = Arc::new(
            MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
                .unwrap()
                .with_chat_encryption(keys.clone(), TEST_DEVICE),
        );
        let activation = keys
            .select_session(Some(EncryptionSession {
                owner: self.b.clone(),
                generation: self.session().generation,
                device_id: TEST_DEVICE,
            }))
            .unwrap();
        (keys, activation)
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
                "payload":outgoing_payload(snapshot)
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
        "created_at":"2026-01-01T12:00:00Z", "source_name": "native",
        "payload":outgoing_payload(snapshot)
    })
}

/// Формирует DTO по ожидаемому телу очереди для независимой проверки HTTPS.
fn outgoing_payload(snapshot: &PreparedSend) -> Value {
    match &snapshot.body {
        PreparedSendBody::Markdown(text) => json!({"kind": "markdown", "content": text}),
        PreparedSendBody::Encrypted {
            key_id, ciphertext, ..
        } => json!({"kind": "e2ee", "key_uuid": key_id, "content": ciphertext}),
    }
}

/// Хранит синтетические ключи и их порядок отдельно для каждого адреса.
#[derive(Default)]
pub(super) struct SendKeys(Mutex<HashMap<ChatKeyAddress, (GroupKey, HeldChatKey)>>); // Ключи тестового профиля.

impl ChatKeyStore for SendKeys {
    /// Копирует ключ выбранного адреса, сохраняя изоляцию контекстов.
    fn load(&self, address: &ChatKeyAddress) -> Result<Option<GroupKey>, KeyStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(address)
            .map(|(key, _)| GroupKey::from_bytes(*key.as_bytes())))
    }

    /// Сохраняет ключ и фиксирует порядок его первого получения.
    fn save(&self, address: &ChatKeyAddress, key: &GroupKey) -> Result<(), KeyStoreError> {
        let mut keys = self.0.lock().unwrap();
        let order = keys
            .values()
            .map(|(_, metadata)| metadata.received_order)
            .max()
            .unwrap_or(0)
            + 1;
        let entry = keys.entry(address.clone()).or_insert_with(|| {
            (
                GroupKey::from_bytes(*key.as_bytes()),
                HeldChatKey {
                    key_id: address.key_id,
                    received_order: order,
                    send_order: 0,
                    added_at_unix_ms: None,
                },
            )
        });
        entry.0 = GroupKey::from_bytes(*key.as_bytes());
        Ok(())
    }

    /// Убирает ключ и связанный порядок получения.
    fn remove(&self, address: &ChatKeyAddress) -> Result<(), KeyStoreError> {
        self.0.lock().unwrap().remove(address);
        Ok(())
    }

    /// Возвращает порядок имеющихся ключей текущего профиля и чата.
    fn list(
        &self,
        owner: &OwnerContext,
        device_id: Uuid,
        stream_id: Uuid,
    ) -> Result<Vec<HeldChatKey>, KeyStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(address, (_, metadata))| {
                (&address.owner == owner
                    && address.device_id == device_id
                    && address.stream_id == stream_id)
                    .then_some(*metadata)
            })
            .collect())
    }

    /// Повышает приоритет отправки только для доступного ключа.
    fn mark_for_sending(&self, address: &ChatKeyAddress) -> Result<(), KeyStoreError> {
        let mut keys = self.0.lock().unwrap();
        let order = keys
            .values()
            .map(|(_, metadata)| metadata.send_order)
            .max()
            .unwrap_or(0)
            + 1;
        keys.get_mut(address).ok_or(KeyStoreError)?.1.send_order = order;
        Ok(())
    }
}

/// Повтор использует тот же UUID и конверт после перезапуска, отзыва и восстановления ключа.
#[tokio::test]
async fn encrypted_send_preserves_envelope_and_fails_closed_when_key_changes() {
    let mut test = Test::new().await;
    let (keys, activation) = test.enable_encryption();
    let generation = test.session().generation;
    let (storage, sync) = test.open();
    assert!(matches!(
        test.source
            .encrypted_send_key(&sync, &activation, SCOPE)
            .await,
        Err(EncryptedMessageError::Key(EncryptionError::MissingKey))
    ));
    let previous = GroupKey::from_bytes([7; 32]);
    keys.save_key(&activation, CHAT, PREVIOUS_KEY, &previous)
        .unwrap();
    for mode in [Access::NoCurrentKey, Access::NullCurrentKey] {
        *test.api.access.lock().unwrap() = mode;
        assert!(matches!(
            test.source
                .prepare_encrypted_send(&sync, &activation, SCOPE, "доступен старый ключ".into())
                .await,
            Err(EncryptedMessageError::Destination(
                DirectSendError::EncryptionUnavailable
            ))
        ));
        assert!(storage.pending_sends(&test.b).unwrap().is_empty());
    }
    *test.api.access.lock().unwrap() = Access::Encrypted;
    *test.api.default_topic.lock().unwrap() = OTHER_TOPIC;
    assert!(matches!(
        test.source
            .prepare_encrypted_send_to_expected(
                &sync,
                &activation,
                SCOPE,
                TOPIC,
                "старый маршрут редактора".into(),
            )
            .await,
        Err(EncryptedMessageError::TopicChanged)
    ));
    assert!(storage.pending_sends(&test.b).unwrap().is_empty());
    *test.api.default_topic.lock().unwrap() = TOPIC;
    assert_eq!(
        test.source
            .encrypted_send_key(&sync, &activation, SCOPE)
            .await
            .unwrap()
            .key_id,
        PREVIOUS_KEY
    );
    keys.save_key(
        &activation,
        CHAT,
        Uuid::from_u128(74),
        &GroupKey::from_bytes([5; 32]),
    )
    .unwrap();
    let text = "секрет первой и повторной отправки";
    let snapshot = test
        .source
        .prepare_encrypted_send(&sync, &activation, SCOPE, text.into())
        .await
        .unwrap();
    let PreparedSendBody::Encrypted {
        key_id,
        ciphertext,
        device_id,
    } = &snapshot.body
    else {
        panic!("очередь ждёт конверт")
    };
    assert_eq!((*key_id, *device_id), (PREVIOUS_KEY, TEST_DEVICE));
    let opened = keys
        .prepare(&activation, CHAT, *key_id)
        .unwrap()
        .decrypt(ciphertext);
    assert_eq!(
        keys.accept(opened)
            .unwrap()
            .with_body(|value| value == text),
        Some(true)
    );
    let path = test.directory.path().join("outbox.sqlite3");
    let database = rusqlite::Connection::open(&path).unwrap();
    let saved: (String, String) = database
        .query_row(
            "SELECT markdown, ciphertext FROM messenger_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(saved, (String::new(), ciphertext.clone()));
    assert!(
        !std::fs::read(path)
            .unwrap()
            .windows(text.len())
            .any(|bytes| bytes == text.as_bytes())
    );
    drop(database);
    drop(sync);
    drop(storage);

    let (storage, sync) = test.open();
    let (stop, runtime) = test.run(sync.clone());
    test.api.ready.send_replace(true);
    let first = test.post(&snapshot).await;
    first.reply.send(None).unwrap();
    test.wait_status(&sync, &snapshot, Some(DeliveryStatus::Unknown))
        .await;

    keys.save_key(
        &activation,
        CHAT,
        CURRENT_KEY,
        &GroupKey::from_bytes([12; 32]),
    )
    .unwrap();
    assert_eq!(
        test.source
            .encrypted_send_key(&sync, &activation, SCOPE)
            .await
            .unwrap()
            .key_id,
        CURRENT_KEY
    );
    keys.remove_key(&activation, CHAT, PREVIOUS_KEY).unwrap();
    assert!(
        sync.retry_encrypted_send(&test.b, generation, snapshot.canonical_id)
            .unwrap()
    );
    test.wait_status(
        &sync,
        &snapshot,
        Some(DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable)),
    )
    .await;
    test.no_post(Duration::from_millis(100)).await;
    keys.save_key(
        &activation,
        CHAT,
        PREVIOUS_KEY,
        &GroupKey::from_bytes([9; 32]),
    )
    .unwrap();
    assert!(
        sync.retry_encrypted_send(&test.b, generation, snapshot.canonical_id)
            .unwrap()
    );
    test.wait_status(
        &sync,
        &snapshot,
        Some(DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable)),
    )
    .await;
    test.no_post(Duration::from_millis(100)).await;
    keys.save_key(&activation, CHAT, PREVIOUS_KEY, &previous)
        .unwrap();
    assert!(
        sync.retry_encrypted_send(&test.b, generation, snapshot.canonical_id)
            .unwrap()
    );
    let retry = test.post(&snapshot).await;
    assert_eq!(retry.body, first.body);
    retry
        .reply
        .send(Some(("201 Created", delivered(&snapshot))))
        .unwrap();
    test.wait_status(&sync, &snapshot, None).await;
    stop.send(()).unwrap();
    runtime.await.unwrap();

    let target = MessageTarget {
        scope: SCOPE,
        topic_id: snapshot.topic_id,
        message_id: snapshot.public_id,
    };
    let original = delivered(&snapshot);
    let expected = HistoryMessage {
        id: snapshot.public_id,
        stream_id: snapshot.stream_id,
        topic_id: snapshot.topic_id,
        body: MessageBody::Encrypted {
            key_id: PREVIOUS_KEY,
            ciphertext: ciphertext.clone(),
        },
        metadata: HistoryMetadata {
            author_id: Some(B),
            created_at: HistoryTime::parse("2026-01-01T12:00:00Z").unwrap(),
            updated_at: None,
        },
    };
    let source = test.source.clone();
    let replacement = "Правка использует старое поколение";
    let mut newer = original.clone();
    newer["updated_at"] = json!("2026-01-01T12:01:00Z");
    test.api
        .lookups
        .lock()
        .unwrap()
        .insert(snapshot.public_id, newer);
    assert!(matches!(
        source
            .edit_encrypted_message(&sync, &activation, target, &expected, replacement.into())
            .await,
        Err(EncryptedMessageError::StaleEdit)
    ));
    test.no_post(Duration::from_millis(100)).await;
    test.api
        .lookups
        .lock()
        .unwrap()
        .insert(snapshot.public_id, original.clone());
    let cursor = sync.cursor(&test.b, "messenger").unwrap();
    let mut changes = sync.subscribe();
    let edit =
        source.edit_encrypted_message(&sync, &activation, target, &expected, replacement.into());
    let serve_edit = async {
        let put = timeout(Duration::from_secs(3), test.requests.recv())
            .await
            .unwrap()
            .unwrap();
        let payload = &put.body["payload"];
        assert_eq!(payload["kind"], "e2ee");
        assert_eq!(payload["key_uuid"], json!(PREVIOUS_KEY));
        assert_eq!(put.body.as_object().unwrap().len(), 1);
        assert!(!put.body.to_string().contains(replacement));
        let opened = keys
            .prepare(&activation, CHAT, PREVIOUS_KEY)
            .unwrap()
            .decrypt(payload["content"].as_str().unwrap());
        assert_eq!(
            keys.accept(opened)
                .unwrap()
                .with_body(|value| value == replacement),
            Some(true)
        );
        let mut updated = original.clone();
        updated["payload"] = payload.clone();
        updated["updated_at"] = json!("2026-01-01T12:01:00Z");
        test.api
            .lookups
            .lock()
            .unwrap()
            .insert(snapshot.public_id, updated.clone());
        put.reply.send(Some(("200 OK", updated))).unwrap();
    };
    let (edited, ()) = tokio::join!(edit, serve_edit);
    let edited = edited.unwrap();
    assert!(matches!(
        edited.body,
        MessageBody::Encrypted {
            key_id: PREVIOUS_KEY,
            ..
        }
    ));
    assert_eq!(
        storage
            .message(&test.b, &snapshot.public_id.to_string())
            .unwrap()
            .unwrap()
            .body,
        edited.body
    );
    assert_eq!(sync.cursor(&test.b, "messenger").unwrap(), cursor);
    assert_eq!(
        changes.try_recv().unwrap().change,
        SyncChange::HistoryMessageChanged {
            message_id: snapshot.public_id
        }
    );
    let stale_page = sync
        .prepare_history_page(&test.b, generation, SCOPE, HistoryPageTarget::New)
        .unwrap()
        .unwrap();
    assert_eq!(
        sync.save_history_page(
            stale_page,
            &[expected],
            HistoryPageEdges::New {
                older: HistoryEdge::End,
                newer: HistoryEdge::End,
            }
        )
        .unwrap(),
        HistoryPageOutcome::Stale
    );
    let expected = edited;
    sync.apply(
        &test.b,
        generation,
        "messenger",
        Event::HistoryChanged {
            cursor: Cursor {
                epoch_generation: "test".into(),
                epoch_version: 2,
            },
            change: HistoryChange::Edit {
                message: Message {
                    id: snapshot.public_id.to_string(),
                    body: expected.body.clone(),
                },
                placement: None,
            },
        },
    )
    .unwrap();
    assert!(matches!(
        changes.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(
        storage
            .message(&test.b, &snapshot.public_id.to_string())
            .unwrap()
            .unwrap()
            .body,
        expected.body
    );
    keys.remove_key(&activation, CHAT, PREVIOUS_KEY).unwrap();
    assert!(matches!(
        source
            .edit_encrypted_message(&sync, &activation, target, &expected, replacement.into())
            .await,
        Err(EncryptedMessageError::Key(EncryptionError::MissingKey))
    ));
    test.no_post(Duration::from_millis(100)).await;

    keys.save_key(&activation, CHAT, PREVIOUS_KEY, &previous)
        .unwrap();
    let (captured, snapshot_captured) = oneshot::channel();
    let (release_get, release) = oneshot::channel();
    *test.api.lookup_gate.lock().unwrap() = Some(LookupGate { captured, release });
    let edit =
        source.edit_encrypted_message(&sync, &activation, target, &expected, replacement.into());
    let local_update = async {
        timeout(Duration::from_secs(3), snapshot_captured)
            .await
            .unwrap()
            .unwrap();
        sync.apply(
            &test.b,
            generation,
            "messenger",
            Event::HistoryChanged {
                cursor: Cursor {
                    epoch_generation: "test".into(),
                    epoch_version: 3,
                },
                change: HistoryChange::Edit {
                    message: Message {
                        id: snapshot.public_id.to_string(),
                        body: MessageBody::Encrypted {
                            key_id: PREVIOUS_KEY,
                            ciphertext: "YQ==".into(),
                        },
                    },
                    placement: None,
                },
            },
        )
        .unwrap();
        release_get.send(()).unwrap();
    };
    let (stale, ()) = tokio::join!(edit, local_update);
    assert!(matches!(stale, Err(EncryptedMessageError::StaleEdit)));
    test.no_post(Duration::from_millis(100)).await;
}

/// Приглашение в Jitsi открывается только после доставки того же защищённого конверта.
#[tokio::test]
async fn encrypted_call_releases_link_only_after_exact_delivery() {
    let mut test = Test::new().await;
    let (keys, activation) = test.enable_encryption();
    let (storage, sync) = test.open();
    let generation = test.session().generation;
    for bad in [
        None,
        Some("http://meet.test"),
        Some("https://user@meet.test"),
        Some("https://meet.test/room"),
        Some("https://meet.test/?parameter=true"),
    ] {
        *test.api.meet_url.lock().unwrap() = bad.map(str::to_owned);
        assert!(
            test.source
                .jitsi_settings(&sync, &test.b, generation)
                .await
                .is_err()
        );
    }
    *test.api.meet_url.lock().unwrap() = Some("https://meet.test".into());
    let settings = test
        .source
        .jitsi_settings(&sync, &test.b, generation)
        .await
        .unwrap();
    assert!(
        settings
            .recognize_invitation("https://meet.test.evil.test/room")
            .is_none()
    );
    assert!(
        settings
            .recognize_invitation("https://meet.test/path/room")
            .is_none()
    );
    assert!(
        settings
            .recognize_invitation("https://meet.test/room?jwt=private")
            .is_none()
    );
    assert!(
        settings
            .recognize_invitation("https://meet.jit.si/room")
            .is_some()
    );
    assert!(
        test.source
            .prepare_encrypted_call(&sync, &activation, SCOPE, TOPIC)
            .await
            .is_err()
    );
    assert!(storage.pending_sends(&test.b).unwrap().is_empty());
    keys.save_key(
        &activation,
        CHAT,
        CURRENT_KEY,
        &GroupKey::from_bytes([82; 32]),
    )
    .unwrap();
    let pending = test
        .source
        .prepare_encrypted_call(&sync, &activation, SCOPE, TOPIC)
        .await
        .unwrap();
    let snapshot = storage.pending_sends(&test.b).unwrap().pop().unwrap();
    assert_eq!(pending.message_id(), snapshot.public_id);
    let PreparedSendBody::Encrypted {
        key_id, ciphertext, ..
    } = &snapshot.body
    else {
        panic!("приглашение должно остаться конвертом");
    };
    assert_eq!(*key_id, CURRENT_KEY);
    let opened = keys
        .accept(
            keys.prepare(&activation, CHAT, *key_id)
                .unwrap()
                .decrypt(ciphertext),
        )
        .unwrap();
    let url = opened.with_body(str::to_owned).unwrap();
    assert!(url.starts_with("https://meet.test/workspace-"));
    assert!(url.contains(&format!("-{CHAT}-{TOPIC}-")));
    assert_eq!(settings.recognize_invitation(&url).unwrap().as_str(), url);
    let db = std::fs::read(test.directory.path().join("outbox.sqlite3")).unwrap();
    assert!(!db.windows(url.len()).any(|bytes| bytes == url.as_bytes()));

    let (stop, runtime) = test.run(sync.clone());
    test.api.ready.send_replace(true);
    let post = test.post(&snapshot).await;
    assert!(!post.body.to_string().contains("meet.test"));
    assert_eq!(post.body["payload"]["content"], json!(ciphertext));
    let waiting = test.source.wait_for_call_invitation(&sync, pending);
    tokio::pin!(waiting);
    assert!(
        timeout(Duration::from_millis(50), &mut waiting)
            .await
            .is_err()
    );
    let remote = delivered(&snapshot);
    post.reply
        .send(Some(("201 Created", remote.clone())))
        .unwrap();
    let confirmed = timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        confirmed.with_url(str::to_owned).as_deref(),
        Some(url.as_str())
    );
    test.api
        .lookups
        .lock()
        .unwrap()
        .insert(snapshot.public_id, remote);
    let expected = test
        .source
        .load_quote_original(&sync, &test.b, generation, SCOPE, snapshot.public_id)
        .await
        .unwrap();
    let join = test
        .source
        .confirm_jitsi_join(&sync, &activation, SCOPE, &expected)
        .await
        .unwrap();
    assert_eq!(join.with_url(str::to_owned).as_deref(), Some(url.as_str()));
    let mut stale = expected.clone();
    stale.metadata.updated_at = HistoryTime::parse("2026-01-01T10:00:00Z");
    assert!(
        test.source
            .confirm_jitsi_join(&sync, &activation, SCOPE, &stale)
            .await
            .is_err()
    );
    assert!(
        test.source
            .confirm_jitsi_join(
                &sync,
                &activation,
                HistoryScope {
                    stream_id: CHAT,
                    topic_id: Some(OTHER_TOPIC),
                },
                &expected
            )
            .await
            .is_err()
    );

    keys.remove_key(&activation, CHAT, CURRENT_KEY).unwrap();
    assert!(
        confirmed
            .with_url(|_| panic!("отозванную ссылку нельзя открыть"))
            .is_none()
    );
    assert!(
        join.with_url(|_| panic!("без исходного ключа вход невозможен"))
            .is_none()
    );
    keys.save_key(
        &activation,
        CHAT,
        CURRENT_KEY,
        &GroupKey::from_bytes([82; 32]),
    )
    .unwrap();
    let join = test
        .source
        .confirm_jitsi_join(&sync, &activation, SCOPE, &expected)
        .await
        .unwrap();
    sync.apply(
        &test.b,
        generation,
        "messenger",
        Event::HistoryChanged {
            cursor: Cursor {
                epoch_generation: "test".into(),
                epoch_version: 2,
            },
            change: HistoryChange::RemoveTopic(MessagePlacement {
                stream_id: CHAT.to_string(),
                topic_id: TOPIC.to_string(),
            }),
        },
    )
    .unwrap();
    assert!(
        join.with_url(|_| panic!("с той же сессией отозванная тема не открывается"))
            .is_none()
    );
    assert!(
        test.source
            .confirm_jitsi_join(&sync, &activation, SCOPE, &expected)
            .await
            .is_err()
    );
    stop.send(()).unwrap();
    runtime.await.unwrap();
}

/// После перезапуска закрывает повтор по подтверждению, пришедшему до готовности сокета.
#[tokio::test]
async fn late_confirmation_closes_encrypted_retry_before_reclaim_after_restart() {
    let mut test = Test::new().await;
    let (keys, activation) = test.enable_encryption();
    let generation = test.session().generation;
    keys.save_key(
        &activation,
        CHAT,
        CURRENT_KEY,
        &GroupKey::from_bytes([7; 32]),
    )
    .unwrap();
    let (_, sync) = test.open();
    let first = test
        .source
        .prepare_encrypted_send(&sync, &activation, SCOPE, "первая попытка".into())
        .await
        .unwrap();
    let (stop, runtime) = test.run(sync.clone());
    test.api.ready.send_replace(true);
    test.post(&first).await.reply.send(None).unwrap();
    test.wait_status(&sync, &first, Some(DeliveryStatus::Unknown))
        .await;
    stop.send(()).unwrap();
    runtime.await.unwrap();

    assert!(
        sync.retry_encrypted_send(&test.b, generation, first.canonical_id)
            .unwrap()
    );
    let never_dispatched = test
        .source
        .prepare_encrypted_send(&sync, &activation, SCOPE, "ещё в очереди".into())
        .await
        .unwrap();
    keys.remove_key(&activation, CHAT, CURRENT_KEY).unwrap();
    drop(sync);
    test.api.ready.send_replace(false);
    test.api.page_events.lock().unwrap().extend([
        message_created(2, delivered(&first)),
        message_created(3, delivered(&never_dispatched)),
    ]);

    let (storage, sync) = test.open();
    assert_eq!(
        storage.pending_sends(&test.b).unwrap(),
        [first.clone(), never_dispatched.clone()]
    );
    let (stop, runtime) = test.run(sync.clone());
    test.wait_status(&sync, &first, None).await;
    test.wait_status(&sync, &never_dispatched, Some(DeliveryStatus::Waiting))
        .await;
    assert!(
        storage
            .message(&test.b, &first.public_id.to_string())
            .unwrap()
            .is_some()
    );
    test.api.ready.send_replace(true);
    test.wait_status(
        &sync,
        &never_dispatched,
        Some(DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable)),
    )
    .await;
    test.no_post(Duration::from_millis(100)).await;
    stop.send(()).unwrap();
    runtime.await.unwrap();
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

/// Проверяет живой источник и точный фрагмент по HTTPS до любых записей очереди.
#[tokio::test]
async fn validates_reply_sources_by_account_and_topic_once_per_message() {
    let test = Test::new().await;
    let (_, sync) = test.open();
    let generation = test.session().generation;
    let id = Uuid::from_u128(0x777);
    let quote = |fragment: &str| QuoteSource {
        message_id: id,
        fragment: Some(fragment.to_owned()),
    };
    let sources = [quote("строка"), quote("😀")];
    let verify = |scope| {
        test.source
            .verify_reply_sources(&sync, &test.b, generation, scope, &sources)
    };
    let valid = json!({
        "uuid": id, "stream_uuid": CHAT, "topic_uuid": TOPIC,
        "project_id": "project", "user_uuid": B, "author_uuid": A,
        "created_at":"2026-01-01T12:00:00Z",
        "payload":{"kind":"markdown","content":"Первая строка 😀"}
    });

    test.api.lookups.lock().unwrap().insert(id, valid.clone());
    let topic = HistoryScope {
        stream_id: CHAT,
        topic_id: Some(TOPIC),
    };
    assert_eq!(verify(topic).await, Ok(()));
    assert_eq!(test.api.lookup_count.load(Relaxed), 1);

    let wrong_topic = HistoryScope {
        stream_id: CHAT,
        topic_id: Some(OTHER_TOPIC),
    };
    assert_eq!(verify(wrong_topic).await, Err(SourceCheckError::Missing));

    let mut wrong_owner = valid.clone();
    wrong_owner["user_uuid"] = json!(A);
    test.api.lookups.lock().unwrap().insert(id, wrong_owner);
    assert_eq!(verify(topic).await, Err(SourceCheckError::Unavailable));

    let mut replaced = valid;
    replaced["payload"]["content"] = json!("Другая строка");
    test.api.lookups.lock().unwrap().insert(id, replaced);
    assert_eq!(verify(topic).await, Err(SourceCheckError::Changed));

    test.api.lookup_failure.store(true, Relaxed);
    assert_eq!(verify(topic).await, Err(SourceCheckError::Unavailable));
    test.api.lookup_failure.store(false, Relaxed);
    test.api.lookups.lock().unwrap().remove(&id);
    assert_eq!(verify(topic).await, Err(SourceCheckError::Missing));
    assert!(sync.outbox(&test.b, generation, topic).unwrap().is_empty());
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

/// Новый снимок оригинала закрывает прежнюю цитату до записи истории.
#[tokio::test]
async fn protected_quote_permit_tracks_committed_original_without_ui_refresh() {
    let mut test = Test::new().await;
    let (keys, activation) = test.enable_encryption();
    let (_, sync) = test.open();
    let generation = test.session().generation;
    keys.save_key(
        &activation,
        CHAT,
        CURRENT_KEY,
        &GroupKey::from_bytes([82; 32]),
    )
    .unwrap();
    let first = test
        .source
        .prepare_encrypted_send(&sync, &activation, SCOPE, "Первый видимый оригинал".into())
        .await
        .unwrap();
    let mut original_wire = delivered(&first);
    original_wire["updated_at"] = json!("2026-01-01T12:00:30Z");
    test.api
        .lookups
        .lock()
        .unwrap()
        .insert(first.public_id, original_wire.clone());
    let original = test
        .source
        .load_quote_original(&sync, &test.b, generation, SCOPE, first.public_id)
        .await
        .unwrap();
    let parent = test
        .source
        .prepare_encrypted_send(
            &sync,
            &activation,
            SCOPE,
            format!("[Оригинал](urn:quote:{})\n\nОтвет", first.public_id),
        )
        .await
        .unwrap();
    let PreparedSendBody::Encrypted {
        key_id, ciphertext, ..
    } = &parent.body
    else {
        unreachable!();
    };
    let citing = HistoryMessage {
        id: parent.public_id,
        stream_id: parent.stream_id,
        topic_id: parent.topic_id,
        body: MessageBody::Encrypted {
            key_id: *key_id,
            ciphertext: ciphertext.clone(),
        },
        metadata: HistoryMetadata {
            updated_at: None,
            ..original.metadata.clone()
        },
    };
    let parent = Arc::new(
        keys.accept(
            keys.prepare(&activation, CHAT, *key_id)
                .unwrap()
                .decrypt(ciphertext),
        )
        .unwrap(),
    );
    let store_snapshot = |message: &HistoryMessage| {
        let guard = sync
            .prepare_history_page(&test.b, generation, SCOPE, HistoryPageTarget::New)
            .unwrap()
            .unwrap();
        assert!(matches!(
            sync.save_history_page(
                guard,
                std::slice::from_ref(message),
                HistoryPageEdges::New {
                    older: HistoryEdge::End,
                    newer: HistoryEdge::End,
                }
            )
            .unwrap(),
            HistoryPageOutcome::Saved(_)
        ));
    };
    store_snapshot(&citing);
    let resolve = || {
        test.source.resolve_protected_quote(
            &sync,
            &activation,
            parent.clone(),
            &citing,
            first.public_id,
            0,
        )
    };
    let quote = resolve().await.unwrap().content.unwrap();
    assert_eq!(
        quote.with_text("Цитата", str::to_owned).as_deref(),
        Some("Первый видимый оригинал")
    );
    let repeated = resolve().await.unwrap().content.unwrap();
    assert!(quote.with_current(|| ()).is_some());
    let changed = test
        .source
        .prepare_encrypted_send(&sync, &activation, SCOPE, "Второй видимый оригинал".into())
        .await
        .unwrap();
    let mut changed_wire = delivered(&changed);
    changed_wire["uuid"] = json!(first.public_id);
    changed_wire["updated_at"] = json!("2026-01-01T12:01:00Z");
    test.api
        .lookups
        .lock()
        .unwrap()
        .insert(first.public_id, changed_wire.clone());
    let mut changes = sync.subscribe();
    let after_edit = resolve().await.unwrap().content.unwrap();
    assert!(quote.with_current(|| ()).is_none());
    assert!(repeated.with_current(|| ()).is_none());
    assert_eq!(
        changes.try_recv().unwrap().change,
        SyncChange::QuoteOriginChanged {
            message_id: first.public_id
        }
    );
    test.api
        .lookups
        .lock()
        .unwrap()
        .insert(first.public_id, original_wire);
    assert!(matches!(
        resolve().await,
        Err(workspace_core::messenger::history::OpenHistoryError::StaleWindow)
    ));
    assert!(changes.try_recv().is_err());
    store_snapshot(&original);
    assert!(after_edit.with_current(|| ()).is_some());

    test.api
        .lookups
        .lock()
        .unwrap()
        .insert(first.public_id, changed_wire);
    let changed = test
        .source
        .load_quote_original(&sync, &test.b, generation, SCOPE, first.public_id)
        .await
        .unwrap();
    store_snapshot(&changed);
    assert_eq!(
        after_edit.with_text("Цитата", str::to_owned).as_deref(),
        Some("Второй видимый оригинал")
    );
    let mut confirmed_citing = citing.clone();
    confirmed_citing.metadata.updated_at = HistoryTime::parse("2026-01-01T12:02:00Z");
    store_snapshot(&confirmed_citing);
    assert!(after_edit.with_current(|| ()).is_none());
    assert!(resolve().await.is_err());
    let after_citing_edit = test
        .source
        .resolve_protected_quote(
            &sync,
            &activation,
            parent.clone(),
            &confirmed_citing,
            first.public_id,
            0,
        )
        .await
        .unwrap()
        .content
        .unwrap();
    assert!(after_citing_edit.with_current(|| ()).is_some());
    sync.apply(
        &test.b,
        generation,
        "messenger",
        Event::HistoryChanged {
            cursor: Cursor {
                epoch_generation: "test".into(),
                epoch_version: 2,
            },
            change: HistoryChange::RemoveTopic(MessagePlacement {
                stream_id: CHAT.to_string(),
                topic_id: TOPIC.to_string(),
            }),
        },
    )
    .unwrap();
    assert!(
        after_citing_edit
            .with_current(|| panic!("Отозванную область нельзя прочитать"))
            .is_none()
    );
    assert!(parent.with_body(|text| text.contains("Ответ")).unwrap());
    assert!(resolve().await.is_err());
}
