//! Проверяет серверный адрес редактируемой или удаляемой строки и выполняет одну HTTPS-мутацию.

use reqwest::{StatusCode, header::ACCEPT};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    messenger::{
        history::OpenHistoryError,
        message_actions::{DeleteError, MessageTarget},
        send::SendError,
    },
    runtime::SourceError,
    storage::{HistoryMessage, MessageBody},
};

use super::{
    MESSAGES_PATH, RestClient, WireHistoryMessage, check_explicit_owner, check_status,
    encrypted_payload, mutations::mutation_id, parse_uuid, request_error, workspace_origin,
};

impl RestClient {
    /// Читает исходное нативное сообщение по подтверждённому размещению и проверяет автора.
    pub(in crate::messenger) async fn encrypted_edit_source(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: MessageTarget,
    ) -> Result<HistoryMessage, OpenHistoryError> {
        self.native_edit_source(owner, token, target, "e2ee").await
    }

    /// Читает обычный Markdown из точного нативного размещения текущего автора.
    pub(in crate::messenger) async fn markdown_edit_source(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: MessageTarget,
    ) -> Result<HistoryMessage, OpenHistoryError> {
        self.native_edit_source(owner, token, target, "markdown")
            .await
    }

    /// Проверяет автора, тип и адрес источника правки.
    async fn native_edit_source(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: MessageTarget,
        kind: &str,
    ) -> Result<HistoryMessage, OpenHistoryError> {
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
            StatusCode::NOT_FOUND => return Err(OpenHistoryError::MessageMissing),
            StatusCode::FORBIDDEN => return Err(OpenHistoryError::AccessLost),
            status => check_status(status)?,
        }
        let row: WireHistoryMessage = response.json().await.map_err(request_error)?;
        check_explicit_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref())?;
        if !matches_native_edit_target(owner, target, &row, kind) {
            return Err(OpenHistoryError::MessageMissing);
        }
        row.into_history(owner, target.scope.stream_id)?
            .ok_or(OpenHistoryError::MessageMissing)
    }

    /// Отправляет единственный PUT с конвертом и сверяет подтверждённое обновление.
    pub(in crate::messenger) async fn edit_encrypted_once(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: MessageTarget,
        key_id: Uuid,
        ciphertext: &str,
    ) -> Result<HistoryMessage, SendError> {
        let message = self
            .edit_native_once(
                owner,
                token,
                target,
                "e2ee",
                encrypted_payload(key_id, ciphertext),
            )
            .await?;
        match &message.body {
            MessageBody::Encrypted {
                key_id: received_key,
                ciphertext: received_text,
            } if *received_key == key_id && received_text == ciphertext => Ok(message),
            _ => Err(SendError::Unknown),
        }
    }

    /// Отправляет единственный PUT с готовым Markdown и сверяет подтверждённое тело.
    pub(in crate::messenger) async fn edit_markdown_once(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: MessageTarget,
        markdown: &str,
    ) -> Result<HistoryMessage, SendError> {
        let payload = serde_json::json!({"kind": "markdown", "content": markdown});
        let message = self
            .edit_native_once(owner, token, target, "markdown", payload)
            .await?;
        if message.body.markdown() == Some(markdown) {
            return Ok(message);
        }
        Err(SendError::Unknown)
    }

    /// Передаёт подготовленное тело и принимает только ответ исходного размещения и типа.
    async fn edit_native_once(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: MessageTarget,
        kind: &str,
        payload: Value,
    ) -> Result<HistoryMessage, SendError> {
        let mut url = workspace_origin(owner).map_err(|_| SendError::InvalidInput)?;
        url.set_path(&format!("{MESSAGES_PATH}{}", target.message_id));
        let response = self
            .send
            .put(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .json(&serde_json::json!({"payload": payload}))
            .send()
            .await
            .map_err(|_| SendError::Unknown)?;
        let status = response.status();
        if status.is_client_error()
            && !matches!(
                status,
                StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
            )
        {
            return Err(SendError::Rejected(status.as_u16()));
        }
        if status != StatusCode::OK {
            return Err(SendError::Unknown);
        }
        let row: WireHistoryMessage = response.json().await.map_err(|_| SendError::Unknown)?;
        if check_explicit_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref()).is_err()
            || !matches_native_edit_target(owner, target, &row, kind)
        {
            return Err(SendError::Unknown);
        }
        row.into_history(owner, target.scope.stream_id)
            .map_err(|_| SendError::Unknown)?
            .ok_or(SendError::Unknown)
    }

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
        let valid_kind = stream.is_direct()
            || (target.scope.topic_id.is_some()
                && !stream.private
                && stream.direct_user_uuid.is_none());
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

/// Сверяет нативный формат, автора и полное размещение для запроса правки.
fn matches_native_edit_target(
    owner: &OwnerContext,
    target: MessageTarget,
    row: &WireHistoryMessage,
    kind: &str,
) -> bool {
    row.uuid == target.message_id
        && row.stream_uuid == target.scope.stream_id
        && row.topic_uuid == target.topic_id
        && row.source_name.as_deref() == Some("native")
        && row.payload.kind == kind
        && parse_uuid(&owner.user_id) == Some(row.author_uuid)
}
