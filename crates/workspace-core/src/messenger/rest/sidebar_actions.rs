//! Отправляет подтверждённые REST-команды каталога и сверяет владельца ответа.

use super::*;
use crate::messenger::details::DirectSidebar;
use crate::messenger::sidebar_actions::{SidebarStreamAction, SidebarStreamResult};
use reqwest::Method;

impl RestClient {
    /// Читает профиль только после проверки принадлежности к личному чату.
    pub(in crate::messenger) async fn sidebar_contact(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream_id: Uuid,
    ) -> Result<DirectSidebar, DetailsError> {
        let stream = self
            .stream_access(owner, token, stream_id)
            .await
            .map_err(details_access_error)?;
        check_explicit_owner(
            owner,
            stream.project_id.as_deref(),
            stream.user_uuid.as_deref(),
        )?;
        let user_id = stream
            .direct_user_uuid
            .as_deref()
            .and_then(parse_uuid)
            .filter(|_| stream.private)
            .ok_or(DetailsError::ActionDenied)?;
        let profile = self
            .sidebar_profile(owner, token, user_id)
            .await?
            .ok_or(DetailsError::ProfileUnavailable)?;
        Ok(DirectSidebar {
            user_id,
            profile,
            is_self: parse_uuid(&owner.user_id) == Some(user_id),
            notification_mode: stream.notification_mode,
        })
    }

    /// Проверяет доступ к разговору, отправляет команду и сверяет адрес результата.
    pub(in crate::messenger) async fn update_sidebar_stream(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream_id: Uuid,
        action: SidebarStreamAction,
    ) -> Result<SidebarStreamResult, DetailsError> {
        let stream = self
            .stream_access(owner, token, stream_id)
            .await
            .map_err(details_access_error)?;
        check_explicit_owner(
            owner,
            stream.project_id.as_deref(),
            stream.user_uuid.as_deref(),
        )?;
        if stream.is_archived {
            return if matches!(action, SidebarStreamAction::Archive) {
                Ok(SidebarStreamResult::Updated)
            } else {
                Err(DetailsError::ActionDenied)
            };
        }

        let stream_path = format!("{STREAMS_PATH}/{stream_id}");
        let create_id = match &action {
            SidebarStreamAction::CreateTopic { id, name }
                if !id.is_nil() && !name.trim().is_empty() =>
            {
                // Повтор после потерянного ответа проверяет ту же тему до записи.
                match self.topic_access(owner, token, stream_id, *id).await {
                    Ok(existing) if existing.name.as_deref() == Some(name.trim()) => {
                        check_explicit_owner(
                            owner,
                            existing.project_id.as_deref(),
                            existing.user_uuid.as_deref(),
                        )?;
                        return Ok(SidebarStreamResult::TopicCreated(*id));
                    }
                    Ok(_) => return Err(DetailsError::ActionDenied),
                    Err(OpenHistoryError::AccessLost) => Some(*id),
                    Err(error) => return Err(details_access_error(error)),
                }
            }
            _ => None,
        };
        let (method, path, body) = match action {
            SidebarStreamAction::MarkRead => (
                Method::POST,
                format!("{stream_path}/actions/read/invoke"),
                None,
            ),
            SidebarStreamAction::CreateTopic { id, name } if create_id.is_some() => (
                Method::POST,
                format!("{TOPICS_PATH}/"),
                Some(
                    serde_json::json!({ "uuid": id, "stream_uuid": stream_id, "name": name.trim() }),
                ),
            ),
            SidebarStreamAction::Rename(name) if !stream.is_direct() && !name.trim().is_empty() => {
                (
                    Method::PUT,
                    stream_path,
                    Some(serde_json::json!({ "name": name.trim() })),
                )
            }
            SidebarStreamAction::Archive => (
                Method::POST,
                format!("{stream_path}/actions/archive/invoke"),
                None,
            ),
            SidebarStreamAction::CreateTopic { .. } | SidebarStreamAction::Rename(_) => {
                return Err(DetailsError::ActionDenied);
            }
        };
        let mut url = workspace_origin(owner)?;
        url.set_path(&path);
        let mut request = self
            .send
            .request(method, url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(request_error)?;
        match response.status() {
            StatusCode::CREATED if create_id.is_some() => {}
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => {
                return Err(DetailsError::ActionDenied);
            }
            status => check_status(status)?,
        }
        if let Some(id) = create_id {
            let topic: WireTopic = response.json().await.map_err(request_error)?;
            check_explicit_owner(
                owner,
                topic.project_id.as_deref(),
                topic.user_uuid.as_deref(),
            )?;
            if !topic.matches(owner, stream_id, id) {
                return Err(SourceError::InvalidResponse.into());
            }
            return Ok(SidebarStreamResult::TopicCreated(id));
        }
        let saved: WireStream = response.json().await.map_err(request_error)?;
        check_explicit_owner(
            owner,
            saved.project_id.as_deref(),
            saved.user_uuid.as_deref(),
        )?;
        if parse_uuid(&saved.uuid) != Some(stream_id) {
            return Err(SourceError::InvalidResponse.into());
        }
        Ok(SidebarStreamResult::Updated)
    }
}
