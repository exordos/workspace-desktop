//! Хранит сообщения, счётчики, позиции чтения и очередь отправки в SQLite.
//! Сначала описаны данные и таблицы, затем чтение и запись.

mod history;
mod outbox;

pub(crate) use history::HistoryPageGuard;
pub use history::{
    HistoryDirection, HistoryEdge, HistoryMessage, HistoryMetadata, HistoryPageEdges,
    HistoryPageOutcome, HistoryPageTarget, HistoryScope, HistoryTime, HistoryWindow,
};

pub use outbox::PreparedSend;

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

use crate::{context::OwnerContext, notification_target::NotificationTarget};

/// Позиция в журнале событий, с которой можно продолжить чтение.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub epoch_generation: String, // Метка журнала от источника. Позиции сравнимы только при одинаковой метке.
    pub epoch_version: u64,       // Номер события внутри этого журнала.
}

/// Сообщение мессенджера в локальной базе.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub id: String,   // Серверный идентификатор сообщения.
    pub text: String, // Текст сообщения.
}

/// Адрес сообщения в журнале. Старые записи могут не содержать этот адрес.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagePlacement {
    pub stream_id: String, // Стрим, которому принадлежит сообщение.
    pub topic_id: String,  // Тема внутри стрима.
}

/// Изменение истории, которое сохраняем вместе с позицией журнала.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryChange {
    Edit {
        message: Message,                    // Новый текст существующего сообщения.
        placement: Option<MessagePlacement>, // Адрес из снимка, если сервер его передал.
    },
    RemoveMessage(String), // UUID удалённого или больше не поддерживаемого сообщения.
    RemoveStream(String),  // UUID стрима, доступ к которому потерян.
    RemoveTopic(MessagePlacement), // Стрим и UUID удалённой темы.
}

/// Данные одной записи журнала. Пустые поля не меняют соответствующие таблицы.
#[derive(Default)]
pub(crate) struct EventWrite<'a> {
    pub message: Option<&'a Message>,               // Новое сообщение.
    pub metadata: Option<&'a HistoryMetadata>,      // Автор и серверное время нового сообщения.
    pub placement: Option<&'a MessagePlacement>,    // Стрим и тема нового сообщения.
    pub unread: Option<u32>,                        // Подтверждённый счётчик.
    pub notification_conversation: Option<&'a str>, // Разговор для цели баннера.
    pub history: Option<&'a HistoryChange>,         // Правка или удаление.
}

/// Число непрочитанных и позиция журнала, до которой оно уже учитывает изменения.
struct StoredUnread {
    count: u32,               // Готовое число непрочитанных сообщений.
    epoch_generation: String, // Метка журнала источника.
    epoch_version: i64,       // Номер события или позиция перед чтением из REST.
}

/// Результат обработки события в локальной базе.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Applied,   // Изменение обработано. Событие журнала также обновило позицию чтения.
    Duplicate, // Эта позиция уже обработана или находится позади сохранённой.
}

/// Результаты одной транзакции. Помогают опубликовать только записанные изменения.
pub(crate) struct AppliedEvent {
    pub outcome: ApplyOutcome,  // Была ли сохранена позиция источника.
    pub message_inserted: bool, // Появилась ли новая строка с сообщением.
    pub unread_changed: bool,   // Изменилось ли число непрочитанных.
}

/// Причины, по которым локальная база не смогла прочитать или записать данные.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("the source epoch changed and requires recovery")]
    EpochChanged, // Метка журнала изменилась. Старую и новую позиции сравнить нельзя.
    #[error("the source cursor version exceeds the supported range")]
    CursorVersionOutOfRange, // Номер позиции нельзя представить в допустимом диапазоне SQLite.
    #[error("the storage connection lock is poisoned")]
    Poisoned, // Доступ к соединению потерян после паники под блокировкой.
    #[error("invalid messenger history page")]
    InvalidHistoryPage, // Страница не подходит по области, направлению или формату.
    #[error("the catalog changed while the request was in flight")]
    StaleCatalog, // После начала запроса доступ к каналу изменился.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error), // Исходная ошибка SQLite.
    #[error("the cached chat list has an invalid format")]
    Catalog(#[from] serde_json::Error), // Не удалось прочитать или записать снимок списка чатов.
}

/// Локальное хранилище сообщений, счётчиков и позиций чтения.
pub struct Storage {
    connection: Mutex<Connection>, // Соединение с SQLite. Блокировка выдаёт его по одному запросу за раз.
    catalog_changes: tokio::sync::watch::Sender<u64>, // Сигнал о зафиксированном отзыве доступа.
}

impl Storage {
    /// Читает сохранённый список чатов этого владельца. Отсутствие записи означает первый запуск.
    pub(crate) fn chat_catalog(
        &self,
        owner: &OwnerContext,
    ) -> Result<Option<String>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        read_chat_catalog(&connection, owner)
    }

    /// Возвращает версию каталога перед началом сетевого запроса.
    pub(crate) fn catalog_revision(&self, owner: &OwnerContext) -> Result<i64, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        read_catalog_revision(&connection, owner)
    }

    /// Даёт сигнал после каждого зафиксированного отзыва доступа.
    pub(crate) fn catalog_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.catalog_changes.subscribe()
    }

    /// Читает все нужные снимки и публикует их до освобождения блокировки базы.
    pub(crate) fn with_catalog_snapshots<T>(
        &self,
        owners: &[OwnerContext],
        publish: impl FnOnce(Vec<Option<String>>) -> T,
    ) -> Result<T, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let snapshots = owners
            .iter()
            .map(|owner| read_chat_catalog(&connection, owner))
            .collect::<Result<_, _>>()?;
        Ok(publish(snapshots))
    }

    /// Заменяет снимок, если после начала запроса доступ к каналам не менялся.
    pub(crate) fn save_chat_catalog(
        &self,
        owner: &OwnerContext,
        snapshot: &str,
        revision: i64,
    ) -> Result<(), StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        if read_catalog_revision(&connection, owner)? != revision {
            return Err(StorageError::StaleCatalog);
        }
        connection.execute(
            "INSERT INTO messenger_catalog (account_id, server, organization_id, project_id, user_id, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (account_id, server, organization_id, project_id, user_id)
             DO UPDATE SET snapshot = excluded.snapshot",
            params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, snapshot],
        )?;
        Ok(())
    }

    /// Открывает базу по пути и создаёт таблицы при необходимости.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::initialize(Connection::open(path)?)
    }

    /// Создаёт базу в памяти для работы без файла.
    pub fn in_memory() -> Result<Self, StorageError> {
        Self::initialize(Connection::open_in_memory()?)
    }

    /// Подготавливает таблицы сообщений, счётчиков и позиций для переданного соединения.
    fn initialize(mut connection: Connection) -> Result<Self, StorageError> {
        connection.pragma_update(None, "foreign_keys", true)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS sync_cursors (
                account_id TEXT NOT NULL,
                server TEXT NOT NULL,
                organization_id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                source TEXT NOT NULL,
                epoch_generation TEXT NOT NULL,
                epoch_version INTEGER NOT NULL CHECK (epoch_version >= 0),
                PRIMARY KEY (account_id, server, organization_id, project_id, user_id, source)
            );
            CREATE TABLE IF NOT EXISTS sync_messages (
                account_id TEXT NOT NULL,
                server TEXT NOT NULL,
                organization_id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                text TEXT NOT NULL,
                stream_id TEXT,
                topic_id TEXT,
                PRIMARY KEY (account_id, server, organization_id, project_id, user_id, message_id)
            );
            CREATE TABLE IF NOT EXISTS messenger_notification_targets (
                account_id TEXT NOT NULL,
                server TEXT NOT NULL,
                organization_id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL,
                PRIMARY KEY (account_id, server, organization_id, project_id, user_id, message_id)
            );
            CREATE TABLE IF NOT EXISTS messenger_catalog (
                account_id TEXT NOT NULL,
                server TEXT NOT NULL,
                organization_id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                snapshot TEXT NOT NULL,
                PRIMARY KEY (account_id, server, organization_id, project_id, user_id)
            );
            CREATE TABLE IF NOT EXISTS messenger_catalog_revision (
                account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
                project_id TEXT NOT NULL, user_id TEXT NOT NULL, revision INTEGER NOT NULL,
                PRIMARY KEY (account_id, server, organization_id, project_id, user_id)
            );
            CREATE TABLE IF NOT EXISTS messenger_unread (
                account_id TEXT NOT NULL,
                server TEXT NOT NULL,
                organization_id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                count INTEGER NOT NULL CHECK (count >= 0),
                epoch_generation TEXT NOT NULL,
                epoch_version INTEGER NOT NULL CHECK (epoch_version >= 0),
                PRIMARY KEY (account_id, server, organization_id, project_id, user_id)
            );",
        )?;
        // Старый кэш хранит только текст. У таких записей адрес остаётся неизвестным.
        let has_placement: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('sync_messages') WHERE name = 'stream_id')",
            [], |row| row.get(0),
        )?;
        if !has_placement {
            transaction.execute_batch(
                "ALTER TABLE sync_messages ADD COLUMN stream_id TEXT;
                ALTER TABLE sync_messages ADD COLUMN topic_id TEXT;",
            )?;
        }
        transaction.execute_batch("CREATE INDEX IF NOT EXISTS sync_messages_by_placement
            ON sync_messages (account_id, server, organization_id, project_id, user_id, stream_id, topic_id)")?;
        history::initialize(&transaction)?;
        outbox::initialize(&transaction)?;
        transaction.commit()?;
        Ok(Self {
            connection: Mutex::new(connection),
            catalog_changes: tokio::sync::watch::channel(0).0,
        })
    }

    /// Проверяет, что сообщение сохранилось и синхронизация связала его с этим разговором.
    pub(crate) fn contains_notification_target(
        &self,
        target: &NotificationTarget,
    ) -> Result<bool, StorageError> {
        let owner = &target.context;
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(connection.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM messenger_notification_targets AS target
                 JOIN sync_messages USING (account_id, server, organization_id, project_id, user_id, message_id)
                 WHERE target.account_id = ?1 AND target.server = ?2 AND target.organization_id = ?3
                   AND target.project_id = ?4 AND target.user_id = ?5 AND target.message_id = ?6
                   AND target.conversation_id = ?7
             )",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                target.message_id,
                target.conversation_id,
            ],
            |row| row.get(0),
        )?)
    }

    /// Читает позицию указанного источника для аккаунта и проекта. `None`, если её ещё нет.
    pub fn cursor(
        &self,
        owner: &OwnerContext,
        source: &str,
    ) -> Result<Option<Cursor>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        read_cursor(&connection, owner, source)
    }

    /// Ищет сообщение по идентификатору в пределах аккаунта и проекта.
    pub fn message(
        &self,
        owner: &OwnerContext,
        message_id: &str,
    ) -> Result<Option<Message>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(connection
            .query_row(
                "SELECT message_id, text FROM sync_messages
                 WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
                   AND project_id = ?4 AND user_id = ?5 AND message_id = ?6",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    message_id,
                ],
                |row| {
                    Ok(Message {
                        id: row.get(0)?,
                        text: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Читает последнее подтверждённое сервером число непрочитанных.
    pub fn unread(&self, owner: &OwnerContext) -> Result<Option<u32>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(read_unread(&connection, owner)?.map(|saved| saved.count))
    }

    /// Удаляет непригодную позицию и зависимые данные владельца одной транзакцией.
    pub(crate) fn reset_source(
        &self,
        owner: &OwnerContext,
        source: &str,
    ) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owner_params = params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id
        ];
        transaction.execute(
            "DELETE FROM sync_cursors WHERE account_id = ?1 AND server = ?2
             AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND source = ?6",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                source
            ],
        )?;
        if source == crate::messenger::SOURCE_ID {
            history::reset(&transaction, owner)?;
            for table in [
                "sync_messages",
                "messenger_notification_targets",
                "messenger_unread",
            ] {
                transaction.execute(
                    &format!(
                        "DELETE FROM {table} WHERE account_id = ?1
                    AND server = ?2 AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5"
                    ),
                    owner_params,
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Записывает снимок REST, сохраняя текущую позицию чтения журнала.
    pub(crate) fn apply_unread_baseline(
        &self,
        owner: &OwnerContext,
        cursor: &Cursor,
        count: u32,
    ) -> Result<bool, StorageError> {
        let epoch_version = i64::try_from(cursor.epoch_version)
            .map_err(|_| StorageError::CursorVersionOutOfRange)?;
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = write_unread(
            &transaction,
            owner,
            &cursor.epoch_generation,
            epoch_version,
            count,
            true,
        )?;
        transaction.commit()?;
        Ok(changed)
    }

    /// Записывает изменение и позицию одной транзакцией.
    /// Пропускает повторную позицию и возвращает ошибку при смене метки журнала.
    pub(crate) fn apply(
        &self,
        owner: &OwnerContext,
        source: &str,
        cursor: &Cursor,
        change: EventWrite<'_>,
    ) -> Result<AppliedEvent, StorageError> {
        let epoch_version = i64::try_from(cursor.epoch_version)
            .map_err(|_| StorageError::CursorVersionOutOfRange)?;
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(previous) = read_cursor(&transaction, owner, source)? {
            if previous.epoch_generation != cursor.epoch_generation {
                return Err(StorageError::EpochChanged);
            }
            if previous.epoch_version >= cursor.epoch_version {
                return Ok(AppliedEvent {
                    outcome: ApplyOutcome::Duplicate,
                    message_inserted: false,
                    unread_changed: false,
                });
            }
        }

        if let Some(history) = change.history
            && history::before_change(&transaction, owner, history)?
        {
            apply_history(&transaction, owner, history)?;
        }

        let revoked = if source == crate::messenger::SOURCE_ID {
            if let Some(HistoryChange::RemoveStream(stream)) = change.history {
                remove_catalog_stream(&transaction, owner, stream)?;
                true
            } else {
                false
            }
        } else {
            false
        };

        let message_inserted = if let Some(message) = change.message
            && history::can_create(&transaction, owner, &message.id)?
        {
            let inserted = transaction.execute(
                "INSERT INTO sync_messages
                 (account_id, server, organization_id, project_id, user_id, message_id, text, stream_id, topic_id, author_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (account_id, server, organization_id, project_id, user_id, message_id)
             DO NOTHING",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    message.id,
                    message.text,
                    change.placement.map(|address| address.stream_id.as_str()).or(change.notification_conversation),
                    change.placement.map(|address| &address.topic_id),
                    change.metadata.and_then(|metadata| metadata.author_id).map(|id| id.to_string()),
                    change.metadata.map(|metadata| metadata.created_at.as_str()),
                    change.metadata.and_then(|metadata| metadata.updated_at.as_ref()).map(HistoryTime::as_str),
                ],
            )? != 0;
            if inserted {
                history::record_creation(
                    &transaction,
                    owner,
                    message,
                    change.placement,
                    change.metadata,
                )?;
            }
            inserted
        } else {
            false
        };
        // Подтверждение очереди фиксируется вместе с сообщением и позицией события.
        if source == crate::messenger::SOURCE_ID
            && let (Some(message), Some(placement), Some(metadata)) =
                (change.message, change.placement, change.metadata)
        {
            outbox::confirm(
                &transaction,
                owner,
                &message.id,
                &placement.stream_id,
                &placement.topic_id,
                metadata.author_id,
            )?;
        }
        // Цель уведомления доступна вместе с новым сообщением и его курсором.
        if message_inserted
            && let (Some(message), Some(conversation_id)) =
                (change.message, change.notification_conversation)
        {
            transaction.execute(
                "INSERT INTO messenger_notification_targets
                 (account_id, server, organization_id, project_id, user_id, message_id, conversation_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    message.id,
                    conversation_id,
                ],
            )?;
        }
        let unread_changed = match change.unread {
            Some(count) => write_unread(
                &transaction,
                owner,
                &cursor.epoch_generation,
                epoch_version,
                count,
                false,
            )?,
            None => false,
        };
        transaction.execute(
            "INSERT INTO sync_cursors
                 (account_id, server, organization_id, project_id, user_id,
                  source, epoch_generation, epoch_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (account_id, server, organization_id, project_id, user_id, source)
             DO UPDATE SET epoch_version = excluded.epoch_version",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                source,
                cursor.epoch_generation,
                epoch_version,
            ],
        )?;
        transaction.commit()?;
        if revoked {
            self.catalog_changes
                .send_modify(|version| *version = version.wrapping_add(1));
        }
        Ok(AppliedEvent {
            outcome: ApplyOutcome::Applied,
            message_inserted,
            unread_changed,
        })
    }
}

/// Читает сохранённый каталог одного владельца внутри общей блокировки базы.
fn read_chat_catalog(
    connection: &Connection,
    owner: &OwnerContext,
) -> Result<Option<String>, StorageError> {
    Ok(connection
        .query_row(
            "SELECT snapshot FROM messenger_catalog WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id
            ],
            |row| row.get(0),
        )
        .optional()?)
}

/// Читает версию отзыва доступа; ноль означает отсутствие изменений.
fn read_catalog_revision(
    connection: &Connection,
    owner: &OwnerContext,
) -> Result<i64, StorageError> {
    Ok(connection
        .query_row(
            "SELECT revision FROM messenger_catalog_revision WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id
            ],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Удаляет канал и увеличивает версию в той же транзакции, что и позиция журнала.
fn remove_catalog_stream(
    connection: &Connection,
    owner: &OwnerContext,
    stream: &str,
) -> Result<(), StorageError> {
    if let Some(snapshot) = read_chat_catalog(connection, owner)? {
        let mut chats: Vec<crate::messenger::catalog::Chat> = serde_json::from_str(&snapshot)?;
        chats.retain(|chat| chat.id.to_string() != stream);
        let snapshot = serde_json::to_string(&chats)?;
        connection.execute(
            "UPDATE messenger_catalog SET snapshot = ?6 WHERE account_id = ?1 AND server = ?2
             AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                snapshot
            ],
        )?;
    }
    connection.execute(
        "INSERT INTO messenger_catalog_revision VALUES (?1, ?2, ?3, ?4, ?5, 1)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id)
         DO UPDATE SET revision = revision + 1",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id
        ],
    )?;
    Ok(())
}

/// Обновляет текст или удаляет затронутые строки и связанные цели уведомлений.
fn apply_history(
    connection: &Connection,
    owner: &OwnerContext,
    history: &HistoryChange,
) -> Result<(), StorageError> {
    match history {
        HistoryChange::Edit { message, placement } => {
            connection.execute(
                "UPDATE sync_messages SET text = ?7, stream_id = COALESCE(?8, stream_id), topic_id = COALESCE(?9, topic_id)
                 WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
                   AND user_id = ?5 AND message_id = ?6",
                params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
                    message.id, message.text, placement.as_ref().map(|address| &address.stream_id),
                    placement.as_ref().map(|address| &address.topic_id)],
            )?;
            if let Some(address) = placement {
                connection.execute(
                    "DELETE FROM messenger_notification_targets WHERE account_id = ?1 AND server = ?2
                     AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5
                     AND message_id = ?6 AND conversation_id <> ?7",
                    params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id,
                        owner.user_id, message.id, address.stream_id],
                )?;
            }
            return Ok(());
        }
        HistoryChange::RemoveMessage(id) => {
            connection.execute(
                "DELETE FROM sync_messages WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
                 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6",
                params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, id],
            )?;
        }
        HistoryChange::RemoveStream(_) | HistoryChange::RemoveTopic(_) => {
            let (stream, topic) = match history {
                HistoryChange::RemoveStream(id) => (id, None),
                HistoryChange::RemoveTopic(address) => {
                    (&address.stream_id, Some(&address.topic_id))
                }
                _ => unreachable!(),
            };
            // Записи без адреса могли принадлежать отозванному стриму или теме.
            connection.execute(
                "DELETE FROM sync_messages WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
                 AND project_id = ?4 AND user_id = ?5
                 AND (stream_id IS NULL OR (stream_id = ?6 AND (?7 IS NULL OR topic_id IS NULL OR topic_id = ?7)))",
                params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, stream, topic],
            )?;
        }
    }
    connection.execute(
        "DELETE FROM messenger_notification_targets AS target WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND NOT EXISTS (
             SELECT 1 FROM sync_messages AS message WHERE message.account_id = ?1 AND message.server = ?2
               AND message.organization_id = ?3 AND message.project_id = ?4 AND message.user_id = ?5
               AND message.message_id = target.message_id
         )",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id],
    )?;
    Ok(())
}

/// Читает число и позицию счётчика в рамках аккаунта и проекта.
fn read_unread(
    connection: &Connection,
    owner: &OwnerContext,
) -> Result<Option<StoredUnread>, StorageError> {
    Ok(connection
        .query_row(
            "SELECT count, epoch_generation, epoch_version FROM messenger_unread
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id
            ],
            |row| {
                Ok(StoredUnread {
                    count: row.get(0)?,
                    epoch_generation: row.get(1)?,
                    epoch_version: row.get(2)?,
                })
            },
        )
        .optional()?)
}

/// Записывает число из более свежего снимка. Возвращает, изменилось ли отображаемое число.
fn write_unread(
    connection: &Connection,
    owner: &OwnerContext,
    epoch_generation: &str,
    epoch_version: i64,
    count: u32,
    from_rest: bool,
) -> Result<bool, StorageError> {
    let previous = read_unread(connection, owner)?;
    if let Some(saved) = &previous
        && saved.epoch_generation == epoch_generation
        && (saved.epoch_version > epoch_version
            || (saved.epoch_version == epoch_version && !from_rest))
    {
        // Ответ REST уже учитывает события до позиции, которую прочитали перед запросом.
        return Ok(false);
    }
    connection.execute(
        "INSERT INTO messenger_unread
         (account_id, server, organization_id, project_id, user_id, count, epoch_generation, epoch_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id)
         DO UPDATE SET count = excluded.count, epoch_generation = excluded.epoch_generation,
                       epoch_version = excluded.epoch_version",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            count,
            epoch_generation,
            epoch_version,
        ],
    )?;
    Ok(previous.is_none_or(|saved| saved.count != count))
}

/// Читает позицию через переданное соединение, в том числе внутри транзакции.
fn read_cursor(
    connection: &Connection,
    owner: &OwnerContext,
    source: &str,
) -> Result<Option<Cursor>, StorageError> {
    let saved = connection
        .query_row(
            "SELECT epoch_generation, epoch_version FROM sync_cursors
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND source = ?6",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                source,
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    saved
        .map(|(epoch_generation, epoch_version)| {
            Ok(Cursor {
                epoch_generation,
                epoch_version: u64::try_from(epoch_version)
                    .map_err(|_| StorageError::CursorVersionOutOfRange)?,
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AccountId;

    /// Записывает тестовое событие с сообщением.
    fn apply_message(
        storage: &Storage,
        owner: &OwnerContext,
        source: &str,
        cursor: Cursor,
        message: Message,
    ) -> Result<ApplyOutcome, StorageError> {
        storage
            .apply(
                owner,
                source,
                &cursor,
                EventWrite {
                    message: Some(&message),
                    ..Default::default()
                },
            )
            .map(|saved| saved.outcome)
    }

    const SOURCE: &str = "messenger";

    /// Возвращает владельца данных для тестов хранилища.
    fn owner() -> OwnerContext {
        OwnerContext {
            account_id: AccountId("account".into()),
            server: "workspace.test".into(),
            organization_id: "organization".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        }
    }

    /// Собирает тестовую позицию из метки и номера события.
    fn cursor(epoch_generation: &str, epoch_version: u64) -> Cursor {
        Cursor {
            epoch_generation: epoch_generation.into(),
            epoch_version,
        }
    }

    /// Создаёт тестовое сообщение с заданным текстом и идентификатором.
    fn message(id: &str, text: &str) -> Message {
        Message {
            id: id.into(),
            text: text.into(),
        }
    }

    /// Проверяет запись очереди после закрытия базы, повторного сохранения и сброса источника.
    #[test]
    fn prepared_send_survives_reopen_duplicate_and_source_reset() {
        use uuid::Uuid;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("workspace.sqlite");
        let a = owner();
        let b = OwnerContext {
            project_id: "other-project".into(),
            ..a.clone()
        };
        let first = PreparedSend {
            canonical_id: Uuid::from_u128(1),
            public_id: Uuid::from_u128(2),
            stream_id: Uuid::from_u128(3),
            topic_id: Uuid::from_u128(4),
            markdown: "**Привет**\n".into(),
        };
        let other = PreparedSend {
            public_id: Uuid::from_u128(5),
            stream_id: Uuid::from_u128(6),
            topic_id: Uuid::from_u128(7),
            markdown: "Другая запись".into(),
            ..first.clone()
        };

        let storage = Storage::open(&path).unwrap();
        assert!(storage.save_prepared_send(&a, &first).unwrap());
        assert!(storage.save_prepared_send(&b, &other).unwrap());
        drop(storage);

        let storage = Storage::open(&path).unwrap();
        assert!(!storage.save_prepared_send(&a, &other).unwrap());
        assert_eq!(
            storage.pending_sends(&a).unwrap(),
            std::slice::from_ref(&first)
        );
        assert_eq!(
            storage.pending_sends(&b).unwrap(),
            std::slice::from_ref(&other)
        );

        apply_message(
            &storage,
            &a,
            SOURCE,
            cursor("epoch", 1),
            message("msg", "cache"),
        )
        .unwrap();
        storage.reset_source(&a, SOURCE).unwrap();
        assert_eq!(storage.cursor(&a, SOURCE).unwrap(), None);
        assert_eq!(storage.pending_sends(&a).unwrap(), [first]);
        assert_eq!(storage.pending_sends(&b).unwrap(), [other]);
    }

    /// Проверяет миграцию старой очереди, единственного победителя и восстановление после закрытия.
    #[test]
    fn first_send_claim_survives_reopen_and_preserves_legacy_pending() {
        use std::sync::Barrier;
        use uuid::Uuid;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("workspace.sqlite");
        let a = owner();
        let b = OwnerContext {
            project_id: "other-project".into(),
            ..a.clone()
        };
        let first = PreparedSend {
            canonical_id: Uuid::from_u128(1),
            public_id: Uuid::from_u128(2),
            stream_id: Uuid::from_u128(3),
            topic_id: Uuid::from_u128(4),
            markdown: "**Первая отправка**".into(),
        };
        let other = PreparedSend {
            markdown: "Другой владелец".into(),
            ..first.clone()
        };

        // Создаём файл со схемой и строками до появления закрепления.
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE messenger_outbox (
                account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
                project_id TEXT NOT NULL, user_id TEXT NOT NULL, canonical_id BLOB NOT NULL,
                public_id BLOB NOT NULL, stream_id BLOB NOT NULL, topic_id BLOB NOT NULL,
                markdown TEXT NOT NULL, state TEXT NOT NULL CHECK (state = 'pending'),
                PRIMARY KEY (account_id, server, organization_id, project_id, user_id, canonical_id)
            );",
            )
            .unwrap();
        for (owner, message) in [(&a, &first), (&b, &other)] {
            legacy.execute(
                "INSERT INTO messenger_outbox VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending')",
                params![
                    owner.account_id.0, owner.server, owner.organization_id, owner.project_id,
                    owner.user_id, message.canonical_id.as_bytes(), message.public_id.as_bytes(),
                    message.stream_id.as_bytes(), message.topic_id.as_bytes(), message.markdown,
                ],
            ).unwrap();
        }
        drop(legacy);

        let left = Storage::open(&path).unwrap();
        let right = Storage::open(&path).unwrap();
        assert_eq!(
            left.pending_sends(&a).unwrap(),
            std::slice::from_ref(&first)
        );
        assert!(left.claimed_sends(&a).unwrap().is_empty());
        let barrier = Barrier::new(2);
        let canonical_id = first.canonical_id;
        let snapshots = std::thread::scope(|scope| {
            let left = scope.spawn(|| {
                barrier.wait();
                left.claim_first_send(&a, canonical_id).unwrap()
            });
            let right = scope.spawn(|| {
                barrier.wait();
                right.claim_first_send(&a, canonical_id).unwrap()
            });
            [left.join().unwrap(), right.join().unwrap()]
        });
        assert_eq!(
            snapshots.into_iter().flatten().collect::<Vec<_>>(),
            std::slice::from_ref(&first)
        );
        drop((left, right));

        let reopened = Storage::open(&path).unwrap();
        assert!(reopened.pending_sends(&a).unwrap().is_empty());
        assert_eq!(reopened.claimed_sends(&a).unwrap(), [first]);
        assert_eq!(reopened.claim_first_send(&a, canonical_id).unwrap(), None);
        assert_eq!(
            reopened.pending_sends(&b).unwrap(),
            std::slice::from_ref(&other)
        );
        assert!(reopened.claimed_sends(&b).unwrap().is_empty());
        assert_eq!(
            reopened.claim_first_send(&b, canonical_id).unwrap(),
            Some(other)
        );
    }

    /// Проверяет оба порядка подтверждения, несовпадения и откат транзакции события.
    #[test]
    fn outbox_confirmation_is_atomic_and_preserves_newer_history() {
        use uuid::Uuid;
        let storage = Storage::in_memory().unwrap();
        let mut owner = owner();
        let author = Uuid::from_u128(10);
        owner.user_id = author.to_string();
        let entry = PreparedSend {
            canonical_id: Uuid::from_u128(1),
            public_id: Uuid::from_u128(2),
            stream_id: Uuid::from_u128(3),
            topic_id: Uuid::from_u128(4),
            markdown: "sent".into(),
        };
        let response = HistoryMessage {
            id: entry.public_id,
            stream_id: entry.stream_id,
            topic_id: entry.topic_id,
            text: "sent".into(),
            metadata: HistoryMetadata {
                author_id: Some(author),
                created_at: HistoryTime::parse("2026-01-01T00:00:00Z").unwrap(),
                updated_at: None,
            },
        };
        let address = MessagePlacement {
            stream_id: entry.stream_id.to_string(),
            topic_id: entry.topic_id.to_string(),
        };
        let sent = message(&entry.public_id.to_string(), "sent");
        let create = |version| {
            storage.apply(
                &owner,
                SOURCE,
                &cursor("epoch", version),
                EventWrite {
                    message: Some(&sent),
                    placement: Some(&address),
                    metadata: Some(&response.metadata),
                    ..Default::default()
                },
            )
        };
        assert!(storage.save_prepared_send(&owner, &entry).unwrap());
        storage
            .claim_first_send(&owner, entry.canonical_id)
            .unwrap()
            .unwrap();
        let other_owner = OwnerContext {
            project_id: "other".into(),
            ..owner.clone()
        };
        assert!(!storage.confirm_http_send(&other_owner, &response).unwrap());
        let mut wrong = response.clone();
        wrong.id = Uuid::from_u128(19);
        assert!(!storage.confirm_http_send(&owner, &wrong).unwrap());
        wrong = response.clone();
        wrong.stream_id = Uuid::from_u128(18);
        assert!(!storage.confirm_http_send(&owner, &wrong).unwrap());
        wrong = response.clone();
        wrong.topic_id = Uuid::from_u128(20);
        assert!(!storage.confirm_http_send(&owner, &wrong).unwrap());
        wrong = response.clone();
        wrong.metadata.author_id = Some(Uuid::from_u128(21));
        assert!(!storage.confirm_http_send(&owner, &wrong).unwrap());
        assert!(storage.confirm_http_send(&owner, &response).unwrap());
        assert!(storage.cursor(&owner, SOURCE).unwrap().is_none());
        assert!(storage.claimed_sends(&owner).unwrap().is_empty());
        create(1).unwrap();
        assert_eq!(
            storage.message(&owner, &sent.id).unwrap(),
            Some(sent.clone())
        );
        assert!(!storage.confirm_http_send(&owner, &response).unwrap());

        // Новая запись с тем же публичным UUID позволяет проверить событие первым и откат.
        let next = PreparedSend {
            canonical_id: Uuid::from_u128(11),
            ..entry.clone()
        };
        storage.save_prepared_send(&owner, &next).unwrap();
        storage
            .claim_first_send(&owner, next.canonical_id)
            .unwrap()
            .unwrap();
        storage
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_confirmation_cursor BEFORE UPDATE ON sync_cursors
             BEGIN SELECT RAISE(ABORT, 'test cursor failure'); END",
            )
            .unwrap();
        assert!(create(2).is_err());
        assert_eq!(storage.claimed_sends(&owner).unwrap(), vec![next.clone()]);
        assert_eq!(
            storage.cursor(&owner, SOURCE).unwrap(),
            Some(cursor("epoch", 1))
        );
        storage
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_confirmation_cursor")
            .unwrap();
        create(2).unwrap();
        assert!(storage.claimed_sends(&owner).unwrap().is_empty());
        let edited = HistoryChange::Edit {
            message: message(&sent.id, "edited"),
            placement: Some(address.clone()),
        };
        storage
            .apply(
                &owner,
                SOURCE,
                &cursor("epoch", 3),
                EventWrite {
                    history: Some(&edited),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!storage.confirm_http_send(&owner, &response).unwrap());
        assert_eq!(
            storage.message(&owner, &sent.id).unwrap().unwrap().text,
            "edited"
        );
        let removed = HistoryChange::RemoveMessage(sent.id.clone());
        storage
            .apply(
                &owner,
                SOURCE,
                &cursor("epoch", 4),
                EventWrite {
                    history: Some(&removed),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!storage.confirm_http_send(&owner, &response).unwrap());
        assert_eq!(storage.message(&owner, &sent.id).unwrap(), None);
    }

    /// Проверяет событие первым и запоздавший HTTP-ответ после правки и удаления.
    #[test]
    fn event_first_and_late_http_do_not_restore_old_messages() {
        use uuid::Uuid;
        let storage = Storage::in_memory().unwrap();
        let mut owner = owner();
        let author = Uuid::from_u128(10);
        owner.user_id = author.to_string();
        let entry = PreparedSend {
            canonical_id: Uuid::from_u128(1),
            public_id: Uuid::from_u128(2),
            stream_id: Uuid::from_u128(3),
            topic_id: Uuid::from_u128(4),
            markdown: "sent".into(),
        };
        let response = HistoryMessage {
            id: entry.public_id,
            stream_id: entry.stream_id,
            topic_id: entry.topic_id,
            text: "sent".into(),
            metadata: HistoryMetadata {
                author_id: Some(author),
                created_at: HistoryTime::parse("2026-01-01T00:00:00Z").unwrap(),
                updated_at: None,
            },
        };
        let address = MessagePlacement {
            stream_id: entry.stream_id.to_string(),
            topic_id: entry.topic_id.to_string(),
        };
        let sent = message(&entry.public_id.to_string(), "sent");
        storage.save_prepared_send(&owner, &entry).unwrap();
        storage
            .claim_first_send(&owner, entry.canonical_id)
            .unwrap()
            .unwrap();
        storage
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_first_cursor BEFORE INSERT ON sync_cursors
             BEGIN SELECT RAISE(ABORT, 'test cursor failure'); END",
            )
            .unwrap();
        assert!(
            storage
                .apply(
                    &owner,
                    SOURCE,
                    &cursor("epoch", 1),
                    EventWrite {
                        message: Some(&sent),
                        placement: Some(&address),
                        metadata: Some(&response.metadata),
                        ..Default::default()
                    }
                )
                .is_err()
        );
        assert_eq!(storage.message(&owner, &sent.id).unwrap(), None);
        assert_eq!(storage.claimed_sends(&owner).unwrap(), vec![entry.clone()]);
        assert_eq!(storage.cursor(&owner, SOURCE).unwrap(), None);
        storage
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_first_cursor")
            .unwrap();
        storage
            .apply(
                &owner,
                SOURCE,
                &cursor("epoch", 1),
                EventWrite {
                    message: Some(&sent),
                    placement: Some(&address),
                    metadata: Some(&response.metadata),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(storage.claimed_sends(&owner).unwrap().is_empty());
        assert!(!storage.confirm_http_send(&owner, &response).unwrap());
        assert_eq!(
            storage.message(&owner, &sent.id).unwrap(),
            Some(sent.clone())
        );

        let edit = HistoryChange::Edit {
            message: message(&sent.id, "edited"),
            placement: Some(address),
        };
        storage
            .apply(
                &owner,
                SOURCE,
                &cursor("epoch", 2),
                EventWrite {
                    history: Some(&edit),
                    ..Default::default()
                },
            )
            .unwrap();
        let again = PreparedSend {
            canonical_id: Uuid::from_u128(11),
            ..entry.clone()
        };
        storage.save_prepared_send(&owner, &again).unwrap();
        storage
            .claim_first_send(&owner, again.canonical_id)
            .unwrap()
            .unwrap();
        assert!(storage.confirm_http_send(&owner, &response).unwrap());
        assert_eq!(
            storage.message(&owner, &sent.id).unwrap().unwrap().text,
            "edited"
        );
        let remove = HistoryChange::RemoveMessage(sent.id.clone());
        storage
            .apply(
                &owner,
                SOURCE,
                &cursor("epoch", 3),
                EventWrite {
                    history: Some(&remove),
                    ..Default::default()
                },
            )
            .unwrap();
        let third = PreparedSend {
            canonical_id: Uuid::from_u128(12),
            ..entry
        };
        storage.save_prepared_send(&owner, &third).unwrap();
        storage
            .claim_first_send(&owner, third.canonical_id)
            .unwrap()
            .unwrap();
        assert!(storage.confirm_http_send(&owner, &response).unwrap());
        assert_eq!(storage.message(&owner, &sent.id).unwrap(), None);
    }

    /// Проверяет, что повтор или смена журнала не повреждают сообщения и позицию.
    #[test]
    fn preserves_cursor_and_messages_on_duplicate_or_epoch_change() {
        let storage = Storage::in_memory().unwrap();
        let owner = owner();
        let first = message("first", "First");
        let position = cursor("epoch-a", 10);
        assert_eq!(
            apply_message(&storage, &owner, SOURCE, position.clone(), first.clone()).unwrap(),
            ApplyOutcome::Applied
        );
        assert_eq!(
            apply_message(
                &storage,
                &owner,
                SOURCE,
                position.clone(),
                message("second", "Duplicate")
            )
            .unwrap(),
            ApplyOutcome::Duplicate
        );
        assert!(matches!(
            apply_message(
                &storage,
                &owner,
                SOURCE,
                cursor("epoch-b", 11),
                message("third", "New epoch")
            ),
            Err(StorageError::EpochChanged)
        ));
        assert_eq!(storage.cursor(&owner, SOURCE).unwrap(), Some(position));
        assert_eq!(storage.message(&owner, "first").unwrap(), Some(first));
        assert_eq!(storage.message(&owner, "second").unwrap(), None);
        assert_eq!(storage.message(&owner, "third").unwrap(), None);
    }

    /// Проверяет, что сбой записи позиции отменяет запись сообщения и позволяет повторить событие.
    #[test]
    fn failed_cursor_write_rolls_back_message_and_allows_replay() {
        let storage = Storage::in_memory().unwrap();
        let owner = owner();
        let first = cursor("epoch", 1);
        let second = cursor("epoch", 2);
        apply_message(
            &storage,
            &owner,
            SOURCE,
            first.clone(),
            message("one", "One"),
        )
        .unwrap();
        storage
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_cursor BEFORE UPDATE ON sync_cursors
                 BEGIN SELECT RAISE(ABORT, 'test cursor failure'); END",
            )
            .unwrap();

        let pending = message("two", "Two");
        assert!(matches!(
            apply_message(&storage, &owner, SOURCE, second.clone(), pending.clone()),
            Err(StorageError::Sqlite(_))
        ));
        assert_eq!(storage.cursor(&owner, SOURCE).unwrap(), Some(first));
        assert_eq!(storage.message(&owner, "two").unwrap(), None);

        storage
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_cursor")
            .unwrap();
        apply_message(&storage, &owner, SOURCE, second.clone(), pending.clone()).unwrap();
        assert_eq!(storage.cursor(&owner, SOURCE).unwrap(), Some(second));
        assert_eq!(storage.message(&owner, "two").unwrap(), Some(pending));
    }

    /// Проверяет границу 64-битного целого SQLite для номера позиции.
    #[test]
    fn accepts_bigint_max_and_rejects_out_of_range_cursor() {
        let storage = Storage::in_memory().unwrap();
        let owner = owner();
        let last = cursor("epoch", i64::MAX as u64);
        apply_message(
            &storage,
            &owner,
            SOURCE,
            last.clone(),
            message("last", "Last"),
        )
        .unwrap();
        assert!(matches!(
            apply_message(
                &storage,
                &owner,
                SOURCE,
                cursor("epoch", last.epoch_version + 1),
                message("overflow", "Overflow")
            ),
            Err(StorageError::CursorVersionOutOfRange)
        ));
        assert_eq!(storage.message(&owner, "overflow").unwrap(), None);
        assert_eq!(storage.cursor(&owner, SOURCE).unwrap(), Some(last));
    }
}
