//! Хранит серверные счётчики строк отдельно от списка разговоров.
//! Сначала идут таблицы и запись значений, затем применение счётчиков к снимку.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    context::OwnerContext,
    messenger::catalog::{CatalogCounts, Chat},
};

use super::{Cursor, StorageError, read_chat_catalog};

/// Создаёт таблицы счётчиков и позиции, учтённой снимком REST.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS messenger_catalog_fence (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL,
            epoch_generation TEXT NOT NULL, epoch_version INTEGER NOT NULL,
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id)
        );
        CREATE TABLE IF NOT EXISTS messenger_catalog_checkpoint (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL,
            epoch_generation TEXT NOT NULL, epoch_version INTEGER NOT NULL,
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id)
        );
        CREATE TABLE IF NOT EXISTS messenger_catalog_counts (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL,
            stream_id TEXT NOT NULL, topic_id TEXT NOT NULL,
            epoch_generation TEXT NOT NULL, epoch_version INTEGER NOT NULL,
            total INTEGER NOT NULL, active INTEGER NOT NULL, passive INTEGER NOT NULL,
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, stream_id, topic_id)
        );",
    )?;
    Ok(())
}

/// Сохраняет начало журнала, перед которым события больше не будут доставлены.
pub(super) fn record_checkpoint(
    connection: &Connection,
    owner: &OwnerContext,
    cursor: &Cursor,
) -> Result<(), StorageError> {
    let version =
        i64::try_from(cursor.epoch_version).map_err(|_| StorageError::CursorVersionOutOfRange)?;
    connection.execute(
        "INSERT INTO messenger_catalog_checkpoint VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id)
         DO UPDATE SET epoch_generation = excluded.epoch_generation, epoch_version = excluded.epoch_version",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id,
                owner.user_id, cursor.epoch_generation, version],
    )?;
    Ok(())
}

/// Определяет, мог ли первый курсор пропустить события для сохранённого каталога.
pub(super) fn needs_reconciliation(
    connection: &Connection,
    owner: &OwnerContext,
) -> Result<bool, StorageError> {
    Ok(connection.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM messenger_catalog_checkpoint AS checkpoint
             LEFT JOIN messenger_catalog_fence AS fence USING (account_id, server, organization_id, project_id, user_id)
             WHERE checkpoint.account_id = ?1 AND checkpoint.server = ?2 AND checkpoint.organization_id = ?3
               AND checkpoint.project_id = ?4 AND checkpoint.user_id = ?5
               AND (fence.epoch_generation IS NULL OR fence.epoch_generation <> checkpoint.epoch_generation
                    OR fence.epoch_version < checkpoint.epoch_version)
         )",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id],
        |row| row.get(0),
    )?)
}

/// Отклоняет ответ, который начали получать раньше позиции первого подключения.
pub(super) fn precedes_checkpoint(
    connection: &Connection,
    owner: &OwnerContext,
    fence: Option<&Cursor>,
) -> Result<bool, StorageError> {
    let checkpoint = connection
        .query_row(
            "SELECT epoch_generation, epoch_version FROM messenger_catalog_checkpoint
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
           AND project_id = ?4 AND user_id = ?5",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    Ok(checkpoint.is_some_and(|(epoch, version)| {
        fence.is_none_or(|fence| {
            fence.epoch_generation != epoch || fence.epoch_version < version as u64
        })
    }))
}

/// Запоминает позицию REST и удаляет уже учтённые REST-снимком счётчики.
pub(super) fn save_fence(
    connection: &Connection,
    owner: &OwnerContext,
    fence: &Cursor,
) -> Result<(), StorageError> {
    let version =
        i64::try_from(fence.epoch_version).map_err(|_| StorageError::CursorVersionOutOfRange)?;
    let values = params![
        owner.account_id.0,
        owner.server,
        owner.organization_id,
        owner.project_id,
        owner.user_id,
        fence.epoch_generation,
        version
    ];
    connection.execute(
        "INSERT INTO messenger_catalog_fence VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id)
         DO UPDATE SET epoch_generation = excluded.epoch_generation, epoch_version = excluded.epoch_version",
        values,
    )?;
    connection.execute(
        "DELETE FROM messenger_catalog_counts
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
           AND project_id = ?4 AND user_id = ?5 AND (epoch_generation <> ?6 OR epoch_version <= ?7)",
        values,
    )?;
    Ok(())
}

/// Записывает абсолютные значения только для событий, ещё не учтённых снимком REST.
pub(super) fn apply(
    connection: &Connection,
    owner: &OwnerContext,
    cursor: &Cursor,
    counts: &CatalogCounts,
) -> Result<bool, StorageError> {
    let version =
        i64::try_from(cursor.epoch_version).map_err(|_| StorageError::CursorVersionOutOfRange)?;
    let changed = connection.execute(
        "INSERT INTO messenger_catalog_counts
         SELECT ?1, ?2, ?3, ?4, ?5, ?8, ?9, ?6, ?7, ?10, ?11, ?12
         WHERE NOT EXISTS (
             SELECT 1 FROM messenger_catalog_fence
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND epoch_generation = ?6 AND epoch_version >= ?7
         )
         ON CONFLICT (account_id, server, organization_id, project_id, user_id, stream_id, topic_id)
         DO UPDATE SET epoch_generation = excluded.epoch_generation, epoch_version = excluded.epoch_version,
                       total = excluded.total, active = excluded.active, passive = excluded.passive",
        params![
            owner.account_id.0, owner.server, owner.organization_id, owner.project_id,
            owner.user_id, cursor.epoch_generation, version, counts.stream_id.to_string(),
            counts.topic_id.map(|id| id.to_string()).unwrap_or_default(),
            counts.total, counts.active, counts.passive
        ],
    )? > 0;
    Ok(changed)
}

/// Читает прежнее полное число для той же строки, включая исходный REST-снимок.
pub(super) fn previous_total(
    connection: &Connection,
    owner: &OwnerContext,
    counts: &CatalogCounts,
) -> Result<Option<i64>, StorageError> {
    let stream = counts.stream_id.to_string();
    let topic = counts.topic_id.map(|id| id.to_string()).unwrap_or_default();
    let address = params![
        owner.account_id.0,
        owner.server,
        owner.organization_id,
        owner.project_id,
        owner.user_id,
        stream,
        topic,
    ];
    let total = connection
        .query_row(
            "SELECT total FROM messenger_catalog_counts
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
             AND user_id = ?5 AND stream_id = ?6 AND topic_id = ?7",
            address,
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if total.is_some() {
        return Ok(total);
    }

    // У первого события ещё нет отдельной строки счётчиков. Берём известное число
    // из уже сохранённого каталога, не разбирая список всех разговоров в памяти.
    Ok(connection
        .query_row(
            "SELECT CAST(CASE WHEN ?7 = '' THEN json_extract(chat.value, '$.metadata.unread_count')
                    ELSE (SELECT json_extract(topic.value, '$.metadata.unread_count')
                          FROM json_each(chat.value, '$.topics') AS topic
                          WHERE json_extract(topic.value, '$.id') = ?7) END AS INTEGER)
             FROM messenger_catalog, json_each(messenger_catalog.snapshot) AS chat
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
             AND user_id = ?5 AND json_extract(chat.value, '$.id') = ?6",
            address,
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

/// Удаляет счётчики отозванного канала или одной темы по тому же адресу, что и каталог.
pub(super) fn remove_scope(
    connection: &Connection,
    owner: &OwnerContext,
    stream: &str,
    topic: Option<&str>,
) -> Result<(), StorageError> {
    connection.execute(
        "DELETE FROM messenger_catalog_counts
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
           AND project_id = ?4 AND user_id = ?5 AND stream_id = ?6
           AND (?7 IS NULL OR topic_id = ?7)",
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

/// Читает существующие строки каталога и подставляет новые счётчики из журнала.
pub(super) fn read_catalog(
    connection: &Connection,
    owner: &OwnerContext,
) -> Result<Option<Vec<Chat>>, StorageError> {
    let Some(snapshot) = read_chat_catalog(connection, owner)? else {
        return Ok(None);
    };
    let mut chats: Vec<Chat> = serde_json::from_str(&snapshot)?;
    let mut statement = connection.prepare_cached(
        "SELECT counts.stream_id, counts.topic_id, counts.total, counts.active, counts.passive
         FROM messenger_catalog_counts AS counts
         JOIN messenger_catalog_fence AS fence USING (account_id, server, organization_id, project_id, user_id)
         WHERE counts.account_id = ?1 AND counts.server = ?2 AND counts.organization_id = ?3
           AND counts.project_id = ?4 AND counts.user_id = ?5
           AND counts.epoch_generation = fence.epoch_generation AND counts.epoch_version > fence.epoch_version",
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
            let key = (row.get::<_, String>(0)?, row.get::<_, String>(1)?);
            let counts = (
                Some(u64::from(row.get::<_, u32>(2)?)),
                Some(u64::from(row.get::<_, u32>(3)?)),
                Some(u64::from(row.get::<_, u32>(4)?)),
            );
            Ok((key, counts))
        },
    )?;
    let mut counts = rows.collect::<Result<HashMap<_, _>, _>>()?;
    for chat in &mut chats {
        let stream_id = chat.id.to_string();
        let key = (stream_id.clone(), String::new());
        if let Some(counts) = counts.remove(&key) {
            (
                chat.metadata.unread_count,
                chat.metadata.active_unread_count,
                chat.metadata.passive_unread_count,
            ) = counts;
        }
        for topic in &mut chat.topics {
            let key = (stream_id.clone(), topic.id.to_string());
            if let Some(counts) = counts.remove(&key) {
                (
                    topic.metadata.unread_count,
                    topic.metadata.active_unread_count,
                    topic.metadata.passive_unread_count,
                ) = counts;
            }
        }
    }
    Ok(Some(chats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context::AccountId,
        messenger::SOURCE_ID,
        storage::{ApplyOutcome, EventWrite, HistoryScope, Storage},
    };
    use serde_json::json;
    use uuid::Uuid;

    /// Уменьшение счётчика без отдельного события чтения запрашивает сверку точной темы.
    #[test]
    fn catalog_unread_decrease_requests_history_read_revalidation() {
        let storage = Storage::in_memory().unwrap();
        let owner = OwnerContext {
            account_id: AccountId("a".into()),
            server: "https://test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let stream_id = Uuid::from_u128(10);
        let topic_id = Uuid::from_u128(20);
        let snapshot = json!([{
            "id": stream_id, "name": "Канал", "direct": false,
            "metadata": { "unread_count": 8 },
            "topics": [{
                "id": topic_id, "name": "Тема", "metadata": { "unread_count": 4 }
            }]
        }])
        .to_string();
        storage
            .save_chat_catalog(&owner, &snapshot, 0, None)
            .unwrap();
        let scope = HistoryScope {
            stream_id,
            topic_id: Some(topic_id),
        };
        let apply = |epoch_version, total| {
            storage
                .apply(
                    &owner,
                    SOURCE_ID,
                    &Cursor {
                        epoch_generation: "epoch".into(),
                        epoch_version,
                    },
                    EventWrite {
                        catalog_counts: Some(&CatalogCounts {
                            stream_id,
                            topic_id: Some(topic_id),
                            total,
                            active: total,
                            passive: 0,
                        }),
                        ..EventWrite::default()
                    },
                )
                .unwrap()
                .count_read_scope
        };
        assert_eq!(apply(1, 2), Some(scope));
        assert_eq!(apply(2, 3), None);
        assert_eq!(apply(3, 3), None);
        assert_eq!(apply(4, 1), Some(scope));
        assert_eq!(apply(4, 0), None);
    }

    /// Поздний REST сохраняет счётчики из журнала, но старые события не откатывают снимок.
    #[test]
    fn catalog_uses_absolute_counts_across_a_concurrent_refresh() {
        let storage = Storage::in_memory().unwrap();
        let owner = OwnerContext {
            account_id: AccountId("a".into()),
            server: "https://test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let other = OwnerContext {
            account_id: AccountId("b".into()),
            ..owner.clone()
        };
        let stream_id = Uuid::from_u128(10);
        let topic_id = Uuid::from_u128(20);
        let snapshot = json!([{
            "id": stream_id, "name": "Канал", "direct": false,
            "metadata": { "active_unread_count": 2 },
            "topics": [{ "id": topic_id, "name": "Тема" }]
        }])
        .to_string();
        let cursor = |epoch_version| Cursor {
            epoch_generation: "epoch".into(),
            epoch_version,
        };
        let apply = |owner: &OwnerContext, version, counts: &CatalogCounts| {
            storage
                .apply(
                    owner,
                    SOURCE_ID,
                    &cursor(version),
                    EventWrite {
                        catalog_counts: Some(counts),
                        ..EventWrite::default()
                    },
                )
                .unwrap()
                .outcome
        };
        let counts = CatalogCounts {
            stream_id,
            topic_id: None,
            total: 3,
            active: 2,
            passive: 1,
        };
        let mut changes = storage.catalog_changes();
        storage
            .save_chat_catalog(&owner, &snapshot, 0, Some(&cursor(10)))
            .unwrap();
        storage
            .save_chat_catalog(&other, &snapshot, 0, Some(&cursor(10)))
            .unwrap();
        changes.borrow_and_update();
        let stale = CatalogCounts {
            active: 100,
            ..counts.clone()
        };
        assert_eq!(apply(&owner, 9, &stale), ApplyOutcome::Applied);
        assert!(!changes.has_changed().unwrap());
        let revision = storage.catalog_revision(&owner).unwrap();
        let next = CatalogCounts {
            total: 7,
            active: 5,
            passive: 2,
            ..counts
        };
        assert_eq!(apply(&owner, 11, &next), ApplyOutcome::Applied);
        assert!(changes.has_changed().unwrap());
        changes.borrow_and_update();
        assert_eq!(apply(&owner, 11, &stale), ApplyOutcome::Duplicate);
        assert!(!changes.has_changed().unwrap());
        let zero = CatalogCounts {
            topic_id: Some(topic_id),
            total: 0,
            active: 0,
            passive: 0,
            ..next
        };
        assert_eq!(apply(&owner, 12, &zero), ApplyOutcome::Applied);
        storage
            .save_chat_catalog(&owner, &snapshot, revision, Some(&cursor(10)))
            .unwrap();
        let chats = storage.chat_catalog(&owner).unwrap().unwrap();
        let chat = &chats[0];
        assert_eq!(
            (
                chat.metadata.unread_count,
                chat.metadata.active_unread_count,
                chat.metadata.passive_unread_count
            ),
            (Some(7), Some(5), Some(2))
        );
        let topic = &chat.topics[0];
        assert_eq!(
            (
                topic.metadata.unread_count,
                topic.metadata.active_unread_count,
                topic.metadata.passive_unread_count
            ),
            (Some(0), Some(0), Some(0))
        );
        let other = storage.chat_catalog(&other).unwrap().unwrap();
        assert_eq!(other[0].metadata.active_unread_count, Some(2));
        assert_eq!(other[0].topics[0].metadata.active_unread_count, None);
    }

    /// Первый курсор и смена эпохи требуют свежей базы и блокируют запоздавшие ответы.
    #[test]
    fn checkpoint_fences_cold_start_and_journal_recovery() {
        let storage = Storage::in_memory().unwrap();
        let owner = OwnerContext {
            account_id: AccountId("account".into()),
            server: "https://test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let stream_id = Uuid::from_u128(10);
        let snapshot = json!([{
            "id": stream_id, "name": "Канал", "direct": false, "topics": [],
            "metadata": { "active_unread_count": 2 }
        }])
        .to_string();
        let first = Cursor {
            epoch_generation: "first".into(),
            epoch_version: 9,
        };
        let second = Cursor {
            epoch_generation: "second".into(),
            epoch_version: 4,
        };
        let checkpoint = |cursor: &Cursor| {
            storage
                .apply(
                    &owner,
                    SOURCE_ID,
                    cursor,
                    EventWrite {
                        checkpoint: true,
                        ..EventWrite::default()
                    },
                )
                .unwrap();
        };
        let mut checkpoints = storage.catalog_checkpoints();
        storage
            .save_chat_catalog(&owner, &snapshot, 0, None)
            .unwrap();
        checkpoint(&first);
        assert!(checkpoints.has_changed().unwrap());
        checkpoints.borrow_and_update();
        assert!(storage.needs_catalog_reconciliation(&owner).unwrap());
        let revision = storage.catalog_revision(&owner).unwrap();
        assert!(matches!(
            storage.save_chat_catalog(&owner, "[]", revision, None),
            Err(StorageError::CatalogBeforeCheckpoint)
        ));
        storage
            .save_chat_catalog(&owner, &snapshot, revision, Some(&first))
            .unwrap();
        assert!(!storage.needs_catalog_reconciliation(&owner).unwrap());

        let old_revision = storage.catalog_revision(&owner).unwrap();
        storage.reset_source(&owner, SOURCE_ID).unwrap();
        checkpoint(&second);
        assert!(checkpoints.has_changed().unwrap());
        assert!(storage.needs_catalog_reconciliation(&owner).unwrap());
        assert!(matches!(
            storage.save_chat_catalog(&owner, "[]", old_revision, Some(&first)),
            Err(StorageError::StaleCatalog)
        ));
        let latest = Cursor {
            epoch_version: 5,
            ..second.clone()
        };
        storage
            .apply(
                &owner,
                SOURCE_ID,
                &latest,
                EventWrite {
                    catalog_counts: Some(&CatalogCounts {
                        stream_id,
                        topic_id: None,
                        total: 4,
                        active: 3,
                        passive: 1,
                    }),
                    ..EventWrite::default()
                },
            )
            .unwrap();
        assert!(storage.chat_catalog(&owner).unwrap().is_none());
        let revision = storage.catalog_revision(&owner).unwrap();
        storage
            .save_chat_catalog(&owner, &snapshot, revision, Some(&second))
            .unwrap();
        let refreshed = storage.chat_catalog(&owner).unwrap().unwrap();
        assert_eq!(refreshed[0].metadata.active_unread_count, Some(3));
        assert!(!storage.needs_catalog_reconciliation(&owner).unwrap());
    }
}
