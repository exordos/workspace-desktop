//! Читает коллекции отмеченных сообщений и упоминаний для подтверждённого входа.

use std::collections::HashSet;

use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    storage::HistoryTime,
    sync::Sync,
};

use super::super::{MessengerSource, catalog::MessagePreview};

const PAGE_SIZE: usize = 50;
const UNREAD_PAGE_SIZE: usize = 100;

/// Серверный фильтр списка сообщений активности.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityMessageFilter {
    Marked,         // Сообщения, отмеченные текущим пользователем.
    Mentions,       // Все сообщения с упоминанием текущего пользователя.
    UnreadMentions, // Упоминания, которые текущий пользователь ещё не прочитал.
}

/// Безопасное краткое содержимое, нужное заголовку карточки.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityMessagePreview {
    Text(MessagePreview), // Короткие части Markdown без адресов вложений.
    Encrypted,    // Зашифрованное сообщение, его текст запрашивается только в разговоре.
    Unsupported,  // Формат сообщения пока не отображается в карточке.
}

/// Строка списка с серверным адресом исходного сообщения.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityMessage {
    pub id: Uuid,                        // Публичный UUID размещения сообщения.
    pub stream_id: Uuid,                 // UUID разговора.
    pub topic_id: Uuid,                  // UUID темы, где лежит сообщение.
    pub author_id: Uuid,                 // UUID автора сообщения.
    pub author_name: Option<String>,     // Имя из справочника, если удалось получить.
    pub created_at: HistoryTime,         // Серверное время для порядка и подписи.
    pub preview: ActivityMessagePreview, // Короткое описание для списка.
    pub read: bool,                      // Прочитал ли сообщение текущий пользователь.
}

/// Принятая страница коллекции и позиция продолжения того же фильтра.
#[derive(Debug)]
pub struct ActivityMessagePage {
    pub messages: Vec<ActivityMessage>, // Сообщения от новых к старым.
    pub next: Option<Uuid>,             // UUID маркера сервера. None означает конец коллекции.
}

/// Причина, по которой не удалось прочитать список.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ActivityMessageError {
    #[error("the sign-in is no longer active")]
    InactiveSession, // Аккаунт вышел либо активен другой вход.
    #[error("the activity messages could not be loaded")]
    Unavailable, // Сервер или сеть не позволили получить список.
    #[error("the activity page marker expired")]
    ExpiredMarker, // Строка маркера больше не относится к коллекции.
    #[error("access to the activity collection was denied")]
    AccessLost, // Сервер подтвердил, что коллекция недоступна.
}

impl From<SourceError> for ActivityMessageError {
    /// Отличает явный отзыв доступа и просроченный маркер от временного отказа.
    fn from(error: SourceError) -> Self {
        match error {
            SourceError::Forbidden => Self::AccessLost,
            SourceError::CursorPruned => Self::ExpiredMarker,
            _ => Self::Unavailable,
        }
    }
}

impl From<AuthError> for ActivityMessageError {
    /// Отличает устаревший вход от временной ошибки доступа.
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::SessionUnavailable
            | AuthError::ReauthenticationRequired
            | AuthError::LogoutPending => Self::InactiveSession,
            _ => Self::Unavailable,
        }
    }
}

impl MessengerSource {
    /// Загружает страницу и проверяет, что ответ относится к действующему входу.
    pub async fn activity_messages(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        filter: ActivityMessageFilter,
        marker: Option<Uuid>,
    ) -> Result<ActivityMessagePage, ActivityMessageError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let mut page = self
            .rest
            .activity_messages(owner, &token, filter, marker, PAGE_SIZE)
            .await
            .map_err(|error| {
                log::debug!("event=messenger_activity_page_failed cause={error}");
                ActivityMessageError::from(error)
            })?;
        auth.with_active_generation(owner, generation, || ())?;
        self.rest
            .activity_author_names(owner, &token, &mut page.messages)
            .await;
        Ok(auth.with_active_generation(owner, generation, || page)?)
    }

    /// Считает уникальные непрочитанные упоминания по всем страницам коллекции.
    pub async fn unread_activity_mentions(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<u64, ActivityMessageError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let mut ids = HashSet::new();
        let mut cursors = HashSet::new();
        let mut marker = None;
        loop {
            let page = self
                .rest
                .activity_messages(
                    owner,
                    &token,
                    ActivityMessageFilter::UnreadMentions,
                    marker,
                    UNREAD_PAGE_SIZE,
                )
                .await
                .map_err(|error| {
                    log::debug!("event=messenger_activity_count_failed cause={error}");
                    ActivityMessageError::from(error)
                })?;
            auth.with_active_generation(owner, generation, || ())?;
            ids.extend(page.messages.into_iter().map(|message| message.id));
            let Some(next) = page.next else {
                return Ok(ids.len() as u64);
            };
            if !cursors.insert(next) {
                return Err(ActivityMessageError::Unavailable);
            }
            marker = Some(next);
        }
    }
}
