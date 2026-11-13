//! Загружает каталог Messenger и хранит последний полный снимок по аккаунту и проекту.
//! Сначала описаны данные для окна, затем загрузка и разбор ответов сервера.

use std::{collections::HashMap, sync::Arc};

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
    rest: Arc<RestClient>, // HTTP-клиент Messenger с настройками TLS.
    storage: Arc<Storage>, // База, в которой лежит последний полный снимок.
}

impl Catalog {
    /// Использует HTTP-клиент мессенджера и общую базу приложения.
    pub(super) fn new(rest: Arc<RestClient>, storage: Arc<Storage>) -> Self {
        Self { rest, storage }
    }

    /// Читает предыдущий снимок для показа при сетевом сбое.
    pub(crate) fn cached(&self, owner: &OwnerContext) -> Result<Option<Vec<Chat>>, StorageError> {
        self.storage
            .chat_catalog(owner)?
            .map(|snapshot| serde_json::from_str(&snapshot).map_err(StorageError::from))
            .transpose()
    }

    /// Обновляет каталог только для поколения, которое запросило загрузку.
    pub(crate) async fn refresh(
        &self,
        auth: &Auth,
        session: &SessionSnapshot,
    ) -> Result<Vec<Chat>, CatalogError> {
        let owner = &session.context;
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, session.generation)
            .await?;
        let chats = self.fetch(owner, &token).await?;
        let snapshot = serde_json::to_string(&chats).map_err(StorageError::from)?;
        auth.with_active_generation(owner, session.generation, || {
            self.storage.save_chat_catalog(owner, &snapshot)
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
