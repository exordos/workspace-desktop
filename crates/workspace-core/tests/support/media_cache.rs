//! Проверяет работу кэша вложений через открытие истории, файловый API и журнал мессенджера.
//! Сначала описывает локальный сервер, затем проверяет чтение, очистку и отзыв доступа.

use std::{
    io::Read,
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
use tokio::{io::AsyncWriteExt, sync::Notify, time::timeout};
use tokio_tungstenite::tungstenite::Message as Frame;
use uuid::Uuid;
use workspace_core::{
    auth::Auth,
    context::{OwnerContext, SessionGeneration},
    messenger::{
        MessengerSource, SOURCE_ID,
        encryption::{EncryptionSession, GroupKey, MessageEncryption},
        files::OpenMediaError,
        history::{LatestHistoryOpening, OpenHistoryError},
    },
    notification_target::NotificationTarget,
    runtime::{EventSource, SourceSession},
    storage::{
        Cursor, HistoryChange, HistoryScope, Message, MessageBody, MessagePlacement,
        PreparedSendBody, Storage,
        media::{MediaCachePolicy, MediaFilter, MediaKind},
    },
    sync::{Event, Sync, SyncChange, SyncUpdate},
};

use super::{
    ANCHOR, CA, CHAT, Fixture, HistoryApi, HttpsSocket, NEWER, OLDER, OTHER_TOPIC, TOPIC,
    outgoing::SendKeys, serve, upgrade,
};

const FILE: Uuid = Uuid::from_u128(90);
const OLD_KEY: Uuid = Uuid::from_u128(91);
const CURRENT_KEY: Uuid = Uuid::from_u128(92);
const DEVICE: Uuid = Uuid::from_u128(93);
const OUTSIDE_FILE: Uuid = Uuid::from_u128(94);
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
    protected: Mutex<Option<Value>>, // Зашифрованное серверное тело целевого сообщения.
    citing: Mutex<Option<Value>>, // Подтверждённое сообщение с цитатой в соседней теме.
    key_rotated: AtomicBool,      // Стрим уже объявил другое текущее поколение.
    hold_quote: AtomicBool,       // Задержать снимок оригинала до события правки.
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
                "project_id": "project", "notification_mode": "default", "is_done": false
            }))
        } else if path.ends_with(&format!("/streams/{CHAT}")) {
            json_response(json!({
                "uuid": CHAT, "user_uuid": user, "project_id": "project", "private": true,
                "direct_user_uuid": ANCHOR, "is_archived": false, "notification_mode": "all_messages",
                "source_name": "native", "encryption": true,
                "current_encryption_key_uuid": if self.key_rotated.load(Relaxed) { CURRENT_KEY } else { OLD_KEY },
            }))
        } else if path.ends_with(&format!("/messages/{NEWER}")) {
            let content = if self.anchor_changed.load(Relaxed) {
                "Вложение убрано на сервере".into()
            } else {
                format!("[вложение](urn:file:{FILE})")
            };
            let original = self.protected_message(NEWER, TOPIC, user, content);
            if self.hold_quote.swap(false, Relaxed) {
                self.received.notify_one();
                self.release.notified().await;
            }
            json_response(original)
        } else if path.ends_with(&format!("/messages/{ANCHOR}")) {
            json_response(self.protected_message(ANCHOR, OTHER_TOPIC, user, String::new()))
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
                self.protected_message(
                    linked,
                    topic,
                    user,
                    format!(
                        "[вложение](urn:file:{FILE}?name=report.pdf) [Документ](https://example.test/{topic})"
                    )
                ),
                history_message(
                    plain,
                    topic,
                    user,
                    "сообщение без вложений https://example.test/news".into()
                )
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

    /// Подставляет исходный защищённый снимок только для целевой строки.
    fn protected_message(&self, id: Uuid, topic: Uuid, user: &str, content: String) -> Value {
        let mut row = history_message(id, topic, user, content);
        let body = if id == NEWER && topic == TOPIC {
            self.protected.lock().unwrap().clone()
        } else if id == ANCHOR && topic == OTHER_TOPIC {
            self.citing.lock().unwrap().clone()
        } else {
            None
        };
        if let Some(protected) = body {
            row["source_name"] = json!("native");
            row["payload"] = protected;
        }
        row
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

/// Исторический ключ цитаты и вложения проверяется независимо от текущего поколения стрима.
#[tokio::test]
async fn protected_original_and_attachment_use_exact_key_and_cancel_without_indexing() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture =
            Fixture::new(|user, _| Ok(super::super::project_token(user, user, Some(user))));
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let encryption = Arc::new(MessageEncryption::new(Arc::new(SendKeys::default())));
        let activation = encryption
            .select_session(Some(EncryptionSession {
                owner: owner.clone(),
                generation,
                device_id: DEVICE,
            }))
            .unwrap();
        let old_key = GroupKey::from_bytes([93; 32]);
        encryption
            .save_key(&activation, CHAT, OLD_KEY, &old_key)
            .unwrap();
        let client = source().with_chat_encryption(encryption.clone(), DEVICE);
        let protected_text =
            format!("[Защищённое имя](urn:file:{FILE})\nhttps://private.test/link");
        let outgoing = client
            .prepare_encrypted_send(&sync, &activation, scope(TOPIC), protected_text.clone())
            .await
            .unwrap();
        let PreparedSendBody::Encrypted {
            key_id, ciphertext, ..
        } = outgoing.body
        else {
            panic!("проверяем только зашифрованное тело");
        };
        assert_eq!(key_id, OLD_KEY);
        *api.protected.lock().unwrap() = Some(json!({
            "kind": "e2ee", "key_uuid": OLD_KEY, "content": ciphertext
        }));
        api.key_rotated.store(true, Relaxed);
        encryption
            .save_key(
                &activation,
                CHAT,
                CURRENT_KEY,
                &GroupKey::from_bytes([45; 32]),
            )
            .unwrap();
        let opening = load_history(&client, &sync, &owner, generation, TOPIC).await;
        let access = opening.media_access().unwrap();
        assert!(matches!(
            client
                .load_quote_original(&sync, &owner, generation, scope(OTHER_TOPIC), NEWER)
                .await,
            Err(OpenHistoryError::MessageMissing)
        ));
        let original = client
            .load_quote_original(&sync, &owner, generation, scope(TOPIC), NEWER)
            .await
            .unwrap();
        assert_eq!(original.topic_id, TOPIC);
        let MessageBody::Encrypted { key_id, ciphertext } = original.body else {
            panic!("цитата хранит исходный конверт");
        };
        assert_eq!(key_id, OLD_KEY);
        let accepted = encryption
            .accept(
                encryption
                    .prepare(&activation, CHAT, key_id)
                    .unwrap()
                    .decrypt(&ciphertext),
            )
            .unwrap();
        assert_eq!(accepted.with_body(str::to_owned), Some(protected_text));
        assert!(matches!(
            client
                .open_protected_media(&sync, &access, &activation, NEWER, OUTSIDE_FILE)
                .await,
            Err(OpenMediaError::Unavailable)
        ));
        assert_eq!(api.downloads.load(Relaxed), 0);
        assert!(opening.open_media(NEWER, FILE).await.is_err());
        let cross_topic = client
            .resolve_quote_original(&sync, &owner, generation, NEWER)
            .await
            .unwrap();
        assert_eq!(cross_topic.topic_id, TOPIC);
        let mut opened = client
            .open_protected_media(&sync, &access, &activation, NEWER, FILE)
            .await
            .unwrap();
        assert_eq!(opened.size().unwrap(), FIRST_BYTES.len() as u64);
        assert_eq!(
            opened
                .with_metadata(|meta| meta.mime.clone())
                .unwrap()
                .as_deref(),
            Some("text/plain")
        );
        let mut read = Vec::new();
        opened.read_to_end(&mut read).unwrap();
        assert_eq!(read, FIRST_BYTES);
        assert_eq!(api.downloads.load(Relaxed), 1);

        let citing = client
            .prepare_encrypted_send(
                &sync,
                &activation,
                scope(OTHER_TOPIC),
                workspace_core::messenger::quote::reference(NEWER, "Оригинал", None),
            )
            .await
            .unwrap();
        let PreparedSendBody::Encrypted {
            key_id: citing_key,
            ciphertext,
            ..
        } = citing.body
        else {
            panic!("цитата хранится как защищённое сообщение");
        };
        assert_eq!(citing_key, CURRENT_KEY);
        *api.citing.lock().unwrap() = Some(json!({
            "kind": "e2ee", "key_uuid": citing_key, "content": ciphertext
        }));
        let other_topic = load_history(&client, &sync, &owner, generation, OTHER_TOPIC)
            .await
            .media_access()
            .unwrap();
        let citing_row = client
            .load_quote_original(&sync, &owner, generation, scope(OTHER_TOPIC), ANCHOR)
            .await
            .unwrap();
        let citing = encryption
            .accept(
                encryption
                    .prepare(&activation, CHAT, citing_key)
                    .unwrap()
                    .decrypt(&ciphertext),
            )
            .unwrap();
        let citing = Arc::new(citing);
        let quote = client
            .resolve_protected_quote(&sync, &activation, citing, &citing_row, NEWER, 0)
            .await
            .unwrap()
            .content
            .unwrap();
        let mut linked = client
            .open_linked_protected_quote_media(&sync, &other_topic, &activation, &quote, FILE)
            .await
            .unwrap();
        encryption
            .remove_key(&activation, CHAT, CURRENT_KEY)
            .unwrap();
        assert!(linked.read(&mut [0]).is_err());
        assert!(
            linked
                .with_valid_access(|| panic!("цитирующий ключ отозван"))
                .is_err(),
            "отзыв ключа цитаты блокирует даже публикацию уже загруженных байтов"
        );
        assert_eq!(opened.size().unwrap(), FIRST_BYTES.len() as u64);
        drop(linked);

        encryption
            .save_key(
                &activation,
                CHAT,
                CURRENT_KEY,
                &GroupKey::from_bytes([45; 32]),
            )
            .unwrap();
        let citing = encryption
            .accept(
                encryption
                    .prepare(&activation, CHAT, citing_key)
                    .unwrap()
                    .decrypt(&ciphertext),
            )
            .unwrap();
        let quote = client
            .resolve_protected_quote(&sync, &activation, Arc::new(citing), &citing_row, NEWER, 0)
            .await
            .unwrap()
            .content
            .unwrap();
        let mut linked = client
            .open_linked_protected_quote_media(&sync, &other_topic, &activation, &quote, FILE)
            .await
            .unwrap();
        let linked_preview = linked.protected_preview_permit().unwrap();
        assert!(linked_preview.with_current(|| ()).is_some());
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
                    reactions: None,
                    message: Message {
                        id: ANCHOR.to_string(),
                        body: MessageBody::Encrypted {
                            key_id: CURRENT_KEY,
                            ciphertext: "Yg==".into(),
                        },
                    },
                    placement: Some(MessagePlacement {
                        stream_id: CHAT.to_string(),
                        topic_id: OTHER_TOPIC.to_string(),
                    }),
                },
            },
        )
        .unwrap();
        // Оба ключа остаются действующими, однако уже готовое вложение потеряло версию ссылки.
        assert!(encryption.prepare(&activation, CHAT, CURRENT_KEY).is_ok());
        assert!(encryption.prepare(&activation, CHAT, OLD_KEY).is_ok());
        assert!(quote.with_current(|| ()).is_none());
        assert!(linked.read(&mut [0]).is_err());
        assert!(linked.with_metadata(|_| ()).is_err());
        assert!(linked.with_valid_access(|| ()).is_err());
        assert_eq!(opened.size().unwrap(), FIRST_BYTES.len() as u64);
        drop(linked);
        assert!(linked_preview.with_current(|| ()).is_none());

        api.hold_next.store(true, Relaxed);
        let pending = client.open_protected_media(&sync, &access, &activation, NEWER, FILE);
        let (stale, ()) = tokio::join!(pending, async {
            api.received.notified().await;
            encryption.remove_key(&activation, CHAT, OLD_KEY).unwrap();
        });
        assert!(matches!(stale, Err(OpenMediaError::Unavailable)));
        assert_eq!(accepted.with_body(str::to_owned), None);
        assert_eq!(
            opened.size().unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(
            opened
                .with_metadata(|_| panic!("метаданные отозваны"))
                .is_err()
        );
        assert!(
            opened
                .with_valid_access(|| panic!("отозванную копию нельзя публиковать"))
                .is_err()
        );
        drop(opened);
        assert_eq!(
            std::fs::read_dir(profile.path().join("media"))
                .unwrap()
                .count(),
            0
        );
        assert!(matches!(
            client
                .open_protected_media(&sync, &access, &activation, NEWER, FILE)
                .await,
            Err(OpenMediaError::Unavailable)
        ));
        assert_eq!(api.downloads.load(Relaxed), 4);
        api.release.notify_one();
        encryption
            .save_key(&activation, CHAT, OLD_KEY, &old_key)
            .unwrap();
        api.hold_next.store(true, Relaxed);
        let pending = client.open_protected_media(&sync, &access, &activation, NEWER, FILE);
        let (stale, ()) = tokio::join!(pending, async {
            api.received.notified().await;
            encryption.select_session(None);
        });
        assert!(matches!(stale, Err(OpenMediaError::Unavailable)));
        assert_eq!(api.downloads.load(Relaxed), 5);
        api.release.notify_one();
        // В SQLite нет ссылки из открытого Markdown и имени, даже после сетевой загрузки.
        let db = std::fs::read(profile.path().join("workspace.sqlite")).unwrap();
        assert!(
            !db.windows(b"private.test".len())
                .any(|bytes| bytes == b"private.test")
        );
        assert!(
            !db.windows(FILE.to_string().len())
                .any(|bytes| bytes == FILE.to_string().as_bytes())
        );
        server.abort();
    })
    .await
    .expect("вложение осталось ждать сеть после отзыва ключа");
}

/// Декодированная копия теряет доступ при обновлении файла или сообщения с тем же ключом.
#[tokio::test]
async fn protected_preview_tracks_committed_file_and_message_after_reader_is_dropped() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture =
            Fixture::new(|user, _| Ok(super::super::project_token(user, user, Some(user))));
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let encryption = Arc::new(MessageEncryption::new(Arc::new(SendKeys::default())));
        let activation = encryption
            .select_session(Some(EncryptionSession {
                owner: owner.clone(),
                generation,
                device_id: DEVICE,
            }))
            .unwrap();
        encryption
            .save_key(&activation, CHAT, OLD_KEY, &GroupKey::from_bytes([93; 32]))
            .unwrap();
        let client = source().with_chat_encryption(encryption.clone(), DEVICE);
        let outgoing = client
            .prepare_encrypted_send(
                &sync,
                &activation,
                scope(TOPIC),
                format!("[Файл](urn:file:{FILE})"),
            )
            .await
            .unwrap();
        let PreparedSendBody::Encrypted { ciphertext, .. } = outgoing.body else {
            panic!("исходная строка должна быть зашифрована");
        };
        *api.protected.lock().unwrap() = Some(json!({
            "kind": "e2ee", "key_uuid": OLD_KEY, "content": ciphertext
        }));
        let opening = load_history(&client, &sync, &owner, generation, TOPIC).await;
        let access = opening.media_access().unwrap();
        let open_preview = || async {
            let mut reader = client
                .open_protected_media(&sync, &access, &activation, NEWER, FILE)
                .await
                .unwrap();
            let mut content = Vec::new();
            reader.read_to_end(&mut content).unwrap();
            assert_eq!(content, FIRST_BYTES);
            reader.protected_preview_permit().unwrap()
        };
        let preview = open_preview().await;
        assert!(preview.with_current(|| ()).is_some());
        let update = |file_id, epoch_version| Event::FileUpdated {
            cursor: Cursor {
                epoch_generation: "test".into(),
                epoch_version,
            },
            file_id,
        };
        sync.apply(&owner, generation, SOURCE_ID, update(OUTSIDE_FILE, 1))
            .unwrap();
        assert!(preview.with_current(|| ()).is_some());
        sync.apply(&owner, generation, SOURCE_ID, update(FILE, 2))
            .unwrap();
        assert!(preview.with_current(|| ()).is_none());

        let next = open_preview().await;
        sync.apply(&owner, generation, SOURCE_ID, update(FILE, 2))
            .unwrap();
        assert!(next.with_current(|| ()).is_some());
        sync.apply(
            &owner,
            generation,
            SOURCE_ID,
            Event::HistoryChanged {
                cursor: Cursor {
                    epoch_generation: "test".into(),
                    epoch_version: 3,
                },
                change: HistoryChange::Edit {
                    reactions: None,
                    message: Message {
                        id: NEWER.to_string(),
                        body: MessageBody::Encrypted {
                            key_id: OLD_KEY,
                            ciphertext: "Yg==".into(),
                        },
                    },
                    placement: Some(MessagePlacement {
                        stream_id: CHAT.to_string(),
                        topic_id: TOPIC.to_string(),
                    }),
                },
            },
        )
        .unwrap();
        assert!(encryption.prepare(&activation, CHAT, OLD_KEY).is_ok());
        assert!(next.with_current(|| ()).is_none());
        server.abort();
    })
    .await
    .expect("проверить версии декодированного вложения до и после записи");
}

/// Старый REST не может вернуть оригинал до или после уже известной правки.
#[tokio::test]
async fn protected_quote_rejects_realtime_edit_before_and_during_old_rest_response() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        *api.protected.lock().unwrap() = Some(json!({
            "kind": "e2ee", "key_uuid": OLD_KEY, "content": "YQ=="
        }));
        let (url, server) = server(api.clone()).await;
        let fixture =
            Fixture::new(|user, _| Ok(super::super::project_token(user, user, Some(user))));
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let client = source();
        load_history(&client, &sync, &owner, generation, TOPIC).await;

        api.hold_quote.store(true, Relaxed);
        let request = client.resolve_quote_original(&sync, &owner, generation, NEWER);
        let (racing, ()) = tokio::join!(request, async {
            api.received.notified().await;
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
                        reactions: None,
                        message: Message {
                            id: NEWER.to_string(),
                            body: MessageBody::Encrypted {
                                key_id: OLD_KEY,
                                ciphertext: "Yg==".into(),
                            },
                        },
                        placement: Some(MessagePlacement {
                            stream_id: CHAT.to_string(),
                            topic_id: TOPIC.to_string(),
                        }),
                    },
                },
            )
            .unwrap();
            api.release.notify_one();
        });
        assert!(matches!(racing, Err(OpenHistoryError::StaleWindow)));
        assert!(matches!(
            client
                .resolve_quote_original(&sync, &owner, generation, NEWER)
                .await,
            Err(OpenHistoryError::StaleWindow)
        ));
        server.abort();
    })
    .await
    .expect("оригинал не закончил сверку известной правки");
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
        let access = opening.media_access().unwrap();
        let links = client.list_links(&sync, &access, false, None, 10).unwrap();
        assert!(links.links.iter().any(|link| link.message_id == NEWER));
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
                    reactions: None,
                    message: Message {
                        id: NEWER.to_string(),
                        body: MessageBody::Markdown("вложение убрано".into()),
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
        let links = client.list_links(&sync, &access, false, None, 10).unwrap();
        assert!(links.links.iter().all(|link| link.message_id != NEWER));
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

/// Галерея читает известную историю и байты, соблюдая область и поколения страниц.
#[tokio::test]
async fn media_gallery_uses_owned_access_and_projects_legacy_history() {
    timeout(DEADLINE, async {
        let api = Arc::new(Api::default());
        let (url, server) = server(api.clone()).await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let generation = generation(&fixture.auth, &owner);
        let profile = tempfile::tempdir().unwrap();
        let sync = sync_for(&fixture.auth, &profile);
        let client = source();
        let access = load_history(&client, &sync, &owner, generation, TOPIC)
            .await
            .media_access()
            .unwrap();
        load_history(&client, &sync, &owner, generation, OTHER_TOPIC).await;

        // Старый профиль хранит подтверждённые сообщения и ссылки на файлы без новых полей.
        let db = rusqlite::Connection::open(profile.path().join("workspace.sqlite")).unwrap();
        db.execute_batch(
            "UPDATE history_changes SET media_projection = 2;
             UPDATE messenger_media_refs SET name = NULL;
             DELETE FROM messenger_web_links;",
        )
        .unwrap();
        db.execute(
            "DELETE FROM history_changes WHERE message_id = ?1",
            [OLDER.to_string()],
        )
        .unwrap();
        drop(db);

        let requests = api.requests.load(Relaxed);
        let list = client
            .list_media(&sync, &access, true, Some(MediaKind::File), None, 1)
            .unwrap();
        assert_eq!(list.attachments.len(), 1);
        let cursor = list.next.unwrap();
        let rest = client
            .list_media(
                &sync,
                &access,
                true,
                Some(MediaKind::File),
                Some(cursor.clone()),
                1,
            )
            .unwrap();
        assert!(rest.next.is_none());
        let entries = [list.attachments[0].clone(), rest.attachments[0].clone()];
        let other = entries
            .iter()
            .find(|item| item.scope == scope(OTHER_TOPIC))
            .unwrap();
        assert_eq!(other.name.as_deref(), Some("report.pdf"));
        assert_eq!(other.size, None);
        assert!(!other.cached);
        assert!(
            client
                .open_gallery_cached_media(&sync, &access, true, other)
                .unwrap()
                .is_none()
        );
        assert!(
            client
                .open_gallery_media(&sync, &access, false, other)
                .await
                .is_err()
        );
        let mut foreign = other.clone();
        foreign.scope.stream_id = Uuid::from_u128(1000);
        assert!(
            client
                .open_gallery_media(&sync, &access, true, &foreign)
                .await
                .is_err()
        );

        let links = client.list_links(&sync, &access, false, None, 1).unwrap();
        let link_cursor = links.next.unwrap();
        let remaining = client
            .list_links(&sync, &access, false, Some(link_cursor.clone()), 1)
            .unwrap();
        assert!(remaining.next.is_none());
        let scoped_urls = [links.links[0].url.clone(), remaining.links[0].url.clone()];
        assert!(scoped_urls.contains(&format!("https://example.test/{TOPIC}")));
        assert!(scoped_urls.contains(&"https://example.test/news".into()));
        assert!(
            client
                .list_links(&sync, &access, true, Some(link_cursor.clone()), 2)
                .is_err()
        );
        let full = client.list_links(&sync, &access, true, None, 10).unwrap();
        assert_eq!(full.links.len(), 4);
        assert!(full.links.iter().any(|item| {
            item.scope == scope(OTHER_TOPIC)
                && item.label == "Документ"
                && item.url == format!("https://example.test/{OTHER_TOPIC}")
        }));
        assert_eq!(api.requests.load(Relaxed), requests);

        assert_eq!(
            bytes(
                client
                    .open_gallery_media(&sync, &access, true, other)
                    .await
                    .unwrap()
            ),
            FIRST_BYTES
        );
        assert_eq!(
            bytes(
                client
                    .open_gallery_cached_media(&sync, &access, true, other)
                    .unwrap()
                    .unwrap()
            ),
            FIRST_BYTES
        );
        let current = client
            .list_media(&sync, &access, true, Some(MediaKind::File), None, 10)
            .unwrap();
        assert!(
            current
                .attachments
                .iter()
                .all(|item| item.cached && item.size == Some(FIRST_BYTES.len() as u64))
        );
        assert_eq!(api.downloads.load(Relaxed), 1);

        sync.apply(
            &owner,
            generation,
            SOURCE_ID,
            Event::HistoryChanged {
                cursor: Cursor {
                    epoch_generation: "test".into(),
                    epoch_version: 1,
                },
                change: HistoryChange::RemoveTopic(MessagePlacement {
                    stream_id: CHAT.to_string(),
                    topic_id: OTHER_TOPIC.to_string(),
                }),
            },
        )
        .unwrap();
        assert!(
            client
                .list_links(&sync, &access, false, Some(link_cursor), 1)
                .is_err()
        );
        assert!(
            client
                .list_media(&sync, &access, true, Some(MediaKind::File), Some(cursor), 1)
                .is_err()
        );
        assert!(
            client
                .open_gallery_cached_media(&sync, &access, true, other)
                .is_err()
        );
        assert!(
            client
                .open_gallery_media(&sync, &access, true, other)
                .await
                .is_err()
        );
        let unaffected = entries
            .iter()
            .find(|item| item.scope == scope(TOPIC))
            .unwrap();
        assert_eq!(
            bytes(
                client
                    .open_gallery_media(&sync, &access, true, unaffected)
                    .await
                    .unwrap()
            ),
            FIRST_BYTES
        );
        let fresh = client.list_links(&sync, &access, true, None, 10).unwrap();
        assert_eq!(fresh.links.len(), 2);
        assert!(fresh.links.iter().all(|item| item.scope == scope(TOPIC)));
        assert_eq!(api.requests.load(Relaxed), requests + 1);
        server.abort();
    })
    .await
    .expect("не завершилась проверка локальных страниц галереи");
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
        assert_eq!(anchor.body.markdown(), Some("Вложение убрано на сервере"));
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
