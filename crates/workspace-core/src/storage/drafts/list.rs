//! Читает локальные черновики владельца и удаляет только выбранную версию.

use std::collections::HashSet;

use super::{DraftReplies, DraftScope, DraftVersion};
use crate::{
    context::OwnerContext,
    storage::{HistoryTime, Storage, StorageError},
};
use rusqlite::params;
use uuid::Uuid;

/// Данные локального черновика для одной строки списка.
pub struct DraftListEntry<Preview = String> {
    pub scope: DraftScope,     // Исходный разговор и точная тема редактора.
    pub version: DraftVersion, // UUID и версия, показанные пользователю.
    pub preview: Preview,      // Короткое превью текста или первой непустой вкладки ответа.
    pub updated_at: Option<HistoryTime>, // Время последней записи. У старых записей может отсутствовать.
    pub from_server: bool, // Истина, если строка прочитана с сервера и не сохранена локально.
}

impl<Preview> DraftListEntry<Preview> {
    /// Меняет представление превью, сохраняя адрес и версию сохранённого черновика.
    pub fn map_preview<T>(self, map: impl FnOnce(Preview) -> T) -> DraftListEntry<T> {
        DraftListEntry {
            scope: self.scope,
            version: self.version,
            preview: map(self.preview),
            updated_at: self.updated_at,
            from_server: self.from_server,
        }
    }
}

impl Storage {
    /// Возвращает сохранённые записи текущего владельца от свежих к старым.
    pub(crate) fn list_plaintext_drafts(
        &self,
        owner: &OwnerContext,
    ) -> Result<Vec<DraftListEntry>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut query = connection.prepare(
            "SELECT draft_id, stream_id, topic_id, local_version, markdown,
                    reply_context, updated_at
             FROM messenger_drafts
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND markdown IS NOT NULL
               AND (markdown != '' OR reply_context IS NOT NULL)
             ORDER BY write_order DESC NULLS LAST, updated_at DESC NULLS LAST, sequence DESC",
        )?;
        let mut rows = query.query(params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
        ])?;
        let mut drafts = Vec::new();
        while let Some(row) = rows.next()? {
            let text: String = row.get(4)?;
            let replies: Option<String> = row.get(5)?;
            let Some(preview) = preview(&text, replies.as_deref())? else {
                continue;
            };
            let updated_at: Option<String> = row.get(6)?;
            drafts.push(DraftListEntry {
                scope: DraftScope {
                    stream_id: row_id(&row.get::<_, String>(1)?)?,
                    topic_id: row_id(&row.get::<_, String>(2)?)?,
                },
                version: DraftVersion {
                    id: row_id(&row.get::<_, String>(0)?)?,
                    version: u64::try_from(row.get::<_, i64>(3)?)
                        .map_err(|_| StorageError::InvalidDraftRecord)?,
                },
                preview,
                updated_at: updated_at.as_deref().and_then(HistoryTime::parse),
                from_server: false,
            });
        }
        Ok(drafts)
    }

    /// Возвращает все локальные UUID, включая удалённые и переданные очереди отправки.
    pub(crate) fn claimed_draft_ids(
        &self,
        owner: &OwnerContext,
    ) -> Result<HashSet<Uuid>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut query = connection.prepare(
            "SELECT draft_id FROM messenger_drafts
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5",
        )?;
        let ids = query.query_map(
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
            ],
            |row| row.get::<_, String>(0),
        )?;
        ids.map(|id| row_id(&id?)).collect()
    }

    /// Проверяет, принадлежит ли UUID локальному снимку, даже если его текст уже удалён.
    pub(crate) fn draft_id_claimed(
        &self,
        owner: &OwnerContext,
        id: Uuid,
    ) -> Result<bool, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM messenger_drafts
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND draft_id = ?6)",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                id.to_string()
            ],
            |row| row.get(0),
        )?)
    }

    /// Убирает текст выбранной версии и оставляет UUID занятым для старых записей редактора.
    pub(crate) fn delete_plaintext_draft(
        &self,
        owner: &OwnerContext,
        scope: DraftScope,
        version: DraftVersion,
    ) -> Result<bool, StorageError> {
        let version_number = i64::try_from(version.version)
            .ok()
            .filter(|value| *value > 0)
            .ok_or(StorageError::InvalidDraftVersion)?;
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let written = connection.execute(
            "UPDATE messenger_drafts SET markdown = NULL, reply_context = NULL
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND stream_id = ?6 AND topic_id = ?7
               AND draft_id = ?8 AND local_version = ?9 AND markdown IS NOT NULL",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                scope.stream_id.to_string(),
                scope.topic_id.to_string(),
                version.id.to_string(),
                version_number,
            ],
        )?;
        Ok(written == 1)
    }
}

/// Читает UUID из записи базы и отклоняет повреждённую строку.
fn row_id(id: &str) -> Result<Uuid, StorageError> {
    Uuid::parse_str(id).map_err(|_| StorageError::InvalidDraftRecord)
}

/// Берёт текст обычного редактора или непустой ответ и ограничивает длину превью.
fn preview(text: &str, replies: Option<&str>) -> Result<Option<String>, StorageError> {
    let reply;
    let text = if !text.trim().is_empty() {
        text
    } else if let Some(replies) = replies {
        let replies: DraftReplies =
            serde_json::from_str(replies).map_err(|_| StorageError::InvalidDraftRecord)?;
        let Some(tab) = replies
            .tabs
            .into_iter()
            .find(|tab| !tab.answer.trim().is_empty())
        else {
            return Ok(None);
        };
        reply = tab.answer;
        &reply
    } else {
        return Ok(None);
    };
    let mut normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Ok(None);
    }
    if let Some((end, _)) = normalized.char_indices().nth(160) {
        normalized.truncate(end);
        normalized.push('…');
    }
    Ok(Some(normalized))
}

#[cfg(test)]
mod tests;
