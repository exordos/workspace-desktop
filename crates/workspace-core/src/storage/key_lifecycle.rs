//! Хранит намерения создания и ротации по владельцу и устройству.
//! Записывает только параметры, UUID и этапы операций. Ключи остаются в защищённом адаптере.

use rusqlite::{OptionalExtension, Row, params};
use uuid::Uuid;

use super::{Storage, StorageError};
use crate::{
    context::OwnerContext,
    messenger::encryption::lifecycle::{KeyLifecycleIntent, PendingKeyLifecycle},
};

/// Этап, записанный до следующего необратимого шага.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LifecyclePhase {
    Prepared = 0,   // Намерение записано, сетевой запрос ещё не разрешён.
    Dispatched = 1, // Запрос мог достичь сервера, результат нужно сверить.
    Verified = 2,   // Сервер подтвердил создание или размещение объявления.
    Complete = 3,   // Локальная запись и последующие действия завершены.
}

/// Строка операции вместе с результатом серверной сверки.
pub(crate) struct StoredKeyLifecycle {
    pub operation: PendingKeyLifecycle, // UUID операции, стрима, ключа и исходные параметры.
    pub phase: LifecyclePhase,          // Последний сохранённый этап.
    pub verified_topic_id: Option<Uuid>, // Проверенная тема, если сервер подтвердил операцию.
}

/// Готовит журнал операций, независимый от сброса синхронизации и сообщений.
pub(super) fn initialize(connection: &rusqlite::Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS messenger_key_lifecycle (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, device_id BLOB NOT NULL,
            operation_id BLOB NOT NULL, stream_id BLOB NOT NULL, key_id BLOB NOT NULL,
            intent TEXT NOT NULL, phase INTEGER NOT NULL CHECK (phase BETWEEN 0 AND 3),
            topic_id BLOB CHECK (phase < 2 OR (topic_id IS NOT NULL AND length(topic_id) = 16)),
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, device_id, operation_id)
        );",
    )?;
    Ok(())
}

/// Восстанавливает точный снимок операции из журнала.
fn stored_lifecycle(row: &Row<'_>) -> rusqlite::Result<StoredKeyLifecycle> {
    let intent =
        serde_json::from_str::<KeyLifecycleIntent>(&row.get::<_, String>(3)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    let phase = match row.get::<_, u8>(4)? {
        0 => LifecyclePhase::Prepared,
        1 => LifecyclePhase::Dispatched,
        2 => LifecyclePhase::Verified,
        3 => LifecyclePhase::Complete,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(StoredKeyLifecycle {
        operation: PendingKeyLifecycle {
            operation_id: Uuid::from_bytes(row.get(0)?),
            stream_id: Uuid::from_bytes(row.get(1)?),
            key_id: Uuid::from_bytes(row.get(2)?),
            intent,
        },
        phase,
        verified_topic_id: row.get::<_, Option<[u8; 16]>>(5)?.map(Uuid::from_bytes),
    })
}

impl Storage {
    /// Резервирует UUID и параметры операции до работы с ключом и сетью.
    pub(crate) fn stage_key_lifecycle(
        &self,
        owner: &OwnerContext,
        device_id: Uuid,
        operation: &PendingKeyLifecycle,
    ) -> Result<bool, StorageError> {
        let intent = serde_json::to_string(&operation.intent)
            .map_err(|_| StorageError::InvalidKeyLifecycle)?;
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(connection.execute(
            "INSERT INTO messenger_key_lifecycle
             (account_id, server, organization_id, project_id, user_id, device_id,
              operation_id, stream_id, key_id, intent, phase)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0) ON CONFLICT DO NOTHING",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                device_id.as_bytes(),
                operation.operation_id.as_bytes(),
                operation.stream_id.as_bytes(),
                operation.key_id.as_bytes(),
                intent,
            ],
        )? == 1)
    }

    /// Читает одну операцию только у того же владельца и устройства.
    pub(crate) fn key_lifecycle(
        &self,
        owner: &OwnerContext,
        device_id: Uuid,
        operation_id: Uuid,
    ) -> Result<Option<StoredKeyLifecycle>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(connection
            .query_row(
                "SELECT operation_id, stream_id, key_id, intent, phase, topic_id
             FROM messenger_key_lifecycle
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
               AND user_id = ?5 AND device_id = ?6 AND operation_id = ?7",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    device_id.as_bytes(),
                    operation_id.as_bytes()
                ],
                stored_lifecycle,
            )
            .optional()?)
    }

    /// Возвращает незавершённые операции в порядке создания для восстановления экрана.
    pub(crate) fn pending_key_lifecycle(
        &self,
        owner: &OwnerContext,
        device_id: Uuid,
    ) -> Result<Vec<PendingKeyLifecycle>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut query = connection.prepare(
            "SELECT operation_id, stream_id, key_id, intent, phase, topic_id
             FROM messenger_key_lifecycle
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
               AND user_id = ?5 AND device_id = ?6 AND phase < 3 ORDER BY rowid",
        )?;
        Ok(query
            .query_map(
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    device_id.as_bytes()
                ],
                |row| Ok(stored_lifecycle(row)?.operation),
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Меняет этап только у той версии операции, которую начал вызывающий код.
    pub(crate) fn advance_key_lifecycle(
        &self,
        owner: &OwnerContext,
        device_id: Uuid,
        operation_id: Uuid,
        expected: LifecyclePhase,
        phase: LifecyclePhase,
        topic_id: Option<Uuid>,
    ) -> Result<bool, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(connection.execute(
            "UPDATE messenger_key_lifecycle SET phase = ?9, topic_id = ?10
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
               AND user_id = ?5 AND device_id = ?6 AND operation_id = ?7 AND phase = ?8",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                device_id.as_bytes(),
                operation_id.as_bytes(),
                expected as u8,
                phase as u8,
                topic_id.map(|id| *id.as_bytes())
            ],
        )? == 1)
    }

    /// Убирает только точное отправленное намерение, которое сервер окончательно отклонил.
    pub(crate) fn discard_rejected_creation(
        &self,
        owner: &OwnerContext,
        device_id: Uuid,
        operation: &PendingKeyLifecycle,
    ) -> Result<bool, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(connection.execute(
            "DELETE FROM messenger_key_lifecycle
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
               AND user_id = ?5 AND device_id = ?6 AND operation_id = ?7
               AND stream_id = ?8 AND key_id = ?9 AND phase = ?10",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                device_id.as_bytes(),
                operation.operation_id.as_bytes(),
                operation.stream_id.as_bytes(),
                operation.key_id.as_bytes(),
                LifecyclePhase::Dispatched as u8,
            ],
        )? == 1)
    }
}
