//! Выполняет действия над подтверждённой строкой истории.
//! Проверяет сессию перед запросом и связывает серверную квитанцию с этим входом.

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{Auth, AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    storage::HistoryScope,
};

use super::{MessengerSource, history::OpenHistoryError};

/// Сообщение и его фактическое размещение в открытой области истории.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageTarget {
    pub scope: HistoryScope, // Стрим лички или точная тема канала.
    pub topic_id: Uuid,      // Фактическая тема сообщения, в том числе в личке.
    pub message_id: Uuid,    // Публичный UUID размещения из истории.
}

impl MessageTarget {
    /// Проверяет, что указаны все UUID и что тема входит в область истории.
    pub(super) fn is_valid(self) -> bool {
        !self.scope.stream_id.is_nil()
            && !self.topic_id.is_nil()
            && !self.message_id.is_nil()
            && self
                .scope
                .topic_id
                .is_none_or(|topic| topic == self.topic_id)
    }
}

/// Квитанция, которую можно прочитать только для того же владельца и поколения входа.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationTicket {
    owner: OwnerContext, // Проект и пользователь, отправившие исходную команду.
    generation: SessionGeneration, // Поколение входа на момент запроса.
    id: Uuid,            // UUID запроса, назначенный сервером.
}

impl MutationTicket {
    /// Связывает UUID из ответа сервера с исходным контекстом команды.
    pub(super) fn new(owner: &OwnerContext, generation: SessionGeneration, id: Uuid) -> Self {
        Self {
            owner: owner.clone(),
            generation,
            id,
        }
    }
}

/// Ответ сервера на один запрос изменения сообщения.
#[derive(Debug, Eq, PartialEq)]
pub enum MutationReceipt {
    Applied,                  // Сервер закончил действие синхронно.
    Accepted(MutationTicket), // Сервер поставил действие в очередь и выдал квитанцию.
}

/// Состояние команды по квитанции. Публикация событий может идти позже.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationStatus {
    Pending,  // Команда ожидает применения.
    Applied,  // Изменение зафиксировано сервером.
    Rejected, // Сервер завершил команду отказом.
}

/// Ошибка вызова удаления. Неопределённый результат требует чтения истории или события.
#[derive(Debug, Error)]
pub enum DeleteError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Сессия изменилась до действия или во время ожидания ответа.
    #[error("invalid message address")]
    InvalidTarget, // UUID или область запроса не соответствуют адресу сообщения.
    #[error("the conversation is unavailable")]
    AccessLost, // Тема или стрим скрыты для исходного аккаунта.
    #[error("the server denied access to the message")]
    Forbidden, // GET или DELETE сообщения вернул 403.
    #[error("the server no longer exposes the message at this address")]
    MessageUnavailable, // Сообщение скрыто или перемещено. Это не подтверждает удаление.
    #[error("the message belongs to another author")]
    NotAuthor, // Автор подтверждённой сервером строки отличается от текущего пользователя.
    #[error("the message is outside the supported native unencrypted conversation")]
    Unsupported, // Тип строки или разговора пока не поддерживается.
    #[error("the server rejected deletion with HTTP {0}")]
    Rejected(u16), // Сервер явно отклонил команду.
    #[error("deletion outcome is unknown")]
    Unknown, // Сервер мог принять действие. Повтор без сверки может создать новую команду.
    #[error(transparent)]
    Source(#[from] SourceError), // Проверка цели не завершилась. DELETE ещё не выполнялся.
}

impl From<OpenHistoryError> for DeleteError {
    /// Сохраняет причину отказа при проверке стрима и темы.
    fn from(error: OpenHistoryError) -> Self {
        match error {
            OpenHistoryError::AccessLost => Self::AccessLost,
            OpenHistoryError::Source(error) => Self::Source(error),
            _ => Self::Source(SourceError::InvalidResponse),
        }
    }
}

/// Причина, по которой не удалось перечитать серверную квитанцию.
#[derive(Debug, Error)]
pub enum MutationStatusError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход из квитанции недействителен.
    #[error("the server no longer exposes the mutation receipt")]
    Unavailable, // Квитанция скрыта или истекла. Итог команды по этому ответу неизвестен.
    #[error(transparent)]
    Source(#[from] SourceError), // Сбой запроса или ответ для другого UUID или владельца.
}

impl MessengerSource {
    /// Проверяет исходную строку и делает один DELETE с токеном её автора.
    /// Само состояние истории обновляется из серверного журнала и чтения.
    pub async fn delete_message(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
        target: MessageTarget,
    ) -> Result<MutationReceipt, DeleteError> {
        if !target.is_valid() {
            return Err(DeleteError::InvalidTarget);
        }
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let checked = self
            .rest
            .check_deletable_message(owner, &token, target)
            .await;
        auth.with_active_generation(owner, generation, || ())?;
        checked?;
        let result = self
            .rest
            .delete_message_once(owner, &token, target.message_id)
            .await;
        auth.with_active_generation(owner, generation, || ())?;
        Ok(match result? {
            Some(id) => MutationReceipt::Accepted(MutationTicket::new(owner, generation, id)),
            None => MutationReceipt::Applied,
        })
    }

    /// Читает серверный статус команды, сохраняя исходного владельца из квитанции.
    pub async fn mutation_status(
        &self,
        auth: &Auth,
        ticket: &MutationTicket,
    ) -> Result<MutationStatus, MutationStatusError> {
        let token = auth
            .access_token_for_generation(&ticket.owner, Service::Messenger, ticket.generation)
            .await?;
        let result = self
            .rest
            .mutation_status(&ticket.owner, &token, ticket.id)
            .await;
        auth.with_active_generation(&ticket.owner, ticket.generation, || ())?;
        result
    }
}
