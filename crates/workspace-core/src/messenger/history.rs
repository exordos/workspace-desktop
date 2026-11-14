//! Загружает последнюю страницу истории и открывает окно вокруг сообщения из уведомления.

use std::collections::HashSet;

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    notification_target::NotificationTarget,
    runtime::SourceError,
    storage::{
        HistoryEdge, HistoryPageEdges, HistoryPageOutcome, HistoryPageTarget, HistoryScope,
        HistoryWindow,
    },
    sync::{Sync, SyncError},
};

use super::MessengerSource;

/// Причина, по которой мессенджер не смог вернуть окно.
#[derive(Debug, Error)]
pub enum OpenHistoryError {
    #[error("the conversation is unavailable")]
    AccessLost, // Сервер больше не показывает историю в этой области владельцу.
    #[error("the message is unavailable in this conversation")]
    MessageMissing, // Сервер не вернул цель или она больше не в этом разговоре.
    #[error("the notification address is invalid")]
    InvalidTarget, // В подтверждённом адресе ожидались UUID разговора и сообщения.
    #[error("history changed during the request")]
    StaleWindow, // Локальная история изменилась во время чтения. Запрос можно повторить.
    #[error(transparent)]
    Auth(#[from] AuthError), // Сессия завершилась, сменилась или не смогла выдать токен.
    #[error(transparent)]
    Sync(#[from] SyncError), // Сбой чтения или записи локального окна.
    #[error(transparent)]
    Source(#[from] SourceError), // Сбой REST или неожиданный ответ.
}

impl MessengerSource {
    /// Загружает хвост истории. До вызова нужно подтвердить доступ владельца к области.
    pub async fn load_latest_history(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<HistoryWindow, OpenHistoryError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let guard = sync
            .prepare_history_page(owner, generation, scope, HistoryPageTarget::New)?
            .ok_or(OpenHistoryError::StaleWindow)?;
        let fetched = self.rest.latest_page(owner, &token, scope).await;
        auth.with_active_generation(owner, generation, || ())?;
        let page = fetched?;
        // Запрос без маркера начинается с конца истории. Маркер ответа ведёт к старым сообщениям.
        let outcome = sync.save_history_page(
            guard,
            &page.messages,
            HistoryPageEdges::New {
                older: page.edge,
                newer: HistoryEdge::End,
            },
        )?;
        let HistoryPageOutcome::Saved(id) = outcome else {
            return Err(OpenHistoryError::StaleWindow);
        };
        sync.history_windows(owner, generation, scope)?
            .into_iter()
            .find(|window| window.id == id)
            .ok_or(OpenHistoryError::StaleWindow)
    }

    /// Открывает окно для ранее подтверждённого адреса и проверяет доступ перед чтением кэша.
    pub async fn open_direct_history(
        &self,
        sync: &Sync,
        target: &NotificationTarget,
        generation: SessionGeneration,
    ) -> Result<HistoryWindow, OpenHistoryError> {
        let owner = &target.context;
        let stream_id = target
            .conversation_id
            .parse()
            .map_err(|_| OpenHistoryError::InvalidTarget)?;
        let anchor_id: Uuid = target
            .message_id
            .parse()
            .map_err(|_| OpenHistoryError::InvalidTarget)?;
        let scope = HistoryScope {
            stream_id,
            topic_id: None,
        };
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let guard = sync
            .prepare_history_page(owner, generation, scope, HistoryPageTarget::New)?
            .ok_or(OpenHistoryError::StaleWindow)?;
        let verified = async {
            self.rest
                .check_direct_access(owner, &token, stream_id)
                .await?;
            self.rest
                .direct_anchor(owner, &token, stream_id, anchor_id)
                .await
        }
        .await;
        auth.with_active_generation(owner, generation, || ())?;
        let anchor = verified?;

        if let Some(window) = sync
            .history_windows(owner, generation, scope)?
            .into_iter()
            .find(|window| {
                !window.needs_refresh
                    && window
                        .messages
                        .iter()
                        .any(|message| message.id == anchor_id)
            })
        {
            return Ok(window);
        }

        let fetched = self
            .rest
            .direct_pages(owner, &token, stream_id, anchor_id)
            .await;
        // После сетевого запроса сначала проверяем сессию, даже если сервер вернул ошибку.
        auth.with_active_generation(owner, generation, || ())?;
        let (older, newer) = fetched?;
        let mut seen = HashSet::from([anchor.id]);
        let mut messages = vec![anchor];
        messages.extend(
            older
                .messages
                .into_iter()
                .chain(newer.messages)
                .filter(|message| seen.insert(message.id)),
        );
        let outcome = sync.save_history_page(
            guard,
            &messages,
            HistoryPageEdges::New {
                older: older.edge,
                newer: newer.edge,
            },
        )?;
        let HistoryPageOutcome::Saved(id) = outcome else {
            return Err(OpenHistoryError::StaleWindow);
        };
        sync.history_windows(owner, generation, scope)?
            .into_iter()
            .find(|window| {
                window.id == id
                    && !window.needs_refresh
                    && window
                        .messages
                        .iter()
                        .any(|message| message.id == anchor_id)
            })
            .ok_or(OpenHistoryError::StaleWindow)
    }
}
