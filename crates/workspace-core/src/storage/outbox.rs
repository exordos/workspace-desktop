//! Сохраняет подготовленные сообщения до первого запроса. Здесь описаны запись, таблица и операции очереди.

use rusqlite::{Connection, params};
use uuid::Uuid;

use super::{Storage, StorageError};
use crate::context::OwnerContext;

/// Подготовленное текстовое сообщение, которое ждёт первого запроса.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedSend {
    pub canonical_id: Uuid, // UUID для тела запроса.
    pub public_id: Uuid,    // UUID размещения для сверки с историей.
    pub stream_id: Uuid,    // Исходный стрим.
    pub topic_id: Uuid,     // Исходная тема.
    pub markdown: String,   // Неизменное содержимое payload вида markdown.
}

/// Создаёт отдельную таблицу очереди, которая не зависит от кэша источника.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS messenger_outbox (
            account_id TEXT NOT NULL,
            server TEXT NOT NULL,
            organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            canonical_id BLOB NOT NULL,
            public_id BLOB NOT NULL,
            stream_id BLOB NOT NULL,
            topic_id BLOB NOT NULL,
            markdown TEXT NOT NULL,
            state TEXT NOT NULL CHECK (state = 'pending'),
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, canonical_id)
        );",
    )?;
    Ok(())
}

impl Storage {
    /// Сохраняет подготовленный текст. Возвращает false, если такой ключ уже занят.
    pub fn save_prepared_send(
        &self,
        owner: &OwnerContext,
        message: &PreparedSend,
    ) -> Result<bool, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        // Первый вариант остаётся в очереди даже при повторе с другим содержимым.
        let inserted = connection.execute(
            "INSERT INTO messenger_outbox (
                account_id, server, organization_id, project_id, user_id,
                canonical_id, public_id, stream_id, topic_id, markdown, state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending')
             ON CONFLICT (account_id, server, organization_id, project_id, user_id, canonical_id)
             DO NOTHING",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                message.canonical_id.as_bytes(),
                message.public_id.as_bytes(),
                message.stream_id.as_bytes(),
                message.topic_id.as_bytes(),
                message.markdown
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Возвращает только ожидающие записи указанного владельца, в порядке добавления.
    pub fn pending_sends(&self, owner: &OwnerContext) -> Result<Vec<PreparedSend>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT canonical_id, public_id, stream_id, topic_id, markdown
             FROM messenger_outbox
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND state = 'pending'
             ORDER BY rowid",
        )?;
        let rows = statement.query_map(
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id
            ],
            |row| {
                Ok(PreparedSend {
                    canonical_id: Uuid::from_bytes(row.get(0)?),
                    public_id: Uuid::from_bytes(row.get(1)?),
                    stream_id: Uuid::from_bytes(row.get(2)?),
                    topic_id: Uuid::from_bytes(row.get(3)?),
                    markdown: row.get(4)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
