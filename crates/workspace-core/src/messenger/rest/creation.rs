//! Проверяет REST-ответы создания стримов и повторяет форму публичного API.
//! Личка определяется парой пользователей, канал создаётся по UUID попытки.

use reqwest::{StatusCode, header::ACCEPT};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    messenger::creation::{ChannelCreation, ConversationCreationError, CreatedChannel},
    runtime::SourceError,
};

use super::{RestClient, STREAMS_PATH, WireStream, check_explicit_owner, workspace_origin};

/// Тело создания нативного стрима. Каждому сценарию соответствует свой ключ повтора.
#[derive(Serialize)]
struct NewStream<'a> {
    name: &'a str,             // У канала пользовательское имя, у лички служебное.
    description: &'a str,      // Описание канала или пустая строка.
    source_name: &'static str, // Нативный стрим Workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    uuid: Option<Uuid>, // UUID операции создания канала. Личке UUID назначает сервер.
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_user_uuid: Option<Uuid>, // Собеседник лички. У канала поле отсутствует.
}

impl RestClient {
    /// Использует видимый серверный снимок пары, чтобы сразу открыть существующую личку.
    pub(in crate::messenger) async fn existing_direct(
        &self,
        owner: &OwnerContext,
        token: &str,
        peer: Uuid,
    ) -> Result<Option<Uuid>, ConversationCreationError> {
        let peer_text = peer.to_string();
        let mut streams = self
            .filtered_collection::<WireStream>(
                owner,
                token,
                &format!("{STREAMS_PATH}/"),
                &[("direct_user_uuid", &peer_text), ("source_name", "native")],
            )
            .await?;
        let Some(stream) = streams.pop() else {
            return Ok(None);
        };
        if !streams.is_empty() {
            return Err(SourceError::InvalidResponse.into());
        }
        direct_stream_id(&stream, owner, peer).map(Some)
    }

    /// Отправляет пару пользователей и проверяет, что сервер вернул нужную личку.
    pub(in crate::messenger) async fn create_direct(
        &self,
        owner: &OwnerContext,
        token: &str,
        peer: Uuid,
    ) -> Result<Uuid, ConversationCreationError> {
        let stream = self
            .create_native_stream(
                owner,
                token,
                &NewStream {
                    name: "Direct",
                    description: "",
                    source_name: "native",
                    uuid: None,
                    direct_user_uuid: Some(peer),
                },
            )
            .await?;
        let stream_id = direct_stream_id(&stream, owner, peer).map_err(unknown_after_post)?;
        default_topic_id(&stream).ok_or(ConversationCreationError::Unknown)?;
        Ok(stream_id)
    }

    /// Создаёт канал с постоянным UUID попытки и проверяет ответ с начальной темой.
    pub(in crate::messenger) async fn create_channel(
        &self,
        owner: &OwnerContext,
        token: &str,
        attempt: &ChannelCreation,
    ) -> Result<CreatedChannel, ConversationCreationError> {
        let stream = self
            .create_native_stream(
                owner,
                token,
                &NewStream {
                    name: &attempt.name,
                    description: &attempt.description,
                    source_name: "native",
                    uuid: Some(attempt.stream_id),
                    direct_user_uuid: None,
                },
            )
            .await?;
        created_channel(&stream, owner, attempt.stream_id).ok_or(ConversationCreationError::Unknown)
    }

    /// Отправляет один POST. Неясный ответ сохраняет возможность безопасного повтора.
    async fn create_native_stream(
        &self,
        owner: &OwnerContext,
        token: &str,
        body: &NewStream<'_>,
    ) -> Result<WireStream, ConversationCreationError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(&format!("{STREAMS_PATH}/"));
        let response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|_| ConversationCreationError::Unknown)?;
        match response.status() {
            StatusCode::OK | StatusCode::CREATED => response
                .json()
                .await
                .map_err(|_| ConversationCreationError::Unknown),
            status
                if status.is_client_error()
                    && !matches!(
                        status,
                        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
                    ) =>
            {
                Err(ConversationCreationError::Rejected(status.as_u16()))
            }
            _ => Err(ConversationCreationError::Unknown),
        }
    }
}

/// Подтверждает проект, владельца ответа и UUID нативного стрима.
fn native_stream_id(stream: &WireStream, owner: &OwnerContext) -> Result<Uuid, SourceError> {
    check_explicit_owner(
        owner,
        stream.project_id.as_deref(),
        stream.user_uuid.as_deref(),
    )?;
    if stream.source_name.as_deref() != Some("native") {
        return Err(SourceError::InvalidResponse);
    }
    Uuid::parse_str(&stream.uuid)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or(SourceError::InvalidResponse)
}

/// Отличает личку нужной пары от посторонней строки и существующего архива.
fn direct_stream_id(
    stream: &WireStream,
    owner: &OwnerContext,
    peer: Uuid,
) -> Result<Uuid, ConversationCreationError> {
    let id = native_stream_id(stream, owner)?;
    if !stream.private
        || stream
            .direct_user_uuid
            .as_deref()
            .and_then(super::parse_uuid)
            != Some(peer)
    {
        return Err(SourceError::InvalidResponse.into());
    }
    if stream.is_archived {
        return Err(ConversationCreationError::Archived);
    }
    Ok(id)
}

/// Подтверждает канал и тему, которые выставил сервер по UUID нашей попытки.
fn created_channel(
    stream: &WireStream,
    owner: &OwnerContext,
    expected: Uuid,
) -> Option<CreatedChannel> {
    let stream_id = native_stream_id(stream, owner).ok()?;
    if stream_id != expected
        || stream.private
        || stream.direct_user_uuid.is_some()
        || stream.is_archived
    {
        return None;
    }
    Some(CreatedChannel {
        stream_id,
        topic_id: default_topic_id(stream)?,
    })
}

/// Берёт непустой UUID темы по умолчанию из ответа создания.
fn default_topic_id(stream: &WireStream) -> Option<Uuid> {
    stream
        .default_topic_uuid
        .as_ref()?
        .as_str()
        .and_then(super::parse_uuid)
        .filter(|id| !id.is_nil())
}

/// Помечает результат записи неясным, если её ответ не удалось подтвердить.
fn unknown_after_post(error: ConversationCreationError) -> ConversationCreationError {
    match error {
        ConversationCreationError::Archived => error,
        _ => ConversationCreationError::Unknown,
    }
}

#[cfg(test)]
mod tests;
