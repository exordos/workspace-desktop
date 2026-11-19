//! Проверяет работу кэша вложений через открытие истории, файловый API и журнал мессенджера.
//! Сначала описывает локальный сервер, затем проверяет чтение, очистку и отзыв доступа.

use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use reqwest::Url;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, sync::Notify, time::timeout};
use tokio_tungstenite::tungstenite::Message as Frame;
use uuid::Uuid;
use workspace_core::{
    auth::Auth,
    context::{OwnerContext, SessionGeneration},
    messenger::{MessengerSource, SOURCE_ID, history::LatestHistoryOpening},
    notification_target::NotificationTarget,
    runtime::{EventSource, SourceSession},
    storage::{
        Cursor, HistoryChange, HistoryScope, Message, MessagePlacement, Storage,
        media::{MediaCachePolicy, MediaFilter},
    },
    sync::{Event, Sync, SyncChange, SyncUpdate},
};

use super::{
    ANCHOR, CA, CHAT, Fixture, HistoryApi, HttpsSocket, NEWER, OLDER, OTHER_TOPIC, TOPIC, serve,
    upgrade,
};

const FILE: Uuid = Uuid::from_u128(90);
const FIRST_BYTES: &[u8] = b"attachment visible to first account";
const SECOND_BYTES: &[u8] = b"response for the other account";
const DEADLINE: Duration = Duration::from_secs(10);

/// Считает запросы и позволяет задержать ответ для проверки гонок.
#[derive(Default)]
pub(super) struct Api {
    requests: AtomicUsize,        // Все HTTPS-запросы к истории, файлам и журналу.
    downloads: AtomicUsize,       // Только запросы байтов вложения.
    hold_next: AtomicBool,        // Задержать следующий запрос файла до сигнала.
    download_status: AtomicUsize, // Подменённый статус файлового API, 0 означает полный ответ.
    anchor_changed: AtomicBool,   // Сервер уже убрал вложение из целевого сообщения.
    received: Notify,             // Сервер получил задержанный запрос.
    release: Notify,              // Тест разрешил вернуть задержанный ответ.
}

impl Api {
    /// Возвращает тему, страницу истории, байты вложения или событие удаления.
    pub(super) async fn reply(
        &self,
        mut socket: HttpsSocket,
        request: Url,
        method: &str,
        headers: &str,
    ) {
        assert_eq!(method, "GET");
        self.requests.fetch_add(1, Relaxed);
        let path = request.path();
        if path.ends_with("/events/ws") {
            assert!(headers.contains("bearer.first"));
            let mut peer = upgrade(socket, headers).await;
            let event = json!({
                "schema_version": 1, "uuid": "delete-file-event",
                "epoch_generation": "test", "epoch_version": 2,
                "project_id": "project", "user_uuid": "first",
                "object_type": "file", "action": "deleted",
                "payload": {"kind": "file.deleted", "uuid": FILE}
            });
            peer.send(Frame::text(event.to_string())).await.unwrap();
            return;
        }

        let user = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .and_then(|(_, value)| value.trim().strip_prefix("Bearer "))
            .unwrap();
        assert!(["first", "second"].contains(&user));
        let (status, mime, body) = if path.ends_with("/events/") {
            ("200 OK", "application/json", b"[]".to_vec())
        } else if path.ends_with(&format!("/files/{FILE}/actions/download")) {
            self.downloads.fetch_add(1, Relaxed);
            if self.hold_next.swap(false, Relaxed) {
                self.received.notify_one();
                self.release.notified().await;
            }
            match self.download_status.load(Relaxed) {
                204 => ("204 No Content", "text/plain", Vec::new()),
                206 => ("206 Partial Content", "text/plain", b"partial".to_vec()),
                403 => ("403 Forbidden", "text/plain", Vec::new()),
                404 => ("404 Not Found", "text/plain", Vec::new()),
                _ => {
                    let bytes = if user == "first" {
                        FIRST_BYTES
                    } else {
                        SECOND_BYTES
                    };
                    ("200 OK", "text/plain", bytes.to_vec())
                }
            }
        } else if let Some(topic) = path.rsplit_once("/stream_topics/").map(|(_, topic)| topic) {
            let topic: Uuid = topic.parse().unwrap();
            assert!([TOPIC, OTHER_TOPIC].contains(&topic));
            json_response(json!({
                "uuid": topic, "stream_uuid": CHAT, "user_uuid": user,
                "project_id": "project", "notification_mode": "default"
            }))
        } else if path.ends_with(&format!("/streams/{CHAT}")) {
            json_response(json!({
                "uuid": CHAT, "user_uuid": user, "project_id": "project", "private": true,
                "direct_user_uuid": ANCHOR, "is_archived": false, "notification_mode": "all_messages"
            }))
        } else if path.ends_with(&format!("/messages/{NEWER}")) {
            let content = if self.anchor_changed.load(Relaxed) {
                "Вложение убрано на сервере".into()
            } else {
                format!("[вложение](urn:file:{FILE})")
            };
            json_response(history_message(NEWER, TOPIC, user, content))
        } else {
            assert!(
                path.ends_with("/messages/"),
                "лишний сетевой запрос: {path}"
            );
            let topic: Uuid = request
                .query_pairs()
                .find(|(key, _)| key == "topic_uuid")
                .map_or(TOPIC, |(_, value)| value.parse().unwrap());
            let (linked, plain) = if topic == TOPIC {
                (NEWER, OLDER)
            } else {
                assert_eq!(topic, OTHER_TOPIC);
                (ANCHOR, Uuid::from_u128(8))
            };
            json_response(json!([
                history_message(linked, topic, user, format!("[вложение](urn:file:{FILE})")),
                history_message(plain, topic, user, "сообщение без вложений".into())
            ]))
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let socket = socket.get_mut();
        // Отмена загрузки вправе закрыть сокет до отправки ответа.
        if socket.write_all(response.as_bytes()).await.is_ok() {
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    }
}

/// Формирует сообщение в области запроса с таким же UUID файла для двух владельцев.
fn history_message(id: Uuid, topic: Uuid, user: &str, content: String) -> Value {
    json!({
        "uuid": id, "stream_uuid": CHAT, "topic_uuid": topic,
        "project_id": "project", "user_uuid": user, "author_uuid": ANCHOR,
        "created_at": "2026-01-01T12:00:00Z",
        "payload": {"kind": "markdown", "content": content}
    })
}

/// Превращает JSON в успешный HTTP-ответ тестового сервера.
fn json_response(value: Value) -> (&'static str, &'static str, Vec<u8>) {
    ("200 OK", "application/json", value.to_string().into_bytes())
}

/// Запускает имеющийся HTTPS/WSS-сервер с ответами для сценариев кэша.
async fn server(api: Arc<Api>) -> (String, tokio::task::JoinHandle<()>) {
    serve(Arc::new(HistoryApi {
        media: Some(api),
        ..HistoryApi::default()
    }))
    .await
}

/// Создаёт штатный клиент с тестовым центром сертификации.
fn source() -> MessengerSource {
    let ca = CertificateDer::from_pem_slice(CA).unwrap();
    MessengerSource::with_private_ca(ca.as_ref()).unwrap()
}

/// Открывает один и тот же профиль для нового экземпляра синхронизации.
fn sync_for(auth: &Auth, profile: &tempfile::TempDir) -> Sync {
    Sync::with_media_cache(
        auth.clone(),
        Arc::new(Storage::open(profile.path().join("workspace.sqlite")).unwrap()),
        profile.path().join("media"),
        MediaCachePolicy {
            budget_bytes: 1024 * 1024,
            max_object_bytes: 1024,
        },
    )
    .unwrap()
}

/// Возвращает поколение входа указанного владельца.
fn generation(auth: &Auth, owner: &OwnerContext) -> SessionGeneration {
    auth.snapshot()
        .into_iter()
        .find(|session| &session.context == owner)
        .unwrap()
        .generation
}

/// Адрес одного из двух разговоров, содержащих ссылку на тот же файл.
fn scope(topic: Uuid) -> HistoryScope {
    HistoryScope {
        stream_id: CHAT,
        topic_id: Some(topic),
    }
}

/// Открывает разговор штатным входом и загружает сообщения с сервера.
async fn load_history<'a>(
    client: &'a MessengerSource,
    sync: &'a Sync,
    owner: &'a OwnerContext,
    generation: SessionGeneration,
    topic: Uuid,
) -> LatestHistoryOpening<'a> {
    let history = client
        .open_latest_history(sync, owner, generation, scope(topic))
        .await
        .unwrap();
    history.refresh().await.unwrap();
    history
}

/// Читает байты из управляемой копии до конца.
fn bytes(mut file: impl Read) -> Vec<u8> {
    let mut content = Vec::new();
    file.read_to_end(&mut content).unwrap();
    content
}

/// Один промах даёт один прямой GET, а после перезапуска файл читается локально только своим владельцем.
#[tokio::test]
async fn media_cache_coalesces_download_and_reuses_local_bytes_after_restart() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture =
            Fixture::new(|user, _| Ok(super::super::project_token(user, user, Some(user))));
        let first = fixture.login_at(&url, "first");
        let second = fixture.login_at(&url, "second");
        let profile = tempfile::tempdir().unwrap();

        {
            let sync = sync_for(&fixture.auth, &profile);
            let client = source();
            let opening = load_history(
                &client,
                &sync,
                &first,
                generation(&fixture.auth, &first),
                TOPIC,
            )
            .await;
            let requests = api.requests.load(Relaxed);
            assert!(opening.open_media(OLDER, FILE).await.is_err());
            assert_eq!(
                api.requests.load(Relaxed),
                requests,
                "нет ссылки на запрошенный файл"
            );

            api.hold_next.store(true, Relaxed);
            let (one, two, ()) = tokio::join!(
                opening.open_media(NEWER, FILE),
                opening.open_media(NEWER, FILE),
                async {
                    api.received.notified().await;
                    api.release.notify_one();
                }
            );
            assert_eq!(bytes(one.unwrap()), FIRST_BYTES);
            assert_eq!(bytes(two.unwrap()), FIRST_BYTES);
            assert_eq!(api.downloads.load(Relaxed), 1);
            assert_eq!(
                api.requests.load(Relaxed),
                requests + 1,
                "единственный запрос — GET файла"
            );
        }

        let auth = fixture.restart();
        auth.restore().await.unwrap();
        let sync = sync_for(&auth, &profile);
        let client = source();
        let reopened = client
            .open_latest_history(&sync, &first, generation(&auth, &first), scope(TOPIC))
            .await
            .unwrap();
        assert!(reopened.cached.is_some());
        let requests = api.requests.load(Relaxed);
        assert_eq!(
            bytes(reopened.open_media(NEWER, FILE).await.unwrap()),
            FIRST_BYTES
        );
        assert_eq!(
            api.requests.load(Relaxed),
            requests,
            "попадание читает только диск"
        );

        let other = load_history(&client, &sync, &second, generation(&auth, &second), TOPIC).await;
        let requests = api.requests.load(Relaxed);
        assert_eq!(
            bytes(other.open_media(NEWER, FILE).await.unwrap()),
            SECOND_BYTES
        );
        assert_eq!(api.requests.load(Relaxed), requests + 1);
        assert_eq!(api.downloads.load(Relaxed), 2);
        server.abort();
    })
    .await
    .expect("не завершилась загрузка или повторное открытие кэша");
}

/// Отказ или неполный ответ не создаёт копию, а права проверяются в файловом API.
#[tokio::test]
async fn media_cache_checks_backend_denial_only_on_direct_download() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let client = source();
        let opening = load_history(
            &client,
            &sync,
            &owner,
            generation(&fixture.auth, &owner),
            TOPIC,
        )
        .await;
        let requests = api.requests.load(Relaxed);

        let rejected = [403, 404, 206, 204];
        for (number, code) in rejected.into_iter().enumerate() {
            api.download_status.store(code, Relaxed);
            assert!(
                opening.open_media(NEWER, FILE).await.is_err(),
                "файловый API вернул {code}"
            );
            assert_eq!(api.downloads.load(Relaxed), number + 1);
            assert_eq!(api.requests.load(Relaxed), requests + number + 1);
        }
        api.download_status.store(0, Relaxed);
        assert_eq!(
            bytes(opening.open_media(NEWER, FILE).await.unwrap()),
            FIRST_BYTES
        );
        assert_eq!(api.downloads.load(Relaxed), rejected.len() + 1);
        assert_eq!(api.requests.load(Relaxed), requests + rejected.len() + 1);
        assert_eq!(
            bytes(opening.open_media(NEWER, FILE).await.unwrap()),
            FIRST_BYTES
        );
        assert_eq!(api.requests.load(Relaxed), requests + rejected.len() + 1);
        server.abort();
    })
    .await
    .expect("не завершилась проверка отказов файлового API");
}

/// Поздний ответ после ручной очистки не обновляет кэш, а WS-удаление отзывает доступ к уже открытой копии.
#[tokio::test]
async fn media_cache_fences_late_download_and_open_reader_after_invalidation() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let client = source();
        let opening = load_history(&client, &sync, &owner, generation, TOPIC).await;

        api.hold_next.store(true, Relaxed);
        let (old, ()) = tokio::join!(opening.open_media(NEWER, FILE), async {
            api.received.notified().await;
            let cleared = sync
                .clear_media(&owner, generation, &MediaFilter::File(FILE))
                .unwrap();
            assert_eq!(cleared.freed_bytes, 0);
            api.release.notify_one();
        });
        assert!(
            old.is_err(),
            "старый ответ не должен восстановить очищенную копию"
        );
        let mut reloaded = opening.open_media(NEWER, FILE).await.unwrap();
        assert_eq!(
            api.downloads.load(Relaxed),
            2,
            "после очистки нужен новый GET"
        );

        let baseline = Cursor {
            epoch_generation: "test".into(),
            epoch_version: 1,
        };
        sync.apply(
            &owner,
            generation,
            SOURCE_ID,
            Event::Checkpoint(baseline.clone()),
        )
        .unwrap();
        let mut events = client
            .connect(SourceSession {
                context: owner.clone(),
                generation,
                token_version: 1,
                cursor: Some(baseline),
                access_token: "first".into(),
            })
            .await
            .unwrap();
        let deleted = events.next().await.unwrap().unwrap();
        assert!(matches!(&deleted, Event::FileDeleted { file_id, .. } if *file_id == FILE));
        sync.apply(&owner, generation, SOURCE_ID, deleted).unwrap();
        assert!(
            reloaded.read(&mut [0]).is_err(),
            "открытое чтение не переживает отзыв"
        );
        api.download_status.store(404, Relaxed);
        assert!(
            opening.open_media(NEWER, FILE).await.is_err(),
            "отозванные байты не выдаются"
        );
        server.abort();
    })
    .await
    .expect("не завершилась гонка загрузки, очистки или отзыва");
}

/// Правка уже сверенного сообщения блокирует старую ссылку до повторной сверки истории.
#[tokio::test]
async fn media_cache_rejects_stale_snapshot_link_after_message_edit() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let client = source();
        let opening = load_history(&client, &sync, &owner, generation, TOPIC).await;
        let mut opened = opening.open_media(NEWER, FILE).await.unwrap();
        sync.apply(
            &owner,
            generation,
            SOURCE_ID,
            Event::HistoryChanged {
                cursor: Cursor {
                    epoch_generation: "test".into(),
                    epoch_version: 1,
                },
                change: HistoryChange::Edit {
                    message: Message {
                        id: NEWER.to_string(),
                        text: "вложение убрано".into(),
                    },
                    placement: None,
                },
            },
        )
        .unwrap();
        let requests = api.requests.load(Relaxed);
        assert!(
            opened.read(&mut [0]).is_err(),
            "источник открытой копии больше не подтверждён"
        );
        assert!(opening.open_media(NEWER, FILE).await.is_err());
        assert_eq!(api.requests.load(Relaxed), requests);

        let reopened = client
            .open_latest_history(&sync, &owner, generation, scope(TOPIC))
            .await
            .unwrap();
        let requests = api.requests.load(Relaxed);
        assert!(reopened.open_media(NEWER, FILE).await.is_err());
        assert_eq!(
            api.requests.load(Relaxed),
            requests,
            "повторное открытие не восстанавливает старую ссылку"
        );
        server.abort();
    })
    .await
    .expect("не завершилась проверка правки и старой копии");
}

/// Очистка темы сохраняет файл для соседней темы, а удаление файла убирает байты, сохраняя историю.
#[tokio::test]
async fn media_cache_scope_cleanup_keeps_shared_copy_until_explicit_clear() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let client = source();
        let first = load_history(&client, &sync, &owner, generation, TOPIC).await;
        let other = load_history(&client, &sync, &owner, generation, OTHER_TOPIC).await;

        assert_eq!(
            bytes(first.open_media(NEWER, FILE).await.unwrap()),
            FIRST_BYTES
        );
        let requests = api.requests.load(Relaxed);
        let page = first.list_media(true, None, None, 20).unwrap();
        let item = page
            .attachments
            .iter()
            .find(|item| item.message_id == ANCHOR)
            .unwrap();
        assert!(item.cached);
        assert!(
            first
                .open_media(item.message_id, item.file_id)
                .await
                .is_err()
        );
        assert_eq!(
            bytes(
                first
                    .open_stream_media(item.message_id, item.file_id)
                    .await
                    .unwrap()
            ),
            FIRST_BYTES
        );
        assert_eq!(api.requests.load(Relaxed), requests);
        let size = FIRST_BYTES.len() as u64;
        let shared = sync
            .clear_media(&owner, generation, &MediaFilter::Scope(scope(TOPIC)))
            .unwrap();
        assert_eq!(shared.estimate.selected_bytes, size);
        assert_eq!(shared.estimate.shared_bytes, size);
        assert_eq!(shared.freed_bytes, 0);
        assert_eq!(
            bytes(other.open_media(ANCHOR, FILE).await.unwrap()),
            FIRST_BYTES
        );
        assert_eq!(api.downloads.load(Relaxed), 1);

        let removed = sync
            .clear_media(&owner, generation, &MediaFilter::File(FILE))
            .unwrap();
        assert_eq!(removed.freed_bytes, size);
        assert_eq!(
            bytes(other.open_media(ANCHOR, FILE).await.unwrap()),
            FIRST_BYTES
        );
        assert_eq!(api.downloads.load(Relaxed), 2);

        let requests = api.requests.load(Relaxed);
        sync.apply(
            &owner,
            generation,
            SOURCE_ID,
            Event::HistoryChanged {
                cursor: Cursor {
                    epoch_generation: "test".into(),
                    epoch_version: 1,
                },
                change: HistoryChange::RemoveStream(CHAT.to_string()),
            },
        )
        .unwrap();
        assert!(other.open_media(ANCHOR, FILE).await.is_err());
        assert_eq!(
            api.requests.load(Relaxed),
            requests,
            "отзыв стрима известен локально"
        );
        server.abort();
    })
    .await
    .expect("не завершилась очистка общей копии");
}

/// Окно вокруг сообщения выдаёт независимый допуск для команды из лички и темы.
#[tokio::test]
async fn media_access_works_after_opening_around_and_moves_to_blocking_reader() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let client = source();

        let around = client
            .open_topic_history_with_media(&sync, &owner, generation, CHAT, TOPIC, NEWER)
            .await
            .unwrap();
        assert!(
            around
                .window
                .messages
                .iter()
                .any(|message| message.id == NEWER)
        );
        let access = around.media_access.unwrap();
        let requests = api.requests.load(Relaxed);
        let reader = client
            .open_media(&sync, &access, NEWER, FILE)
            .await
            .unwrap();
        let content = tokio::task::spawn_blocking(move || bytes(reader))
            .await
            .unwrap();
        assert_eq!(content, FIRST_BYTES);
        assert_eq!(api.requests.load(Relaxed), requests + 1);

        let target = NotificationTarget {
            context: owner,
            conversation_id: CHAT.to_string(),
            message_id: NEWER.to_string(),
        };
        let direct = client
            .open_direct_history_with_media(&sync, &target, generation)
            .await
            .unwrap()
            .media_access
            .unwrap();
        let requests = api.requests.load(Relaxed);
        let content = bytes(
            client
                .open_media(&sync, &direct, NEWER, FILE)
                .await
                .unwrap(),
        );
        assert_eq!(content, FIRST_BYTES);
        assert_eq!(
            api.requests.load(Relaxed),
            requests,
            "превью из лички читает диск"
        );
        server.abort();
    })
    .await
    .expect("не завершилась проверка допуска окна вокруг сообщения");
}

/// Сохраняет допуск этой темы после отзыва соседней и никогда не переносит его в другой профиль.
#[tokio::test]
async fn media_access_checks_scope_owner_and_profile_without_rechecking_membership() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let client = source();
        let opening = load_history(&client, &sync, &owner, generation, TOPIC).await;
        let access = opening.media_access().unwrap();
        assert_eq!(
            bytes(
                client
                    .open_media(&sync, &access, NEWER, FILE)
                    .await
                    .unwrap()
            ),
            FIRST_BYTES
        );
        let requests = api.requests.load(Relaxed);
        let other_profile = tempfile::tempdir().unwrap();
        let other = sync_for(&fixture.auth, &other_profile);
        assert!(
            client
                .open_media(&other, &access, NEWER, FILE)
                .await
                .is_err()
        );

        for (position, topic) in [(1, OTHER_TOPIC), (2, TOPIC)] {
            sync.apply(
                &owner,
                generation,
                SOURCE_ID,
                Event::HistoryChanged {
                    cursor: Cursor {
                        epoch_generation: "test".into(),
                        epoch_version: position,
                    },
                    change: HistoryChange::RemoveTopic(MessagePlacement {
                        stream_id: CHAT.to_string(),
                        topic_id: topic.to_string(),
                    }),
                },
            )
            .unwrap();
            let result = client.open_media(&sync, &access, NEWER, FILE).await;
            if topic == TOPIC {
                assert!(result.is_err());
            } else {
                assert_eq!(bytes(result.unwrap()), FIRST_BYTES);
                assert!(opening.list_media(true, None, None, 20).is_err());
                assert!(opening.open_stream_media(NEWER, FILE).await.is_err());
            }
        }
        assert_eq!(api.requests.load(Relaxed), requests);
        server.abort();
    })
    .await
    .expect("не завершилась проверка предметного отзыва");
}

/// Публикует только принятое изменение файла, инвалидирует читатель и разрешает свежий запрос.
#[tokio::test]
async fn media_file_events_publish_scoped_invalidation_and_refresh_current_link() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let client = source();
        let opening = load_history(&client, &sync, &owner, generation, TOPIC).await;
        let mut opened = opening.open_media(NEWER, FILE).await.unwrap();
        let mut events = sync.subscribe();
        let updated = Event::FileUpdated {
            cursor: Cursor {
                epoch_generation: "test".into(),
                epoch_version: 1,
            },
            file_id: FILE,
        };
        let expected = SyncUpdate {
            context: owner.clone(),
            generation,
            change: SyncChange::MediaFileInvalidated { file_id: FILE },
        };
        sync.apply(&owner, generation, SOURCE_ID, updated.clone())
            .unwrap();
        assert_eq!(events.try_recv().unwrap(), expected);
        assert!(opened.read(&mut [0]).is_err());
        sync.apply(&owner, generation, SOURCE_ID, updated).unwrap();
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        let requests = api.requests.load(Relaxed);
        let mut fresh = opening.open_media(NEWER, FILE).await.unwrap();
        assert_eq!(api.requests.load(Relaxed), requests + 1);
        let mut first_byte = [0];
        fresh.read_exact(&mut first_byte).unwrap();
        assert_eq!(first_byte[0], FIRST_BYTES[0]);

        api.download_status.store(404, Relaxed);
        sync.apply(
            &owner,
            generation,
            SOURCE_ID,
            Event::FileDeleted {
                cursor: Cursor {
                    epoch_generation: "test".into(),
                    epoch_version: 2,
                },
                file_id: FILE,
            },
        )
        .unwrap();
        assert_eq!(events.try_recv().unwrap(), expected);
        assert!(fresh.read(&mut [0]).is_err());
        let requests = api.requests.load(Relaxed);
        assert!(opening.open_media(NEWER, FILE).await.is_err());
        assert_eq!(
            api.requests.load(Relaxed),
            requests + 1,
            "права проверяет файловый GET"
        );
        server.abort();
    })
    .await
    .expect("не завершилась инвалидация изменений файла");
}

/// Свежий ответ для целевого сообщения вытесняет старую ссылку ещё до события журнала.
#[tokio::test]
async fn media_access_around_replaces_an_attachment_removed_from_the_fresh_anchor() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let client = source();
        let latest = load_history(&client, &sync, &owner, generation, TOPIC).await;
        let old = latest.media_access().unwrap();
        assert_eq!(
            bytes(client.open_media(&sync, &old, NEWER, FILE).await.unwrap()),
            FIRST_BYTES
        );

        api.anchor_changed.store(true, Relaxed);
        let around = client
            .open_topic_history_with_media(&sync, &owner, generation, CHAT, TOPIC, NEWER)
            .await
            .unwrap();
        let anchor = around
            .window
            .messages
            .iter()
            .find(|row| row.id == NEWER)
            .unwrap();
        assert_eq!(anchor.text, "Вложение убрано на сервере");
        let requests = api.requests.load(Relaxed);
        let new = around.media_access.unwrap();
        assert!(client.open_media(&sync, &old, NEWER, FILE).await.is_err());
        assert!(client.open_media(&sync, &new, NEWER, FILE).await.is_err());
        assert_eq!(api.requests.load(Relaxed), requests);
        server.abort();
    })
    .await
    .expect("не завершилась проверка свежей ссылки на целевом сообщении");
}
