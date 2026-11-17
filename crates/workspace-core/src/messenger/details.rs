//! Загружает проверенные сведения о собеседнике или точной теме разговора.

use thiserror::Error;

use crate::{
    auth::{Auth, AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    directory::UserProfile,
    runtime::SourceError,
    storage::HistoryScope,
};

use super::MessengerSource;

/// Карточка, доступная только после проверки требуемого маршрута на сервере.
#[derive(Debug, PartialEq, Eq)]
pub enum ConversationDetails {
    Peer(UserProfile), // Профиль точного собеседника в обычной личке.
    Topic {
        channel: String,             // Название родительского канала.
        name: String,                // Название выбранной темы.
        description: Option<String>, // Непустое описание канала.
        done: Option<bool>,          // Признак завершения темы, если сервер его вернул.
    },
}

/// Различает потерю чата, отсутствие профиля и временный сбой.
#[derive(Debug, Error)]
pub enum DetailsError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход изменился или не выдал токен.
    #[error("conversation access lost")]
    AccessLost, // Входу больше недоступен стрим или тема.
    #[error("peer profile unavailable")]
    ProfileUnavailable, // Сервер не показывает профиль проверенного собеседника.
    #[error("conversation details unsupported")]
    Unsupported, // Адрес ведёт в тип разговора вне этого среза.
    #[error(transparent)]
    Source(#[from] SourceError), // Ошибка запроса или несовпадение данных.
}

impl MessengerSource {
    /// Запрашивает карточку с токеном текущего поколения и заново проверяет вход перед выдачей.
    pub async fn conversation_details(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<ConversationDetails, DetailsError> {
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self.rest.conversation_details(owner, &token, scope).await;
        auth.with_active_generation(owner, generation, || ())?;
        result
    }
}
