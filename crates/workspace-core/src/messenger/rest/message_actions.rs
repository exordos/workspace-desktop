//! Проверяет серверный адрес удаляемой строки и выполняет одну HTTPS-мутацию.

use reqwest::{StatusCode, header::ACCEPT};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    messenger::message_actions::{DeleteError, MessageTarget},
    runtime::SourceError,
};

use super::{
    MESSAGES_PATH, RestClient, WireHistoryMessage, check_explicit_owner, check_status,
    mutations::mutation_id, parse_uuid, request_error, workspace_origin,
};

impl RestClient {
    /// Подтверждает точное размещение, автора и формат по ответам API владельца.
    pub(in crate::messenger) async fn check_deletable_message(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: MessageTarget,
    ) -> Result<(), DeleteError> {
        let stream = self
            .stream_access(owner, token, target.scope.stream_id)
            .await?;
        check_explicit_owner(
            owner,
            stream.project_id.as_deref(),
            stream.user_uuid.as_deref(),
        )?;
        let valid_kind = match target.scope.topic_id {
            Some(_) => !stream.private && stream.direct_user_uuid.is_none(),
            None => stream.is_direct(),
        };
        if !valid_kind
            || stream.is_archived
            || stream.source_name.as_deref() != Some("native")
            || stream.encryption != Some(Value::Bool(false))
        {
            return Err(DeleteError::Unsupported);
        }
        let topic = self
            .topic_access(owner, token, target.scope.stream_id, target.topic_id)
            .await?;
        check_explicit_owner(
            owner,
            topic.project_id.as_deref(),
            topic.user_uuid.as_deref(),
        )?;

        let response = self
            .request(
                owner,
                token,
                &format!("{MESSAGES_PATH}{}", target.message_id),
            )?
            .send()
            .await
            .map_err(request_error)?;
        match response.status() {
            StatusCode::FORBIDDEN => return Err(DeleteError::Forbidden),
            StatusCode::NOT_FOUND => return Err(DeleteError::MessageUnavailable),
            status => check_status(status)?,
        }
        let row: WireHistoryMessage = response.json().await.map_err(request_error)?;
        check_explicit_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref())?;
        if row.uuid != target.message_id
            || row.stream_uuid != target.scope.stream_id
            || row.topic_uuid != target.topic_id
        {
            return Err(DeleteError::MessageUnavailable);
        }
        let author = parse_uuid(&owner.user_id).ok_or(SourceError::InvalidResponse)?;
        if row.author_uuid != author {
            return Err(DeleteError::NotAuthor);
        }
        if row.source_name.as_deref() != Some("native")
            || row.payload.kind != "markdown"
            || !matches!(row.payload.content, Some(Value::String(_)))
        {
            return Err(DeleteError::Unsupported);
        }
        Ok(())
    }

    /// Отправляет DELETE без повтора и различает применение, приём в очередь и отказ.
    pub(in crate::messenger) async fn delete_message_once(
        &self,
        owner: &OwnerContext,
        token: &str,
        message_id: Uuid,
    ) -> Result<Option<Uuid>, DeleteError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(&format!("{MESSAGES_PATH}{message_id}"));
        let response = self
            .send
            .delete(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| DeleteError::Unknown)?;

        match response.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => {
                mutation_id(response.headers()).map_err(|_| DeleteError::Unknown)
            }
            StatusCode::FORBIDDEN => Err(DeleteError::Forbidden),
            StatusCode::NOT_FOUND => Err(DeleteError::MessageUnavailable),
            status if status.is_client_error() && status != StatusCode::REQUEST_TIMEOUT => {
                Err(DeleteError::Rejected(status.as_u16()))
            }
            _ => Err(DeleteError::Unknown),
        }
    }
}
