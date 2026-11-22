//! Проверяет отзыв канала через Sync, SQLite и подписку каталога с задержанным HTTPS-ответом.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use rustls::{
    ServerConfig,
    crypto::ring,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::{Notify, oneshot},
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use uuid::Uuid;
use workspace_core::{
    auth::{Auth, IamClient, IamError, IamProject, IamToken},
    bootstrap::{self, ChatListStatus},
    messenger::MessengerSource,
    runtime::Runtime,
    storage::{ApplyOutcome, Cursor, HistoryChange, MessagePlacement, Storage},
    sync::{Event, Sync},
};

use super::MemoryStores;

pub(super) const PROJECT: &str = "00000000-0000-0000-0000-000000000001";
pub(super) const A: &str = "00000000-0000-0000-0000-000000000002";
const B: &str = "00000000-0000-0000-0000-000000000003";
const STREAM: &str = "00000000-0000-0000-0000-000000000004";
const OTHER: &str = "00000000-0000-0000-0000-000000000005";
const TOPIC: &str = "00000000-0000-0000-0000-000000000006";
const MESSAGE: &str = "00000000-0000-0000-0000-000000000007";
const CA: &[u8] = include_bytes!("../../../../tests/fixtures/tls/ca.pem");
const CERT: &[u8] = include_bytes!("../../../../tests/fixtures/tls/localhost.pem");
const KEY: &[u8] = include_bytes!("../../../../tests/fixtures/tls/localhost-key.pem");
const DEADLINE: Duration = Duration::from_secs(10);

/// Выдаёт двум тестовым пользователям токены с UUID проекта.
pub(super) struct TestIam;

/// Создаёт токен для заданного пользователя.
fn token(user: &str, selected: bool) -> IamToken {
    IamToken {
        access_token: user.into(),
        refresh_token: Some(user.into()),
        expires_in: Duration::from_secs(600),
        user_id: user.into(),
        project_id: selected.then(|| PROJECT.into()),
        organization_id: selected.then(|| "org".into()),
    }
}

impl IamClient for TestIam {
    /// Оставляет адрес локального сервера без изменений.
    fn normalize_server(&self, server: &str) -> Result<String, IamError> {
        Ok(server.into())
    }
    /// Принимает UUID пользователя как тестовый логин.
    fn login(&self, _: &str, login: &str, _: &str, _: Option<&str>) -> Result<IamToken, IamError> {
        Ok(token(login, false))
    }
    /// Возвращает единственный тестовый проект.
    fn projects(&self, _: &str, _: &str) -> Result<Vec<IamProject>, IamError> {
        Ok(vec![IamProject {
            id: PROJECT.into(),
            name: "test".into(),
            organization_id: Some("org".into()),
        }])
    }
    /// Привязывает токен к проекту.
    fn select_project(&self, _: &str, user: &str, _: &str) -> Result<IamToken, IamError> {
        Ok(token(user, true))
    }
    /// Восстанавливает тот же токен после перезапуска.
    fn refresh(&self, _: &str, user: &str) -> Result<IamToken, IamError> {
        Ok(token(user, true))
    }
}

/// Барьеры задерживают старый ответ B до фиксации отзыва.
#[derive(Default)]
struct Api {
    delayed: Notify,           // Сервер получил старый запрос B.
    release: Notify,           // Тест разрешает вернуть его после отзыва.
    first_b: AtomicBool,       // Задерживаем только первый запрос списка каналов B.
    revoked: AtomicBool,       // Новые ответы B уже учитывают отзыв.
    preview_requested: Notify, // Сервер получил запрос превью для уже готового списка.
    preview_release: Notify,   // Разрешает вернуть превью после проверки готового списка.
}

/// Запускает локальный HTTPS для списка каналов и тем.
async fn server(api: Arc<Api>) -> (String, tokio::task::JoinHandle<()>) {
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
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let tls = tls.clone();
            let api = api.clone();
            tokio::spawn(async move {
                let mut socket = BufReader::new(tls.accept(socket).await.unwrap());
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                let mut headers = String::new();
                loop {
                    line.clear();
                    socket.read_line(&mut line).await.unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    headers.push_str(&line);
                }
                let user = if headers.contains(B) {
                    B
                } else {
                    assert!(headers.contains(A));
                    A
                };
                let old_request = user == B
                    && path.contains("/streams/")
                    && !api.first_b.swap(true, Ordering::SeqCst);
                if old_request {
                    api.delayed.notify_one();
                    api.release.notified().await;
                }
                let body =
                    if path.contains("/epoch/") {
                        json!({"epoch_generation": "epoch", "epoch_version": 0})
                    } else if path.contains("/messages/") {
                        assert_eq!(user, A);
                        assert!(path.contains(&format!("project_id={PROJECT}")));
                        assert!(path.contains(&format!("uuid={MESSAGE}")));
                        api.preview_requested.notify_one();
                        api.preview_release.notified().await;
                        json!([{
                            "uuid": MESSAGE, "project_id": PROJECT, "user_uuid": A,
                            "stream_uuid": STREAM, "topic_uuid": TOPIC, "author_uuid": A,
                            "created_at": "2026-11-17T12:13:14Z",
                            "payload": {
                                "kind": "markdown",
                                "content": "**hello**\n[world](https://example.invalid/a(b)c)"
                            }
                        }, {
                            "uuid": MESSAGE, "project_id": PROJECT, "user_uuid": B,
                            "stream_uuid": STREAM, "topic_uuid": TOPIC, "author_uuid": B,
                            "created_at": "2026-11-17T12:13:14Z",
                            "payload": { "kind": "markdown", "content": "other account" }
                        }])
                    } else if path.contains("/stream_topics/") {
                        json!([{
                            "uuid": TOPIC, "project_id": PROJECT, "user_uuid": user,
                            "stream_uuid": STREAM, "name": "topic", "is_done": true,
                            "last_message_uuid": MESSAGE, "unread_count": 0, "color": 0xAA_BBCCu32
                        }])
                    } else {
                        let streams =
                            if user == B && !old_request && api.revoked.load(Ordering::SeqCst) {
                                vec![OTHER]
                            } else {
                                vec![STREAM, OTHER]
                            };
                        json!(streams.into_iter().map(|id| json!({
                        "uuid": id, "project_id": PROJECT, "user_uuid": user, "name": id,
                        "private": false, "is_archived": false, "default_topic_uuid": TOPIC,
                        "color": 0xBB_CCDDu32, "active_unread_count": 2
                    })).collect::<Vec<_>>())
                    }
                    .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            });
        }
    });
    (address, task)
}

/// Создаёт событие отзыва с заданной позицией журнала.
fn revoke(version: u64) -> Event {
    Event::HistoryChanged {
        cursor: Cursor {
            epoch_generation: "epoch".into(),
            epoch_version: version,
        },
        change: HistoryChange::RemoveStream(STREAM.into()),
    }
}

/// Проверяет изоляцию аккаунтов, откат ошибки и запоздавший ответ после отзыва.
#[tokio::test]
async fn revoke_stream_updates_catalog_and_rejects_old_response() {
    let api = Arc::new(Api::default());
    let (server_url, server_task) = server(api.clone()).await;
    let stores = Arc::new(MemoryStores::default());
    let auth = Auth::new(Arc::new(TestIam), stores.clone(), stores);
    let login = |user| {
        let attempt = auth
            .begin_login(&server_url, user, "password", None)
            .unwrap();
        auth.complete_login(attempt.id, PROJECT).unwrap()
    };
    let a = login(A);
    let b = login(B);
    let generation = auth
        .snapshot()
        .into_iter()
        .find(|s| s.context == b)
        .unwrap()
        .generation;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog.sqlite");
    let storage = Arc::new(Storage::open(&path).unwrap());
    let db = rusqlite::Connection::open(&path).unwrap();
    for owner in [&a, &b] {
        let snapshot = json!([{ "id": STREAM, "name": "revoked", "direct": false, "topics": [{"id": TOPIC, "name": "topic"}] }, { "id": OTHER, "name": "other", "direct": false, "topics": [] }]).to_string();
        db.execute(
            "INSERT INTO messenger_catalog VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                snapshot
            ],
        )
        .unwrap();
    }
    let ca = CertificateDer::from_pem_slice(CA).unwrap();
    let messenger = MessengerSource::with_private_ca(ca.as_ref()).unwrap();
    let (catalog, startup) = bootstrap::chat_catalog(messenger.catalog(storage.clone()));
    let mut lists = catalog.subscribe();
    let sync = Arc::new(Sync::new(auth.clone(), storage.clone()));
    let runtime = Runtime::new(sync.clone(), vec![]).unwrap();
    let (stop, stopped) = oneshot::channel();
    let worker = tokio::spawn(bootstrap::run(
        runtime,
        Some(startup),
        async {
            let _ = stopped.await;
        },
        |result| {
            result.map_err(|_| {
                workspace_core::runtime::RuntimeError::DuplicateSource("restore".into())
            })
        },
    ));
    timeout(DEADLINE, api.delayed.notified())
        .await
        .expect("запрос B не дошёл до сервера");
    timeout(DEADLINE, async {
        loop {
            let ready = {
                let lists = lists.borrow();
                lists
                    .get(&a)
                    .is_some_and(|l| l.status == ChatListStatus::Ready)
                    && lists
                        .get(&b)
                        .is_some_and(|l| l.chats.as_ref().is_some_and(|c| c.len() == 2))
            };
            if ready {
                break;
            }
            lists.changed().await.unwrap();
        }
    })
    .await
    .expect("исходные каталоги не появились");
    {
        let lists = lists.borrow();
        let fresh = lists[&a].chats.as_ref().unwrap();
        let stream = fresh
            .iter()
            .find(|chat| chat.id.to_string() == STREAM)
            .unwrap();
        let topic = &stream.topics[0];
        assert_eq!(stream.metadata.default_topic_id, Some(topic.id));
        assert_eq!(stream.metadata.color, Some(0xBB_CCDD));
        assert_eq!(stream.metadata.active_unread_count, Some(2));
        assert_eq!(
            topic.metadata.last_message_id,
            Some(MESSAGE.parse().unwrap())
        );
        assert!(topic.metadata.is_done && topic.metadata.is_default);
        assert_eq!(topic.metadata.unread_count, Some(0));
        assert_eq!(topic.metadata.color, Some(0xAA_BBCC));
        assert!(topic.metadata.preview.is_none());
        // Сервер сослался на тему чужого канала. Она не становится целью перехода.
        let other = fresh
            .iter()
            .find(|chat| chat.id.to_string() == OTHER)
            .unwrap();
        assert_eq!(other.metadata.default_topic_id, None);
        // Старый снимок B читается, пока свежий ответ ещё в пути.
        let cached = lists[&b].chats.as_ref().unwrap();
        assert!(
            cached
                .iter()
                .all(|chat| chat.metadata.active_unread_count.is_none())
        );
    }
    timeout(DEADLINE, api.preview_requested.notified())
        .await
        .expect("запрос превью не дошёл до сервера");
    api.preview_release.notify_one();
    timeout(DEADLINE, async {
        loop {
            {
                let lists = lists.borrow();
                let preview = lists[&a]
                    .chats
                    .as_ref()
                    .unwrap()
                    .iter()
                    .find_map(|chat| chat.topics.first()?.metadata.preview.as_ref());
                if let Some(preview) = preview {
                    assert_eq!(preview.text, "hello world");
                    assert_eq!(preview.author_name.as_deref(), Some("Вы"));
                    break;
                }
            }
            lists.changed().await.unwrap();
        }
    })
    .await
    .expect("превью не появилось после готового каталога");
    let generation_a = lists.borrow()[&a].generation;
    let apply_a = |version, change| {
        sync.apply(
            &a,
            generation_a,
            "messenger",
            Event::HistoryChanged {
                cursor: Cursor {
                    epoch_generation: "epoch".into(),
                    epoch_version: version,
                },
                change,
            },
        )
        .unwrap();
    };
    apply_a(1, HistoryChange::RemoveMessage(MESSAGE.into()));
    timeout(DEADLINE, async {
        loop {
            let hidden = lists.borrow()[&a]
                .chats
                .as_ref()
                .unwrap()
                .iter()
                .filter_map(|chat| chat.topics.first())
                .any(|topic| {
                    topic.metadata.last_message_id.is_none() && topic.metadata.preview.is_none()
                });
            if hidden {
                break;
            }
            lists.changed().await.unwrap();
        }
    })
    .await
    .expect("превью удалённого сообщения осталось в каталоге");
    apply_a(
        2,
        HistoryChange::RemoveTopic(MessagePlacement {
            stream_id: STREAM.into(),
            topic_id: TOPIC.into(),
        }),
    );
    timeout(DEADLINE, async {
        loop {
            let hidden = lists.borrow()[&a]
                .chats
                .as_ref()
                .unwrap()
                .iter()
                .all(|chat| chat.topics.is_empty() && chat.metadata.default_topic_id.is_none());
            if hidden {
                break;
            }
            lists.changed().await.unwrap();
        }
    })
    .await
    .expect("удалённая тема осталась в каталоге");
    db.execute_batch("CREATE TRIGGER fail_revoke BEFORE UPDATE ON messenger_catalog BEGIN SELECT RAISE(FAIL, 'test failure'); END;").unwrap();
    lists.borrow_and_update();
    assert!(sync.apply(&b, generation, "messenger", revoke(1)).is_err());
    assert!(sync.cursor(&b, "messenger").unwrap().is_none());
    assert_eq!(lists.borrow()[&b].chats.as_ref().unwrap().len(), 2);
    assert!(!lists.has_changed().unwrap());
    db.execute_batch("DROP TRIGGER fail_revoke").unwrap();
    assert_eq!(
        sync.apply(&b, generation, "messenger", revoke(1)).unwrap(),
        ApplyOutcome::Applied
    );
    timeout(DEADLINE, async {
        loop {
            let filtered = {
                let lists = lists.borrow();
                lists
                    .get(&b)
                    .and_then(|l| l.chats.as_ref())
                    .is_some_and(|chats| {
                        chats.len() == 1 && chats[0].id == Uuid::parse_str(OTHER).unwrap()
                    })
            };
            if filtered {
                break;
            }
            lists.changed().await.unwrap();
        }
    })
    .await
    .expect("отзыв не дошёл до подписки");
    assert_eq!(lists.borrow()[&a].chats.as_ref().unwrap().len(), 2);
    assert_eq!(
        sync.apply(&b, generation, "messenger", revoke(1)).unwrap(),
        ApplyOutcome::Duplicate
    );
    api.revoked.store(true, Ordering::SeqCst);
    api.release.notify_one();
    timeout(DEADLINE, async {
        loop {
            if lists.borrow()[&b].status == ChatListStatus::Failed {
                break;
            }
            lists.changed().await.unwrap();
        }
    })
    .await
    .expect("устаревший ответ не завершился");
    assert_eq!(lists.borrow()[&b].chats.as_ref().unwrap().len(), 1);
    let saved: String = db
        .query_row(
            "SELECT snapshot FROM messenger_catalog WHERE account_id = ?1",
            [&b.account_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!saved.contains(STREAM));
    assert!(saved.contains(OTHER));
    assert!(catalog.retry(b.clone(), generation));
    timeout(DEADLINE, async {
        loop {
            let ready = {
                let lists = lists.borrow();
                lists.get(&b).is_some_and(|list| {
                    list.status == ChatListStatus::Ready
                        && list.chats.as_ref().is_some_and(|chats| chats.len() == 1)
                })
            };
            if ready {
                break;
            }
            lists.changed().await.unwrap();
        }
    })
    .await
    .expect("повторная загрузка каталога не завершилась");
    assert_eq!(lists.borrow()[&a].chats.as_ref().unwrap().len(), 2);
    stop.send(()).unwrap();
    timeout(DEADLINE, worker).await.unwrap().unwrap().unwrap();
    drop(db);
    drop(sync);
    drop(storage);
    let reopened = rusqlite::Connection::open(&path).unwrap();
    let saved: String = reopened
        .query_row(
            "SELECT snapshot FROM messenger_catalog WHERE account_id = ?1",
            [&b.account_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!saved.contains(STREAM));
    let saved_a: String = reopened
        .query_row(
            "SELECT snapshot FROM messenger_catalog WHERE account_id = ?1",
            [&a.account_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert!(saved_a.contains(STREAM));
    assert!(!saved_a.contains(TOPIC));
    assert!(!saved_a.contains(MESSAGE));
    server_task.abort();
}
