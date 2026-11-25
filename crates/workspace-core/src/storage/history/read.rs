//! Хранит серверные признаки чтения отдельно от тел и краёв истории.
//! Сначала описана запись событий, затем сверка окна и подтверждённой границы.

use rusqlite::{Connection, TransactionBehavior, params};
use uuid::Uuid;

use super::{
    HistoryMessage, HistoryScope, HistoryTime, advance, history_message, message_revision,
    owner_params, read_history_window, read_state, read_window,
};
use crate::{
    context::OwnerContext,
    storage::{HistoryChange, Storage, StorageError},
};

/// Состояние окна и порядок локальных записей до сверки с сервером.
pub(crate) struct HistoryReadGuard {
    scope: HistoryScope, // Точный адрес открытого окна.
    window_id: i64,      // Идентификатор окна на момент запроса.
    generation: i64,     // Поколение истории, меняется при отзыве и сбросе.
    revision: i64,       // Последний принятый снимок перед началом сверки.
}

/// Версия флагов и доступа перед отправкой отметки.
#[derive(Clone, Copy)]
pub(crate) struct HistoryReadFence {
    generation: i64, // Сброс или отзыв лишает запрос права записи.
    revision: i64,   // Более свежие признаки чтения сохраняются.
}

/// Флаг чтения точного размещения из сверки страницы.
pub(crate) struct HistoryReadRow {
    pub id: Uuid,                // Публичный UUID размещения.
    pub scope: HistoryScope,     // Точный стрим и тема.
    pub created_at: HistoryTime, // Серверное время того же размещения.
    pub read: Option<bool>,      // Флаг владельца. None означает неполный ответ.
}

/// Добавляет таблицу флагов и отдельно хранит подтверждённый тип источника.
pub(super) fn init(connection: &Connection) -> Result<(), StorageError> {
    let has_source: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('sync_messages') WHERE name = 'native_source')",
        [],
        |row| row.get(0),
    )?;
    if !has_source {
        connection.execute(
            "ALTER TABLE sync_messages ADD COLUMN native_source INTEGER",
            [],
        )?;
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS history_read_flags (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, message_id TEXT NOT NULL,
            is_read INTEGER NOT NULL, read_revision INTEGER NOT NULL,
            read_effect_revision INTEGER NOT NULL,
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, message_id)
        );",
    )?;
    Ok(())
}

/// Сохраняет точный флаг, если после старта запроса не было более свежей сверки.
pub(in crate::storage) fn write_read(
    connection: &Connection,
    owner: &OwnerContext,
    id: Uuid,
    read: bool,
    revision: i64,
    before: Option<i64>,
) -> Result<bool, StorageError> {
    let changed = connection.execute(
        "INSERT INTO history_read_flags
             (account_id, server, organization_id, project_id, user_id, message_id, is_read,
              read_revision, read_effect_revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id, message_id)
         DO UPDATE SET is_read = excluded.is_read, read_revision = excluded.read_revision,
             read_effect_revision = CASE
                 WHEN ?9 IS NULL OR history_read_flags.is_read <> excluded.is_read
                 THEN excluded.read_effect_revision ELSE history_read_flags.read_effect_revision END
         WHERE ?9 IS NULL OR history_read_flags.read_revision <= ?9",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            id.to_string(),
            read,
            revision,
            before
        ],
    )?;
    Ok(changed != 0)
}

/// Сохраняет точные UUID из журнала, даже если страница ещё загружается.
pub(in crate::storage) fn apply_read_event(
    connection: &Connection,
    owner: &OwnerContext,
    flags: &[(Uuid, bool)],
    scope: Option<HistoryScope>,
) -> Result<Vec<HistoryScope>, StorageError> {
    if flags.is_empty() {
        return Ok(Vec::new());
    }
    let revision = advance(connection, owner, false)?;
    let mut scopes = std::collections::HashSet::new();
    for &(id, read) in flags {
        if message_revision(connection, owner, &id.to_string())?.deleted {
            continue;
        }
        let current = history_message(connection, owner, id)?;
        if let Some(message) = &current
            && scope.is_some_and(|scope| !contains(scope, message))
        {
            continue;
        }
        write_read(connection, owner, id, read, revision, None)?;
        if let Some(message) = current
            && message.read != Some(read)
        {
            scopes.insert(message_scope(&message));
        }
    }
    let mut scopes: Vec<_> = scopes.into_iter().collect();
    scopes.sort_unstable_by_key(|scope| (scope.stream_id, scope.topic_id));
    Ok(scopes)
}

/// Удаляет признаки для отозванной области и точного удалённого сообщения.
pub(in crate::storage) fn forget_read(
    connection: &Connection,
    owner: &OwnerContext,
    change: &HistoryChange,
) -> Result<(), StorageError> {
    let (stream, topic) = match change {
        HistoryChange::Edit { .. } => return Ok(()),
        HistoryChange::RemoveMessage(id) => {
            connection.execute(
                "DELETE FROM history_read_flags WHERE account_id = ?1 AND server = ?2
                 AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    id
                ],
            )?;
            return Ok(());
        }
        HistoryChange::RemoveTopic(scope) => {
            (scope.stream_id.as_str(), Some(scope.topic_id.as_str()))
        }
        HistoryChange::RemoveStream(id) => (id.as_str(), None),
    };
    connection.execute(
        "DELETE FROM history_read_flags WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND (
             message_id IN (SELECT message_id FROM sync_messages WHERE account_id = ?1 AND server = ?2
                 AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5
                 AND stream_id = ?6 AND (?7 IS NULL OR topic_id = ?7))
             OR message_id NOT IN (SELECT message_id FROM sync_messages WHERE account_id = ?1
                 AND server = ?2 AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5)
         )",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id,
            owner.user_id, stream, topic],
    )?;
    Ok(())
}

/// Сбрасывает признаки чтения вместе с журналом исходного владельца.
pub(in crate::storage) fn reset_read(
    connection: &Connection,
    owner: &OwnerContext,
) -> Result<(), StorageError> {
    connection.execute(
        "DELETE FROM history_read_flags WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5",
        owner_params(owner),
    )?;
    Ok(())
}

/// Возвращает точный адрес сообщения внутри темы.
fn message_scope(message: &HistoryMessage) -> HistoryScope {
    HistoryScope {
        stream_id: message.stream_id,
        topic_id: Some(message.topic_id),
    }
}

/// Проверяет принадлежность сообщения теме или всей истории стрима.
fn contains(scope: HistoryScope, message: &HistoryMessage) -> bool {
    scope.stream_id == message.stream_id && scope.topic_id.is_none_or(|id| id == message.topic_id)
}

impl Storage {
    /// Возвращает текущий номер записи, чтобы поздний ответ не затёр более свежий флаг.
    pub(crate) fn history_read_revision(
        &self,
        owner: &OwnerContext,
    ) -> Result<HistoryReadFence, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let state = read_state(&connection, owner)?;
        Ok(HistoryReadFence {
            generation: state.generation,
            revision: state.revision,
        })
    }

    /// Находит адрес и данные уже сохранённой строки для проверки перед отправкой отметки.
    pub(crate) fn history_read_candidate(
        &self,
        owner: &OwnerContext,
        scope: HistoryScope,
        id: Uuid,
    ) -> Result<Option<HistoryMessage>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(history_message(&connection, owner, id)?.filter(|message| contains(scope, message)))
    }

    /// Записывает прочитанный префикс выбранной темы после доказанного применения запроса.
    pub(crate) fn confirm_history_read(
        &self,
        owner: &OwnerContext,
        scope: HistoryScope,
        boundary: Uuid,
        created_at: &HistoryTime,
        before: HistoryReadFence,
    ) -> Result<bool, StorageError> {
        let Some(topic_id) = scope.topic_id else {
            return Ok(false);
        };
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(message) = history_message(&transaction, owner, boundary)? else {
            return Ok(false);
        };
        if !contains(scope, &message)
            || &message.metadata.created_at != created_at
            || !message.read_eligible()
        {
            return Ok(false);
        }
        let state = read_state(&transaction, owner)?;
        if state.generation != before.generation {
            return Ok(false);
        }
        let revision = advance(&transaction, owner, false)?;
        // Повторный прежний флаг в REST подтверждает свежесть снимка, но не отменяет
        // результат команды, которую сервер мог применить уже после этого снимка.
        let changed = transaction.execute(
            "INSERT INTO history_read_flags
                 (account_id, server, organization_id, project_id, user_id, message_id, is_read,
                  read_revision, read_effect_revision)
             SELECT account_id, server, organization_id, project_id, user_id, message_id, 1, ?10, ?10
             FROM sync_messages WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
             AND project_id = ?4 AND user_id = ?5 AND stream_id = ?6 AND topic_id = ?7
             AND (created_at, message_id) <= (?8, ?9)
             ON CONFLICT (account_id, server, organization_id, project_id, user_id, message_id)
             DO UPDATE SET is_read = 1, read_revision = excluded.read_revision,
                 read_effect_revision = excluded.read_effect_revision
             WHERE history_read_flags.read_effect_revision <= ?11",
            params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id,
                owner.user_id, scope.stream_id.to_string(), topic_id.to_string(), created_at.as_str(),
                boundary.to_string(), revision, before.revision],
        )?;
        transaction.commit()?;
        Ok(changed != 0)
    }

    /// Снимает список строк для сверки вместе с версией открытого окна перед сетью.
    pub(crate) fn prepare_history_read(
        &self,
        owner: &OwnerContext,
        window_scope: HistoryScope,
        window_id: i64,
        scope: Option<HistoryScope>,
    ) -> Result<Option<(HistoryReadGuard, Vec<HistoryMessage>)>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let Some(window) = read_history_window(&connection, owner, window_scope, window_id)? else {
            return Ok(None);
        };
        let state = read_state(&connection, owner)?;
        let messages = window
            .messages
            .into_iter()
            .filter(|message| {
                message.read != Some(true) && scope.is_none_or(|scope| contains(scope, message))
            })
            .collect();
        Ok(Some((
            HistoryReadGuard {
                scope: window_scope,
                window_id,
                generation: state.generation,
                revision: state.revision,
            },
            messages,
        )))
    }

    /// Обновляет только флаги тех же размещений, не меняя строки, порядок и края окна.
    pub(crate) fn save_history_read(
        &self,
        owner: &OwnerContext,
        guard: HistoryReadGuard,
        messages: &[HistoryReadRow],
    ) -> Result<Option<HistoryScope>, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_state(&transaction, owner)?.generation != guard.generation
            || read_window(&transaction, owner, guard.scope, guard.window_id)?.is_none()
        {
            return Ok(None);
        }
        let revision = advance(&transaction, owner, false)?;
        let mut changed = false;
        for message in messages {
            let Some(read) = message.read else {
                continue;
            };
            if let Some(current) = history_message(&transaction, owner, message.id)?
                && current.metadata.created_at == message.created_at
                && message_scope(&current) == message.scope
                && contains(guard.scope, &current)
            {
                let accepted = write_read(
                    &transaction,
                    owner,
                    message.id,
                    read,
                    revision,
                    Some(guard.revision),
                )?;
                changed |= accepted && current.read != message.read;
            }
        }
        transaction.commit()?;
        Ok(changed.then_some(guard.scope))
    }
}
