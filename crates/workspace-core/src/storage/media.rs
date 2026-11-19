//! Хранит индекс вложений и копий, управляет записью и удалением локальных файлов.
//! Запросы для каталога и сводки вынесены в соседний модуль.

mod queries;
#[cfg(test)]
mod tests;

pub use queries::{
    MediaClearReport, MediaCopy, MediaCopyCursor, MediaCopyPage, MediaCopyQuery, MediaCopySort,
    MediaEstimate, MediaFilter, MediaHistoryCursor, MediaHistoryItem, MediaHistoryPage,
    MediaReapReport,
};

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;
use uuid::Uuid;

use crate::context::OwnerContext;

use super::{HistoryScope, Storage, StorageError};

/// Вид вложения для фильтров.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i64)]
pub enum MediaKind {
    File,  // Документ или другой файл без специального просмотра.
    Image, // Фото или изображение.
    Video, // Видео.
    Audio, // Аудиофайл.
}

impl MediaKind {
    /// Читает вид, который хранилище записало в SQLite.
    fn read(value: i64) -> rusqlite::Result<Self> {
        match value {
            0 => Ok(Self::File),
            1 => Ok(Self::Image),
            2 => Ok(Self::Video),
            3 => Ok(Self::Audio),
            _ => Err(rusqlite::Error::IntegralValueOutOfRange(0, value)),
        }
    }
}

/// Вид сохранённых байтов одного файла.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(i64)]
pub enum MediaVariant {
    Original, // Полный оригинал из файлового API.
}

/// Ключ копии внутри контекста владельца.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MediaKey {
    pub file_id: Uuid,         // UUID файла в Workspace.
    pub variant: MediaVariant, // Вариант локальной копии.
}

/// Прямая ссылка, выделенная из поддерживаемой разметки сообщения.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MediaLink {
    pub file_id: Uuid,   // UUID цели из Markdown.
    pub kind: MediaKind, // Вид из ссылки.
}

/// Где пользователь открывает вложение.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MediaSource {
    pub scope: HistoryScope, // Доступный стрим или тема.
    pub message_id: Uuid,    // Сообщение с прямой ссылкой на файл.
}

/// Дополнительные сведения о локальной копии.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaMetadata {
    pub name: Option<String>,      // Имя для отображения, если известно.
    pub mime: Option<String>,      // MIME из авторизованного ответа, если известен.
    pub file_stream: Option<Uuid>, // Стрим ACL файла, если известен.
}

/// Программные ограничения общего кэша профиля.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaCachePolicy {
    pub budget_bytes: u64,     // Бюджет всех копий вложений в профиле.
    pub max_object_bytes: u64, // Ограничение одной сохраняемой копии.
}

impl Default for MediaCachePolicy {
    /// Возвращает рабочие пределы по умолчанию для обычного профиля.
    fn default() -> Self {
        const MIB: u64 = 1024 * 1024;
        Self {
            budget_bytes: 512 * MIB,
            max_object_bytes: 64 * MIB,
        }
    }
}

/// Ошибка индекса, диска или изменившихся условий доступа.
#[derive(Debug, Error)]
pub enum MediaCacheError {
    #[error(transparent)]
    Storage(#[from] StorageError), // Не удалось прочитать или записать локальный индекс.
    #[error("media cache disk operation failed")]
    Disk(#[source] io::Error), // Сбой ФС, внутренний путь не входит в публичное описание.
    #[error("attachment is unavailable in the current scope")]
    Unavailable, // Исходная ссылка исчезла или отсутствовала.
    #[error("media cache entry changed during the operation")]
    Stale, // После начала работы очистили или отозвали запись.
    #[error("media exceeds the cache object limit")]
    TooLarge, // Оригинал превышает лимит одной записи.
}

impl From<io::Error> for MediaCacheError {
    /// Скрывает подробности пути в сообщении об ошибке.
    fn from(error: io::Error) -> Self {
        Self::Disk(error)
    }
}

impl From<rusqlite::Error> for MediaCacheError {
    /// Передаёт ошибку SQLite через общее хранилище.
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

/// Короткоживущий снимок условий, взятый до сетевого запроса.
#[derive(Clone, Debug)]
pub(crate) struct MediaWriteFence {
    owner: String,       // Кодированный полный OwnerContext.
    scope: HistoryScope, // Фактическое размещение сообщения в истории.
    key: MediaKey,       // Цель, для которой был начат запрос.
    reference: i64,      // Версия ссылки, исчезает при изменении или удалении.
    version: i64,        // Сумма монотонных ревизий владельца, области и файла.
}

/// Данные физической копии и её опознаватель, общий для диска и индекса.
#[derive(Debug)]
struct Object {
    id: Uuid,                // Уникален даже для повторной загрузки прежнего файла.
    size: u64,               // Длина целого файла.
    metadata: MediaMetadata, // Данные из разрешённого ответа.
}

/// Хранилище байтов и число активных читателей на каждой физической копии.
struct Disk {
    root: PathBuf,                        // Закрытый каталог внутри профиля.
    readers: Mutex<HashMap<Uuid, usize>>, // Счётчики открытых файлов.
    clear_claims: Mutex<HashSet<Uuid>>,   // Версии, удаление которых ждёт отдельная команда.
    policy: MediaCachePolicy,             // Граница размера кэша и файла.
}

/// Управляет дисковыми копиями вложений и использует общую SQLite.
#[derive(Clone)]
pub struct MediaCache {
    storage: Arc<Storage>, // Хранилище сообщений и индекса вложений.
    disk: Arc<Disk>,       // Директория и активные читатели этого профиля.
}

/// Временная копия, которую можно публиковать только после успешно принятых байтов.
pub(crate) struct MediaTempFile {
    path: PathBuf,   // Созданный приложением временный файл в каталоге кэша.
    id: Uuid,        // Идентификатор будущей физической копии.
    file: File,      // Открытый файл для входящих порций.
    size: u64,       // Количество уже записанных байтов.
    limit: u64,      // Предельный объём на один файл.
    published: bool, // Запись SQLite подтвердила перемещённый файл.
}

/// Читает готовую копию без раскрытия пути к ней.
pub(crate) struct MediaReader {
    cache: MediaCache,      // Индекс и счётчики читателей.
    fence: MediaWriteFence, // Ссылка, через которую открыта копия.
    object: Object,         // Версия конкретных байтов.
    file: Option<File>,     // Дескриптор до освобождения читателя.
}

/// Создаёт индекс истории, индекс файлов и версии для защиты публикации.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS messenger_media_refs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL, stream TEXT NOT NULL, topic TEXT,
            message TEXT NOT NULL, position INTEGER NOT NULL,
            file TEXT NOT NULL, kind INTEGER NOT NULL, created_at TEXT NOT NULL,
            UNIQUE(owner, message, position)
        );
        CREATE INDEX IF NOT EXISTS messenger_media_refs_scope
            ON messenger_media_refs(owner, stream, created_at DESC, id DESC);
        CREATE INDEX IF NOT EXISTS messenger_media_refs_file
            ON messenger_media_refs(owner, file, stream, topic);
        CREATE TABLE IF NOT EXISTS messenger_media_objects (
            id TEXT PRIMARY KEY, owner TEXT NOT NULL, file TEXT NOT NULL,
            variant INTEGER NOT NULL, kind INTEGER NOT NULL, size INTEGER NOT NULL,
            used_at INTEGER NOT NULL, name TEXT, mime TEXT, file_stream TEXT,
            pending INTEGER NOT NULL DEFAULT 0
        );
        CREATE UNIQUE INDEX IF NOT EXISTS messenger_media_ready_key
            ON messenger_media_objects(owner, file, variant) WHERE pending = 0;
        CREATE INDEX IF NOT EXISTS messenger_media_objects_owner
            ON messenger_media_objects(owner, pending, size, id);
        CREATE INDEX IF NOT EXISTS messenger_media_objects_budget
            ON messenger_media_objects(pending, used_at, id);
        CREATE TABLE IF NOT EXISTS messenger_media_origins (
            object TEXT NOT NULL REFERENCES messenger_media_objects(id) ON DELETE CASCADE,
            stream TEXT NOT NULL, PRIMARY KEY(object, stream)
        );
        CREATE TABLE IF NOT EXISTS messenger_media_versions (
            owner TEXT NOT NULL, file TEXT NOT NULL, version INTEGER NOT NULL,
            PRIMARY KEY(owner, file)
        );",
    )?;
    Ok(())
}

/// Обновляет прямые ссылки сообщения в транзакции применения истории.
pub(crate) fn replace_message_refs(
    connection: &Connection,
    owner: &OwnerContext,
    message_id: Uuid,
    scope: HistoryScope,
    created_at: &str,
    links: &[MediaLink],
) -> Result<(), StorageError> {
    let owner = owner_key(owner);
    let stream = scope.stream_id.to_string();
    let topic = scope.topic_id.map(|id| id.to_string());
    let message = message_id.to_string();
    let mut statement = connection.prepare_cached(
        "SELECT stream, topic, file, kind, created_at FROM messenger_media_refs
         WHERE owner = ?1 AND message = ?2 ORDER BY position",
    )?;
    let previous = statement
        .query_map(params![owner, message], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                MediaKind::read(row.get(3)?)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if previous.len() == links.len()
        && previous.iter().zip(links).all(|(row, link)| {
            row.0 == stream
                && row.1 == topic
                && row.2 == link.file_id.to_string()
                && row.3 == link.kind
                && row.4 == created_at
        })
    {
        return Ok(());
    }
    connection.execute(
        "DELETE FROM messenger_media_refs WHERE owner = ?1 AND message = ?2",
        params![owner, message],
    )?;
    let mut insert = connection.prepare_cached(
        "INSERT INTO messenger_media_refs
         (owner, stream, topic, message, position, file, kind, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for (position, link) in links.iter().enumerate() {
        insert.execute(params![
            owner,
            stream,
            topic,
            message,
            position as i64,
            link.file_id.to_string(),
            link.kind as i64,
            created_at
        ])?;
    }
    Ok(())
}

/// Убирает места появления вложений в удалённом сообщении.
pub(crate) fn remove_message_refs(
    connection: &Connection,
    owner: &OwnerContext,
    message_id: Uuid,
) -> Result<(), StorageError> {
    connection.execute(
        "DELETE FROM messenger_media_refs WHERE owner = ?1 AND message = ?2",
        params![owner_key(owner), message_id.to_string()],
    )?;
    Ok(())
}

/// Убирает ссылки области и закрывает доступ к копиям, затронутым отзывом.
pub(crate) fn revoke_scope(
    connection: &Connection,
    owner: &OwnerContext,
    scope: HistoryScope,
) -> Result<(), StorageError> {
    let owner = owner_key(owner);
    let stream = scope.stream_id.to_string();
    let topic = scope.topic_id.map(|id| id.to_string());
    // Файловый ACL может относиться к другому стриму, который HTTP-ответ не указал.
    bump(connection, &owner, "")?;
    connection.execute(
        "UPDATE messenger_media_objects AS object SET pending = 1
         WHERE owner = ?1 AND pending = 0 AND (
            (?3 IS NULL AND (file_stream = ?2 OR (file_stream IS NULL AND EXISTS (
                SELECT 1 FROM messenger_media_origins AS origin
                WHERE origin.object = object.id AND origin.stream = ?2
            ))))
            OR (
                EXISTS (SELECT 1 FROM messenger_media_refs ref
                    WHERE ref.owner = ?1 AND ref.file = object.file AND ref.stream = ?2
                    AND (?3 IS NULL OR ref.topic = ?3))
                AND NOT EXISTS (SELECT 1 FROM messenger_media_refs ref
                    WHERE ref.owner = ?1 AND ref.file = object.file
                    AND (ref.stream <> ?2 OR (?3 IS NOT NULL AND ref.topic IS NOT ?3)))
            )
         )",
        params![owner, stream, topic],
    )?;
    connection.execute(
        "DELETE FROM messenger_media_refs WHERE owner = ?1 AND stream = ?2
         AND (?3 IS NULL OR topic = ?3)",
        params![owner, stream, topic],
    )?;
    Ok(())
}

/// Инвалидирует варианты файла и старые сетевые ответы только этого владельца.
pub(crate) fn revoke_file(
    connection: &Connection,
    owner: &OwnerContext,
    file_id: Uuid,
) -> Result<(), StorageError> {
    invalidate_file(connection, &owner_key(owner), &file_id.to_string())
}

/// Сбрасывает проекцию и копии владельца после потери истории событий.
pub(crate) fn reset(connection: &Connection, owner: &OwnerContext) -> Result<(), StorageError> {
    let owner = owner_key(owner);
    bump(connection, &owner, "")?;
    connection.execute(
        "DELETE FROM messenger_media_refs WHERE owner = ?1",
        params![owner],
    )?;
    connection.execute(
        "UPDATE messenger_media_objects SET pending = 1 WHERE owner = ?1 AND pending = 0",
        params![owner],
    )?;
    Ok(())
}

/// Превращает полный контекст в однозначный ключ для SQLite.
fn owner_key(owner: &OwnerContext) -> String {
    serde_json::to_string(owner).expect("контекст состоит из строк")
}

/// Увеличивает версию владельца или отдельного файла.
fn bump(connection: &Connection, owner: &str, file: &str) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO messenger_media_versions VALUES (?1, ?2, 1)
         ON CONFLICT(owner, file) DO UPDATE SET version = version + 1",
        params![owner, file],
    )?;
    Ok(())
}

/// Хранит ревизии групп и тем отдельно от UUID файлов.
fn scope_key(stream: Uuid, topic: Option<Uuid>) -> String {
    match topic {
        Some(topic) => format!("scope:{stream}/{topic}"),
        None => format!("scope:{stream}"),
    }
}

/// Суммирует монотонные версии, чтобы за один запрос заметить любое изменение.
fn version(
    connection: &Connection,
    owner: &str,
    file: &str,
    scope: HistoryScope,
) -> rusqlite::Result<i64> {
    connection
        .prepare_cached(
            "SELECT COALESCE(SUM(version), 0) FROM messenger_media_versions
         WHERE owner = ?1 AND file IN ('', ?2, ?3, ?4)",
        )?
        .query_row(
            params![
                owner,
                file,
                scope_key(scope.stream_id, None),
                scope
                    .topic_id
                    .map(|topic| scope_key(scope.stream_id, Some(topic))),
            ],
            |row| row.get(0),
        )
}

/// Откладывает физическое удаление всех копий этого файла.
fn invalidate_file(connection: &Connection, owner: &str, file: &str) -> Result<(), StorageError> {
    bump(connection, owner, file)?;
    connection.execute(
        "UPDATE messenger_media_objects SET pending = 1
         WHERE owner = ?1 AND file = ?2 AND pending = 0",
        params![owner, file],
    )?;
    Ok(())
}

impl MediaCache {
    /// Привязывает кэш к профилю и убирает следы незавершённых операций прошлого запуска.
    pub fn new(
        storage: Arc<Storage>,
        root: PathBuf,
        policy: MediaCachePolicy,
    ) -> Result<Self, MediaCacheError> {
        let mut directory = fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(&root)?;
        let cache = Self {
            storage,
            disk: Arc::new(Disk {
                root,
                readers: Mutex::new(HashMap::new()),
                clear_claims: Mutex::new(HashSet::new()),
                policy,
            }),
        };
        cache.recover()?;
        cache.enforce_budget()?;
        Ok(cache)
    }

    /// Возвращает предельный размер оригинала до начала передачи.
    pub(crate) fn max_object_bytes(&self) -> u64 {
        self.disk
            .policy
            .max_object_bytes
            .min(self.disk.policy.budget_bytes)
            .min(i64::MAX as u64)
    }

    /// Захватывает версию индексированной ссылки и текущие границы очистки.
    pub(crate) fn prepare(
        &self,
        owner: &OwnerContext,
        source: MediaSource,
        key: MediaKey,
    ) -> Result<MediaWriteFence, MediaCacheError> {
        let connection = self.connection()?;
        let owner = owner_key(owner);
        let file = key.file_id.to_string();
        let (reference, topic) = connection
            .query_row(
                "SELECT id, topic FROM messenger_media_refs WHERE owner = ?1 AND stream = ?2
                 AND (?3 IS NULL OR topic = ?3) AND message = ?4 AND file = ?5
                 ORDER BY id LIMIT 1",
                params![
                    owner,
                    source.scope.stream_id.to_string(),
                    source.scope.topic_id.map(|id| id.to_string()),
                    source.message_id.to_string(),
                    file
                ],
                |row| Ok((row.get(0)?, optional_uuid_column(row, 1)?)),
            )
            .optional()?
            .ok_or(MediaCacheError::Unavailable)?;
        let scope = HistoryScope {
            stream_id: source.scope.stream_id,
            topic_id: topic,
        };
        Ok(MediaWriteFence {
            version: version(&connection, &owner, &file, scope)?,
            owner,
            scope,
            key,
            reference,
        })
    }

    /// Проверяет, что после начала операции не изменились ссылка и границы записи.
    pub(crate) fn is_current(&self, fence: &MediaWriteFence) -> Result<bool, MediaCacheError> {
        Ok(fence.is_current(&*self.connection()?)?)
    }

    /// Открывает готовую копию по ранее подтверждённой прямой ссылке.
    pub(crate) fn open_fenced(
        &self,
        fence: &MediaWriteFence,
        now: u64,
    ) -> Result<Option<MediaReader>, MediaCacheError> {
        let connection = self.connection()?;
        if !fence.is_current(&connection)? {
            return Err(MediaCacheError::Stale);
        }
        let Some(object) = connection
            .query_row(
                "SELECT id, size, name, mime, file_stream FROM messenger_media_objects AS candidate
                 WHERE owner = ?1 AND file = ?2 AND variant = ?3 AND pending = 0
                 AND (file_stream IS NOT NULL OR EXISTS (
                     SELECT 1 FROM messenger_media_origins
                     WHERE object = candidate.id AND stream = ?4
                 ))",
                params![
                    fence.owner,
                    fence.key.file_id.to_string(),
                    fence.key.variant as i64,
                    fence.scope.stream_id.to_string()
                ],
                object_row,
            )
            .optional()?
        else {
            return Ok(None);
        };
        let id = object.id;
        let path = self.disk.object_path(id);
        let file = match File::open(path) {
            Ok(file) if file.metadata()?.len() == object.size => file,
            Ok(_) => {
                mark_pending(&connection, id)?;
                return Ok(None);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                connection.execute(
                    "DELETE FROM messenger_media_objects WHERE id = ?1",
                    [id.to_string()],
                )?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        connection.execute(
            "UPDATE messenger_media_objects SET used_at = MAX(used_at, ?2) WHERE id = ?1",
            params![id.to_string(), timestamp(now)],
        )?;
        // Открытый дескриптор держит байты, даже когда запись скрыта в SQLite.
        let mut readers = self
            .disk
            .readers
            .lock()
            .map_err(|_| StorageError::Poisoned)?;
        *readers.entry(id).or_default() += 1;
        Ok(Some(MediaReader {
            cache: self.clone(),
            fence: fence.clone(),
            object,
            file: Some(file),
        }))
    }

    /// Открывает локальную копию по текущей проекции сообщения без сетевой проверки.
    #[cfg(test)]
    pub(crate) fn open(
        &self,
        owner: &OwnerContext,
        source: MediaSource,
        key: MediaKey,
        now: u64,
    ) -> Result<Option<MediaReader>, MediaCacheError> {
        self.open_fenced(&self.prepare(owner, source, key)?, now)
    }

    /// Резервирует уникальный файл для записи сетевых порций.
    pub(crate) fn stage(&self) -> Result<MediaTempFile, MediaCacheError> {
        let id = Uuid::new_v4();
        let path = self.disk.root.join(format!("{id}.part"));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok(MediaTempFile {
            path,
            id,
            file,
            size: 0,
            limit: self.max_object_bytes(),
            published: false,
        })
    }

    /// Публикует законченный оригинал, пока его ссылка и границы очистки ещё действуют.
    pub(crate) fn publish(
        &self,
        fence: MediaWriteFence,
        mut staged: MediaTempFile,
        metadata: MediaMetadata,
        now: u64,
    ) -> Result<(), MediaCacheError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !fence.is_current(&transaction)? {
            return Err(MediaCacheError::Stale);
        }
        let file = fence.key.file_id.to_string();
        let variant = fence.key.variant as i64;
        let existing = transaction
            .query_row(
                "SELECT id FROM messenger_media_objects
                 WHERE owner = ?1 AND file = ?2 AND variant = ?3 AND pending = 0",
                params![fence.owner, file, variant],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let file_stream = metadata.file_stream.map(|stream| stream.to_string());
        let new_copy = existing.is_none();
        let object_id = match existing {
            Some(id) => {
                // Два подтверждённых размещения могут пользоваться одним неизменяемым файлом.
                transaction.execute(
                    "UPDATE messenger_media_objects SET
                     used_at = MAX(used_at, ?2), name = COALESCE(?3, name),
                     mime = COALESCE(?4, mime), file_stream = COALESCE(?5, file_stream)
                     WHERE id = ?1",
                    params![
                        id,
                        timestamp(now),
                        metadata.name,
                        metadata.mime,
                        file_stream
                    ],
                )?;
                id
            }
            None => {
                let id = staged.id.to_string();
                let reference_kind: i64 = transaction.query_row(
                    "SELECT kind FROM messenger_media_refs WHERE id = ?1",
                    [fence.reference],
                    |row| row.get(0),
                )?;
                let kind = type_from_mime(metadata.mime.as_deref())
                    .map_or(reference_kind, |kind| kind as i64);
                let final_path = self.disk.object_path(staged.id);
                fs::rename(&staged.path, &final_path)?;
                // До commit файл остаётся собственностью временной записи и удалится при ошибке.
                staged.path = final_path;
                transaction.execute(
                    "INSERT INTO messenger_media_objects
                     (id, owner, file, variant, kind, size, used_at, name, mime, file_stream)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        id,
                        fence.owner,
                        file,
                        variant,
                        kind,
                        staged.size as i64,
                        timestamp(now),
                        metadata.name,
                        metadata.mime,
                        file_stream
                    ],
                )?;
                id
            }
        };
        transaction.execute(
            "INSERT OR IGNORE INTO messenger_media_origins(object, stream) VALUES (?1, ?2)",
            params![object_id, fence.scope.stream_id.to_string()],
        )?;
        transaction.commit()?;
        staged.published = new_copy;
        Ok(())
    }

    /// Применяет подтверждённый сетевой отказ только к ещё актуальному запросу.
    pub(crate) fn reject(&self, fence: &MediaWriteFence) -> Result<(), MediaCacheError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if fence.is_current(&transaction)? {
            invalidate_file(&transaction, &fence.owner, &fence.key.file_id.to_string())?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Восстанавливает индекс и убирает служебные файлы после остановки приложения.
    fn recover(&self) -> Result<(), MediaCacheError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT id FROM messenger_media_objects")?;
        let entries = statement
            .query_map([], |row| uuid_column(row, 0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        for &id in &entries {
            if let Err(error) = fs::metadata(self.disk.object_path(id)) {
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error.into());
                }
                connection.execute(
                    "DELETE FROM messenger_media_objects WHERE id = ?1",
                    [id.to_string()],
                )?;
            }
        }
        for file in fs::read_dir(&self.disk.root)? {
            let file = file?;
            let path = file.path();
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|text| Uuid::parse_str(text).ok());
            if let Some(id) = id {
                let suffix = path.extension().and_then(|suffix| suffix.to_str());
                if suffix == Some("part") || (suffix == Some("bin") && !entries.contains(&id)) {
                    fs::remove_file(path)?;
                }
            }
        }
        drop(statement);
        drop(connection);
        self.reap()?;
        Ok(())
    }

    /// Защищает короткую операцию с индексом от одновременного обращения.
    fn connection(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.storage
            .connection
            .lock()
            .map_err(|_| StorageError::Poisoned)
    }
}

impl MediaWriteFence {
    /// Сравнивает ссылку и версии с актуальным состоянием в той же транзакции.
    fn is_current(&self, connection: &Connection) -> rusqlite::Result<bool> {
        Ok(self.version
            == version(
                connection,
                &self.owner,
                &self.key.file_id.to_string(),
                self.scope,
            )?
            && connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM messenger_media_refs WHERE id = ?1)",
                [self.reference],
                |row| row.get::<_, bool>(0),
            )?)
    }
}

impl Disk {
    /// Строит путь к конкретной версии байтов без имени файла или полей владельца.
    fn object_path(&self, id: Uuid) -> PathBuf {
        self.root.join(format!("{id}.bin"))
    }
}

impl MediaTempFile {
    /// Возвращает число записанных байтов для проверки длины и целостности.
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    /// Сбрасывает запись на диск перед короткой транзакцией публикации.
    pub(crate) fn seal(&self) -> Result<(), MediaCacheError> {
        self.file.sync_all()?;
        Ok(())
    }
}

impl Write for MediaTempFile {
    /// Записывает порцию только пока копия помещается в лимит.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.limit - self.size {
            return Err(io::Error::from(io::ErrorKind::FileTooLarge));
        }
        let written = self.file.write(bytes)?;
        self.size += written as u64;
        Ok(written)
    }

    /// Передаёт вызов сброса в открытый временный файл.
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for MediaTempFile {
    /// Удаляет временный файл даже при отмене или сетевой ошибке.
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl MediaReader {
    /// Возвращает длину готовой копии.
    pub(crate) fn size(&self) -> u64 {
        self.object.size
    }

    /// Возвращает метаданные, полученные из файлового API.
    pub(crate) fn metadata(&self) -> &MediaMetadata {
        &self.object.metadata
    }

    /// Проверяет, что именно эта копия и исходная ссылка ещё доступны.
    pub(crate) fn is_current(&self) -> Result<bool, MediaCacheError> {
        Ok(self.cache.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM messenger_media_refs AS ref
             JOIN messenger_media_objects AS object ON object.owner = ref.owner AND object.file = ref.file
             WHERE ref.id = ?1 AND object.id = ?2 AND object.pending = 0)",
            params![self.fence.reference, self.object.id.to_string()],
            |row| row.get(0),
        )?)
    }
}

impl Read for MediaReader {
    /// Отказывает, если после открытия отозвали ссылку или сделали версию недоступной.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.is_current().map_err(io::Error::other)? {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        // Вызывающий повторяет проверку перед публикацией порции в текущей сессии.
        let length = buffer.len().min(64 * 1024);
        self.file
            .as_mut()
            .expect("читатель владеет файлом")
            .read(&mut buffer[..length])
    }
}

impl Drop for MediaReader {
    /// Освобождает дескриптор до снятия отметки о том, что байты заняты.
    fn drop(&mut self) {
        drop(self.file.take());
        let mut readers = self
            .cache
            .disk
            .readers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(readers_count) = readers.get_mut(&self.object.id) else {
            return;
        };
        *readers_count -= 1;
        if *readers_count == 0 {
            readers.remove(&self.object.id);
        }
    }
}

/// Принимает серверный тип для группировки фото, видео и аудио.
fn type_from_mime(mime: Option<&str>) -> Option<MediaKind> {
    match mime?.split_once('/')?.0 {
        "image" => Some(MediaKind::Image),
        "video" => Some(MediaKind::Video),
        "audio" => Some(MediaKind::Audio),
        _ => None,
    }
}

/// Читает и проверяет UUID из текстового столбца SQLite.
fn uuid_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    row.get::<_, String>(index)?
        .parse()
        .map_err(|error: uuid::Error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

/// Читает необязательный UUID из текстового столбца SQLite.
fn optional_uuid_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<Uuid>> {
    row.get::<_, Option<String>>(index)?
        .map(|text| {
            text.parse().map_err(|error: uuid::Error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

/// Читает неотрицательное время или размер из целого SQLite.
fn unsigned_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

/// Читает строку конкретной готовой копии.
fn object_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Object> {
    Ok(Object {
        id: uuid_column(row, 0)?,
        size: unsigned_column(row, 1)?,
        metadata: MediaMetadata {
            name: row.get(2)?,
            mime: row.get(3)?,
            file_stream: optional_uuid_column(row, 4)?,
        },
    })
}

/// Приводит время к диапазону целых SQLite.
fn timestamp(now: u64) -> i64 {
    now.min(i64::MAX as u64) as i64
}

/// Скрывает конкретную версию, чтобы затем удалить её за пределами транзакции.
fn mark_pending(connection: &Connection, id: Uuid) -> rusqlite::Result<()> {
    connection.execute(
        "UPDATE messenger_media_objects SET pending = 1 WHERE id = ?1",
        [id.to_string()],
    )?;
    Ok(())
}
