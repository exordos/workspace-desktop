//! Хранит ссылки и байты аватаров по пользователю. Описывает чтение, запись,
//! вытеснение по давности использования и сверку после событий журнала.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::Type};
use uuid::Uuid;

use crate::context::OwnerContext;

use super::{Storage, StorageError};

const AVATAR_BYTE_BUDGET: i64 = 128 * 1024 * 1024;
const MIN_CHECK_STAMP_INTERVAL_SECS: i64 = 60 * 60;

/// Сведения об аватаре из события журнала Workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AvatarEvent<'a> {
    Snapshot {
        user_id: Uuid,              // Пользователь, чьё фото указано в событии.
        reference: Option<&'a str>, // Ссылка из профиля. None подтверждает отсутствие фото.
    },
    Reconcile {
        user_id: Uuid, // Пользователь, для которого сервер прислал неполный снимок.
    },
    FileDeleted {
        file_id: Uuid, // Удалённый файл, который мог использоваться как аватар.
    },
}

/// Версии, с которыми начался сетевой запрос для пользователя.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AvatarFence {
    reset_revision: i64, // Меняется при сбросе журнала владельца, даже для неизвестного UUID.
    revision: i64,       // Меняется при замене фото или запросе на сверку. 0 до первой записи.
}

/// Фото из локальной базы в исходном формате.
#[derive(Debug)]
pub(crate) struct AvatarBlob {
    pub format: String, // Сохранённый MIME изображения.
    pub bytes: Vec<u8>, // Проверенные при загрузке байты изображения.
}

/// Согласованный снимок строки аватара для чтения и условной записи.
#[derive(Debug)]
pub(crate) struct AvatarSnapshot {
    pub fence: AvatarFence,        // С этими версиями можно начать чтение из сети.
    pub reference: Option<String>, // Ссылка из профиля. None до первой сверки или при отсутствии фото.
    pub checked_at: Option<i64>,   // Unix-время сверки в секундах. None требует запроса профиля.
    pub image: Option<AvatarBlob>, // Отсутствует до загрузки или после вытеснения.
}

/// Добавляет индекс аватаров, их байты и отметки сброса в новый или существующий профиль.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS directory_avatar_state (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL,
            reset_revision INTEGER NOT NULL CHECK (reset_revision > 0),
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id)
         );
         CREATE TABLE IF NOT EXISTS directory_avatars (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, avatar_user_id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0), reference TEXT, checked_at INTEGER,
            format TEXT, bytes BLOB, last_used INTEGER NOT NULL DEFAULT 0,
            CHECK ((format IS NULL) = (bytes IS NULL)),
            CHECK (bytes IS NULL OR (reference IS NOT NULL AND length(bytes) > 0)),
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, avatar_user_id)
         );
         CREATE INDEX IF NOT EXISTS directory_avatar_images_by_use
            ON directory_avatars (last_used) WHERE bytes IS NOT NULL;
         CREATE INDEX IF NOT EXISTS directory_avatars_by_reference
            ON directory_avatars (account_id, server, organization_id, project_id, user_id, reference)
            WHERE reference IS NOT NULL;",
    )?;
    Ok(())
}

impl Storage {
    /// Захватывает версии перед запросом изменения фото.
    pub(crate) fn avatar_fence(
        &self,
        owner: &OwnerContext,
        user_id: Uuid,
    ) -> Result<AvatarFence, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        read_fence(&connection, owner, user_id)
    }

    /// Применяет подтверждение правки, если событие из журнала его ещё не обогнало.
    pub(crate) fn save_avatar_edit(
        &self,
        owner: &OwnerContext,
        user_id: Uuid,
        fence: AvatarFence,
        reference: Option<&str>,
    ) -> Result<bool, StorageError> {
        Ok(self
            .save_avatar_reference(owner, user_id, fence, reference, unix_seconds())?
            .is_some_and(|snapshot| snapshot.fence != fence))
    }

    /// Читает фото и его версии. Обращение к готовому фото учитывается при вытеснении.
    pub(crate) fn avatar_snapshot(
        &self,
        owner: &OwnerContext,
        user_id: Uuid,
        now: i64,
    ) -> Result<AvatarSnapshot, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let snapshot = read_snapshot(&transaction, owner, user_id)?;
        if snapshot.image.is_some() {
            transaction.execute(
                "UPDATE directory_avatars SET last_used = ?7
                 WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
                   AND project_id = ?4 AND user_id = ?5 AND avatar_user_id = ?6 AND last_used < ?7",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    user_id.to_string(),
                    now
                ],
            )?;
        }
        transaction.commit()?;
        Ok(snapshot)
    }

    /// Сохраняет ответ справочника, если после начала запроса ссылка и журнал не изменились.
    /// Возвращает новое состояние или None, если ответ опоздал.
    pub(crate) fn save_avatar_reference(
        &self,
        owner: &OwnerContext,
        user_id: Uuid,
        fence: AvatarFence,
        reference: Option<&str>,
        now: i64,
    ) -> Result<Option<AvatarSnapshot>, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_fence(&transaction, owner, user_id)? != fence {
            return Ok(None);
        }
        observe_reference(&transaction, owner, user_id, reference, now)?;
        let snapshot = read_snapshot(&transaction, owner, user_id)?;
        transaction.commit()?;
        Ok(Some(snapshot))
    }

    /// Сохраняет байты для подтверждённой ссылки, если версии с начала загрузки совпадают.
    pub(crate) fn save_avatar_bytes(
        &self,
        owner: &OwnerContext,
        user_id: Uuid,
        fence: AvatarFence,
        format: &str,
        bytes: &[u8],
        now: i64,
    ) -> Result<bool, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_fence(&transaction, owner, user_id)? != fence {
            return Ok(false);
        }
        let saved_row: Option<i64> = transaction.query_row(
            "UPDATE directory_avatars SET format = ?7, bytes = ?8, last_used = max(last_used, ?9)
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND avatar_user_id = ?6
               AND reference IS NOT NULL AND checked_at IS NOT NULL
             RETURNING rowid",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                user_id.to_string(),
                format,
                bytes,
                now
            ],
            |row| row.get(0),
        ).optional()?;
        if let Some(row_id) = saved_row {
            trim(&transaction, AVATAR_BYTE_BUDGET, Some(row_id))?;
        }
        transaction.commit()?;
        Ok(saved_row.is_some())
    }

    /// Проверяет версии без повторного чтения байтов перед выдачей аватара.
    pub(crate) fn avatar_fence_is_current(
        &self,
        owner: &OwnerContext,
        user_id: Uuid,
        fence: AvatarFence,
    ) -> Result<bool, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(read_fence(&connection, owner, user_id)? == fence)
    }
}

/// Применяет событие об аватаре в транзакции журнала и возвращает UUID для уведомления.
pub(super) fn apply(
    connection: &Connection,
    owner: &OwnerContext,
    event: AvatarEvent<'_>,
) -> Result<Vec<Uuid>, StorageError> {
    match event {
        AvatarEvent::Snapshot { user_id, reference } => {
            Ok(
                observe_reference(connection, owner, user_id, reference, unix_seconds())?
                    .then_some(user_id)
                    .into_iter()
                    .collect(),
            )
        }
        AvatarEvent::Reconcile { user_id } => {
            connection.execute(
                "INSERT INTO directory_avatars
                     (account_id, server, organization_id, project_id, user_id, avatar_user_id, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)
                 ON CONFLICT (account_id, server, organization_id, project_id, user_id, avatar_user_id)
                 DO UPDATE SET revision = directory_avatars.revision + 1, checked_at = NULL",
                params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id,
                    owner.user_id, user_id.to_string()],
            )?;
            Ok(vec![user_id])
        }
        AvatarEvent::FileDeleted { file_id } => remove_file(connection, owner, file_id),
    }
}

/// Убирает удалённый файл из кэша и закрывает запись из начатых ранее запросов.
fn remove_file(
    connection: &Connection,
    owner: &OwnerContext,
    file_id: Uuid,
) -> Result<Vec<Uuid>, StorageError> {
    let mut statement = connection.prepare(
        "UPDATE directory_avatars
         SET revision = revision + 1, reference = NULL, format = NULL, bytes = NULL, checked_at = ?7
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
           AND project_id = ?4 AND user_id = ?5 AND reference = ?6
         RETURNING avatar_user_id",
    )?;
    // Сервер может удалить файл отдельно, даже если поле аватара в профиле осталось прежним.
    let affected = statement.query_map(
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            format!("urn:image:{file_id}"),
            unix_seconds(),
        ],
        |row| {
            let user_id: String = row.get(0)?;
            Uuid::parse_str(&user_id).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
            })
        },
    )?;
    Ok(affected.collect::<Result<_, _>>()?)
}

/// Возвращает время сверки в секундах от Unix-эпохи.
fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Помечает аватары для сверки и отклоняет запросы, начатые до сброса журнала.
pub(super) fn reset(connection: &Connection, owner: &OwnerContext) -> Result<(), StorageError> {
    let owner_params = params![
        owner.account_id.0,
        owner.server,
        owner.organization_id,
        owner.project_id,
        owner.user_id,
    ];
    connection.execute(
        "INSERT INTO directory_avatar_state
             (account_id, server, organization_id, project_id, user_id, reset_revision)
         VALUES (?1, ?2, ?3, ?4, ?5, 1)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id)
         DO UPDATE SET reset_revision = directory_avatar_state.reset_revision + 1",
        owner_params,
    )?;
    connection.execute(
        "UPDATE directory_avatars SET checked_at = NULL
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
           AND project_id = ?4 AND user_id = ?5",
        owner_params,
    )?;
    Ok(())
}

/// Читает обе версии одним запросом. Для ещё неизвестного владельца или UUID версия равна нулю.
fn read_fence(
    connection: &Connection,
    owner: &OwnerContext,
    user_id: Uuid,
) -> Result<AvatarFence, StorageError> {
    Ok(connection.query_row(
        "SELECT COALESCE((SELECT reset_revision FROM directory_avatar_state
                 WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
                   AND project_id = ?4 AND user_id = ?5), 0),
                COALESCE((SELECT revision FROM directory_avatars
                 WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
                   AND project_id = ?4 AND user_id = ?5 AND avatar_user_id = ?6), 0)",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            user_id.to_string()
        ],
        |row| {
            Ok(AvatarFence {
                reset_revision: row.get(0)?,
                revision: row.get(1)?,
            })
        },
    )?)
}

/// Читает версии и содержимое аватара в одной транзакции.
fn read_snapshot(
    connection: &Connection,
    owner: &OwnerContext,
    user_id: Uuid,
) -> Result<AvatarSnapshot, StorageError> {
    let fence = read_fence(connection, owner, user_id)?;
    let (reference, checked_at, image) = connection
        .query_row(
            "SELECT reference, checked_at, format, bytes FROM directory_avatars
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
           AND project_id = ?4 AND user_id = ?5 AND avatar_user_id = ?6",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                user_id.to_string()
            ],
            |row| {
                let image = if let Some(format) = row.get(2)? {
                    Some(AvatarBlob {
                        format,
                        bytes: row.get(3)?,
                    })
                } else {
                    None
                };
                Ok((row.get(0)?, row.get(1)?, image))
            },
        )
        .optional()?
        .unwrap_or_default();
    Ok(AvatarSnapshot {
        fence,
        reference,
        checked_at,
        image,
    })
}

/// Записывает подтверждённую ссылку. При одинаковой ссылке сохраняет старые байты.
fn observe_reference(
    connection: &Connection,
    owner: &OwnerContext,
    user_id: Uuid,
    reference: Option<&str>,
    now: i64,
) -> Result<bool, StorageError> {
    let previous: Option<(Option<String>, Option<i64>)> = connection
        .query_row(
            "SELECT reference, checked_at FROM directory_avatars
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
           AND project_id = ?4 AND user_id = ?5 AND avatar_user_id = ?6",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                user_id.to_string()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let changed = previous.as_ref().is_none_or(|(previous, checked_at)| {
        previous.as_deref() != reference || checked_at.is_none()
    });
    // События о присутствии часто повторяют ту же ссылку. Время сверки достаточно обновлять раз в час.
    if !changed
        && previous
            .and_then(|(_, checked_at)| checked_at)
            .is_some_and(|stamp| {
                (0..MIN_CHECK_STAMP_INTERVAL_SECS).contains(&now.saturating_sub(stamp))
            })
    {
        return Ok(false);
    }
    connection.execute(
        "INSERT INTO directory_avatars
             (account_id, server, organization_id, project_id, user_id, avatar_user_id,
              revision, reference, checked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id, avatar_user_id)
         DO UPDATE SET revision = directory_avatars.revision + ?9,
            format = CASE WHEN reference IS excluded.reference THEN format END,
            bytes = CASE WHEN reference IS excluded.reference THEN bytes END,
            reference = excluded.reference, checked_at = excluded.checked_at",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            user_id.to_string(),
            reference,
            now,
            changed
        ],
    )?;
    Ok(changed)
}

/// Оставляет недавние фото в бюджете. Из записей одной секунды первой оставляет только что сохранённую.
fn trim(connection: &Connection, budget: i64, saved_row: Option<i64>) -> Result<(), StorageError> {
    connection.execute(
        "WITH recent_images AS (
             SELECT rowid, SUM(length(bytes)) OVER (
                 ORDER BY last_used DESC, (rowid IS ?2) DESC, rowid DESC
             ) AS used_bytes
             FROM directory_avatars WHERE bytes IS NOT NULL
         )
         UPDATE directory_avatars SET bytes = NULL, format = NULL
         WHERE rowid IN (SELECT rowid FROM recent_images WHERE used_bytes > ?1)",
        params![budget, saved_row],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
