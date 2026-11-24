//! Связывает страницу локальных черновиков с буфером редактора и хранилищем Core.

use workspace_core::{
    messenger::activity::{ActivityDraftEntry, ServerDraftError},
    storage::{DraftScope, DraftVersion},
    sync::SyncError,
};

use super::RuntimeClient;
use crate::gpui_client::contracts::AccountSession;

/// Причина, по которой не завершилась операция на странице черновиков.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::gpui_client) enum LocalDraftError {
    InactiveSession, // Выбранный вход уже завершён или изменился.
    Unavailable,     // Не удалось прочитать или сохранить локальные данные.
    Worker,          // Фоновый исполнитель не принял запрос.
}

impl From<SyncError> for LocalDraftError {
    /// Сворачивает ошибку Core к безопасному состоянию для окна.
    fn from(error: SyncError) -> Self {
        match error {
            SyncError::Auth(_) => Self::InactiveSession,
            _ => Self::Unavailable,
        }
    }
}

impl From<ServerDraftError> for LocalDraftError {
    /// Сворачивает ошибку серверного запроса к безопасному состоянию страницы.
    fn from(error: ServerDraftError) -> Self {
        match error {
            ServerDraftError::Auth(_) => Self::InactiveSession,
            _ => Self::Unavailable,
        }
    }
}

impl RuntimeClient {
    /// Показывает и уже сохранённые черновики, и последний ввод выбранного входа.
    pub(in crate::gpui_client) async fn list_local_drafts(
        &self,
        session: AccountSession,
    ) -> Result<Vec<ActivityDraftEntry>, LocalDraftError> {
        self.request(move |services| async move {
            if !services.drafts.flush_session(&services.sync, &session) {
                return Err(LocalDraftError::Unavailable);
            }
            Ok(services
                .sync
                .list_local_drafts(&session.owner, session.generation)?
                .into_iter()
                .map(|entry| entry.map_preview(Into::into))
                .collect())
        })
        .await
        .map_err(|_| LocalDraftError::Worker)?
    }

    /// Получает серверный список отдельно, чтобы сбой сети не мешал локальным карточкам.
    pub(in crate::gpui_client) async fn list_server_drafts(
        &self,
        session: AccountSession,
    ) -> Result<Vec<ActivityDraftEntry>, LocalDraftError> {
        self.request(move |services| async move {
            services
                .messenger
                .list_server_drafts(&services.sync, &session.owner, session.generation)
                .await
                .map_err(LocalDraftError::from)
        })
        .await
        .map_err(|_| LocalDraftError::Worker)?
    }

    /// Удаляет показанную версию. Если черновик изменился, возвращает false для перечитывания списка.
    pub(in crate::gpui_client) async fn delete_local_draft(
        &self,
        session: AccountSession,
        scope: DraftScope,
        version: DraftVersion,
    ) -> Result<bool, LocalDraftError> {
        self.request(move |services| async move {
            if !services.drafts.flush_session(&services.sync, &session) {
                return Err(LocalDraftError::Unavailable);
            }
            Ok(services
                .drafts
                .delete_visible_version(&services.sync, &session, scope, version)?)
        })
        .await
        .map_err(|_| LocalDraftError::Worker)?
    }

    /// Передаёт серверу одно удаление видимой версии, сохраняя локальные правки.
    pub(in crate::gpui_client) async fn delete_server_draft(
        &self,
        session: AccountSession,
        scope: DraftScope,
        version: DraftVersion,
    ) -> Result<bool, LocalDraftError> {
        self.request(move |services| async move {
            if !services.drafts.flush_session(&services.sync, &session) {
                return Err(LocalDraftError::Unavailable);
            }
            services
                .messenger
                .delete_server_draft(
                    &services.sync,
                    &session.owner,
                    session.generation,
                    scope,
                    version,
                )
                .await
                .map_err(LocalDraftError::from)
        })
        .await
        .map_err(|_| LocalDraftError::Worker)?
    }
}
