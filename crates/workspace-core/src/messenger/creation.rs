//! Открывает или создаёт личный разговор и создаёт канал с темой по умолчанию.
//! Здесь хранятся параметры повтора и проверяется поколение входа.

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    sync::Sync,
};

use super::MessengerSource;

/// Одна попытка создания канала с заранее выбранным UUID для безопасного повтора.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelCreation {
    pub(super) stream_id: Uuid, // Один UUID сохраняется на все повторы того же ввода.
    pub(super) name: String,    // Название канала без крайних пробелов.
    pub(super) description: String, // Описание без крайних пробелов, может быть пустым.
}

impl ChannelCreation {
    /// Проверяет длину полей в символах по Workspace API и назначает UUID операции.
    pub fn new(name: &str, description: &str) -> Result<Self, ConversationCreationError> {
        let name = name.trim();
        let description = description.trim();
        if name.is_empty() || name.chars().count() > 2048 || description.chars().count() > 255 {
            return Err(ConversationCreationError::InvalidInput);
        }
        Ok(Self {
            stream_id: Uuid::new_v4(),
            name: name.to_owned(),
            description: description.to_owned(),
        })
    }

    /// Проверяет, совпадает ли новый ввод с параметрами уже отправленной попытки.
    pub fn matches(&self, name: &str, description: &str) -> bool {
        self.name == name.trim() && self.description == description.trim()
    }
}

/// Подтверждённый новый канал и его начальная тема.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreatedChannel {
    pub stream_id: Uuid, // UUID канала для списка, истории и добавления участников.
    pub topic_id: Uuid,  // UUID темы по умолчанию, подтверждённый в исходной сессии.
}

/// Причина, по которой не удалось открыть или создать разговор.
#[derive(Debug, Error)]
pub enum ConversationCreationError {
    #[error("conversation fields are invalid")]
    InvalidInput, // Собеседник или поля формы не прошли локальную проверку.
    #[error("the direct conversation is archived")]
    Archived, // Существующую личку можно открыть или восстановить в архиве.
    #[error("workspace rejected conversation creation with HTTP {0}")]
    Rejected(u16), // Сервер определённо отклонил запрос.
    #[error("conversation creation outcome is unknown")]
    Unknown, // Стрим мог появиться. Повторяем операцию с той же парой или тем же UUID.
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход завершился или не смог выдать токен.
    #[error(transparent)]
    Source(#[from] SourceError), // Чтение существующего стрима не удалось до отправки команды.
}

impl MessengerSource {
    /// Открывает видимую личку или создаёт её по канонической паре пользователей.
    pub async fn open_or_create_direct(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        peer: Uuid,
    ) -> Result<Uuid, ConversationCreationError> {
        let current = Uuid::parse_str(&owner.user_id).map_err(|_| SourceError::InvalidResponse)?;
        if peer.is_nil() || peer == current {
            return Err(ConversationCreationError::InvalidInput);
        }
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let existing = self.rest.existing_direct(owner, &token, peer).await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        if let Some(stream) = existing? {
            return Ok(stream);
        }
        let result = self.rest.create_direct(owner, &token, peer).await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result
    }

    /// Создаёт канал или подтверждает результат повтора с тем же UUID и тем же вводом.
    pub async fn create_channel(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        attempt: &ChannelCreation,
    ) -> Result<CreatedChannel, ConversationCreationError> {
        let token = sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self.rest.create_channel(owner, &token, attempt).await;
        sync.auth()
            .with_active_generation(owner, generation, || ())?;
        result
    }
}
