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
    Channel(ChannelSidebar), // Канал или защищённая личка со списком участников.
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
    pub name: String,                               // Название стрима.
    pub description: Option<String>,                // Публичное описание, если заполнено.
    pub color: Option<u32>,                         // Цвет значка RGB, если указан.
    pub invite_only: bool,                          // Стрим доступен по приглашению.
    pub is_private: bool,                           // Сервер считает стрим закрытым.
    pub encrypted: bool, // Сообщения канала защищены сквозным шифрованием.
    pub direct_peer: Option<Uuid>, // Собеседник для защищённой лички.
    pub allow_key_sharing_by_holders: Option<bool>, // Текущее значение политики, если сервер его поддерживает.
    pub can_edit_key_sharing: bool, // Владелец защищённого чата с поддержкой настройки.
    pub notification_mode: StreamNotificationMode, // Настройка текущего пользователя.
    pub can_add_members: bool,      // Текущий пользователь может приглашать.
    pub topics: Vec<SidebarTopic>,  // Темы стрима, в том числе завершённые.
    pub members: Vec<SidebarMember>, // Все подтверждённые привязки участников.
}

/// Строка темы, открываемой по её UUID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarTopic {
    pub id: Uuid,     // UUID темы, сохраняемый при переименовании.
    pub name: String, // Название темы.
    pub done: bool,   // Тема отмечена завершённой для всех участников.
    pub notification_mode: TopicNotificationMode, // Настройка уведомлений темы.
    pub unread_count: u32, // Все непрочитанные сообщения текущего пользователя.
    pub active_unread_count: u32, // Непрочитанные, которые учитываются с настройками уведомлений.
    pub passive_unread_count: u32, // Остальные непрочитанные сообщения.
    pub summary: Option<String>, // Последняя серверная сводка, если она появилась.
    pub summary_enabled: bool, // Workspace продолжает обновлять сводку.
    pub summary_has_new_messages: bool, // После сводки в теме появились сообщения.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TopicNotificationMode {
    Default, // Использовать настройку стрима.
    Mute,    // Выключить уведомления темы.
    Unmute,  // Включить уведомления темы.
    Follow,  // Следить за темой.
}

/// Действие над темой разговора, доступное участнику через её контекстное меню.
pub enum SidebarTopicAction {
    SetNotifications(TopicNotificationMode), // Задаёт личную настройку уведомлений темы.
    MarkRead,                                // Прочитывает тему от имени текущего пользователя.
    Rename(String),                          // Сохраняет новое название темы.
    ToggleDone,                              // Меняет общий признак завершённости.
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
    pub(in crate::messenger) kind: SidebarStreamKind, // Тип стрима для проверки пригодности старого снимка.
    pub(in crate::messenger) name: String,            // Название стрима.
    pub(in crate::messenger) description: Option<String>, // Заполненное описание стрима.
    pub(in crate::messenger) color: Option<u32>,      // Цвет значка RGB.
    pub(in crate::messenger) invite_only: bool,       // Признак доступа по приглашению.
    pub(in crate::messenger) is_private: bool,        // Признак закрытого стрима.
    pub(in crate::messenger) encrypted: bool,         // Подтверждённый режим шифрования канала.
    pub(in crate::messenger) allow_key_sharing_by_holders: Option<bool>, // Политика в ответе сервера, если известна.
    pub(in crate::messenger) can_edit_key_sharing: bool, // Владелец защищённого чата может поменять политику.
    pub(in crate::messenger) notification_mode: StreamNotificationMode, // Настройка подписчика.
    pub(in crate::messenger) can_add_members: bool,      // Текущий пользователь владеет стримом.
}

/// Тип карточки, выведенный из подтверждённых полей стрима.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::messenger) enum SidebarStreamKind {
    Peer(Uuid),          // Обычная личка с подтверждённым UUID собеседника.
    ProtectedPeer(Uuid), // Защищённая личка с общей карточкой стрима и участниками.
    Channel,             // Стрим с темами и участниками.
    Unsupported,         // Неизвестный тип стрима или неподтверждённый режим шифрования.
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
    grant_revision: u64, // Меняется, когда стрим заново становится доступным или журнал сбрасывается.
    details: Option<Arc<SidebarDetails>>, // Полный снимок, пригодный для повторного открытия.
    revoked: bool,       // Сервер сообщил, что подписка текущего пользователя удалена.
    loading: Option<SidebarLoad>, // Общий результат для ожидающих экземпляров панели.
    pending: HashMap<SidebarPatchKey, SidebarChange>, // Последние снимки сущностей, принятые во время HTTP.
}

/// Позволяет сохранить одно последнее изменение каждой сущности во время чтения REST.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SidebarPatchKey {
    Stream,        // Метаданные родительского стрима.
    Topic(Uuid),   // UUID изменённой или удалённой темы.
    Profile(Uuid), // UUID изменённого профиля.
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
                state.pending.clear();
                if stream_id.is_none() {
                    state.grant_revision += 1;
                    state.revoked = false;
                } else if revoked {
                    state.revoked = true;
                }
                state.revision += 1;
            }
        }
    }

    /// Запоминает доступ к стриму до выполнения запроса выхода.
    fn leave_grant(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
    ) -> Result<u64, DetailsError> {
        let entry = self.entry(owner, generation, stream_id);
        let state = entry.state.lock().expect("sidebar entry");
        if state.revoked {
            Err(DetailsError::AccessLost)
        } else {
            Ok(state.grant_revision)
        }
    }

    /// Применяет подтверждение выхода, пока после него не выдали новый доступ к стриму.
    pub(crate) fn complete_leave(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        grant_revision: u64,
    ) -> bool {
        let entry = self.entry(owner, generation, stream_id);
        let mut state = entry.state.lock().expect("sidebar entry");
        if state.grant_revision != grant_revision || state.revoked {
            return false;
        }
        state.details = None;
        state.pending.clear();
        state.revoked = true;
        state.revision += 1;
        true
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
            SidebarChange::Stream { created: true, .. } => {
                self.grant_revision += 1;
                if self.revoked {
                    self.revoked = false;
                    self.revision += 1;
                    return true;
                }
            }
            SidebarChange::MembershipBinding(binding) if self.details.as_ref().is_some_and(|details| {
                !matches!(details.as_ref(), SidebarDetails::Channel(channel) if channel.members.iter().any(|member| member.binding_id == *binding))
            }) => return false,
            _ => {}
        }
        let Some(details) = &self.details else {
            if self.revoked || self.loading.is_none() {
                return false;
            }
            let key = match change {
                SidebarChange::Stream { .. } => SidebarPatchKey::Stream,
                SidebarChange::Topic { topic, .. } => SidebarPatchKey::Topic(topic.id),
                SidebarChange::RemoveTopic { topic_id, .. } => SidebarPatchKey::Topic(*topic_id),
                SidebarChange::Profile { user_id, .. } => SidebarPatchKey::Profile(*user_id),
                SidebarChange::Membership(_) | SidebarChange::MembershipBinding(_) => {
                    // Привязки могут меняться между страницами. Тогда читаем весь состав заново.
                    self.revision += 1;
                    return false;
                }
            };
            self.pending.insert(key, change.clone());
            return false;
        };
        let updated = match change {
            SidebarChange::Membership(_) | SidebarChange::MembershipBinding(_) => None,
            SidebarChange::Stream { metadata, .. } if details.kind() != metadata.kind => None,
            SidebarChange::Profile { user_id, profile }
                if !details.needs_profile(*user_id, profile) =>
            {
                return false;
            }
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
    /// Сверяет тип готового снимка с обновлённым описанием стрима.
    fn kind(&self) -> SidebarStreamKind {
        match self {
            Self::Peer(peer) => SidebarStreamKind::Peer(peer.user_id),
            Self::Channel(channel) => channel
                .direct_peer
                .map_or(SidebarStreamKind::Channel, SidebarStreamKind::ProtectedPeer),
        }
    }

    /// Проверяет, повлияет ли обновление профиля на хотя бы одну строку панели.
    fn needs_profile(&self, user_id: Uuid, profile: &UserProfile) -> bool {
        match self {
            Self::Peer(peer) => peer.user_id == user_id && &peer.profile != profile,
            Self::Channel(channel) => channel.members.iter().any(|member| {
                member.user_id == user_id && member.profile.as_ref() != Some(profile)
            }),
        }
    }

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
                channel.is_private = metadata.is_private;
                channel.encrypted = metadata.encrypted;
                channel.allow_key_sharing_by_holders = metadata.allow_key_sharing_by_holders;
                channel.can_edit_key_sharing = metadata.can_edit_key_sharing;
                channel.notification_mode = metadata.notification_mode;
                if channel.can_add_members != metadata.can_add_members {
                    // Права удаления вычисляются с учётом собственного UUID, который передаёт привязка.
                    for member in &mut channel.members {
                        member.can_remove = metadata.can_add_members || member.is_self;
                    }
                }
                channel.can_add_members = metadata.can_add_members;
            }
            (Self::Channel(channel), SidebarChange::Topic { topic, .. })
                if channel.direct_peer.is_none() =>
            {
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
            (Self::Channel(channel), SidebarChange::RemoveTopic { topic_id, .. })
                if channel.direct_peer.is_none() =>
            {
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
                let mut state = entry.state.lock().expect("sidebar entry");
                state.loading = None;
                state.pending.clear();
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
            let mut state = entry.state.lock().expect("sidebar entry");
            state.pending.clear();
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
            let pending = std::mem::take(&mut state.pending);
            if let (Ok(details), Some(SidebarChange::Stream { metadata, .. })) =
                (&result, pending.get(&SidebarPatchKey::Stream))
                && details.kind() != metadata.kind
            {
                return None;
            }
            Some(result.map(|mut details| {
                for change in pending.values() {
                    details.apply(change);
                }
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
        self.check_sidebar_mutation_access(sync, owner, generation, stream_id)?;
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
        Ok(sync.refresh_sidebar_catalog(owner, generation, stream_id)?)
    }

    /// Меняет политику передачи ключей подтверждённого защищённого чата по праву владельца.
    pub async fn update_sidebar_key_sharing(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        allow: bool,
    ) -> Result<(), DetailsError> {
        let details = self
            .sidebar_details(sync, owner, generation, stream_id)
            .await?;
        let SidebarDetails::Channel(channel) = details.as_ref() else {
            return Err(DetailsError::ActionDenied);
        };
        if !channel.can_edit_key_sharing {
            return Err(DetailsError::ActionDenied);
        }
        if channel.allow_key_sharing_by_holders == Some(allow) {
            return Ok(());
        }
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .update_sidebar_key_sharing(owner, &token, stream_id, allow)
            .await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result?;
        Ok(sync.refresh_sidebar(owner, generation, stream_id)?)
    }

    /// Выполняет действие для подтверждённой темы разговора текущей сессии.
    pub async fn update_sidebar_topic(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        topic_id: Uuid,
        action: SidebarTopicAction,
    ) -> Result<(), DetailsError> {
        self.check_sidebar_mutation_access(sync, owner, generation, stream_id)?;
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .update_sidebar_topic(owner, &token, stream_id, topic_id, action)
            .await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result?;
        Ok(sync.refresh_sidebar_catalog(owner, generation, stream_id)?)
    }

    /// Проверяет локальный отзыв стрима перед серверной проверкой действия.
    pub(in crate::messenger) fn check_sidebar_mutation_access(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
    ) -> Result<(), DetailsError> {
        sync.auth().with_active_generation(owner, generation, || {
            let entry = sync.sidebar_cache.entry(owner, generation, stream_id);
            if entry.state.lock().expect("sidebar entry").revoked {
                Err(DetailsError::AccessLost)
            } else {
                Ok(())
            }
        })?
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
        Ok(sync.refresh_sidebar(owner, generation, stream_id)?)
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
        let leave_grant = if member.is_self {
            Some(sync.auth().with_active_generation(owner, generation, || {
                sync.sidebar_cache.leave_grant(owner, generation, stream_id)
            })??)
        } else {
            None
        };
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
        if let Some(leave_grant) = leave_grant {
            sync.complete_sidebar_leave(owner, generation, stream_id, leave_grant)?;
        } else {
            sync.refresh_sidebar(owner, generation, stream_id)?;
        }
        Ok(())
    }
}

/// Строит веб-ссылку на профиль в текущем Workspace.
pub fn sidebar_profile_link(owner: &OwnerContext, user_id: Uuid) -> Result<String, SourceError> {
    let mut url = super::super::rest::workspace_origin(owner)?;
    url.set_fragment(Some(&format!("user/{user_id}")));
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AccountId;

    /// Готовит два разделённых контекста с одинаковым ID канала.
    fn owner(login: &str) -> OwnerContext {
        OwnerContext {
            account_id: AccountId(login.into()),
            server: "https://workspace.test".into(),
            organization_id: "test".into(),
            project_id: "project".into(),
            user_id: login.into(),
        }
    }

    /// Даёт снимок канала, который сервер присылает после смены счётчика или названия.
    fn stream_details(id: Uuid, name: &str, created: bool) -> SidebarChange {
        SidebarChange::Stream {
            id,
            created,
            metadata: SidebarStreamMetadata {
                kind: SidebarStreamKind::Channel,
                name: name.into(),
                description: None,
                color: None,
                invite_only: false,
                is_private: false,
                encrypted: false,
                allow_key_sharing_by_holders: None,
                can_edit_key_sharing: false,
                notification_mode: StreamNotificationMode::AllMessages,
                can_add_members: false,
            },
        }
    }

    /// Заполняет кэш полученным снимком, чтобы проверить обновления без повторного HTTP.
    fn fill_channel(entry: &SidebarEntry) {
        entry.state.lock().unwrap().details =
            Some(Arc::new(SidebarDetails::Channel(ChannelSidebar {
                name: "Канал".into(),
                description: None,
                color: None,
                invite_only: false,
                is_private: false,
                encrypted: false,
                direct_peer: None,
                allow_key_sharing_by_holders: None,
                can_edit_key_sharing: false,
                notification_mode: StreamNotificationMode::AllMessages,
                can_add_members: false,
                topics: Vec::new(),
                members: Vec::new(),
            })));
    }

    /// Смена счётчика сохраняет тот же снимок, а переименование обновляет только свой аккаунт.
    #[test]
    fn applies_metadata_without_refetching_or_crossing_accounts() {
        let cache = SidebarCache::default();
        let first = owner("first");
        let second = owner("second");
        let id = Uuid::new_v4();
        let generation = SessionGeneration(1);
        let first_entry = cache.entry(&first, generation, id);
        let second_entry = cache.entry(&second, generation, id);
        fill_channel(&first_entry);
        fill_channel(&second_entry);
        assert!(
            cache
                .apply(&first, generation, &stream_details(id, "Канал", false))
                .is_empty()
        );
        assert_eq!(
            cache.apply(&first, generation, &stream_details(id, "Другое имя", false)),
            [id]
        );
        let right_name = |entry: &SidebarEntry, name: &str| {
            matches!(
                entry.state.lock().unwrap().details.as_deref(),
                Some(SidebarDetails::Channel(channel)) if channel.name == name,
            )
        };
        assert!(right_name(&first_entry, "Другое имя"));
        assert!(right_name(&second_entry, "Канал"));
        assert_eq!(
            cache.apply(&first, generation, &SidebarChange::Membership(id)),
            [id]
        );
        assert!(first_entry.state.lock().unwrap().details.is_none());
        assert!(right_name(&second_entry, "Канал"));
    }

    /// Отзыв доступа перебивает ожидающий ответ и поздний успех команды.
    #[test]
    fn revoke_survives_refresh_until_server_grants_stream_again() {
        let cache = SidebarCache::default();
        let owner = owner("first");
        let id = Uuid::new_v4();
        let generation = SessionGeneration(1);
        let entry = cache.entry(&owner, generation, id);
        fill_channel(&entry);
        cache.invalidate(&owner, generation, Some(id), true);
        cache.invalidate(&owner, generation, Some(id), false);
        assert!(
            cache
                .apply(&owner, generation, &stream_details(id, "Канал", false))
                .is_empty()
        );
        {
            let state = entry.state.lock().unwrap();
            assert!(state.revoked);
            assert!(state.details.is_none());
            assert_eq!(state.revision, 2);
        }
        let old_grant = entry.state.lock().unwrap().grant_revision;
        assert_eq!(
            cache.apply(&owner, generation, &stream_details(id, "Канал", true)),
            [id]
        );
        assert!(!cache.complete_leave(&owner, generation, id, old_grant));
        assert!(!entry.state.lock().unwrap().revoked);
        let next = cache.entry(&owner, SessionGeneration(2), id);
        assert!(!next.state.lock().unwrap().revoked);
        assert!(!Arc::ptr_eq(&entry, &next));
    }
}
