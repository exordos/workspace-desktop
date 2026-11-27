//! Проверяет открытие истории по уведомлению и загрузку последней страницы через HTTPS.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering::Relaxed},
};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use reqwest::Url;
use rustls::{
    ServerConfig,
    crypto::ring,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::Notify,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message as Frame, handshake::derive_accept_key, protocol::Role},
};
use uuid::Uuid;
use workspace_core::{
    auth::AuthError,
    messenger::{MessengerSource, history::OpenHistoryError},
    notification_target::NotificationTarget,
    runtime::{EventSource, SourceSession},
    storage::{
        Cursor, HistoryChange, HistoryEdge, HistoryPageEdges, HistoryPageOutcome,
        HistoryPageTarget, HistoryScope, MessageBody, Storage,
    },
    sync::{Event, Sync, SyncChange},
};

use super::Fixture;

#[path = "attachment_upload.rs"]
mod attachment_upload;
#[path = "avatars.rs"]
mod avatars;
#[path = "folder_runtime.rs"]
mod folder_runtime;
#[path = "key_lifecycle.rs"]
mod key_lifecycle;
#[path = "media_cache.rs"]
mod media_cache;
#[path = "outgoing.rs"]
mod outgoing;

const CHAT: Uuid = Uuid::from_u128(10);
const TOPIC: Uuid = Uuid::from_u128(11);
const OLDER: Uuid = Uuid::from_u128(12);
const ANCHOR: Uuid = Uuid::from_u128(13);
const NEWER: Uuid = Uuid::from_u128(14);
const OTHER_TOPIC: Uuid = Uuid::from_u128(15);
const LIVE: Uuid = Uuid::from_u128(16);
const OUTSIDE: Uuid = Uuid::from_u128(17);
const LEGACY_KEY: Uuid = Uuid::from_u128(42);
const CA: &[u8] = include_bytes!("../../../../tests/fixtures/tls/ca.pem");
const CERT: &[u8] = include_bytes!("../../../../tests/fixtures/tls/localhost.pem");
const KEY: &[u8] = include_bytes!("../../../../tests/fixtures/tls/localhost-key.pem");

type HttpsSocket = BufReader<tokio_rustls::server::TlsStream<TcpStream>>;

/// Согласует протокол событий и сохраняет уже считанные из сокета данные.
async fn upgrade(mut socket: HttpsSocket, headers: &str) -> WebSocketStream<HttpsSocket> {
    let key = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim())
        })
        .unwrap();
    let accept = derive_accept_key(key.as_bytes());
    socket.get_mut().write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: workspace.events.v1\r\n\r\n").as_bytes()).await.unwrap();
    WebSocketStream::from_raw_socket(socket, Role::Server, None).await
}

/// Даёт тесту управлять ответами и задержать одну страницу до выхода из аккаунта.
#[derive(Default)]
struct HistoryApi {
    access_mode: AtomicUsize, // 0 — доступ, 1 — 403, 2 — другой владелец, 3 — другой стрим, 4 — задержка, 5 — обрыв, 6 — другая тема.
    pages: AtomicUsize,       // Количество запросов соседних страниц.
    anchor_other_topic: AtomicUsize, // 1 — целевое сообщение находится в соседней теме.
    require_topic: AtomicUsize, // 1 — соседние страницы обязаны содержать фильтр темы.
    deny: AtomicUsize,        // 0 — доступ есть, 1 — скрыт чат, 2 — скрыто сообщение.
    delay: Notify,            // Запрос страницы дошёл до сервера.
    latest_mode: AtomicUsize, // 0 — обычная страница, 1 — сбой, 2 — задержка, 3 — чужая тема, 4 — маркер и в теме.
    legacy_encrypted_mode: AtomicUsize, // 0 — выключен, 1 — цель удалена, 2 — тело изменилось, 3 — подтверждено.
    older_mode: AtomicUsize, // 0 — конец, 1 — сбой, 2 — задержать один ответ, 3 — продолжение.
    older_pages: AtomicUsize, // Число запросов к старому краю.
    newer_mode: AtomicUsize, // 0 — конец, 1 — сбой, 2 — задержка, 3 — продолжение темы.
    newer_pages: AtomicUsize, // Число запросов к новому краю.
    release: Notify,         // Можно вернуть задержанный ответ.
    topic_realtime: AtomicUsize, // 1 — включить отдельный сценарий событий двух тем.
    outgoing: Option<Arc<outgoing::Api>>, // Подмена маршрутов очереди для теста отправки.
    folders: Option<Arc<folder_runtime::Api>>, // Подмена маршрутов папок и журнала.
    avatars: Option<Arc<avatars::Api>>, // Подмена профиля и файлов для тестов аватаров.
    media: Option<Arc<media_cache::Api>>, // Подмена истории, байтов и журнала для кэша вложений.
    upload: Option<Arc<attachment_upload::Api>>, // Проверяемый API отправки нового файла.
    key_lifecycle: Option<Arc<key_lifecycle::Api>>, // Управление каналом и ротацией ключа.
}

/// Формирует серверное сообщение с одинаковыми UUID для обоих аккаунтов.
fn message(id: Uuid, user: &str) -> Value {
    json!({
        "uuid": id, "stream_uuid": CHAT, "topic_uuid": TOPIC,
        "project_id": "project", "user_uuid": user, "author_uuid": ANCHOR,
        "created_at": "2026-01-01T12:00:00Z",
        "payload": { "kind": "markdown", "content": user }
    })
}

/// Формирует зашифрованную запись, одинаковую для страницы и точного запроса.
fn legacy_encrypted_message(user: &str, ciphertext: &str) -> Value {
    let mut row = message(ANCHOR, user);
    row["payload"] = json!({"kind": "e2ee", "key_uuid": LEGACY_KEY, "content": ciphertext});
    row
}

/// Принимает один HTTPS-запрос и возвращает данные только для токена B.
async fn reply(socket: TcpStream, tls: TlsAcceptor, api: Arc<HistoryApi>) {
    let mut socket = match tls.accept(socket).await {
        Ok(socket) => BufReader::new(socket),
        // При остановке синхронизации клиент может закрыть ещё не готовое соединение.
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return,
        Err(error) => panic!("ошибка TLS на тестовом сервере истории: {error}"),
    };
    let mut line = String::new();
    socket.read_line(&mut line).await.unwrap();
    let method = line.split_whitespace().next().unwrap().to_owned();
    let request = Url::parse(&format!(
        "https://localhost{}",
        line.split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    let mut headers = String::new();
    loop {
        line.clear();
        socket.read_line(&mut line).await.unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        headers.push_str(&line);
    }
    if let Some(outgoing) = &api.outgoing {
        outgoing.reply(socket, request, &method, &headers).await;
        return;
    }
    if let Some(folders) = &api.folders {
        folders.reply(socket, request, &method, &headers).await;
        return;
    }
    if let Some(avatars) = &api.avatars {
        avatars.reply(socket, request, &method, &headers).await;
        return;
    }
    if let Some(media) = &api.media {
        media.reply(socket, request, &method, &headers).await;
        return;
    }
    if let Some(upload) = &api.upload {
        upload.reply(socket, request, &method, &headers).await;
        return;
    }
    if let Some(lifecycle) = &api.key_lifecycle {
        lifecycle.reply(socket, request, &method, &headers).await;
        return;
    }
    if api.topic_realtime.load(Relaxed) == 1 && request.path().ends_with("/events/ws") {
        let mut socket = upgrade(socket, &headers).await;
        for (version, id, created) in [
            (9, LIVE, "2026-01-01T11:30:00Z"),
            (9, LIVE, "2026-01-01T11:30:00Z"),
            (10, OUTSIDE, "2026-01-01T13:00:00Z"),
        ] {
            let event = json!({"schema_version": 1, "uuid": format!("event-{version}"), "epoch_generation": "test", "epoch_version": version, "project_id": "project", "user_uuid": "first", "object_type": "message", "action": "created", "payload": {"kind": "message.created", "uuid": id, "stream_uuid": CHAT, "topic_uuid": TOPIC, "author_uuid": ANCHOR, "created_at": created, "payload": {"kind": "markdown", "content": "live"}}});
            socket.send(Frame::text(event.to_string())).await.unwrap();
        }
        return;
    }
    let user = if headers.contains("authorization: Bearer second") {
        "second"
    } else {
        assert!(headers.contains("authorization: Bearer first"));
        "first"
    };
    let mut marker = String::new();
    let legacy_mode = api.legacy_encrypted_mode.load(Relaxed);
    let (status, body) = if legacy_mode != 0 && request.path().ends_with("/messages/") {
        ("200 OK", json!([legacy_encrypted_message(user, "YQ==")]))
    } else if legacy_mode != 0 && request.path().ends_with(&format!("/messages/{ANCHOR}")) {
        match legacy_mode {
            1 => ("404 Not Found", Value::Null),
            2 => ("200 OK", legacy_encrypted_message(user, "Yg==")),
            _ => ("200 OK", legacy_encrypted_message(user, "YQ==")),
        }
    } else if api.topic_realtime.load(Relaxed) == 1 && request.path().ends_with("/events/") {
        ("200 OK", json!([]))
    } else if request.path().ends_with(&format!("/streams/{CHAT}")) {
        if api.deny.load(Relaxed) == 1 {
            ("404 Not Found", Value::Null)
        } else {
            (
                "200 OK",
                json!({
                    "uuid": CHAT, "user_uuid": user, "project_id": "project",
                    "private": true, "is_archived": false, "direct_user_uuid": ANCHOR,
                    "notification_mode": "all_messages"
                }),
            )
        }
    } else if request.path().ends_with(&format!("/stream_topics/{TOPIC}"))
        || (api.topic_realtime.load(Relaxed) == 1
            && request
                .path()
                .ends_with(&format!("/stream_topics/{OTHER_TOPIC}")))
    {
        let mode = api.access_mode.load(Relaxed);
        if mode == 4 {
            api.delay.notify_one();
            api.release.notified().await;
        }
        if mode == 5 {
            return;
        }
        if mode == 1 {
            ("403 Forbidden", Value::Null)
        } else {
            (
                "200 OK",
                json!({
                    "uuid": if mode == 6 { ANCHOR } else if request.path().ends_with(&OTHER_TOPIC.to_string()) { OTHER_TOPIC } else { TOPIC },
                    "stream_uuid": if mode == 3 { ANCHOR } else { CHAT },
                    "user_uuid": if mode == 2 { "other" } else { user },
                    "project_id": "project", "notification_mode": "default"
                }),
            )
        }
    } else if request.path().ends_with(&format!("/messages/{ANCHOR}")) {
        if api.deny.load(Relaxed) == 2 {
            ("404 Not Found", Value::Null)
        } else {
            let mut anchor = message(ANCHOR, user);
            if api.anchor_other_topic.load(Relaxed) == 1 {
                anchor["topic_uuid"] = json!(NEWER);
            }
            ("200 OK", anchor)
        }
    } else {
        assert!(request.path().ends_with("/messages/"));
        let query = request
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query["stream_uuid"], CHAT.to_string());
        assert_eq!(query["page_limit"], "50");
        assert_eq!(query["sort_key"], "created_at");
        if query
            .get("page_marker")
            .is_some_and(|value| value == &NEWER.to_string())
        {
            assert_eq!(query["sort_dir"], "asc");
            api.newer_pages.fetch_add(1, Relaxed);
            let mode = api.newer_mode.load(Relaxed);
            if mode == 1 {
                ("500 Internal Server Error", Value::Null)
            } else {
                assert_eq!(
                    query.get("topic_uuid").map(|value| value.as_ref()),
                    (mode == 3).then(|| TOPIC.to_string()).as_deref()
                );
                if mode == 2 {
                    api.newer_mode.store(0, Relaxed);
                    api.delay.notify_one();
                    api.release.notified().await;
                }
                if mode == 3 {
                    marker = format!("X-Pagination-Marker: {ANCHOR}\r\n");
                }
                let mut newer = message(ANCHOR, user);
                newer["created_at"] = json!("2026-01-01T13:00:00Z");
                ("200 OK", json!([newer]))
            }
        } else if query
            .get("page_marker")
            .is_some_and(|marker| marker == &OLDER.to_string())
        {
            assert_eq!(query["sort_dir"], "desc");
            api.older_pages.fetch_add(1, Relaxed);
            let mut older = message(ANCHOR, user);
            older["created_at"] = json!("2026-01-01T10:00:00Z");
            match api.older_mode.load(Relaxed) {
                1 => ("500 Internal Server Error", Value::Null),
                mode => {
                    assert_eq!(
                        query.get("topic_uuid").map(|topic| topic.as_ref()),
                        (mode == 3).then(|| TOPIC.to_string()).as_deref()
                    );
                    if mode == 2 {
                        api.older_mode.store(0, Relaxed);
                        api.delay.notify_one();
                        api.release.notified().await;
                    }
                    if mode == 3 {
                        marker = format!("X-Pagination-Marker: {ANCHOR}\r\n");
                    }
                    ("200 OK", json!([older]))
                }
            }
        } else if let Some(anchor) = query.get("page_marker") {
            assert_eq!(anchor, &ANCHOR.to_string());
            assert_eq!(
                query.get("topic_uuid").map(|topic| topic.as_ref()),
                (api.require_topic.load(Relaxed) == 1)
                    .then(|| TOPIC.to_string())
                    .as_deref()
            );
            let count = api.pages.fetch_add(1, Relaxed);
            if count == 4 {
                api.delay.notify_one();
                api.release.notified().await;
            }
            let neighbor = if query["sort_dir"] == "desc" {
                marker = format!("X-Pagination-Marker: {OLDER}\r\n");
                OLDER
            } else {
                assert_eq!(query["sort_dir"], "asc");
                NEWER
            };
            (
                "200 OK",
                json!([message(neighbor, user), message(ANCHOR, user)]),
            )
        } else {
            assert_eq!(query["sort_dir"], "desc");
            let mut latest = message(NEWER, user);
            let mut older = message(OLDER, user);
            older["created_at"] = json!("2026-01-01T11:00:00Z");
            if let Some(topic) = query.get("topic_uuid") {
                if api.topic_realtime.load(Relaxed) == 1 && topic == &OTHER_TOPIC.to_string() {
                    latest["topic_uuid"] = json!(OTHER_TOPIC);
                    older["topic_uuid"] = json!(OTHER_TOPIC);
                    latest["uuid"] = json!(Uuid::from_u128(18));
                    older["uuid"] = json!(Uuid::from_u128(19));
                } else {
                    assert_eq!(topic, &TOPIC.to_string());
                }
            }
            if !query.contains_key("topic_uuid") || api.latest_mode.load(Relaxed) == 4 {
                marker = format!("X-Pagination-Marker: {OLDER}\r\n");
            }
            match api.latest_mode.load(Relaxed) {
                1 => ("500 Internal Server Error", Value::Null),
                mode => {
                    if mode == 2 {
                        api.delay.notify_one();
                        api.release.notified().await;
                    }
                    if mode == 3 {
                        latest["topic_uuid"] = json!(ANCHOR);
                    }
                    ("200 OK", json!([latest, older]))
                }
            }
        }
    };
    let body = body.to_string();
    socket.get_mut().write_all(format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{marker}\r\n{body}",
        body.len()
    ).as_bytes()).await.unwrap();
    socket.get_mut().shutdown().await.unwrap();
}

/// Проверяет сетевые события темы и границы окон двух аккаунтов.
#[tokio::test]
async fn realtime_topic_windows_remain_isolated() {
    timeout(Duration::from_secs(10), async {
        let api = Arc::new(HistoryApi::default());
        api.topic_realtime.store(1, Relaxed);
        let (server, worker) = serve(api).await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let first = fixture.login_at(&server, "first");
        let second = fixture.login_at(&server, "second");
        let generation = |owner: &_| {
            fixture
                .auth
                .snapshot()
                .into_iter()
                .find(|session| &session.context == owner)
                .unwrap()
                .generation
        };
        let first_generation = generation(&first);
        let second_generation = generation(&second);
        let sync = Sync::new(
            fixture.auth.clone(),
            Arc::new(Storage::in_memory().unwrap()),
        );
        let source =
            MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
                .unwrap();
        let scope = HistoryScope {
            stream_id: CHAT,
            topic_id: Some(TOPIC),
        };
        let neighbor = HistoryScope {
            topic_id: Some(OTHER_TOPIC),
            ..scope
        };
        let first_window = source
            .open_latest_history(&sync, &first, first_generation, scope)
            .await
            .unwrap()
            .refresh()
            .await
            .unwrap();
        let neighbor_window = source
            .open_latest_history(&sync, &first, first_generation, neighbor)
            .await
            .unwrap()
            .refresh()
            .await
            .unwrap();
        let second_window = source
            .open_latest_history(&sync, &second, second_generation, scope)
            .await
            .unwrap()
            .refresh()
            .await
            .unwrap();
        let initial = Cursor {
            epoch_generation: "test".into(),
            epoch_version: 8,
        };
        sync.apply(
            &first,
            first_generation,
            "messenger",
            Event::Checkpoint(initial.clone()),
        )
        .unwrap();
        let mut updates = sync.subscribe();
        let mut events = source
            .connect(SourceSession {
                context: first.clone(),
                generation: first_generation,
                token_version: 1,
                cursor: Some(initial),
                access_token: "first".into(),
            })
            .await
            .unwrap();
        let inside = events.next().await.unwrap().unwrap();
        assert_eq!(inside.cursor().epoch_version, 9);
        sync.apply(&first, first_generation, "messenger", inside)
            .unwrap();
        assert_eq!(
            updates.try_recv().unwrap().change,
            SyncChange::HistoryMessageCreated {
                scope,
                message_id: LIVE
            }
        );
        let updated = sync
            .refresh_history_window(&first, first_generation, scope, first_window.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            updated
                .messages
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [OLDER, LIVE, NEWER]
        );
        assert_eq!(updated.scope, scope);
        assert_eq!(
            sync.cursor(&first, "messenger")
                .unwrap()
                .unwrap()
                .epoch_version,
            9
        );
        assert_eq!(
            sync.refresh_history_window(&first, first_generation, neighbor, neighbor_window.id)
                .unwrap(),
            Some(neighbor_window.clone())
        );
        assert_eq!(
            sync.refresh_history_window(&second, second_generation, scope, second_window.id)
                .unwrap(),
            Some(second_window.clone())
        );
        // Источник должен пропустить повтор версии 9 и вернуть следующее событие.
        let outside = events.next().await.unwrap().unwrap();
        assert_eq!(outside.cursor().epoch_version, 10);
        assert!(updates.try_recv().is_err());
        sync.apply(&first, first_generation, "messenger", outside)
            .unwrap();
        assert_eq!(
            updates.try_recv().unwrap().change,
            SyncChange::HistoryMessageCreated {
                scope,
                message_id: OUTSIDE
            }
        );
        let after = sync
            .refresh_history_window(&first, first_generation, scope, first_window.id)
            .unwrap()
            .unwrap();
        assert_eq!(after.messages, updated.messages);
        assert_eq!(after.newer, HistoryEdge::Unchecked);
        assert_eq!(
            sync.cursor(&first, "messenger")
                .unwrap()
                .unwrap()
                .epoch_version,
            10
        );
        assert_eq!(
            sync.refresh_history_window(&first, first_generation, neighbor, neighbor_window.id)
                .unwrap(),
            Some(neighbor_window)
        );
        assert_eq!(
            sync.refresh_history_window(&second, second_generation, scope, second_window.id)
                .unwrap(),
            Some(second_window)
        );
        worker.abort();
    })
    .await
    .expect("сценарий событий темы не завершился вовремя");
}

/// Проверяет новый край, сетевой сбой и запоздалые ответы через локальный HTTPS.
#[tokio::test]
async fn newer_history_extends_only_the_current_edge() {
    let api = Arc::new(HistoryApi::default());
    let (server, worker) = serve(api.clone()).await;
    let fixture = Fixture::new(|_, _| unreachable!());
    let owner = fixture.login_at(&server, "first");
    let generation = fixture.auth.snapshot()[0].generation;
    let sync = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let stream = HistoryScope {
        stream_id: CHAT,
        topic_id: None,
    };
    let topic = HistoryScope {
        topic_id: Some(TOPIC),
        ..stream
    };
    let cursor = Cursor {
        epoch_generation: "test".into(),
        epoch_version: 8,
    };
    sync.apply(
        &owner,
        generation,
        "messenger",
        Event::Checkpoint(cursor.clone()),
    )
    .unwrap();
    let seed = source
        .load_latest_history(&sync, &owner, generation, stream)
        .await
        .unwrap();
    let create = |scope| {
        let guard = sync
            .prepare_history_page(&owner, generation, scope, HistoryPageTarget::New)
            .unwrap()
            .unwrap();
        let outcome = sync
            .save_history_page(
                guard,
                &seed.messages,
                HistoryPageEdges::New {
                    older: HistoryEdge::More(OLDER.to_string()),
                    newer: HistoryEdge::More(NEWER.to_string()),
                },
            )
            .unwrap();
        let HistoryPageOutcome::Saved(id) = outcome else {
            panic!("окно не сохранено")
        };
        id
    };
    let stream_id = create(stream);
    let topic_id = create(topic);
    let load = |scope, id| source.load_newer_history(&sync, &owner, generation, scope, id);
    let before = sync.history_windows(&owner, generation, stream).unwrap();
    api.newer_mode.store(1, Relaxed);
    assert!(load(stream, stream_id).await.is_err());
    assert_eq!(
        sync.history_windows(&owner, generation, stream).unwrap(),
        before
    );
    api.newer_mode.store(3, Relaxed);
    let topic_window = load(topic, topic_id).await.unwrap();
    assert_eq!(topic_window.newer, HistoryEdge::More(ANCHOR.to_string()));
    assert_eq!(topic_window.older, HistoryEdge::More(OLDER.to_string()));
    assert_eq!(
        topic_window
            .messages
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [OLDER, NEWER, ANCHOR]
    );
    api.newer_mode.store(2, Relaxed);
    let replace = async {
        timeout(Duration::from_secs(3), api.delay.notified())
            .await
            .expect("запрос нового края не дошёл до сервера");
        let result = load(stream, stream_id).await.unwrap();
        api.release.notify_one();
        result
    };
    let (late, window) = tokio::join!(load(stream, stream_id), replace);
    assert!(matches!(late, Err(OpenHistoryError::StaleWindow)));
    assert_eq!(window.newer, HistoryEdge::End);
    assert_eq!(window.older, HistoryEdge::More(OLDER.to_string()));
    assert_eq!(
        window.messages.iter().map(|row| row.id).collect::<Vec<_>>(),
        [OLDER, NEWER, ANCHOR]
    );
    let count = api.newer_pages.load(Relaxed);
    assert_eq!(load(stream, stream_id).await.unwrap(), window);
    assert_eq!(api.newer_pages.load(Relaxed), count);
    assert_eq!(sync.cursor(&owner, "messenger").unwrap(), Some(cursor));
    let another = create(stream);
    let before = sync.history_windows(&owner, generation, stream).unwrap();
    api.newer_mode.store(2, Relaxed);
    let relogin = async {
        timeout(Duration::from_secs(3), api.delay.notified())
            .await
            .expect("запрос старой сессии не дошёл до сервера");
        fixture.auth.logout(&owner.account_id).unwrap();
        let fresh = fixture.login_at(&server, "first");
        api.release.notify_one();
        fresh
    };
    let (late, fresh) = tokio::join!(load(stream, another), relogin);
    assert!(matches!(
        late,
        Err(OpenHistoryError::Auth(AuthError::SessionUnavailable))
    ));
    let current = fixture
        .auth
        .snapshot()
        .into_iter()
        .find(|session| session.context == fresh)
        .unwrap()
        .generation;
    assert_eq!(
        sync.history_windows(&fresh, current, stream).unwrap(),
        before
    );
    worker.abort();
}

/// Запускает локальный сервер с тестовым сертификатом и общим счётчиком запросов.
async fn serve(api: Arc<HistoryApi>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("https://{}", listener.local_addr().unwrap());
    let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from_pem_slice(CERT).unwrap()],
            PrivateKeyDer::from_pem_slice(KEY).unwrap(),
        )
        .unwrap();
    let tls = TlsAcceptor::from(Arc::new(config));
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (socket, _) = result.unwrap();
                    connections.spawn(reply(socket, tls.clone(), api.clone()));
                }
                Some(result) = connections.join_next() => result.unwrap(),
            }
        }
    });
    (address, task)
}

/// Для B загружает и переиспользует своё окно, различает отказ и не пишет после повторного входа.
#[tokio::test]
async fn direct_history_checks_access_and_isolates_sessions() {
    let api = Arc::new(HistoryApi::default());
    let (server, worker) = serve(api.clone()).await;
    let fixture = Fixture::new(|_, _| unreachable!());
    let first = fixture.login_at(&server, "first");
    let second = fixture.login_at(&server, "second");
    let generation = |owner: &_| {
        fixture
            .auth
            .snapshot()
            .into_iter()
            .find(|session| session.context == *owner)
            .unwrap()
            .generation
    };
    let first_generation = generation(&first);
    let second_generation = generation(&second);
    let sync = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let target = NotificationTarget {
        context: second.clone(),
        conversation_id: CHAT.to_string(),
        message_id: ANCHOR.to_string(),
    };
    let first_target = NotificationTarget {
        context: first.clone(),
        ..target.clone()
    };
    let scope = HistoryScope {
        stream_id: CHAT,
        topic_id: None,
    };
    let cursor = Cursor {
        epoch_generation: "test".into(),
        epoch_version: 8,
    };
    sync.apply(
        &second,
        second_generation,
        "messenger",
        Event::Checkpoint(cursor.clone()),
    )
    .unwrap();

    let first_window = source
        .open_direct_history(&sync, &first_target, first_generation)
        .await
        .unwrap();
    let second_window = source
        .open_direct_history(&sync, &target, second_generation)
        .await
        .unwrap();
    assert_eq!(
        second_window
            .messages
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [OLDER, ANCHOR, NEWER]
    );
    assert!(
        first_window
            .messages
            .iter()
            .all(|row| row.body.markdown() == Some("first"))
    );
    assert!(
        second_window
            .messages
            .iter()
            .all(|row| row.body.markdown() == Some("second"))
    );
    assert_eq!(second_window.older, HistoryEdge::More(OLDER.to_string()));
    assert_eq!(second_window.newer, HistoryEdge::End);
    assert_eq!(sync.cursor(&second, "messenger").unwrap(), Some(cursor));

    assert_eq!(
        source
            .open_direct_history(&sync, &target, second_generation)
            .await
            .unwrap(),
        second_window
    );
    api.deny.store(1, Relaxed);
    assert!(matches!(
        source
            .open_direct_history(&sync, &target, second_generation)
            .await,
        Err(OpenHistoryError::AccessLost)
    ));
    api.deny.store(2, Relaxed);
    assert!(matches!(
        source
            .open_direct_history(&sync, &target, second_generation)
            .await,
        Err(OpenHistoryError::MessageMissing)
    ));
    assert_eq!(api.pages.load(Relaxed), 4);

    api.deny.store(0, Relaxed);
    let empty = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let pending = source.open_direct_history(&empty, &target, second_generation);
    let relogin = async {
        timeout(Duration::from_secs(3), api.delay.notified())
            .await
            .expect("страница B не запрошена");
        fixture.auth.logout(&second.account_id).unwrap();
        let fresh = fixture.login_at(&server, "second");
        api.release.notify_one();
        fresh
    };
    let (result, fresh) = tokio::join!(pending, relogin);
    assert!(matches!(
        result,
        Err(OpenHistoryError::Auth(AuthError::SessionUnavailable))
    ));
    assert!(
        empty
            .history_windows(&fresh, generation(&fresh), scope)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        sync.history_windows(&first, first_generation, scope)
            .unwrap(),
        [first_window]
    );
    worker.abort();
}

/// После обновления профиля доверяет E2EE-строке со старой отметкой только после точного запроса.
#[tokio::test]
async fn rechecks_legacy_encrypted_edit_against_exact_message_endpoint() {
    let api = Arc::new(HistoryApi::default());
    let (server, worker) = serve(api.clone()).await;
    let fixture = Fixture::new(|_, _| unreachable!());
    let owner = fixture.login_at(&server, "first");
    let generation = fixture.auth.snapshot()[0].generation;
    let profile = tempfile::tempdir().unwrap();
    let path = profile.path().join("workspace.sqlite");
    let cursor = Cursor {
        epoch_generation: "test".into(),
        epoch_version: 12,
    };
    let old_sync = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::open(&path).unwrap()),
    );
    old_sync
        .apply(
            &owner,
            generation,
            "messenger",
            Event::HistoryChanged {
                cursor: cursor.clone(),
                change: HistoryChange::RemoveMessage(ANCHOR.to_string()),
            },
        )
        .unwrap();
    drop(old_sync);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "ALTER TABLE history_changes DROP COLUMN legacy_encryption_check;
             ALTER TABLE sync_messages DROP COLUMN encryption_key_id;",
        )
        .unwrap();
    let storage = Arc::new(Storage::open(&path).unwrap());
    let sync = Sync::new(fixture.auth.clone(), storage.clone());
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let scope = HistoryScope {
        stream_id: CHAT,
        topic_id: Some(TOPIC),
    };
    let latest = || source.load_latest_history(&sync, &owner, generation, scope);
    for mode in [1, 2] {
        api.legacy_encrypted_mode.store(mode, Relaxed);
        assert!(matches!(latest().await, Err(OpenHistoryError::StaleWindow)));
        assert!(
            storage
                .message(&owner, &ANCHOR.to_string())
                .unwrap()
                .is_none()
        );
    }

    api.legacy_encrypted_mode.store(3, Relaxed);
    let window = latest().await.unwrap();
    assert_eq!(window.messages.len(), 1);
    let message = &window.messages[0];
    assert_eq!(message.id, ANCHOR);
    assert_eq!(
        message.body,
        MessageBody::Encrypted {
            key_id: LEGACY_KEY,
            ciphertext: "YQ==".into()
        }
    );
    assert_eq!(sync.cursor(&owner, "messenger").unwrap(), Some(cursor));
    api.legacy_encrypted_mode.store(1, Relaxed);
    assert_eq!(latest().await.unwrap().messages, window.messages);
    worker.abort();
}

/// Последняя страница обновляет только запрошенную область и сохраняется только для текущего входа.
#[tokio::test]
async fn latest_history_preserves_windows_on_failure_and_session_change() {
    let api = Arc::new(HistoryApi::default());
    let (server, worker) = serve(api.clone()).await;
    let fixture = Fixture::new(|_, _| unreachable!());
    let owner = fixture.login_at(&server, "first");
    let current = fixture.auth.snapshot()[0].generation;
    let sync = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let stream = HistoryScope {
        stream_id: CHAT,
        topic_id: None,
    };
    let topic = HistoryScope {
        topic_id: Some(TOPIC),
        ..stream
    };
    let cursor = Cursor {
        epoch_generation: "test".into(),
        epoch_version: 8,
    };
    sync.apply(
        &owner,
        current,
        "messenger",
        Event::Checkpoint(cursor.clone()),
    )
    .unwrap();

    let stream_window = source
        .load_latest_history(&sync, &owner, current, stream)
        .await
        .unwrap();
    assert!(
        sync.history_windows(&owner, current, topic)
            .unwrap()
            .is_empty()
    );
    let topic_window = source
        .load_latest_history(&sync, &owner, current, topic)
        .await
        .unwrap();
    assert_eq!(
        sync.history_windows(&owner, current, stream).unwrap(),
        std::slice::from_ref(&stream_window)
    );
    for window in [&stream_window, &topic_window] {
        assert_eq!(
            window.messages.iter().map(|row| row.id).collect::<Vec<_>>(),
            [OLDER, NEWER]
        );
        assert_eq!(window.newer, HistoryEdge::End);
    }
    assert_eq!(stream_window.older, HistoryEdge::More(OLDER.to_string()));
    assert_eq!(topic_window.older, HistoryEdge::End);
    assert_eq!(sync.cursor(&owner, "messenger").unwrap(), Some(cursor));

    api.latest_mode.store(1, Relaxed);
    assert!(
        source
            .load_latest_history(&sync, &owner, current, stream)
            .await
            .is_err()
    );
    assert_eq!(
        sync.history_windows(&owner, current, stream).unwrap(),
        [stream_window]
    );
    api.latest_mode.store(3, Relaxed);
    assert!(
        source
            .load_latest_history(&sync, &owner, current, topic)
            .await
            .is_err()
    );
    assert_eq!(
        sync.history_windows(&owner, current, topic).unwrap(),
        [topic_window]
    );

    api.latest_mode.store(2, Relaxed);
    let empty = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let pending = source.load_latest_history(&empty, &owner, current, stream);
    let relogin = async {
        timeout(Duration::from_secs(3), api.delay.notified())
            .await
            .expect("последняя страница не запрошена");
        fixture.auth.logout(&owner.account_id).unwrap();
        let fresh = fixture.login_at(&server, "first");
        api.release.notify_one();
        fresh
    };
    let (result, fresh) = tokio::join!(pending, relogin);
    assert!(matches!(
        result,
        Err(OpenHistoryError::Auth(AuthError::SessionUnavailable))
    ));
    let fresh_generation = fixture
        .auth
        .snapshot()
        .into_iter()
        .find(|session| session.context == fresh)
        .unwrap()
        .generation;
    assert!(
        empty
            .history_windows(&fresh, fresh_generation, stream)
            .unwrap()
            .is_empty()
    );
    worker.abort();
}

/// Проверяет догрузку стрима и темы, повтор после сбоя и отклонение устаревших ответов.
#[tokio::test]
async fn older_history_extends_only_the_current_edge() {
    let api = Arc::new(HistoryApi::default());
    api.latest_mode.store(4, Relaxed);
    let (server, worker) = serve(api.clone()).await;
    let fixture = Fixture::new(|_, _| unreachable!());
    let owner = fixture.login_at(&server, "first");
    let current = fixture.auth.snapshot()[0].generation;
    let sync = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let stream = HistoryScope {
        stream_id: CHAT,
        topic_id: None,
    };
    let topic = HistoryScope {
        topic_id: Some(TOPIC),
        ..stream
    };
    let cursor = Cursor {
        epoch_generation: "test".into(),
        epoch_version: 8,
    };
    sync.apply(
        &owner,
        current,
        "messenger",
        Event::Checkpoint(cursor.clone()),
    )
    .unwrap();

    let original = source
        .load_latest_history(&sync, &owner, current, stream)
        .await
        .unwrap();
    let topic_window = source
        .load_latest_history(&sync, &owner, current, topic)
        .await
        .unwrap();
    let load = |scope, id| source.load_older_history(&sync, &owner, current, scope, id);
    api.older_mode.store(1, Relaxed);
    assert!(load(stream, original.id).await.is_err());
    assert_eq!(
        sync.history_windows(&owner, current, stream).unwrap(),
        std::slice::from_ref(&original)
    );

    api.older_mode.store(3, Relaxed);
    let extended_topic = load(topic, topic_window.id).await.unwrap();
    assert_eq!(extended_topic.id, topic_window.id);
    assert_eq!(extended_topic.older, HistoryEdge::More(ANCHOR.to_string()));
    assert_eq!(extended_topic.newer, topic_window.newer);
    assert_eq!(
        sync.history_windows(&owner, current, stream).unwrap(),
        std::slice::from_ref(&original)
    );

    api.older_mode.store(2, Relaxed);
    let replace_edge = async {
        timeout(Duration::from_secs(3), api.delay.notified())
            .await
            .expect("первая догрузка не дошла до сервера");
        let result = load(stream, original.id).await;
        api.release.notify_one();
        result.unwrap()
    };
    let (late, extended) = tokio::join!(load(stream, original.id), replace_edge);
    assert!(matches!(late, Err(OpenHistoryError::StaleWindow)));
    assert_eq!(extended.id, original.id);
    assert_eq!(extended.older, HistoryEdge::End);
    assert_eq!(extended.newer, original.newer);
    for window in [&extended, &extended_topic] {
        assert_eq!(
            window.messages.iter().map(|row| row.id).collect::<Vec<_>>(),
            [ANCHOR, OLDER, NEWER]
        );
    }
    let count = api.older_pages.load(Relaxed);
    assert_eq!(load(stream, original.id).await.unwrap(), extended);
    assert_eq!(api.older_pages.load(Relaxed), count);
    assert_eq!(sync.cursor(&owner, "messenger").unwrap(), Some(cursor));

    let another = source
        .load_latest_history(&sync, &owner, current, stream)
        .await
        .unwrap();
    let before = sync.history_windows(&owner, current, stream).unwrap();
    api.older_mode.store(2, Relaxed);
    let relogin = async {
        timeout(Duration::from_secs(3), api.delay.notified())
            .await
            .expect("догрузка уходящей сессии не дошла до сервера");
        fixture.auth.logout(&owner.account_id).unwrap();
        let fresh = fixture.login_at(&server, "first");
        api.release.notify_one();
        fresh
    };
    let (late, fresh) = tokio::join!(load(stream, another.id), relogin);
    assert!(matches!(
        late,
        Err(OpenHistoryError::Auth(AuthError::SessionUnavailable))
    ));
    let generation = fixture
        .auth
        .snapshot()
        .into_iter()
        .find(|session| session.context == fresh)
        .unwrap()
        .generation;
    assert_eq!(
        sync.history_windows(&fresh, generation, stream).unwrap(),
        before
    );
    worker.abort();
}

/// Проверяет раздельную выдачу сохранённого хвоста и сетевого обновления после проверки доступа.
#[tokio::test]
async fn opening_latest_history_checks_access_before_cache_and_refreshes_separately() {
    let api = Arc::new(HistoryApi::default());
    let (server, worker) = serve(api.clone()).await;
    let fixture = Fixture::new(|_, _| unreachable!());
    let owner = fixture.login_at(&server, "first");
    let generation = fixture.auth.snapshot()[0].generation;
    let sync = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let direct = HistoryScope {
        stream_id: CHAT,
        topic_id: None,
    };
    let topic = HistoryScope {
        topic_id: Some(TOPIC),
        ..direct
    };
    let cursor = Cursor {
        epoch_generation: "test".into(),
        epoch_version: 8,
    };
    sync.apply(
        &owner,
        generation,
        "messenger",
        Event::Checkpoint(cursor.clone()),
    )
    .unwrap();

    let empty = source
        .open_latest_history(&sync, &owner, generation, direct)
        .await
        .unwrap();
    assert!(empty.cached.is_none());
    let direct_window = empty.refresh().await.unwrap();
    let topic_empty = source
        .open_latest_history(&sync, &owner, generation, topic)
        .await
        .unwrap();
    assert!(topic_empty.cached.is_none());
    let topic_window = topic_empty.refresh().await.unwrap();
    for (scope, expected) in [(direct, &direct_window), (topic, &topic_window)] {
        let opening = source
            .open_latest_history(&sync, &owner, generation, scope)
            .await
            .unwrap();
        assert_eq!(opening.cached.as_ref(), Some(expected));
        api.latest_mode.store(1, Relaxed);
        assert!(opening.refresh().await.is_err());
        assert_eq!(opening.cached.as_ref(), Some(expected));
        api.latest_mode.store(0, Relaxed);
        assert_eq!(opening.refresh().await.unwrap().scope, scope);
    }
    api.deny.store(1, Relaxed);
    assert!(matches!(
        source
            .open_latest_history(&sync, &owner, generation, direct)
            .await,
        Err(OpenHistoryError::AccessLost)
    ));
    api.deny.store(0, Relaxed);
    api.access_mode.store(1, Relaxed);
    assert!(matches!(
        source
            .open_latest_history(&sync, &owner, generation, topic)
            .await,
        Err(OpenHistoryError::AccessLost)
    ));
    api.access_mode.store(0, Relaxed);

    let opening = source
        .open_latest_history(&sync, &owner, generation, direct)
        .await
        .unwrap();
    api.latest_mode.store(2, Relaxed);
    let release = async {
        timeout(Duration::from_secs(3), api.delay.notified())
            .await
            .expect("обновление не дошло до сервера");
        assert_eq!(
            opening.cached.as_ref().unwrap().messages,
            direct_window.messages
        );
        api.release.notify_one();
    };
    let (result, ()) = tokio::join!(opening.refresh(), release);
    assert_eq!(result.unwrap().scope, direct);
    assert_eq!(sync.cursor(&owner, "messenger").unwrap(), Some(cursor));
    api.access_mode.store(4, Relaxed);
    let relogin = async {
        timeout(Duration::from_secs(3), api.delay.notified())
            .await
            .expect("проверка доступа не дошла до сервера");
        fixture.auth.logout(&owner.account_id).unwrap();
        fixture.login_at(&server, "first");
        api.release.notify_one();
    };
    let (late, ()) = tokio::join!(
        source.open_latest_history(&sync, &owner, generation, topic),
        relogin
    );
    assert!(matches!(
        late,
        Err(OpenHistoryError::Auth(AuthError::SessionUnavailable))
    ));
    assert!(matches!(
        opening.refresh().await,
        Err(OpenHistoryError::Auth(AuthError::SessionUnavailable))
    ));
    worker.abort();
}

/// Проверяет ответы для обеих областей и отклоняет ответ после повторного входа.
#[tokio::test]
async fn verifies_conversation_access_for_current_generation() {
    let api = Arc::new(HistoryApi::default());
    let (server, worker) = serve(api.clone()).await;
    let fixture = Fixture::new(|_, _| unreachable!());
    let owner = fixture.login_at(&server, "first");
    let generation = fixture.auth.snapshot()[0].generation;
    let sync = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let direct = HistoryScope {
        stream_id: CHAT,
        topic_id: None,
    };
    let topic = HistoryScope {
        topic_id: Some(TOPIC),
        ..direct
    };
    let cursor = Cursor {
        epoch_generation: "test".into(),
        epoch_version: 8,
    };
    sync.apply(
        &owner,
        generation,
        "messenger",
        Event::Checkpoint(cursor.clone()),
    )
    .unwrap();
    let verify = |scope| source.verify_conversation_access(&sync, &owner, generation, scope);

    assert_eq!(verify(direct).await.unwrap(), direct);
    assert_eq!(verify(topic).await.unwrap(), topic);
    api.deny.store(1, Relaxed);
    assert!(matches!(
        verify(direct).await,
        Err(OpenHistoryError::AccessLost)
    ));
    api.access_mode.store(1, Relaxed);
    assert!(matches!(
        verify(topic).await,
        Err(OpenHistoryError::AccessLost)
    ));
    for mode in [2, 3, 5, 6] {
        api.access_mode.store(mode, Relaxed);
        assert!(
            matches!(verify(topic).await, Err(OpenHistoryError::Source(_))),
            "режим {mode}"
        );
    }
    assert_eq!(sync.cursor(&owner, "messenger").unwrap(), Some(cursor));
    for scope in [direct, topic] {
        assert!(
            sync.history_windows(&owner, generation, scope)
                .unwrap()
                .is_empty()
        );
    }

    api.access_mode.store(4, Relaxed);
    let relogin = async {
        timeout(Duration::from_secs(3), api.delay.notified())
            .await
            .expect("запрос доступа к теме не дошёл до сервера");
        fixture.auth.logout(&owner.account_id).unwrap();
        fixture.login_at(&server, "first");
        api.release.notify_one();
    };
    let (result, ()) = tokio::join!(verify(topic), relogin);
    assert!(matches!(
        result,
        Err(OpenHistoryError::Auth(AuthError::SessionUnavailable))
    ));
    worker.abort();
}

/// Открывает тему двух аккаунтов, проверяет кэш, отсутствие цели и устаревший вход.
#[tokio::test]
async fn topic_anchor_history_checks_scope_and_isolates_sessions() {
    let api = Arc::new(HistoryApi::default());
    api.require_topic.store(1, Relaxed);
    let (server, worker) = serve(api.clone()).await;
    let fixture = Fixture::new(|_, _| unreachable!());
    let first = fixture.login_at(&server, "first");
    let second = fixture.login_at(&server, "second");
    let generation = |owner: &_| {
        fixture
            .auth
            .snapshot()
            .into_iter()
            .find(|session| session.context == *owner)
            .unwrap()
            .generation
    };
    let first_generation = generation(&first);
    let second_generation = generation(&second);
    let sync = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let source =
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap();
    let scope = HistoryScope {
        stream_id: CHAT,
        topic_id: Some(TOPIC),
    };
    let cursor = Cursor {
        epoch_generation: "test".into(),
        epoch_version: 8,
    };
    sync.apply(
        &second,
        second_generation,
        "messenger",
        Event::Checkpoint(cursor.clone()),
    )
    .unwrap();
    let first_window = source
        .open_topic_history(&sync, &first, first_generation, CHAT, TOPIC, ANCHOR)
        .await
        .unwrap();
    let second_window = source
        .open_topic_history(&sync, &second, second_generation, CHAT, TOPIC, ANCHOR)
        .await
        .unwrap();
    assert_eq!(
        second_window
            .messages
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [OLDER, ANCHOR, NEWER]
    );
    assert!(
        first_window
            .messages
            .iter()
            .all(|row| row.body.markdown() == Some("first"))
    );
    assert!(
        second_window
            .messages
            .iter()
            .all(|row| row.body.markdown() == Some("second"))
    );
    assert_eq!(second_window.older, HistoryEdge::More(OLDER.to_string()));
    assert_eq!(second_window.newer, HistoryEdge::End);
    assert_eq!(sync.cursor(&second, "messenger").unwrap(), Some(cursor));
    assert_eq!(
        source
            .open_topic_history(&sync, &second, second_generation, CHAT, TOPIC, ANCHOR)
            .await
            .unwrap(),
        second_window
    );
    assert_eq!(api.pages.load(Relaxed), 4);
    api.access_mode.store(1, Relaxed);
    assert!(matches!(
        source
            .open_topic_history(&sync, &second, second_generation, CHAT, TOPIC, ANCHOR)
            .await,
        Err(OpenHistoryError::AccessLost)
    ));
    api.access_mode.store(0, Relaxed);
    api.deny.store(2, Relaxed);
    assert!(matches!(
        source
            .open_topic_history(&sync, &second, second_generation, CHAT, TOPIC, ANCHOR)
            .await,
        Err(OpenHistoryError::MessageMissing)
    ));
    api.deny.store(0, Relaxed);
    api.anchor_other_topic.store(1, Relaxed);
    assert!(matches!(
        source
            .open_topic_history(&sync, &second, second_generation, CHAT, TOPIC, ANCHOR)
            .await,
        Err(OpenHistoryError::MessageMissing)
    ));
    api.anchor_other_topic.store(0, Relaxed);
    let empty = Sync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let pending =
        source.open_topic_history(&empty, &second, second_generation, CHAT, TOPIC, ANCHOR);
    let relogin = async {
        timeout(Duration::from_secs(3), api.delay.notified())
            .await
            .expect("страница темы B не запрошена");
        fixture.auth.logout(&second.account_id).unwrap();
        let fresh = fixture.login_at(&server, "second");
        api.release.notify_one();
        fresh
    };
    let (result, fresh) = tokio::join!(pending, relogin);
    assert!(matches!(
        result,
        Err(OpenHistoryError::Auth(AuthError::SessionUnavailable))
    ));
    assert!(
        empty
            .history_windows(&fresh, generation(&fresh), scope)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        sync.history_windows(&first, first_generation, scope)
            .unwrap(),
        [first_window]
    );
    worker.abort();
}
