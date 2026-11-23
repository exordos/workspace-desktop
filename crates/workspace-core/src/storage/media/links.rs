//! Хранит веб-ссылки из подтверждённых сообщений и выдаёт их страницами.

use rusqlite::{Connection, params};
use uuid::Uuid;

use super::{
    ExternalLink, MediaCache, MediaCacheError, optional_uuid_column, owner_key,
    queries::{GalleryCursorScope, after_page, page_limit},
    uuid_column,
};
use crate::{
    context::OwnerContext,
    storage::{HistoryScope, StorageError},
};

/// Граница следующей страницы веб-ссылок того же разговора.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkHistoryCursor {
    scope: GalleryCursorScope, // Владелец, поколение и область начала просмотра.
    created_at: String,        // Время исходного сообщения.
    id: i64,                   // Версия ссылки для разрешения совпадений времени.
}

/// Веб-ссылка, известная из открытого Markdown исходного сообщения.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkHistoryItem {
    pub scope: HistoryScope, // Стрим и тема сообщения, где встретилась ссылка.
    pub message_id: Uuid,    // Исходное сообщение.
    pub created_at: String,  // Серверное время сообщения в UTC.
    pub url: String,         // Проверенный абсолютный адрес HTTP или HTTPS.
    pub label: String,       // Подпись из сообщения или адрес, когда подписи нет.
}

/// Страница веб-ссылок из уже известной Core истории.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkHistoryPage {
    pub links: Vec<LinkHistoryItem>,     // Ссылки текущей страницы.
    pub next: Option<LinkHistoryCursor>, // Граница следующей страницы, если она есть.
}

/// Создаёт проекцию ссылок и порядок выдачи по разговору и дате.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS messenger_web_links (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL, stream TEXT NOT NULL, topic TEXT,
            message TEXT NOT NULL, position INTEGER NOT NULL,
            url TEXT NOT NULL, label TEXT NOT NULL, created_at TEXT NOT NULL,
            UNIQUE(owner, message, position)
        );
        CREATE INDEX IF NOT EXISTS messenger_web_links_scope
            ON messenger_web_links(owner, stream, created_at DESC, id DESC);",
    )?;
    Ok(())
}

/// Заменяет ссылки сообщения вместе с сохранением его актуального текста.
pub(crate) fn replace_link_refs(
    connection: &Connection,
    owner: &OwnerContext,
    message_id: Uuid,
    scope: HistoryScope,
    created_at: &str,
    links: &[ExternalLink],
) -> Result<(), StorageError> {
    let owner = owner_key(owner);
    let stream = scope.stream_id.to_string();
    let topic = scope.topic_id.map(|id| id.to_string());
    let message = message_id.to_string();
    let mut insert = connection.prepare_cached(
        "INSERT INTO messenger_web_links (owner, stream, topic, message, position, url, label, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(owner, message, position) DO UPDATE SET
               stream = excluded.stream, topic = excluded.topic, url = excluded.url,
               label = excluded.label, created_at = excluded.created_at",
    )?;
    for (position, link) in links.iter().enumerate() {
        insert.execute(params![
            owner,
            stream,
            topic,
            message,
            position as i64,
            link.url,
            link.label,
            created_at
        ])?;
    }
    // У неизменившейся позиции сохраняется id, поэтому пересечение окон не срывает пагинацию.
    connection.execute(
        "DELETE FROM messenger_web_links WHERE owner = ?1 AND message = ?2 AND position >= ?3",
        params![owner, message, links.len() as i64],
    )?;
    Ok(())
}

/// Удаляет все ссылки сообщения при удалении или конфликте его версий.
pub(super) fn remove_message(
    connection: &Connection,
    owner: &OwnerContext,
    message_id: Uuid,
) -> Result<(), StorageError> {
    connection.execute(
        "DELETE FROM messenger_web_links WHERE owner = ?1 AND message = ?2",
        params![owner_key(owner), message_id.to_string()],
    )?;
    Ok(())
}

/// Удаляет ссылки отозванного стрима или темы.
pub(super) fn remove_scope(
    connection: &Connection,
    owner: &str,
    stream: &str,
    topic: Option<&str>,
) -> Result<(), StorageError> {
    connection.execute(
        "DELETE FROM messenger_web_links WHERE owner = ?1 AND stream = ?2
         AND (?3 IS NULL OR topic = ?3)",
        params![owner, stream, topic],
    )?;
    Ok(())
}

/// Удаляет проекцию владельца после сброса журнала.
pub(super) fn remove_owner(connection: &Connection, owner: &str) -> Result<(), StorageError> {
    connection.execute(
        "DELETE FROM messenger_web_links WHERE owner = ?1",
        params![owner],
    )?;
    Ok(())
}

impl MediaCache {
    /// Возвращает страницу веб-ссылок из локального индекса подтверждённой области.
    pub(crate) fn list_links(
        &self,
        owner: &OwnerContext,
        scope: HistoryScope,
        generation: i64,
        after: Option<LinkHistoryCursor>,
        limit: usize,
    ) -> Result<LinkHistoryPage, MediaCacheError> {
        let boundary = GalleryCursorScope::new(owner, scope, generation);
        if after
            .as_ref()
            .is_some_and(|cursor| cursor.scope != boundary)
        {
            return Err(MediaCacheError::Stale);
        }
        let connection = self.connection()?;
        let limit = page_limit(limit);
        let mut statement = connection.prepare_cached(
            "SELECT id, stream, topic, message, created_at, url, label
             FROM messenger_web_links
             WHERE owner = ?1 AND stream = ?2 AND (?3 IS NULL OR topic = ?3)
               AND (?4 IS NULL OR (created_at, id) < (?4, ?5))
             ORDER BY created_at DESC, id DESC LIMIT ?6",
        )?;
        let mut entries = statement
            .query_map(
                params![
                    owner_key(owner),
                    scope.stream_id.to_string(),
                    scope.topic_id.map(|id| id.to_string()),
                    after.as_ref().map(|cursor| cursor.created_at.as_str()),
                    after.as_ref().map(|cursor| cursor.id),
                    limit as i64 + 1
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        LinkHistoryItem {
                            scope: HistoryScope {
                                stream_id: uuid_column(row, 1)?,
                                topic_id: optional_uuid_column(row, 2)?,
                            },
                            message_id: uuid_column(row, 3)?,
                            created_at: row.get(4)?,
                            url: row.get(5)?,
                            label: row.get(6)?,
                        },
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next = after_page(&mut entries, limit).map(|(id, item)| LinkHistoryCursor {
            scope: boundary,
            id: *id,
            created_at: item.created_at.clone(),
        });
        Ok(LinkHistoryPage {
            links: entries.into_iter().map(|(_, item)| item).collect(),
            next,
        })
    }
}
