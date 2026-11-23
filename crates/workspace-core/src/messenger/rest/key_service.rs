//! Читает и однократно отправляет служебные сообщения о ключах нативного чата.
//! Маршрут, владелец, публичное размещение и тело подтверждаются перед возвратом в обмен ключами.

use std::collections::{HashMap, HashSet};

use super::{
    ACCEPT, MESSAGES_PATH, RestClient, SourceError, StatusCode, WireStream, check_explicit_owner,
    check_status,
    sidebar::{BINDINGS_PATH, WireBinding},
    workspace_origin,
};
use crate::{
    context::OwnerContext,
    messenger::{
        encryption::{
            key_exchange::KeyRequestTarget,
            key_service::{KeyServiceMessage, KeyServicePayload, KeySharingPolicy},
        },
        history::OpenHistoryError,
        send::SendError,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

impl RestClient {
    /// Подтверждает принадлежность нативного разговора и темы для операции ключа.
    pub(in crate::messenger) async fn check_key_service_target(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: KeyRequestTarget,
    ) -> Result<(), OpenHistoryError> {
        self.key_service_stream(owner, token, target)
            .await
            .map(|_| ())
    }

    /// Перечитывает права и политику до проверки автора выдачи или отказа.
    pub(in crate::messenger) async fn key_sharing_policy(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: KeyRequestTarget,
    ) -> Result<KeySharingPolicy, OpenHistoryError> {
        let stream = self.key_service_stream(owner, token, target).await?;
        let stream_id = target.stream_id.to_string();
        let bindings = self
            .filtered_collection::<WireBinding>(
                owner,
                token,
                BINDINGS_PATH,
                &[("stream_uuid", stream_id.as_str())],
            )
            .await?;
        let mut roles = HashMap::new();
        let mut binding_ids = HashSet::new();
        for binding in bindings {
            if binding.project_id != owner.project_id
                || binding.stream_uuid != target.stream_id
                || binding.user_uuid.is_nil()
                || binding.uuid.is_nil()
                || !binding_ids.insert(binding.uuid)
                || roles.insert(binding.user_uuid, binding.role).is_some()
            {
                return Err(SourceError::InvalidResponse.into());
            }
        }
        let protected = stream.encryption == Some(Value::Bool(true));
        let stream_owner = stream
            .owner
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| SourceError::InvalidResponse)?;
        Ok(KeySharingPolicy {
            stream_owner,
            protected,
            holders: stream.allow_key_sharing_by_holders,
            roles,
        })
    }

    /// Подтверждает явного владельца ответа, тип стрима и исходную тему.
    pub(super) async fn key_service_stream(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: KeyRequestTarget,
    ) -> Result<WireStream, OpenHistoryError> {
        let stream = self.stream_access(owner, token, target.stream_id).await?;
        check_explicit_owner(
            owner,
            stream.project_id.as_deref(),
            stream.user_uuid.as_deref(),
        )?;
        if stream.is_archived
            || stream.source_name.as_deref() != Some("native")
            || !matches!(stream.encryption, Some(Value::Bool(_)))
        {
            return Err(OpenHistoryError::AccessLost);
        }
        let topic = self
            .topic_access(owner, token, target.stream_id, target.topic_id)
            .await?;
        check_explicit_owner(
            owner,
            topic.project_id.as_deref(),
            topic.user_uuid.as_deref(),
        )?;
        Ok(stream)
    }

    /// Ищет публичное размещение запроса при восстановлении неизвестного результата POST.
    pub(in crate::messenger) async fn lookup_key_service_message(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: KeyRequestTarget,
        public_id: Uuid,
    ) -> Result<Option<KeyServiceMessage>, SendError> {
        match self
            .key_service_projection(owner, token, target, public_id)
            .await?
        {
            KeyServiceProjection::Missing => Ok(None),
            KeyServiceProjection::Malformed => Err(SendError::Unknown),
            KeyServiceProjection::Found(message) => Ok(Some(message)),
        }
    }

    /// Читает возможный ответ из локального индекса. Чужая или неверная проекция не отвечает на запрос.
    pub(in crate::messenger) async fn lookup_key_reply_candidate(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: KeyRequestTarget,
        public_id: Uuid,
    ) -> Result<Option<KeyServiceMessage>, SendError> {
        match self
            .key_service_projection(owner, token, target, public_id)
            .await?
        {
            KeyServiceProjection::Missing | KeyServiceProjection::Malformed => Ok(None),
            KeyServiceProjection::Found(message) => Ok(Some(message)),
        }
    }

    /// Разделяет подтверждённое отсутствие, чужую проекцию и временный сбой чтения.
    async fn key_service_projection(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: KeyRequestTarget,
        public_id: Uuid,
    ) -> Result<KeyServiceProjection, SendError> {
        let request = self
            .request(owner, token, &format!("{MESSAGES_PATH}{public_id}"))
            .map_err(|_| SendError::InvalidInput)?;
        let response = request.send().await.map_err(|_| SendError::Unknown)?;
        match response.status() {
            StatusCode::NOT_FOUND => return Ok(KeyServiceProjection::Missing),
            StatusCode::FORBIDDEN => return Ok(KeyServiceProjection::Malformed),
            status => check_status(status).map_err(|_| SendError::Unknown)?,
        }
        let row: WireKeyServiceMessage = match response.json().await {
            Ok(row) => row,
            Err(error) if error.is_decode() => return Ok(KeyServiceProjection::Malformed),
            Err(_) => return Err(SendError::Unknown),
        };
        Ok(match row.into_message(owner, target, public_id) {
            Ok(message) => KeyServiceProjection::Found(message),
            Err(_) => KeyServiceProjection::Malformed,
        })
    }

    /// Отправляет служебное сообщение одним POST по заранее сохранённому UUID.
    pub(in crate::messenger) async fn post_key_service_once(
        &self,
        owner: &OwnerContext,
        token: &str,
        target: KeyRequestTarget,
        canonical_id: Uuid,
        payload: &KeyServicePayload,
    ) -> Result<KeyServiceMessage, SendError> {
        if canonical_id.is_nil() || payload.key_id() != target.key_id {
            return Err(SendError::InvalidInput);
        }
        let mut url = workspace_origin(owner).map_err(|_| SendError::InvalidInput)?;
        url.set_path(MESSAGES_PATH);
        let response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .json(&json!({
                "uuid": canonical_id,
                "stream_uuid": target.stream_id,
                "topic_uuid": target.topic_id,
                "payload": payload,
            }))
            .send()
            .await
            .map_err(|_| SendError::Unknown)?;
        let status = response.status();
        if status.is_client_error()
            && status != StatusCode::REQUEST_TIMEOUT
            && status != StatusCode::TOO_MANY_REQUESTS
        {
            return Err(SendError::Rejected(status.as_u16()));
        }
        if status != StatusCode::CREATED {
            return Err(SendError::Unknown);
        }
        let row: WireKeyServiceMessage = response.json().await.map_err(|_| SendError::Unknown)?;
        row.into_message(owner, target, target.public_id(canonical_id))
    }
}

/// Подтверждённое сервером состояние точного адреса после успешного чтения.
enum KeyServiceProjection {
    Missing,                  // Сервер вернул 404.
    Malformed, // Проекция недоступна владельцу или не соответствует адресу и формату.
    Found(KeyServiceMessage), // Точная строка в проекции текущего входа.
}

/// Серверная проекция размещения с типизированным служебным телом.
#[derive(Deserialize)]
struct WireKeyServiceMessage {
    uuid: Uuid,                  // Публичный UUID размещения.
    stream_uuid: Uuid,           // Стрим из подтверждения сервера.
    topic_uuid: Uuid,            // Тема из подтверждения сервера.
    project_id: Option<String>,  // Проект проекции ответа.
    user_uuid: Option<String>,   // Владелец проекции, а не автор сообщения.
    author_uuid: Uuid,           // Автор, от имени которого сервер сохранил сообщение.
    source_name: Option<String>, // Нативный источник.
    payload: KeyServicePayload,  // Поля служебного тела.
}

impl WireKeyServiceMessage {
    /// Пропускает только точное размещение в проекции текущего владельца.
    fn into_message(
        self,
        owner: &OwnerContext,
        target: KeyRequestTarget,
        public_id: Uuid,
    ) -> Result<KeyServiceMessage, SendError> {
        check_explicit_owner(owner, self.project_id.as_deref(), self.user_uuid.as_deref())
            .map_err(|_: SourceError| SendError::Unknown)?;
        if self.uuid != public_id
            || self.stream_uuid != target.stream_id
            || self.topic_uuid != target.topic_id
            || self.source_name.as_deref() != Some("native")
            || self.payload.key_id() != target.key_id
        {
            return Err(SendError::Unknown);
        }
        Ok(KeyServiceMessage {
            id: self.uuid,
            stream_id: self.stream_uuid,
            topic_id: self.topic_uuid,
            author_id: self.author_uuid,
            payload: self.payload,
        })
    }
}
