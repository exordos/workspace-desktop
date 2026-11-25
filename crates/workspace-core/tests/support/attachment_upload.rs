//! Проверяет загрузку, прогресс и очистку вложений через штатные HTTPS-запросы.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::Duration,
};

use reqwest::Url;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Notify, watch},
    time::timeout,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        MessengerSource,
        encrypted_send::EncryptedMessageError,
        encryption::{EncryptionError, EncryptionSession, GroupKey, MessageEncryption},
        files::{
            AttachmentUpload, AttachmentUploadControl, AttachmentUploadProgress,
            DiscardAttachmentError, UploadAttachmentError,
        },
        references::media_references,
        send::DirectSendError,
    },
    storage::{HistoryScope, Storage},
    sync::Sync,
};

use super::{
    CA, CHAT, Fixture, HistoryApi, HttpsSocket, OTHER_TOPIC, TOPIC, outgoing::SendKeys, serve,
};

const FILE: Uuid = Uuid::from_u128(83);
const DEVICE: Uuid = Uuid::from_u128(84);
const CHAT_KEY: Uuid = Uuid::from_u128(85);
const CONTENT: &[u8] = b"Workspace attachment body";
const LONG_CONTENT: &[u8] = &[b'x'; 128 * 1024];
const NAME: &str = "Отчёт _ ноябрь.txt";

/// Даёт проверить отказ, чужую квитанцию и гонки загрузки и удаления.
#[derive(Default)]
pub(super) struct Api {
    mode: AtomicUsize, // 0 успех, 1 чужой стрим, 2 закрытая тема, 3 E2EE, 4 задержка, 5 отказ DELETE, 6 задержка E2EE.
    posts: AtomicUsize, // Число полученных multipart, считая оставшиеся без ответа.
    deletes: AtomicUsize, // Число полученных запросов очистки.
    received: Notify,  // Multipart уже полностью прочитан на сервере.
    release: Notify,   // Тест закончил работу с задержанным ответом.
    deleted: Notify,   // Сервер принял DELETE.
    content: Option<&'static [u8]>, // Байты требуемого файла, по умолчанию CONTENT.
}

impl Api {
    /// Отвечает на штатные запросы темы, стрима и multipart в контексте одного владельца.
    pub(super) async fn reply(
        &self,
        mut socket: HttpsSocket,
        request: Url,
        method: &str,
        headers: &str,
    ) {
        let mode = self.mode.load(Relaxed);
        let content = self.content.unwrap_or(CONTENT);
        assert!(headers.contains("authorization: Bearer first"));
        let path = request.path();
        let (status, response): (_, Value) = match method {
            "GET" if path.ends_with(&format!("/stream_topics/{TOPIC}")) => (
                "200 OK",
                json!({
                    "uuid": TOPIC, "stream_uuid": CHAT, "project_id": "project", "user_uuid": "first",
                    "notification_mode": "default", "is_done": mode == 2,
                }),
            ),
            "GET" if path.ends_with(&format!("/streams/{CHAT}")) => (
                "200 OK",
                json!({
                    "uuid": CHAT, "project_id": "project", "user_uuid": "first", "source_name": "native",
                    "private": false, "is_archived": false, "notification_mode": "all_messages",
                    "encryption": matches!(mode, 3 | 6), "current_encryption_key_uuid": CHAT_KEY,
                }),
            ),
            "POST" if path.ends_with("/files/") => {
                self.posts.fetch_add(1, Relaxed);
                assert!(headers.contains("content-type: multipart/form-data; boundary="));
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut bytes = vec![0; length];
                socket.read_exact(&mut bytes).await.unwrap();
                let body = String::from_utf8_lossy(&bytes);
                assert!(body.contains(&format!("name=\"stream_uuid\"\r\n\r\n{CHAT}")));
                assert!(body.contains("name=\"file\""));
                assert!(body.contains(&format!("name=\"name\"\r\n\r\n{NAME}")));
                assert!(body.contains("text/plain"));
                assert!(body.contains(std::str::from_utf8(content).unwrap()));
                assert!(!body.contains("name=\"acl\""));
                if matches!(mode, 4 | 6) {
                    self.received.notify_one();
                    self.release.notified().await;
                }
                (
                    "201 Created",
                    json!({
                        "uuid": FILE, "stream_uuid": if mode == 1 { OTHER_TOPIC } else { CHAT }, "user_uuid": "first",
                        "name": NAME, "content_type": "text/plain", "size_bytes": content.len(),
                        "hash": format!("{:x}", Sha256::digest(content)),
                    }),
                )
            }
            "DELETE" if path.ends_with(&format!("/files/{FILE}")) => {
                self.deletes.fetch_add(1, Relaxed);
                self.deleted.notify_one();
                let status = if mode == 5 {
                    "503 Service Unavailable"
                } else {
                    "200 OK"
                };
                (status, Value::Null)
            }
            _ => panic!("лишний запрос {method} {path}"),
        };
        let body = response.to_string();
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        // Клиент вправе закрыть запрос сразу после отмены.
        let _ = socket.get_mut().write_all(reply.as_bytes()).await;
        let _ = socket.get_mut().shutdown().await;
    }
}

/// Готовит новое вложение для отдельной попытки через обычную тему.
fn file() -> AttachmentUpload {
    AttachmentUpload {
        name: "Отчёт / ноябрь.txt".into(),
        content_type: "text/plain".into(),
        bytes: CONTENT.to_vec(),
        dimensions: None,
    }
}

/// Загружает файл один раз и проверяет отказ при сменившейся цели, отзыве или отмене.
#[tokio::test]
async fn uploads_once_into_verified_plain_topic_and_cancels_without_retry() {
    timeout(Duration::from_secs(8), async {
        let api = Arc::new(Api::default());
        let (url, server) = serve(Arc::new(HistoryApi {
            upload: Some(api.clone()),
            ..HistoryApi::default()
        }))
        .await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let generation = fixture.auth.snapshot()[0].generation;
        let source =
            MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
                .unwrap();
        let sync = Sync::new(
            fixture.auth.clone(),
            Arc::new(Storage::in_memory().unwrap()),
        );
        let scope = HistoryScope {
            stream_id: CHAT,
            topic_id: Some(TOPIC),
        };
        let upload =
            |cancel| source.upload_attachment(&sync, &owner, generation, scope, file(), cancel);

        let result = upload(None).await.unwrap();
        assert_eq!(result.id(), FILE);
        let parsed = media_references(&result.markdown);
        assert_eq!(parsed[0].file_id, FILE);
        assert_eq!(parsed[0].name.as_deref(), Some(NAME));
        assert_eq!(api.posts.load(Relaxed), 1);
        source
            .discard_uploaded_attachment(&sync, result)
            .await
            .unwrap();
        assert_eq!(api.deletes.load(Relaxed), 1);

        api.mode.store(1, Relaxed);
        assert!(matches!(
            upload(None).await,
            Err(UploadAttachmentError::Unknown)
        ));
        assert_eq!(api.posts.load(Relaxed), 2);

        api.mode.store(2, Relaxed);
        assert!(matches!(
            upload(None).await,
            Err(UploadAttachmentError::Destination(
                DirectSendError::TopicClosed
            ))
        ));
        api.mode.store(3, Relaxed);
        assert!(matches!(
            upload(None).await,
            Err(UploadAttachmentError::Destination(
                DirectSendError::EncryptionUnavailable
            ))
        ));
        assert_eq!(api.posts.load(Relaxed), 2);

        api.mode.store(4, Relaxed);
        let (cancel, canceled) = watch::channel(false);
        let request = upload(Some(canceled));
        let cancellation = async {
            api.received.notified().await;
            cancel.send(true).unwrap();
        };
        let (result, ()) = tokio::join!(request, cancellation);
        assert!(matches!(result, Err(UploadAttachmentError::Canceled)));
        api.release.notify_one();
        assert_eq!(api.posts.load(Relaxed), 3);

        api.mode.store(0, Relaxed);
        let other_profile = Sync::new(
            fixture.auth.clone(),
            Arc::new(Storage::in_memory().unwrap()),
        );
        let receipt = upload(None).await.unwrap();
        assert!(matches!(
            source
                .discard_uploaded_attachment(&other_profile, receipt)
                .await,
            Err(DiscardAttachmentError::Session(_))
        ));
        let receipt = upload(None).await.unwrap();
        fixture.auth.logout(&owner.account_id).unwrap();
        fixture.login_at(&url, "first");
        assert!(matches!(
            source.discard_uploaded_attachment(&sync, receipt).await,
            Err(DiscardAttachmentError::Session(_))
        ));
        assert_eq!(api.deletes.load(Relaxed), 1);
        server.abort();
    })
    .await
    .expect("сервер должен принять и завершить проверочные запросы");
}

/// Проверяет ключ перед POST, прогресс multipart и очистку при отзыве ключевого контекста.
#[tokio::test]
async fn encrypted_attachment_transfers_bytes_and_discards_stale_receipt() {
    timeout(Duration::from_secs(8), async {
        let api = Arc::new(Api {
            mode: AtomicUsize::new(3),
            content: Some(LONG_CONTENT),
            ..Api::default()
        });
        let (url, server) = serve(Arc::new(HistoryApi {
            upload: Some(api.clone()),
            ..HistoryApi::default()
        }))
        .await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&url, "first");
        let generation = fixture.auth.snapshot()[0].generation;
        let keys = Arc::new(MessageEncryption::new(Arc::new(SendKeys::default())));
        let activation = keys
            .select_session(Some(EncryptionSession {
                owner,
                generation,
                device_id: DEVICE,
            }))
            .unwrap();
        let source =
            MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
                .unwrap()
                .with_chat_encryption(keys.clone(), DEVICE);
        let sync = Sync::new(
            fixture.auth.clone(),
            Arc::new(Storage::in_memory().unwrap()),
        );
        let scope = HistoryScope {
            stream_id: CHAT,
            topic_id: Some(TOPIC),
        };
        let upload = |control| {
            source.upload_encrypted_attachment(
                &sync,
                &activation,
                scope,
                AttachmentUpload {
                    bytes: LONG_CONTENT.to_vec(),
                    ..file()
                },
                control,
            )
        };

        assert!(matches!(
            upload(AttachmentUploadControl::default()).await,
            Err(UploadAttachmentError::Encryption(
                EncryptedMessageError::Key(EncryptionError::MissingKey)
            ))
        ));
        assert_eq!(api.posts.load(Relaxed), 0);
        keys.save_key(&activation, CHAT, CHAT_KEY, &GroupKey::from_bytes([7; 32]))
            .unwrap();
        let (progress, observe) = watch::channel(AttachmentUploadProgress::default());
        let receipt = upload(AttachmentUploadControl {
            progress: Some(progress),
            cancel: None,
        })
        .await
        .unwrap();
        assert_eq!(receipt.id(), FILE);
        assert_eq!(
            *observe.borrow(),
            AttachmentUploadProgress {
                loaded_bytes: LONG_CONTENT.len() as u64,
                total_bytes: LONG_CONTENT.len() as u64,
            }
        );
        api.mode.store(5, Relaxed);
        assert!(matches!(
            source.discard_uploaded_attachment(&sync, receipt).await,
            Err(DiscardAttachmentError::Rejected(503))
        ));
        api.deleted.notified().await;
        assert_eq!(api.deletes.load(Relaxed), 1);

        api.mode.store(6, Relaxed);
        let upload = upload(AttachmentUploadControl::default());
        let invalidate = async {
            api.received.notified().await;
            keys.select_session(None);
            api.release.notify_one();
        };
        let (result, ()) = tokio::join!(upload, invalidate);
        assert!(matches!(
            result,
            Err(UploadAttachmentError::Encryption(
                EncryptedMessageError::Key(EncryptionError::Stale)
            ))
        ));
        api.deleted.notified().await;
        assert_eq!(api.posts.load(Relaxed), 2);
        assert_eq!(api.deletes.load(Relaxed), 2);
        server.abort();
    })
    .await
    .expect("сервер должен принять и завершить проверочные запросы");
}
