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

const USERS_PATH: &str = "/api/workspace/v1/users/";

/// Разговор и его темы для списка чатов.
#[derive(Debug, Deserialize, Serialize)]
pub struct Chat {
    pub id: Uuid,           // Идентификатор разговора.
    pub name: String,       // Название канала или имя собеседника.
    pub direct: bool,       // У разговора один собеседник или это чат с собой.
    pub topics: Vec<Topic>, // Доступные темы канала.
    #[serde(default)]
    pub metadata: ChatMetadata, // Сведения для строки разговора и переходов.
}

/// Сведения о разговоре из каталога и справочника собеседников.
#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ChatMetadata {
    pub direct_peer_id: Option<Uuid>, // Собеседник в личном чате.
}

/// Тема канала в списке чатов.
#[derive(Debug, Deserialize, Serialize)]
pub struct Topic {
    pub id: Uuid,     // Идентификатор темы.
    pub name: String, // Название темы.
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
        self.storage
            .chat_catalog(owner)?
            .map(|snapshot| serde_json::from_str(&snapshot).map_err(StorageError::from))
            .transpose()
    }

    /// Подписывает загрузчик на изменения каталога после фиксации событий.
    pub(crate) fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.storage.catalog_changes()
    }

    /// Передаёт снимки обработчику, пока отзыв доступа не может изменить базу.
    pub(crate) fn with_snapshots<T>(
        &self,
        owners: &[OwnerContext],
        publish: impl FnOnce(Vec<Option<Vec<Chat>>>) -> T,
    ) -> Result<T, StorageError> {
        self.storage.with_catalog_snapshots(owners, |snapshots| {
            let parsed = snapshots
                .into_iter()
                .map(|snapshot| {
                    snapshot
                        .map(|value| serde_json::from_str::<Vec<Chat>>(&value))
                        .transpose()
                })
                .collect::<Result<_, _>>()?;
            Ok(publish(parsed))
        })?
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
        let chats = self.fetch(owner, &token).await?;
        let snapshot = serde_json::to_string(&chats).map_err(StorageError::from)?;
        auth.with_active_generation(owner, session.generation, || {
            self.storage.save_chat_catalog(owner, &snapshot, revision)
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
        let names = self.direct_names(owner, token, &streams).await;
        let mut topics_by_chat: HashMap<Uuid, Vec<Topic>> = HashMap::new();
        for topic in topics {
            topics_by_chat
                .entry(topic.stream_uuid)
                .or_default()
                .push(Topic {
                    id: topic.uuid,
                    name: topic.name,
                });
        }
        let mut chats: Vec<_> = streams
            .into_iter()
            .map(|chat| {
                let direct = chat.private && chat.direct_user_uuid.is_some();
                let name = match chat.direct_user_uuid.filter(|_| direct) {
                    Some(peer) if peer == user => "Избранное".to_owned(),
                    Some(peer) => names.get(&peer).cloned().unwrap_or(chat.name),
                    None => chat.name,
                };
                let mut topics = if direct {
                    Vec::new()
                } else {
                    topics_by_chat.remove(&chat.uuid).unwrap_or_default()
                };
                topics.sort_by_cached_key(|topic| (topic.name.to_lowercase(), topic.id));
                Chat {
                    id: chat.uuid,
                    name,
                    direct,
                    topics,
                    metadata: ChatMetadata {
                        direct_peer_id: chat.direct_user_uuid.filter(|id| direct && !id.is_nil()),
                    },
                }
            })
            .collect();
        chats.sort_by_cached_key(|chat| (chat.name.to_lowercase(), chat.id));
        Ok(chats)
    }

    /// Берёт имена собеседников из справочника. При сбое остаются серверные названия разговоров.
    async fn direct_names(
        &self,
        owner: &OwnerContext,
        token: &str,
        streams: &[WireChat],
    ) -> HashMap<Uuid, String> {
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
            Ok(users) => users
                .into_iter()
                .map(|user| {
                    let name = [user.first_name.as_deref(), user.last_name.as_deref()]
                        .into_iter()
                        .flatten()
                        .map(str::trim)
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ");
                    (
                        user.uuid,
                        if name.is_empty() { user.username } else { name },
                    )
                })
                .collect(),
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
    uuid: Uuid,                     // Идентификатор разговора.
    project_id: Uuid,               // Проект, которому принадлежит разговор.
    user_uuid: Uuid,                // Пользователь, от имени которого получен разговор.
    name: String,                   // Название, сохранённое на сервере.
    private: bool,                  // Признак личного разговора.
    direct_user_uuid: Option<Uuid>, // Собеседник. У канала его нет.
    is_archived: bool,              // Архивный разговор скрыт из основного списка.
}

/// Поля темы из REST, нужные для вложенной строки и проверки владельца.
#[derive(Deserialize)]
struct WireTopic {
    uuid: Uuid,        // Идентификатор темы.
    project_id: Uuid,  // Проект, которому принадлежит тема.
    user_uuid: Uuid,   // Пользователь, от имени которого получена тема.
    stream_uuid: Uuid, // Канал, которому принадлежит тема.
    name: String,      // Название темы.
}

/// Поля профиля из справочника Workspace, которые нужны для имени собеседника.
#[derive(Deserialize)]
struct WireUser {
    uuid: Uuid,                 // Идентификатор пользователя.
    username: String,           // Имя учётной записи при отсутствии имени и фамилии.
    first_name: Option<String>, // Имя пользователя, если заполнено.
    last_name: Option<String>,  // Фамилия пользователя, если заполнена.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bootstrap::{ChatList, ChatListStatus, ChatLists},
        context::{AccountId, SessionGeneration},
        storage::{Cursor, EventWrite, HistoryChange},
    };

    /// Ранее сохранённые каталоги загружаются до первой сверки с сервером.
    #[test]
    fn legacy_snapshot_without_metadata_still_opens() {
        let data = serde_json::json!([{
            "id": Uuid::from_u128(1), "name": "Личный чат", "direct": true, "topics": []
        }]);
        let chats: Vec<Chat> = serde_json::from_value(data).unwrap();
        assert!(chats[0].metadata.direct_peer_id.is_none());
    }

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
        storage.save_chat_catalog(&owner, &snapshot, 0).unwrap();
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
        crate::bootstrap::publish_catalog(&messenger.catalog(storage), &mut current, &lists)
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
