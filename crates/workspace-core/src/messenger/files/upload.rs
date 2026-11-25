//! Проверяет файл и цель композера, связывает квитанцию multipart с загрузившей сессией.
//! Неиспользованную квитанцию можно обменять на одну попытку серверной очистки.

use std::{
    future::pending,
    sync::{Arc, Weak},
};

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
use crate::messenger::{
    encrypted_send::{ChatEncryption, EncryptedMessageError},
    encryption::{EncryptionActivation, MessageDecryption},
    references::build_media_reference_markdown,
    rest::RestClient,
    send::DirectSendError,
};

/// Предел исходного файла в редакторе Electron, в байтах.
pub const MAX_ATTACHMENT_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// Выбранный пользователем файл. Платформа читает его вне потока интерфейса.
pub struct AttachmentUpload {
    pub name: String,                   // Имя для показа и записи на сервере.
    pub content_type: String, // MIME файла, при пустом значении используется общий бинарный тип.
    pub bytes: Vec<u8>,       // Снимок байтов для одной попытки.
    pub dimensions: Option<(u32, u32)>, // Ширина и высота медиа в пикселях, если известны.
}

/// Параметры наблюдения за одной попыткой загрузки.
#[derive(Default)]
pub struct AttachmentUploadControl {
    pub cancel: Option<watch::Receiver<bool>>, // True или закрытый канал отменяет попытку.
    pub progress: Option<watch::Sender<AttachmentUploadProgress>>, // Получает число прочитанных байтов.
}

/// Сколько байтов файла HTTP-клиент взял в тело запроса.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AttachmentUploadProgress {
    pub loaded_bytes: u64, // Байты, уже переданные HTTP-клиенту.
    pub total_bytes: u64,  // Полный размер файла, без частей multipart.
}

/// Квитанция подтверждённой загрузки и одноразовый допуск к серверной очистке.
/// После передачи ссылки в сообщение квитанцию нужно выбросить.
#[derive(Debug)]
pub struct UploadedAttachment {
    id: Uuid,                      // UUID, выданный файловым API в проверенной квитанции.
    owner: OwnerContext,           // Владелец файла и адрес сервера из исходного запроса.
    generation: SessionGeneration, // Вход, который загрузил файл и может запросить очистку.
    authority: Weak<()>,           // Профиль, в котором сервер подтвердил файл.
    pub markdown: String,          // Каноническая ссылка с заголовком и сведениями о файле.
}

impl UploadedAttachment {
    /// Возвращает UUID для сравнения с сохранённой ссылкой.
    pub fn id(&self) -> Uuid {
        self.id
    }
}

/// Почему серверная очистка подтверждённого вложения не завершилась.
#[derive(Debug, Error)]
pub enum DiscardAttachmentError {
    #[error("attachment cleanup failed with HTTP {0}")]
    Rejected(u16), // Сервер вернул код, отличный от успеха или отсутствия файла.
    #[error("attachment cleanup outcome is unknown")]
    Unknown, // HTTPS оборвался или не удалось начать запрос.
    #[error(transparent)]
    Session(#[from] AuthError), // Вход завершился или перестал выдавать токен.
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
    Encryption(#[from] EncryptedMessageError), // Цель или ключ защищённой отправки больше недоступны.
    #[error(transparent)]
    Session(#[from] AuthError), // Сессия больше не принимает результат.
    #[error(transparent)]
    Storage(#[from] StorageError), // Локальная версия доступа недоступна.
}

/// Подготовленные байты и поля, с которыми будет сопоставляться квитанция сервера.
pub(in crate::messenger) struct PreparedAttachment {
    pub(in crate::messenger) bytes: Vec<u8>, // Исходные байты части multipart.
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
    pub(in crate::messenger) fn markdown(self, id: Uuid) -> String {
        build_media_reference_markdown(id, &self.name, &self.mime, self.size, self.dimensions)
    }
}

/// Источник разрешения на отправку для открытого или защищённого композера.
enum UploadAccess<'a> {
    Plain {
        owner: &'a OwnerContext,       // Пользователь и проект источника.
        generation: SessionGeneration, // Точный вход открытого композера.
    },
    Encrypted(&'a EncryptionActivation), // Активный ключевой контекст защищённого композера.
}

/// Проверенная тема, приём результата из которой зависит от версии доступа и ключа.
struct UploadDestination<'a> {
    scope: HistoryScope, // Подтверждённая точная тема.
    key: Option<(&'a ChatEncryption, MessageDecryption)>, // Отсутствует у открытой отправки.
}

impl UploadDestination<'_> {
    /// Выполняет короткую проверку или приём результата под допуском исходного ключа.
    fn verify<T>(
        &self,
        action: impl FnOnce() -> Result<T, UploadAttachmentError>,
    ) -> Result<T, UploadAttachmentError> {
        if let Some((profile, key)) = &self.key {
            profile
                .keys
                .authorize_outgoing(key, false, action)
                .map_err(EncryptedMessageError::from)?
        } else {
            action()
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
        self.upload_attachment_with_control(
            sync,
            owner,
            generation,
            scope,
            upload,
            AttachmentUploadControl {
                cancel,
                progress: None,
            },
        )
        .await
    }

    /// Загружает файл обычного композера с наблюдением за байтами и отменой.
    pub async fn upload_attachment_with_control(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        upload: AttachmentUpload,
        control: AttachmentUploadControl,
    ) -> Result<UploadedAttachment, UploadAttachmentError> {
        self.upload_attachment_inner(
            sync,
            UploadAccess::Plain { owner, generation },
            scope,
            upload,
            control,
        )
        .await
    }

    /// Загружает файл с ACL стрима после проверки точной темы и действующего ключа.
    pub async fn upload_encrypted_attachment(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
        upload: AttachmentUpload,
        control: AttachmentUploadControl,
    ) -> Result<UploadedAttachment, UploadAttachmentError> {
        self.upload_attachment_inner(
            sync,
            UploadAccess::Encrypted(activation),
            scope,
            upload,
            control,
        )
        .await
    }

    /// Убирает подтверждённый несохранённый файл одной попыткой в его исходной сессии.
    pub async fn discard_uploaded_attachment(
        &self,
        sync: &Sync,
        upload: UploadedAttachment,
    ) -> Result<(), DiscardAttachmentError> {
        if !upload
            .authority
            .ptr_eq(&Arc::downgrade(sync.media_authority()))
        {
            return Err(AuthError::SessionUnavailable.into());
        }
        discard(&self.rest, sync.auth(), upload).await
    }

    /// Загружает один снимок байтов и выдаёт квитанцию только при действующем доступе.
    async fn upload_attachment_inner(
        &self,
        sync: &Sync,
        access: UploadAccess<'_>,
        scope: HistoryScope,
        upload: AttachmentUpload,
        control: AttachmentUploadControl,
    ) -> Result<UploadedAttachment, UploadAttachmentError> {
        let (owner, generation) = match &access {
            UploadAccess::Plain { owner, generation } => (*owner, *generation),
            UploadAccess::Encrypted(activation) => {
                let session = activation.session();
                (&session.owner, session.generation)
            }
        };
        let AttachmentUploadControl { cancel, progress } = control;
        let auth = sync.auth();
        let storage = sync.storage();
        let operation = Uuid::new_v4();
        let bytes = upload.bytes.len();
        log::debug!("Загрузка вложения: op_id={operation} status=started bytes={bytes}");
        let attempt = async {
            let prepared = tokio::task::spawn_blocking(move || prepare(upload))
                .await
                .map_err(|_| UploadAttachmentError::InvalidInput)??;
            let initial = auth.with_active_generation(owner, generation, || {
                storage.media_access_revision(owner, scope)
            })??;
            let destination = self.resolve_upload_destination(sync, access, scope).await?;
            let token = auth
                .access_token_for_generation(owner, Service::Messenger, generation)
                .await?;
            let version = destination.verify(|| {
                auth.with_active_generation(owner, generation, || {
                    if storage.media_access_revision(owner, scope)? != initial {
                        return Err(DirectSendError::AccessLost.into());
                    }
                    Ok(storage.media_access_revision(owner, destination.scope)?)
                })?
            })?;
            log::debug!("Загрузка вложения: op_id={operation} status=request");
            let (id, markdown) = self
                .rest
                .multipart_attachment(
                    owner,
                    &token,
                    destination.scope.stream_id,
                    prepared,
                    progress,
                )
                .await?;
            Ok((
                UploadedAttachment {
                    id,
                    owner: owner.clone(),
                    generation,
                    authority: Arc::downgrade(sync.media_authority()),
                    markdown,
                },
                destination,
                version,
            ))
        };
        let result = tokio::select! {
            biased;
            _ = canceled(cancel) => Err(UploadAttachmentError::Canceled),
            _ = inactive_session(auth, owner, generation) => Err(AuthError::SessionUnavailable.into()),
            result = attempt => result,
        };
        let result = result.and_then(|(upload, destination, version)| {
            let current = destination.verify(|| {
                auth.with_active_generation(owner, generation, || {
                    if storage.media_access_revision(owner, destination.scope)? != version {
                        return Err(DirectSendError::AccessLost.into());
                    }
                    Ok(())
                })?
            });
            if let Err(error) = current {
                // Сервер успел подтвердить файл, но редактор больше не может принять ссылку.
                let rest = self.rest.clone();
                let auth = auth.clone();
                tokio::spawn(async move {
                    let _ = discard(&rest, &auth, upload).await;
                });
                return Err(error);
            }
            Ok(upload)
        });
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
            Err(UploadAttachmentError::Encryption(_)) => "invalid_key",
            Err(UploadAttachmentError::Session(_)) => "session_ended",
            Err(UploadAttachmentError::Storage(_)) => "storage_error",
        };
        log::debug!("Загрузка вложения: op_id={operation} status={status}");
        result
    }

    /// Подтверждает режим, стрим и точную тему до отправки файла.
    async fn resolve_upload_destination<'a>(
        &'a self,
        sync: &Sync,
        access: UploadAccess<'_>,
        scope: HistoryScope,
    ) -> Result<UploadDestination<'a>, UploadAttachmentError> {
        let (topic_id, key) = match access {
            UploadAccess::Plain { owner, generation } => {
                let checked = self
                    .resolve_plain_send(sync, owner, generation, scope)
                    .await?;
                let topic = checked.topic_id();
                // Личка без явно выбранной темы сначала вернула UUID темы по умолчанию.
                if scope.topic_id.is_none() {
                    self.check_topic_send(sync, owner, generation, scope.stream_id, topic)
                        .await?;
                }
                (topic, None)
            }
            UploadAccess::Encrypted(activation) => {
                let profile = self
                    .encryption_profile(activation)
                    .map_err(EncryptedMessageError::from)?;
                let (topic, key) = self
                    .resolve_encrypted_send(sync, profile, activation, scope, None)
                    .await?;
                (topic, Some((profile, key)))
            }
        };
        Ok(UploadDestination {
            scope: HistoryScope {
                topic_id: Some(topic_id),
                ..scope
            },
            key,
        })
    }
}

/// Делает единственную попытку очистки и связывает её в журнале с отдельной операцией.
async fn discard(
    rest: &RestClient,
    auth: &Auth,
    upload: UploadedAttachment,
) -> Result<(), DiscardAttachmentError> {
    let UploadedAttachment {
        id,
        owner,
        generation,
        ..
    } = upload;
    let operation = Uuid::new_v4();
    log::debug!("Очистка вложения: op_id={operation} status=started");
    let attempt = async {
        let token = auth
            .access_token_for_generation(&owner, Service::Messenger, generation)
            .await?;
        let result = rest.delete_attachment(&owner, &token, id).await;
        auth.with_active_generation(&owner, generation, || ())?;
        result
    };
    let result = tokio::select! {
        biased;
        _ = inactive_session(auth, &owner, generation) => Err(AuthError::SessionUnavailable.into()),
        result = attempt => result,
    };
    let status = match &result {
        Ok(()) => "discarded",
        Err(DiscardAttachmentError::Rejected(_)) => "rejected",
        Err(DiscardAttachmentError::Unknown) => "unknown",
        Err(DiscardAttachmentError::Session(_)) => "session_ended",
    };
    log::debug!("Очистка вложения: op_id={operation} status={status}");
    result
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
        let markdown = upload.receipt.markdown(id);
        assert!(markdown.starts_with("!["));
        let [parsed]: [_; 1] = media_references(&markdown).try_into().unwrap();
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
