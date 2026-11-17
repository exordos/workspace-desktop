//! Читает сведения стрима, тем и участников через Workspace REST и разбирает события панели.

use std::collections::HashSet;

use futures::{StreamExt, TryStreamExt, stream};
use reqwest::{Method, StatusCode};
use serde::Serialize;

use super::*;
use crate::messenger::details::{
    ChannelSidebar, DirectSidebar, SidebarChange, SidebarDetails, SidebarMember,
    SidebarStreamMetadata, SidebarTopic, StreamRole,
};

const BINDINGS_PATH: &str = "/api/workspace/v1/messenger/stream_bindings/";
const USERS_PATH: &str = "/api/workspace/v1/users/";
const MAX_PROFILE_REQUESTS: usize = 8;

/// Привязка участника к стриму и роль в нём.
#[derive(Deserialize)]
struct WireBinding {
    uuid: Uuid,         // Идентификатор привязки.
    project_id: String, // Проект, к которому относится привязка.
    stream_uuid: Uuid,  // Стрим, состав которого загружен.
    user_uuid: Uuid,    // Пользователь, которому принадлежит привязка.
    role: StreamRole,   // Роль привязки в ответе сервера.
}

/// Тело запроса смены настройки уведомлений.
#[derive(Serialize)]
struct UpdateNotifications {
    notification_mode: StreamNotificationMode, // Новое значение для текущего пользователя.
}

/// Тело запроса приглашения обычных участников.
#[derive(Serialize)]
struct AddMembers<'a> {
    member: &'a [Uuid], // Пользователи, которых сервер добавит с ролью member.
}

impl RestClient {
    /// Проверяет доступ, читает карточку лички или загружает темы и состав стрима.
    pub(in crate::messenger) async fn sidebar_details(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream_id: Uuid,
    ) -> Result<SidebarDetails, DetailsError> {
        let stream = self
            .stream_access(owner, token, stream_id)
            .await
            .map_err(details_access_error)?;
        check_explicit_owner(
            owner,
            stream.project_id.as_deref(),
            stream.user_uuid.as_deref(),
        )?;
        let current_user = parse_uuid(&owner.user_id).ok_or(SourceError::InvalidResponse)?;
        if !matches!(stream.encryption, Some(Value::Bool(false))) {
            return Err(DetailsError::Unsupported);
        }

        if stream.private {
            let user_id = stream
                .direct_user_uuid
                .as_deref()
                .and_then(parse_uuid)
                .ok_or(DetailsError::Unsupported)?;
            let profile = self
                .sidebar_profile(owner, token, user_id)
                .await?
                .ok_or(DetailsError::ProfileUnavailable)?;
            return Ok(SidebarDetails::Peer(DirectSidebar {
                user_id,
                profile,
                is_self: user_id == current_user,
                notification_mode: stream.notification_mode,
            }));
        }
        if stream.direct_user_uuid.is_some() {
            return Err(DetailsError::Unsupported);
        }

        let metadata = stream.sidebar_metadata(owner)?;
        let stream_uuid = stream_id.to_string();
        let filters = [("stream_uuid", stream_uuid.as_str())];
        let topic_path = format!("{TOPICS_PATH}/");
        let (topics, bindings) = tokio::try_join!(
            self.filtered_collection::<WireTopic>(owner, token, &topic_path, &filters),
            self.filtered_collection::<WireBinding>(owner, token, BINDINGS_PATH, &filters),
        )
        .map_err(|error| match error {
            SourceError::Forbidden => DetailsError::AccessLost,
            _ => error.into(),
        })?;
        let mut topic_ids = HashSet::new();
        let topics = topics
            .into_iter()
            .map(|topic| {
                let (id, row) = topic.into_sidebar_topic(owner, stream_id)?;
                if !topic_ids.insert(id) {
                    return Err(SourceError::InvalidResponse);
                }
                Ok(row)
            })
            .collect::<Result<_, _>>()?;

        let mut binding_ids = HashSet::new();
        let mut user_ids = HashSet::new();
        for binding in &bindings {
            if binding.stream_uuid != stream_id
                || binding.project_id != owner.project_id
                || binding.user_uuid.is_nil()
                || binding.uuid.is_nil()
                || !binding_ids.insert(binding.uuid)
                || !user_ids.insert(binding.user_uuid)
            {
                return Err(SourceError::InvalidResponse.into());
            }
        }
        let members = stream::iter(bindings.into_iter().map(|binding| async move {
            let profile = self
                .sidebar_profile(owner, token, binding.user_uuid)
                .await?;
            let is_self = binding.user_uuid == current_user;
            Ok::<_, DetailsError>(SidebarMember {
                binding_id: binding.uuid,
                user_id: binding.user_uuid,
                profile,
                role: binding.role,
                is_self,
                can_remove: metadata.can_add_members || is_self,
            })
        }))
        .buffered(MAX_PROFILE_REQUESTS)
        .try_collect()
        .await?;

        Ok(SidebarDetails::Channel(ChannelSidebar {
            name: metadata.name,
            description: metadata.description,
            color: metadata.color,
            invite_only: metadata.invite_only,
            notification_mode: metadata.notification_mode,
            can_add_members: metadata.can_add_members,
            topics,
            members,
        }))
    }

    /// Читает публичный профиль. Для ещё не зарегистрированного участника сервер вернёт 404.
    async fn sidebar_profile(
        &self,
        owner: &OwnerContext,
        token: &str,
        user_id: Uuid,
    ) -> Result<Option<crate::directory::UserProfile>, DetailsError> {
        let profile: ProfileRow = match self
            .access_row(owner, token, &format!("{USERS_PATH}{user_id}"))
            .await
        {
            Ok(profile) => profile,
            Err(OpenHistoryError::AccessLost) => return Ok(None),
            Err(error) => return Err(details_access_error(error)),
        };
        profile
            .into_profile(user_id)
            .map(Some)
            .ok_or(SourceError::InvalidResponse.into())
    }

    /// Читает справочник для поиска людей в диалоге добавления.
    pub(in crate::messenger) async fn sidebar_directory(
        &self,
        owner: &OwnerContext,
        token: &str,
    ) -> Result<Vec<(Uuid, crate::directory::UserProfile)>, DetailsError> {
        self.collection::<ProfileRow>(owner, token, USERS_PATH)
            .await?
            .into_iter()
            .map(|row| {
                let id = row.uuid;
                row.into_profile(id)
                    .map(|profile| (id, profile))
                    .ok_or(SourceError::InvalidResponse.into())
            })
            .collect()
    }

    /// Передаёт новое значение пользовательских уведомлений серверу.
    pub(in crate::messenger) async fn update_sidebar_notifications(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream_id: Uuid,
        mode: StreamNotificationMode,
    ) -> Result<(), DetailsError> {
        self.sidebar_mutation(
            owner,
            token,
            Method::POST,
            &format!("{STREAMS_PATH}/{stream_id}/actions/notifications/invoke"),
            Some(&UpdateNotifications {
                notification_mode: mode,
            }),
        )
        .await
    }

    /// Добавляет переданных пользователей в стрим.
    pub(in crate::messenger) async fn add_sidebar_members(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream_id: Uuid,
        user_ids: &[Uuid],
    ) -> Result<(), DetailsError> {
        self.sidebar_mutation(
            owner,
            token,
            Method::POST,
            &format!("{STREAMS_PATH}/{stream_id}/actions/add_users/invoke"),
            Some(&AddMembers { member: user_ids }),
        )
        .await
    }

    /// Удаляет привязку участника в серверном источнике истины.
    pub(in crate::messenger) async fn remove_sidebar_member(
        &self,
        owner: &OwnerContext,
        token: &str,
        binding_id: Uuid,
    ) -> Result<(), DetailsError> {
        self.sidebar_mutation::<()>(
            owner,
            token,
            Method::DELETE,
            &format!("{BINDINGS_PATH}{binding_id}"),
            None,
        )
        .await
    }

    /// Передаёт один запрос изменения и различает отказ в доступе и временный сбой.
    async fn sidebar_mutation<T: Serialize + ?Sized>(
        &self,
        owner: &OwnerContext,
        token: &str,
        method: Method,
        path: &str,
        body: Option<&T>,
    ) -> Result<(), DetailsError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(path);
        let mut request = self
            .send
            .request(method, url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let status = request.send().await.map_err(request_error)?.status();
        match status {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => Ok(()),
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => {
                Err(DetailsError::ActionDenied)
            }
            status => Err(check_status(status).expect_err("status is not OK").into()),
        }
    }
}

impl WireStream {
    /// Оставляет только поля, важные для панели, без счётчиков и UUID последнего сообщения.
    fn sidebar_metadata(&self, owner: &OwnerContext) -> Result<SidebarStreamMetadata, SourceError> {
        Ok(SidebarStreamMetadata {
            name: required_title(self.name.clone())?,
            description: self
                .description
                .clone()
                .filter(|value| !value.trim().is_empty()),
            color: self.color.filter(|color| *color <= 0x00ff_ffff),
            invite_only: self.invite_only.unwrap_or_default(),
            notification_mode: self.notification_mode,
            can_add_members: self.owner.as_deref() == Some(owner.user_id.as_str()),
        })
    }
}

impl WireTopic {
    /// Подтверждает, что результат фильтра относится к нужному стриму и пользователю.
    fn into_sidebar_topic(
        self,
        owner: &OwnerContext,
        stream_id: Uuid,
    ) -> Result<(Uuid, SidebarTopic), SourceError> {
        let id = parse_uuid(&self.uuid)
            .filter(|id| !id.is_nil())
            .ok_or(SourceError::InvalidResponse)?;
        if !self.matches(owner, stream_id, id) {
            return Err(SourceError::InvalidResponse);
        }
        check_explicit_owner(owner, self.project_id.as_deref(), self.user_uuid.as_deref())?;
        Ok((
            id,
            SidebarTopic {
                id,
                name: required_title(self.name)?,
                done: self.is_done.unwrap_or_default(),
                notification_mode: self.notification_mode,
            },
        ))
    }
}

/// Берёт только известные снимки и оставляет неполный старый формат обычным курсором.
pub(super) fn wire_sidebar_change(
    event: &WireEnvelope,
    raw: &Value,
    owner: &OwnerContext,
) -> Option<SidebarChange> {
    let entity = event
        .payload
        .uuid
        .as_deref()
        .and_then(parse_uuid)
        .filter(|uuid| !uuid.is_nil())?;
    match (
        event.object_type.as_str(),
        event.action.as_str(),
        event.payload.kind.as_str(),
    ) {
        ("stream", "created", "stream.created")
        | ("stream", "updated", "stream.updated")
        | ("stream", "read", "stream.read") => {
            let stream = WireStream::deserialize(raw).ok()?;
            check_explicit_owner(
                owner,
                stream.project_id.as_deref(),
                stream.user_uuid.as_deref(),
            )
            .ok()?;
            Some(SidebarChange::Stream {
                id: entity,
                created: event.action == "created",
                metadata: stream.sidebar_metadata(owner).ok()?,
            })
        }
        ("topic", "created", "topic.created")
        | ("topic", "updated", "topic.updated")
        | ("topic", "read", "topic.read") => {
            let stream_id = event.payload.stream_uuid.as_deref().and_then(parse_uuid)?;
            let (_, topic) = WireTopic::deserialize(raw)
                .ok()?
                .into_sidebar_topic(owner, stream_id)
                .ok()?;
            Some(SidebarChange::Topic { stream_id, topic })
        }
        ("stream_binding", "created", "stream_bindings.created") => {
            Some(SidebarChange::Membership(entity))
        }
        ("stream_binding", "updated", "stream_binding.updated")
        | ("stream_binding", "deleted", "stream_binding.deleted") => {
            let stream_id = event.payload.stream_uuid.as_deref().and_then(parse_uuid);
            if let Some(id) = stream_id {
                Some(SidebarChange::Membership(id))
            } else {
                Some(SidebarChange::MembershipBinding(entity))
            }
        }
        ("user", "updated", "user.updated") => {
            let profile = ProfileRow::deserialize(raw).ok()?.into_profile(entity)?;
            Some(SidebarChange::Profile {
                user_id: entity,
                profile,
            })
        }
        _ => None,
    }
}
