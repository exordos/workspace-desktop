//! Открывает один и тот же чат с собой, даже если его одновременно запросили несколько экранов.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use tokio::sync::OnceCell;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    storage::HistoryScope,
    sync::Sync,
};

use super::super::MessengerSource;

/// Причина, по которой не удалось открыть «Избранное».
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SelfChatError {
    #[error("the sign-in is no longer active")]
    InactiveSession, // Аккаунт вышел либо активен другой вход того же аккаунта.
    #[error("the self chat could not be confirmed")]
    Unavailable, // Нет подтверждённых данных локально или на сервере.
}

impl From<AuthError> for SelfChatError {
    /// Отличает завершённый вход от временной ошибки при получении доступа.
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::SessionUnavailable
            | AuthError::ReauthenticationRequired
            | AuthError::LogoutPending => Self::InactiveSession,
            _ => Self::Unavailable,
        }
    }
}

/// Разделяет итог выполняющегося запроса между экранами одного входа.
type SelfChatRequest = OnceCell<Result<HistoryScope, SelfChatError>>;

/// Хранит только выполняющиеся запросы, отдельно для каждого владельца и поколения.
#[derive(Default)]
pub(in crate::messenger) struct SelfChatRequests {
    active: Mutex<HashMap<(OwnerContext, SessionGeneration), Weak<SelfChatRequest>>>, // Слабые ссылки исчезают вместе с последним получателем.
}

impl SelfChatRequests {
    /// Переиспользует живой запрос и убирает следы завершившихся запросов.
    fn request(&self, owner: &OwnerContext, generation: SessionGeneration) -> Arc<SelfChatRequest> {
        let mut active = self.active.lock().unwrap();
        active.retain(|_, request| request.strong_count() > 0);
        let request = active.entry((owner.clone(), generation)).or_default();
        if let Some(request) = request.upgrade() {
            return request;
        }
        let cell = Arc::new(SelfChatRequest::new());
        *request = Arc::downgrade(&cell);
        cell
    }
}

impl MessengerSource {
    /// Находит или создаёт чат с собой и возвращает обычную область личной истории.
    pub async fn ensure_self_chat(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<HistoryScope, SelfChatError> {
        let auth = sync.auth();
        auth.with_active_generation(owner, generation, || ())?;
        let request = self.self_chat_requests.request(owner, generation);
        let result = request
            .get_or_init(|| self.resolve_self_chat(sync, owner, generation))
            .await;
        auth.with_active_generation(owner, generation, || ())?;
        *result
    }

    /// Сначала сверяет известный чат, затем при необходимости создаёт его для себя.
    async fn resolve_self_chat(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<HistoryScope, SelfChatError> {
        let user = owner
            .user_id
            .parse::<uuid::Uuid>()
            .ok()
            .filter(|user| !user.is_nil())
            .ok_or(SelfChatError::Unavailable)?;
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let catalog =
            auth.with_active_generation(owner, generation, || sync.storage().chat_catalog(owner))?;
        let existing = catalog
            .map_err(|_| SelfChatError::Unavailable)?
            .and_then(|chats| {
                chats
                    .into_iter()
                    .find(|chat| chat.direct && chat.metadata.direct_peer_id == Some(user))
            });
        if let Some(chat) = existing {
            match self.rest.find_self_chat(owner, &token, user, chat.id).await {
                Ok(Some(scope)) => return Ok(scope),
                Ok(None) => {}
                Err(error) => {
                    log::debug!("event=messenger_self_chat_lookup_failed cause={error}");
                    return Err(SelfChatError::Unavailable);
                }
            }
        }
        auth.with_active_generation(owner, generation, || ())?;
        // Сервер возвращает тот же разговор при повторе запроса для себя в этом проекте.
        self.rest
            .create_self_chat(owner, &token, user)
            .await
            .map_err(|error| {
                log::debug!("event=messenger_self_chat_create_failed cause={error}");
                SelfChatError::Unavailable
            })
    }
}

#[cfg(test)]
mod tests;
