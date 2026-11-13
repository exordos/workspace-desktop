//! Хранит сообщения, счётчики и позиции чтения в SQLite.
//! Сначала описаны данные и таблицы, затем чтение и запись.

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
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error), // Исходная ошибка SQLite.
}

/// Локальное хранилище сообщений, счётчиков и позиций чтения.
pub struct Storage {
    connection: Mutex<Connection>, // Соединение с SQLite. Блокировка выдаёт его по одному запросу за раз.
}

impl Storage {
    /// Открывает базу по пути и создаёт таблицы при необходимости.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::initialize(Connection::open(path)?)
    }

    /// Создаёт базу в памяти для работы без файла.
    pub fn in_memory() -> Result<Self, StorageError> {
        Self::initialize(Connection::open_in_memory()?)
    }

    /// Подготавливает таблицы сообщений, счётчиков и позиций для переданного соединения.
    fn initialize(connection: Connection) -> Result<Self, StorageError> {
        connection.execute_batch(
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
        Ok(Self {
            connection: Mutex::new(connection),
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
        message: Option<&Message>,
        unread: Option<u32>,
        notification_conversation: Option<&str>,
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

        let message_inserted = if let Some(message) = message {
            transaction.execute(
                "INSERT INTO sync_messages
                 (account_id, server, organization_id, project_id, user_id, message_id, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
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
                ],
            )? != 0
        } else {
            false
        };
        // Цель уведомления доступна вместе с новым сообщением и его курсором.
        if message_inserted
            && let (Some(message), Some(conversation_id)) = (message, notification_conversation)
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
        let unread_changed = match unread {
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
        Ok(AppliedEvent {
            outcome: ApplyOutcome::Applied,
            message_inserted,
            unread_changed,
        })
    }
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
            .apply(owner, source, &cursor, Some(&message), None, None)
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
