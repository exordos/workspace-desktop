//! Хранит окна истории стримов и тем, их края и ссылки на общие записи сообщений.
//! Типы описывают снимок страницы, а операции SQLite защищают его от запоздавшей записи.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use super::{Cursor, Message, Storage, StorageError, read_cursor};
use crate::{
    context::OwnerContext,
    messenger::{
        SOURCE_ID,
        references::{external_links, media_links},
    },
};

const MEDIA_PROJECTION_CONFLICT: i64 = 1;
const MEDIA_PROJECTION_CURRENT: i64 = 3;

/// Область запроса истории. Пустая тема означает весь стрим.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryScope {
    pub stream_id: Uuid,        // UUID стрима.
    pub topic_id: Option<Uuid>, // UUID темы или весь стрим.
}

/// Состояние одного края загруженного участка.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryEdge {
    Unchecked,    // У края может быть пропуск или больше нет действующего маркера.
    More(String), // Маркер сервера для следующей страницы в этом направлении.
    End,          // Сервер подтвердил конец истории.
}

/// Направление догрузки относительно сохранённого участка.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryDirection {
    Older, // Догрузка к более старым сообщениям.
    Newer, // Догрузка к более новым сообщениям.
}

/// Серверное время UTC, пригодное для точного сравнения в SQLite.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HistoryTime(String); // Строка UTC с фиксированной точностью до микросекунд.

impl HistoryTime {
    /// Возвращает нормализованное представление для записи в SQLite.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Принимает время сервера в UTC с суффиксом Z или +00:00 и сохраняет его с шестью знаками дроби.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value
            .strip_suffix('Z')
            .or_else(|| value.strip_suffix("+00:00"))?;
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        let bytes = whole.as_bytes();
        if bytes.len() != 19 || fraction.len() > 6 || (fraction.is_empty() && value.ends_with('.'))
        {
            return None;
        }
        for (index, byte) in bytes.iter().copied().enumerate() {
            let valid = match index {
                4 | 7 => byte == b'-',
                10 => byte == b'T',
                13 | 16 => byte == b':',
                _ => byte.is_ascii_digit(),
            };
            if !valid {
                return None;
            }
        }
        if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        // Сравниваем строки по одному формату и не округляем серверные микросекунды.
        Some(Self(format!("{whole}.{fraction:0<6}Z")))
    }
}

/// Серверные поля текстового сообщения, которые нужны для истории.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryMetadata {
    pub author_id: Option<Uuid>,         // Автор, если сервер передал UUID.
    pub created_at: HistoryTime,         // Время создания для сортировки.
    pub updated_at: Option<HistoryTime>, // Время правки, если оно известно.
}

/// Одна запись подтверждённой страницы или сохранённого окна.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryMessage {
    pub id: Uuid,                  // Публичный UUID сообщения.
    pub stream_id: Uuid,           // Стрим сообщения из ответа сервера.
    pub topic_id: Uuid,            // Тема сообщения внутри стрима.
    pub text: String,              // Содержимое Markdown.
    pub metadata: HistoryMetadata, // Автор и серверное время.
}

/// Состав окна и отдельные состояния его краёв.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryWindow {
    pub id: i64,                       // Локальный ID, который сохраняется при догрузке.
    pub scope: HistoryScope,           // Стрим или тема этого окна.
    pub messages: Vec<HistoryMessage>, // Ссылки на тела сообщений в порядке от старых к новым.
    pub older: HistoryEdge,            // Край в сторону старых сообщений.
    pub newer: HistoryEdge,            // Край в сторону новых сообщений.
    pub needs_refresh: bool, // Были изменения, после которых окно нужно сверить с сервером.
}

/// Подтверждённые сервером края первого окна или продолжения существующего.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryPageEdges {
    New {
        older: HistoryEdge, // Край нового окна со стороны старых сообщений.
        newer: HistoryEdge, // Край нового окна со стороны новых сообщений.
    },
    Extend(HistoryEdge), // Новый край в направлении запроса.
}

/// Операция, для которой запрашиваем страницу.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryPageTarget {
    New, // Создание отдельного окна. Пропуски между окнами сохраняются.
    Extend {
        window_id: i64,              // ID окна, которому принадлежит маркер.
        direction: HistoryDirection, // Какой край обновляем.
    },
}

/// Снимок условий записи, полученный до обращения к серверу.
#[derive(Debug)]
pub(crate) struct HistoryPageGuard {
    scope: HistoryScope,       // Область запроса страницы.
    target: HistoryPageTarget, // Новое окно или конкретный край.
    marker: Option<String>,    // Маркер края. Для нового окна его нет.
    edge_revision: i64,        // Версия края на момент старта.
    revision: i64,             // Последняя локальная версия сообщений на момент запроса.
    generation: i64,           // Версия сброса кэша и отзыва доступа.
    cursor: Option<Cursor>,    // Позиция событий перед запросом. Она не описывает сам снимок.
}

impl HistoryPageGuard {
    /// Возвращает маркер, который нужно передать серверу для догрузки.
    pub(crate) fn marker(&self) -> Option<&str> {
        self.marker.as_deref()
    }
}

/// Результат записи страницы. По отклонённой странице можно повторить запрос.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryPageOutcome {
    Saved(i64), // ID окна после успешной записи.
    Stale,      // Во время запроса изменилась строка, поколение кэша или край.
}

/// Состояние границы в SQLite.
#[derive(Clone, Debug)]
struct EdgeRecord {
    kind: i64,              // 0 — не проверена, 1 — есть маркер, 2 — конец.
    marker: Option<String>, // Маркер продолжения для состояния 1.
}

impl From<&HistoryEdge> for EdgeRecord {
    /// Переводит состояние края в два поля SQLite.
    fn from(edge: &HistoryEdge) -> Self {
        match edge {
            HistoryEdge::Unchecked => Self {
                kind: 0,
                marker: None,
            },
            HistoryEdge::More(marker) => Self {
                kind: 1,
                marker: Some(marker.clone()),
            },
            HistoryEdge::End => Self {
                kind: 2,
                marker: None,
            },
        }
    }
}

impl EdgeRecord {
    /// Восстанавливает состояние края из полей базы.
    fn into_edge(self) -> HistoryEdge {
        match (self.kind, self.marker) {
            (1, Some(marker)) => HistoryEdge::More(marker),
            (2, _) => HistoryEdge::End,
            _ => HistoryEdge::Unchecked,
        }
    }
}

/// Монотонные версии записей и сбросов для одного владельца.
#[derive(Default)]
struct HistoryState {
    revision: i64,   // Последнее локальное изменение сообщения или страницы.
    generation: i64, // Версия сброса или отзыва доступа.
}

/// Порядковый ключ сообщения внутри области.
type OrderKey = (String, String);

/// Состояние сохранённого окна в SQLite.
struct WindowRecord {
    older: EdgeRecord,       // Состояние старшего края.
    newer: EdgeRecord,       // Состояние младшего края.
    older_revision: i64,     // Версия запроса старых сообщений.
    newer_revision: i64,     // Версия запроса новых сообщений.
    first: Option<OrderKey>, // Нижняя граница подтверждённого участка.
    last: Option<OrderKey>,  // Верхняя граница подтверждённого участка.
    needs_refresh: bool,     // Окно ждёт повторной сверки.
}

/// Добавляет поля сообщений и таблицы окон в одной миграции.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    let has_metadata: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('sync_messages') WHERE name = 'created_at')",
        [], |row| row.get(0),
    )?;
    if !has_metadata {
        connection.execute_batch(
            "ALTER TABLE sync_messages ADD COLUMN author_id TEXT;
             ALTER TABLE sync_messages ADD COLUMN created_at TEXT;
             ALTER TABLE sync_messages ADD COLUMN updated_at TEXT;",
        )?;
    }
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS history_messages_stream_order
            ON sync_messages(account_id, server, organization_id, project_id, user_id, stream_id, created_at, message_id);
         CREATE INDEX IF NOT EXISTS history_messages_topic_order
            ON sync_messages(account_id, server, organization_id, project_id, user_id, stream_id, topic_id, created_at, message_id);
         CREATE TABLE IF NOT EXISTS history_state (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL,
            revision INTEGER NOT NULL, generation INTEGER NOT NULL,
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id)
         );
         CREATE TABLE IF NOT EXISTS history_changes (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, message_id TEXT NOT NULL,
            revision INTEGER NOT NULL, deleted INTEGER NOT NULL,
            snapshot INTEGER NOT NULL, snapshot_epoch TEXT, snapshot_position INTEGER,
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, message_id)
         );
         CREATE TABLE IF NOT EXISTS history_windows (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, stream_id TEXT NOT NULL, topic_id TEXT,
            older_kind INTEGER NOT NULL, older_marker TEXT, newer_kind INTEGER NOT NULL, newer_marker TEXT,
            older_revision INTEGER NOT NULL, newer_revision INTEGER NOT NULL,
            first_time TEXT, first_id TEXT, last_time TEXT, last_id TEXT,
            needs_refresh INTEGER NOT NULL DEFAULT 0,
            UNIQUE (id, account_id, server, organization_id, project_id, user_id)
         );
         CREATE INDEX IF NOT EXISTS history_windows_by_scope
            ON history_windows(account_id, server, organization_id, project_id, user_id, stream_id, topic_id);
         CREATE TABLE IF NOT EXISTS history_members (
            window_id INTEGER NOT NULL,
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, message_id TEXT NOT NULL,
            PRIMARY KEY (window_id, message_id),
            FOREIGN KEY (window_id, account_id, server, organization_id, project_id, user_id)
                REFERENCES history_windows(id, account_id, server, organization_id, project_id, user_id)
                ON DELETE CASCADE,
            FOREIGN KEY (account_id, server, organization_id, project_id, user_id, message_id)
                REFERENCES sync_messages(account_id, server, organization_id, project_id, user_id, message_id)
                ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS history_members_by_message
            ON history_members(account_id, server, organization_id, project_id, user_id, message_id);",
    )?;
    // 0 — без проекции, 1 — конфликт, 2 — старый индекс файлов, 3 — текущий индекс.
    let has_media_flag: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('history_changes') WHERE name = 'media_projection')",
        [], |row| row.get(0),
    )?;
    if !has_media_flag {
        connection.execute_batch(
            "ALTER TABLE history_changes ADD COLUMN media_projection INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    Ok(())
}

/// Читает версию владельца. Отсутствие записи означает, что изменений ещё не было.
fn read_state(connection: &Connection, owner: &OwnerContext) -> Result<HistoryState, StorageError> {
    Ok(connection
        .query_row(
            "SELECT revision, generation FROM history_state WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5",
            owner_params(owner),
            |row| {
                Ok(HistoryState {
                    revision: row.get(0)?,
                    generation: row.get(1)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

/// Увеличивает номер изменений, а при очистке ещё и поколение запросов.
fn advance(
    connection: &Connection,
    owner: &OwnerContext,
    invalidate: bool,
) -> Result<i64, StorageError> {
    Ok(connection.query_row(
        "INSERT INTO history_state VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id)
         DO UPDATE SET revision = history_state.revision + 1,
                       generation = history_state.generation + excluded.generation
         RETURNING revision",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            invalidate
        ],
        |row| row.get(0),
    )?)
}

/// Собирает полный ключ владельца для запросов без других параметров.
fn owner_params(owner: &OwnerContext) -> [&str; 5] {
    [
        &owner.account_id.0,
        &owner.server,
        &owner.organization_id,
        &owner.project_id,
        &owner.user_id,
    ]
}

/// Читает окно только в заявленной области и у нужного владельца.
fn read_window(
    connection: &Connection,
    owner: &OwnerContext,
    scope: HistoryScope,
    id: i64,
) -> Result<Option<WindowRecord>, StorageError> {
    Ok(connection.query_row(
        "SELECT older_kind, older_marker, newer_kind, newer_marker, older_revision, newer_revision,
                first_time, first_id, last_time, last_id, needs_refresh
         FROM history_windows WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
         AND project_id = ?4 AND user_id = ?5 AND stream_id = ?6 AND topic_id IS ?7 AND id = ?8",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
            scope.stream_id.to_string(), scope.topic_id.map(|id| id.to_string()), id],
        |row| {
            let first_time: Option<String> = row.get(6)?;
            let first_id: Option<String> = row.get(7)?;
            let last_time: Option<String> = row.get(8)?;
            let last_id: Option<String> = row.get(9)?;
            Ok(WindowRecord {
                older: EdgeRecord { kind: row.get(0)?, marker: row.get(1)? },
                newer: EdgeRecord { kind: row.get(2)?, marker: row.get(3)? },
                older_revision: row.get(4)?,
                newer_revision: row.get(5)?,
                first: first_time.zip(first_id),
                last: last_time.zip(last_id),
                needs_refresh: row.get(10)?,
            })
        },
    ).optional()?)
}

impl WindowRecord {
    /// Возвращает состояние и версию указанного края.
    fn edge(&self, direction: HistoryDirection) -> (&EdgeRecord, i64) {
        match direction {
            HistoryDirection::Older => (&self.older, self.older_revision),
            HistoryDirection::Newer => (&self.newer, self.newer_revision),
        }
    }
}

/// Вычисляет ключ сортировки в том же порядке, что и сервер.
fn order_key(message: &HistoryMessage) -> OrderKey {
    (
        message.metadata.created_at.0.clone(),
        message.id.to_string(),
    )
}

/// Хранит версию UUID, включая удаления без локального тела.
#[derive(Default)]
struct MessageRevision {
    revision: i64,  // Локальная версия последней записи.
    deleted: bool,  // Запись удалена событием.
    snapshot: bool, // Последнее подтверждённое тело пришло со страницы истории.
}

/// Читает состояние сообщения перед применением снимка или события.
fn message_revision(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
) -> Result<MessageRevision, StorageError> {
    Ok(connection.query_row(
        "SELECT revision, deleted, snapshot FROM history_changes WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, id],
        |row| Ok(MessageRevision { revision: row.get(0)?, deleted: row.get(1)?, snapshot: row.get(2)? }),
    ).optional()?.unwrap_or_default())
}

/// Записывает, когда и откуда изменился UUID сообщения.
fn record_revision(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
    revision: i64,
    deleted: bool,
    snapshot: Option<Option<&Cursor>>,
) -> Result<(), StorageError> {
    let cursor = snapshot.flatten();
    let position = cursor
        .map(|position| i64::try_from(position.epoch_version))
        .transpose()
        .map_err(|_| StorageError::CursorVersionOutOfRange)?;
    connection.execute(
        "INSERT INTO history_changes
         (account_id, server, organization_id, project_id, user_id, message_id,
          revision, deleted, snapshot, snapshot_epoch, snapshot_position)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id, message_id)
         DO UPDATE SET revision = excluded.revision, deleted = excluded.deleted, snapshot = excluded.snapshot,
                       snapshot_epoch = excluded.snapshot_epoch, snapshot_position = excluded.snapshot_position",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, id,
            revision, deleted, snapshot.is_some(), cursor.map(|position| &position.epoch_generation),
            position],
    )?;
    Ok(())
}

impl Storage {
    /// Запоминает версию данных и маркер до запроса страницы.
    pub(crate) fn prepare_history_page(
        &self,
        owner: &OwnerContext,
        scope: HistoryScope,
        target: HistoryPageTarget,
    ) -> Result<Option<HistoryPageGuard>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let (marker, edge_revision) = match target {
            HistoryPageTarget::New => (None, 0),
            HistoryPageTarget::Extend {
                window_id,
                direction,
            } => {
                let Some(window) = read_window(&connection, owner, scope, window_id)? else {
                    return Ok(None);
                };
                let (edge, revision) = window.edge(direction);
                if window.needs_refresh || edge.kind != 1 {
                    return Ok(None);
                }
                (edge.marker.clone(), revision)
            }
        };
        let state = read_state(&connection, owner)?;
        Ok(Some(HistoryPageGuard {
            scope,
            target,
            marker,
            edge_revision,
            revision: state.revision,
            generation: state.generation,
            cursor: read_cursor(&connection, owner, SOURCE_ID)?,
        }))
    }

    /// Возвращает поколение истории, после смены которого прежний доступ нужно подтвердить заново.
    pub(crate) fn media_history_generation(
        &self,
        owner: &OwnerContext,
    ) -> Result<i64, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(read_state(&connection, owner)?.generation)
    }

    /// Дополняет индекс файлов и веб-ссылок из ранее сохранённой подтверждённой истории.
    pub(crate) fn index_media_history(
        &self,
        owner: &OwnerContext,
        scope: HistoryScope,
        generation: i64,
        message: Option<Uuid>,
    ) -> Result<bool, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_state(&transaction, owner)?.generation != generation {
            return Ok(false);
        }
        let ids: Vec<String> = {
            let mut query = transaction.prepare(
                "SELECT message_id FROM sync_messages
                 WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
                   AND user_id = ?5 AND stream_id = ?6 AND (?7 IS NULL OR topic_id = ?7)
                   AND (?8 IS NULL OR message_id = ?8)
                   AND created_at IS NOT NULL
                   AND (instr(text, 'urn') > 0 OR instr(text, '://') > 0 OR instr(text, 'www.') > 0)
                   AND NOT EXISTS (
                       SELECT 1 FROM history_changes AS changes
                       WHERE changes.account_id = ?1 AND changes.server = ?2
                         AND changes.organization_id = ?3 AND changes.project_id = ?4
                         AND changes.user_id = ?5 AND changes.message_id = sync_messages.message_id
                         AND changes.media_projection IN (?9, ?10)
                   )",
            )?;
            query
                .query_map(
                    params![
                        owner.account_id.0,
                        owner.server,
                        owner.organization_id,
                        owner.project_id,
                        owner.user_id,
                        scope.stream_id.to_string(),
                        scope.topic_id.map(|topic| topic.to_string()),
                        message.map(|id| id.to_string()),
                        MEDIA_PROJECTION_CONFLICT,
                        MEDIA_PROJECTION_CURRENT,
                    ],
                    |row| row.get(0),
                )?
                .collect::<rusqlite::Result<_>>()?
        };
        for id in &ids {
            project_media_message(&transaction, owner, id)?;
        }
        transaction.commit()?;
        Ok(true)
    }

    /// Применяет целую подтверждённую страницу. Спорная страница оставляет окно в прежнем состоянии.
    pub(crate) fn save_history_page(
        &self,
        owner: &OwnerContext,
        guard: HistoryPageGuard,
        messages: &[HistoryMessage],
        edges: HistoryPageEdges,
    ) -> Result<HistoryPageOutcome, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_state(&transaction, owner)?.generation != guard.generation {
            return Ok(HistoryPageOutcome::Stale);
        }
        let bad_marker =
            |edge: &HistoryEdge| matches!(edge, HistoryEdge::More(value) if value.is_empty());
        if match &edges {
            HistoryPageEdges::New { older, newer } => bad_marker(older) || bad_marker(newer),
            HistoryPageEdges::Extend(edge) => bad_marker(edge),
        } {
            return Err(StorageError::InvalidHistoryPage);
        }
        let mut ids = std::collections::HashSet::new();
        let mut keys = messages.iter().map(order_key).collect::<Vec<_>>();
        keys.sort_unstable();
        if messages.iter().any(|message| {
            !ids.insert(message.id)
                || message.stream_id != guard.scope.stream_id
                || guard
                    .scope
                    .topic_id
                    .is_some_and(|topic| topic != message.topic_id)
        }) {
            return Err(StorageError::InvalidHistoryPage);
        }
        let stream = guard.scope.stream_id.to_string();
        for message in messages {
            let id = message.id.to_string();
            let stored = message_revision(&transaction, owner, &id)?;
            if stored.deleted || stored.revision > guard.revision {
                return Ok(HistoryPageOutcome::Stale);
            }
            let conflicting_address: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_messages WHERE account_id = ?1 AND server = ?2
                 AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6
                 AND ((stream_id IS NOT NULL AND stream_id <> ?7) OR (topic_id IS NOT NULL AND topic_id <> ?8)
                      OR (created_at IS NOT NULL AND created_at <> ?9)))",
                params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
                    id, stream, message.topic_id.to_string(), message.metadata.created_at.0],
                |row| row.get(0),
            )?;
            if conflicting_address {
                return Ok(HistoryPageOutcome::Stale);
            }
        }
        // Ответ мог не включить сообщение, которое уже дошло по журналу. В этом
        // случае он не подтверждает участок, даже когда все его строки актуальны.
        let changed_in_scope: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM history_changes JOIN sync_messages
             USING (account_id, server, organization_id, project_id, user_id, message_id)
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
               AND user_id = ?5 AND stream_id = ?6 AND (?7 IS NULL OR topic_id = ?7)
               AND revision > ?8 AND snapshot = 0)",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                stream,
                guard.scope.topic_id.map(|id| id.to_string()),
                guard.revision
            ],
            |row| row.get(0),
        )?;
        if changed_in_scope {
            return Ok(HistoryPageOutcome::Stale);
        }
        let revision = advance(&transaction, owner, false)?;
        let window_id = match (guard.target, edges) {
            (HistoryPageTarget::New, HistoryPageEdges::New { older, newer }) => {
                let older = EdgeRecord::from(&older);
                let newer = EdgeRecord::from(&newer);
                transaction.query_row(
                    "INSERT INTO history_windows (account_id, server, organization_id, project_id, user_id,
                    stream_id, topic_id, older_kind, older_marker, newer_kind, newer_marker,
                    older_revision, newer_revision, first_time, first_id, last_time, last_id)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12, ?13, ?14, ?15, ?16)
                    RETURNING id",
                    params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
                        stream, guard.scope.topic_id.map(|id| id.to_string()), older.kind, older.marker, newer.kind, newer.marker,
                        revision, keys.first().map(|key| &key.0), keys.first().map(|key| &key.1),
                        keys.last().map(|key| &key.0), keys.last().map(|key| &key.1)],
                    |row| row.get(0),
                )?
            }
            (
                HistoryPageTarget::Extend {
                    window_id,
                    direction,
                },
                HistoryPageEdges::Extend(edge),
            ) => {
                let Some(window) = read_window(&transaction, owner, guard.scope, window_id)? else {
                    return Ok(HistoryPageOutcome::Stale);
                };
                let (current, edge_revision) = window.edge(direction);
                if window.needs_refresh
                    || edge_revision != guard.edge_revision
                    || current.marker != guard.marker
                {
                    return Ok(HistoryPageOutcome::Stale);
                }
                if matches!(&edge, HistoryEdge::More(marker) if Some(marker) == guard.marker.as_ref())
                {
                    return Err(StorageError::InvalidHistoryPage);
                }
                let boundary = match direction {
                    HistoryDirection::Older => (&window.first, keys.last()),
                    HistoryDirection::Newer => (&window.last, keys.first()),
                };
                if let (Some(previous), Some(nearest)) = boundary
                    && match direction {
                        HistoryDirection::Older => nearest >= previous,
                        HistoryDirection::Newer => nearest <= previous,
                    }
                {
                    return Err(StorageError::InvalidHistoryPage);
                }
                let edge = EdgeRecord::from(&edge);
                let (columns, key) = match direction {
                    HistoryDirection::Older => (
                        "older_kind = ?1, older_marker = ?2, older_revision = ?3,
                         first_time = COALESCE(?4, first_time), first_id = COALESCE(?5, first_id)",
                        keys.first(),
                    ),
                    HistoryDirection::Newer => (
                        "newer_kind = ?1, newer_marker = ?2, newer_revision = ?3,
                         last_time = COALESCE(?4, last_time), last_id = COALESCE(?5, last_id)",
                        keys.last(),
                    ),
                };
                transaction.execute(
                    &format!("UPDATE history_windows SET {columns} WHERE id = ?6"),
                    params![
                        edge.kind,
                        edge.marker,
                        revision,
                        key.map(|key| &key.0),
                        key.map(|key| &key.1),
                        window_id
                    ],
                )?;
                window_id
            }
            _ => return Err(StorageError::InvalidHistoryPage),
        };
        for message in messages {
            save_snapshot(
                &transaction,
                owner,
                &stream,
                message,
                revision,
                guard.cursor.as_ref(),
            )?;
            project_message_refs(
                &transaction,
                owner,
                message.id,
                HistoryScope {
                    stream_id: message.stream_id,
                    topic_id: Some(message.topic_id),
                },
                message.metadata.created_at.as_str(),
                &message.text,
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO history_members VALUES (?6, ?1, ?2, ?3, ?4, ?5, ?7)",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    window_id,
                    message.id.to_string()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(HistoryPageOutcome::Saved(window_id))
    }

    /// Перечитывает одно окно в его исходной области и у того же владельца.
    pub(crate) fn history_window(
        &self,
        owner: &OwnerContext,
        scope: HistoryScope,
        id: i64,
    ) -> Result<Option<HistoryWindow>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        read_history_window(&connection, owner, scope, id)
    }

    /// Читает подтверждённые окна области. Сохранённые по отдельности сообщения в них не попадают.
    pub(crate) fn history_windows(
        &self,
        owner: &OwnerContext,
        scope: HistoryScope,
    ) -> Result<Vec<HistoryWindow>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut query = connection.prepare(
            "SELECT id FROM history_windows WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
             AND project_id = ?4 AND user_id = ?5 AND stream_id = ?6 AND topic_id IS ?7
             ORDER BY last_time DESC, last_id DESC, id DESC",
        )?;
        let ids = query.query_map(
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                scope.stream_id.to_string(),
                scope.topic_id.map(|id| id.to_string())
            ],
            |row| row.get(0),
        )?;
        let mut result = Vec::new();
        for id in ids {
            let id = id?;
            if let Some(window) = read_history_window(&connection, owner, scope, id)? {
                result.push(window);
            }
        }
        Ok(result)
    }
}

/// Обновляет ссылки открытого сообщения после события или при обновлении старого индекса.
pub(super) fn project_media_message(
    connection: &Connection,
    owner: &OwnerContext,
    message_id: &str,
) -> Result<(), StorageError> {
    let Ok(id) = Uuid::parse_str(message_id) else {
        return Ok(());
    };
    let row: Option<(String, String, String, String)> = connection
        .query_row(
            "SELECT stream_id, topic_id, created_at, text FROM sync_messages
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
               AND user_id = ?5 AND message_id = ?6
               AND stream_id IS NOT NULL AND topic_id IS NOT NULL AND created_at IS NOT NULL",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                message_id,
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    match row {
        Some((stream_id, topic_id, created_at, text))
            if let (Ok(stream_id), Ok(topic_id)) = (stream_id.parse(), topic_id.parse()) =>
        {
            let scope = HistoryScope {
                stream_id,
                topic_id: Some(topic_id),
            };
            project_message_refs(connection, owner, id, scope, &created_at, &text)
        }
        _ => super::media::remove_message_refs(connection, owner, id),
    }?;
    mark_media_indexed(connection, owner, message_id)
}

/// Обновляет оба индекса из одного подтверждённого текста и размещения сообщения.
fn project_message_refs(
    connection: &Connection,
    owner: &OwnerContext,
    message_id: Uuid,
    scope: HistoryScope,
    created_at: &str,
    text: &str,
) -> Result<(), StorageError> {
    super::media::replace_message_refs(
        connection,
        owner,
        message_id,
        scope,
        created_at,
        &media_links(text),
    )?;
    super::media::replace_link_refs(
        connection,
        owner,
        message_id,
        scope,
        created_at,
        &external_links(text),
    )
}

/// Запоминает завершённый разбор, включая обычные сообщения без вложений.
fn mark_media_indexed(
    connection: &Connection,
    owner: &OwnerContext,
    message_id: &str,
) -> Result<(), StorageError> {
    connection.execute(
        "INSERT INTO history_changes
           (account_id, server, organization_id, project_id, user_id, message_id,
            revision, deleted, snapshot, media_projection)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, 0, ?7)
         ON CONFLICT(account_id, server, organization_id, project_id, user_id, message_id)
         DO UPDATE SET media_projection = excluded.media_projection
         WHERE media_projection <> excluded.media_projection",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            message_id,
            MEDIA_PROJECTION_CURRENT,
        ],
    )?;
    Ok(())
}

/// Собирает снимок окна и его строки под одной блокировкой базы.
fn read_history_window(
    connection: &Connection,
    owner: &OwnerContext,
    scope: HistoryScope,
    id: i64,
) -> Result<Option<HistoryWindow>, StorageError> {
    let Some(window) = read_window(connection, owner, scope, id)? else {
        return Ok(None);
    };
    Ok(Some(HistoryWindow {
        id,
        scope,
        messages: read_members(connection, owner, id)?,
        older: window.older.into_edge(),
        newer: window.newer.into_edge(),
        needs_refresh: window.needs_refresh,
    }))
}

/// Сохраняет тело страницы по общему ключу владельца и сообщения.
fn save_snapshot(
    connection: &Connection,
    owner: &OwnerContext,
    stream: &str,
    message: &HistoryMessage,
    revision: i64,
    cursor: Option<&Cursor>,
) -> Result<(), StorageError> {
    let id = message.id.to_string();
    connection.execute(
        "INSERT INTO sync_messages (account_id, server, organization_id, project_id, user_id,
            message_id, text, stream_id, topic_id, author_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id, message_id) DO UPDATE SET
            text = excluded.text, stream_id = excluded.stream_id, topic_id = excluded.topic_id,
            author_id = COALESCE(excluded.author_id, sync_messages.author_id), created_at = excluded.created_at,
            updated_at = COALESCE(excluded.updated_at, sync_messages.updated_at)",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
            id, message.text, stream, message.topic_id.to_string(), message.metadata.author_id.map(|id| id.to_string()),
            message.metadata.created_at.0, message.metadata.updated_at.as_ref().map(|time| &time.0)],
    )?;
    record_revision(connection, owner, &id, revision, false, Some(cursor))?;
    mark_media_indexed(connection, owner, &id)
}

/// Читает общие тела сообщений по ссылкам окна с проверкой полного ключа владельца.
fn read_members(
    connection: &Connection,
    owner: &OwnerContext,
    id: i64,
) -> Result<Vec<HistoryMessage>, StorageError> {
    let mut query = connection.prepare(
        "SELECT message_id, stream_id, topic_id, text, author_id, created_at, updated_at
         FROM history_members JOIN sync_messages USING (account_id, server, organization_id, project_id, user_id, message_id)
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5
           AND window_id = ?6 ORDER BY created_at, message_id",
    )?;
    let records = query.query_map(
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
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        },
    )?;
    records
        .map(|record| {
            let (id, stream, topic, text, author, created, updated) = record?;
            Ok(HistoryMessage {
                id: Uuid::parse_str(&id).map_err(|_| StorageError::InvalidHistoryPage)?,
                stream_id: Uuid::parse_str(&stream)
                    .map_err(|_| StorageError::InvalidHistoryPage)?,
                topic_id: Uuid::parse_str(&topic).map_err(|_| StorageError::InvalidHistoryPage)?,
                text,
                metadata: HistoryMetadata {
                    author_id: author
                        .as_deref()
                        .map(Uuid::parse_str)
                        .transpose()
                        .map_err(|_| StorageError::InvalidHistoryPage)?,
                    created_at: HistoryTime(created),
                    updated_at: updated.map(HistoryTime),
                },
            })
        })
        .collect()
}

/// Удаляет окна и отметки владельца при сбросе источника. Старые запросы теряют право записи.
pub(super) fn reset(connection: &Connection, owner: &OwnerContext) -> Result<(), StorageError> {
    advance(connection, owner, true)?;
    for table in ["history_windows", "history_changes"] {
        connection.execute(
            &format!(
                "DELETE FROM {table} WHERE account_id = ?1 AND server = ?2
            AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5"
            ),
            owner_params(owner),
        )?;
    }
    Ok(())
}

/// Сохраняет версию события и связывает сообщение с уже подтверждёнными участками.
pub(super) fn record_creation(
    connection: &Connection,
    owner: &OwnerContext,
    message: &Message,
    placement: Option<&super::MessagePlacement>,
    metadata: Option<&HistoryMetadata>,
) -> Result<(), StorageError> {
    let revision = advance(connection, owner, false)?;
    record_revision(connection, owner, &message.id, revision, false, None)?;
    let (Some(address), Some(metadata)) = (placement, metadata) else {
        return Ok(());
    };
    // Событие вне диапазона не доказывает непрерывность. Но прежний конец больше не актуален.
    connection.execute(
        "UPDATE history_windows SET newer_kind = 0, newer_marker = NULL, newer_revision = ?10
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5
         AND stream_id = ?7 AND (topic_id IS NULL OR topic_id = ?8) AND newer_kind = 2
         AND (last_time IS NULL OR (last_time, last_id) < (?9, ?6))",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
            message.id, address.stream_id, address.topic_id, metadata.created_at.0, revision],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO history_members (window_id, account_id, server, organization_id, project_id, user_id, message_id)
         SELECT id, account_id, server, organization_id, project_id, user_id, ?6 FROM history_windows
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5
         AND stream_id = ?7 AND (topic_id IS NULL OR topic_id = ?8) AND needs_refresh = 0
         AND (first_time, first_id) <= (?9, ?6) AND (last_time, last_id) >= (?9, ?6)",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
            message.id, address.stream_id, address.topic_id, metadata.created_at.0],
    )?;
    Ok(())
}

/// Проверяет, может ли событие изменить общую строку. Готовит окна к удалению или сверке.
pub(super) fn before_change(
    connection: &Connection,
    owner: &OwnerContext,
    change: &super::HistoryChange,
) -> Result<bool, StorageError> {
    use super::HistoryChange;
    match change {
        HistoryChange::RemoveStream(stream) => {
            invalidate_scope(connection, owner, stream, None)?;
        }
        HistoryChange::RemoveTopic(address) => {
            invalidate_scope(
                connection,
                owner,
                &address.stream_id,
                Some(&address.topic_id),
            )?;
        }
        HistoryChange::RemoveMessage(id) => {
            let revision = advance(connection, owner, false)?;
            record_revision(connection, owner, id, revision, true, None)?;
            invalidate_message_edges(connection, owner, id, revision)?;
        }
        HistoryChange::Edit { message, placement } => {
            let stored = message_revision(connection, owner, &message.id)?;
            if stored.deleted {
                return Ok(false);
            }
            let revision = advance(connection, owner, false)?;
            if stored.snapshot {
                // REST-снимок мог уже учесть более позднюю правку. Сохраняем его до сверки,
                // но новая локальная версия блокирует страницы, которые начали читать раньше.
                connection.execute(
                    "UPDATE history_changes SET revision = ?7 WHERE account_id = ?1 AND server = ?2
                     AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6",
                    params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
                        message.id, revision],
                )?;
                let same_body: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sync_messages WHERE account_id = ?1 AND server = ?2
                     AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6
                     AND text = ?7 AND (?8 IS NULL OR (stream_id = ?8 AND topic_id = ?9)))",
                    params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
                        message.id, message.text, placement.as_ref().map(|address| &address.stream_id),
                        placement.as_ref().map(|address| &address.topic_id)],
                    |row| row.get(0),
                )?;
                if !same_body {
                    // До получения согласованной страницы старые файловые ссылки нельзя открыть.
                    connection.execute(
                        "UPDATE history_changes SET media_projection = ?7
                         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
                           AND project_id = ?4 AND user_id = ?5 AND message_id = ?6",
                        params![
                            owner.account_id.0,
                            owner.server,
                            owner.organization_id,
                            owner.project_id,
                            owner.user_id,
                            message.id,
                            MEDIA_PROJECTION_CONFLICT,
                        ],
                    )?;
                    if let Ok(id) = Uuid::parse_str(&message.id) {
                        super::media::remove_message_refs(connection, owner, id)?;
                    }
                    // Окна по прежнему адресу могут показывать сообщение уже не в том разговоре.
                    if let Some(address) = placement {
                        drop_incompatible_members(
                            connection,
                            owner,
                            &message.id,
                            address,
                            revision,
                        )?;
                    }
                    connection.execute(
                        "UPDATE history_windows SET needs_refresh = 1 WHERE id IN (
                         SELECT window_id FROM history_members WHERE account_id = ?1 AND server = ?2
                         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6)",
                        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, message.id],
                    )?;
                }
                return Ok(false);
            }
            record_revision(connection, owner, &message.id, revision, false, None)?;
            if let Some(address) = placement {
                drop_incompatible_members(connection, owner, &message.id, address, revision)?;
            }
        }
    }
    Ok(true)
}

/// Проверяет, менялось ли сообщение после начала отправки или уже сохранялось в истории.
pub(super) fn has_message_revision(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
) -> Result<bool, StorageError> {
    Ok(message_revision(connection, owner, id)?.revision != 0)
}

/// Проверяет отметку удаления перед повторным созданием того же публичного UUID.
pub(super) fn can_create(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
) -> Result<bool, StorageError> {
    Ok(!message_revision(connection, owner, id)?.deleted)
}

/// Помечает края, если удалённый UUID был маркером продолжения.
fn invalidate_message_edges(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
    revision: i64,
) -> Result<(), StorageError> {
    for prefix in ["older", "newer"] {
        connection.execute(
            &format!("UPDATE history_windows SET {prefix}_kind = 0, {prefix}_marker = NULL, {prefix}_revision = ?7
                WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
                AND user_id = ?5 AND {prefix}_marker = ?6"),
            params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, id, revision],
        )?;
    }
    Ok(())
}

/// Удаляет связи с окнами, которым больше не соответствует размещение сообщения.
fn drop_incompatible_members(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
    address: &super::MessagePlacement,
    revision: i64,
) -> Result<(), StorageError> {
    let owner_and_address = params![
        owner.account_id.0,
        owner.server,
        owner.organization_id,
        owner.project_id,
        owner.user_id,
        id,
        address.stream_id,
        address.topic_id
    ];
    connection.execute(
        "UPDATE history_windows SET needs_refresh = 1 WHERE (stream_id <> ?7 OR (topic_id IS NOT NULL AND topic_id <> ?8))
         AND id IN (SELECT window_id FROM history_members WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6)",
        owner_and_address,
    )?;
    let removed = connection.execute(
        "DELETE FROM history_members WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
         AND project_id = ?4 AND user_id = ?5 AND message_id = ?6 AND window_id IN (
             SELECT id FROM history_windows WHERE stream_id <> ?7 OR (topic_id IS NOT NULL AND topic_id <> ?8))",
        owner_and_address,
    )?;
    if removed > 0 {
        invalidate_message_edges(connection, owner, id, revision)?;
    }
    Ok(())
}

/// Убирает окна потерянной области и сбрасывает старые запросы, включая страницы всего стрима.
fn invalidate_scope(
    connection: &Connection,
    owner: &OwnerContext,
    stream: &str,
    topic: Option<&str>,
) -> Result<(), StorageError> {
    let revision = advance(connection, owner, true)?;
    connection.execute(
        "DELETE FROM history_windows WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
         AND project_id = ?4 AND user_id = ?5 AND stream_id = ?6 AND (?7 IS NULL OR topic_id = ?7)",
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
    if topic.is_some() {
        connection.execute(
            "UPDATE history_windows SET needs_refresh = 1, older_kind = 0, newer_kind = 0,
             older_marker = NULL, newer_marker = NULL, older_revision = ?7, newer_revision = ?7
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5
               AND stream_id = ?6 AND topic_id IS NULL",
            params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, stream, revision],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context::AccountId,
        storage::{EventWrite, HistoryChange, MessagePlacement},
    };

    /// Создаёт независимый ключ для тестового аккаунта.
    fn owner(name: &str) -> OwnerContext {
        OwnerContext {
            account_id: AccountId(name.into()),
            server: "workspace.test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: name.into(),
        }
    }

    /// Возвращает тестовый стрим и его отдельную тему.
    fn scopes() -> (HistoryScope, HistoryScope) {
        let stream = HistoryScope {
            stream_id: Uuid::from_u128(1),
            topic_id: None,
        };
        (
            stream,
            HistoryScope {
                topic_id: Some(Uuid::from_u128(2)),
                ..stream
            },
        )
    }

    /// Создаёт сообщение с заданным серверным временем.
    fn message(scope: HistoryScope, id: u128, timestamp: &str) -> HistoryMessage {
        HistoryMessage {
            id: Uuid::from_u128(id),
            stream_id: scope.stream_id,
            topic_id: scope.topic_id.unwrap(),
            text: format!("текст {id}"),
            metadata: HistoryMetadata {
                author_id: Some(Uuid::from_u128(3)),
                created_at: HistoryTime::parse(timestamp).unwrap(),
                updated_at: None,
            },
        }
    }

    /// Записывает первое окно с двумя независимо доступными краями.
    fn new_window(
        storage: &Storage,
        owner: &OwnerContext,
        scope: HistoryScope,
        messages: &[HistoryMessage],
    ) -> i64 {
        let guard = storage
            .prepare_history_page(owner, scope, HistoryPageTarget::New)
            .unwrap()
            .unwrap();
        let HistoryPageOutcome::Saved(id) = storage
            .save_history_page(
                owner,
                guard,
                messages,
                HistoryPageEdges::New {
                    older: HistoryEdge::More("older".into()),
                    newer: HistoryEdge::More("newer".into()),
                },
            )
            .unwrap()
        else {
            panic!("новое окно должно сохраниться")
        };
        id
    }

    /// Сохраняет событие мессенджера вместе с позицией.
    fn event(storage: &Storage, owner: &OwnerContext, position: u64, change: EventWrite<'_>) {
        storage
            .apply(
                owner,
                SOURCE_ID,
                &Cursor {
                    epoch_generation: "test".into(),
                    epoch_version: position,
                },
                change,
            )
            .unwrap();
    }

    /// Проверяет общие тела, независимые окна и чтение по владельцу после повторного открытия.
    #[test]
    fn windows_share_messages_and_keep_independent_edges() {
        let path = tempfile::tempdir().unwrap();
        let database = path.path().join("cache.sqlite3");
        let storage = Storage::open(&database).unwrap();
        let owner_a = owner("A");
        let owner_b = owner("B");
        let (stream, topic) = scopes();
        let base = message(topic, 10, "2026-01-01T10:00:00+00:00");
        let tie = message(topic, 11, "2026-01-01T10:00:00.000000Z");
        let older = message(topic, 9, "2026-01-01T09:00:00Z");
        let newer = message(topic, 12, "2026-01-01T11:00:00.000001Z");

        let id = new_window(&storage, &owner_a, stream, &[tie.clone(), base.clone()]);
        new_window(&storage, &owner_a, topic, std::slice::from_ref(&base));
        let mut changed = base.clone();
        changed.text = "снимок темы обновил общее тело".into();
        new_window(&storage, &owner_a, topic, &[changed.clone()]);
        new_window(
            &storage,
            &owner_a,
            stream,
            &[message(topic, 20, "2026-01-02T10:00:00Z")],
        );
        new_window(&storage, &owner_b, stream, std::slice::from_ref(&base));
        let prepare = |direction| {
            storage
                .prepare_history_page(
                    &owner_a,
                    stream,
                    HistoryPageTarget::Extend {
                        window_id: id,
                        direction,
                    },
                )
                .unwrap()
                .unwrap()
        };
        let older_guard = prepare(HistoryDirection::Older);
        let same_edge = prepare(HistoryDirection::Older);
        let newer_guard = prepare(HistoryDirection::Newer);
        assert_eq!(older_guard.marker(), Some("older"));
        assert_eq!(newer_guard.marker(), Some("newer"));
        for (guard, item) in [(older_guard, older.clone()), (newer_guard, newer.clone())] {
            assert_eq!(
                storage
                    .save_history_page(
                        &owner_a,
                        guard,
                        &[item],
                        HistoryPageEdges::Extend(HistoryEdge::End)
                    )
                    .unwrap(),
                HistoryPageOutcome::Saved(id)
            );
        }
        assert_eq!(
            storage
                .save_history_page(
                    &owner_a,
                    same_edge,
                    &[],
                    HistoryPageEdges::Extend(HistoryEdge::End)
                )
                .unwrap(),
            HistoryPageOutcome::Stale
        );
        assert_eq!(storage.cursor(&owner_a, SOURCE_ID).unwrap(), None);

        drop(storage);
        let storage = Storage::open(database).unwrap();
        let windows = storage.history_windows(&owner_a, stream).unwrap();
        assert_eq!(windows.len(), 2);
        let window = windows.iter().find(|window| window.id == id).unwrap();
        assert_eq!(window.messages, [older, changed.clone(), tie, newer]);
        assert_eq!(
            (&window.older, &window.newer),
            (&HistoryEdge::End, &HistoryEdge::End)
        );
        assert_eq!(
            storage.history_windows(&owner_a, topic).unwrap()[0].messages,
            [changed]
        );
        assert_eq!(
            storage.history_windows(&owner_b, stream).unwrap()[0].messages,
            [base]
        );
    }

    /// Проверяет, что снимки не возвращают удалённые данные, а события не теряют изменения окна.
    #[test]
    fn page_guards_reject_deletions_revocations_and_epoch_reset() {
        let storage = Storage::in_memory().unwrap();
        let owner = owner("A");
        let (stream, topic) = scopes();
        let base = message(topic, 10, "2026-01-01T10:00:00Z");
        let missing = message(topic, 11, "2026-01-01T11:00:00Z");
        let make_guard = |scope| {
            storage
                .prepare_history_page(&owner, scope, HistoryPageTarget::New)
                .unwrap()
                .unwrap()
        };
        let initial = make_guard(topic);
        let marker = base.id.to_string();
        storage
            .save_history_page(
                &owner,
                initial,
                std::slice::from_ref(&base),
                HistoryPageEdges::New {
                    older: HistoryEdge::More(marker.clone()),
                    newer: HistoryEdge::End,
                },
            )
            .unwrap();
        new_window(&storage, &owner, stream, std::slice::from_ref(&base));
        let save_late = |guard, message| {
            storage
                .save_history_page(
                    &owner,
                    guard,
                    &[message],
                    HistoryPageEdges::New {
                        older: HistoryEdge::End,
                        newer: HistoryEdge::End,
                    },
                )
                .unwrap()
        };

        let stale_edit = HistoryChange::Edit {
            message: Message {
                id: base.id.to_string(),
                text: "более старый текст из журнала".into(),
            },
            placement: None,
        };
        event(
            &storage,
            &owner,
            1,
            EventWrite {
                history: Some(&stale_edit),
                ..Default::default()
            },
        );
        let window = storage.history_windows(&owner, topic).unwrap().remove(0);
        assert_eq!(window.messages, std::slice::from_ref(&base));
        assert!(window.needs_refresh);

        let before_deletion = make_guard(topic);
        event(
            &storage,
            &owner,
            2,
            EventWrite {
                history: Some(&HistoryChange::RemoveMessage(base.id.to_string())),
                ..Default::default()
            },
        );
        event(
            &storage,
            &owner,
            3,
            EventWrite {
                history: Some(&HistoryChange::RemoveMessage(missing.id.to_string())),
                ..Default::default()
            },
        );
        assert_eq!(
            save_late(before_deletion, base.clone()),
            HistoryPageOutcome::Stale
        );
        assert_eq!(
            save_late(make_guard(topic), missing),
            HistoryPageOutcome::Stale
        );
        let window = storage.history_windows(&owner, topic).unwrap().remove(0);
        assert_eq!(window.older, HistoryEdge::Unchecked);
        assert!(window.messages.is_empty());
        assert!(
            storage.history_windows(&owner, stream).unwrap()[0]
                .messages
                .is_empty()
        );

        let before_revocation = make_guard(stream);
        let address = MessagePlacement {
            stream_id: stream.stream_id.to_string(),
            topic_id: topic.topic_id.unwrap().to_string(),
        };
        event(
            &storage,
            &owner,
            4,
            EventWrite {
                history: Some(&HistoryChange::RemoveTopic(address)),
                ..Default::default()
            },
        );
        assert_eq!(
            save_late(before_revocation, base.clone()),
            HistoryPageOutcome::Stale
        );
        assert!(storage.history_windows(&owner, topic).unwrap().is_empty());
        assert!(storage.history_windows(&owner, stream).unwrap()[0].needs_refresh);

        let before_reset = make_guard(stream);
        storage.reset_source(&owner, SOURCE_ID).unwrap();
        assert_eq!(
            save_late(before_reset, base.clone()),
            HistoryPageOutcome::Stale
        );
        assert!(storage.history_windows(&owner, stream).unwrap().is_empty());
        assert_eq!(storage.cursor(&owner, SOURCE_ID).unwrap(), None);
        assert!(matches!(
            save_late(make_guard(stream), base),
            HistoryPageOutcome::Saved(_)
        ));
    }

    /// Проверяет применение новых сообщений только в пределах подтверждённого участка.
    #[test]
    fn realtime_does_not_bridge_unconfirmed_history() {
        let storage = Storage::in_memory().unwrap();
        let owner = owner("A");
        let (stream, topic) = scopes();
        let lower = message(topic, 10, "2026-01-01T10:00:00Z");
        let upper = message(topic, 30, "2026-01-01T12:00:00Z");
        let guard = storage
            .prepare_history_page(&owner, stream, HistoryPageTarget::New)
            .unwrap()
            .unwrap();
        storage
            .save_history_page(
                &owner,
                guard,
                &[lower, upper],
                HistoryPageEdges::New {
                    older: HistoryEdge::End,
                    newer: HistoryEdge::End,
                },
            )
            .unwrap();
        let address = MessagePlacement {
            stream_id: stream.stream_id.to_string(),
            topic_id: topic.topic_id.unwrap().to_string(),
        };
        let late_page = storage
            .prepare_history_page(&owner, stream, HistoryPageTarget::New)
            .unwrap()
            .unwrap();
        let inside = message(topic, 20, "2026-01-01T11:00:00Z");
        let outside = message(topic, 40, "2026-01-01T13:00:00Z");
        for (position, item) in [(1, &inside), (2, &outside)] {
            let body = Message {
                id: item.id.to_string(),
                text: item.text.clone(),
            };
            event(
                &storage,
                &owner,
                position,
                EventWrite {
                    message: Some(&body),
                    placement: Some(&address),
                    metadata: Some(&item.metadata),
                    ..Default::default()
                },
            );
        }
        assert_eq!(
            storage
                .save_history_page(
                    &owner,
                    late_page,
                    &[],
                    HistoryPageEdges::New {
                        older: HistoryEdge::End,
                        newer: HistoryEdge::End
                    }
                )
                .unwrap(),
            HistoryPageOutcome::Stale
        );
        let window = storage.history_windows(&owner, stream).unwrap().remove(0);
        assert_eq!(window.messages[1], inside);
        assert_eq!(window.messages.len(), 3);
        assert_eq!(window.newer, HistoryEdge::Unchecked);
        assert!(
            storage
                .message(&owner, &outside.id.to_string())
                .unwrap()
                .is_some()
        );
        assert_eq!(
            storage
                .cursor(&owner, SOURCE_ID)
                .unwrap()
                .unwrap()
                .epoch_version,
            2
        );
    }
}
