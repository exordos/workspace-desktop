//! Передаёт запрос «Избранного» с исходным поколением входа в Core.

use workspace_core::{messenger::activity::SelfChatError, storage::HistoryScope};

use super::RuntimeClient;
use crate::gpui_client::contracts::AccountSession;

/// Безопасная причина ошибки для экрана «Избранное».
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::gpui_client) enum SelfChatRequestError {
    InactiveSession, // Выбранный вход уже завершён или изменился.
    Unavailable,     // Core не смог получить подтверждённый разговор.
    Worker,          // Фоновый исполнитель не принял запрос.
}

impl RuntimeClient {
    /// Подтверждает чат с собой для конкретного поколения входа.
    pub(in crate::gpui_client) async fn ensure_self_chat(
        &self,
        session: AccountSession,
    ) -> Result<HistoryScope, SelfChatRequestError> {
        self.request(move |services| async move {
            services
                .messenger
                .ensure_self_chat(&services.sync, &session.owner, session.generation)
                .await
                .map_err(|error| match error {
                    SelfChatError::InactiveSession => SelfChatRequestError::InactiveSession,
                    SelfChatError::Unavailable => SelfChatRequestError::Unavailable,
                })
        })
        .await
        .map_err(|_| SelfChatRequestError::Worker)?
    }
}
