//! Связывает получение событий с локальной базой.
//! Проверяет сессию перед записью и публикует изменения для оболочки после записи.

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    auth::{Auth, AuthError},
    context::{OwnerContext, SessionGeneration},
    messenger::details::{SidebarCache, SidebarChange},
    notification_target::{NotificationTarget, NotificationTargetError},
    storage::{
        Cursor, DeliveryStatus, EventWrite, HistoryChange, HistoryMessage, HistoryMetadata,
        HistoryPageEdges, HistoryPageGuard, HistoryPageOutcome, HistoryPageTarget, HistoryScope,
        HistoryWindow, Message, MessagePlacement, OutboxEntry, PreparedSend, Storage, StorageError,
    },
};

pub use crate::storage::ApplyOutcome;

const MAX_MESSAGE_CHARACTERS: usize = 40_000;

/// Вычисляет UUID размещения из темы и строкового канонического UUID, как Workspace API.
fn public_send_id(topic_id: uuid::Uuid, canonical_id: uuid::Uuid) -> uuid::Uuid {
    uuid::Uuid::new_v5(&topic_id, canonical_id.to_string().as_bytes())
}

#[cfg(test)]
mod send_identity_tests {
    /// Сверяет формулу с контрольным примером Workspace UI.
    #[test]
    fn public_id_matches_workspace_reference() {
        let topic = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let canonical = "00000000-0000-4000-8000-000000000002".parse().unwrap();
        assert_eq!(
            super::public_send_id(topic, canonical).to_string(),
            "d2919e10-8c62-5b88-89e6-ed7da20fc912"
        );
    }
}

/// Изменение, которое оболочка показывает для одного аккаунта и проекта.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncUpdate {
    pub context: OwnerContext, // Аккаунт и проект, получившие изменение.
    pub generation: SessionGeneration, // Вход, для которого изменение ещё актуально.
    pub change: SyncChange,    // Новое значение счётчика или цель уведомления.
}

/// Изменения после записи в локальную базу.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncChange {
    Unread(u32),    // Готовое число непрочитанных в папке «Все чаты».
    MessengerReset, // Кэш очищен. Счётчик неизвестен до снимка с сервера.
    HistoryMessageCreated {
        scope: HistoryScope,    // Стрим и тема сохранённого сообщения.
        message_id: uuid::Uuid, // UUID новой строки в базе.
    },
    HistoryMessageChanged {
        message_id: Uuid, // UUID исправленной или удалённой строки.
    },
    HistoryAccessRevoked(HistoryRevocation), // Область, которую больше нельзя показывать.
    SidebarDetailsChanged {
        stream_id: Uuid, // Стрим, чья открытая панель должна получить обновление.
    },
    OutboxChanged {
        scope: HistoryScope, // Исходная тема, очередь которой нужно перечитать.
    },
    IncomingDirectMessage {
        conversation_id: String, // Идентификатор личного разговора на сервере.
        message_id: String,      // Идентификатор нового сообщения.
    },
}

/// Идентификатор области истории, удалённой по событию мессенджера.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryRevocation {
    Stream(Uuid), // UUID стрима, доступ к которому отозван.
    Topic {
        stream_id: Uuid, // Стрим, включая его окно без фильтра темы.
        topic_id: Uuid,  // Удалённая или отозванная тема.
    },
}

/// Изменение, которое источник передаёт для сохранения.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    MessageCreated {
        cursor: Cursor,                      // Позиция, которую сохраняем вместе с сообщением.
        message: Message,                    // Новое сообщение для локальной базы.
        placement: Option<MessagePlacement>, // Стрим и тема из снимка.
        metadata: Option<HistoryMetadata>,   // Автор и время, если снимок пригоден для истории.
    },
    IncomingDirectMessage {
        cursor: Cursor,          // Позиция нового события после готовности WebSocket.
        message: Message,        // Входящее непрочитанное сообщение для локальной базы.
        conversation_id: String, // Проверенный идентификатор личного разговора.
        placement: Option<MessagePlacement>, // Стрим и тема из снимка.
        metadata: Option<HistoryMetadata>, // Автор и время, если снимок пригоден для истории.
    },
    HistoryChanged {
        cursor: Cursor,        // Позиция изменения в журнале.
        change: HistoryChange, // Правка, удаление или отзыв доступа.
    },
    SidebarChanged {
        cursor: Cursor,        // Позиция в общем журнале Workspace.
        change: SidebarChange, // Изменение профиля, стрима, темы или состава.
    },
    UnreadChanged {
        cursor: Cursor, // Позиция события обновления папки в журнале.
        count: u32,     // Новый счётчик папки «Все чаты».
    },
    UnreadBaseline {
        cursor: Cursor, // Позиция сервера перед чтением счётчика. Курсор сообщений не меняется.
        count: u32,     // Счётчик, загруженный отдельным запросом при подключении.
    },
    SourceReady(Cursor), // WebSocket готов, но отдельный снимок счётчика недоступен.
    Checkpoint(Cursor),  // Новая позиция чтения без сообщения.
}

impl Event {
    /// Возвращает позицию, до которой нужно продвинуть чтение после записи.
    pub fn cursor(&self) -> &Cursor {
        match self {
            Self::MessageCreated { cursor, .. }
            | Self::IncomingDirectMessage { cursor, .. }
            | Self::HistoryChanged { cursor, .. }
            | Self::SidebarChanged { cursor, .. }
            | Self::UnreadChanged { cursor, .. }
            | Self::UnreadBaseline { cursor, .. }
            | Self::SourceReady(cursor)
            | Self::Checkpoint(cursor) => cursor,
        }
    }

    /// Возвращает сообщение, если оно есть в событии.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::MessageCreated { message, .. } | Self::IncomingDirectMessage { message, .. } => {
                Some(message)
            }
            _ => None,
        }
    }
}

/// Разрешение записать страницу, запрошенную в действующей сессии.
#[derive(Debug)]
pub struct HistoryRequest {
    owner: OwnerContext,           // Владелец, для которого запросили страницу.
    generation: SessionGeneration, // Поколение сессии на момент запроса.
    guard: HistoryPageGuard,       // Версия кэша и края окна перед запросом.
}

impl HistoryRequest {
    /// Возвращает маркер, если страница продолжает существующее окно.
    pub fn marker(&self) -> Option<&str> {
        self.guard.marker()
    }
}

/// Причина, по которой событие не удалось обработать.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Ошибка проверки действующей сессии.
    #[error(transparent)]
    Storage(#[from] StorageError), // Ошибка локального хранилища.
}

/// Причина отказа подготовки снимка отправки.
#[derive(Debug, Error)]
pub enum PrepareSendError {
    #[error(transparent)]
    Sync(#[from] SyncError), // Сессия недействительна или база недоступна.
    #[error("the generated message identifier is already in use")]
    IdCollision, // Случайный UUID уже занят: исходная запись осталась без изменений.
    #[error("a message must contain non-whitespace text up to 40000 Unicode code points")]
    InvalidText, // Пустой текст или превышение лимита Workspace.
}

/// Проверяет право сессии на запись и передаёт события в хранилище.
pub struct Sync {
    pub(crate) sidebar_cache: SidebarCache, // Временные снимки панелей на время сессии.
    auth: Auth,            // Авторизация для проверки владельца и поколения сессии.
    storage: Arc<Storage>, // Общая локальная база сообщений, позиций и счётчиков.
    updates: broadcast::Sender<SyncUpdate>, // Канал изменений. Оформление окна получает их после записи.
}

impl Sync {
    /// Создаёт синхронизацию с указанной авторизацией и базой.
    pub fn new(auth: Auth, storage: Arc<Storage>) -> Self {
        Self {
            auth,
            storage,
            sidebar_cache: SidebarCache::default(),
            updates: broadcast::channel(128).0,
        }
    }

    /// Даёт фоновой задаче доступ к авторизации и состояниям сессий.
    pub(crate) fn auth(&self) -> &Auth {
        &self.auth
    }

    /// Возвращает последнюю записанную позицию источника для владельца.
    pub fn cursor(&self, owner: &OwnerContext, source: &str) -> Result<Option<Cursor>, SyncError> {
        Ok(self.storage.cursor(owner, source)?)
    }

    /// Возвращает сохранённый счётчик. `None` означает, что сервер его ещё не прислал.
    pub fn unread(&self, owner: &OwnerContext) -> Result<Option<u32>, SyncError> {
        Ok(self.storage.unread(owner)?)
    }

    /// Сбрасывает данные источника, пока поколение сессии имеет право на запись.
    pub(crate) fn reset_source(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        source: &str,
    ) -> Result<(), SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            self.storage.reset_source(owner, source)?;
            if source == crate::messenger::SOURCE_ID {
                self.sidebar_cache
                    .invalidate(owner, generation, None, false);
                self.publish(owner, generation, SyncChange::MessengerReset);
            }
            Ok(())
        })?
    }

    /// Фиксирует условия записи до сетевого запроса. Доступ к стриму или теме проверяет вызывающий модуль.
    pub fn prepare_history_page(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        target: HistoryPageTarget,
    ) -> Result<Option<HistoryRequest>, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            Ok(self
                .storage
                .prepare_history_page(owner, scope, target)?
                .map(|guard| HistoryRequest {
                    owner: owner.clone(),
                    generation,
                    guard,
                }))
        })?
    }

    /// Применяет страницу той же сессии, которая начала запрос, не двигая курсор событий.
    pub fn save_history_page(
        &self,
        request: HistoryRequest,
        messages: &[HistoryMessage],
        edges: HistoryPageEdges,
    ) -> Result<HistoryPageOutcome, SyncError> {
        self.auth
            .with_active_generation(&request.owner, request.generation, || {
                Ok(self.storage.save_history_page(
                    &request.owner,
                    request.guard,
                    messages,
                    edges,
                )?)
            })?
    }

    /// Читает окна активной сессии. Вызывающий модуль заранее подтверждает доступ к области.
    pub fn history_windows(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<Vec<HistoryWindow>, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            Ok(self.storage.history_windows(owner, scope)?)
        })?
    }

    /// Перечитывает подтверждённое окно того же входа без запроса к серверу.
    pub fn refresh_history_window(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
        id: i64,
    ) -> Result<Option<HistoryWindow>, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            Ok(self
                .storage
                .history_window(owner, scope, id)?
                .filter(|window| !window.needs_refresh))
        })?
    }

    /// Принимает ответ отправки только для действующего входа, не изменяя курсор событий.
    pub fn confirm_http_send(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        message: &HistoryMessage,
    ) -> Result<bool, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            let confirmed = self.storage.confirm_http_send(owner, message)?;
            if confirmed {
                let scope = HistoryScope {
                    stream_id: message.stream_id,
                    topic_id: Some(message.topic_id),
                };
                self.publish(
                    owner,
                    generation,
                    SyncChange::HistoryMessageCreated {
                        scope,
                        message_id: message.id,
                    },
                );
                self.publish(owner, generation, SyncChange::OutboxChanged { scope });
            }
            Ok(confirmed)
        })?
    }

    /// Читает статус ожидающих сообщений в области истории действующей сессии.
    pub fn outbox(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<Vec<OutboxEntry>, SyncError> {
        self.read_outbox(owner, generation, Some(scope))
    }

    /// Читает очередь указанного входа для фоновой обработки или экрана.
    pub(crate) fn read_outbox(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: Option<HistoryScope>,
    ) -> Result<Vec<OutboxEntry>, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            Ok(self.storage.outbox(owner, scope)?)
        })?
    }

    /// Отмечает незавершённые запросы после запуска фоновой задачи.
    pub(crate) fn recover_sends(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<(), SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            for scope in self.storage.recover_interrupted_sends(owner)? {
                self.publish(owner, generation, SyncChange::OutboxChanged { scope });
            }
            Ok(())
        })?
    }

    /// Разрешает первый POST только после записи захвата в действующей сессии.
    pub(crate) fn claim_send(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        canonical_id: Uuid,
    ) -> Result<Option<PreparedSend>, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            let claimed = self.storage.claim_first_send(owner, canonical_id)?;
            if let Some(snapshot) = &claimed {
                self.publish_send(owner, generation, snapshot);
            }
            Ok(claimed)
        })?
    }

    /// Фиксирует итог активной попытки или предварительный отказ для новой записи.
    pub(crate) fn set_send_status(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        snapshot: &PreparedSend,
        claimed: bool,
        status: DeliveryStatus,
    ) -> Result<(), SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            if self
                .storage
                .set_send_status(owner, snapshot.canonical_id, claimed, status)?
            {
                self.publish_send(owner, generation, snapshot);
            }
            Ok(())
        })?
    }

    /// Создаёт снимок отправки и возвращает его только после записи для действующего входа.
    pub fn prepare_send(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: uuid::Uuid,
        topic_id: uuid::Uuid,
        markdown: String,
    ) -> Result<PreparedSend, PrepareSendError> {
        if markdown.trim().is_empty() || markdown.chars().count() > MAX_MESSAGE_CHARACTERS {
            return Err(PrepareSendError::InvalidText);
        }
        self.auth
            .with_active_generation(owner, generation, || {
                let canonical_id = uuid::Uuid::new_v4();
                let message = PreparedSend {
                    canonical_id,
                    public_id: public_send_id(topic_id, canonical_id),
                    stream_id,
                    topic_id,
                    markdown,
                };
                if !self
                    .storage
                    .save_prepared_send(owner, &message)
                    .map_err(SyncError::from)?
                {
                    return Err(PrepareSendError::IdCollision);
                }
                self.publish_send(owner, generation, &message);
                Ok(message)
            })
            .map_err(SyncError::from)?
    }

    /// Подписывает оболочку на изменения. После пропуска событий счётчик можно перечитать.
    pub fn subscribe(&self) -> broadcast::Receiver<SyncUpdate> {
        self.updates.subscribe()
    }

    /// Проверяет цель клика в действующей сессии и возвращает подтверждённый адрес сообщения.
    pub fn resolve_notification_target(
        &self,
        target: NotificationTarget,
        generation: SessionGeneration,
    ) -> Result<NotificationTarget, NotificationTargetError> {
        let exists = self
            .auth
            .with_active_generation(&target.context, generation, || {
                self.storage.contains_notification_target(&target)
            })??;
        if exists {
            Ok(target)
        } else {
            Err(NotificationTargetError::UnknownTarget)
        }
    }

    /// Записывает событие, пока поколение сессии остаётся действующим.
    /// Проверка сессии и запись защищены одной блокировкой аккаунта.
    pub fn apply(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        source: &str,
        event: Event,
    ) -> Result<ApplyOutcome, SyncError> {
        self.auth.with_active_generation(owner, generation, || {
            if matches!(event, Event::SourceReady(_)) {
                return Ok(ApplyOutcome::Applied);
            }
            if let Event::UnreadBaseline { cursor, count } = &event {
                if self.storage.apply_unread_baseline(owner, cursor, *count)? {
                    self.publish(owner, generation, SyncChange::Unread(*count));
                }
                return Ok(ApplyOutcome::Applied);
            }

            let unread = match &event {
                Event::UnreadChanged { count, .. } => Some(*count),
                _ => None,
            };
            let conversation = match &event {
                Event::IncomingDirectMessage {
                    conversation_id, ..
                } => Some(conversation_id.as_str()),
                _ => None,
            };
            let placement = match &event {
                Event::MessageCreated { placement, .. }
                | Event::IncomingDirectMessage { placement, .. } => placement.as_ref(),
                _ => None,
            };
            let metadata = match &event {
                Event::MessageCreated { metadata, .. }
                | Event::IncomingDirectMessage { metadata, .. } => metadata.as_ref(),
                _ => None,
            };
            let history = match &event {
                Event::HistoryChanged { change, .. } => Some(change),
                _ => None,
            };
            let saved = self.storage.apply(
                owner,
                source,
                event.cursor(),
                EventWrite {
                    message: event.message(),
                    unread,
                    notification_conversation: conversation,
                    placement,
                    metadata,
                    history,
                },
            )?;
            if let Some(count) = unread.filter(|_| saved.unread_changed) {
                self.publish(owner, generation, SyncChange::Unread(count));
            }
            if (saved.message_inserted || saved.send_confirmed)
                && let (Some(placement), Some(_), Some(message)) =
                    (placement, metadata, event.message())
                && let (Ok(stream_id), Ok(topic_id), Ok(message_id)) = (
                    placement.stream_id.parse(),
                    placement.topic_id.parse(),
                    message.id.parse(),
                )
            {
                let scope = HistoryScope {
                    stream_id,
                    topic_id: Some(topic_id),
                };
                if saved.message_inserted {
                    self.publish(
                        owner,
                        generation,
                        SyncChange::HistoryMessageCreated { scope, message_id },
                    );
                }
                if saved.send_confirmed {
                    self.publish(owner, generation, SyncChange::OutboxChanged { scope });
                }
            }
            if saved.outcome == ApplyOutcome::Applied && source == crate::messenger::SOURCE_ID {
                if let Event::SidebarChanged { change, .. } = &event {
                    for stream_id in self.sidebar_cache.apply(owner, generation, change) {
                        self.publish(
                            owner,
                            generation,
                            SyncChange::SidebarDetailsChanged { stream_id },
                        );
                    }
                }
                if let Some(change) = history.and_then(history_update) {
                    match &change {
                        SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(id)) => {
                            self.sidebar_cache
                                .invalidate(owner, generation, Some(*id), true);
                        }
                        SyncChange::HistoryAccessRevoked(HistoryRevocation::Topic {
                            stream_id,
                            topic_id,
                        }) => {
                            if self
                                .sidebar_cache
                                .apply(
                                    owner,
                                    generation,
                                    &SidebarChange::RemoveTopic {
                                        stream_id: *stream_id,
                                        topic_id: *topic_id,
                                    },
                                )
                                .contains(stream_id)
                            {
                                self.publish(
                                    owner,
                                    generation,
                                    SyncChange::SidebarDetailsChanged {
                                        stream_id: *stream_id,
                                    },
                                );
                            }
                        }
                        _ => {}
                    }
                    self.publish(owner, generation, change);
                }
            }
            if saved.message_inserted
                && let Event::IncomingDirectMessage {
                    message,
                    conversation_id,
                    ..
                } = event
            {
                self.publish(
                    owner,
                    generation,
                    SyncChange::IncomingDirectMessage {
                        conversation_id,
                        message_id: message.id,
                    },
                );
            }
            Ok(saved.outcome)
        })?
    }

    /// Сообщает, что очередь указанной темы изменилась.
    fn publish_send(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        snapshot: &PreparedSend,
    ) {
        self.publish(
            owner,
            generation,
            SyncChange::OutboxChanged {
                scope: HistoryScope {
                    stream_id: snapshot.stream_id,
                    topic_id: Some(snapshot.topic_id),
                },
            },
        );
    }

    /// Перечитывает панель после подтверждённого действия текущего пользователя.
    pub(crate) fn refresh_sidebar(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        revoked: bool,
    ) -> Result<(), AuthError> {
        self.auth.with_active_generation(owner, generation, || {
            self.sidebar_cache
                .invalidate(owner, generation, Some(stream_id), revoked);
            let change = if revoked {
                SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(stream_id))
            } else {
                SyncChange::SidebarDetailsChanged { stream_id }
            };
            self.publish(owner, generation, change);
        })
    }

    /// Публикует изменение под блокировкой действующей сессии после записи в базу.
    fn publish(&self, owner: &OwnerContext, generation: SessionGeneration, change: SyncChange) {
        let _ = self.updates.send(SyncUpdate {
            context: owner.clone(),
            generation,
            change,
        });
    }
}

/// Выбирает изменение, которое открытая история должна показать после записи.
fn history_update(change: &HistoryChange) -> Option<SyncChange> {
    let update = match change {
        HistoryChange::Edit { message, .. } => SyncChange::HistoryMessageChanged {
            message_id: message.id.parse().ok()?,
        },
        HistoryChange::RemoveMessage(id) => SyncChange::HistoryMessageChanged {
            message_id: id.parse().ok()?,
        },
        HistoryChange::RemoveStream(id) => {
            SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(id.parse().ok()?))
        }
        HistoryChange::RemoveTopic(address) => {
            SyncChange::HistoryAccessRevoked(HistoryRevocation::Topic {
                stream_id: address.stream_id.parse().ok()?,
                topic_id: address.topic_id.parse().ok()?,
            })
        }
    };
    Some(update)
}
