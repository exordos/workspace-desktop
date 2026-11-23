//! Готовит защищённую отправку и правку после проверки назначения, сессии и ключа.
//! Очередь получает только готовый конверт, а серверную правку сверяем с исходной строкой.

use std::sync::Arc;

use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    auth::{AuthError, Service},
    storage::{HistoryMessage, HistoryScope, MessageBody, PreparedSend, PreparedSendBody},
    sync::{MAX_MESSAGE_CHARACTERS, PrepareSendError, Sync, SyncError},
};

use super::{
    MessengerSource,
    encryption::{
        ChatKeyAddress, EncryptionActivation, EncryptionError, MessageDecryption, MessageEncryption,
    },
    history::OpenHistoryError,
    message_actions::MessageTarget,
    send::{DirectSendError, SendError},
};

/// Доступная цель защищённого композера в текущем входе.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncryptedSendTarget {
    pub topic_id: Uuid, // Фактическая тема записи, в том числе у лички.
    pub key_id: Uuid,   // Локально доступное поколение для новой отправки.
}

/// Причина, по которой не получилось отправить или изменить защищённое сообщение.
#[derive(Debug, Error)]
pub enum EncryptedMessageError {
    #[error(transparent)]
    Destination(#[from] DirectSendError), // Режим стрима, тема или доступ не подтверждены.
    #[error(transparent)]
    Key(#[from] EncryptionError), // Не удалось получить или проверить локальный ключ.
    #[error(transparent)]
    Queue(#[from] PrepareSendError), // Неверный текст или отказ записи очереди.
    #[error(transparent)]
    Transport(#[from] SendError), // Сервер отказал в изменении или итог неизвестен.
    #[error(transparent)]
    History(#[from] OpenHistoryError), // Исходная строка недоступна.
    #[error(transparent)]
    Auth(#[from] AuthError), // Учётные данные или поколение сменились.
    #[error(transparent)]
    Sync(#[from] SyncError), // Не удалось проверить текущую локальную версию.
    #[error("the encrypted message cannot be edited")]
    NotEditable, // Строка не принадлежит владельцу или имеет другой формат.
    #[error("the encrypted message changed while it was being edited")]
    StaleEdit, // Редактор начал работу с более старой версией сообщения.
    #[error("the default topic changed before the encrypted message was prepared")]
    TopicChanged, // Подтверждённая тема отличается от показанной редактором.
}

/// Доступ к локальным ключам того же профиля, на котором работает очередь.
pub(super) struct ChatEncryption {
    pub(super) keys: Arc<MessageEncryption>, // Сервис проверки активаций и защищённых записей.
    device_id: Uuid,                         // Устройство, закреплённое за профилем.
    pub(super) lifecycle_gate: tokio::sync::Mutex<()>, // Последовательно применяет создание и ротацию.
}

impl MessengerSource {
    /// Подключает тот же профиль ключей к пользовательским командам и фоновой отправке.
    pub fn with_chat_encryption(mut self, keys: Arc<MessageEncryption>, device_id: Uuid) -> Self {
        self.chat_encryption = Some(ChatEncryption {
            keys,
            device_id,
            lifecycle_gate: tokio::sync::Mutex::new(()),
        });
        self
    }

    /// Подтверждает режим, тему и доступный ключ для состояния защищённого композера.
    pub async fn encrypted_send_key(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
    ) -> Result<EncryptedSendTarget, EncryptedMessageError> {
        let profile = self.encryption_profile(activation)?;
        let (topic_id, key) = self
            .resolve_encrypted_send(sync, profile, activation, scope, None)
            .await?;
        let session = activation.session();
        let key_id = profile.keys.authorize_outgoing(&key, true, || {
            sync.auth()
                .with_active_generation(&session.owner, session.generation, || key.key_id())
        })??;
        Ok(EncryptedSendTarget { topic_id, key_id })
    }

    /// Готовит одно защищённое тело и фиксирует его до фонового запроса.
    pub async fn prepare_encrypted_send(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
        plaintext: String,
    ) -> Result<PreparedSend, EncryptedMessageError> {
        self.prepare_encrypted_send_inner(sync, activation, scope, None, plaintext)
            .await
    }

    /// Сохраняет сообщение только если сервер подтвердил ту же тему, что видел редактор.
    pub async fn prepare_encrypted_send_to_expected(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
        expected_topic: Uuid,
        plaintext: String,
    ) -> Result<PreparedSend, EncryptedMessageError> {
        self.prepare_encrypted_send_inner(sync, activation, scope, Some(expected_topic), plaintext)
            .await
    }

    /// Шифрует тело только после подтверждения области и ожидаемой темы.
    async fn prepare_encrypted_send_inner(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
        expected_topic: Option<Uuid>,
        plaintext: String,
    ) -> Result<PreparedSend, EncryptedMessageError> {
        let plaintext = validated_plaintext(plaintext)?;
        let profile = self.encryption_profile(activation)?;
        let (topic_id, key) = self
            .resolve_encrypted_send(sync, profile, activation, scope, expected_topic)
            .await?;
        let ciphertext = key.seal(&plaintext)?;
        let session = activation.session();
        Ok(profile.keys.authorize_outgoing(&key, true, || {
            sync.prepare_send_body(
                &session.owner,
                session.generation,
                scope.stream_id,
                topic_id,
                PreparedSendBody::Encrypted {
                    key_id: key.key_id(),
                    ciphertext,
                    device_id: profile.device_id,
                },
                None,
            )
        })??)
    }

    /// Проверяет собственное исходное сообщение и шифрует правку его точным поколением.
    pub async fn edit_encrypted_message(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        target: MessageTarget,
        expected: &HistoryMessage,
        plaintext: String,
    ) -> Result<HistoryMessage, EncryptedMessageError> {
        let plaintext = validated_plaintext(plaintext)?;
        let profile = self.encryption_profile(activation)?;
        if !target.is_valid()
            || expected.id != target.message_id
            || expected.stream_id != target.scope.stream_id
            || expected.topic_id != target.topic_id
        {
            return Err(EncryptedMessageError::NotEditable);
        }
        let session = activation.session();
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(&session.owner, Service::Messenger, session.generation)
            .await?;
        let scope = HistoryScope {
            stream_id: target.scope.stream_id,
            topic_id: Some(target.topic_id),
        };
        let access = self
            .rest
            .encrypted_send_target(&session.owner, &token, scope)
            .await;
        auth.with_active_generation(&session.owner, session.generation, || ())?;
        access?;
        let original = self
            .rest
            .encrypted_edit_source(&session.owner, &token, target)
            .await;
        auth.with_active_generation(&session.owner, session.generation, || ())?;
        let original = original?;
        if original != *expected {
            return Err(EncryptedMessageError::StaleEdit);
        }
        let MessageBody::Encrypted { key_id, .. } = expected.body else {
            return Err(EncryptedMessageError::NotEditable);
        };
        let key = profile
            .keys
            .prepare(activation, scope.stream_id, key_id)
            .map_err(EncryptionError::from)?;
        let ciphertext = key.seal(&plaintext)?;
        let version = profile
            .keys
            .authorize_outgoing(&key, false, || {
                sync.prepare_encrypted_edit(&session.owner, session.generation, expected)
            })??
            .ok_or(EncryptedMessageError::StaleEdit)?;
        let result = self
            .rest
            .edit_encrypted_once(&session.owner, &token, target, key_id, &ciphertext)
            .await;
        let confirmed = result?;
        if !profile.keys.authorize_outgoing(&key, false, || {
            sync.confirm_encrypted_edit(
                &session.owner,
                session.generation,
                expected,
                version,
                &confirmed,
            )
        })?? {
            return Err(EncryptedMessageError::StaleEdit);
        }
        Ok(confirmed)
    }

    /// Сверяет устройство и достаёт профиль, выделенный этому экземпляру клиента.
    pub(super) fn encryption_profile(
        &self,
        activation: &EncryptionActivation,
    ) -> Result<&ChatEncryption, EncryptionError> {
        self.chat_encryption
            .as_ref()
            .filter(|profile| {
                profile.device_id == activation.session().device_id && !profile.device_id.is_nil()
            })
            .ok_or(EncryptionError::Stale)
    }

    /// Перечитывает режим и актуальное поколение при каждой проверке композера или отправке.
    async fn resolve_encrypted_send(
        &self,
        sync: &Sync,
        profile: &ChatEncryption,
        activation: &EncryptionActivation,
        scope: HistoryScope,
        expected_topic: Option<Uuid>,
    ) -> Result<(Uuid, MessageDecryption), EncryptedMessageError> {
        let session = activation.session();
        let auth = sync.auth();
        let token = auth
            .access_token_for_generation(&session.owner, Service::Messenger, session.generation)
            .await?;
        let lookup = self
            .rest
            .encrypted_send_target(&session.owner, &token, scope)
            .await;
        auth.with_active_generation(&session.owner, session.generation, || ())?;
        let (topic_id, current_key) = lookup?;
        if expected_topic.is_some_and(|expected| expected != topic_id) {
            return Err(EncryptedMessageError::TopicChanged);
        }
        let key = profile
            .keys
            .sending_key(activation, scope.stream_id, current_key)?;
        Ok((topic_id, key))
    }

    /// Подтверждает исходный ключ и закрепляет попытку до следующей операции с ключом.
    pub(super) fn authorize_outbox_dispatch<T>(
        &self,
        owner: &crate::context::OwnerContext,
        snapshot: &PreparedSend,
        admit: impl FnOnce() -> T,
    ) -> Result<T, EncryptionError> {
        let PreparedSendBody::Encrypted {
            key_id,
            device_id,
            ciphertext,
        } = &snapshot.body
        else {
            return Ok(admit());
        };
        let profile = self
            .chat_encryption
            .as_ref()
            .filter(|profile| profile.device_id == *device_id && !device_id.is_nil())
            .ok_or(EncryptionError::Stale)?;
        let address = ChatKeyAddress {
            owner: owner.clone(),
            device_id: *device_id,
            stream_id: snapshot.stream_id,
            key_id: *key_id,
        };
        profile
            .keys
            .authorize_prepared_envelope(&address, ciphertext, admit)
    }
}

/// Проверяет лимит и убирает встроенные выдержки из новых защищённых конвертов.
fn validated_plaintext(plaintext: String) -> Result<Zeroizing<String>, PrepareSendError> {
    let plaintext = Zeroizing::new(plaintext);
    if plaintext.trim().is_empty() || plaintext.chars().count() > MAX_MESSAGE_CHARACTERS {
        return Err(PrepareSendError::InvalidText);
    }
    Ok(super::quote::project_owned(plaintext).0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Общая подготовка отправки и правки убирает встроенный фрагмент до шифрования.
    #[test]
    fn outgoing_whole_body_never_seals_a_legacy_quote_excerpt() {
        let source = Uuid::from_u128(15);
        let unsafe_link = super::super::quote::reference(source, "Автор", Some("секрет"));
        let markdown = format!("{unsafe_link}\n\n**Ответ**");
        let prepared = validated_plaintext(markdown).unwrap();
        assert_eq!(
            &*prepared,
            &format!("[Автор](urn:quote:{source})\n\n**Ответ**")
        );
    }

    /// Считает символы вне BMP как одну кодовую точку, как ограничение Electron.
    #[test]
    fn validates_unicode_code_points_before_sealing() {
        assert!(validated_plaintext("😀".repeat(MAX_MESSAGE_CHARACTERS)).is_ok());
        assert!(matches!(
            validated_plaintext("😀".repeat(MAX_MESSAGE_CHARACTERS + 1)),
            Err(PrepareSendError::InvalidText)
        ));
    }
}
