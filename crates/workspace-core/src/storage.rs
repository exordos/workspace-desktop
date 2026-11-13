use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

use crate::context::OwnerContext;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cursor {
    /// The source issues an opaque epoch identifier. Only versions within one epoch are ordered.
    pub epoch_generation: String,
    pub epoch_version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Applied,
    Duplicate,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("the source epoch changed and requires recovery")]
    EpochChanged,
    #[error("the source cursor version exceeds the supported range")]
    CursorVersionOutOfRange,
    #[error("the storage connection lock is poisoned")]
    Poisoned,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub struct Storage {
    connection: Mutex<Connection>,
}

impl Storage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::initialize(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, StorageError> {
        Self::initialize(Connection::open_in_memory()?)
    }

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
            );",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn cursor(
        &self,
        owner: &OwnerContext,
        source: &str,
    ) -> Result<Option<Cursor>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        read_cursor(&connection, owner, source)
    }

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

    pub(crate) fn apply_message(
        &self,
        owner: &OwnerContext,
        source: &str,
        cursor: Cursor,
        message: Message,
    ) -> Result<ApplyOutcome, StorageError> {
        let epoch_version = i64::try_from(cursor.epoch_version)
            .map_err(|_| StorageError::CursorVersionOutOfRange)?;
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(previous) = read_cursor(&transaction, owner, source)? {
            if previous.epoch_generation != cursor.epoch_generation {
                return Err(StorageError::EpochChanged);
            }
            if previous.epoch_version >= cursor.epoch_version {
                return Ok(ApplyOutcome::Duplicate);
            }
        }

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
        )?;
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
        Ok(ApplyOutcome::Applied)
    }
}

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

    const SOURCE: &str = "messenger";

    fn owner() -> OwnerContext {
        OwnerContext {
            account_id: AccountId("account".into()),
            server: "workspace.test".into(),
            organization_id: "organization".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        }
    }

    fn cursor(epoch_generation: &str, epoch_version: u64) -> Cursor {
        Cursor {
            epoch_generation: epoch_generation.into(),
            epoch_version,
        }
    }

    fn message(id: &str, text: &str) -> Message {
        Message {
            id: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn preserves_cursor_and_messages_on_duplicate_or_epoch_change() {
        let storage = Storage::in_memory().unwrap();
        let owner = owner();
        let first = message("first", "First");
        let position = cursor("epoch-a", 10);
        assert_eq!(
            storage
                .apply_message(&owner, SOURCE, position.clone(), first.clone())
                .unwrap(),
            ApplyOutcome::Applied
        );
        assert_eq!(
            storage
                .apply_message(
                    &owner,
                    SOURCE,
                    position.clone(),
                    message("second", "Duplicate")
                )
                .unwrap(),
            ApplyOutcome::Duplicate
        );
        assert!(matches!(
            storage.apply_message(
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

    #[test]
    fn failed_cursor_write_rolls_back_message_and_allows_replay() {
        let storage = Storage::in_memory().unwrap();
        let owner = owner();
        let first = cursor("epoch", 1);
        let second = cursor("epoch", 2);
        storage
            .apply_message(&owner, SOURCE, first.clone(), message("one", "One"))
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
            storage.apply_message(&owner, SOURCE, second.clone(), pending.clone()),
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
        storage
            .apply_message(&owner, SOURCE, second.clone(), pending.clone())
            .unwrap();
        assert_eq!(storage.cursor(&owner, SOURCE).unwrap(), Some(second));
        assert_eq!(storage.message(&owner, "two").unwrap(), Some(pending));
    }

    #[test]
    fn accepts_bigint_max_and_rejects_out_of_range_cursor() {
        let storage = Storage::in_memory().unwrap();
        let owner = owner();
        let last = cursor("epoch", i64::MAX as u64);
        storage
            .apply_message(&owner, SOURCE, last.clone(), message("last", "Last"))
            .unwrap();
        assert!(matches!(
            storage.apply_message(
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
