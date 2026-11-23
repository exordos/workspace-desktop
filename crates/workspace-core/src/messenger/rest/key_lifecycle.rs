//! Создаёт нативный защищённый стрим и проверяет серверный результат.
//! Также читает владельца, режим и точную тему для объявления нового ключа.

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    ACCEPT, RestClient, STREAMS_PATH, SourceError, StatusCode, TOPICS_PATH, WireStream,
    check_explicit_owner, check_status, parse_uuid, request_error,
    sidebar::{BINDINGS_PATH, WireBinding},
    workspace_origin,
};
use crate::{
    context::OwnerContext,
    messenger::{
        encryption::{
            key_exchange::KeyRequestTarget,
            lifecycle::{EncryptedChannelInput, KeyLifecycleError},
        },
        history::OpenHistoryError,
        send::SendError,
    },
    storage::HistoryScope,
};

/// Минимальная проекция темы для сверки старого ответа без признака темы по умолчанию в стриме.
#[derive(Deserialize)]
struct DefaultTopicRow {
    uuid: Uuid,         // UUID темы.
    stream_uuid: Uuid,  // Стрим темы для проверки области ответа.
    project_id: String, // Проект из проекции.
    user_uuid: Uuid,    // Владелец проекции.
    is_default: bool,   // Тема назначена сервером по умолчанию.
}

impl RestClient {
    /// Отправляет единственный POST нативного канала с заранее закреплёнными UUID.
    pub(in crate::messenger) async fn create_encrypted_stream_once(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream_id: Uuid,
        key_id: Uuid,
        input: &EncryptedChannelInput,
    ) -> Result<(), KeyLifecycleError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(&format!("{STREAMS_PATH}/"));
        let response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .json(&json!({
                "uuid": stream_id, "name": input.name, "description": input.description,
                "source_name": "native", "invite_only": false, "announce": false,
                "encryption": true, "current_encryption_key_uuid": key_id,
            }))
            .send()
            .await
            .map_err(|_| SendError::Unknown)?;
        let status = response.status();
        // Конфликт может означать, что стрим с закреплённым UUID уже существует.
        if status.is_client_error()
            && !matches!(
                status,
                StatusCode::REQUEST_TIMEOUT | StatusCode::CONFLICT | StatusCode::TOO_MANY_REQUESTS
            )
        {
            return Err(SendError::Rejected(status.as_u16()).into());
        }
        if status != StatusCode::CREATED {
            return Err(SendError::Unknown.into());
        }
        let stream: WireStream = response.json().await.map_err(|_| SendError::Unknown)?;
        check_creation_stream(&stream, owner, stream_id, Some(key_id))
    }

    /// Проверяет точный стрим владельца и возвращает его подтверждённую тему по умолчанию.
    pub(in crate::messenger) async fn confirmed_encrypted_creation(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream_id: Uuid,
        key_id: Option<Uuid>,
    ) -> Result<Uuid, KeyLifecycleError> {
        let response = self
            .request(owner, token, &format!("{STREAMS_PATH}/{stream_id}"))?
            .send()
            .await
            .map_err(request_error)?;
        match response.status() {
            StatusCode::NOT_FOUND => return Err(KeyLifecycleError::Uncertain),
            StatusCode::FORBIDDEN => return Err(KeyLifecycleError::NotOwner),
            status => check_status(status)?,
        }
        let row: WireStream = response.json().await.map_err(request_error)?;
        check_creation_stream(&row, owner, stream_id, key_id)?;
        self.lifecycle_default_topic(owner, token, stream_id, &row)
            .await
    }

    /// Возвращает отсутствующих участников для приглашения под действующим входом.
    pub(in crate::messenger) async fn missing_encrypted_channel_members(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream_id: Uuid,
        members: &[Uuid],
    ) -> Result<Vec<Uuid>, KeyLifecycleError> {
        if members.is_empty() {
            return Ok(Vec::new());
        }
        let stream = stream_id.to_string();
        let bindings = self
            .filtered_collection::<WireBinding>(
                owner,
                token,
                BINDINGS_PATH,
                &[("stream_uuid", &stream)],
            )
            .await?;
        if bindings.iter().any(|binding| {
            binding.project_id != owner.project_id
                || binding.stream_uuid != stream_id
                || binding.uuid.is_nil()
                || binding.user_uuid.is_nil()
        }) {
            return Err(SourceError::InvalidResponse.into());
        }
        Ok(members
            .iter()
            .copied()
            .filter(|member| !bindings.iter().any(|binding| binding.user_uuid == *member))
            .collect())
    }

    /// Подтверждает owner-only политику, точное размещение и текущее поколение перед ротацией.
    pub(in crate::messenger) async fn key_rotation_destination(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
    ) -> Result<(Uuid, Uuid), KeyLifecycleError> {
        let (topic_id, stream) = self.lifecycle_key_destination(owner, token, scope).await?;
        let key_id = protected_stream_owner(&stream, owner, scope.stream_id)?;
        Ok((topic_id, key_id))
    }

    /// Читает текущее поколение и право владельца, проверяя членство обычного участника.
    pub(in crate::messenger) async fn local_keys_route(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
    ) -> Result<(Uuid, Uuid, bool), KeyLifecycleError> {
        let (topic_id, stream) = self.lifecycle_key_destination(owner, token, scope).await?;
        let (current, is_owner) = protected_stream_generation(&stream, owner, scope.stream_id)?;
        if !is_owner {
            let user = parse_uuid(&owner.user_id).ok_or(SourceError::InvalidResponse)?;
            let missing = self
                .missing_encrypted_channel_members(owner, token, scope.stream_id, &[user])
                .await?;
            if !missing.is_empty() {
                return Err(OpenHistoryError::AccessLost.into());
            }
        }
        Ok((topic_id, current, is_owner))
    }

    /// Подтверждает точное размещение операции или тему по умолчанию.
    async fn lifecycle_key_destination(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
    ) -> Result<(Uuid, WireStream), KeyLifecycleError> {
        let topic_id = match scope.topic_id {
            Some(topic) => topic,
            None => {
                let stream = self.stream_access(owner, token, scope.stream_id).await?;
                protected_stream_generation(&stream, owner, scope.stream_id)?;
                self.lifecycle_default_topic(owner, token, scope.stream_id, &stream)
                    .await?
            }
        };
        let stream = self
            .key_service_stream(
                owner,
                token,
                KeyRequestTarget {
                    stream_id: scope.stream_id,
                    topic_id,
                    key_id: Uuid::nil(),
                },
            )
            .await?;
        Ok((topic_id, stream))
    }

    /// Предпочитает точный UUID из стрима, старые ответы сверяет по признаку темы.
    async fn lifecycle_default_topic(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream_id: Uuid,
        stream: &WireStream,
    ) -> Result<Uuid, KeyLifecycleError> {
        let topic = stream
            .default_topic_uuid
            .as_ref()
            .and_then(Value::as_str)
            .and_then(parse_uuid)
            .filter(|id| !id.is_nil());
        if let Some(topic_id) = topic {
            let row = self.topic_access(owner, token, stream_id, topic_id).await?;
            check_explicit_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref())?;
            return Ok(topic_id);
        }
        let expected_user = parse_uuid(&owner.user_id).ok_or(SourceError::InvalidResponse)?;
        let stream = stream_id.to_string();
        let topics = self
            .filtered_collection::<DefaultTopicRow>(
                owner,
                token,
                &format!("{TOPICS_PATH}/"),
                &[("stream_uuid", &stream)],
            )
            .await?;
        if topics.iter().any(|topic| {
            topic.stream_uuid != stream_id
                || topic.project_id != owner.project_id
                || topic.user_uuid != expected_user
                || topic.uuid.is_nil()
        }) {
            return Err(SourceError::InvalidResponse.into());
        }
        let mut defaults = topics.into_iter().filter(|topic| topic.is_default);
        let topic = defaults.next().ok_or(KeyLifecycleError::Uncertain)?;
        if defaults.next().is_some() {
            return Err(SourceError::InvalidResponse.into());
        }
        Ok(topic.uuid)
    }
}

/// Проверяет канонического владельца и формат уже существующего защищённого стрима.
fn protected_stream_owner(
    stream: &WireStream,
    owner: &OwnerContext,
    stream_id: Uuid,
) -> Result<Uuid, KeyLifecycleError> {
    let (key_id, is_owner) = protected_stream_generation(stream, owner, stream_id)?;
    if !is_owner {
        return Err(KeyLifecycleError::NotOwner);
    }
    Ok(key_id)
}

/// Проверяет принадлежность проекции и читает точный текущий UUID вместе с признаком владельца.
fn protected_stream_generation(
    stream: &WireStream,
    owner: &OwnerContext,
    stream_id: Uuid,
) -> Result<(Uuid, bool), KeyLifecycleError> {
    check_explicit_owner(
        owner,
        stream.project_id.as_deref(),
        stream.user_uuid.as_deref(),
    )?;
    let expected_owner = parse_uuid(&owner.user_id)
        .filter(|id| !id.is_nil())
        .ok_or(SourceError::InvalidResponse)?;
    if parse_uuid(&stream.uuid) != Some(stream_id)
        || stream.is_archived
        || stream.source_name.as_deref() != Some("native")
        || stream.encryption != Some(Value::Bool(true))
    {
        return Err(KeyLifecycleError::NotOwner);
    }
    let actual_owner = stream
        .owner
        .as_deref()
        .and_then(parse_uuid)
        .filter(|id| !id.is_nil())
        .ok_or(SourceError::InvalidResponse)?;
    let key_id = stream
        .current_encryption_key_uuid
        .as_ref()
        .and_then(Value::as_str)
        .and_then(parse_uuid)
        .filter(|id| !id.is_nil())
        .ok_or(KeyLifecycleError::Changed)?;
    Ok((key_id, actual_owner == expected_owner))
}

/// Отличает подтверждение нашего создания от другого канала или изменения исходного ключа.
fn check_creation_stream(
    stream: &WireStream,
    owner: &OwnerContext,
    stream_id: Uuid,
    key_id: Option<Uuid>,
) -> Result<(), KeyLifecycleError> {
    let active = protected_stream_owner(stream, owner, stream_id)?;
    if stream.private
        || stream.direct_user_uuid.is_some()
        || key_id.is_some_and(|expected| expected != active)
    {
        return Err(KeyLifecycleError::Changed);
    }
    Ok(())
}
