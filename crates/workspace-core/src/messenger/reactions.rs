//! Переключает реакцию владельца на подтверждённое сообщение.
//! Сверяет серверную запись, применяет ответ в историю и учитывает удачные добавления.

use std::{collections::HashSet, sync::Mutex};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{Auth, AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    sync::{Sync, SyncError},
};

use super::{
    MessengerSource, history::OpenHistoryError, message_actions::MessageTarget,
    quick_reactions::canonical_reaction_name,
};

/// Как сервер изменил реакцию владельца.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReactionToggle {
    Added,   // Владелец поставил реакцию.
    Removed, // Владелец снял реакцию.
}

/// Причина, по которой не удалось переключить реакцию.
#[derive(Debug, Error)]
pub enum ReactionError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход завершился или сменился во время команды.
    #[error(transparent)]
    History(#[from] OpenHistoryError), // Сообщение или его разговор больше недоступны.
    #[error(transparent)]
    Source(#[from] SourceError), // Сервер вернул сбой или неподходящий ответ.
    #[error("invalid reaction target or emoji name")]
    InvalidInput, // Неподходящий адрес, тип сообщения или имя эмодзи.
    #[error("the reaction command is already pending")]
    Pending, // Для этого эмодзи и сообщения уже выполняется команда.
    #[error("the server rejected a reaction with HTTP {0}")]
    Rejected(u16), // Мутация отклонена сервером.
}

impl MessengerSource {
    /// Переключает факт текущего пользователя в точном размещении открытой истории.
    pub async fn toggle_message_reaction(
        &self,
        auth: &Auth,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        target: MessageTarget,
        emoji: &str,
    ) -> Result<ReactionToggle, ReactionError> {
        let emoji = emoji.trim();
        if !target.is_valid() || !valid_reaction_emoji(emoji) {
            return Err(ReactionError::InvalidInput);
        }
        let canonical = canonical_reaction_name(emoji);
        let _pending =
            self.reaction_requests
                .begin(owner, generation, target.message_id, &canonical)?;
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let validated = self.rest.check_reaction_target(owner, &token, target).await;
        auth.with_active_generation(owner, generation, || ())?;
        validated?;

        let mut existing = self
            .rest
            .own_reaction(owner, &token, target.message_id, &canonical)
            .await;
        auth.with_active_generation(owner, generation, || ())?;
        if matches!(existing, Ok(None)) {
            let added = self
                .rest
                .add_reaction(owner, &token, target.message_id, emoji, &canonical)
                .await;
            auth.with_active_generation(owner, generation, || ())?;
            match added {
                Ok(confirmed) => {
                    self.apply_reaction_confirmation(
                        sync, owner, generation, target, &confirmed, true,
                    )?;
                    if let Err(error) = sync.record_quick_reaction(owner, generation, &canonical) {
                        log::debug!("Messenger quick reaction usage: {error}");
                    }
                    return Ok(ReactionToggle::Added);
                }
                Err(ReactionError::Rejected(409)) => {
                    // Другое устройство могло создать реакцию после чтения.
                    existing = self
                        .rest
                        .own_reaction(owner, &token, target.message_id, &canonical)
                        .await;
                    auth.with_active_generation(owner, generation, || ())?;
                }
                Err(error) => return Err(error),
            }
        }
        let (reaction_id, confirmed) = existing?.ok_or(ReactionError::Rejected(409))?;
        let removed = self.rest.remove_reaction(owner, &token, reaction_id).await;
        auth.with_active_generation(owner, generation, || ())?;
        removed?;
        self.apply_reaction_confirmation(sync, owner, generation, target, &confirmed, false)?;
        Ok(ReactionToggle::Removed)
    }

    /// Применяет подтверждение к кэшу, оставляя серверный итог при сбое локальной базы.
    fn apply_reaction_confirmation(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        target: MessageTarget,
        emoji: &str,
        selected: bool,
    ) -> Result<(), ReactionError> {
        match sync.confirm_message_reaction(owner, generation, target, emoji, selected) {
            Ok(()) => Ok(()),
            Err(SyncError::Auth(error)) => Err(error.into()),
            Err(error) => {
                log::debug!("Messenger reaction confirmation cache: {error}");
                Ok(())
            }
        }
    }
}

/// Проверяет лимит API и запрещает пустой ключ или пробелы по краям.
pub(super) fn valid_reaction_emoji(emoji: &str) -> bool {
    !emoji.is_empty() && emoji == emoji.trim() && emoji.chars().count() <= 128
}

/// Выделяет одну команду по владельцу, входу, сообщению и каноническому эмодзи.
#[derive(Clone, Eq, Hash, PartialEq)]
struct ReactionRequest {
    owner: OwnerContext,           // Владелец и проект команды.
    generation: SessionGeneration, // Поколение входа.
    id: Uuid,                      // Публичный UUID размещения.
    emoji: String,                 // Каноническое имя эмодзи.
}

/// Не даёт повторному клику запустить вторую команду, пока первая ждёт сеть.
#[derive(Default)]
pub(super) struct ReactionRequests(Mutex<HashSet<ReactionRequest>>); // Ключи команд, ожидающих сеть.

impl ReactionRequests {
    /// Зарегистрирует команду до завершения или отмены сетевого запроса.
    fn begin(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
        id: Uuid,
        emoji: &str,
    ) -> Result<PendingReaction<'_>, ReactionError> {
        let request = ReactionRequest {
            owner: owner.clone(),
            generation,
            id,
            emoji: emoji.to_owned(),
        };
        let mut active = self.0.lock().map_err(|_| SourceError::Temporary)?;
        if !active.insert(request.clone()) {
            return Err(ReactionError::Pending);
        }
        Ok(PendingReaction {
            requests: self,
            request,
        })
    }
}

/// Убирает занятый ключ, даже если операция отменена вместе с экраном.
struct PendingReaction<'a> {
    requests: &'a ReactionRequests, // Общий набор активных команд источника.
    request: ReactionRequest,       // Ключ этой команды.
}

impl Drop for PendingReaction<'_> {
    /// Освобождает ключ команды для следующего клика.
    fn drop(&mut self) {
        if let Ok(mut active) = self.requests.0.lock() {
            active.remove(&self.request);
        }
    }
}
