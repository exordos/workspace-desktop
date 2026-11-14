//! Проверяет открытие истории по уведомлению и загрузку последней страницы через HTTPS.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering::Relaxed},
};
use std::time::Duration;

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
use uuid::Uuid;
use workspace_core::{
    auth::AuthError,
    messenger::{MessengerSource, history::OpenHistoryError},
    notification_target::NotificationTarget,
    storage::{Cursor, HistoryEdge, HistoryScope, Storage},
    sync::{Event, Sync},
};

use super::Fixture;

const CHAT: Uuid = Uuid::from_u128(10);
const TOPIC: Uuid = Uuid::from_u128(11);
const OLDER: Uuid = Uuid::from_u128(12);
const ANCHOR: Uuid = Uuid::from_u128(13);
const NEWER: Uuid = Uuid::from_u128(14);
const CA: &[u8] =
    include_bytes!("../../../workspace-desktop/src/shell/background_message/tls/ca.pem");
const CERT: &[u8] =
    include_bytes!("../../../workspace-desktop/src/shell/background_message/tls/localhost.pem");
const KEY: &[u8] =
    include_bytes!("../../../workspace-desktop/src/shell/background_message/tls/localhost-key.pem");

/// Даёт тесту управлять ответами и задержать одну страницу до выхода из аккаунта.
#[derive(Default)]
struct HistoryApi {
    pages: AtomicUsize,       // Количество запросов соседних страниц.
    deny: AtomicUsize,        // 0 — доступ есть, 1 — скрыт чат, 2 — скрыто сообщение.
    delay: Notify,            // Запрос страницы дошёл до сервера.
    latest_mode: AtomicUsize, // 0 — обычная страница, 1 — сбой, 2 — задержка, 3 — чужая тема.
    release: Notify,          // Можно вернуть задержанный ответ.
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

/// Принимает один HTTPS-запрос и возвращает данные только для токена B.
async fn reply(socket: TcpStream, tls: TlsAcceptor, api: Arc<HistoryApi>) {
    let mut socket = BufReader::new(tls.accept(socket).await.unwrap());
    let mut line = String::new();
    socket.read_line(&mut line).await.unwrap();
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
    let user = if headers.contains("authorization: Bearer second") {
        "second"
    } else {
        assert!(headers.contains("authorization: Bearer first"));
        "first"
    };
    let mut marker = String::new();
    let (status, body) = if request.path().ends_with(&format!("/streams/{CHAT}")) {
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
    } else if request.path().ends_with(&format!("/messages/{ANCHOR}")) {
        if api.deny.load(Relaxed) == 2 {
            ("404 Not Found", Value::Null)
        } else {
            ("200 OK", message(ANCHOR, user))
        }
    } else {
        assert!(request.path().ends_with("/messages/"));
        let query = request
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query["stream_uuid"], CHAT.to_string());
        assert_eq!(query["page_limit"], "50");
        assert_eq!(query["sort_key"], "created_at");
        if let Some(anchor) = query.get("page_marker") {
            assert_eq!(anchor, &ANCHOR.to_string());
            assert!(!query.contains_key("topic_uuid"));
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
                assert_eq!(topic, &TOPIC.to_string());
            } else {
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
    assert!(first_window.messages.iter().all(|row| row.text == "first"));
    assert!(
        second_window
            .messages
            .iter()
            .all(|row| row.text == "second")
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
