//! Передаёт выбранный фильтр и счётчик упоминаний на фоновый поток Core.

use uuid::Uuid;
use workspace_core::messenger::activity::{
    ActivityMessageError, ActivityMessageFilter, ActivityMessagePage,
};

use super::RuntimeClient;
use crate::gpui_client::contracts::AccountSession;

impl RuntimeClient {
    /// Запрашивает страницу с серверным маркером у Core для переданного входа.
    pub(in crate::gpui_client) async fn activity_messages(
        &self,
        session: AccountSession,
        filter: ActivityMessageFilter,
        marker: Option<Uuid>,
    ) -> Result<ActivityMessagePage, ActivityMessageError> {
        self.request(move |services| async move {
            services
                .messenger
                .activity_messages(
                    &services.sync,
                    &session.owner,
                    session.generation,
                    filter,
                    marker,
                )
                .await
        })
        .await
        .map_err(|_| ActivityMessageError::Unavailable)?
    }

    /// Считает непрочитанные упоминания только для переданного входа.
    pub(in crate::gpui_client) async fn unread_activity_mentions(
        &self,
        session: AccountSession,
    ) -> Result<u64, ActivityMessageError> {
        self.request(move |services| async move {
            services
                .messenger
                .unread_activity_mentions(&services.sync, &session.owner, session.generation)
                .await
        })
        .await
        .map_err(|_| ActivityMessageError::Unavailable)?
    }
}
