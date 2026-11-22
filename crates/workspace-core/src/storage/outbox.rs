//! Хранит подготовленные сообщения и закрепляет первую попытку для последующей сверки.

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use uuid::Uuid;

use super::{
    DraftAccess, DraftVersion, HistoryMessage, HistoryScope, Message, MessagePlacement, Storage,
    StorageError, drafts, history,
};
use crate::context::OwnerContext;

/// Неизменный снимок подготовленного текстового сообщения.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedSend {
    pub canonical_id: Uuid, // UUID для тела запроса.
    pub public_id: Uuid,    // UUID размещения для сверки с историей.
    pub stream_id: Uuid,    // Исходный стрим.
    pub topic_id: Uuid,     // Исходная тема.
    pub markdown: String,   // Неизменное содержимое payload вида markdown.
}

/// Состояние отправки, пока для неё нет подтверждённой строки истории.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryStatus {
    Waiting,               // Снимок сохранён, первый запрос ещё не разрешён.
    Sending,               // Первая попытка закреплена и выполняется.
    Unknown,               // Запрос мог быть принят. Ищем подтверждение.
    Blocked(SendBlock),    // Первая попытка остановлена до сетевого запроса.
    Rejected(Option<u16>), // Отправка остановлена. Some содержит код отказа сервера.
}

/// Причина, по которой сохранённый снимок нельзя отправить.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendBlock {
    AccessLost,            // Личка или её исходная тема стали недоступны.
    EncryptionUnavailable, // Нет явного разрешения отправлять открытый текст.
}

/// Строка очереди вместе с состоянием, которое показывает окно.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboxEntry {
    pub snapshot: PreparedSend, // Текст и исходные идентификаторы сообщения.
    pub status: DeliveryStatus, // Последнее зафиксированное состояние отправки.
}

/// Создаёт отдельную таблицу очереди, которая не зависит от кэша источника.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS messenger_outbox (
            account_id TEXT NOT NULL,
            server TEXT NOT NULL,
            organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            canonical_id BLOB NOT NULL,
            public_id BLOB NOT NULL,
            stream_id BLOB NOT NULL,
            topic_id BLOB NOT NULL,
            markdown TEXT NOT NULL,
            state TEXT NOT NULL CHECK (state = 'pending'),
            first_attempt_claimed INTEGER NOT NULL DEFAULT 0 CHECK (first_attempt_claimed IN (0, 1)),
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, canonical_id)
        );",
    )?;
    let has_claim: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('messenger_outbox')
                        WHERE name = 'first_attempt_claimed')",
        [],
        |row| row.get(0),
    )?;
    if !has_claim {
        // Все записи прежней схемы ещё ждали отправки. Миграция оставляет их в очереди.
        connection.execute_batch(
            "ALTER TABLE messenger_outbox ADD COLUMN first_attempt_claimed
             INTEGER NOT NULL DEFAULT 0 CHECK (first_attempt_claimed IN (0, 1));",
        )?;
    }
    let has_status: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('messenger_outbox')
                        WHERE name = 'final_status')",
        [],
        |row| row.get(0),
    )?;
    if !has_status {
        connection.execute_batch(
            "ALTER TABLE messenger_outbox ADD COLUMN final_status INTEGER NOT NULL DEFAULT 0
             CHECK (final_status IN (0, 1, 2, 3, 4) OR final_status BETWEEN 400 AND 499);
             UPDATE messenger_outbox SET final_status = 1 WHERE first_attempt_claimed = 1;",
        )?;
    }
    Ok(())
}

/// Читает неизменный снимок из результата запроса к очереди.
fn read_prepared_send(row: &Row<'_>) -> rusqlite::Result<PreparedSend> {
    Ok(PreparedSend {
        canonical_id: Uuid::from_bytes(row.get(0)?),
        public_id: Uuid::from_bytes(row.get(1)?),
        stream_id: Uuid::from_bytes(row.get(2)?),
        topic_id: Uuid::from_bytes(row.get(3)?),
        markdown: row.get(4)?,
    })
}

/// Закрывает только закреплённую отправку, совпавшую с автором и исходным адресом.
pub(super) fn confirm(
    connection: &Connection,
    owner: &OwnerContext,
    id: &str,
    stream: &str,
    topic: &str,
    author: Option<Uuid>,
) -> Result<bool, StorageError> {
    if author.is_none() || author != owner.user_id.parse().ok() {
        return Ok(false);
    }
    let (Ok(id), Ok(stream), Ok(topic)) = (
        id.parse::<Uuid>(),
        stream.parse::<Uuid>(),
        topic.parse::<Uuid>(),
    ) else {
        return Ok(false);
    };
    Ok(connection.execute(
        "DELETE FROM messenger_outbox WHERE account_id = ?1 AND server = ?2
         AND organization_id = ?3 AND project_id = ?4 AND user_id = ?5
         AND public_id = ?6 AND stream_id = ?7 AND topic_id = ?8 AND first_attempt_claimed = 1",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            id.as_bytes(),
            stream.as_bytes(),
            topic.as_bytes()
        ],
    )? != 0)
}

/// Останавливает ожидающий POST вместе с записью события об отзыве.
pub(super) fn reject_revoked(
    connection: &Connection,
    owner: &OwnerContext,
    stream: &str,
    topic: Option<&str>,
) -> Result<(), StorageError> {
    let Ok(stream) = stream.parse::<Uuid>() else {
        return Ok(());
    };
    let topic = match topic {
        Some(topic) => {
            let Ok(topic) = topic.parse::<Uuid>() else {
                return Ok(());
            };
            Some(*topic.as_bytes())
        }
        None => None,
    };
    connection.execute(
        "UPDATE messenger_outbox SET final_status = 3
         WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
           AND project_id = ?4 AND user_id = ?5
           AND stream_id = ?6 AND (?7 IS NULL OR topic_id = ?7)
           AND first_attempt_claimed = 0 AND final_status = 0",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            stream.as_bytes(),
            topic
        ],
    )?;
    Ok(())
}

impl Storage {
    /// Сохраняет подтверждение HTTP и закрывает подходящую запись одной транзакцией.
    pub(crate) fn confirm_http_send(
        &self,
        owner: &OwnerContext,
        message: &HistoryMessage,
    ) -> Result<bool, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = message.id.to_string();
        let stream = message.stream_id.to_string();
        let topic = message.topic_id.to_string();
        let confirmed = confirm(
            &transaction,
            owner,
            &id,
            &stream,
            &topic,
            message.metadata.author_id,
        )?;
        if confirmed && !history::has_message_revision(&transaction, owner, &id)? {
            let inserted = transaction.execute(
                "INSERT OR IGNORE INTO sync_messages (account_id, server, organization_id, project_id, user_id,
                 message_id, text, stream_id, topic_id, author_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id, owner.user_id,
                    id, message.text, stream, topic, message.metadata.author_id.map(|id| id.to_string()),
                    message.metadata.created_at.as_str(), message.metadata.updated_at.as_ref().map(|time| time.as_str())],
            )? != 0;
            if inserted {
                history::record_creation(
                    &transaction,
                    owner,
                    &Message {
                        id,
                        text: message.text.clone(),
                    },
                    Some(&MessagePlacement {
                        stream_id: stream,
                        topic_id: topic,
                    }),
                    Some(&message.metadata),
                )?;
                history::project_media_message(&transaction, owner, &message.id.to_string())?;
            }
        }
        transaction.commit()?;
        Ok(confirmed)
    }

    /// Сохраняет подготовленный текст. Возвращает false, если такой ключ уже занят.
    pub fn save_prepared_send(
        &self,
        owner: &OwnerContext,
        message: &PreparedSend,
    ) -> Result<bool, StorageError> {
        self.save_prepared_send_with_draft(owner, message, None)
    }

    /// Передаёт ответственность за снимок поля очереди в той же транзакции.
    pub(crate) fn save_prepared_send_with_draft(
        &self,
        owner: &OwnerContext,
        message: &PreparedSend,
        draft: Option<(&DraftAccess, DraftVersion)>,
    ) -> Result<bool, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Первый вариант остаётся в очереди даже при повторе с другим содержимым.
        let inserted = transaction.execute(
            "INSERT INTO messenger_outbox (
                account_id, server, organization_id, project_id, user_id,
                canonical_id, public_id, stream_id, topic_id, markdown, state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending')
             ON CONFLICT (account_id, server, organization_id, project_id, user_id, canonical_id)
             DO NOTHING",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                message.canonical_id.as_bytes(),
                message.public_id.as_bytes(),
                message.stream_id.as_bytes(),
                message.topic_id.as_bytes(),
                message.markdown
            ],
        )?;
        if inserted == 1
            && let Some((access, version)) = draft
        {
            if !access.belongs_to(owner)
                || access.scope().stream_id != message.stream_id
                || access.scope().topic_id != message.topic_id
            {
                return Err(StorageError::StaleDraftAccess);
            }
            drafts::consume(&transaction, access, version)?;
        }
        transaction.commit()?;
        Ok(inserted == 1)
    }

    /// Возвращает только ожидающие записи указанного владельца, в порядке добавления.
    pub fn pending_sends(&self, owner: &OwnerContext) -> Result<Vec<PreparedSend>, StorageError> {
        self.sends_by_claim(owner, false)
    }

    /// Возвращает закреплённые записи для сверки с историей после сбоя или перезапуска.
    pub fn claimed_sends(&self, owner: &OwnerContext) -> Result<Vec<PreparedSend>, StorageError> {
        self.sends_by_claim(owner, true)
    }

    /// Закрепляет первую попытку. Только выигравший вызов получает снимок для отправки.
    pub fn claim_first_send(
        &self,
        owner: &OwnerContext,
        canonical_id: Uuid,
    ) -> Result<Option<PreparedSend>, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        // SQLite проверяет право на попытку и сохраняет его в одном запросе.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let snapshot = transaction
            .query_row(
                "UPDATE messenger_outbox SET first_attempt_claimed = 1
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND canonical_id = ?6
               AND first_attempt_claimed = 0 AND final_status = 0
             RETURNING canonical_id, public_id, stream_id, topic_id, markdown",
                params![
                    owner.account_id.0,
                    owner.server,
                    owner.organization_id,
                    owner.project_id,
                    owner.user_id,
                    canonical_id.as_bytes(),
                ],
                read_prepared_send,
            )
            .optional()?;
        transaction.commit()?;
        Ok(snapshot)
    }

    /// Читает все статусы разговора или рабочие записи по всем чатам владельца.
    pub(crate) fn outbox(
        &self,
        owner: &OwnerContext,
        scope: Option<HistoryScope>,
    ) -> Result<Vec<OutboxEntry>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT canonical_id, public_id, stream_id, topic_id, markdown,
                    first_attempt_claimed, final_status
             FROM messenger_outbox
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5
               AND (?6 IS NULL OR stream_id = ?6)
               AND (?7 IS NULL OR topic_id = ?7)
               AND (?6 IS NOT NULL OR final_status IN (0, 1))
             ORDER BY rowid",
        )?;
        let stream = scope.map(|scope| *scope.stream_id.as_bytes());
        let topic = scope.and_then(|scope| scope.topic_id.map(|topic| *topic.as_bytes()));
        let rows = statement.query_map(
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                stream,
                topic
            ],
            |row| {
                let claimed = row.get::<_, bool>(5)?;
                let status = match (claimed, row.get::<_, u16>(6)?) {
                    (false, 0) => DeliveryStatus::Waiting,
                    (true, 0) => DeliveryStatus::Sending,
                    (_, 1) => DeliveryStatus::Unknown,
                    (_, 2) => DeliveryStatus::Rejected(None),
                    (_, 3) => DeliveryStatus::Blocked(SendBlock::AccessLost),
                    (_, 4) => DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable),
                    (_, status) => DeliveryStatus::Rejected(Some(status)),
                };
                Ok(OutboxEntry {
                    snapshot: read_prepared_send(row)?,
                    status,
                })
            },
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// После смены фоновой задачи результат незавершённой попытки нужно сверить.
    pub(crate) fn recover_interrupted_sends(
        &self,
        owner: &OwnerContext,
    ) -> Result<Vec<HistoryScope>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare(
            "UPDATE messenger_outbox SET final_status = 1
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND first_attempt_claimed = 1
               AND final_status = 0
             RETURNING stream_id, topic_id",
        )?;
        let scopes = statement.query_map(
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id
            ],
            |row| {
                Ok(HistoryScope {
                    stream_id: Uuid::from_bytes(row.get(0)?),
                    topic_id: Some(Uuid::from_bytes(row.get(1)?)),
                })
            },
        )?;
        Ok(scopes.collect::<Result<_, _>>()?)
    }

    /// Записывает исход или запрет до отправки, если запись ещё не была закрыта.
    pub(crate) fn set_send_status(
        &self,
        owner: &OwnerContext,
        canonical_id: Uuid,
        claimed: bool,
        status: DeliveryStatus,
    ) -> Result<bool, StorageError> {
        let status = match status {
            DeliveryStatus::Unknown => 1,
            DeliveryStatus::Rejected(None) => 2,
            DeliveryStatus::Blocked(SendBlock::AccessLost) => 3,
            DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable) => 4,
            DeliveryStatus::Rejected(Some(code)) => code,
            DeliveryStatus::Waiting | DeliveryStatus::Sending => return Ok(false),
        };
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(connection.execute(
            "UPDATE messenger_outbox SET final_status = ?8
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND canonical_id = ?6
               AND first_attempt_claimed = ?7 AND final_status = 0",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                canonical_id.as_bytes(),
                claimed,
                status
            ],
        )? == 1)
    }

    /// Читает записи владельца по признаку закрепления, в порядке добавления.
    fn sends_by_claim(
        &self,
        owner: &OwnerContext,
        claimed: bool,
    ) -> Result<Vec<PreparedSend>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT canonical_id, public_id, stream_id, topic_id, markdown
             FROM messenger_outbox
             WHERE account_id = ?1 AND server = ?2 AND organization_id = ?3
               AND project_id = ?4 AND user_id = ?5 AND first_attempt_claimed = ?6
               AND (final_status IN (0, 1))
             ORDER BY rowid",
        )?;
        let rows = statement.query_map(
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                claimed,
            ],
            read_prepared_send,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
