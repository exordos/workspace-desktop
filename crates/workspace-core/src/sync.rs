//! Связывает получение событий с локальной базой.
//! Проверяет сессию перед записью и публикует изменения для оболочки после записи.

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::broadcast;

use crate::{
    auth::{Auth, AuthError},
    context::{OwnerContext, SessionGeneration},
    notification_target::{NotificationTarget, NotificationTargetError},
    storage::{
        Cursor, EventWrite, HistoryChange, HistoryMessage, HistoryMetadata, HistoryPageEdges,
        HistoryPageGuard, HistoryPageOutcome, HistoryPageTarget, HistoryScope, HistoryWindow,
        Message, MessagePlacement, Storage, StorageError,
    },
};

pub use crate::storage::ApplyOutcome;

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
    IncomingDirectMessage {
        conversation_id: String, // Идентификатор личного разговора на сервере.
        message_id: String,      // Идентификатор нового сообщения.
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
    UnreadChanged {
        cursor: Cursor, // Позиция события обновления папки в журнале.
        count: u32,     // Новый счётчик папки «Все чаты».
    },
    UnreadBaseline {
        cursor: Cursor, // Позиция сервера перед чтением счётчика. Курсор сообщений не меняется.
        count: u32,     // Счётчик, загруженный отдельным запросом при подключении.
    },
    Checkpoint(Cursor), // Новая позиция чтения без сообщения.
}

impl Event {
    /// Возвращает позицию, до которой нужно продвинуть чтение после записи.
    pub fn cursor(&self) -> &Cursor {
        match self {
            Self::MessageCreated { cursor, .. }
            | Self::IncomingDirectMessage { cursor, .. }
            | Self::HistoryChanged { cursor, .. }
            | Self::UnreadChanged { cursor, .. }
            | Self::UnreadBaseline { cursor, .. }
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

/// Проверяет право сессии на запись и передаёт события в хранилище.
pub struct Sync {
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
            // Оба пути создания проходят здесь после фиксации сообщения и курсора.
            if saved.message_inserted
                && let (Some(placement), Some(_), Some(message)) =
                    (placement, metadata, event.message())
                && let (Ok(stream_id), Ok(topic_id), Ok(message_id)) = (
                    placement.stream_id.parse(),
                    placement.topic_id.parse(),
                    message.id.parse(),
                )
            {
                self.publish(
                    owner,
                    generation,
                    SyncChange::HistoryMessageCreated {
                        scope: HistoryScope {
                            stream_id,
                            topic_id: Some(topic_id),
                        },
                        message_id,
                    },
                );
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

    /// Публикует изменение под блокировкой действующей сессии после записи в базу.
    fn publish(&self, owner: &OwnerContext, generation: SessionGeneration, change: SyncChange) {
        let _ = self.updates.send(SyncUpdate {
            context: owner.clone(),
            generation,
            change,
        });
    }
}
