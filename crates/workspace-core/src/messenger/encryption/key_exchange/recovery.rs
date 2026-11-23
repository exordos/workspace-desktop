//! Проверяет замену неверного ключа по исходному зашифрованному сообщению.
//! Хранилище запоминает только шифротекст и отпечаток ключа, под которым возникла ошибка.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::{HistoryMessage, HistoryScope, MessageBody};

use super::super::{GroupKey, decrypt_message};
use super::responses::fingerprint;
use super::{
    ChatKeyAddress, ChatKeyStore, EncryptionActivation, KeyExchange, KeyExchangeError,
    KeyRequestReceipt, KeyRequestTarget,
};

/// Свидетельство, по которому можно проверить замену конкретного локального ключа.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct KeyRecovery {
    pub message_id: Uuid,   // Публичный UUID исходного сообщения в нужной теме.
    pub ciphertext: String, // Исходный конверт из авторитетного API.
    pub previous_key_fingerprint: [u8; 32], // Отпечаток ключа на момент ошибки.
}

/// Снимок сообщения, на котором пользователь увидел ошибку расшифровки.
pub(super) struct RecoverySource {
    message_id: Uuid,   // Публичное размещение сообщения, которое нужно перечитать.
    ciphertext: String, // Конверт, показанный в исходном окне истории.
}

impl KeyRecovery {
    /// Возвращает исходный конверт для Fresh GET перед отправкой или приёмом ответа.
    pub(super) fn source(&self) -> RecoverySource {
        RecoverySource {
            message_id: self.message_id,
            ciphertext: self.ciphertext.clone(),
        }
    }
}

impl KeyExchange {
    /// Запрашивает замену неверного ключа для той версии сообщения, которую видел пользователь.
    pub async fn request_recovery(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        message_id: Uuid,
        expected_ciphertext: &str,
        device_name: &str,
    ) -> Result<KeyRequestReceipt, KeyExchangeError> {
        if message_id.is_nil() || expected_ciphertext.is_empty() {
            return Err(KeyExchangeError::InvalidInput);
        }
        let source = RecoverySource {
            message_id,
            ciphertext: expected_ciphertext.to_owned(),
        };
        self.request_generation(activation, target, device_name, Some(source))
            .await
    }

    /// Подтверждает, что сервер по-прежнему хранит тот же конверт в исходной теме.
    pub(super) async fn verify_recovery_source(
        &self,
        activation: &EncryptionActivation,
        token: &str,
        target: KeyRequestTarget,
        source: &RecoverySource,
    ) -> Result<HistoryMessage, KeyExchangeError> {
        let scope = HistoryScope {
            stream_id: target.stream_id,
            topic_id: Some(target.topic_id),
        };
        let result = self
            .messenger
            .rest
            .history_anchor(&activation.session().owner, token, scope, source.message_id)
            .await;
        self.require_current(activation)?;
        let message = result?;
        if !matches!(&message.body, MessageBody::Encrypted { key_id, ciphertext }
            if *key_id == target.key_id && ciphertext == &source.ciphertext)
        {
            return Err(KeyExchangeError::Stale);
        }
        Ok(message)
    }
}

/// Запоминает только прежнюю неработающую версию ключа под блокировкой операций хранилища.
pub(super) fn capture_recovery(
    store: &dyn ChatKeyStore,
    address: &ChatKeyAddress,
    source: RecoverySource,
) -> Result<KeyRecovery, KeyExchangeError> {
    let previous = store.load(address)?.ok_or(KeyExchangeError::MissingKey)?;
    if decrypt_message(
        &previous,
        address.stream_id,
        address.key_id,
        &source.ciphertext,
    )
    .is_ok()
    {
        return Err(KeyExchangeError::KeyAlreadyUsable);
    }
    Ok(KeyRecovery {
        message_id: source.message_id,
        ciphertext: source.ciphertext,
        previous_key_fingerprint: fingerprint(&previous),
    })
}

/// Проверяет неизменность старого ключа и подлинность содержимого под новым.
pub(super) fn authenticates_recovery(
    address: &ChatKeyAddress,
    recovery: &KeyRecovery,
    current: Option<&GroupKey>,
    candidate: &GroupKey,
) -> Result<bool, KeyExchangeError> {
    if current.is_none_or(|key| fingerprint(key) != recovery.previous_key_fingerprint) {
        return Err(KeyExchangeError::Stale);
    }
    Ok(decrypt_message(
        candidate,
        address.stream_id,
        address.key_id,
        &recovery.ciphertext,
    )
    .is_ok())
}
