//! Собирает известные запросы ключей из тем защищённого чата для массовой выдачи.
//! Результат служит подсказкой для серверной проверки каждого запроса.

use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::context::OwnerContext;

use super::{Storage, StorageError, response_uuid};

const KEY_REQUEST_PAGE_SIZE: usize = 64;

/// Сохранённое размещение запроса и публичное имя устройства для панели чата.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyRequestHint {
    pub stream_id: Uuid,     // Стрим, в котором опубликован запрос.
    pub topic_id: Uuid,      // Точная тема размещения.
    pub key_id: Uuid,        // Какое поколение запрашивает устройство.
    pub request_id: Uuid,    // Публичный UUID для серверной проверки.
    pub device_id: String,   // Непрозрачный идентификатор устройства получателя.
    pub device_name: String, // Публичное имя из служебной строки.
}

/// Непрозрачная позиция после последней просмотренной строки того же стрима.
#[derive(Clone, Debug)]
pub struct KeyRequestCursor {
    stream_id: Uuid,    // Стрим исходной страницы.
    created_at: String, // Нормализованное время или пустая строка без времени.
    message_id: String, // Точное поле SQLite для совпадающего времени.
}

/// Ограниченный пакет сохранённых кандидатов для авторитетной проверки.
pub struct KeyRequestPage {
    pub requests: Vec<KeyRequestHint>, // Кандидаты по всем сохранённым темам стрима.
    pub next: Option<KeyRequestCursor>, // `None` означает конец просмотра.
}

/// Индексирует только служебные строки, участвующие в поиске и сверке запроса.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS messenger_key_requests_by_stream ON sync_messages (
            account_id, server, organization_id, project_id, user_id, stream_id,
            COALESCE(created_at, ''), message_id
         ) WHERE json_extract(key_service_payload, '$.kind') = 'key_request';
         CREATE INDEX IF NOT EXISTS messenger_key_responses_by_request ON sync_messages (
            account_id, server, organization_id, project_id, user_id, stream_id, topic_id,
            json_extract(key_service_payload, '$.request_message_uuid'),
            json_extract(key_service_payload, '$.key_uuid'),
            json_extract(key_service_payload, '$.recipient_device_id')
         ) WHERE json_extract(key_service_payload, '$.kind') IN ('key_grant', 'key_reject');",
    )?;
    Ok(())
}

impl Storage {
    /// Читает одну страницу неудалённых запросов. Локальный ответ не подтверждает разрешение.
    pub(crate) fn key_service_requests(
        &self,
        owner: &OwnerContext,
        stream_id: Uuid,
        after: Option<KeyRequestCursor>,
    ) -> Result<KeyRequestPage, StorageError> {
        let mut page = KeyRequestPage {
            requests: Vec::new(),
            next: None,
        };
        if stream_id.is_nil()
            || after
                .as_ref()
                .is_some_and(|cursor| cursor.stream_id != stream_id)
        {
            return Ok(page);
        }
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut query = connection.prepare(
            "SELECT m.topic_id, m.message_id, COALESCE(m.created_at, ''),
                   json_extract(m.key_service_payload, '$.key_uuid'),
                   json_extract(m.key_service_payload, '$.device_id'),
                   json_extract(m.key_service_payload, '$.device_name')
             FROM sync_messages AS m LEFT JOIN history_changes AS changed
               USING (account_id, server, organization_id, project_id, user_id, message_id)
             WHERE m.account_id = ?1 AND m.server = ?2 AND m.organization_id = ?3
               AND m.project_id = ?4 AND m.user_id = ?5 AND m.stream_id = ?6
               AND json_extract(m.key_service_payload, '$.kind') = 'key_request'
               AND m.topic_id IS NOT NULL AND COALESCE(changed.deleted, 0) = 0
               AND (COALESCE(m.created_at, ''), m.message_id) > (?7, ?8)
             ORDER BY COALESCE(m.created_at, ''), m.message_id LIMIT ?9",
        )?;
        let (after_time, after_id) = after.map_or_else(
            || (String::new(), String::new()),
            |cursor| (cursor.created_at, cursor.message_id),
        );
        let mut rows = query.query(params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            stream_id.to_string(),
            after_time,
            after_id,
            KEY_REQUEST_PAGE_SIZE as i64 + 1,
        ])?;
        let mut cursor = None;
        for index in 0..=KEY_REQUEST_PAGE_SIZE {
            let Some(row) = rows.next()? else {
                break;
            };
            if index == KEY_REQUEST_PAGE_SIZE {
                page.next = cursor;
                break;
            }
            let message_id: String = row.get(1)?;
            let request_id = response_uuid(&message_id);
            cursor = Some(KeyRequestCursor {
                stream_id,
                created_at: row.get(2)?,
                message_id,
            });
            let (Some(topic_id), Some(request_id), Some(key_id)) = (
                response_uuid(&row.get::<_, String>(0)?),
                request_id,
                response_uuid(&row.get::<_, String>(3)?),
            ) else {
                continue;
            };
            let device_id: String = row.get(4)?;
            if device_id.is_empty() {
                continue;
            }
            page.requests.push(KeyRequestHint {
                stream_id,
                topic_id,
                key_id,
                request_id,
                device_id,
                device_name: row.get(5)?,
            });
        }
        Ok(page)
    }

    /// Находит адреса ответов для точного запроса. Их автора и тело проверит KeyExchange.
    pub(crate) fn key_reply_candidates(
        &self,
        owner: &OwnerContext,
        request: &KeyRequestHint,
        after: Option<Uuid>,
    ) -> Result<Vec<Uuid>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut query = connection.prepare(
            "SELECT reply.message_id FROM sync_messages AS reply LEFT JOIN history_changes AS changed
               USING (account_id, server, organization_id, project_id, user_id, message_id)
             WHERE reply.account_id = ?1 AND reply.server = ?2 AND reply.organization_id = ?3
               AND reply.project_id = ?4 AND reply.user_id = ?5 AND reply.stream_id = ?6
               AND reply.topic_id = ?7 AND COALESCE(changed.deleted, 0) = 0
               AND json_extract(reply.key_service_payload, '$.kind') IN ('key_grant', 'key_reject')
               AND json_extract(reply.key_service_payload, '$.request_message_uuid') = ?8
               AND json_extract(reply.key_service_payload, '$.key_uuid') = ?9
               AND json_extract(reply.key_service_payload, '$.recipient_device_id') = ?10
               AND reply.message_id > ?11
             ORDER BY reply.message_id LIMIT ?12",
        )?;
        Ok(query
            .query_map(
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    request.stream_id.to_string(),
                    request.topic_id.to_string(),
                    request.request_id.to_string(),
                    request.key_id.to_string(),
                    request.device_id,
                    after.map_or_else(String::new, |id| id.to_string()),
                    KEY_REQUEST_PAGE_SIZE as i64,
                ],
                |row| row.get::<_, String>(0),
            )?
            .map(|id| id?.parse().map_err(|_| rusqlite::Error::InvalidQuery))
            .collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests;
