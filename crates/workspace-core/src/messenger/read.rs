//! Передаёт серверу явно выбранную границу прочтения одного разговора.

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    storage::HistoryScope,
    sync::Sync,
};

use super::MessengerSource;

/// Результат вызова отметки прочтения.
#[derive(Debug, Error)]
pub enum ReadError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход завершился, изменился или не выдал токен.
    #[error("invalid server origin")]
    InvalidInput, // Адрес организации нельзя использовать для запроса.
    #[error("server returned HTTP {0}")]
    Http(u16), // Сервер ответил ошибкой с указанным статусом.
    #[error("read outcome is unknown")]
    Unknown, // Ответ потерян или не подтверждает ожидаемую область и сообщение.
}

impl MessengerSource {
    /// Отправляет одну границу для ранее подтверждённой области текущей сессии.
    pub async fn read_up_to(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        message_id: Uuid,
    ) -> Result<(), ReadError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self.rest.read_up_to(owner, &token, scope, message_id).await;
        // Даже успешный ответ не относится к новому входу того же аккаунта.
        auth.with_active_generation(owner, generation, || ())?;
        result
    }
}
