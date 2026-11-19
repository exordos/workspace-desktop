//! Открывает историю, догружает её края и находит окно для уведомления.

use std::collections::HashSet;

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    notification_target::NotificationTarget,
    runtime::SourceError,
    storage::{
        HistoryDirection, HistoryEdge, HistoryPageEdges, HistoryPageOutcome, HistoryPageTarget,
        HistoryScope, HistoryWindow,
        media::{MediaHistoryCursor, MediaHistoryPage, MediaKind},
    },
    sync::{Sync, SyncError},
};

use super::{
    MessengerSource,
    files::{MediaConversation, MediaFile, OpenMediaError},
};

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

/// Подтверждённое открытие: сохранённый хвост доступен до сетевого обновления.
pub struct LatestHistoryOpening<'a> {
    source: &'a MessengerSource,   // Источник для запроса последней страницы.
    sync: &'a Sync,                // Хранилище и проверка текущей сессии.
    owner: &'a OwnerContext,       // Владелец подтверждённого разговора.
    generation: SessionGeneration, // Поколение входа при проверке доступа.
    scope: HistoryScope,           // Точная область, подтверждённая сервером.
    pub cached: Option<HistoryWindow>, // Пригодный хвост или None, если его нет.
    media_generation: Option<i64>, // Версия истории, в которой доступ подтвердил сервер.
}

impl<'a> LatestHistoryOpening<'a> {
    /// Открывает вложение из этой темы или личного разговора.
    pub async fn open_media(
        &self,
        message_id: Uuid,
        file_id: Uuid,
    ) -> Result<MediaFile<'a>, OpenMediaError> {
        self.open_media_in_scope(self.scope, message_id, file_id)
            .await
    }

    /// Открывает результат списка вложений из известной истории того же стрима.
    pub async fn open_stream_media(
        &self,
        message_id: Uuid,
        file_id: Uuid,
    ) -> Result<MediaFile<'a>, OpenMediaError> {
        self.open_media_in_scope(self.stream_scope(), message_id, file_id)
            .await
    }

    /// Привязывает открытие вложения к доступу, подтверждённому для истории.
    async fn open_media_in_scope(
        &self,
        scope: HistoryScope,
        message_id: Uuid,
        file_id: Uuid,
    ) -> Result<MediaFile<'a>, OpenMediaError> {
        let history_generation = self.media_generation.ok_or(OpenMediaError::Unconfigured)?;
        self.source
            .open_cached_media(
                self.sync,
                MediaConversation {
                    owner: self.owner,
                    generation: self.generation,
                    scope,
                    history_generation,
                },
                message_id,
                file_id,
            )
            .await
    }

    /// Возвращает весь стрим, к которому относится подтверждённое открытие.
    fn stream_scope(&self) -> HistoryScope {
        HistoryScope {
            stream_id: self.scope.stream_id,
            topic_id: None,
        }
    }

    /// Возвращает страницу вложений уже известной истории темы или её стрима.
    pub fn list_media(
        &self,
        whole_stream: bool,
        kind: Option<MediaKind>,
        after: Option<MediaHistoryCursor>,
        limit: usize,
    ) -> Result<MediaHistoryPage, OpenMediaError> {
        let history_generation = self.media_generation.ok_or(OpenMediaError::Unconfigured)?;
        let cache = self
            .sync
            .media_cache()
            .ok_or(OpenMediaError::Unconfigured)?;
        let scope = if whole_stream {
            self.stream_scope()
        } else {
            self.scope
        };
        self.sync
            .auth()
            .with_active_generation(self.owner, self.generation, || {
                let current = if after.is_none() {
                    self.sync.storage().index_media_history(
                        self.owner,
                        scope,
                        history_generation,
                        None,
                    )?
                } else {
                    self.sync.storage().media_history_generation(self.owner)? == history_generation
                };
                if !current {
                    return Err(OpenMediaError::Unavailable);
                }
                Ok(cache.list_history(self.owner, scope, kind, after, limit)?)
            })?
    }

    /// Запрашивает последнюю страницу; ошибка не удаляет ранее прочитанное окно.
    pub async fn refresh(&self) -> Result<HistoryWindow, OpenHistoryError> {
        self.source
            .load_latest_history(self.sync, self.owner, self.generation, self.scope)
            .await
    }
}

impl MessengerSource {
    /// Проверяет доступ и возвращает сохранённый хвост до запроса последней страницы.
    pub async fn open_latest_history<'a>(
        &'a self,
        sync: &'a Sync,
        owner: &'a OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<LatestHistoryOpening<'a>, OpenHistoryError> {
        let media_generation = sync
            .media_cache()
            .and_then(|_| sync.storage().media_history_generation(owner).ok());
        let scope = self
            .verify_conversation_access(sync, owner, generation, scope)
            .await?;
        // Окна не склеиваем: выбираем последний сохранённый подтверждённый хвост.
        let cached = sync
            .history_windows(owner, generation, scope)?
            .into_iter()
            .filter(|window| !window.needs_refresh && window.newer == HistoryEdge::End)
            .max_by_key(|window| window.id);
        Ok(LatestHistoryOpening {
            source: self,
            sync,
            owner,
            generation,
            scope,
            cached,
            media_generation,
        })
    }

    /// Подтверждает область на сервере для текущей сессии без чтения сохранённой истории.
    pub async fn verify_conversation_access(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<HistoryScope, OpenHistoryError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = match scope.topic_id {
            Some(topic) => {
                self.rest
                    .check_topic_access(owner, &token, scope.stream_id, topic)
                    .await
            }
            None => {
                self.rest
                    .check_direct_access(owner, &token, scope.stream_id)
                    .await
            }
        };
        // Ответ от прежней сессии не даёт доступа к сохранённым сообщениям.
        auth.with_active_generation(owner, generation, || ())?;
        result?;
        Ok(scope)
    }

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
        let fetched = self.rest.descending_page(owner, &token, scope, None).await;
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

    /// Догружает старый край окна в области, к которой вызывающий уже подтвердил доступ.
    /// На конце истории возвращает текущее окно без запроса.
    pub async fn load_older_history(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        window_id: i64,
    ) -> Result<HistoryWindow, OpenHistoryError> {
        self.extend_history(
            sync,
            owner,
            generation,
            scope,
            window_id,
            HistoryDirection::Older,
        )
        .await
    }

    /// Догружает новый край окна после подтверждения доступа к области.
    pub async fn load_newer_history(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        window_id: i64,
    ) -> Result<HistoryWindow, OpenHistoryError> {
        self.extend_history(
            sync,
            owner,
            generation,
            scope,
            window_id,
            HistoryDirection::Newer,
        )
        .await
    }

    /// Продолжает выбранный край и отклоняет ответ после его замены.
    async fn extend_history(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        window_id: i64,
        direction: HistoryDirection,
    ) -> Result<HistoryWindow, OpenHistoryError> {
        let read_window = || {
            sync.history_windows(owner, generation, scope)?
                .into_iter()
                .find(|window| window.id == window_id && !window.needs_refresh)
                .ok_or(OpenHistoryError::StaleWindow)
        };
        let target = HistoryPageTarget::Extend {
            window_id,
            direction,
        };
        let Some(guard) = sync.prepare_history_page(owner, generation, scope, target)? else {
            let window = read_window()?;
            let edge = match direction {
                HistoryDirection::Older => &window.older,
                HistoryDirection::Newer => &window.newer,
            };
            return if *edge == HistoryEdge::End {
                Ok(window)
            } else {
                Err(OpenHistoryError::StaleWindow)
            };
        };
        let marker = guard.marker().ok_or(OpenHistoryError::StaleWindow)?;
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let fetched = match direction {
            HistoryDirection::Older => {
                self.rest
                    .descending_page(owner, &token, scope, Some(marker))
                    .await
            }
            HistoryDirection::Newer => self.rest.ascending_page(owner, &token, scope, marker).await,
        };
        auth.with_active_generation(owner, generation, || ())?;
        let page = fetched?;
        // SQLite проверит, что с начала запроса край не успела заменить другая страница.
        let outcome =
            sync.save_history_page(guard, &page.messages, HistoryPageEdges::Extend(page.edge))?;
        if outcome != HistoryPageOutcome::Saved(window_id) {
            return Err(OpenHistoryError::StaleWindow);
        }
        read_window()
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
        self.open_anchor_history(sync, owner, generation, scope, anchor_id)
            .await
    }

    /// Открывает окно известной темы вокруг сообщения после проверки доступа.
    pub async fn open_topic_history(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        topic_id: Uuid,
        anchor_id: Uuid,
    ) -> Result<HistoryWindow, OpenHistoryError> {
        let scope = HistoryScope {
            stream_id,
            topic_id: Some(topic_id),
        };
        self.open_anchor_history(sync, owner, generation, scope, anchor_id)
            .await
    }

    /// Проверяет область и цель, затем возвращает кэш или загружает соседние страницы.
    async fn open_anchor_history(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        anchor_id: Uuid,
    ) -> Result<HistoryWindow, OpenHistoryError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let guard = sync
            .prepare_history_page(owner, generation, scope, HistoryPageTarget::New)?
            .ok_or(OpenHistoryError::StaleWindow)?;
        let verified = async {
            match scope.topic_id {
                Some(topic) => {
                    self.rest
                        .check_topic_access(owner, &token, scope.stream_id, topic)
                        .await?
                }
                None => {
                    self.rest
                        .check_direct_access(owner, &token, scope.stream_id)
                        .await?
                }
            }
            self.rest
                .history_anchor(owner, &token, scope, anchor_id)
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
            .anchor_pages(owner, &token, scope, anchor_id)
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
