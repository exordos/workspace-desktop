//! Типы служебных тел E2EE, которые история хранит вместе с размещением сообщения.

mod requests;

use std::collections::HashMap;

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::context::OwnerContext;

use super::{HistoryScope, Storage, StorageError};

pub use requests::{KeyRequestCursor, KeyRequestHint, KeyRequestPage};

const KEY_RESPONSE_PAGE_SIZE: usize = 64;

/// Общие поля выдачи и отказа. Идентификаторы устройств приходят как строки.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct KeyResponseTarget {
    pub key_uuid: Uuid,             // Поколение ключа, о котором попросило устройство.
    pub request_message_uuid: Uuid, // Публичный UUID размещённого сервером запроса.
    pub sender_device_id: String,   // Устройство, с которого отправлен ответ.
    pub recipient_device_id: String, // Устройство, к которому относится ответ.
}

/// Тела служебных сообщений REST и realtime. UUID стрима, темы и автора приходят отдельно.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum KeyServicePayload {
    #[serde(rename = "key_request")]
    Request {
        key_uuid: Uuid,      // Какой ключ нужен устройству.
        device_id: String,   // Идентификатор устройства получателя.
        device_name: String, // Имя устройства для участника, который принимает решение.
        public_key: String,  // Публичный X25519-ключ запроса в стандартном Base64.
    },
    #[serde(rename = "key_grant")]
    Grant {
        #[serde(flatten)]
        target: KeyResponseTarget, // Точное размещение запроса и нужное устройство.
        content: String, // Зашифрованный конверт выдачи в стандартном Base64.
    },
    #[serde(rename = "key_reject")]
    Reject(KeyResponseTarget), // Запрос и устройство, которым отказано.
    #[serde(rename = "key_announced")]
    Announced {
        key_uuid: Uuid, // Новое поколение ключа, объявленное владельцем чата.
    },
}

impl KeyServicePayload {
    /// Возвращает поколение ключа для группировки запросов и ответов.
    pub fn key_id(&self) -> Uuid {
        match self {
            Self::Request { key_uuid, .. } | Self::Announced { key_uuid } => *key_uuid,
            Self::Grant { target, .. } | Self::Reject(target) => target.key_uuid,
        }
    }
}

/// Сохранённое размещение ответа. Для применения нужно заново проверить запись на сервере.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyResponseHint {
    pub scope: HistoryScope,        // Исходный стрим и точная тема ответа.
    pub key_id: Uuid,               // Поколение, указанное в сохранённом сообщении.
    pub response_id: Uuid,          // Публичный UUID для авторитетного GET.
    pub request_id: Uuid,           // Публичный UUID запроса для проверки ожидающего секрета.
    pub canonical_id: Option<Uuid>, // Исходный UUID ответа, если его вернул сервер.
}

/// Позиция поиска по времени и публичному UUID. Следующая страница начинает чтение за ней.
#[derive(Clone, Debug)]
pub struct KeyResponseCursor {
    created_at: String, // Нормализованное время или пустая строка у записи без времени.
    response_id: String, // Точное поле SQLite, которое разрешает совпадения времени.
}

/// Ограниченный пакет сохранённых ответов для устройства.
pub struct KeyResponsePage {
    pub responses: Vec<KeyResponseHint>, // Только адреса и идентификаторы для сверки.
    pub next: Option<KeyResponseCursor>, // `None` означает, что прочитаны все доступные страницы.
}

/// Индексирует запросы и ответы для устройств, не включая пользовательские сообщения.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS messenger_key_responses_by_recipient ON sync_messages (
            account_id, server, organization_id, project_id, user_id,
            json_extract(key_service_payload, '$.recipient_device_id'), COALESCE(created_at, ''), message_id
         ) WHERE json_extract(key_service_payload, '$.kind') IN ('key_grant', 'key_reject');",
    )?;
    requests::initialize(connection)
}

impl Storage {
    /// Читает даты первых объявлений из загруженной истории подтверждённого стрима.
    pub(crate) fn key_announcement_dates(
        &self,
        owner: &OwnerContext,
        stream_id: Uuid,
    ) -> Result<HashMap<Uuid, u64>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut query = connection.prepare(
            "SELECT json_extract(m.key_service_payload, '$.key_uuid') AS key_id,
                    CAST(ROUND(unixepoch(MIN(m.created_at), 'subsec') * 1000) AS INTEGER)
             FROM sync_messages AS m LEFT JOIN history_changes AS changed
               USING (account_id, server, organization_id, project_id, user_id, message_id)
             WHERE m.account_id = ?1 AND m.server = ?2 AND m.organization_id = ?3
               AND m.project_id = ?4 AND m.user_id = ?5 AND m.stream_id = ?6
               AND json_extract(m.key_service_payload, '$.kind') = 'key_announced'
               AND m.created_at IS NOT NULL AND COALESCE(changed.deleted, 0) = 0
             GROUP BY key_id",
        )?;
        let mut rows = query.query(params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            stream_id.to_string(),
        ])?;
        let mut dates = HashMap::new();
        while let Some(row) = rows.next()? {
            let id: Option<String> = row.get(0)?;
            let milliseconds: Option<i64> = row.get(1)?;
            if let (Some(id), Some(milliseconds)) = (
                id.as_deref().and_then(response_uuid),
                milliseconds.and_then(|time| u64::try_from(time).ok()),
            ) {
                dates.insert(id, milliseconds);
            }
        }
        Ok(dates)
    }

    /// Читает ответы нужного устройства во всех сохранённых темах этого владельца.
    pub(crate) fn key_service_responses(
        &self,
        owner: &OwnerContext,
        recipient: Uuid,
        after: Option<KeyResponseCursor>,
    ) -> Result<KeyResponsePage, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut query = connection.prepare(
            "SELECT m.stream_id, m.topic_id, m.message_id, m.key_service_canonical_id,
                   json_extract(m.key_service_payload, '$.key_uuid'),
                   json_extract(m.key_service_payload, '$.request_message_uuid'),
                   COALESCE(m.created_at, '')
             FROM sync_messages AS m LEFT JOIN history_changes AS h
               USING (account_id, server, organization_id, project_id, user_id, message_id)
             WHERE m.account_id = ?1 AND m.server = ?2 AND m.organization_id = ?3
               AND m.project_id = ?4 AND m.user_id = ?5
               AND json_extract(m.key_service_payload, '$.recipient_device_id') = ?6
               AND json_extract(m.key_service_payload, '$.kind') IN ('key_grant', 'key_reject')
               AND m.stream_id IS NOT NULL AND m.topic_id IS NOT NULL
               AND COALESCE(h.deleted, 0) = 0
               AND (COALESCE(m.created_at, ''), m.message_id) > (?7, ?8)
             ORDER BY COALESCE(m.created_at, ''), m.message_id LIMIT ?9",
        )?;
        let (after_time, after_id) = after.map_or_else(
            || (String::new(), String::new()),
            |cursor| (cursor.created_at, cursor.response_id),
        );
        let mut rows = query.query(params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            recipient.to_string(),
            after_time,
            after_id,
            KEY_RESPONSE_PAGE_SIZE as i64 + 1,
        ])?;
        let mut responses = Vec::new();
        let mut next = None;
        for index in 0..=KEY_RESPONSE_PAGE_SIZE {
            let Some(row) = rows.next()? else {
                break;
            };
            if index == KEY_RESPONSE_PAGE_SIZE {
                return Ok(KeyResponsePage { responses, next });
            }
            let public_id: String = row.get(2)?;
            next = Some(KeyResponseCursor {
                created_at: row.get(6)?,
                response_id: public_id.clone(),
            });
            let (
                Some(stream_id),
                Some(topic_id),
                Some(response_id),
                Some(key_id),
                Some(request_id),
            ) = (
                response_uuid(&row.get::<_, String>(0)?),
                response_uuid(&row.get::<_, String>(1)?),
                response_uuid(&public_id),
                response_uuid(&row.get::<_, String>(4)?),
                response_uuid(&row.get::<_, String>(5)?),
            )
            else {
                continue;
            };
            responses.push(KeyResponseHint {
                scope: HistoryScope {
                    stream_id,
                    topic_id: Some(topic_id),
                },
                response_id,
                canonical_id: row
                    .get::<_, Option<String>>(3)?
                    .as_deref()
                    .and_then(response_uuid),
                key_id,
                request_id,
            });
        }
        Ok(KeyResponsePage {
            responses,
            next: None,
        })
    }
}

/// Отклоняет неверный UUID из сохранённой записи истории.
fn response_uuid(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value).ok().filter(|id| !id.is_nil())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context::AccountId,
        storage::{
            HistoryEdge, HistoryMessage, HistoryMetadata, HistoryPageEdges, HistoryPageTarget,
            HistoryTime, MessageBody,
        },
    };

    /// Использует первое сохранённое объявление, ограниченное входом и стримом.
    #[test]
    fn key_announcement_dates_use_first_loaded_announcement_in_the_stream() {
        let storage = Storage::in_memory().unwrap();
        let owner = OwnerContext {
            account_id: AccountId("keys".into()),
            server: "https://workspace.test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let [stream, topic, key, author] = [(); 4].map(|()| Uuid::new_v4());
        let rows =
            ["1970-01-01T00:00:02.321Z", "1970-01-01T00:00:04Z"].map(|time| HistoryMessage {
                id: Uuid::new_v4(),
                stream_id: stream,
                topic_id: topic,
                body: MessageBody::KeyService {
                    canonical_id: None,
                    payload: KeyServicePayload::Announced { key_uuid: key },
                },
                metadata: HistoryMetadata {
                    author_id: Some(author),
                    created_at: HistoryTime::parse(time).unwrap(),
                    updated_at: None,
                },
            });
        let scope = HistoryScope {
            stream_id: stream,
            topic_id: Some(topic),
        };
        let guard = storage
            .prepare_history_page(&owner, scope, HistoryPageTarget::New)
            .unwrap()
            .unwrap();
        storage
            .save_history_page(
                &owner,
                guard,
                &rows,
                HistoryPageEdges::New {
                    older: HistoryEdge::End,
                    newer: HistoryEdge::End,
                },
            )
            .unwrap();
        assert_eq!(
            storage.key_announcement_dates(&owner, stream).unwrap(),
            HashMap::from([(key, 2321)]),
        );
        assert!(
            storage
                .key_announcement_dates(&owner, Uuid::new_v4())
                .unwrap()
                .is_empty()
        );
        let other_owner = OwnerContext {
            account_id: AccountId("other".into()),
            ..owner
        };
        assert!(
            storage
                .key_announcement_dates(&other_owner, stream)
                .unwrap()
                .is_empty()
        );
    }

    /// Продолжает после совпадающего времени, даже если на первой странице есть неверный ответ.
    #[test]
    fn key_response_pages_are_bounded_and_device_scoped() {
        let storage = Storage::in_memory().unwrap();
        let owner = OwnerContext {
            account_id: AccountId("keys".into()),
            server: "https://workspace.test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let [stream, topic, recipient, other_device, key, request, author] =
            [(); 7].map(|()| Uuid::new_v4());
        let scope = HistoryScope {
            stream_id: stream,
            topic_id: Some(topic),
        };
        let rows = (1..=68)
            .map(|id| HistoryMessage {
                id: Uuid::from_u128(id),
                stream_id: stream,
                topic_id: topic,
                body: MessageBody::KeyService {
                    canonical_id: if id == 67 {
                        Some(Uuid::from_u128(101))
                    } else {
                        None
                    },
                    payload: KeyServicePayload::Reject(KeyResponseTarget {
                        key_uuid: key,
                        request_message_uuid: if id == 32 { Uuid::nil() } else { request },
                        sender_device_id: "other".into(),
                        recipient_device_id: if id == 68 { other_device } else { recipient }
                            .to_string(),
                    }),
                },
                metadata: HistoryMetadata {
                    author_id: Some(author),
                    created_at: HistoryTime::parse("2026-01-01T10:00:00Z").unwrap(),
                    updated_at: None,
                },
            })
            .collect::<Vec<_>>();
        let guard = storage
            .prepare_history_page(&owner, scope, HistoryPageTarget::New)
            .unwrap()
            .unwrap();
        storage
            .save_history_page(
                &owner,
                guard,
                &rows,
                HistoryPageEdges::New {
                    older: HistoryEdge::End,
                    newer: HistoryEdge::End,
                },
            )
            .unwrap();
        let first = storage
            .key_service_responses(&owner, recipient, None)
            .unwrap();
        assert_eq!(first.responses.len(), KEY_RESPONSE_PAGE_SIZE - 1);
        assert_eq!(
            first.responses.last().unwrap().response_id,
            Uuid::from_u128(64)
        );
        let second = storage
            .key_service_responses(&owner, recipient, first.next)
            .unwrap();
        assert!(second.next.is_none());
        assert_eq!(
            second
                .responses
                .iter()
                .map(|hint| hint.response_id.as_u128())
                .collect::<Vec<_>>(),
            [65, 66, 67]
        );
        assert_eq!(second.responses[2].canonical_id, Some(Uuid::from_u128(101)));
        assert_eq!(
            storage
                .key_service_responses(&owner, other_device, None)
                .unwrap()
                .responses
                .len(),
            1
        );
        let other_owner = OwnerContext {
            account_id: AccountId("other".into()),
            ..owner
        };
        assert!(
            storage
                .key_service_responses(&other_owner, recipient, None)
                .unwrap()
                .responses
                .is_empty()
        );
    }
}
