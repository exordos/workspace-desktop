//! Снимки двух правых панелей, загрузка и обновление на время действующей сессии.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    auth::{Auth, Service},
    context::{OwnerContext, SessionGeneration},
    directory::UserProfile,
    runtime::SourceError,
    sync::Sync,
};

use super::{super::rest::RestClient, DetailsError, MessengerSource};

/// Сведения для всего разговора, общие для его тем.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarDetails {
    Peer(DirectSidebar),     // Собеседник обычного личного чата.
    Channel(ChannelSidebar), // Стрим с темами и списком участников.
}

/// Сведения о личном чате, включая особый случай чата с собой.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectSidebar {
    pub user_id: Uuid,        // UUID собеседника или текущего пользователя.
    pub profile: UserProfile, // Публичная карточка собеседника.
    pub is_self: bool,        // Собеседник совпадает с текущим пользователем.
    pub notification_mode: StreamNotificationMode, // Настройка текущего пользователя.
}

/// Сведения о стриме, видимые его участнику.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelSidebar {
    pub name: String,                              // Название стрима.
    pub description: Option<String>,               // Публичное описание, если заполнено.
    pub color: Option<u32>,                        // Цвет значка RGB, если указан.
    pub invite_only: bool,                         // Стрим доступен по приглашению.
    pub notification_mode: StreamNotificationMode, // Настройка текущего пользователя.
    pub can_add_members: bool,                     // Текущий пользователь может приглашать.
    pub topics: Vec<SidebarTopic>,                 // Темы стрима, в том числе завершённые.
    pub members: Vec<SidebarMember>,               // Все подтверждённые привязки участников.
}

/// Строка темы, открываемой по её UUID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarTopic {
    pub id: Uuid,     // UUID темы, сохраняемый при переименовании.
    pub name: String, // Название темы.
    pub done: bool,   // Тема отмечена завершённой для текущего пользователя.
    pub notification_mode: TopicNotificationMode, // Настройка уведомлений темы.
}

/// Участник, его привязка и действия, видимые в рамках текущей сессии.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarMember {
    pub binding_id: Uuid, // UUID привязки, который принимает команда удаления.
    pub user_id: Uuid,    // UUID участника в справочнике.
    pub profile: Option<UserProfile>, // Каталог может ещё не знать приглашённого пользователя.
    pub role: StreamRole, // Роль участника в стриме.
    pub is_self: bool,    // Привязка текущего пользователя.
    pub can_remove: bool, // Пользователь может снять эту привязку.
}

/// Известные серверные роли в канале.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamRole {
    Guest,         // Гостевой доступ.
    Member,        // Обычное участие.
    Moderator,     // Модератор.
    Administrator, // Администратор.
    Owner,         // Владелец.
    #[serde(other)]
    Unknown, // Новая серверная роль без подтверждённых прав в клиенте.
}

/// Настройка уведомлений отдельного разговора на сервере.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamNotificationMode {
    AllMessages,  // Включены уведомления о сообщениях.
    MentionsOnly, // Уведомления только об упоминаниях.
    Muted,        // Уведомления выключены.
}

/// Настройка уведомлений темы на сервере.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopicNotificationMode {
    Default, // Использовать настройку стрима.
    Mute,    // Выключить уведомления темы.
    Unmute,  // Включить уведомления темы.
    Follow,  // Следить за темой.
}

/// Изменение журнала, которое может обновить сохранённую панель.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarChange {
    Stream {
        id: Uuid,                        // Стрим из серверного снимка.
        created: bool,                   // Сервер вновь предоставил видимость стрима.
        metadata: SidebarStreamMetadata, // Поля стрима, которые влияют на панель.
    },
    Topic {
        stream_id: Uuid,     // Родительский стрим темы.
        topic: SidebarTopic, // Актуальная строка темы.
    },
    RemoveTopic {
        stream_id: Uuid, // Родительский стрим удалённой темы.
        topic_id: Uuid,  // Тема, которую больше нельзя открывать.
    },
    MembershipBinding(Uuid), // Старый формат события передал UUID привязки без стрима.
    Membership(Uuid),        // Состав или роль участников стрима изменились.
    Profile {
        user_id: Uuid,        // UUID профиля, который изменился.
        profile: UserProfile, // Полный снимок профиля из события справочника.
    },
}

/// Часть снимка стрима, которую можно обновить из обычного события.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarStreamMetadata {
    pub(in crate::messenger) name: String, // Название стрима.
    pub(in crate::messenger) description: Option<String>, // Заполненное описание стрима.
    pub(in crate::messenger) color: Option<u32>, // Цвет значка RGB.
    pub(in crate::messenger) invite_only: bool, // Признак доступа по приглашению.
    pub(in crate::messenger) notification_mode: StreamNotificationMode, // Настройка подписчика.
    pub(in crate::messenger) can_add_members: bool, // Текущий пользователь владеет стримом.
}

type SidebarKey = (OwnerContext, SessionGeneration, Uuid);

/// Хранит все открытые ранее панели текущих сессий.
#[derive(Default)]
pub(crate) struct SidebarCache {
    entries: Mutex<HashMap<SidebarKey, Arc<SidebarEntry>>>, // Кэш разделён между контекстами и входами.
}

/// Один запрос занимает момент загрузки для стрима независимо от количества открытий.
#[derive(Default)]
pub(crate) struct SidebarEntry {
    state: Mutex<CachedSidebar>, // Короткая блокировка снимка и версии изменений.
}

/// Позволяет обнаружить событие между началом HTTP и записью его ответа.
#[derive(Default)]
struct CachedSidebar {
    revision: u64,                        // Меняется при принятии события или сбросе.
    details: Option<Arc<SidebarDetails>>, // Полный снимок, пригодный для повторного открытия.
    revoked: bool, // Сервер сообщил, что подписка текущего пользователя удалена.
    loading: Option<SidebarLoad>, // Общий результат для ожидающих экземпляров панели.
}

type SidebarResult = Result<Arc<SidebarDetails>, DetailsError>;
type SidebarLoad = watch::Receiver<Option<SidebarResult>>;

/// Результат кэша или подписка на общую загрузку.
enum SidebarRead {
    Immediate(SidebarResult), // Снимок готов либо доступ отозван.
    Loading(SidebarLoad),     // Загрузка продолжается даже после закрытия панели.
}

impl SidebarCache {
    /// Получает общий вход загрузки и освобождает данные прежних поколений владельца.
    fn entry(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
    ) -> Arc<SidebarEntry> {
        let mut entries = self.entries.lock().expect("sidebar cache");
        entries.retain(|(context, session, _), _| context != owner || *session == generation);
        entries
            .entry((owner.clone(), generation, stream_id))
            .or_default()
            .clone()
    }

    /// Применяет только подтверждённые события журнала и возвращает затронутые панели.
    pub(crate) fn apply(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        change: &SidebarChange,
    ) -> Vec<Uuid> {
        let entries = self.entries.lock().expect("sidebar cache");
        entries
            .iter()
            .filter_map(|((context, session, stream_id), entry)| {
                if context != owner || *session != generation {
                    return None;
                }
                let mut state = entry.state.lock().expect("sidebar entry");
                state.apply(*stream_id, change).then_some(*stream_id)
            })
            .collect()
    }

    /// Очищает панель и отзывает ответы, начатые до сброса или потери доступа.
    pub(crate) fn invalidate(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Option<Uuid>,
        revoked: bool,
    ) {
        let entries = self.entries.lock().expect("sidebar cache");
        for ((context, session, id), entry) in entries.iter() {
            if context == owner
                && *session == generation
                && stream_id.is_none_or(|stream| stream == *id)
            {
                let mut state = entry.state.lock().expect("sidebar entry");
                state.details = None;
                if stream_id.is_none() || revoked {
                    state.revoked = revoked;
                }
                state.revision += 1;
            }
        }
    }

    /// Освобождает кэш сессии при выходе или начале нового поколения.
    pub(crate) fn clear_session(&self, owner: &OwnerContext, generation: SessionGeneration) {
        self.entries
            .lock()
            .expect("sidebar cache")
            .retain(|(context, session, _), _| context != owner || *session != generation);
    }
}

impl CachedSidebar {
    /// Обновляет локальный снимок или помечает ожидающий ответ устаревшим.
    fn apply(&mut self, stream_id: Uuid, change: &SidebarChange) -> bool {
        match change {
            SidebarChange::Stream { id, .. } | SidebarChange::Membership(id) if *id != stream_id => return false,
            SidebarChange::Topic { stream_id: id, .. } | SidebarChange::RemoveTopic { stream_id: id, .. }
                if *id != stream_id => return false,
            SidebarChange::Stream { created: true, .. } if self.revoked => {
                self.revoked = false;
                self.revision += 1;
                return true;
            }
            SidebarChange::MembershipBinding(binding) if self.details.as_ref().is_some_and(|details| {
                !matches!(details.as_ref(), SidebarDetails::Channel(channel) if channel.members.iter().any(|member| member.binding_id == *binding))
            }) => return false,
            _ => {}
        }
        // При загрузке ещё неизвестен состав, поэтому защищаем и ответ справочника.
        let Some(details) = &self.details else {
            if !self.revoked {
                self.revision += 1;
            }
            return false;
        };
        let updated = match change {
            SidebarChange::Membership(_) | SidebarChange::MembershipBinding(_) => None,
            _ => {
                let mut next = (**details).clone();
                next.apply(change);
                if next == **details {
                    return false;
                }
                Some(Arc::new(next))
            }
        };
        self.revision += 1;
        self.details = updated;
        true
    }
}

impl SidebarDetails {
    /// Переносит поля из полного события в готовую карточку.
    fn apply(&mut self, change: &SidebarChange) {
        match (self, change) {
            (Self::Peer(peer), SidebarChange::Profile { user_id, profile })
                if peer.user_id == *user_id =>
            {
                peer.profile = profile.clone();
            }
            (Self::Peer(peer), SidebarChange::Stream { metadata, .. }) => {
                peer.notification_mode = metadata.notification_mode;
            }
            (Self::Channel(channel), SidebarChange::Stream { metadata, .. }) => {
                channel.name.clone_from(&metadata.name);
                channel.description.clone_from(&metadata.description);
                channel.color = metadata.color;
                channel.invite_only = metadata.invite_only;
                channel.notification_mode = metadata.notification_mode;
                if channel.can_add_members != metadata.can_add_members {
                    // Права удаления вычисляются с учётом собственного UUID, который передаёт привязка.
                    for member in &mut channel.members {
                        member.can_remove = metadata.can_add_members || member.is_self;
                    }
                }
                channel.can_add_members = metadata.can_add_members;
            }
            (Self::Channel(channel), SidebarChange::Topic { topic, .. }) => {
                if let Some(current) = channel
                    .topics
                    .iter_mut()
                    .find(|current| current.id == topic.id)
                {
                    current.clone_from(topic);
                } else {
                    channel.topics.push(topic.clone());
                }
            }
            (Self::Channel(channel), SidebarChange::RemoveTopic { topic_id, .. }) => {
                channel.topics.retain(|topic| topic.id != *topic_id);
            }
            (Self::Channel(channel), SidebarChange::Profile { user_id, profile }) => {
                if let Some(member) = channel
                    .members
                    .iter_mut()
                    .find(|member| member.user_id == *user_id)
                {
                    member.profile = Some(profile.clone());
                }
            }
            _ => {}
        }
    }
}

impl MessengerSource {
    /// Возвращает снимок панели из памяти или присоединяется к общей загрузке стрима.
    pub async fn sidebar_details(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
    ) -> SidebarResult {
        let auth = sync.auth();
        let mut loading = match auth.with_active_generation(owner, generation, || {
            let entry = sync.sidebar_cache.entry(owner, generation, stream_id);
            let mut state = entry.state.lock().expect("sidebar entry");
            if state.revoked {
                return SidebarRead::Immediate(Err(DetailsError::AccessLost));
            }
            if let Some(details) = &state.details {
                return SidebarRead::Immediate(Ok(details.clone()));
            }
            if let Some(loading) = &state.loading {
                return SidebarRead::Loading(loading.clone());
            }
            let (send, receive) = watch::channel(None);
            state.loading = Some(receive.clone());
            let rest = self.rest.clone();
            let auth = auth.clone();
            let owner = owner.clone();
            let entry = entry.clone();
            // Панель может закрыться, пока загрузка продолжает пополнять кэш сессии.
            tokio::spawn(async move {
                let result =
                    load_sidebar(&rest, &auth, &owner, generation, stream_id, &entry).await;
                entry.state.lock().expect("sidebar entry").loading = None;
                send.send_replace(Some(result));
            });
            SidebarRead::Loading(receive)
        })? {
            SidebarRead::Loading(loading) => loading,
            SidebarRead::Immediate(cached) => return cached,
        };
        loading
            .wait_for(Option::is_some)
            .await
            .map_err(|_| SourceError::Temporary)?
            .clone()
            .expect("sidebar request completed")?;
        auth.with_active_generation(owner, generation, || {
            // Событие могло отозвать или обновить снимок после завершения запроса.
            let entry = sync.sidebar_cache.entry(owner, generation, stream_id);
            let state = entry.state.lock().expect("sidebar entry");
            if state.revoked {
                Err(DetailsError::AccessLost)
            } else {
                state.details.clone().ok_or(SourceError::Temporary.into())
            }
        })?
    }
}

/// Загружает данные и отклоняет результат, если журнал поменял состояние во время запроса.
async fn load_sidebar(
    rest: &RestClient,
    auth: &Auth,
    owner: &OwnerContext,
    generation: SessionGeneration,
    stream_id: Uuid,
    entry: &SidebarEntry,
) -> SidebarResult {
    for _ in 0..3 {
        let revision = auth.with_active_generation(owner, generation, || {
            let state = entry.state.lock().expect("sidebar entry");
            if state.revoked {
                Err(DetailsError::AccessLost)
            } else {
                Ok(state.revision)
            }
        })??;
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = rest.sidebar_details(owner, &token, stream_id).await;
        let confirmed = auth.with_active_generation(owner, generation, || {
            let mut state = entry.state.lock().expect("sidebar entry");
            if state.revoked {
                return Some(Err(DetailsError::AccessLost));
            }
            if state.revision != revision {
                return None;
            }
            Some(result.map(|details| {
                let details = Arc::new(details);
                state.details = Some(details.clone());
                details
            }))
        })?;
        if let Some(confirmed) = confirmed {
            return confirmed;
        }
    }
    Err(SourceError::Temporary.into())
}

impl MessengerSource {
    /// Читает профили, доступные для выбора нового участника.
    pub async fn sidebar_directory(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<Vec<(Uuid, UserProfile)>, DetailsError> {
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self.rest.sidebar_directory(owner, &token).await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result
    }

    /// Меняет настройку уведомлений от имени пользователя и обновляет панель после успеха.
    pub async fn update_sidebar_notifications(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        mode: StreamNotificationMode,
    ) -> Result<(), DetailsError> {
        self.sidebar_details(sync, owner, generation, stream_id)
            .await?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .update_sidebar_notifications(owner, &token, stream_id, mode)
            .await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result?;
        Ok(sync.refresh_sidebar(owner, generation, stream_id, false)?)
    }

    /// Добавляет новых участников, если текущему пользователю доступно приглашение в стрим.
    pub async fn add_sidebar_members(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        user_ids: &[Uuid],
    ) -> Result<(), DetailsError> {
        let details = self
            .sidebar_details(sync, owner, generation, stream_id)
            .await?;
        let SidebarDetails::Channel(channel) = details.as_ref() else {
            return Err(DetailsError::ActionDenied);
        };
        if !channel.can_add_members || user_ids.iter().any(Uuid::is_nil) {
            return Err(DetailsError::ActionDenied);
        }
        let mut user_ids = user_ids.to_vec();
        user_ids.sort_unstable();
        user_ids.dedup();
        user_ids.retain(|id| !channel.members.iter().any(|member| member.user_id == *id));
        if user_ids.is_empty() {
            return Ok(());
        }
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .add_sidebar_members(owner, &token, stream_id, &user_ids)
            .await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result?;
        Ok(sync.refresh_sidebar(owner, generation, stream_id, false)?)
    }

    /// Снимает видимую привязку, если это сам участник или текущий пользователь владеет стримом.
    pub async fn remove_sidebar_member(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        binding_id: Uuid,
    ) -> Result<(), DetailsError> {
        let details = self
            .sidebar_details(sync, owner, generation, stream_id)
            .await?;
        let SidebarDetails::Channel(channel) = details.as_ref() else {
            return Err(DetailsError::ActionDenied);
        };
        let member = channel
            .members
            .iter()
            .find(|member| member.binding_id == binding_id && member.can_remove)
            .ok_or(DetailsError::ActionDenied)?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .remove_sidebar_member(owner, &token, binding_id)
            .await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result?;
        Ok(sync.refresh_sidebar(owner, generation, stream_id, member.is_self)?)
    }
}

/// Строит веб-ссылку на профиль в текущем Workspace.
pub fn sidebar_profile_link(owner: &OwnerContext, user_id: Uuid) -> Result<String, SourceError> {
    let mut url = super::super::rest::workspace_origin(owner)?;
    url.set_fragment(Some(&format!("user/{user_id}")));
    Ok(url.into())
}
