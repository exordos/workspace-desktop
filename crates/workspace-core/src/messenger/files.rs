//! Открывает вложения из известной истории, объединяет загрузки и выдаёт управляемое чтение.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    sync::{Arc, Mutex, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use futures::StreamExt;
use reqwest::{Response, header};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{sync::mpsc, task::spawn_blocking};
use uuid::Uuid;

use crate::{
    auth::{Auth, AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    storage::{
        StorageError,
        media::{
            MediaCache, MediaCacheError, MediaHistoryItem, MediaKey, MediaMetadata, MediaReader,
            MediaSource, MediaTempFile, MediaVariant, MediaWriteFence,
        },
    },
    sync::Sync,
};

use super::{MessengerSource, history::MediaAccess, rest::RestClient};

/// Причина отказа при открытии вложения.
#[derive(Debug, Error)]
pub enum OpenMediaError {
    #[error("no attachment cache has been configured for this profile")]
    Unconfigured, // У профиля отсутствует каталог кэша.
    #[error("attachment is unavailable")]
    Unavailable, // Ссылка недействительна или сервер отозвал доступ.
    #[error("attachment response was incomplete or failed verification")]
    InvalidBody, // Размер или хэш полной копии отличается от ответа сервера.
    #[error(transparent)]
    Session(#[from] AuthError), // Вход сменился, завершился или не выдал токен.
    #[error(transparent)]
    Cache(#[from] MediaCacheError), // Локальная запись изменилась или пришёл сбой хранилища.
    #[error(transparent)]
    Source(#[from] SourceError), // HTTPS не вернул пригодный ответ.
}

impl From<StorageError> for OpenMediaError {
    /// Выдаёт сбой общей базы как ошибку локального кэша.
    fn from(error: StorageError) -> Self {
        MediaCacheError::from(error).into()
    }
}

/// Читает завершённую копию через действующую сессию и проверенную ссылку.
pub struct MediaFile {
    auth: Auth,                    // Авторизация профиля остаётся доступной в фоновом потоке.
    owner: OwnerContext,           // Владелец открытого объекта.
    generation: SessionGeneration, // Сессия, которой разрешено чтение.
    reader: MediaReader,           // Дескриптор и версии ссылки и физической записи.
}

impl MediaFile {
    /// Возвращает длину завершённой копии.
    pub fn size(&self) -> u64 {
        self.reader.size()
    }

    /// Возвращает сведения, полученные при авторизованной загрузке.
    pub fn metadata(&self) -> &MediaMetadata {
        self.reader.metadata()
    }
}

impl Read for MediaFile {
    /// Передаёт порцию байтов только пока сессия и копия действительны.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let auth = &self.auth;
        let revoked = |_| io::Error::new(io::ErrorKind::PermissionDenied, "media session revoked");
        auth.with_active_generation(&self.owner, self.generation, || ())
            .map_err(revoked)?;
        // Диск читаем вне блокировки сессии, затем выдаём только проверенную порцию.
        let mut chunk = [0; 64 * 1024];
        let limit = buffer.len().min(chunk.len());
        let size = self.reader.read(&mut chunk[..limit])?;
        auth.with_active_generation(&self.owner, self.generation, || {
            if !self.reader.is_current().map_err(io::Error::other)? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "media copy revoked",
                ));
            }
            buffer[..size].copy_from_slice(&chunk[..size]);
            Ok(size)
        })
        .map_err(revoked)?
    }
}

/// Ключ работающей загрузки в памяти одного профиля.
type LoadKey = (OwnerContext, u64, MediaKey);

/// Общая точка ожидания для одновременных открытий одного варианта.
type LoadGate = tokio::sync::Mutex<()>;

/// Позволяет одной загрузке наполнить кэш для всех потребителей текущего входа.
#[derive(Default)]
pub(crate) struct MediaLoads(Mutex<HashMap<LoadKey, Weak<LoadGate>>>);

impl MediaLoads {
    /// Возвращает тот же затвор для повторного запроса, пока им кто-то пользуется.
    fn gate(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        key: MediaKey,
    ) -> Arc<LoadGate> {
        let mut gates = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let key = (owner.clone(), generation.0, key);
        if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
            return gate;
        }
        gates.retain(|_, gate| gate.strong_count() != 0);
        let gate = Arc::new(LoadGate::default());
        gates.insert(key, Arc::downgrade(&gate));
        gate
    }
}

impl MessengerSource {
    /// Открывает вложение по допуску истории, не подтверждая разговор повторным HTTP-запросом.
    pub async fn open_media(
        &self,
        sync: &Sync,
        access: &MediaAccess,
        message_id: Uuid,
        file_id: Uuid,
    ) -> Result<MediaFile, OpenMediaError> {
        let source = MediaSource {
            scope: access.scope,
            message_id,
        };
        self.open_cached_media(sync, access, source, file_id, None)
            .await
    }

    /// Открывает оригинал через точный источник записи выбранной галереи.
    pub async fn open_gallery_media(
        &self,
        sync: &Sync,
        access: &MediaAccess,
        whole_stream: bool,
        item: &MediaHistoryItem,
    ) -> Result<MediaFile, OpenMediaError> {
        let source = gallery_source(access, whole_stream, item)?;
        let current =
            sync.auth()
                .with_active_generation(&access.owner, access.generation, || {
                    access.verify_gallery(sync, None)
                })??;
        self.open_cached_media(sync, access, source, item.file_id, Some(current))
            .await
    }

    /// Открывает уже сохранённые байты, оставляя отсутствие копии без сетевого запроса.
    pub fn open_gallery_cached_media(
        &self,
        sync: &Sync,
        access: &MediaAccess,
        whole_stream: bool,
        item: &MediaHistoryItem,
    ) -> Result<Option<MediaFile>, OpenMediaError> {
        let source = gallery_source(access, whole_stream, item)?;
        let cache = sync.media_cache().ok_or(OpenMediaError::Unconfigured)?;
        let key = MediaKey {
            file_id: item.file_id,
            variant: MediaVariant::Original,
        };
        sync.auth()
            .with_active_generation(&access.owner, access.generation, || {
                access.verify(sync)?;
                let fence = cache.prepare(&access.owner, source, key)?;
                Ok(cache
                    .open_fenced(&fence, unix_seconds())?
                    .map(|reader| MediaFile::new(sync, access, reader)))
            })?
    }

    /// Открывает готовую копию или загружает оригинал, пока ссылка и сессия действительны.
    pub(super) async fn open_cached_media(
        &self,
        sync: &Sync,
        access: &MediaAccess,
        source: MediaSource,
        file_id: Uuid,
        gallery: Option<i64>,
    ) -> Result<MediaFile, OpenMediaError> {
        let owner = &access.owner;
        let generation = access.generation;
        let cache = sync.media_cache().ok_or(OpenMediaError::Unconfigured)?;
        let key = MediaKey {
            file_id,
            variant: MediaVariant::Original,
        };
        let fence = sync.auth().with_active_generation(
            owner,
            generation,
            || -> Result<_, OpenMediaError> {
                verify_access(sync, access, gallery)?;
                match cache.prepare(owner, source, key) {
                    Err(MediaCacheError::Unavailable) => {
                        // Старый профиль индексирует запрошенное сообщение при первом открытии вложения.
                        let storage = sync.storage();
                        let history_generation = storage.media_history_generation(owner)?;
                        if !storage.index_media_history(
                            owner,
                            source.scope,
                            history_generation,
                            Some(source.message_id),
                        )? {
                            return Err(OpenMediaError::Unavailable);
                        }
                        Ok(cache.prepare(owner, source, key)?)
                    }
                    result => Ok(result?),
                }
            },
        )??;
        let gate = sync.media_loads.gate(owner, generation, key);
        let _guard = gate.lock().await;
        let operation = Uuid::new_v4();
        let open = || {
            sync.auth().with_active_generation(owner, generation, || {
                verify_access(sync, access, gallery)?;
                Ok::<_, OpenMediaError>(cache.open_fenced(&fence, unix_seconds())?)
            })?
        };
        let reader = match open()? {
            Some(reader) => {
                log::debug!("Кэш вложений: op_id={operation} status=hit");
                reader
            }
            None => {
                log::debug!("Кэш вложений: op_id={operation} status=fetch");
                let received =
                    finish_download(&self.rest, sync, cache, access, key, &fence, gallery).await;
                if received.is_err() {
                    log::debug!("Кэш вложений: op_id={operation} status=fetch_failed");
                }
                received?;
                let ready = open()?.ok_or(OpenMediaError::Unavailable)?;
                let cleanup = cache.clone();
                spawn_blocking(move || {
                    if let Err(error) = cleanup.enforce_budget() {
                        log::warn!("Не удалось вытеснить копии вложений: {error}");
                    }
                });
                log::debug!(
                    "Кэш вложений: op_id={operation} status=stored bytes={}",
                    ready.size()
                );
                ready
            }
        };
        Ok(MediaFile::new(sync, access, reader))
    }
}

impl MediaFile {
    /// Связывает читатель байтов с профилем и поколением, которым выдан допуск.
    fn new(sync: &Sync, access: &MediaAccess, reader: MediaReader) -> Self {
        Self {
            auth: sync.auth().clone(),
            owner: access.owner.clone(),
            generation: access.generation,
            reader,
        }
    }
}

/// Принимает запись из текущей темы либо из другой темы выбранного стрима.
fn gallery_source(
    access: &MediaAccess,
    whole_stream: bool,
    item: &MediaHistoryItem,
) -> Result<MediaSource, OpenMediaError> {
    let panel_scope = access.gallery_scope(whole_stream);
    if item.scope.stream_id != panel_scope.stream_id
        || panel_scope
            .topic_id
            .is_some_and(|topic| Some(topic) != item.scope.topic_id)
    {
        return Err(OpenMediaError::Unavailable);
    }
    Ok(MediaSource {
        scope: item.scope,
        message_id: item.message_id,
    })
}

/// Проверяет версию доступа темы, а у галереи ещё и поколение всей истории.
fn verify_access(
    sync: &Sync,
    access: &MediaAccess,
    gallery: Option<i64>,
) -> Result<(), OpenMediaError> {
    if gallery.is_some() {
        access.verify_gallery(sync, gallery).map(|_| ())
    } else {
        access.verify(sync)
    }
}

/// Отслеживает отзыв во время передачи и публикует только ответ действующей попытки.
async fn finish_download(
    rest: &RestClient,
    sync: &Sync,
    cache: &MediaCache,
    access: &MediaAccess,
    key: MediaKey,
    fence: &MediaWriteFence,
    gallery: Option<i64>,
) -> Result<(), OpenMediaError> {
    let auth = sync.auth();
    let owner = &access.owner;
    let generation = access.generation;
    let mut sessions = std::pin::pin!(auth.subscribe());
    let mut changes = sync.media_changes();
    let check_current = || {
        auth.with_active_generation(owner, generation, || -> Result<_, OpenMediaError> {
            verify_access(sync, access, gallery)?;
            if !cache.is_current(fence)? {
                return Err(MediaCacheError::Stale.into());
            }
            Ok(())
        })?
    };
    check_current()?;
    let transfer = async {
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let response = rest.media_download(owner, &token, key.file_id).await?;
        match response {
            Some(response) => Ok(Some(receive_original(cache.clone(), response).await?)),
            None => Ok::<_, OpenMediaError>(None),
        }
    };
    let mut transfer = std::pin::pin!(transfer);
    let completed = loop {
        tokio::select! {
            result = &mut transfer => break result?,
            _ = sessions.next() => check_current()?,
            _ = changes.changed() => check_current()?,
        }
    };
    let Some((staged, metadata)) = completed else {
        auth.with_active_generation(owner, generation, || {
            verify_access(sync, access, gallery)?;
            Ok::<_, OpenMediaError>(cache.reject(fence)?)
        })??;
        sync.note_media_change();
        return Err(OpenMediaError::Unavailable);
    };
    auth.with_active_generation(owner, generation, || {
        verify_access(sync, access, gallery)?;
        Ok::<_, OpenMediaError>(cache.publish(fence.clone(), staged, metadata, unix_seconds())?)
    })??;
    Ok(())
}

/// Принимает порции без хранения целого оригинала в памяти и готовит закрытый файл.
async fn receive_original(
    cache: MediaCache,
    mut response: Response,
) -> Result<(MediaTempFile, MediaMetadata), OpenMediaError> {
    let limit = cache.max_object_bytes();
    let expected_size = response.content_length();
    if expected_size.is_some_and(|size| size > limit) {
        return Err(MediaCacheError::TooLarge.into());
    }
    let headers = response.headers();
    let expected_sha = headers
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix('"')?.strip_suffix('"'))
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase);
    let metadata = MediaMetadata {
        name: headers
            .get(header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .and_then(attachment_name),
        mime: headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        file_stream: None,
    };
    let (sender, mut receiver) = mpsc::channel::<Box<[u8]>>(2);
    let writer = spawn_blocking(move || -> Result<_, MediaCacheError> {
        let mut staged = cache.stage()?;
        let mut digest = Sha256::new();
        while let Some(chunk) = receiver.blocking_recv() {
            staged.write_all(&chunk)?;
            if expected_sha.is_some() {
                digest.update(&chunk);
            }
        }
        let matching_hash =
            expected_sha.is_none_or(|expected| format!("{:x}", digest.finalize()) == expected);
        staged.seal()?;
        Ok((staged, matching_hash))
    });
    let mut size = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| OpenMediaError::InvalidBody)?
    {
        size = size.saturating_add(chunk.len() as u64);
        if size > limit {
            return Err(MediaCacheError::TooLarge.into());
        }
        if sender.send(Box::from(&chunk[..])).await.is_err() {
            break;
        }
    }
    drop(sender);
    let (staged, matching_hash) = writer.await.map_err(|_| OpenMediaError::InvalidBody)??;
    if expected_size.is_some_and(|expected| expected != staged.size())
        || !matching_hash
        || size != staged.size()
    {
        return Err(OpenMediaError::InvalidBody);
    }
    Ok((staged, metadata))
}

/// Выделяет безопасную подпись из резервного имени ответа файлового API.
fn attachment_name(disposition: &str) -> Option<String> {
    let name = disposition
        .split(';')
        .find_map(|part| part.trim().strip_prefix("filename="))?;
    let name = name.trim_matches('"');
    if name.is_empty()
        || name
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '/' | '\\'))
    {
        return None;
    }
    Some(name.to_owned())
}

/// Возвращает время обращения для сортировки недавно использованных копий.
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_secs())
}
