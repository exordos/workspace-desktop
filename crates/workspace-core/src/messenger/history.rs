//! Открывает историю, догружает её края и находит окно для уведомления.

use std::{
    collections::HashSet,
    sync::{Arc, Weak},
};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    notification_target::NotificationTarget,
    runtime::SourceError,
    storage::{
        HistoryDirection, HistoryEdge, HistoryMessage, HistoryPageEdges, HistoryPageOutcome,
        HistoryPageTarget, HistoryScope, HistoryWindow,
        media::{
            GalleryCounts, LinkHistoryCursor, LinkHistoryPage, MediaCache, MediaHistoryCursor,
            MediaHistoryPage, MediaKind,
        },
    },
    sync::{HistoryRequest, Sync, SyncError},
};

use super::{
    MessengerSource,
    files::{MediaFile, OpenMediaError},
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

/// Допуск к вложениям области, которую Core уже подтвердил при открытии истории.
#[derive(Clone)]
pub struct MediaAccess {
    authority: Weak<()>,            // Идентичность Sync, который выдал допуск.
    pub(super) owner: OwnerContext, // Владелец истории.
    pub(super) generation: SessionGeneration, // Подтверждённый вход владельца.
    pub(super) scope: HistoryScope, // Точная область подтверждённого открытия.
    revision: i64,                  // Версия отзыва области и сброса журнала перед запросом.
}

impl MediaAccess {
    /// Захватывает условия до запроса доступа. Допуск выдаётся только после успешного запроса.
    fn capture(
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Option<Self> {
        sync.media_cache()?;
        Some(Self {
            authority: Arc::downgrade(sync.media_authority()),
            owner: owner.clone(),
            generation,
            scope,
            revision: sync.storage().media_access_revision(owner, scope).ok()?,
        })
    }

    /// Проверяет профиль и область под блокировкой текущей сессии.
    pub(super) fn verify(&self, sync: &Sync) -> Result<(), OpenMediaError> {
        if !self
            .authority
            .ptr_eq(&Arc::downgrade(sync.media_authority()))
            || sync
                .storage()
                .media_access_revision(&self.owner, self.scope)?
                != self.revision
        {
            return Err(OpenMediaError::Unavailable);
        }
        Ok(())
    }

    /// Дополнительно проверяет, что галерея опирается на текущее поколение журнала.
    pub(super) fn verify_gallery(
        &self,
        sync: &Sync,
        expected: Option<i64>,
    ) -> Result<i64, OpenMediaError> {
        self.verify(sync)?;
        let current = sync.storage().media_history_generation(&self.owner)?;
        if expected.is_some_and(|generation| generation != current) {
            return Err(OpenMediaError::Unavailable);
        }
        Ok(current)
    }

    /// Строит весь стрим или оставляет точную область, подтверждённую при открытии истории.
    pub(super) fn gallery_scope(&self, whole_stream: bool) -> HistoryScope {
        HistoryScope {
            stream_id: self.scope.stream_id,
            topic_id: self.scope.topic_id.filter(|_| !whole_stream),
        }
    }

    /// Проверяет допуск, готовит первую страницу старого профиля и читает локальный индекс.
    fn list_gallery<T>(
        &self,
        sync: &Sync,
        whole_stream: bool,
        first: bool,
        expected: Option<i64>,
        read: impl FnOnce(&MediaCache, HistoryScope, i64) -> Result<T, OpenMediaError>,
    ) -> Result<T, OpenMediaError> {
        let cache = sync.media_cache().ok_or(OpenMediaError::Unconfigured)?;
        let scope = self.gallery_scope(whole_stream);
        sync.auth()
            .with_active_generation(&self.owner, self.generation, || {
                let expected = self.verify_gallery(sync, expected)?;
                if first
                    && !sync
                        .storage()
                        .index_media_history(&self.owner, scope, expected, None)?
                {
                    return Err(OpenMediaError::Unavailable);
                }
                read(cache, scope, expected)
            })?
    }
}

/// Подтверждённое окно вокруг сообщения и допуск для открытия его вложений.
pub struct AnchorHistoryOpening {
    pub window: HistoryWindow,             // Окно с целевым сообщением.
    pub media_access: Option<MediaAccess>, // Настроенный для этого профиля допуск.
}

/// Подтверждённое открытие: сохранённый хвост доступен до сетевого обновления.
pub struct LatestHistoryOpening<'a> {
    source: &'a MessengerSource,   // Источник для запроса последней страницы.
    sync: &'a Sync,                // Хранилище и проверка текущей сессии.
    owner: &'a OwnerContext,       // Владелец подтверждённого разговора.
    generation: SessionGeneration, // Поколение входа при проверке доступа.
    scope: HistoryScope,           // Точная область, подтверждённая сервером.
    pub cached: Option<HistoryWindow>, // Пригодный хвост или None, если его нет.
    media_access: Option<MediaAccess>, // Подтверждённый допуск к вложениям.
    gallery_generation: Option<i64>, // Поколение истории при подтверждении допуска.
}

impl LatestHistoryOpening<'_> {
    /// Возвращает допуск, который можно передать следующей фоновой команде.
    pub fn media_access(&self) -> Option<MediaAccess> {
        self.media_access.clone()
    }

    /// Открывает вложение из этой темы или личного разговора.
    pub async fn open_media(
        &self,
        message_id: Uuid,
        file_id: Uuid,
    ) -> Result<MediaFile, OpenMediaError> {
        let access = self
            .media_access
            .as_ref()
            .ok_or(OpenMediaError::Unconfigured)?;
        self.source
            .open_media(self.sync, access, message_id, file_id)
            .await
    }

    /// Открывает результат списка вложений из известной истории того же стрима.
    pub async fn open_stream_media(
        &self,
        message_id: Uuid,
        file_id: Uuid,
    ) -> Result<MediaFile, OpenMediaError> {
        let access = self
            .media_access
            .as_ref()
            .ok_or(OpenMediaError::Unconfigured)?;
        let generation = self
            .gallery_generation
            .ok_or(OpenMediaError::Unconfigured)?;
        self.source
            .open_cached_media(
                self.sync,
                access,
                crate::storage::media::MediaSource {
                    scope: access.gallery_scope(true),
                    message_id,
                },
                file_id,
                Some(generation),
            )
            .await
    }

    /// Возвращает страницу вложений уже известной истории темы или её стрима.
    pub fn list_media(
        &self,
        whole_stream: bool,
        kind: Option<MediaKind>,
        after: Option<MediaHistoryCursor>,
        limit: usize,
    ) -> Result<MediaHistoryPage, OpenMediaError> {
        let access = self
            .media_access
            .as_ref()
            .ok_or(OpenMediaError::Unconfigured)?;
        let generation = self
            .gallery_generation
            .ok_or(OpenMediaError::Unconfigured)?;
        access.list_gallery(
            self.sync,
            whole_stream,
            after.is_none(),
            Some(generation),
            |cache, scope, generation| {
                Ok(cache.list_history(&access.owner, scope, generation, kind, after, limit)?)
            },
        )
    }

    /// Запрашивает последнюю страницу; ошибка не удаляет ранее прочитанное окно.
    pub async fn refresh(&self) -> Result<HistoryWindow, OpenHistoryError> {
        self.source
            .load_latest_history(self.sync, self.owner, self.generation, self.scope)
            .await
    }
}

impl MessengerSource {
    /// Считает известные позиции в области, с которой откроется галерея.
    pub fn gallery_counts(
        &self,
        sync: &Sync,
        access: &MediaAccess,
    ) -> Result<GalleryCounts, OpenMediaError> {
        access.list_gallery(sync, false, true, None, |cache, scope, _| {
            Ok(cache.gallery_counts(&access.owner, scope)?)
        })
    }

    /// Возвращает известные вложения по допуску подтверждённой темы или её стрима.
    pub fn list_media(
        &self,
        sync: &Sync,
        access: &MediaAccess,
        whole_stream: bool,
        kind: Option<MediaKind>,
        after: Option<MediaHistoryCursor>,
        limit: usize,
    ) -> Result<MediaHistoryPage, OpenMediaError> {
        access.list_gallery(
            sync,
            whole_stream,
            after.is_none(),
            None,
            |cache, scope, generation| {
                Ok(cache.list_history(&access.owner, scope, generation, kind, after, limit)?)
            },
        )
    }

    /// Возвращает известные веб-ссылки из открытых сообщений подтверждённого разговора.
    pub fn list_links(
        &self,
        sync: &Sync,
        access: &MediaAccess,
        whole_stream: bool,
        after: Option<LinkHistoryCursor>,
        limit: usize,
    ) -> Result<LinkHistoryPage, OpenMediaError> {
        access.list_gallery(
            sync,
            whole_stream,
            after.is_none(),
            None,
            |cache, scope, generation| {
                Ok(cache.list_links(&access.owner, scope, generation, after, limit)?)
            },
        )
    }

    /// Проверяет доступ и возвращает сохранённый хвост до запроса последней страницы.
    pub async fn open_latest_history<'a>(
        &'a self,
        sync: &'a Sync,
        owner: &'a OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<LatestHistoryOpening<'a>, OpenHistoryError> {
        let media_access = MediaAccess::capture(sync, owner, generation, scope);
        let gallery_generation = media_access
            .as_ref()
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
            media_access,
            gallery_generation,
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
        let mut guard = sync
            .prepare_history_page(owner, generation, scope, HistoryPageTarget::New)?
            .ok_or(OpenHistoryError::StaleWindow)?;
        let fetched = self.rest.descending_page(owner, &token, scope, None).await;
        auth.with_active_generation(owner, generation, || ())?;
        let page = fetched?;
        self.verify_legacy_encrypted_messages(
            sync,
            owner,
            generation,
            &token,
            &mut guard,
            &page.messages,
        )
        .await?;
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
        let Some(mut guard) = sync.prepare_history_page(owner, generation, scope, target)? else {
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
        self.verify_legacy_encrypted_messages(
            sync,
            owner,
            generation,
            &token,
            &mut guard,
            &page.messages,
        )
        .await?;
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
        Ok(self
            .open_direct_history_with_media(sync, target, generation)
            .await?
            .window)
    }

    /// Открывает личку вокруг сообщения и возвращает подтверждённый допуск к вложениям.
    pub async fn open_direct_history_with_media(
        &self,
        sync: &Sync,
        target: &NotificationTarget,
        generation: SessionGeneration,
    ) -> Result<AnchorHistoryOpening, OpenHistoryError> {
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
        Ok(self
            .open_topic_history_with_media(sync, owner, generation, stream_id, topic_id, anchor_id)
            .await?
            .window)
    }

    /// Открывает тему вокруг сообщения и возвращает подтверждённый допуск к вложениям.
    pub async fn open_topic_history_with_media(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        topic_id: Uuid,
        anchor_id: Uuid,
    ) -> Result<AnchorHistoryOpening, OpenHistoryError> {
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
    ) -> Result<AnchorHistoryOpening, OpenHistoryError> {
        let media_access = MediaAccess::capture(sync, owner, generation, scope);
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let mut guard = sync
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
        let mut anchor = verified?;

        if let Some(window) = sync
            .history_windows(owner, generation, scope)?
            .into_iter()
            .find(|window| {
                !window.needs_refresh
                    && window.messages.iter().any(|message| {
                        message.id == anchor.id
                            && message.stream_id == anchor.stream_id
                            && message.topic_id == anchor.topic_id
                            && message.body == anchor.body
                    })
            })
        {
            return Ok(AnchorHistoryOpening {
                window,
                media_access,
            });
        }

        self.rest
            .hydrate_own_reactions(owner, &token, std::slice::from_mut(&mut anchor))
            .await;
        auth.with_active_generation(owner, generation, || ())?;
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
        self.verify_legacy_encrypted_messages(
            sync, owner, generation, &token, &mut guard, &messages,
        )
        .await?;
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
        let window = sync
            .history_windows(owner, generation, scope)?
            .into_iter()
            .find(|window| {
                window.id == id
                    && !window.needs_refresh
                    && window
                        .messages
                        .iter()
                        .any(|message| message.id == anchor_id)
            })
            .ok_or(OpenHistoryError::StaleWindow)?;
        Ok(AnchorHistoryOpening {
            window,
            media_access,
        })
    }

    /// Перепроверяет старые удаления точным запросом владельца и запоминает совпавшие тела.
    async fn verify_legacy_encrypted_messages(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        token: &str,
        request: &mut HistoryRequest,
        messages: &[HistoryMessage],
    ) -> Result<(), OpenHistoryError> {
        let pending = sync
            .storage()
            .legacy_encrypted_messages(owner, messages)
            .map_err(SyncError::from)?;
        for message in pending {
            sync.auth()
                .with_active_generation(owner, generation, || ())?;
            let scope = HistoryScope {
                stream_id: message.stream_id,
                topic_id: Some(message.topic_id),
            };
            let checked = self
                .rest
                .history_anchor(owner, token, scope, message.id)
                .await;
            sync.auth()
                .with_active_generation(owner, generation, || ())?;
            match checked {
                Ok(confirmed) if confirmed.same_content(message) => {
                    request.verify_legacy_encrypted_message(message.id);
                }
                Ok(_) | Err(OpenHistoryError::MessageMissing) => {
                    return Err(OpenHistoryError::StaleWindow);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
