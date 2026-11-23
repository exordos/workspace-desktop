//! Открывает вложение подтверждённого зашифрованного сообщения.
//! Сверяет ссылку в расшифрованном теле и держит временные байты только до конца чтения.

use std::{fs::File, io, io::Read, pin::pin, sync::Arc};

use futures::StreamExt;
use uuid::Uuid;

use crate::{
    auth::{ActiveGeneration, Service},
    context::OwnerContext,
    messenger::{
        MessengerSource,
        encryption::{EncryptionActivation, MessageReadPermission},
        history::{MediaAccess, OpenHistoryError},
        quote::{ProtectedQuote, ProtectedQuoteMediaPermit, QuoteSourceFence},
        references::media_references,
    },
    storage::{
        HistoryScope, MessageBody, Storage,
        media::{MediaMetadata, MediaTempFile},
    },
    sync::{ProtectedFilePermit, QuoteOriginPermit, Sync},
};

use super::{MediaFile, MediaFileReader, OpenMediaError, receive_original};

/// Байты в файле без имени и без индекса ссылок, привязанные к разрешению оригинала.
pub(super) struct ProtectedMediaFile {
    storage: Arc<Storage>,          // Локальные версии истории и доступа.
    source: ProtectedMediaSource,   // Исходная строка и её связь с обычным файлом.
    permit: ProtectedPreviewPermit, // Доступ к файлу, исходнику и цитирующей строке, если она есть.
    metadata: MediaMetadata,        // Имя и тип, полученные из обычного файлового API.
    reader: File,                   // Дескриптор принятой копии.
    staged: MediaTempFile,          // Удаляет временный путь при освобождении.
}

/// Разрешение получено либо от строки в открытой истории, либо от подтверждённой цитаты.
enum FileReadPermission {
    History {
        origin: Box<QuoteOriginPermit>, // Точная версия сообщения с вложением.
        key: MessageReadPermission,     // Локальный ключ, которым прочитали его тело.
    },
    Quote(Box<ProtectedQuoteMediaPermit>), // Версии сообщений и ключей цитаты и оригинала.
}

impl FileReadPermission {
    /// Сверяет отзыв в памяти внутри уже проверенного входа.
    fn with_current<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        match self {
            Self::History { origin, key } => {
                origin.with_current(|| key.with_current(read)).flatten()
            }
            Self::Quote(quote) => quote.with_current_under_auth(read),
        }
    }
}

/// Разрешение на показ декодированного защищённого вложения после закрытия файла.
#[derive(Clone)]
pub struct ProtectedPreviewPermit(Arc<ProtectedPreviewAccess>); // Общие версии без владения профилем.

/// Версии, которые достаточно сверить в памяти перед отрисовкой.
struct ProtectedPreviewAccess {
    session: ActiveGeneration,  // Поколение Auth без владения авторизацией.
    file: ProtectedFilePermit,  // Версия UUID файла до следующего серверного изменения.
    source: FileReadPermission, // Разрешения на сообщения и ключи, из которых получили ссылку.
}

impl ProtectedPreviewPermit {
    /// Разрешает краткий показ пока действуют вход, файл, сообщения и ключи.
    /// Замыкание не обращается к Core, диску или сети.
    pub fn with_current<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        self.0
            .session
            .with_current(|| self.with_current_under_auth(read))
            .flatten()
    }

    /// Сверяет версии под уже удерживаемой блокировкой Auth.
    fn with_current_under_auth<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        self.0
            .file
            .with_current(|| self.0.source.with_current(read))
            .flatten()
    }
}

/// Версии оригинала и файла перед загрузкой без добавления ссылки в индекс.
struct ProtectedMediaSource {
    scope: HistoryScope,     // Фактическое размещение исходного сообщения.
    message_id: Uuid,        // Исходная строка вложения.
    file_id: Uuid,           // UUID запрашиваемого файла.
    quote: QuoteSourceFence, // Версии оригинала и права чтения.
    file_revision: i64,      // Версия отзыва или замены файла.
}

impl ProtectedMediaSource {
    /// Сверяет правку оригинала и отзыв права файла перед передачей байтов.
    fn verify(&self, storage: &Storage, owner: &OwnerContext) -> Result<(), OpenMediaError> {
        let current = QuoteSourceFence::capture(storage, owner, self.scope, self.message_id)
            .map_err(quote_media_error)?;
        if current == self.quote
            && storage.media_file_revision(owner, self.scope, self.file_id)? == self.file_revision
        {
            return Ok(());
        }
        Err(OpenMediaError::Unavailable)
    }
}

impl ProtectedMediaFile {
    /// Возвращает число принятых байтов.
    pub(super) fn size(&self) -> u64 {
        self.staged.size()
    }

    /// Возвращает сведения обычного файлового ответа.
    pub(super) fn metadata(&self) -> &MediaMetadata {
        &self.metadata
    }

    /// Даёт превью удерживать те же версии после освобождения временной копии.
    pub(super) fn preview_permit(&self) -> Option<ProtectedPreviewPermit> {
        self.permit.with_current(|| self.permit.clone())
    }

    /// Разрешает короткую передачу или публикацию, пока действуют оригинал и ключ.
    pub(super) fn with_current<T>(
        &self,
        owner: &OwnerContext,
        read: impl FnOnce() -> T,
    ) -> io::Result<T> {
        let revoked = || {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "protected attachment access revoked",
            )
        };
        self.source
            .verify(&self.storage, owner)
            .map_err(|_| revoked())?;
        self.permit
            .with_current_under_auth(read)
            .ok_or_else(revoked)
    }
}

impl Read for ProtectedMediaFile {
    /// Читает порцию перед проверкой и передачей наружу в общем владельце файла.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buffer)
    }
}

impl MessengerSource {
    /// Открывает вложение из точного зашифрованного оригинала через обычный файловый API.
    /// Ссылки и метаданные Markdown не добавляются в локальный индекс.
    pub async fn open_protected_media(
        &self,
        sync: &Sync,
        access: &MediaAccess,
        activation: &EncryptionActivation,
        message_id: Uuid,
        file_id: Uuid,
    ) -> Result<MediaFile, OpenMediaError> {
        self.load_protected_media(
            sync,
            access,
            activation,
            Some(access.scope),
            (message_id, file_id),
            None,
        )
        .await
    }

    /// Открывает файл подтверждённой цитаты и удерживает обе версии до передачи или сохранения.
    pub async fn open_linked_protected_quote_media(
        &self,
        sync: &Sync,
        access: &MediaAccess,
        activation: &EncryptionActivation,
        quote: &ProtectedQuote,
        file_id: Uuid,
    ) -> Result<MediaFile, OpenMediaError> {
        let permission = quote
            .media_permit(activation, file_id)
            .ok_or(OpenMediaError::Unavailable)?;
        let citing_scope = permission.citing_scope();
        if citing_scope.stream_id != access.scope.stream_id
            || access
                .scope
                .topic_id
                .is_some_and(|topic| Some(topic) != citing_scope.topic_id)
        {
            return Err(OpenMediaError::Unavailable);
        }
        self.load_protected_media(
            sync,
            access,
            activation,
            None,
            (permission.source_message_id(), file_id),
            Some(permission),
        )
        .await
    }

    /// Сверяет ссылку в исходном зашифрованном теле и получает обычные байты файла.
    async fn load_protected_media(
        &self,
        sync: &Sync,
        access: &MediaAccess,
        activation: &EncryptionActivation,
        expected_scope: Option<HistoryScope>,
        (message_id, file_id): (Uuid, Uuid),
        quoting: Option<ProtectedQuoteMediaPermit>,
    ) -> Result<MediaFile, OpenMediaError> {
        let session = activation.session();
        if session.owner != access.owner
            || session.generation != access.generation
            || file_id.is_nil()
        {
            return Err(OpenMediaError::Unavailable);
        }
        let profile = self
            .encryption_profile(activation)
            .map_err(|_| OpenMediaError::Unavailable)?;
        let cache = sync.media_cache().ok_or(OpenMediaError::Unconfigured)?;
        let auth = sync.auth();
        let owner = &access.owner;
        let generation = access.generation;
        auth.with_active_generation(owner, generation, || access.verify(sync))??;
        let mut keys = profile.keys.subscribe();
        let mut selection = profile.keys.observe_selection();
        let (original, fence) = self
            .quote_original_with_fence(sync, owner, generation, expected_scope, message_id)
            .await
            .map_err(quote_media_error)?;
        let scope = HistoryScope {
            stream_id: original.stream_id,
            topic_id: Some(original.topic_id),
        };
        let (source, file, direct_origin) =
            auth.with_active_generation(owner, generation, || -> Result<_, OpenMediaError> {
                access.verify(sync)?;
                let storage = sync.storage();
                if QuoteSourceFence::capture(storage, owner, scope, message_id)
                    .map_err(quote_media_error)?
                    != fence
                    || !storage.quote_source_matches(owner, &original)?
                {
                    return Err(OpenMediaError::Unavailable);
                }
                let direct_origin = match quoting.as_ref() {
                    Some(quote) if quote.matches_source(&original) => None,
                    Some(_) => return Err(OpenMediaError::Unavailable),
                    None => Some(
                        sync.capture_quote_origin(owner, generation, &original)
                            .ok_or(OpenMediaError::Unavailable)?,
                    ),
                };
                let source = ProtectedMediaSource {
                    scope,
                    message_id,
                    file_id,
                    quote: fence,
                    file_revision: storage.media_file_revision(owner, scope, file_id)?,
                };
                // Версия файла и разрешение относятся к одной завершённой записи истории.
                let file = sync.capture_protected_file(owner, file_id);
                Ok((source, file, direct_origin))
            })??;
        let permission = match (quoting, direct_origin) {
            (Some(quote), _) => FileReadPermission::Quote(Box::new(quote)),
            (None, Some(origin)) => {
                let MessageBody::Encrypted { key_id, ciphertext } = original.body else {
                    return Err(OpenMediaError::Unavailable);
                };
                let engine = Arc::clone(&profile.keys);
                let activation = activation.clone();
                let key = tokio::task::spawn_blocking(move || {
                    let prepared = engine.prepare(&activation, scope.stream_id, key_id).ok()?;
                    let accepted = engine.accept(prepared.decrypt(&ciphertext)).ok()?;
                    accepted
                        .with_body(|markdown| {
                            media_references(markdown)
                                .iter()
                                .any(|reference| reference.file_id == file_id)
                        })?
                        .then(|| accepted.into_permission())
                })
                .await
                .map_err(|_| OpenMediaError::Unavailable)?
                .ok_or(OpenMediaError::Unavailable)?;
                FileReadPermission::History {
                    origin: Box::new(origin),
                    key,
                }
            }
            (None, None) => return Err(OpenMediaError::Unavailable),
        };
        let permit = ProtectedPreviewPermit(Arc::new(ProtectedPreviewAccess {
            session: auth.active_generation(owner, generation),
            file,
            source: permission,
        }));
        let check = || {
            auth.with_active_generation(owner, generation, || {
                access.verify(sync)?;
                source.verify(sync.storage(), owner)?;
                if permit.with_current_under_auth(|| ()).is_none() {
                    return Err(OpenMediaError::Unavailable);
                }
                Ok(())
            })?
        };

        let mut sessions = pin!(auth.subscribe());
        let mut history_changes = sync.media_changes();
        check()?;
        let transfer = async {
            let token = auth
                .access_token_for_generation(owner, Service::Messenger, generation)
                .await?;
            check()?;
            let response = self
                .rest
                .media_download(owner, &token, file_id)
                .await?
                .ok_or(OpenMediaError::Unavailable)?;
            receive_original(cache.clone(), response).await
        };
        let (staged, metadata) = {
            let mut transfer = pin!(transfer);
            loop {
                tokio::select! {
                    result = &mut transfer => break result?,
                    _ = keys.recv() => check()?,
                    _ = selection.changed() => check()?,
                    _ = sessions.next() => check()?,
                    _ = history_changes.changed() => check()?,
                }
            }
        };
        check()?;
        let reader = staged.open_read()?;
        Ok(MediaFile {
            auth: auth.clone(),
            owner: owner.clone(),
            generation,
            reader: MediaFileReader::Protected(ProtectedMediaFile {
                storage: Arc::clone(sync.storage()),
                source,
                permit,
                metadata,
                reader,
                staged,
            }),
        })
    }
}

/// Сохраняет причину ошибки авторизации и не раскрывает детали исходного сообщения.
fn quote_media_error(error: OpenHistoryError) -> OpenMediaError {
    match error {
        OpenHistoryError::Auth(error) => error.into(),
        OpenHistoryError::Source(error) => error.into(),
        _ => OpenMediaError::Unavailable,
    }
}
