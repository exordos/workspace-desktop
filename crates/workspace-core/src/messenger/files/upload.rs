//! Проверяет файлы редактора, подтверждает обычную цель отправки и создаёт ссылку после multipart.
//! Ошибку или отмену загрузки не повторяет автоматически, поскольку сервер сам создаёт UUID файла.

use std::future::pending;

use futures::StreamExt;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    auth::{Auth, AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    directory::AvatarFormat,
    storage::{HistoryScope, StorageError},
    sync::Sync,
};

use super::MessengerSource;
use crate::messenger::{references::build_media_reference_markdown, send::DirectSendError};

/// Предел исходного файла в редакторе Electron, в байтах.
pub const MAX_ATTACHMENT_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// Выбранный пользователем файл. Платформа читает его вне потока интерфейса.
pub struct AttachmentUpload {
    pub name: String,                   // Имя для показа и записи на сервере.
    pub content_type: String, // MIME файла, при пустом значении используется общий бинарный тип.
    pub bytes: Vec<u8>,       // Снимок байтов для одной попытки.
    pub dimensions: Option<(u32, u32)>, // Ширина и высота медиа в пикселях, если известны.
}

/// Подтверждённое сервером вложение, готовое к вставке в Markdown черновика.
#[derive(Debug, Eq, PartialEq)]
pub struct UploadedAttachment {
    pub id: Uuid,         // UUID, присвоенный файловым API.
    pub markdown: String, // Каноническая ссылка с заголовком и сведениями о файле.
}

/// Ошибка одной попытки загрузки. Сервер мог создать файл до потери ответа или отмены.
#[derive(Debug, Error)]
pub enum UploadAttachmentError {
    #[error("attachment is empty")]
    Empty, // Пустой файл запрещён редактором.
    #[error("attachment exceeds the upload limit")]
    TooLarge, // Файл не проходит предварительный предел или сервер вернул 413.
    #[error("invalid attachment format")]
    InvalidInput, // MIME, сигнатура известного изображения или адрес не прошли проверку.
    #[error("attachment upload was canceled")]
    Canceled, // Пользователь отменил операцию, не подтверждённый сервером UUID неизвестен.
    #[error("attachment upload failed with HTTP {0}")]
    Rejected(u16), // API явно отказал и вернул код ответа.
    #[error("attachment upload outcome is unknown")]
    Unknown, // HTTPS оборвался или ответ не подтвердил точный файл и его контекст.
    #[error(transparent)]
    Destination(#[from] DirectSendError), // Цель не разрешает обычную отправку.
    #[error(transparent)]
    Session(#[from] AuthError), // Сессия больше не принимает результат.
    #[error(transparent)]
    Storage(#[from] StorageError), // Локальная версия доступа недоступна.
}

/// Подготовленные байты и поля, с которыми будет сопоставляться квитанция сервера.
pub(in crate::messenger) struct PreparedAttachment {
    pub(in crate::messenger) bytes: Vec<u8>, // Уходит через multipart без копии.
    pub(in crate::messenger) receipt: ExpectedAttachment, // Сверяется после отправки байтов.
}

/// Собственные метаданные файла для проверки полученной квитанции.
pub(in crate::messenger) struct ExpectedAttachment {
    pub(in crate::messenger) name: String, // Санитизированное имя, переданное в двух частях формы.
    pub(in crate::messenger) mime: String, // Проверенный заголовок части файла.
    pub(in crate::messenger) size: usize,  // Длина исходных байтов.
    pub(in crate::messenger) sha256: String, // Ожидаемый SHA-256 в шестнадцатеричном виде.
    dimensions: Option<(u32, u32)>,        // Подсказка для размера карточки, API её не хранит.
}

impl ExpectedAttachment {
    /// Использует UUID из квитанции только после сверки её полей в REST.
    pub(in crate::messenger) fn reference(self, id: Uuid) -> UploadedAttachment {
        UploadedAttachment {
            id,
            markdown: build_media_reference_markdown(
                id,
                &self.name,
                &self.mime,
                self.size,
                self.dimensions,
            ),
        }
    }
}

impl MessengerSource {
    /// Загружает один файл в подтверждённый стрим активной сессии и возвращает точную Markdown-ссылку.
    pub async fn upload_attachment(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        upload: AttachmentUpload,
        cancel: Option<watch::Receiver<bool>>,
    ) -> Result<UploadedAttachment, UploadAttachmentError> {
        let auth = sync.auth();
        let operation = Uuid::new_v4();
        let bytes = upload.bytes.len();
        log::debug!("Загрузка вложения: op_id={operation} status=started bytes={bytes}");
        let attempt = async {
            let prepared = tokio::task::spawn_blocking(move || prepare(upload))
                .await
                .map_err(|_| UploadAttachmentError::InvalidInput)??;
            let storage = sync.storage();
            let initial = auth.with_active_generation(owner, generation, || {
                storage.media_access_revision(owner, scope)
            })??;
            let checked = self
                .resolve_plain_send(sync, owner, generation, scope)
                .await?;
            let target = HistoryScope {
                topic_id: Some(checked.topic_id()),
                ..scope
            };
            // Личка без явно выбранной темы сначала вернула UUID темы по умолчанию.
            if scope.topic_id.is_none() {
                self.check_topic_send(
                    sync,
                    owner,
                    generation,
                    target.stream_id,
                    checked.topic_id(),
                )
                .await?;
            }
            let token = auth
                .access_token_for_generation(owner, Service::Messenger, generation)
                .await?;
            let version = auth.with_active_generation(owner, generation, || {
                if storage.media_access_revision(owner, scope)? != initial {
                    return Err(DirectSendError::AccessLost.into());
                }
                Ok::<_, UploadAttachmentError>(storage.media_access_revision(owner, target)?)
            })??;
            log::debug!("Загрузка вложения: op_id={operation} status=request");
            let result = self
                .rest
                .multipart_attachment(owner, &token, target.stream_id, prepared)
                .await;
            auth.with_active_generation(owner, generation, || {
                if storage.media_access_revision(owner, target)? != version {
                    return Err(DirectSendError::AccessLost.into());
                }
                result
            })?
        };
        let result = tokio::select! {
            biased;
            _ = canceled(cancel) => Err(UploadAttachmentError::Canceled),
            _ = inactive_session(auth, owner, generation) => Err(AuthError::SessionUnavailable.into()),
            result = attempt => result,
        };
        let status = match &result {
            Ok(_) => "uploaded",
            Err(UploadAttachmentError::Canceled) => "canceled",
            Err(UploadAttachmentError::Empty | UploadAttachmentError::InvalidInput) => {
                "invalid_file"
            }
            Err(UploadAttachmentError::TooLarge) => "too_large",
            Err(UploadAttachmentError::Rejected(_)) => "rejected",
            Err(UploadAttachmentError::Unknown) => "unknown",
            Err(UploadAttachmentError::Destination(_)) => "invalid_destination",
            Err(UploadAttachmentError::Session(_)) => "session_ended",
            Err(UploadAttachmentError::Storage(_)) => "storage_error",
        };
        log::debug!("Загрузка вложения: op_id={operation} status={status}");
        result
    }
}

/// Ставит отмену выше результата, если оба события поступили к одному опросу.
async fn canceled(cancel: Option<watch::Receiver<bool>>) {
    let Some(mut cancel) = cancel else {
        return pending().await;
    };
    while !*cancel.borrow_and_update() {
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

/// Прерывает запрос уже на этапе ожидания, когда исходная сессия вышла или сменилась.
async fn inactive_session(auth: &Auth, owner: &OwnerContext, generation: SessionGeneration) {
    let mut sessions = Box::pin(auth.subscribe());
    while sessions.next().await.is_some() {
        if auth
            .with_active_generation(owner, generation, || ())
            .is_err()
        {
            return;
        }
    }
}

/// Ограничивает размер, проверяет заявленный формат и снимает хеш до начала HTTP.
fn prepare(upload: AttachmentUpload) -> Result<PreparedAttachment, UploadAttachmentError> {
    let size = upload.bytes.len();
    if size == 0 {
        return Err(UploadAttachmentError::Empty);
    }
    if size > MAX_ATTACHMENT_UPLOAD_BYTES {
        return Err(UploadAttachmentError::TooLarge);
    }
    let mime = normalize_type(&upload.content_type).ok_or(UploadAttachmentError::InvalidInput)?;
    if let Some(format) = AvatarFormat::from_content_type(&mime)
        && AvatarFormat::sniff(&upload.bytes) != Some(format)
    {
        return Err(UploadAttachmentError::InvalidInput);
    }
    Ok(PreparedAttachment {
        receipt: ExpectedAttachment {
            name: sanitize_name(&upload.name),
            mime,
            size,
            sha256: format!("{:x}", Sha256::digest(&upload.bytes)),
            dimensions: upload.dimensions,
        },
        bytes: upload.bytes,
    })
}

/// Приводит Content-Type к одному типу без параметров, заголовков и подстановочных знаков.
fn normalize_type(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return Some("application/octet-stream".into());
    }
    let (main, sub) = value.split_once('/')?;
    let token = |field: &str| {
        !field.is_empty()
            && field
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'+-.^_`|~".contains(&byte))
    };
    if value.len() > 255 || !token(main) || !token(sub) {
        return None;
    }
    let value = value.to_ascii_lowercase();
    Some(if value == "image/jpg" {
        "image/jpeg".into()
    } else {
        value
    })
}

/// Делает имя пригодным для multipart и показа, убирая пути, управляющие знаки и двойные точки.
fn sanitize_name(value: &str) -> String {
    let mut name = String::new();
    for ch in value.trim().chars().take(255) {
        if ch == '.' && name.ends_with('.') {
            continue;
        }
        name.push(if ch.is_control() || r#"<>:"/\|?*"#.contains(ch) {
            '_'
        } else {
            ch
        });
    }
    if matches!(name.as_str(), "" | ".") {
        "file".into()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{messenger::references::media_references, storage::media::MediaKind};

    /// Соединяет проверенные байты со ссылкой, которую понимают история и локальный индекс.
    #[test]
    fn prepares_canonical_attachment_markdown() {
        let upload = prepare(AttachmentUpload {
            name: "Фото [1] / .png".into(),
            content_type: "Image/PNG".into(),
            bytes: b"\x89PNG\r\n\x1a\n".to_vec(),
            dimensions: Some((320, 480)),
        })
        .unwrap();
        let id = Uuid::from_u128(2);
        let attachment = upload.receipt.reference(id);
        assert!(attachment.markdown.starts_with("!["));
        let [parsed]: [_; 1] = media_references(&attachment.markdown).try_into().unwrap();
        assert_eq!(parsed.file_id, id);
        assert_eq!(parsed.kind, MediaKind::Image);
        assert_eq!(parsed.name.as_deref(), Some("Фото [1] _ .png"));
        assert_eq!(parsed.metadata.content_type.as_deref(), Some("image/png"));
        assert_eq!(
            parsed.metadata.width.zip(parsed.metadata.height),
            Some((320, 480))
        );
        assert_eq!(parsed.metadata.size_bytes, Some(8));
    }

    /// Отклоняет пустые, переведённые в заголовок и ошибочно названные изображения.
    #[test]
    fn rejects_empty_or_invalid_file() {
        for (mime, bytes) in [
            ("text/plain", b"".as_slice()),
            ("image/png", b"wrong"),
            ("application/bad\nheader", b"x"),
        ] {
            assert!(
                prepare(AttachmentUpload {
                    name: "file".into(),
                    content_type: mime.into(),
                    bytes: bytes.to_vec(),
                    dimensions: None
                })
                .is_err()
            );
        }
    }

    /// Предварительная отмена и смена значения завершают ожидание без сети.
    #[tokio::test]
    async fn cancellation_is_sticky() {
        let (sender, receiver) = watch::channel(false);
        sender.send(true).unwrap();
        canceled(Some(receiver)).await;
    }
}
