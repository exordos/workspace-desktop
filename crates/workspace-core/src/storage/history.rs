//! Хранит окна истории стримов и тем, их края и ссылки на общие записи сообщений.
//! Типы описывают снимок страницы, а операции SQLite защищают его от запоздавшей записи.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{CatalogMessage, Cursor, Message, MessageBody, Storage, StorageError, read_cursor};
use crate::{
    context::OwnerContext,
    messenger::{
        SOURCE_ID,
        quick_reactions::canonical_reaction_name,
        references::{external_links, media_links},
    },
};

const MEDIA_PROJECTION_CONFLICT: i64 = 1;
const MEDIA_PROJECTION_CURRENT: i64 = 4;

/// Область запроса истории. Пустая тема означает весь стрим.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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

/// Серверные поля сообщения, которые нужны для истории.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryMetadata {
    pub author_id: Option<Uuid>,         // Автор, если сервер передал UUID.
    pub created_at: HistoryTime,         // Время создания для сортировки.
    pub updated_at: Option<HistoryTime>, // Время правки, если оно известно.
}

/// Один серверный счётчик реакций и известные участники группы.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct MessageReaction {
    pub emoji: String,            // Ключ эмодзи из ответа сервера.
    pub count: u32,               // Число пользователей с этой реакцией.
    pub users: Option<Vec<Uuid>>, // Полный список, если сервер его раскрыл.
    pub is_own: bool,             // Реакция есть у владельца снимка.
}

/// Одна запись подтверждённой страницы или сохранённого окна.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryMessage {
    pub id: Uuid,                        // Публичный UUID сообщения.
    pub stream_id: Uuid,                 // Стрим сообщения из ответа сервера.
    pub topic_id: Uuid,                  // Тема сообщения внутри стрима.
    pub body: MessageBody,               // Открытый Markdown или зашифрованный конверт.
    pub metadata: HistoryMetadata,       // Автор и серверное время.
    pub reactions: Vec<MessageReaction>, // Счётчики и выбранные реакции владельца.
}

impl HistoryMessage {
    /// Сверяет адрес, автора, время и тело сообщения для цитирования или правки.
    pub fn same_content(&self, other: &Self) -> bool {
        self.id == other.id
            && self.stream_id == other.stream_id
            && self.topic_id == other.topic_id
            && self.body == other.body
            && self.metadata == other.metadata
    }
}

/// Локальные версии канонической строки, полученные перед подтверждённой серверной правкой.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct EncryptedEditVersion {
    generation: i64, // Версия сброса журнала или отзыва доступа.
    revision: i64,   // Последнее изменение именно этой строки.
}

/// Результат применения проверенной квитанции PUT.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ConfirmedEdit {
    Applied, // SQLite обновлён. Можно сообщить ленте о новой версии.
    Present, // Такая версия уже сохранена.
    Stale,   // Версия, с которой начал редактор, больше не действует.
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
    scope: HistoryScope,              // Область запроса страницы.
    target: HistoryPageTarget,        // Новое окно или конкретный край.
    marker: Option<String>,           // Маркер края. Для нового окна его нет.
    edge_revision: i64,               // Версия края на момент старта.
    revision: i64,                    // Последняя локальная версия сообщений на момент запроса.
    generation: i64,                  // Версия сброса кэша и отзыва доступа.
    cursor: Option<Cursor>, // Позиция событий перед запросом. Она не описывает сам снимок.
    verified_messages: HashSet<Uuid>, // Старые отметки, для которых REST подтвердил наличие сообщения.
}

impl HistoryPageGuard {
    /// Возвращает маркер, который нужно передать серверу для догрузки.
    pub(crate) fn marker(&self) -> Option<&str> {
        self.marker.as_deref()
    }

    /// Запоминает UUID, для которого точный запрос подтвердил ту же запись, что и страница.
    pub(crate) fn verify_legacy_encrypted_message(&mut self, id: Uuid) {
        self.verified_messages.insert(id);
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
    let has_encryption: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('sync_messages') WHERE name = 'encryption_key_id')",
        [], |row| row.get(0),
    )?;
    if !has_encryption {
        connection.execute(
            "ALTER TABLE sync_messages ADD COLUMN encryption_key_id TEXT",
            [],
        )?;
    }
    let has_key_service: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('sync_messages') WHERE name = 'key_service_payload')",
        [], |row| row.get(0),
    )?;
    if !has_key_service {
        connection.execute_batch(
            "ALTER TABLE sync_messages ADD COLUMN key_service_payload TEXT;
             ALTER TABLE sync_messages ADD COLUMN key_service_canonical_id TEXT;",
        )?;
    }
    let has_reactions: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('sync_messages') WHERE name = 'reactions_json')",
        [],
        |row| row.get(0),
    )?;
    if !has_reactions {
        connection.execute_batch(
            "ALTER TABLE sync_messages ADD COLUMN reactions_json TEXT NOT NULL DEFAULT '[]';",
        )?;
    }
    let has_reaction_revision: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('sync_messages') WHERE name = 'reactions_revision')",
        [],
        |row| row.get(0),
    )?;
    if !has_reaction_revision {
        connection.execute_batch(
            "ALTER TABLE sync_messages ADD COLUMN reactions_revision INTEGER NOT NULL DEFAULT 0;",
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
    let has_legacy_encryption_flag: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('history_changes') WHERE name = 'legacy_encryption_check')",
        [],
        |row| row.get(0),
    )?;
    if !has_legacy_encryption_flag {
        // Старый клиент записывал удаление и правку неподдерживаемого формата одинаково.
        // У отметок уже нет адреса сообщения, поэтому сверяем все окна затронутого владельца.
        connection.execute_batch(
            "ALTER TABLE history_changes ADD COLUMN legacy_encryption_check INTEGER NOT NULL DEFAULT 0;
             UPDATE history_changes SET legacy_encryption_check = 1 WHERE deleted = 1;
             UPDATE history_windows SET needs_refresh = 1
             WHERE (account_id, server, organization_id, project_id, user_id) IN (
                 SELECT account_id, server, organization_id, project_id, user_id
                 FROM history_changes WHERE legacy_encryption_check = 1
             );
             UPDATE history_state SET generation = generation + 1
             WHERE (account_id, server, organization_id, project_id, user_id) IN (
                 SELECT account_id, server, organization_id, project_id, user_id
                 FROM history_changes WHERE legacy_encryption_check = 1
             );",
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
    revision: i64,                 // Локальная версия последней записи.
    deleted: bool,                 // Запись удалена событием.
    snapshot: bool,                // Последнее подтверждённое тело пришло со страницы истории.
    legacy_encryption_check: bool, // Старая отметка может скрывать поддержанный теперь формат.
    media_projection: i64,         // Конфликт известного тела со свежим событием журнала.
}

impl MessageRevision {
    /// Разрешает сверку только для старой отметки и зашифрованного тела.
    fn needs_encryption_check(&self, message: &HistoryMessage) -> bool {
        self.deleted
            && self.legacy_encryption_check
            && matches!(message.body, MessageBody::Encrypted { .. })
    }
}

/// Читает состояние сообщения перед применением снимка или события.
fn message_revision(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
) -> Result<MessageRevision, StorageError> {
    Ok(connection.query_row(
        "SELECT revision, deleted, snapshot, legacy_encryption_check, media_projection FROM history_changes WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, id],
        |row| Ok(MessageRevision {
            revision: row.get(0)?, deleted: row.get(1)?, snapshot: row.get(2)?,
            legacy_encryption_check: row.get(3)?, media_projection: row.get(4)?,
        }),
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
                       snapshot_epoch = excluded.snapshot_epoch, snapshot_position = excluded.snapshot_position,
                       legacy_encryption_check = 0",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, id,
            revision, deleted, snapshot.is_some(), cursor.map(|position| &position.epoch_generation),
            position],
    )?;
    Ok(())
}

impl Storage {
    /// Подтверждает точное тело доставленного сообщения по владельцу, автору и размещению.
    pub(crate) fn matches_delivered_body(
        &self,
        owner: &OwnerContext,
        prepared: &super::PreparedSend,
    ) -> Result<bool, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let state = message_revision(&connection, owner, &prepared.public_id.to_string())?;
        if state.deleted || state.media_projection == MEDIA_PROJECTION_CONFLICT {
            return Ok(false);
        }
        Ok(
            history_message(&connection, owner, prepared.public_id)?.is_some_and(|message| {
                message.stream_id == prepared.stream_id
                    && message.topic_id == prepared.topic_id
                    && message.metadata.author_id.is_some()
                    && message.metadata.author_id == owner.user_id.parse().ok()
                    && match (&prepared.body, &message.body) {
                        (
                            super::PreparedSendBody::Markdown(sent),
                            MessageBody::Markdown(received),
                        ) => sent == received,
                        _ => prepared.body.matches_message(&message.body),
                    }
            }),
        )
    }

    /// Закрепляет действующую строку редактора непосредственно перед отправкой PUT.
    pub(crate) fn prepare_encrypted_edit(
        &self,
        owner: &OwnerContext,
        expected: &HistoryMessage,
    ) -> Result<Option<EncryptedEditVersion>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        encrypted_edit_version(&connection, owner, expected)
    }

    /// Применяет подтверждённое тело, пока строка и её доступ совпадают с началом операции.
    pub(crate) fn confirm_encrypted_edit(
        &self,
        owner: &OwnerContext,
        expected: &HistoryMessage,
        version: EncryptedEditVersion,
        confirmed: &HistoryMessage,
    ) -> Result<ConfirmedEdit, StorageError> {
        let (
            MessageBody::Encrypted {
                key_id: old_key, ..
            },
            MessageBody::Encrypted {
                key_id: new_key, ..
            },
        ) = (&expected.body, &confirmed.body)
        else {
            return Ok(ConfirmedEdit::Stale);
        };
        if expected.id != confirmed.id
            || expected.stream_id != confirmed.stream_id
            || expected.topic_id != confirmed.topic_id
            || expected.metadata.author_id != confirmed.metadata.author_id
            || expected.metadata.created_at != confirmed.metadata.created_at
            || confirmed.metadata.updated_at.is_none()
            || confirmed.metadata.updated_at < expected.metadata.updated_at
            || old_key != new_key
        {
            return Ok(ConfirmedEdit::Stale);
        }
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if encrypted_edit_version(&transaction, owner, confirmed)?
            .is_some_and(|present| present.generation == version.generation)
        {
            return Ok(ConfirmedEdit::Present);
        }
        if encrypted_edit_version(&transaction, owner, expected)? != Some(version) {
            return Ok(ConfirmedEdit::Stale);
        }
        let revision = advance(&transaction, owner, false)?;
        let cursor = read_cursor(&transaction, owner, SOURCE_ID)?;
        save_snapshot(
            &transaction,
            owner,
            &confirmed.stream_id.to_string(),
            confirmed,
            revision,
            cursor.as_ref(),
            None,
        )?;
        project_media_message(&transaction, owner, &confirmed.id.to_string())?;
        transaction.commit()?;
        drop(connection);
        self.notify_catalog_rows();
        Ok(ConfirmedEdit::Applied)
    }

    /// Метка оригинала цитаты до запроса. Подтверждённое удаление не позволяет его показать.
    pub(crate) fn quote_source_revision(
        &self,
        owner: &OwnerContext,
        message_id: Uuid,
    ) -> Result<Option<(i64, i64)>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let state = read_state(&connection, owner)?;
        let source = message_revision(&connection, owner, &message_id.to_string())?;
        Ok((!source.deleted || source.legacy_encryption_check)
            .then_some((state.generation, source.revision)))
    }

    /// Сверяет оригинал со свежим локальным телом и отметкой конфликта журнала.
    pub(crate) fn quote_source_matches(
        &self,
        owner: &OwnerContext,
        response: &HistoryMessage,
    ) -> Result<bool, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let revision = message_revision(&connection, owner, &response.id.to_string())?;
        if revision.deleted {
            return Ok(revision.needs_encryption_check(response));
        }
        if revision.media_projection == MEDIA_PROJECTION_CONFLICT {
            return Ok(false);
        }
        let Some(current) = history_message(&connection, owner, response.id)? else {
            // Событие правки могло прийти до загрузки самого оригинала.
            return Ok(revision.revision == 0);
        };
        if current.stream_id != response.stream_id
            || current.topic_id != response.topic_id
            || current.metadata.created_at != response.metadata.created_at
            || current
                .metadata
                .author_id
                .is_some_and(|id| Some(id) != response.metadata.author_id)
        {
            return Ok(false);
        }
        let previous_time = current.metadata.updated_at.as_ref();
        let response_time = response.metadata.updated_at.as_ref();
        Ok(if current.body == response.body {
            response_time >= previous_time
        } else {
            // Сам журнал мог не передать время правки. Исходной строке из REST можно
            // предпочесть лишь новый снимок с известным, более поздним временем.
            revision.snapshot && response_time > previous_time
        })
    }

    /// Проверяет каноническую строку и отсутствие более свежей неподтверждённой правки.
    pub(crate) fn matches_encrypted_edit(
        &self,
        owner: &OwnerContext,
        message: &HistoryMessage,
    ) -> Result<bool, StorageError> {
        Ok(matches!(&message.body, MessageBody::Encrypted { .. })
            && self.matches_edit_source(owner, message)?)
    }

    /// Сверяет полный обычный Markdown, включая исходные зашифрованные ссылки.
    pub(crate) fn matches_markdown_edit(
        &self,
        owner: &OwnerContext,
        message: &HistoryMessage,
    ) -> Result<bool, StorageError> {
        Ok(message.body.markdown().is_some() && self.matches_edit_source(owner, message)?)
    }

    /// Подтверждает принадлежность, время и тип сохранённого редактируемого сообщения.
    fn matches_edit_source(
        &self,
        owner: &OwnerContext,
        message: &HistoryMessage,
    ) -> Result<bool, StorageError> {
        let (body, key_id) = message.body.storage_parts();
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_messages LEFT JOIN history_changes
             USING (account_id, server, organization_id, project_id, user_id, message_id)
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
               AND user_id = ?5 AND message_id = ?6 AND stream_id = ?7 AND topic_id = ?8
               AND text = ?9 AND encryption_key_id IS ?10 AND key_service_payload IS NULL AND author_id IS ?11
               AND created_at = ?12 AND updated_at IS ?13
               AND COALESCE(history_changes.deleted, 0) = 0
               AND COALESCE(history_changes.media_projection, 0) <> ?14)",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                message.id.to_string(),
                message.stream_id.to_string(),
                message.topic_id.to_string(),
                body,
                key_id,
                message.metadata.author_id.map(|author| author.to_string()),
                message.metadata.created_at.as_str(),
                message
                    .metadata
                    .updated_at
                    .as_ref()
                    .map(HistoryTime::as_str),
                MEDIA_PROJECTION_CONFLICT,
            ],
            |row| row.get(0),
        )?)
    }

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
            verified_messages: HashSet::new(),
        }))
    }

    /// Находит записи страницы, для которых старая отметка требует точного запроса.
    pub(crate) fn legacy_encrypted_messages<'a>(
        &self,
        owner: &OwnerContext,
        messages: &'a [HistoryMessage],
    ) -> Result<Vec<&'a HistoryMessage>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut pending = Vec::new();
        for message in messages {
            if matches!(message.body, MessageBody::Encrypted { .. })
                && message_revision(&connection, owner, &message.id.to_string())?
                    .needs_encryption_check(message)
            {
                pending.push(message);
            }
        }
        Ok(pending)
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
                   AND (text GLOB '*[uU][rR][nN]:*' OR instr(text, '://') > 0 OR instr(text, 'www.') > 0)
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
            let verified = stored.needs_encryption_check(message)
                && guard.verified_messages.contains(&message.id);
            if (stored.deleted && !verified) || stored.revision > guard.revision {
                return Ok(HistoryPageOutcome::Stale);
            }
            if matches!(message.body, MessageBody::Encrypted { .. })
                && let Some(current) = history_message(&transaction, owner, message.id)?
                && current.metadata.updated_at > message.metadata.updated_at
            {
                // После подтверждённого PUT реплика чтения ещё может вернуть прежнюю правку.
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
                Some(guard.revision),
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
                message.body.markdown(),
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
        drop(connection);
        if !messages.is_empty() {
            self.notify_catalog_rows();
        }
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
    let row: Option<(String, String, String, Option<String>)> = connection
        .query_row(
            "SELECT stream_id, topic_id, created_at,
                    CASE WHEN encryption_key_id IS NULL AND key_service_payload IS NULL THEN text END
             FROM sync_messages
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
            project_message_refs(connection, owner, id, scope, &created_at, text.as_deref())
        }
        _ => super::media::remove_message_refs(connection, owner, id),
    }?;
    mark_media_indexed(connection, owner, message_id)
}

/// Обновляет оба индекса по Markdown и очищает ссылки для зашифрованного тела.
fn project_message_refs(
    connection: &Connection,
    owner: &OwnerContext,
    message_id: Uuid,
    scope: HistoryScope,
    created_at: &str,
    text: Option<&str>,
) -> Result<(), StorageError> {
    let Some(text) = text else {
        return super::media::remove_message_refs(connection, owner, message_id);
    };
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
    reaction_guard: Option<i64>,
) -> Result<(), StorageError> {
    let id = message.id.to_string();
    let (body, key_id) = message.body.storage_parts();
    let (service, canonical) = message.body.key_service_parts()?;
    connection.execute(
        "INSERT INTO sync_messages (account_id, server, organization_id, project_id, user_id,
            message_id, text, stream_id, topic_id, author_id, created_at, updated_at, encryption_key_id,
            key_service_payload, key_service_canonical_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id, message_id) DO UPDATE SET
            text = excluded.text, stream_id = excluded.stream_id, topic_id = excluded.topic_id,
            author_id = COALESCE(excluded.author_id, sync_messages.author_id), created_at = excluded.created_at,
            updated_at = COALESCE(excluded.updated_at, sync_messages.updated_at),
            encryption_key_id = excluded.encryption_key_id, key_service_payload = excluded.key_service_payload,
            key_service_canonical_id = CASE WHEN excluded.key_service_payload IS NULL THEN NULL
                ELSE COALESCE(excluded.key_service_canonical_id, sync_messages.key_service_canonical_id) END",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
            id, body, stream, message.topic_id.to_string(), message.metadata.author_id.map(|id| id.to_string()),
            message.metadata.created_at.0, message.metadata.updated_at.as_ref().map(|time| &time.0), key_id,
            service, canonical],
    )?;
    if let Some(guard) = reaction_guard {
        write_reactions(connection, owner, &id, &message.reactions, revision, guard)?;
    }
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
        "SELECT message_id, stream_id, topic_id, text, author_id, created_at, updated_at, encryption_key_id,
                key_service_payload, key_service_canonical_id, reactions_json
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
        history_record,
    )?;
    records.map(|record| record?.try_into()).collect()
}

/// Поля размещённого сообщения со строковыми идентификаторами SQLite.
struct HistoryRecord {
    id: String,                      // Публичный UUID.
    stream: String,                  // UUID стрима.
    topic: String,                   // UUID темы.
    body: MessageBody,               // Типизированное сохранённое тело.
    author: Option<String>,          // UUID автора, если известен.
    created: String,                 // Нормализованное время создания.
    updated: Option<String>,         // Нормализованное время последней правки.
    reactions: Vec<MessageReaction>, // Сохранённые группы реакций.
}

impl TryFrom<HistoryRecord> for HistoryMessage {
    type Error = StorageError;

    /// Проверяет UUID и собирает каноническую строку истории.
    fn try_from(record: HistoryRecord) -> Result<Self, Self::Error> {
        let parse =
            |value: &str| Uuid::parse_str(value).map_err(|_| StorageError::InvalidHistoryPage);
        Ok(Self {
            id: parse(&record.id)?,
            stream_id: parse(&record.stream)?,
            topic_id: parse(&record.topic)?,
            body: record.body,
            metadata: HistoryMetadata {
                author_id: record.author.as_deref().map(parse).transpose()?,
                created_at: HistoryTime(record.created),
                updated_at: record.updated.map(HistoryTime),
            },
            reactions: record.reactions,
        })
    }
}

/// Разбирает общий набор полей строки и безопасный формат сохранённого тела.
fn history_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryRecord> {
    Ok(HistoryRecord {
        id: row.get(0)?,
        stream: row.get(1)?,
        topic: row.get(2)?,
        body: MessageBody::from_storage(row, 3, 7, 8, 9)?,
        author: row.get(4)?,
        created: row.get(5)?,
        updated: row.get(6)?,
        reactions: read_reactions_column(row, 10)?,
    })
}

/// Читает сохранённые реакции из одного столбца SQLite.
fn read_reactions_column(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<Vec<MessageReaction>> {
    let json: String = row.get(column)?;
    serde_json::from_str(&json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

/// Возвращает текущие группы точной строки владельца, если строка сохранена.
fn read_reactions(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
) -> Result<Option<Vec<MessageReaction>>, StorageError> {
    Ok(connection
        .query_row(
            "SELECT reactions_json FROM sync_messages WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                id
            ],
            |row| read_reactions_column(row, 0),
        )
        .optional()?)
}

/// Записывает счётчики, если с начала чтения не пришла их более свежая версия.
fn write_reactions(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
    reactions: &[MessageReaction],
    revision: i64,
    guard: i64,
) -> Result<(), StorageError> {
    let json = serde_json::to_string(reactions).map_err(StorageError::ReactionProjection)?;
    connection.execute(
        "UPDATE sync_messages SET reactions_json = ?7, reactions_revision = ?8
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
           AND user_id = ?5 AND message_id = ?6 AND reactions_revision <= ?9",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            id,
            json,
            revision,
            guard,
        ],
    )?;
    Ok(())
}

/// Применяет полный серверный снимок и сохраняет известный выбор для групп без списка.
pub(super) fn apply_reaction_aggregate(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
    aggregate: &[MessageReaction],
) -> Result<bool, StorageError> {
    let Some(previous) = read_reactions(connection, owner, id)? else {
        return Ok(false);
    };
    let mut next = aggregate.to_vec();
    for reaction in &mut next {
        if reaction.users.is_none() {
            let emoji = canonical_reaction_name(&reaction.emoji);
            reaction.is_own |= previous
                .iter()
                .any(|old| old.is_own && canonical_reaction_name(&old.emoji) == emoji);
        }
    }
    commit_reaction_change(connection, owner, id, &previous, &next)
}

/// Применяет подтверждение роли пользователя до прихода абсолютного снимка счётчиков.
pub(super) fn apply_own_reaction(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
    emoji: &str,
    selected: bool,
) -> Result<bool, StorageError> {
    let Some(previous) = read_reactions(connection, owner, id)? else {
        return Ok(false);
    };
    let user_id = Uuid::parse_str(&owner.user_id).ok();
    let mut next = previous.clone();
    let position = next
        .binary_search_by(|group| group.emoji.as_str().cmp(emoji))
        .or_else(|insertion| {
            let emoji = canonical_reaction_name(emoji);
            next.iter()
                .position(|group| canonical_reaction_name(&group.emoji) == emoji)
                .ok_or(insertion)
        });
    match position {
        Ok(index) => {
            let group = &mut next[index];
            if group.is_own == selected {
                return Ok(false);
            }
            group.is_own = selected;
            if selected {
                group.count = group.count.saturating_add(1);
                if let (Some(users), Some(user)) = (&mut group.users, user_id) {
                    users.push(user);
                }
            } else {
                group.count = group.count.saturating_sub(1);
                if let Some(users) = &mut group.users {
                    users.retain(|user| Some(*user) != user_id);
                }
                if group.count == 0 {
                    next.remove(index);
                }
            }
        }
        Err(index) if selected => next.insert(
            index,
            MessageReaction {
                emoji: emoji.to_owned(),
                count: 1,
                users: user_id.map(|user| vec![user]),
                is_own: true,
            },
        ),
        Err(_) => return Ok(false),
    }
    commit_reaction_change(connection, owner, id, &previous, &next)
}

/// Повышает только версию реакций, чтобы ранняя страница не вернула прежние счётчики.
fn commit_reaction_change(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
    previous: &[MessageReaction],
    next: &[MessageReaction],
) -> Result<bool, StorageError> {
    if previous == next {
        return Ok(false);
    }
    let revision = advance(connection, owner, false)?;
    write_reactions(connection, owner, id, next, revision, revision)?;
    Ok(true)
}

impl Storage {
    /// Применяет ответ команды только к исходному размещению, пока оно есть в базе.
    pub(crate) fn confirm_message_reaction(
        &self,
        owner: &OwnerContext,
        message_id: Uuid,
        scope: HistoryScope,
        topic_id: Uuid,
        emoji: &str,
        selected: bool,
    ) -> Result<bool, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let matches = history_message(&transaction, owner, message_id)?
            .is_some_and(|row| row.stream_id == scope.stream_id && row.topic_id == topic_id);
        if !matches {
            return Ok(false);
        }
        let changed = apply_own_reaction(
            &transaction,
            owner,
            &message_id.to_string(),
            emoji,
            selected,
        )?;
        transaction.commit()?;
        Ok(changed)
    }
}

/// Читает точное размещённое сообщение из локальной истории того же владельца.
fn history_message(
    connection: &Connection,
    owner: &OwnerContext,
    id: Uuid,
) -> Result<Option<HistoryMessage>, StorageError> {
    let current = connection.query_row(
        "SELECT message_id, stream_id, topic_id, text, author_id, created_at, updated_at, encryption_key_id,
                key_service_payload, key_service_canonical_id, reactions_json
         FROM sync_messages WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
           AND project_id = ?4 AND user_id = ?5 AND message_id = ?6
           AND stream_id IS NOT NULL AND topic_id IS NOT NULL AND created_at IS NOT NULL",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id, id.to_string()],
        history_record,
    ).optional()?;
    current.map(TryInto::try_into).transpose()
}

/// Читает конец известной области по тому же индексу порядка, что и история.
pub(super) fn latest_catalog_message(
    connection: &Connection,
    owner: &OwnerContext,
    scope: HistoryScope,
) -> Result<Option<CatalogMessage>, StorageError> {
    let topic_filter = if scope.topic_id.is_some() {
        "topic_id = ?7"
    } else {
        "topic_id IS NOT NULL AND ?7 IS NULL"
    };
    let mut query = connection.prepare_cached(&format!(
        "SELECT message_id, stream_id, topic_id, text, author_id, created_at, updated_at, encryption_key_id,
                key_service_payload, key_service_canonical_id, reactions_json
         FROM sync_messages
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3 AND project_id = ?4
           AND user_id = ?5 AND stream_id = ?6 AND created_at IS NOT NULL AND {topic_filter}
         ORDER BY created_at DESC, message_id DESC"
    ))?;
    let rows = query.query_map(
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            scope.stream_id.to_string(),
            scope.topic_id.map(|id| id.to_string())
        ],
        history_record,
    )?;
    for row in rows {
        let record = row?;
        // Старый журнал мог сохранить сообщение без публичного UUID.
        let Ok(message) = HistoryMessage::try_from(record) else {
            continue;
        };
        let can_preview = !matches!(message.body, MessageBody::Encrypted { .. })
            || message_revision(connection, owner, &message.id.to_string())?.media_projection
                != MEDIA_PROJECTION_CONFLICT;
        return Ok(Some(CatalogMessage {
            message,
            can_preview,
        }));
    }
    Ok(None)
}

/// Разрешает серверное превью, пока общая строка или событие не уточнили его состояние.
pub(super) fn catalog_hint_allowed(
    connection: &Connection,
    owner: &OwnerContext,
    scope: HistoryScope,
    id: Uuid,
) -> Result<bool, StorageError> {
    Ok(connection.prepare_cached(
        "SELECT NOT EXISTS (
            SELECT 1 FROM history_changes WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
              AND project_id = ?4 AND user_id = ?5 AND message_id = ?6
              AND (deleted OR media_projection = ?9)
         ) AND NOT EXISTS (
            SELECT 1 FROM sync_messages WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
              AND project_id = ?4 AND user_id = ?5 AND message_id = ?6
              AND ((stream_id IS NOT NULL AND topic_id IS NOT NULL AND created_at IS NOT NULL)
                   OR stream_id <> ?7 OR (?8 IS NOT NULL AND topic_id <> ?8))
         )"
    )?.query_row(
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
            id.to_string(), scope.stream_id.to_string(), scope.topic_id.map(|id| id.to_string()), MEDIA_PROJECTION_CONFLICT],
        |row| row.get(0),
    )?)
}

/// Сверяет точный зашифрованный снимок и возвращает его версии под блокировкой базы.
fn encrypted_edit_version(
    connection: &Connection,
    owner: &OwnerContext,
    expected: &HistoryMessage,
) -> Result<Option<EncryptedEditVersion>, StorageError> {
    if !matches!(expected.body, MessageBody::Encrypted { .. }) {
        return Ok(None);
    }
    let revision = message_revision(connection, owner, &expected.id.to_string())?;
    if revision.deleted
        || revision.media_projection == MEDIA_PROJECTION_CONFLICT
        || history_message(connection, owner, expected.id)?
            .is_none_or(|current| !current.same_content(expected))
    {
        return Ok(None);
    }
    Ok(Some(EncryptedEditVersion {
        generation: read_state(connection, owner)?.generation,
        revision: revision.revision,
    }))
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

/// Как сохранённое событие должно повлиять на тело и наблюдателей строки.
pub(super) enum HistoryEventEffect {
    Write,     // Заменяем или удаляем общую строку и публикуем изменение.
    Reconcile, // Тело остаётся до сверки, экран должен перечитать окно.
    Known,     // Уже записали тот же конверт, курсор продвигается без обновления строки.
}

/// Проверяет эффект события в общей строке. Готовит окна к удалению или сверке.
pub(super) fn before_change(
    connection: &Connection,
    owner: &OwnerContext,
    change: &super::HistoryChange,
) -> Result<HistoryEventEffect, StorageError> {
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
        HistoryChange::Edit {
            message,
            placement,
            reactions,
        } => {
            let stored = message_revision(connection, owner, &message.id)?;
            if stored.deleted {
                if stored.legacy_encryption_check {
                    let revision = advance(connection, owner, false)?;
                    set_message_revision(connection, owner, &message.id, revision)?;
                }
                return Ok(HistoryEventEffect::Reconcile);
            }
            let same_body = if stored.snapshot || reactions.is_some() {
                let (body, key_id) = message.body.storage_parts();
                let (service, canonical) = message.body.key_service_parts()?;
                let same_body: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sync_messages WHERE account_id = ?1 AND server = ?2
                     AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6
                     AND text = ?7 AND (?8 IS NULL OR (stream_id = ?8 AND topic_id = ?9))
                     AND encryption_key_id IS ?10 AND key_service_payload IS ?11
                     AND (?12 IS NULL OR key_service_canonical_id IS ?12))",
                    params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
                        message.id, body, placement.as_ref().map(|address| &address.stream_id),
                        placement.as_ref().map(|address| &address.topic_id), key_id, service, canonical],
                    |row| row.get(0),
                )?;
                same_body
            } else {
                false
            };
            if same_body
                && (reactions.is_some() || matches!(message.body, MessageBody::Encrypted { .. }))
            {
                // Событие со счётчиками может повторить прежнее тело. Цитаты и вложения остаются доступны.
                return Ok(HistoryEventEffect::Known);
            }
            if stored.snapshot {
                // REST-снимок мог уже учесть более позднюю правку. Сохраняем его до сверки,
                // но новая локальная версия блокирует страницы, которые начали читать раньше.
                let revision = advance(connection, owner, false)?;
                set_message_revision(connection, owner, &message.id, revision)?;
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
                return Ok(HistoryEventEffect::Reconcile);
            }
            let revision = advance(connection, owner, false)?;
            record_revision(connection, owner, &message.id, revision, false, None)?;
            if let Some(address) = placement {
                drop_incompatible_members(connection, owner, &message.id, address, revision)?;
            }
        }
    }
    Ok(HistoryEventEffect::Write)
}

/// Проверяет, менялось ли сообщение после начала отправки или уже сохранялось в истории.
pub(super) fn has_message_revision(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
) -> Result<bool, StorageError> {
    Ok(message_revision(connection, owner, id)?.revision != 0)
}

/// Проверяет удаление и отменяет ожидающую сверку при более новом событии создания.
pub(super) fn before_creation(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
) -> Result<bool, StorageError> {
    let stored = message_revision(connection, owner, id)?;
    if stored.legacy_encryption_check {
        let revision = advance(connection, owner, false)?;
        set_message_revision(connection, owner, id, revision)?;
    }
    Ok(!stored.deleted)
}

/// Повышает локальную версию UUID, сохраняя подтверждённое тело или отметку удаления.
fn set_message_revision(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
    revision: i64,
) -> Result<(), StorageError> {
    connection.execute(
        "UPDATE history_changes SET revision = ?7 WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5 AND message_id = ?6",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            id,
            revision
        ],
    )?;
    Ok(())
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
        storage::{EventWrite, HistoryChange, MessagePlacement, PreparedSendBody},
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
            reactions: Vec::new(),
            id: Uuid::from_u128(id),
            stream_id: scope.stream_id,
            topic_id: scope.topic_id.unwrap(),
            body: MessageBody::Markdown(format!("текст {id}")),
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

    /// Проверяет добавление, дубль журнала и абсолютный счётчик без инвалидации тела.
    #[test]
    fn message_reactions_keep_history_and_respect_owner() {
        let storage = Storage::in_memory().unwrap();
        let mut owner = owner("first");
        let user = Uuid::from_u128(31);
        owner.user_id = user.to_string();
        let mut other = owner.clone();
        other.account_id = AccountId("second".into());
        let (_, scope) = scopes();
        let original = message(scope, 58, "2026-01-01T10:00:00Z");
        let id = new_window(&storage, &owner, scope, std::slice::from_ref(&original));
        let other_id = new_window(&storage, &other, scope, std::slice::from_ref(&original));
        let current = || {
            storage
                .history_window(&owner, scope, id)
                .unwrap()
                .unwrap()
                .messages
                .remove(0)
        };
        let topic = original.topic_id;
        let earlier = storage
            .prepare_history_page(&owner, scope, HistoryPageTarget::New)
            .unwrap()
            .unwrap();

        assert!(
            storage
                .confirm_message_reaction(&owner, original.id, scope, topic, "heart", true)
                .unwrap()
        );
        assert!(matches!(
            storage
                .save_history_page(
                    &owner,
                    earlier,
                    std::slice::from_ref(&original),
                    HistoryPageEdges::New {
                        older: HistoryEdge::End,
                        newer: HistoryEdge::End
                    },
                )
                .unwrap(),
            HistoryPageOutcome::Saved(_),
        ));
        assert_eq!(
            current().reactions,
            vec![MessageReaction {
                emoji: "heart".into(),
                count: 1,
                users: Some(vec![user]),
                is_own: true,
            }]
        );
        let reaction_event = EventWrite {
            own_reaction: Some((original.id, "heart", true)),
            ..Default::default()
        };
        event(&storage, &owner, 1, reaction_event);
        assert_eq!(current().reactions[0].count, 1);
        let edit = HistoryChange::Edit {
            message: Message {
                id: original.id.to_string(),
                body: original.body.clone(),
            },
            placement: Some(MessagePlacement {
                stream_id: original.stream_id.to_string(),
                topic_id: topic.to_string(),
            }),
            reactions: Some(vec![MessageReaction {
                emoji: "heart".into(),
                count: 10,
                users: None,
                is_own: false,
            }]),
        };
        let saved = storage
            .apply(
                &owner,
                SOURCE_ID,
                &Cursor {
                    epoch_generation: "test".into(),
                    epoch_version: 2,
                },
                EventWrite {
                    history: Some(&edit),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!saved.history_changed);
        assert_eq!(saved.reaction_message, Some(original.id));
        assert_eq!(current().reactions[0].count, 10);
        assert!(current().reactions[0].is_own);
        assert!(
            !storage
                .confirm_message_reaction(&owner, original.id, scope, Uuid::nil(), "heart", false)
                .unwrap()
        );
        assert!(
            storage
                .confirm_message_reaction(&owner, original.id, scope, topic, "heart", false)
                .unwrap()
        );
        assert_eq!(current().reactions[0].count, 9);
        assert!(!current().reactions[0].is_own);
        event(
            &storage,
            &owner,
            3,
            EventWrite {
                own_reaction: Some((original.id, "heart", true)),
                ..Default::default()
            },
        );
        assert!(
            !storage
                .confirm_message_reaction(&owner, original.id, scope, topic, "heart", true)
                .unwrap()
        );
        assert_eq!(current().reactions[0].count, 10);
        assert!(current().reactions[0].is_own);
        assert_eq!(current().body, original.body);
        assert_eq!(
            storage
                .history_window(&other, scope, other_id)
                .unwrap()
                .unwrap()
                .messages,
            vec![original]
        );
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

    /// Отклоняет правку при изменении зашифрованной ссылки, даже пока старый снимок виден.
    #[test]
    fn fragment_edit_requires_exact_live_markdown_source() {
        let storage = Storage::in_memory().unwrap();
        let a = owner("A");
        let b = owner("B");
        let (_, topic) = scopes();
        let mut original = message(topic, 18, "2026-01-02T03:04:05Z");
        original.body = MessageBody::Markdown("[secret](urn:secret:v1:original)".into());
        new_window(&storage, &a, topic, std::slice::from_ref(&original));
        assert!(storage.matches_markdown_edit(&a, &original).unwrap());
        assert!(!storage.matches_markdown_edit(&b, &original).unwrap());
        let change = HistoryChange::Edit {
            reactions: None,
            message: Message {
                id: original.id.to_string(),
                body: MessageBody::Markdown("[secret](urn:secret:v1:changed)".into()),
            },
            placement: Some(MessagePlacement {
                stream_id: original.stream_id.to_string(),
                topic_id: original.topic_id.to_string(),
            }),
        };
        event(
            &storage,
            &a,
            1,
            EventWrite {
                history: Some(&change),
                ..Default::default()
            },
        );
        assert!(!storage.matches_markdown_edit(&a, &original).unwrap());
    }

    /// Реакция во время редактирования сохраняется вместе с подтверждённым текстом.
    #[test]
    fn encrypted_edit_and_quote_fence_ignore_reaction_changes() {
        let storage = Storage::in_memory().unwrap();
        let owner = owner("A");
        let (_, topic) = scopes();
        let mut original = message(topic, 18, "2026-01-02T03:04:05Z");
        let key_id = Uuid::from_u128(42);
        original.body = MessageBody::Encrypted {
            key_id,
            ciphertext: "first".into(),
        };
        let id = new_window(&storage, &owner, topic, std::slice::from_ref(&original));
        let start = storage
            .prepare_encrypted_edit(&owner, &original)
            .unwrap()
            .unwrap();
        let quote = storage.quote_source_revision(&owner, original.id).unwrap();
        assert!(
            storage
                .confirm_message_reaction(
                    &owner,
                    original.id,
                    topic,
                    original.topic_id,
                    "heart",
                    true
                )
                .unwrap()
        );
        assert_eq!(
            storage.quote_source_revision(&owner, original.id).unwrap(),
            quote
        );
        let mut confirmed = original.clone();
        confirmed.body = MessageBody::Encrypted {
            key_id,
            ciphertext: "confirmed".into(),
        };
        confirmed.metadata.updated_at = HistoryTime::parse("2026-01-02T03:04:06Z");
        assert!(matches!(
            storage
                .confirm_encrypted_edit(&owner, &original, start, &confirmed)
                .unwrap(),
            ConfirmedEdit::Applied
        ));
        let saved = storage
            .history_window(&owner, topic, id)
            .unwrap()
            .unwrap()
            .messages
            .remove(0);
        assert!(saved.same_content(&confirmed));
        assert!(saved.reactions[0].is_own);
    }

    /// Подтверждение PUT не заменяет более свежую строку или отозванный доступ.
    #[test]
    fn confirmed_encrypted_edit_keeps_newer_snapshot() {
        let storage = Storage::in_memory().unwrap();
        let owner = owner("A");
        let (_, topic) = scopes();
        let mut original = message(topic, 18, "2026-01-02T03:04:05Z");
        let key_id = Uuid::from_u128(42);
        original.body = MessageBody::Encrypted {
            key_id,
            ciphertext: "first".into(),
        };
        new_window(&storage, &owner, topic, std::slice::from_ref(&original));
        let start = storage
            .prepare_encrypted_edit(&owner, &original)
            .unwrap()
            .unwrap();
        let mut confirmed = original.clone();
        confirmed.body = MessageBody::Encrypted {
            key_id,
            ciphertext: "confirmed".into(),
        };
        confirmed.metadata.updated_at = HistoryTime::parse("2026-01-02T03:04:06Z");
        let mut newer = confirmed.clone();
        newer.body = MessageBody::Encrypted {
            key_id,
            ciphertext: "newer".into(),
        };
        newer.metadata.updated_at = HistoryTime::parse("2026-01-02T03:04:07Z");
        let change = HistoryChange::Edit {
            reactions: None,
            message: Message {
                id: original.id.to_string(),
                body: newer.body.clone(),
            },
            placement: None,
        };
        event(
            &storage,
            &owner,
            1,
            EventWrite {
                history: Some(&change),
                ..Default::default()
            },
        );
        new_window(&storage, &owner, topic, std::slice::from_ref(&newer));
        assert!(matches!(
            storage
                .confirm_encrypted_edit(&owner, &original, start, &confirmed)
                .unwrap(),
            ConfirmedEdit::Stale
        ));
        assert_eq!(
            storage
                .message(&owner, &newer.id.to_string())
                .unwrap()
                .unwrap()
                .body,
            newer.body
        );
        let post = storage
            .prepare_encrypted_edit(&owner, &newer)
            .unwrap()
            .unwrap();
        event(
            &storage,
            &owner,
            2,
            EventWrite {
                history: Some(&HistoryChange::RemoveStream(topic.stream_id.to_string())),
                ..Default::default()
            },
        );
        assert!(matches!(
            storage
                .confirm_encrypted_edit(&owner, &newer, post, &newer)
                .unwrap(),
            ConfirmedEdit::Stale
        ));
    }

    /// Проверяет снимок, живую правку и чтение после перезапуска без индексации конвертов.
    #[test]
    fn encrypted_history_survives_reopen_without_projecting_links() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.sqlite");
        let a = owner("A");
        let b = owner("B");
        let (stream, topic) = scopes();
        let storage = Storage::open(&path).unwrap();
        let ciphertext = "[ссылка](https://workspace.test/secret)".to_owned();
        let key_id = Uuid::from_u128(40);
        let mut first = message(topic, 10, "2026-01-01T10:00:00Z");
        first.body = MessageBody::Encrypted {
            key_id,
            ciphertext: ciphertext.clone(),
        };
        let latest = message(topic, 12, "2026-01-01T12:00:00Z");
        let window_id = new_window(&storage, &a, stream, &[first.clone(), latest.clone()]);
        let other_owner_row = message(topic, 10, "2026-01-01T10:00:00Z");
        let b_window_id = new_window(&storage, &b, stream, std::slice::from_ref(&other_owner_row));
        let live = message(topic, 11, "2026-01-01T11:00:00Z");
        let address = MessagePlacement {
            stream_id: live.stream_id.to_string(),
            topic_id: live.topic_id.to_string(),
        };
        let mut created = Message {
            id: live.id.to_string(),
            body: first.body.clone(),
        };
        event(
            &storage,
            &a,
            1,
            EventWrite {
                message: Some(&created),
                placement: Some(&address),
                metadata: Some(&live.metadata),
                ..Default::default()
            },
        );
        created.body = MessageBody::Encrypted {
            key_id: Uuid::from_u128(41),
            ciphertext,
        };
        event(
            &storage,
            &a,
            2,
            EventWrite {
                history: Some(&HistoryChange::Edit {
                    reactions: None,
                    message: created.clone(),
                    placement: Some(address),
                }),
                ..Default::default()
            },
        );
        drop(storage);

        let storage = Storage::open(&path).unwrap();
        let history = storage
            .history_window(&a, stream, window_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            history.messages,
            [
                first,
                HistoryMessage {
                    body: created.body.clone(),
                    ..live
                },
                latest
            ]
        );
        assert_eq!(storage.message(&a, &created.id).unwrap(), Some(created));
        let other_window = storage
            .history_window(&b, stream, b_window_id)
            .unwrap()
            .unwrap();
        assert_eq!(other_window.messages, [other_owner_row]);
        let connection = storage.connection.lock().unwrap();
        let references: u32 = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM messenger_web_links) +
                        (SELECT COUNT(*) FROM messenger_media_refs)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(references, 0);
    }

    /// Дополняет индекс из старой базы после появления новых URN, не восстанавливая спорную запись.
    #[test]
    fn rebuilds_v3_gallery_links_and_uppercase_attachments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("v3-gallery.sqlite");
        let owner = owner("A");
        let (_, topic) = scopes();
        let file = Uuid::from_u128(71);
        let mut web = message(topic, 72, "2026-01-01T10:00:00Z");
        web.body = MessageBody::Markdown("[Документ](URN:URL:https://example.test/guide)".into());
        let mut media = message(topic, 73, "2026-01-01T10:01:00Z");
        media.body = MessageBody::Markdown(format!("[Файл](URN:FILE:{file})"));
        let mut conflicted = message(topic, 74, "2026-01-01T10:02:00Z");
        conflicted.body =
            MessageBody::Markdown("[Старая цель](urn:url:https://example.test/stale)".into());
        let conflicted_id = conflicted.id.to_string();
        let storage = Storage::open(&path).unwrap();
        new_window(&storage, &owner, topic, &[web, media, conflicted]);
        {
            let db = storage.connection.lock().unwrap();
            db.execute_batch(
                "UPDATE history_changes SET media_projection = 3;
                 DELETE FROM messenger_web_links;
                 DELETE FROM messenger_media_refs;",
            )
            .unwrap();
            db.execute(
                "UPDATE history_changes SET media_projection = ?1 WHERE message_id = ?2",
                params![MEDIA_PROJECTION_CONFLICT, conflicted_id],
            )
            .unwrap();
        }
        drop(storage);

        let storage = Storage::open(&path).unwrap();
        let generation = storage.media_history_generation(&owner).unwrap();
        assert!(
            storage
                .index_media_history(&owner, topic, generation, None)
                .unwrap()
        );
        let db = storage.connection.lock().unwrap();
        let links = db
            .prepare("SELECT url FROM messenger_web_links")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(links, ["https://example.test/guide"]);
        let stored_file: String = db
            .query_row("SELECT file FROM messenger_media_refs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored_file, file.to_string());
        assert_eq!(
            message_revision(&db, &owner, &conflicted_id)
                .unwrap()
                .media_projection,
            MEDIA_PROJECTION_CONFLICT,
        );
    }

    /// Хранит служебное размещение за владельцем и сохраняет канонический UUID при неполном снимке.
    #[test]
    fn key_service_placement_is_durable_without_media_or_notification_projection() {
        use crate::storage::{KeyResponseTarget, KeyServicePayload};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service-history.sqlite");
        let a = owner("A");
        let b = owner("B");
        let (stream, topic) = scopes();
        let storage = Storage::open(&path).unwrap();
        let canonical = Uuid::from_u128(61);
        let payload = KeyServicePayload::Grant {
            target: KeyResponseTarget {
                key_uuid: Uuid::from_u128(62),
                request_message_uuid: Uuid::from_u128(63),
                sender_device_id: "old-device".into(),
                recipient_device_id: "new-device".into(),
            },
            content: "[служебный конверт](https://workspace.test/opaque)".into(),
        };
        let initial = message(topic, 10, "2026-01-01T10:00:00Z");
        new_window(&storage, &a, stream, std::slice::from_ref(&initial));
        let mut service = message(topic, 11, "2026-01-01T11:00:00Z");
        service.id = Uuid::new_v5(&service.topic_id, canonical.to_string().as_bytes());
        service.body = MessageBody::KeyService {
            payload: payload.clone(),
            canonical_id: Some(canonical),
        };
        let wire_id = service.id.to_string();
        let placement = MessagePlacement {
            stream_id: service.stream_id.to_string(),
            topic_id: service.topic_id.to_string(),
        };
        event(
            &storage,
            &a,
            1,
            EventWrite {
                message: Some(&Message {
                    id: wire_id.clone(),
                    body: service.body.clone(),
                }),
                placement: Some(&placement),
                metadata: Some(&service.metadata),
                notification_conversation: Some(&placement.stream_id),
                ..Default::default()
            },
        );
        let incomplete = HistoryMessage {
            body: MessageBody::KeyService {
                payload,
                canonical_id: None,
            },
            ..service.clone()
        };
        let window_id = new_window(&storage, &a, stream, &[initial, incomplete]);
        drop(storage);

        let reopened = Storage::open(&path).unwrap();
        assert_eq!(
            reopened.message(&a, &wire_id).unwrap().unwrap().body,
            service.body
        );
        assert!(reopened.message(&b, &wire_id).unwrap().is_none());
        assert_eq!(
            reopened
                .history_window(&a, stream, window_id)
                .unwrap()
                .unwrap()
                .messages
                .last(),
            Some(&service)
        );
        assert_eq!(
            reopened
                .cursor(&a, SOURCE_ID)
                .unwrap()
                .unwrap()
                .epoch_version,
            1
        );
        let count: u32 = reopened
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM messenger_web_links) +
                    (SELECT COUNT(*) FROM messenger_media_refs) +
                    (SELECT COUNT(*) FROM messenger_notification_targets)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    /// Перепроверяет только старую отметку, оставленную клиентом после правки E2EE.
    #[test]
    fn legacy_encrypted_edit_requires_new_scoped_history_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.sqlite");
        let a = owner("A");
        let b = owner("B");
        let (stream, topic) = scopes();
        let ordinary = message(topic, 14, "2026-01-01T10:00:00Z");
        let mut encrypted = ordinary.clone();
        encrypted.body = MessageBody::Encrypted {
            key_id: Uuid::from_u128(40),
            ciphertext: "YQ==".into(),
        };
        let plaintext_removed = message(topic, 15, "2026-01-01T11:00:00Z");
        let stored = Storage::open(&path).unwrap();
        let a_window = new_window(
            &stored,
            &a,
            stream,
            std::slice::from_ref(&plaintext_removed),
        );
        let b_window = new_window(&stored, &b, stream, std::slice::from_ref(&ordinary));
        for (position, id) in [(1, encrypted.id), (2, plaintext_removed.id)] {
            // До поддержки E2EE правка такого сообщения шла по пути обычного удаления.
            event(
                &stored,
                &a,
                position,
                EventWrite {
                    history: Some(&HistoryChange::RemoveMessage(id.to_string())),
                    ..Default::default()
                },
            );
        }
        let outbox = super::super::PreparedSend {
            canonical_id: Uuid::from_u128(19),
            public_id: Uuid::from_u128(20),
            stream_id: stream.stream_id,
            topic_id: topic.topic_id.unwrap(),
            body: PreparedSendBody::Markdown("готовое сообщение".into()),
        };
        assert!(stored.save_prepared_send(&a, &outbox).unwrap());
        let prepare = |storage: &Storage| {
            storage
                .prepare_history_page(&a, stream, HistoryPageTarget::New)
                .unwrap()
                .unwrap()
        };
        let mut pre_migration_request = prepare(&stored);
        pre_migration_request.verify_legacy_encrypted_message(encrypted.id);
        drop(stored);
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "ALTER TABLE history_changes DROP COLUMN legacy_encryption_check;
                 ALTER TABLE sync_messages DROP COLUMN encryption_key_id;",
            )
            .unwrap();

        let stored = Storage::open(&path).unwrap();
        assert!(
            stored
                .history_window(&a, stream, a_window)
                .unwrap()
                .unwrap()
                .needs_refresh
        );
        assert!(
            !stored
                .history_window(&b, stream, b_window)
                .unwrap()
                .unwrap()
                .needs_refresh
        );
        assert_eq!(stored.pending_sends(&a).unwrap(), [outbox]);
        let cursor = stored.cursor(&a, SOURCE_ID).unwrap().unwrap();
        assert_eq!(
            (&*cursor.epoch_generation, cursor.epoch_version),
            ("test", 2)
        );
        let save = |guard, message| {
            stored
                .save_history_page(
                    &a,
                    guard,
                    std::slice::from_ref(message),
                    HistoryPageEdges::New {
                        older: HistoryEdge::End,
                        newer: HistoryEdge::End,
                    },
                )
                .unwrap()
        };
        assert_eq!(
            save(pre_migration_request, &encrypted),
            HistoryPageOutcome::Stale
        );
        assert_eq!(
            save(prepare(&stored), &encrypted),
            HistoryPageOutcome::Stale
        );
        let mut wrong_proof = prepare(&stored);
        wrong_proof.verify_legacy_encrypted_message(plaintext_removed.id);
        assert_eq!(
            save(wrong_proof, &plaintext_removed),
            HistoryPageOutcome::Stale
        );
        assert_eq!(
            stored
                .legacy_encrypted_messages(&a, std::slice::from_ref(&encrypted))
                .unwrap(),
            [&encrypted]
        );
        assert!(
            stored
                .legacy_encrypted_messages(&b, std::slice::from_ref(&encrypted))
                .unwrap()
                .is_empty()
        );

        let mut stale_request = prepare(&stored);
        stale_request.verify_legacy_encrypted_message(encrypted.id);
        let creation = Message {
            id: encrypted.id.to_string(),
            body: encrypted.body.clone(),
        };
        for position in [2, 3] {
            event(
                &stored,
                &a,
                position,
                EventWrite {
                    message: Some(&creation),
                    ..Default::default()
                },
            );
            assert!(stored.message(&a, &creation.id).unwrap().is_none());
        }
        assert_eq!(save(stale_request, &encrypted), HistoryPageOutcome::Stale);
        let mut stale_request = prepare(&stored);
        stale_request.verify_legacy_encrypted_message(encrypted.id);
        event(
            &stored,
            &a,
            4,
            EventWrite {
                history: Some(&HistoryChange::Edit {
                    reactions: None,
                    message: creation.clone(),
                    placement: None,
                }),
                ..Default::default()
            },
        );
        assert!(stored.message(&a, &creation.id).unwrap().is_none());
        assert_eq!(save(stale_request, &encrypted), HistoryPageOutcome::Stale);

        let mut verified_request = prepare(&stored);
        verified_request.verify_legacy_encrypted_message(encrypted.id);
        assert!(matches!(
            save(verified_request, &encrypted),
            HistoryPageOutcome::Saved(_)
        ));
        assert_eq!(
            stored.message(&a, &creation.id).unwrap(),
            Some(creation.clone())
        );
        assert_eq!(
            stored.message(&b, &creation.id).unwrap().unwrap().body,
            ordinary.body
        );
        event(
            &stored,
            &a,
            5,
            EventWrite {
                history: Some(&HistoryChange::RemoveMessage(creation.id.clone())),
                ..Default::default()
            },
        );
        let mut wrong_proof = prepare(&stored);
        wrong_proof.verify_legacy_encrypted_message(encrypted.id);
        assert_eq!(save(wrong_proof, &encrypted), HistoryPageOutcome::Stale);
        assert!(stored.message(&a, &creation.id).unwrap().is_none());
        let before_reset = prepare(&stored);
        stored.reset_source(&a, SOURCE_ID).unwrap();
        assert_eq!(save(before_reset, &encrypted), HistoryPageOutcome::Stale);
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
        changed.body = MessageBody::Markdown("снимок темы обновил общее тело".into());
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
            reactions: None,
            message: Message {
                id: base.id.to_string(),
                body: MessageBody::Markdown("более старый текст из журнала".into()),
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
                body: item.body.clone(),
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
