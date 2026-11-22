//! Загружает каталог Messenger и хранит последний полный снимок по аккаунту и проекту.
//! Сначала описаны данные для окна, затем загрузка и разбор ответов сервера.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{Auth, AuthError, Service, SessionSnapshot},
    context::OwnerContext,
    runtime::SourceError,
    storage::{Storage, StorageError},
};

use super::rest::{RestClient, STREAMS_PATH, TOPICS_PATH};

mod preview;
pub use preview::SidebarPreview;
pub(crate) use preview::{SidebarPreviews, attach as attach_previews};

const USERS_PATH: &str = "/api/workspace/v1/users/";

/// Разговор и его темы для списка чатов.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Chat {
    pub id: Uuid,           // Идентификатор разговора.
    pub name: String,       // Название канала или имя собеседника.
    pub direct: bool,       // У разговора один собеседник или это чат с собой.
    pub topics: Vec<Topic>, // Доступные темы канала.
    #[serde(default)]
    pub metadata: ChatMetadata, // Сведения для строки разговора и переходов.
}

/// Тема канала в списке чатов.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Topic {
    pub id: Uuid,     // Идентификатор темы.
    pub name: String, // Название темы.
    #[serde(default)]
    pub metadata: TopicMetadata, // Сведения для вложенной строки.
}

/// Сведения о разговоре из каталога и справочника собеседников.
#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ChatMetadata {
    pub default_topic_id: Option<Uuid>, // Доступная тема по умолчанию, если сервер её указал.
    pub direct_peer_id: Option<Uuid>,   // Собеседник в личном чате.
    pub direct_peer_username: Option<String>, // Имя учётной записи собеседника для поиска.
    pub direct_peer_email: Option<String>, // Доступный адрес собеседника для поиска.
    pub last_message_id: Option<Uuid>,  // Ссылка на последнее сообщение, если оно есть.
    #[serde(skip)]
    pub preview: Option<SidebarPreview>, // Превью из фонового запроса текущего входа.
    pub color: Option<u32>,             // Цвет в формате RGB, заданный сервером.
    pub invite_only: bool,              // Вступление в канал требует приглашения.
    pub notification_mode: Option<String>, // Режим уведомлений из ответа сервера.
    pub unread_count: Option<u64>,      // Все непрочитанные сообщения, если известны.
    pub active_unread_count: Option<u64>, // Непрочитанные, влияющие на уведомления.
    pub passive_unread_count: Option<u64>, // Остальные непрочитанные сообщения.
}

/// Сведения о состоянии темы, полученные вместе со списком тем.
#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TopicMetadata {
    pub is_done: bool,                 // Тема завершена.
    pub is_default: bool,              // Тема используется по умолчанию в своём канале.
    pub color: Option<u32>,            // Цвет темы в формате RGB, если его вернул сервер.
    pub last_message_id: Option<Uuid>, // Ссылка на последнее сообщение, если оно есть.
    #[serde(skip)]
    pub preview: Option<SidebarPreview>, // Превью из фонового запроса текущего входа.
    pub notification_mode: Option<String>, // Режим уведомлений темы из ответа сервера.
    pub unread_count: Option<u64>,     // Все непрочитанные сообщения, если известны.
    pub active_unread_count: Option<u64>, // Непрочитанные, влияющие на уведомления.
    pub passive_unread_count: Option<u64>, // Остальные непрочитанные сообщения.
}

/// Абсолютные серверные счётчики одной строки каталога.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogCounts {
    pub stream_id: Uuid,        // Канал или личный разговор.
    pub topic_id: Option<Uuid>, // Тема. None означает весь разговор.
    pub total: u32,             // Все непрочитанные сообщения.
    pub active: u32,            // Непрочитанные, влияющие на уведомления.
    pub passive: u32,           // Остальные непрочитанные сообщения.
}

/// Причина, по которой список чатов не удалось обновить.
#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Сессия не смогла выдать токен или завершилась.
    #[error(transparent)]
    Source(#[from] SourceError), // Сервер не вернул полный список.
    #[error(transparent)]
    Storage(#[from] StorageError), // Ошибка записи полного снимка.
}

/// Загружает чаты и темы через REST и сохраняет их в SQLite.
pub struct Catalog {
    rest: Arc<RestClient>,            // HTTP-клиент Messenger с настройками TLS.
    storage: Arc<Storage>,            // База, в которой лежит последний полный снимок.
    refresh_gates: Arc<RefreshGates>, // Набор загрузок, общий для Bootstrap и фоновой сверки.
}

/// Слабые ссылки на блокировки загрузки для каждого владельца.
pub(super) type RefreshGates = Mutex<HashMap<OwnerContext, Weak<tokio::sync::Mutex<()>>>>;

impl Catalog {
    /// Использует HTTP-клиент мессенджера и общую базу приложения.
    pub(super) fn new(
        rest: Arc<RestClient>,
        storage: Arc<Storage>,
        refresh_gates: Arc<RefreshGates>,
    ) -> Self {
        Self {
            rest,
            storage,
            refresh_gates,
        }
    }

    /// Читает предыдущий снимок для показа при сетевом сбое.
    pub(crate) fn cached(&self, owner: &OwnerContext) -> Result<Option<Vec<Chat>>, StorageError> {
        self.storage.chat_catalog(owner)
    }

    /// Подписывает загрузчик на изменения каталога после фиксации событий.
    pub(crate) fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.storage.catalog_changes()
    }

    /// Будит загрузчик, когда курсор журнала впервые становится известен.
    pub(crate) fn checkpoints(&self) -> tokio::sync::watch::Receiver<u64> {
        self.storage.catalog_checkpoints()
    }

    /// Определяет, нужен ли список, уже учитывающий исходную позицию журнала.
    pub(crate) fn needs_reconciliation(&self, owner: &OwnerContext) -> Result<bool, StorageError> {
        self.storage.needs_catalog_reconciliation(owner)
    }

    /// Передаёт снимки обработчику, пока отзыв доступа не может изменить базу.
    pub(crate) fn with_snapshots<T>(
        &self,
        owners: &[OwnerContext],
        publish: impl FnOnce(Vec<Option<Vec<Chat>>>) -> T,
    ) -> Result<T, StorageError> {
        self.storage.with_catalog_snapshots(owners, publish)
    }

    /// Обновляет каталог только для поколения, которое запросило загрузку.
    pub(crate) async fn refresh(
        &self,
        auth: &Auth,
        session: &SessionSnapshot,
    ) -> Result<Vec<Chat>, CatalogError> {
        let gate = self.refresh_gate(&session.context);
        let _loading = gate.lock().await;
        loop {
            match self.refresh_locked(auth, session).await {
                // Папки или сброс журнала могли запросить новый каталог во время загрузки.
                Err(CatalogError::Storage(StorageError::StaleCatalog))
                    if self.storage.catalog_needs_refresh(&session.context)? => {}
                result => return result,
            }
        }
    }

    /// После другой загрузки проверяет, осталась ли работа для фоновой сверки.
    pub(super) async fn refresh_if_needed(
        &self,
        auth: &Auth,
        session: &SessionSnapshot,
    ) -> Result<bool, CatalogError> {
        let gate = self.refresh_gate(&session.context);
        let _loading = gate.lock().await;
        if !self.storage.catalog_needs_refresh(&session.context)? {
            return Ok(false);
        }
        match self.refresh_locked(auth, session).await {
            Ok(_) | Err(CatalogError::Storage(StorageError::StaleCatalog)) => Ok(true),
            Err(error) => Err(error),
        }
    }

    /// Возвращает общую блокировку загрузки этого владельца, не удерживая чужие аккаунты.
    fn refresh_gate(&self, owner: &OwnerContext) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self
            .refresh_gates
            .lock()
            .expect("блокировки загрузки каталога");
        gates.retain(|_, gate| gate.strong_count() != 0);
        if let Some(gate) = gates.get(owner).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(owner.clone(), Arc::downgrade(&gate));
        gate
    }

    /// Читает полный каталог и сохраняет его, пока действуют исходные сессия и ревизия.
    async fn refresh_locked(
        &self,
        auth: &Auth,
        session: &SessionSnapshot,
    ) -> Result<Vec<Chat>, CatalogError> {
        let owner = &session.context;
        let revision = self.storage.catalog_revision(owner)?;
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, session.generation)
            .await?;
        // Позиция журнала до запроса — нижняя граница событий, которые учёл REST.
        let fence = self.storage.cursor(owner, super::SOURCE_ID)?;
        let chats = self.fetch(owner, &token).await?;
        let snapshot = serde_json::to_string(&chats).map_err(StorageError::from)?;
        auth.with_active_generation(owner, session.generation, || {
            self.storage
                .save_chat_catalog(owner, &snapshot, revision, fence.as_ref())
        })??;
        Ok(chats)
    }

    /// Загружает разговоры и темы, проверяет их владельца и связывает темы с каналами.
    async fn fetch(&self, owner: &OwnerContext, token: &str) -> Result<Vec<Chat>, SourceError> {
        let streams_path = format!("{STREAMS_PATH}/");
        let topics_path = format!("{TOPICS_PATH}/");
        let (streams, topics): (Vec<WireChat>, Vec<WireTopic>) = tokio::try_join!(
            self.rest.collection(owner, token, &streams_path),
            self.rest.collection(owner, token, &topics_path),
        )?;
        let project = owner
            .project_id
            .parse::<Uuid>()
            .map_err(|_| SourceError::InvalidResponse)?;
        let user = owner
            .user_id
            .parse::<Uuid>()
            .map_err(|_| SourceError::InvalidResponse)?;
        if streams
            .iter()
            .any(|chat| chat.project_id != project || chat.user_uuid != user)
            || topics
                .iter()
                .any(|topic| topic.project_id != project || topic.user_uuid != user)
        {
            return Err(SourceError::InvalidResponse);
        }
        let streams: Vec<_> = streams
            .into_iter()
            .filter(|chat| !chat.is_archived)
            .collect();
        let users = self.direct_users(owner, token, &streams).await;
        Ok(Self::assemble(streams, topics, &users, user))
    }

    /// Собирает чат из уже проверенных данных сервера и справочника.
    fn assemble(
        streams: Vec<WireChat>,
        topics: Vec<WireTopic>,
        users: &HashMap<Uuid, WireUser>,
        user: Uuid,
    ) -> Vec<Chat> {
        let mut topics_by_chat: HashMap<Uuid, Vec<Topic>> = HashMap::new();
        for topic in topics {
            topics_by_chat
                .entry(topic.stream_uuid)
                .or_default()
                .push(Topic {
                    id: topic.uuid,
                    name: topic.name,
                    metadata: TopicMetadata {
                        is_done: topic.is_done,
                        is_default: topic.is_default,
                        color: topic.color,
                        last_message_id: topic.last_message_uuid,
                        preview: None,
                        notification_mode: topic.notification_mode,
                        unread_count: topic.unread_count,
                        active_unread_count: topic.active_unread_count,
                        passive_unread_count: topic.passive_unread_count,
                    },
                });
        }
        let mut chats: Vec<_> = streams
            .into_iter()
            .map(|chat| {
                let direct = chat.private && chat.direct_user_uuid.is_some();
                let peer_id = chat.direct_user_uuid.filter(|_| direct);
                let peer = peer_id.and_then(|id| users.get(&id));
                let name = match peer_id {
                    Some(peer) if peer == user => "Избранное".to_owned(),
                    Some(_) => peer.map(WireUser::display_name).unwrap_or(chat.name),
                    None => chat.name,
                };
                let mut topics = if direct {
                    Vec::new()
                } else {
                    topics_by_chat.remove(&chat.uuid).unwrap_or_default()
                };
                topics.sort_by_cached_key(|topic| (topic.name.to_lowercase(), topic.id));
                let default_topic_id = match chat.default_topic_uuid {
                    Some(id) => topics.iter().find(|topic| topic.id == id),
                    None => topics.iter().find(|topic| topic.metadata.is_default),
                }
                .map(|topic| topic.id);
                for topic in &mut topics {
                    topic.metadata.is_default = Some(topic.id) == default_topic_id;
                }
                Chat {
                    id: chat.uuid,
                    name,
                    direct,
                    topics,
                    metadata: ChatMetadata {
                        default_topic_id,
                        direct_peer_id: peer_id,
                        direct_peer_username: peer.map(|profile| profile.username.clone()),
                        direct_peer_email: peer
                            .and_then(|profile| profile.email.as_deref())
                            .map(str::trim)
                            .filter(|email| !email.is_empty())
                            .map(str::to_owned),
                        last_message_id: chat.last_message_uuid,
                        preview: None,
                        color: chat.color,
                        invite_only: chat.invite_only,
                        notification_mode: chat.notification_mode,
                        unread_count: chat.unread_count,
                        active_unread_count: chat.active_unread_count,
                        passive_unread_count: chat.passive_unread_count,
                    },
                }
            })
            .collect();
        chats.sort_by_cached_key(|chat| (chat.name.to_lowercase(), chat.id));
        chats
    }

    /// Берёт профили собеседников из справочника. При сбое используются названия разговоров.
    async fn direct_users(
        &self,
        owner: &OwnerContext,
        token: &str,
        streams: &[WireChat],
    ) -> HashMap<Uuid, WireUser> {
        if !streams
            .iter()
            .any(|chat| chat.private && chat.direct_user_uuid.is_some())
        {
            return HashMap::new();
        }
        match self
            .rest
            .collection::<WireUser>(owner, token, USERS_PATH)
            .await
        {
            Ok(users) => users.into_iter().map(|user| (user.uuid, user)).collect(),
            Err(error) => {
                log::warn!("Не удалось загрузить имена собеседников: {error}");
                HashMap::new()
            }
        }
    }
}

/// Поля разговора из REST, нужные для списка и проверки владельца.
#[derive(Deserialize)]
struct WireChat {
    uuid: Uuid,                       // Идентификатор разговора.
    project_id: Uuid,                 // Проект, которому принадлежит разговор.
    user_uuid: Uuid,                  // Пользователь, от имени которого получен разговор.
    name: String,                     // Название, сохранённое на сервере.
    private: bool,                    // Признак личного разговора.
    direct_user_uuid: Option<Uuid>,   // Собеседник. У канала его нет.
    is_archived: bool,                // Архивный разговор скрыт из основного списка.
    default_topic_uuid: Option<Uuid>, // Тема по умолчанию, указанная сервером.
    last_message_uuid: Option<Uuid>,  // Последнее сообщение в разговоре.
    color: Option<u32>,               // Цвет разговора в формате RGB.
    #[serde(default)]
    invite_only: bool, // Вступление по приглашению.
    notification_mode: Option<String>, // Режим уведомлений канала.
    unread_count: Option<u64>,        // Все непрочитанные сообщения.
    active_unread_count: Option<u64>, // Активные непрочитанные сообщения.
    passive_unread_count: Option<u64>, // Пассивные непрочитанные сообщения.
}

/// Поля темы из REST, нужные для вложенной строки и проверки владельца.
#[derive(Deserialize)]
struct WireTopic {
    uuid: Uuid,         // Идентификатор темы.
    project_id: Uuid,   // Проект, которому принадлежит тема.
    user_uuid: Uuid,    // Пользователь, от имени которого получена тема.
    stream_uuid: Uuid,  // Канал, которому принадлежит тема.
    name: String,       // Название темы.
    color: Option<u32>, // Цвет темы в формате RGB.
    #[serde(default)]
    is_done: bool, // Тема завершена.
    #[serde(default)]
    is_default: bool, // Тема используется по умолчанию.
    last_message_uuid: Option<Uuid>, // Последнее сообщение в теме.
    notification_mode: Option<String>, // Режим уведомлений темы.
    unread_count: Option<u64>, // Все непрочитанные сообщения.
    active_unread_count: Option<u64>, // Активные непрочитанные сообщения.
    passive_unread_count: Option<u64>, // Пассивные непрочитанные сообщения.
}

/// Поля профиля из справочника Workspace, которые нужны для имени собеседника.
#[derive(Deserialize)]
struct WireUser {
    uuid: Uuid,                 // Идентификатор пользователя.
    username: String,           // Имя учётной записи при отсутствии имени и фамилии.
    first_name: Option<String>, // Имя пользователя, если заполнено.
    last_name: Option<String>,  // Фамилия пользователя, если заполнена.
    email: Option<String>,      // Адрес пользователя, доступный в проекте.
}

impl WireUser {
    /// Собирает отображаемое имя собеседника, используя логин при пустом имени.
    fn display_name(&self) -> String {
        let name = [self.first_name.as_deref(), self.last_name.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if name.is_empty() {
            self.username.clone()
        } else {
            name
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bootstrap::{ChatList, ChatListStatus, ChatLists},
        context::{AccountId, SessionGeneration},
        storage::{Cursor, EventWrite, HistoryChange},
    };

    /// Снимок, уже ожидающий публикации, перечитывается после отзыва из базы.
    #[test]
    fn publication_reloads_snapshot_committed_before_revocation() {
        let owner = OwnerContext {
            account_id: AccountId("account".into()),
            server: "https://test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let stream = Uuid::from_u128(1);
        let old = vec![Chat {
            id: stream,
            name: "channel".into(),
            direct: false,
            topics: Vec::new(),
            metadata: ChatMetadata::default(),
        }];
        let snapshot = serde_json::to_string(&old).unwrap();
        let storage = Arc::new(Storage::in_memory().unwrap());
        storage
            .save_chat_catalog(
                &owner,
                &snapshot,
                0,
                Some(&Cursor {
                    epoch_generation: "epoch".into(),
                    epoch_version: 0,
                }),
            )
            .unwrap();
        let mut current = ChatLists::new();
        current.insert(
            owner.clone(),
            ChatList {
                generation: SessionGeneration(1),
                chats: Some(Arc::from(old)),
                status: ChatListStatus::Ready,
            },
        );
        storage
            .apply(
                &owner,
                crate::messenger::SOURCE_ID,
                &Cursor {
                    epoch_generation: "epoch".into(),
                    epoch_version: 1,
                },
                EventWrite {
                    history: Some(&HistoryChange::RemoveStream(stream.to_string())),
                    ..EventWrite::default()
                },
            )
            .unwrap();
        let (lists, published) = tokio::sync::watch::channel(ChatLists::new());
        let messenger = crate::messenger::MessengerSource::new().unwrap();
        crate::bootstrap::publish_catalog(
            &messenger.catalog(storage),
            &mut current,
            &HashMap::new(),
            &lists,
        )
        .unwrap();
        assert!(
            published.borrow()[&owner]
                .chats
                .as_ref()
                .unwrap()
                .is_empty()
        );
    }
}
