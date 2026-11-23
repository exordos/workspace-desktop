//! Отправляет подготовленный снимок сообщения одной попыткой и возвращает результат для сверки.

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    storage::{DraftScope, HistoryMessage, HistoryScope, PreparedSend},
    sync::Sync,
};

use super::{MessengerSource, history::OpenHistoryError};

/// Вид проверенного источника Markdown.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum MarkdownOrigin {
    Native,   // Нативный чат явно подтверждён сервером.
    External, // Сервер явно указал внешний источник и владельца.
    Unknown,  // Старый ответ разрешает обычную отправку, происхождение не доказано.
}

/// Проверенный адрес обычной отправки со сведениями об источнике.
#[derive(Clone)]
pub struct PlainSendTarget {
    owner: OwnerContext,           // Пользователь и проект ответа.
    generation: SessionGeneration, // Вход, который получил подтверждение.
    scope: DraftScope,             // Точная серверная тема записи.
    origin: MarkdownOrigin,        // Доступность защиты фрагментов или буквальных маркеров.
}

impl PlainSendTarget {
    /// Возвращает проверенную тему записи.
    pub fn topic_id(&self) -> Uuid {
        self.scope.topic_id
    }

    /// Разрешает ввод с защищёнными диапазонами в подтверждённом нативном чате.
    pub fn fragments_allowed(&self) -> bool {
        self.origin == MarkdownOrigin::Native
    }

    /// Сохраняет значение маркера как обычного текста у подтверждённого внешнего провайдера.
    pub fn literal_markers_allowed(&self) -> bool {
        self.origin == MarkdownOrigin::External
    }

    /// Проверяет, что разрешение относится к переданному владельцу, входу и теме.
    pub(crate) fn address(&self) -> (&OwnerContext, SessionGeneration, DraftScope) {
        (&self.owner, self.generation, self.scope)
    }
}

/// Причина, по которой нельзя подготовить открытый текст для разговора.
#[derive(Debug, Error)]
pub enum DirectSendError {
    #[error("the conversation or topic is unavailable")]
    AccessLost, // Личка или тема недоступна владельцу.
    #[error("the server did not allow unencrypted messages")]
    EncryptionUnavailable, // Состояние шифрования неизвестно или шифрование включено.
    #[error("the server did not provide a valid send topic")]
    TopicUnavailable, // Личный стрим не объявил пригодную тему для записи.
    #[error("the topic is closed for sending")]
    TopicClosed, // Тема канала завершена.
    #[error(transparent)]
    Auth(#[from] AuthError), // Поколение сессии изменилось или не удалось получить токен.
    #[error(transparent)]
    Source(#[from] SourceError), // Запрос к мессенджеру не завершился корректно.
}

impl From<OpenHistoryError> for DirectSendError {
    /// Переиспользует результат проверки доступа, общей с открытием истории.
    fn from(error: OpenHistoryError) -> Self {
        match error {
            OpenHistoryError::AccessLost => Self::AccessLost,
            OpenHistoryError::Auth(error) => Self::Auth(error),
            OpenHistoryError::Source(error) => Self::Source(error),
            _ => Self::Source(SourceError::InvalidResponse),
        }
    }
}

/// Результат попытки, который определяет дальнейшую сверку очереди.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SendError {
    #[error("invalid prepared message or server origin")]
    InvalidInput, // Запрос не был отправлен: адрес или идентификаторы неверны.
    #[error("encrypted message key is unavailable for sending")]
    EncryptionUnavailable, // Снимок ещё не отправили, локальный ключ недоступен.
    #[error("server rejected the request with HTTP {0}")]
    Rejected(u16), // Получен однозначный отказ API с указанным HTTP-статусом.
    #[error("delivery outcome is unknown")]
    Unknown, // Сервер мог принять запрос; требуется сверка, а не автоматический повтор.
}

impl MessengerSource {
    /// Подтверждает обычную цель и происхождение Markdown одним запросом уже нужных данных.
    pub async fn resolve_plain_send(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        scope: HistoryScope,
    ) -> Result<PlainSendTarget, DirectSendError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let resolved = if let Some(topic_id) = scope.topic_id {
            self.rest
                .check_markdown_topic(owner, &token, scope.stream_id, topic_id, false)
                .await
                .map(|origin| (topic_id, origin))
        } else {
            self.rest
                .direct_markdown_target(owner, &token, scope.stream_id)
                .await
        };
        auth.with_active_generation(owner, generation, || ())?;
        let (topic_id, origin) = resolved?;
        Ok(PlainSendTarget {
            owner: owner.clone(),
            generation,
            scope: DraftScope {
                stream_id: scope.stream_id,
                topic_id,
            },
            origin,
        })
    }

    /// Возвращает явную тему лички из подтверждённого ответа для того же поколения.
    pub async fn resolve_direct_send(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
    ) -> Result<Uuid, DirectSendError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let topic = self.rest.direct_send_topic(owner, &token, stream_id).await;
        auth.with_active_generation(owner, generation, || ())?;
        topic
    }

    /// Подтверждает явную тему канала для открытия редактора или записи сообщения в очередь.
    pub async fn check_topic_send(
        &self,
        sync: &Sync,
        owner: &OwnerContext,
        generation: SessionGeneration,
        stream_id: Uuid,
        topic_id: Uuid,
    ) -> Result<(), DirectSendError> {
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let result = self
            .rest
            .check_send_topic(owner, &token, stream_id, topic_id, false)
            .await;
        auth.with_active_generation(owner, generation, || ())?;
        result
    }

    /// Ищет подтверждение доставки по публичному UUID, не изменяя очередь.
    pub async fn lookup_prepared_message(
        &self,
        owner: &OwnerContext,
        token: &str,
        snapshot: &PreparedSend,
    ) -> Result<HistoryMessage, SendError> {
        self.rest.lookup_prepared(owner, token, snapshot).await
    }

    /// Отправляет снимок с переданным токеном без повторов и без изменения очереди.
    pub async fn post_prepared_message(
        &self,
        owner: &OwnerContext,
        token: &str,
        snapshot: &PreparedSend,
    ) -> Result<HistoryMessage, SendError> {
        self.authorize_outbox_dispatch(owner, snapshot, || ())
            .map_err(|_| SendError::EncryptionUnavailable)?;
        self.rest.post_prepared(owner, token, snapshot).await
    }
}
