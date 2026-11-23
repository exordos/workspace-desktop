//! Подключает подготовку фрагментов к проверенному маршруту и очереди Messenger.

use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    auth::{AuthError, Service},
    messenger::{
        MessengerSource,
        encryption::{
            EncryptionActivation, EncryptionError, FragmentDecryptionResult,
            key_exchange::KeyRequestTarget,
        },
        history::OpenHistoryError,
        send::{DirectSendError, SendError},
    },
    storage::{HistoryScope, PreparedSend, PreparedSendBody},
    sync::{PrepareSendError, Sync, SyncError},
};

use super::{FragmentError, FragmentReference, protected_ranges, seal_fragments};

/// Причина, по которой не завершилась пользовательская операция с фрагментами.
#[derive(Debug, Error)]
pub enum FragmentMessageError {
    #[error(transparent)]
    Fragment(#[from] FragmentError), // Защищённые диапазоны или их шифртекст не подходят.
    #[error(transparent)]
    Key(#[from] EncryptionError), // Сессия или ключ отправки больше недоступны.
    #[error(transparent)]
    Destination(#[from] DirectSendError), // Место для записи сообщения не подтверждено.
    #[error(transparent)]
    Source(#[from] OpenHistoryError), // Исходный чат ссылки недоступен.
    #[error(transparent)]
    Queue(#[from] PrepareSendError), // Готовое сообщение не записалось в очередь.
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход завершился, изменился или не выдал токен.
    #[error(transparent)]
    Sync(#[from] SyncError), // Не удалось найти исходную версию в локальной истории.
    #[error(transparent)]
    Edit(#[from] SendError), // Сервер отказал в изменении или не подтвердил его.
    #[error("the source message or a fragment key changed")]
    StaleEdit, // Сообщение, активный вход или один из ключей изменился.
    #[error("another protected key operation is still in progress")]
    Busy, // Не удалось допустить запрос одновременно с операцией защищённого хранилища.
    #[error("the default topic changed before the protected fragments were prepared")]
    TopicChanged, // Подтверждённая тема отличается от показанной редактором.
}

impl MessengerSource {
    /// Готовит смешанный Markdown и сохраняет его в очереди до первого сетевого запроса.
    pub async fn prepare_fragment_send(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
        expected_topic: Uuid,
        markdown: String,
        new_key: bool,
    ) -> Result<PreparedSend, FragmentMessageError> {
        let markdown = Zeroizing::new(markdown);
        if protected_ranges(&markdown)?.is_empty() {
            return Err(FragmentError::InvalidFragment.into());
        }
        let profile = self.encryption_profile(activation)?;
        let session = activation.session();
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(&session.owner, Service::Messenger, session.generation)
            .await?;
        let topic = self
            .rest
            .fragment_send_topic(&session.owner, &token, scope)
            .await;
        auth.with_active_generation(&session.owner, session.generation, || ())?;
        let topic_id = topic?;
        if topic_id != expected_topic {
            return Err(FragmentMessageError::TopicChanged);
        }
        let key = profile
            .keys
            .fragment_sending_key(activation, scope.stream_id, new_key)?;
        let sealed = seal_fragments(&markdown, &key)?;
        Ok(profile.keys.authorize_outgoing(&key, true, || {
            sync.prepare_send_body(
                &session.owner,
                session.generation,
                scope.stream_id,
                topic_id,
                PreparedSendBody::Markdown(sealed),
                None,
            )
        })??)
    }

    /// Проверяет исходный чат перед открытием фрагмента, включая ссылки из пересылки.
    pub async fn prepare_linked_fragment(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        reference: &FragmentReference,
    ) -> Result<FragmentDecryptionResult, FragmentMessageError> {
        let profile = self.encryption_profile(activation)?;
        let session = activation.session();
        let origin = sync.prepare_fragment_origins(
            &session.owner,
            session.generation,
            &[reference.stream_id()],
        )?;
        self.linked_fragment_topic(sync, activation, reference)
            .await?;
        let mut result = profile
            .keys
            .prepare_fragment(activation, reference)
            .map_err(FragmentError::from)?;
        result.0.origin = Some(origin);
        Ok(result)
    }

    /// Адресует запрос ключа исходному разговору, даже если фрагмент показан в пересылке.
    pub async fn fragment_key_request_target(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        reference: &FragmentReference,
    ) -> Result<KeyRequestTarget, FragmentMessageError> {
        let topic_id = self
            .linked_fragment_topic(sync, activation, reference)
            .await?
            .ok_or(DirectSendError::TopicUnavailable)?;
        Ok(KeyRequestTarget {
            stream_id: reference.stream_id(),
            topic_id,
            key_id: reference.key_id(),
        })
    }

    /// Перечитывает принадлежность исходного стрима для того же активного входа.
    async fn linked_fragment_topic(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        reference: &FragmentReference,
    ) -> Result<Option<Uuid>, FragmentMessageError> {
        let session = activation.session();
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(&session.owner, Service::Messenger, session.generation)
            .await?;
        let result = self
            .rest
            .fragment_source_topic(&session.owner, &token, reference.stream_id())
            .await;
        auth.with_active_generation(&session.owner, session.generation, || ())?;
        Ok(result?)
    }
}
