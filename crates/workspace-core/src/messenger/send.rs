//! Отправляет подготовленный снимок сообщения одной попыткой и возвращает результат для сверки.

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    storage::{HistoryMessage, PreparedSend},
    sync::Sync,
};

use super::{MessengerSource, history::OpenHistoryError};

/// Причина, по которой нельзя подготовить открытый текст для разговора.
#[derive(Debug, Error)]
pub enum DirectSendError {
    #[error("the conversation or topic is unavailable")]
    AccessLost, // Личка или тема недоступна владельцу.
    #[error("the server did not allow unencrypted messages")]
    EncryptionUnavailable, // Состояние шифрования неизвестно или шифрование включено.
    #[error("the server did not provide a valid send topic")]
    TopicUnavailable, // Личный стрим не объявил пригодную тему для записи.
    #[error("the topic is closed for sending")]
    TopicClosed, // Тема канала завершена.
    #[error(transparent)]
    Auth(#[from] AuthError), // Поколение сессии изменилось или не удалось получить токен.
    #[error(transparent)]
    Source(#[from] SourceError), // Запрос к мессенджеру не завершился корректно.
}

impl From<OpenHistoryError> for DirectSendError {
    /// Переиспользует результат проверки доступа, общей с открытием истории.
    fn from(error: OpenHistoryError) -> Self {
        match error {
            OpenHistoryError::AccessLost => Self::AccessLost,
            OpenHistoryError::Auth(error) => Self::Auth(error),
            OpenHistoryError::Source(error) => Self::Source(error),
            _ => Self::Source(SourceError::InvalidResponse),
        }
    }
}

/// Результат попытки, который определяет дальнейшую сверку очереди.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SendError {
    #[error("invalid prepared message or server origin")]
    InvalidInput, // Запрос не был отправлен: адрес или идентификаторы неверны.
    #[error("server rejected the request with HTTP {0}")]
    Rejected(u16), // Получен однозначный отказ API с указанным HTTP-статусом.
    #[error("delivery outcome is unknown")]
    Unknown, // Сервер мог принять запрос; требуется сверка, а не автоматический повтор.
}

impl MessengerSource {
    /// Возвращает явную тему лички из подтверждённого ответа для того же поколения.
    pub async fn resolve_direct_send(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
    ) -> Result<Uuid, DirectSendError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let topic = self.rest.direct_send_topic(owner, &token, stream_id).await;
        auth.with_active_generation(owner, generation, || ())?;
        topic
    }

    /// Подтверждает явную тему канала для открытия редактора или записи сообщения в очередь.
    pub async fn check_topic_send(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        topic_id: Uuid,
    ) -> Result<(), DirectSendError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .check_send_topic(owner, &token, stream_id, topic_id, false)
            .await;
        auth.with_active_generation(owner, generation, || ())?;
        result
    }

    /// Ищет подтверждение доставки по публичному UUID, не изменяя очередь.
    pub async fn lookup_prepared_message(
        &self,
        owner: &OwnerContext,
        token: &str,
        snapshot: &PreparedSend,
    ) -> Result<HistoryMessage, SendError> {
        self.rest.lookup_prepared(owner, token, snapshot).await
    }

    /// Отправляет снимок с переданным токеном без повторов и без изменения очереди.
    pub async fn post_prepared_message(
        &self,
        owner: &OwnerContext,
        token: &str,
        snapshot: &PreparedSend,
    ) -> Result<HistoryMessage, SendError> {
        self.rest.post_prepared(owner, token, snapshot).await
    }
}
