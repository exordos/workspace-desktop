//! Запрашивает коллекции активности и проверяет область и содержание строк.

use std::collections::{HashMap, HashSet};

use reqwest::{RequestBuilder, StatusCode, header::HeaderMap};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    directory::ProfileRow,
    messenger::{
        activity::{
            ActivityMessage, ActivityMessageFilter, ActivityMessagePage, ActivityMessagePreview,
        },
        catalog::MessagePreview,
    },
    runtime::SourceError,
    storage::HistoryTime,
};

use super::{
    MESSAGES_PATH, RestClient, WireHistoryMessage, check_explicit_owner, check_status, get_json,
    request_error,
};

/// Публичная строка сообщения и личные флаги, используемые коллекциями.
#[derive(Deserialize)]
struct WireActivityMessage {
    #[serde(flatten)]
    message: WireHistoryMessage, // Общий ответ ресурса сообщений.
    read: bool,      // Признак прочтения текущим пользователем.
    mentioned: bool, // Признак упоминания текущего пользователя.
    starred: bool,   // Признак личной отметки.
}

impl RestClient {
    /// Запрашивает одну страницу указанного фильтра от новых сообщений к старым.
    pub(in crate::messenger) async fn activity_messages(
        &self,
        owner: &OwnerContext,
        token: &str,
        filter: ActivityMessageFilter,
        marker: Option<Uuid>,
        page_size: usize,
    ) -> Result<ActivityMessagePage, SourceError> {
        let response = self
            .activity_request(owner, token, filter, marker, page_size)?
            .send()
            .await
            .map_err(request_error)?;
        if marker.is_some() && response.status() == StatusCode::NOT_FOUND {
            return Err(SourceError::CursorPruned);
        }
        check_status(response.status())?;
        let next = next_marker(response.headers(), marker)?;
        let rows: Vec<WireActivityMessage> = response.json().await.map_err(request_error)?;
        let mut messages = rows
            .into_iter()
            .map(|row| row.into_activity(owner, filter))
            .collect::<Result<Vec<_>, _>>()?;
        messages
            .sort_by(|left, right| (&right.created_at, right.id).cmp(&(&left.created_at, left.id)));
        Ok(ActivityMessagePage { messages, next })
    }

    /// Собирает точный фильтр и применяет маркер только к его коллекции.
    fn activity_request(
        &self,
        owner: &OwnerContext,
        token: &str,
        filter: ActivityMessageFilter,
        marker: Option<Uuid>,
        page_size: usize,
    ) -> Result<RequestBuilder, SourceError> {
        let request = self.request(owner, token, MESSAGES_PATH)?.query(&[
            ("project_id", owner.project_id.as_str()),
            ("page_limit", &page_size.to_string()),
            ("sort_key", "created_at"),
            ("sort_dir", "desc"),
        ]);
        let request = match filter {
            ActivityMessageFilter::Marked => request.query(&[("starred", true)]),
            ActivityMessageFilter::Mentions => request.query(&[("mentioned", true)]),
            ActivityMessageFilter::UnreadMentions => {
                request.query(&[("mentioned", true), ("read", false)])
            }
        };
        Ok(match marker {
            Some(marker) => request.query(&[("page_marker", marker)]),
            None => request,
        })
    }

    /// Получает имена авторов одним ограниченным запросом к общему справочнику.
    pub(in crate::messenger) async fn activity_author_names(
        &self,
        owner: &OwnerContext,
        token: &str,
        messages: &mut [ActivityMessage],
    ) {
        let ids: HashSet<_> = messages.iter().map(|message| message.author_id).collect();
        if ids.is_empty() {
            return;
        }
        let mut query = vec![
            ("project_id", owner.project_id.clone()),
            ("page_limit", ids.len().to_string()),
        ];
        query.extend(ids.iter().map(|id| ("uuid", id.to_string())));
        let Ok(request) = self.request(owner, token, "/api/workspace/v1/users/") else {
            return;
        };
        let Ok(profiles) = get_json::<Vec<ProfileRow>>(request.query(&query)).await else {
            return;
        };
        let names: HashMap<_, _> = profiles
            .into_iter()
            .filter(|profile| ids.contains(&profile.uuid))
            .filter_map(|profile| {
                let id = profile.uuid;
                profile
                    .into_profile(id)
                    .map(|profile| (id, profile.display_name))
            })
            .collect();
        for message in messages {
            message.author_name = names.get(&message.author_id).cloned();
        }
    }
}

/// Принимает только новый непустой UUID или подтверждённый конец коллекции.
fn next_marker(headers: &HeaderMap, previous: Option<Uuid>) -> Result<Option<Uuid>, SourceError> {
    headers
        .get("X-Pagination-Marker")
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| value.parse::<Uuid>().ok())
                .filter(|value| !value.is_nil() && Some(*value) != previous)
                .ok_or(SourceError::InvalidResponse)
        })
        .transpose()
}

impl WireActivityMessage {
    /// Принимает только ожидаемый проект, пользователя, фильтр и публичное размещение.
    fn into_activity(
        self,
        owner: &OwnerContext,
        filter: ActivityMessageFilter,
    ) -> Result<ActivityMessage, SourceError> {
        let row = self.message;
        check_explicit_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref())?;
        let matches = match filter {
            ActivityMessageFilter::Marked => self.starred,
            ActivityMessageFilter::Mentions => self.mentioned,
            ActivityMessageFilter::UnreadMentions => self.mentioned && !self.read,
        };
        if !matches
            || [row.uuid, row.stream_uuid, row.topic_uuid, row.author_uuid]
                .iter()
                .any(Uuid::is_nil)
        {
            return Err(SourceError::InvalidResponse);
        }
        let created_at = HistoryTime::parse(&row.created_at).ok_or(SourceError::InvalidResponse)?;
        let preview = match (row.payload.kind.as_str(), row.payload.content) {
            ("markdown", Some(Value::String(text))) => {
                ActivityMessagePreview::Text(MessagePreview::short(&text, 80))
            }
            ("markdown", _) => return Err(SourceError::InvalidResponse),
            ("e2ee", _) => ActivityMessagePreview::Encrypted,
            _ => ActivityMessagePreview::Unsupported,
        };
        Ok(ActivityMessage {
            id: row.uuid,
            stream_id: row.stream_uuid,
            topic_id: row.topic_uuid,
            author_id: row.author_uuid,
            author_name: None,
            created_at,
            preview,
            read: self.read,
        })
    }
}

#[cfg(test)]
mod tests;
