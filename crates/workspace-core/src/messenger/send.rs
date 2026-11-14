//! Отправляет подготовленный снимок сообщения одной попыткой и возвращает результат для сверки.

use thiserror::Error;

use crate::{
    context::OwnerContext,
    storage::{HistoryMessage, PreparedSend},
};

use super::MessengerSource;

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
