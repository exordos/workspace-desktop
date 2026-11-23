//! Проверяет нативный исходный чат ссылки и тему отправки смешанного сообщения.

use serde_json::Value;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    messenger::{history::OpenHistoryError, send::DirectSendError},
    runtime::SourceError,
    storage::HistoryScope,
};

use super::{RestClient, WireStream, check_explicit_owner, parse_uuid};

impl RestClient {
    /// Подтверждает принадлежность исходного чата и получает доступную тему для запроса ключа.
    pub(in crate::messenger) async fn fragment_source_topic(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream_id: Uuid,
    ) -> Result<Option<Uuid>, OpenHistoryError> {
        let row = self.fragment_stream(owner, token, stream_id).await?;
        Ok(default_topic(&row))
    }

    /// Проверяет, что в эту нативную тему всё ещё можно отправлять обычный Markdown.
    pub(in crate::messenger) async fn fragment_send_topic(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
    ) -> Result<Uuid, DirectSendError> {
        let row = self.fragment_stream(owner, token, scope.stream_id).await?;
        row.check_plain_send()?;
        let direct = row.is_direct();
        if row.direct_user_uuid.is_some() && !direct {
            return Err(SourceError::InvalidResponse.into());
        }
        let topic = if direct {
            let default = default_topic(&row).ok_or(DirectSendError::TopicUnavailable)?;
            if scope.topic_id.is_some_and(|id| id != default) {
                return Err(DirectSendError::TopicUnavailable);
            }
            default
        } else {
            scope
                .topic_id
                .filter(|id| !id.is_nil())
                .ok_or(DirectSendError::TopicUnavailable)?
        };
        let target = self
            .topic_access(owner, token, scope.stream_id, topic)
            .await?;
        check_explicit_owner(
            owner,
            target.project_id.as_deref(),
            target.user_uuid.as_deref(),
        )?;
        match target.is_done {
            Some(false) => Ok(topic),
            Some(true) => Err(DirectSendError::TopicClosed),
            None if direct => Ok(topic),
            None => Err(SourceError::InvalidResponse.into()),
        }
    }

    /// Принимает описание только видимого владельцу нативного разговора.
    async fn fragment_stream(
        &self,
        owner: &OwnerContext,
        token: &str,
        id: Uuid,
    ) -> Result<WireStream, OpenHistoryError> {
        let row = self.stream_access(owner, token, id).await?;
        check_explicit_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref())?;
        if row.is_archived
            || row.source_name.as_deref() != Some("native")
            || !matches!(row.encryption, Some(Value::Bool(_)))
        {
            return Err(OpenHistoryError::AccessLost);
        }
        Ok(row)
    }
}

/// Разбирает объявленную сервером тему по умолчанию.
fn default_topic(row: &WireStream) -> Option<Uuid> {
    row.default_topic_uuid
        .as_ref()
        .and_then(Value::as_str)
        .and_then(parse_uuid)
        .filter(|id| !id.is_nil())
}
