//! Хранит черновики обычного текста в SQLite. Сначала описаны адрес и версия,
//! затем защищённые операции чтения, записи и отзыва доступа.

use std::cmp::Ordering;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use crate::context::OwnerContext;

use super::{Storage, StorageError};

#[cfg(test)]
mod tests;

/// Точный адрес черновика, в том числе для личного разговора.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DraftScope {
    pub stream_id: Uuid, // Стрим, которому принадлежит тема.
    pub topic_id: Uuid,  // Подтверждённая тема разговора.
}

/// Версия обычного ввода, подготовленная к локальному сохранению.
#[derive(Clone, Eq, PartialEq)]
pub struct PlaintextDraft {
    pub id: Uuid,         // Постоянный UUID черновика в пределах владельца.
    pub version: u64,     // Положительная локальная версия одного черновика.
    pub markdown: String, // Обычный текст точно в том виде, в каком его ввели.
}

/// Условия, при которых можно читать или записывать черновики одной области.
pub struct DraftAccess {
    owner: OwnerContext, // Владелец этих данных, включая аккаунт, сервер и проект.
    scope: DraftScope,   // Точная тема, захваченная до проверки доступа.
    revision: i64,       // Сумма версий сброса, отзыва стрима и отзыва темы.
}

/// Результат сохранения введённого снимка.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DraftWriteOutcome {
    Saved,           // Снимок создан или заменил более ранний.
    Unchanged,       // Такой снимок уже записан.
    StaleVersion,    // Записана более свежая версия черновика.
    VersionConflict, // Та же версия уже относится к другому тексту.
    ScopeConflict,   // UUID владельца уже используется в другом разговоре.
    Revoked,         // Текст удалён при отзыве доступа. Новый черновик требует своего UUID.
}

/// Добавляет таблицы черновиков и версий доступа к существующему профилю.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS messenger_drafts (
            sequence INTEGER PRIMARY KEY,
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, draft_id TEXT NOT NULL,
            stream_id TEXT NOT NULL, topic_id TEXT NOT NULL,
            local_version INTEGER NOT NULL CHECK (typeof(local_version) = 'integer' AND local_version > 0),
            markdown TEXT,
            UNIQUE (account_id, server, organization_id, project_id, user_id, draft_id)
         );
         CREATE INDEX IF NOT EXISTS messenger_drafts_by_scope
            ON messenger_drafts(account_id, server, organization_id, project_id, user_id, stream_id, topic_id, sequence)
            WHERE markdown IS NOT NULL;
         CREATE TABLE IF NOT EXISTS messenger_draft_access (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, stream_id TEXT NOT NULL, topic_id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision > 0),
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, stream_id, topic_id)
         );",
    )?;
    Ok(())
}

impl Storage {
    /// Захватывает локальную версию доступа до проверки разрешений в мессенджере.
    /// Вызывающий проверяет участие и поколение входа до использования снимка.
    pub fn prepare_draft_access(
        &self,
        owner: &OwnerContext,
        scope: DraftScope,
    ) -> Result<DraftAccess, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(DraftAccess {
            owner: owner.clone(),
            scope,
            revision: access_revision(&connection, owner, scope)?,
        })
    }

    /// Читает указанный UUID или первый сохранённый черновик темы для обычного открытия редактора.
    pub fn read_plaintext_draft(
        &self,
        access: &DraftAccess,
        draft_id: Option<Uuid>,
    ) -> Result<Option<PlaintextDraft>, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        access.verify(&transaction)?;
        let owner = &access.owner;
        let row = transaction
            .query_row(
                "SELECT draft_id, local_version, markdown FROM messenger_drafts
                 WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
                 AND project_id = ?4 AND user_id = ?5 AND stream_id = ?6 AND topic_id = ?7
                 AND markdown IS NOT NULL AND (?8 IS NULL OR draft_id = ?8)
                 ORDER BY sequence LIMIT 1",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    access.scope.stream_id.to_string(),
                    access.scope.topic_id.to_string(),
                    draft_id.map(|id| id.to_string())
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        transaction.commit()?;
        row.map(|(id, version, markdown)| {
            Ok(PlaintextDraft {
                id: Uuid::parse_str(&id).map_err(|_| StorageError::InvalidDraftRecord)?,
                version: u64::try_from(version).map_err(|_| StorageError::InvalidDraftRecord)?,
                markdown,
            })
        })
        .transpose()
    }

    /// Сохраняет обычный снимок вместе с версией. Пустой текст фиксирует очищенный редактор.
    /// Вызывающий передаёт только разрешённый к хранению текст действующего входа.
    pub fn save_plaintext_draft(
        &self,
        access: &DraftAccess,
        draft: &PlaintextDraft,
    ) -> Result<DraftWriteOutcome, StorageError> {
        let version = i64::try_from(draft.version)
            .ok()
            .filter(|version| *version > 0)
            .ok_or(StorageError::InvalidDraftVersion)?;
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        access.verify(&transaction)?;

        let owner = &access.owner;
        let id = draft.id.to_string();
        let stream = access.scope.stream_id.to_string();
        let topic = access.scope.topic_id.to_string();
        let previous = transaction
            .query_row(
                "SELECT stream_id, topic_id, local_version, markdown FROM messenger_drafts
                 WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
                 AND project_id = ?4 AND user_id = ?5 AND draft_id = ?6",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    id
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        if let Some((old_stream, old_topic, old_version, text)) = previous {
            if old_stream != stream || old_topic != topic {
                return Ok(DraftWriteOutcome::ScopeConflict);
            }
            let Some(text) = text else {
                return Ok(DraftWriteOutcome::Revoked);
            };
            match version.cmp(&old_version) {
                Ordering::Less => return Ok(DraftWriteOutcome::StaleVersion),
                Ordering::Equal if text == draft.markdown => {
                    return Ok(DraftWriteOutcome::Unchanged);
                }
                Ordering::Equal => return Ok(DraftWriteOutcome::VersionConflict),
                Ordering::Greater => {}
            }
        }
        transaction.execute(
            "INSERT INTO messenger_drafts (account_id, server, organization_id, project_id,
             user_id, draft_id, stream_id, topic_id, local_version, markdown)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (account_id, server, organization_id, project_id, user_id, draft_id)
             DO UPDATE SET local_version = excluded.local_version, markdown = excluded.markdown",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                id,
                stream,
                topic,
                version,
                draft.markdown
            ],
        )?;
        transaction.commit()?;
        Ok(DraftWriteOutcome::Saved)
    }
}

impl DraftAccess {
    /// Проверяет, что область не отзывалась и журнал не сбрасывался после захвата.
    fn verify(&self, connection: &Connection) -> Result<(), StorageError> {
        if access_revision(connection, &self.owner, self.scope)? != self.revision {
            return Err(StorageError::StaleDraftAccess);
        }
        Ok(())
    }
}

/// Суммирует счётчики только для владельца и вложенных областей этого черновика.
fn access_revision(
    connection: &Connection,
    owner: &OwnerContext,
    scope: DraftScope,
) -> Result<i64, StorageError> {
    Ok(connection.query_row(
        "SELECT COALESCE(SUM(revision), 0) FROM messenger_draft_access
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
         AND project_id = ?4 AND user_id = ?5
         AND (stream_id = '' OR (stream_id = ?6 AND (topic_id = '' OR topic_id = ?7)))",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            scope.stream_id.to_string(),
            scope.topic_id.to_string()
        ],
        |row| row.get(0),
    )?)
}

/// Отклоняет отложенные операции владельца после сброса журнала, сохраняя его ввод.
pub(super) fn invalidate_owner(
    connection: &Connection,
    owner: &OwnerContext,
) -> Result<(), StorageError> {
    advance_access(connection, owner, "", "")
}

/// Удаляет черновики отозванной области и отклоняет её отложенные операции.
pub(super) fn revoke(
    connection: &Connection,
    owner: &OwnerContext,
    stream: &str,
    topic: Option<&str>,
) -> Result<(), StorageError> {
    // Пустой идентификатор зарезервирован для сброса владельца или всего стрима.
    let Ok(stream) = Uuid::parse_str(stream) else {
        return Ok(());
    };
    let stream = stream.to_string();
    let topic = match topic.map(Uuid::parse_str).transpose() {
        Ok(topic) => topic.map(|id| id.to_string()).unwrap_or_default(),
        Err(_) => return Ok(()),
    };
    advance_access(connection, owner, &stream, &topic)?;
    connection.execute(
        // Оставляем только идентичность записи, чтобы новый вход не восстановил отозванный текст под старым UUID.
        "UPDATE messenger_drafts SET markdown = NULL WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
         AND project_id = ?4 AND user_id = ?5 AND stream_id = ?6 AND (?7 = '' OR topic_id = ?7)",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            stream,
            topic
        ],
    )?;
    Ok(())
}

/// Увеличивает версию отозванной области в транзакции синхронизации.
fn advance_access(
    connection: &Connection,
    owner: &OwnerContext,
    stream: &str,
    topic: &str,
) -> Result<(), StorageError> {
    connection.execute(
        "INSERT INTO messenger_draft_access (account_id, server, organization_id, project_id,
         user_id, stream_id, topic_id, revision) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id, stream_id, topic_id)
         DO UPDATE SET revision = revision + 1",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            stream,
            topic
        ],
    )?;
    Ok(())
}
