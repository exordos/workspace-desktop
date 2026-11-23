//! Читает каталог и последние известные сообщения под одной блокировкой SQLite.

use std::collections::HashMap;

use rusqlite::Connection;
use uuid::Uuid;

use crate::{context::OwnerContext, messenger::catalog::Chat};

use super::{HistoryMessage, HistoryScope, StorageError, catalog_counts, history};

/// Последняя сохранённая строка области и возможность использовать её тело.
pub(crate) struct CatalogMessage {
    pub(crate) message: HistoryMessage, // Каноническое сообщение из общей базы.
    pub(crate) can_preview: bool,       // Спорное защищённое тело ждёт сверки с сервером.
}

/// Данные для построения списка разговоров одного владельца.
pub(crate) struct CatalogSnapshot {
    pub(crate) chats: Vec<Chat>, // Доступные разговоры и ссылки из серверного снимка.
    pub(crate) messages: HashMap<HistoryScope, CatalogMessage>, // Последние строки личек и тем.
}

/// Читает последние сообщения только для областей доступного каталога.
pub(super) fn read_snapshot(
    connection: &Connection,
    owner: &OwnerContext,
) -> Result<Option<CatalogSnapshot>, StorageError> {
    let Some(mut chats) = catalog_counts::read_catalog(connection, owner)? else {
        return Ok(None);
    };
    let mut messages = HashMap::new();
    for chat in &mut chats {
        let scope = HistoryScope {
            stream_id: chat.id,
            topic_id: None,
        };
        check_hint(connection, owner, scope, &mut chat.metadata.last_message_id)?;
        if chat.direct
            && let Some(message) = history::latest_catalog_message(connection, owner, scope)?
        {
            messages.insert(scope, message);
        }
        for topic in &mut chat.topics {
            let scope = HistoryScope {
                topic_id: Some(topic.id),
                ..scope
            };
            check_hint(
                connection,
                owner,
                scope,
                &mut topic.metadata.last_message_id,
            )?;
            if let Some(message) = history::latest_catalog_message(connection, owner, scope)? {
                messages.insert(scope, message);
            }
        }
    }
    Ok(Some(CatalogSnapshot { chats, messages }))
}

/// Оставляет подсказку сервера, когда её можно использовать без общей записи.
fn check_hint(
    connection: &Connection,
    owner: &OwnerContext,
    scope: HistoryScope,
    last_message_id: &mut Option<Uuid>,
) -> Result<(), StorageError> {
    if let Some(id) = *last_message_id
        && !history::catalog_hint_allowed(connection, owner, scope, id)?
    {
        *last_message_id = None;
    }
    Ok(())
}
