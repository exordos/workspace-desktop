//! Проверяет и создаёт личный разговор текущего пользователя с собой по REST.

use reqwest::{StatusCode, header::ACCEPT};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{context::OwnerContext, runtime::SourceError, storage::HistoryScope};

use super::{
    RestClient, STREAMS_PATH, WireStream, check_explicit_owner, check_status, parse_uuid,
    request_error, workspace_origin,
};

impl RestClient {
    /// Сверяет известный UUID чата с сервером. 404 разрешает поиск через создание.
    pub(in crate::messenger) async fn find_self_chat(
        &self,
        owner: &OwnerContext,
        token: &str,
        user: Uuid,
        expected: Uuid,
    ) -> Result<Option<HistoryScope>, SourceError> {
        let response = self
            .request(owner, token, &format!("{STREAMS_PATH}/{expected}"))?
            .send()
            .await
            .map_err(request_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        check_status(response.status())?;
        let stream: WireStream = response.json().await.map_err(request_error)?;
        let scope = self
            .confirm_self_chat(owner, token, user, Some(expected), stream)
            .await?;
        Ok(Some(scope))
    }

    /// Создаёт нативный личный чат или получает уже существующий чат той же пары.
    pub(in crate::messenger) async fn create_self_chat(
        &self,
        owner: &OwnerContext,
        token: &str,
        user: Uuid,
    ) -> Result<HistoryScope, SourceError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(&format!("{STREAMS_PATH}/"));
        let response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .json(&json!({
                "name": "Personal notes",
                "description": "",
                "source_name": "native",
                "direct_user_uuid": user,
            }))
            .send()
            .await
            .map_err(request_error)?;
        if response.status() != StatusCode::CREATED {
            check_status(response.status())?;
        }
        let stream: WireStream = response.json().await.map_err(request_error)?;
        self.confirm_self_chat(owner, token, user, None, stream)
            .await
    }

    /// Проверяет доступ к теме по умолчанию и возвращает область обычной личной истории.
    async fn confirm_self_chat(
        &self,
        owner: &OwnerContext,
        token: &str,
        user: Uuid,
        expected: Option<Uuid>,
        stream: WireStream,
    ) -> Result<HistoryScope, SourceError> {
        let (stream_id, topic_id) = self_chat_ids(owner, user, expected, &stream)?;
        let topic = self
            .topic_access(owner, token, stream_id, topic_id)
            .await
            .map_err(|error| match error {
                crate::messenger::history::OpenHistoryError::Source(cause) => cause,
                _ => SourceError::InvalidResponse,
            })?;
        check_explicit_owner(
            owner,
            topic.project_id.as_deref(),
            topic.user_uuid.as_deref(),
        )?;
        Ok(HistoryScope {
            stream_id,
            topic_id: None,
        })
    }
}

/// Принимает чат только для текущего проекта и пользователя, с непустыми UUID чата и темы.
fn self_chat_ids(
    owner: &OwnerContext,
    user: Uuid,
    expected: Option<Uuid>,
    stream: &WireStream,
) -> Result<(Uuid, Uuid), SourceError> {
    check_explicit_owner(
        owner,
        stream.project_id.as_deref(),
        stream.user_uuid.as_deref(),
    )?;
    let stream_id = parse_uuid(&stream.uuid)
        .filter(|id| !id.is_nil() && expected.is_none_or(|expected| expected == *id))
        .ok_or(SourceError::InvalidResponse)?;
    let topic_id = stream
        .default_topic_uuid
        .as_ref()
        .and_then(Value::as_str)
        .and_then(parse_uuid)
        .filter(|id| !id.is_nil())
        .ok_or(SourceError::InvalidResponse)?;
    if !stream.private
        || stream.is_archived
        || stream.source_name.as_deref() != Some("native")
        || stream.direct_user_uuid.as_deref().and_then(parse_uuid) != Some(user)
    {
        return Err(SourceError::InvalidResponse);
    }
    Ok((stream_id, topic_id))
}

#[cfg(test)]
mod tests;
