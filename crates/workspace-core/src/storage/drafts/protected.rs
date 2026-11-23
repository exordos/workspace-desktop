//! Очищает обычный черновик при появлении защиты и сохраняет адрес серверного удаления.

use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::context::OwnerContext;

use super::{DraftAccess, DraftScope, DraftVersion, Storage, StorageError};

/// Добавляет только флаг незавершённого серверного удаления к существующим адресам.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    let has_pending_purge: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('messenger_drafts')
          WHERE name = 'pending_server_purge')",
        [],
        |row| row.get(0),
    )?;
    if !has_pending_purge {
        connection.execute_batch(
            "ALTER TABLE messenger_drafts ADD COLUMN pending_server_purge
              INTEGER NOT NULL DEFAULT 0 CHECK(pending_server_purge IN (0, 1));",
        )?;
    }
    Ok(())
}

impl Storage {
    /// Убирает локальный текст и ответы одной версии или фиксирует отзыв раньше её записи.
    /// Возвращает false при конфликте области или уже сохранённом новом вводе.
    pub(crate) fn protect_plaintext_draft(
        &self,
        access: &DraftAccess,
        version: DraftVersion,
    ) -> Result<bool, StorageError> {
        let version_number = sqlite_version(version)?;
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        access.verify(&transaction)?;
        let owner = &access.owner;
        let changed = transaction.execute(
            "INSERT INTO messenger_drafts (account_id, server, organization_id, project_id, user_id,
                draft_id, stream_id, topic_id, local_version, pending_server_purge)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1)
             ON CONFLICT(account_id, server, organization_id, project_id, user_id, draft_id)
             DO UPDATE SET local_version = excluded.local_version, pending_server_purge = 1,
                           markdown = NULL, reply_context = NULL
             WHERE messenger_drafts.stream_id = excluded.stream_id
               AND messenger_drafts.topic_id = excluded.topic_id
               AND (messenger_drafts.local_version < excluded.local_version
                   OR (messenger_drafts.local_version = excluded.local_version
                       AND messenger_drafts.markdown IS NOT NULL))",
            params![
                owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
                version.id.to_string(), access.scope.stream_id.to_string(), access.scope.topic_id.to_string(),
                version_number,
            ],
        )?;
        // Повтор той же команды не восстанавливает уже закрытую сетевую очередь.
        let recorded = changed == 1
            || transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM messenger_drafts
              WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
                AND user_id = ?5 AND draft_id = ?6 AND stream_id = ?7 AND topic_id = ?8
                AND local_version = ?9 AND markdown IS NULL)",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    version.id.to_string(),
                    access.scope.stream_id.to_string(),
                    access.scope.topic_id.to_string(),
                    version_number,
                ],
                |row| row.get(0),
            )?;
        transaction.commit()?;
        Ok(recorded)
    }

    /// Читает только адреса и локальные версии незавершённых удалений текущего владельца.
    pub(crate) fn pending_protected_draft_purges(
        &self,
        owner: &OwnerContext,
    ) -> Result<Vec<(DraftScope, DraftVersion)>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut query = connection.prepare(
            "SELECT draft_id, stream_id, topic_id, local_version FROM messenger_drafts
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
               AND user_id = ?5 AND pending_server_purge = 1 AND markdown IS NULL
             ORDER BY sequence",
        )?;
        let mut rows = query.query(params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
        ])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let id = Uuid::parse_str(&row.get::<_, String>(0)?)
                .map_err(|_| StorageError::InvalidDraftRecord)?;
            let stream_id = Uuid::parse_str(&row.get::<_, String>(1)?)
                .map_err(|_| StorageError::InvalidDraftRecord)?;
            let topic_id = Uuid::parse_str(&row.get::<_, String>(2)?)
                .map_err(|_| StorageError::InvalidDraftRecord)?;
            result.push((
                DraftScope {
                    stream_id,
                    topic_id,
                },
                DraftVersion {
                    id,
                    version: u64::try_from(row.get::<_, i64>(3)?)
                        .map_err(|_| StorageError::InvalidDraftRecord)?,
                },
            ));
        }
        Ok(result)
    }

    /// Подтверждает серверное удаление, оставляя отзыв UUID для опоздавшей локальной записи.
    pub(crate) fn confirm_protected_draft_purge(
        &self,
        owner: &OwnerContext,
        scope: DraftScope,
        version: DraftVersion,
    ) -> Result<(), StorageError> {
        let version_number = sqlite_version(version)?;
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        connection.execute(
            "UPDATE messenger_drafts SET pending_server_purge = 0
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
               AND user_id = ?5 AND draft_id = ?6 AND stream_id = ?7 AND topic_id = ?8
               AND local_version = ?9 AND markdown IS NULL",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                version.id.to_string(),
                scope.stream_id.to_string(),
                scope.topic_id.to_string(),
                version_number,
            ],
        )?;
        Ok(())
    }
}

/// Проверяет положительную версию перед записью в целое SQLite.
fn sqlite_version(version: DraftVersion) -> Result<i64, StorageError> {
    i64::try_from(version.version)
        .ok()
        .filter(|value| *value > 0)
        .ok_or(StorageError::InvalidDraftVersion)
}
