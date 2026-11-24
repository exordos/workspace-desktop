//! Проверяет право ответить на запрос ключа и завершает свой запрос по проверенному ответу.
//! Подготовленные ответы повторяются по тому же UUID, а полученный ключ и квитанция сохраняются вместе.

mod index;
#[cfg(test)]
mod tests;

use index::ReplyResolution;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::storage::KeyResponseHint;

use super::super::{
    DecryptionError, GroupKey, MessageDecryption,
    grant::{RequestPublicKey, open_group_key, seal_group_key},
    key_service::{KeyResponseTarget, KeySharingPolicy},
};
use super::SendError;
use super::recovery::authenticates_recovery;
use super::{
    ChatKeyAddress, ChatKeyStore, EncryptionActivation, KeyExchange, KeyExchangeError, KeyRecovery,
    KeyRequestReceipt, KeyRequestTarget, KeyServiceMessage, KeyServicePayload, PreparedKeyRequest,
    Service, forget_pending_request, fresh_message_uuid, request_payload, verify_request,
};

/// Результат ответа на запрос, который экран показывает участникам.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyRequestOutcome {
    Granted,  // Чат выдал ключ указанному устройству.
    Rejected, // В доступе к ключу отказано.
}

/// Запись завершения запроса, которой заменяется приватная часть пары.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct KeyRequestCompletion {
    pub canonical_id: Uuid, // UUID подготовленного запроса, который нужно завершить.
    pub request_id: Uuid,   // Проверенный публичный UUID запроса.
    pub response_id: Uuid,  // Проверенный публичный UUID ответа.
    pub outcome: KeyRequestOutcome, // Выдача или отказ.
}

/// Неизменяемый снимок выдачи или отказа, подготовленный до POST.
#[derive(Clone, Deserialize, Serialize)]
pub struct StoredKeyReply {
    pub canonical_id: Uuid, // UUID, повторяемый после неизвестного результата.
    pub payload: KeyServicePayload, // Точное служебное тело, включая шифротекст выдачи.
    pub key_fingerprint: Option<[u8; 32]>, // Отпечаток ключа выдачи. `None`, если это отказ.
    pub request_public_key: String, // Публичный ключ запроса, которому адресован этот ответ.
}

/// Действующие действия для проверенного запроса другого устройства.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyReplyCapabilities {
    pub can_grant: bool,  // Можно выдать ровно запрошенный локальный ключ.
    pub can_reject: bool, // Можно отказать с учётом правил отказа и наличия ключа.
}

/// Подтверждённое состояние запроса другого устройства в загруженной истории.
pub enum KeyRequestStatus {
    Pending {
        author_id: Uuid,                    // Проверенный автор запроса.
        capabilities: KeyReplyCapabilities, // Действия до подтверждённого ответа.
    },
    Completed {
        author_id: Uuid,            // Проверенный автор запроса.
        outcome: KeyRequestOutcome, // Проверенная выдача или отказ.
    },
}

impl KeyResponseHint {
    /// Возвращает исходный адрес, который проверит авторитетное чтение ответа.
    pub fn target(&self) -> KeyRequestTarget {
        KeyRequestTarget {
            stream_id: self.scope.stream_id,
            topic_id: self.scope.topic_id.unwrap_or(Uuid::nil()),
            key_id: self.key_id,
        }
    }
}

impl KeyExchange {
    /// Перепроверяет сохранённый ответ только если локальное устройство ещё ждёт этот запрос.
    pub async fn apply_persisted_key_response(
        &self,
        activation: &EncryptionActivation,
        hint: &KeyResponseHint,
    ) -> Result<Option<KeyRequestCompletion>, KeyExchangeError> {
        let target = hint.target();
        let request_id = hint.request_id;
        if target.topic_id.is_nil()
            || target.stream_id.is_nil()
            || target.key_id.is_nil()
            || request_id.is_nil()
            || hint.response_id.is_nil()
        {
            return Err(KeyExchangeError::InvalidInput);
        }
        let pending = self
            .with_request_store(activation, target, move |encryption, address| {
                let request = encryption.store.load_request(address, target.topic_id)?;
                Ok(request
                    .is_some_and(|request| target.public_id(request.canonical_id) == request_id))
            })
            .await?;
        if !pending {
            return Ok(None);
        }
        self.apply_key_response(activation, target, hint.response_id)
            .await
    }

    /// Читает подтверждённый запрос, текущую роль и наличие ключа для двух независимых действий.
    pub async fn key_reply_capabilities(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        request_id: Uuid,
    ) -> Result<KeyReplyCapabilities, KeyExchangeError> {
        Ok(
            match self
                .read_key_request_status(activation, target, request_id, false)
                .await?
            {
                Some(KeyRequestStatus::Pending { capabilities, .. }) => capabilities,
                _ => KeyReplyCapabilities::default(),
            },
        )
    }

    /// Показывает историю только для запросов, на которые у текущего входа есть право отвечать.
    pub async fn key_request_status(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        request_id: Uuid,
    ) -> Result<Option<KeyRequestStatus>, KeyExchangeError> {
        self.read_key_request_status(activation, target, request_id, true)
            .await
    }

    /// Проверяет получателя, доступность ответа и исход уже рассмотренного запроса.
    async fn read_key_request_status(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        request_id: Uuid,
        include_history: bool,
    ) -> Result<Option<KeyRequestStatus>, KeyExchangeError> {
        let token = self.exchange_token(activation).await?;
        let request = self
            .key_message(activation, &token, target, request_id)
            .await?;
        let local = activation.session().device_id.to_string();
        if reply_recipient(&request, &local)?.recipient_device_id == local {
            return Ok(None);
        }
        let resolution = self
            .confirm_reply_resolution(activation, &token, target, &request)
            .await?;
        if !include_history && matches!(resolution, ReplyResolution::Resolved(_)) {
            return Ok(None);
        }
        let key = self.reply_key(activation, target).await?;
        let policy = self.reply_policy(activation, &token, target).await?;
        let issuer = Uuid::parse_str(&activation.session().owner.user_id)
            .map_err(|_| KeyExchangeError::InvalidInput)?;
        let has_key = key.as_ref().is_some_and(|key| {
            key.access
                .is_current(self.encryption.selection.lock().unwrap().as_ref())
        });
        self.require_current(activation)?;
        let capabilities = KeyReplyCapabilities {
            can_grant: policy.permits_outbound_reply(
                issuer,
                request.author_id,
                KeyRequestOutcome::Granted,
                has_key,
            ),
            can_reject: policy.permits_outbound_reply(
                issuer,
                request.author_id,
                KeyRequestOutcome::Rejected,
                has_key,
            ),
        };
        if !capabilities.can_grant && !capabilities.can_reject {
            return Ok(None);
        }
        Ok(Some(match resolution {
            ReplyResolution::Resolved(outcome) => KeyRequestStatus::Completed {
                author_id: request.author_id,
                outcome,
            },
            ReplyResolution::Pending | ReplyResolution::Unverified => KeyRequestStatus::Pending {
                author_id: request.author_id,
                capabilities,
            },
        }))
    }

    /// Выдаёт точный локальный ключ либо отказывает в той же теме, где опубликован запрос.
    pub async fn respond_to_key_request(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        request_id: Uuid,
        outcome: KeyRequestOutcome,
    ) -> Result<KeyServiceMessage, KeyExchangeError> {
        let session = activation.session();
        let owner = &session.owner;
        let issuer = Uuid::parse_str(&owner.user_id).map_err(|_| KeyExchangeError::InvalidInput)?;
        let token = self.exchange_token(activation).await?;
        let request = self
            .key_message(activation, &token, target, request_id)
            .await?;
        let local = session.device_id.to_string();
        if reply_recipient(&request, &local)?.recipient_device_id == local {
            return Err(KeyExchangeError::Forbidden);
        }
        let saved = self
            .with_request_store(activation, target, move |encryption, address| {
                Ok(encryption
                    .store
                    .load_reply(address, target.topic_id, request_id)?)
            })
            .await?;
        let reply = match saved {
            Some(reply) => {
                validate_saved_reply(&reply, &request, &session.device_id.to_string(), outcome)?;
                reply
            }
            None => {
                let has_key = self.reply_key(activation, target).await?.is_some();
                self.require_reply_policy(
                    activation,
                    &token,
                    target,
                    request.author_id,
                    outcome,
                    has_key,
                )
                .await?;
                let source = request.clone();
                self.with_request_store(activation, target, move |encryption, address| {
                    prepare_reply(encryption.store.as_ref(), address, &source, outcome)
                })
                .await?
            }
        };
        let public_id = target.public_id(reply.canonical_id);
        let existing = self
            .messenger
            .rest
            .lookup_key_service_message(owner, &token, target, public_id)
            .await;
        self.require_current(activation)?;
        let message = match existing? {
            Some(message) => message,
            None => {
                match self
                    .confirm_reply_resolution(activation, &token, target, &request)
                    .await?
                {
                    ReplyResolution::Resolved(_) => return Err(KeyExchangeError::Forbidden),
                    ReplyResolution::Unverified => return Err(SendError::Unknown.into()),
                    ReplyResolution::Pending => {}
                }
                let key = match reply.key_fingerprint {
                    Some(expected) => Some(
                        self.prepare_delivery_key(activation, target, expected)
                            .await?,
                    ),
                    None => self.reply_key(activation, target).await?,
                };
                // Сервер не принимает ревизию политики при POST. Перечитываем правила сразу
                // перед локальным допуском, но изменение прав после этого чтения может опередить запись на сервере.
                let needs_key = self
                    .require_reply_policy(
                        activation,
                        &token,
                        target,
                        request.author_id,
                        outcome,
                        key.is_some(),
                    )
                    .await?;
                self.post_key_message(
                    activation,
                    &token,
                    target,
                    reply.canonical_id,
                    &reply.payload,
                    key.as_ref().filter(|_| needs_key),
                )
                .await?
            }
        };
        if message.id != public_id
            || message.author_id != issuer
            || message.payload != reply.payload
            || !message.matches_request(&request)
        {
            return Err(KeyExchangeError::InvalidResponse);
        }
        Ok(message)
    }

    /// Перечитывает правила и возвращает, нужно ли закрепить точный ключ перед отправкой.
    async fn require_reply_policy(
        &self,
        activation: &EncryptionActivation,
        token: &str,
        target: KeyRequestTarget,
        requester: Uuid,
        outcome: KeyRequestOutcome,
        has_key: bool,
    ) -> Result<bool, KeyExchangeError> {
        let issuer = Uuid::parse_str(&activation.session().owner.user_id)
            .map_err(|_| KeyExchangeError::InvalidInput)?;
        let policy = self.reply_policy(activation, token, target).await?;
        if !policy.permits_outbound_reply(issuer, requester, outcome, has_key) {
            if outcome == KeyRequestOutcome::Granted
                && !has_key
                && policy.permits_outbound_reply(issuer, requester, outcome, true)
            {
                return Err(KeyExchangeError::MissingKey);
            }
            return Err(KeyExchangeError::Forbidden);
        }
        Ok(!policy.permits_outbound_reply(issuer, requester, outcome, false))
    }

    /// Перечитывает права и убеждается, что вход не сменился за время запроса.
    async fn reply_policy(
        &self,
        activation: &EncryptionActivation,
        token: &str,
        target: KeyRequestTarget,
    ) -> Result<KeySharingPolicy, KeyExchangeError> {
        let policy = self
            .messenger
            .rest
            .key_sharing_policy(&activation.session().owner, token, target)
            .await;
        self.require_current(activation)?;
        Ok(policy?)
    }

    /// Загружает ровно ту версию ключа, из которой был подготовлен сохранённый ответ.
    pub(super) async fn prepare_delivery_key(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        expected: [u8; 32],
    ) -> Result<MessageDecryption, KeyExchangeError> {
        let key = self
            .reply_key(activation, target)
            .await?
            .ok_or(KeyExchangeError::MissingKey)?;
        if fingerprint(&key.key) != expected {
            return Err(KeyExchangeError::KeyConflict);
        }
        Ok(key)
    }

    /// Читает именно запрошенное поколение на рабочем потоке.
    async fn reply_key(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
    ) -> Result<Option<MessageDecryption>, KeyExchangeError> {
        let encryption = self.encryption.clone();
        let selected = activation.clone();
        let result = tokio::task::spawn_blocking(move || {
            encryption.prepare(&selected, target.stream_id, target.key_id)
        })
        .await
        .map_err(|_| KeyExchangeError::Worker)?;
        self.require_current(activation)?;
        match result {
            Ok(key) => Ok(Some(key)),
            Err(DecryptionError::MissingKey) => Ok(None),
            Err(DecryptionError::Stale) => Err(KeyExchangeError::Stale),
            Err(DecryptionError::InvalidCiphertext) => Err(KeyExchangeError::KeyConflict),
            Err(DecryptionError::KeyStore(error)) => Err(KeyExchangeError::Store(error)),
        }
    }

    /// Проверяет серверное авторство и сохраняет ответ только для локального ожидающего устройства.
    pub async fn apply_key_response(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        response_id: Uuid,
    ) -> Result<Option<KeyRequestCompletion>, KeyExchangeError> {
        let session = activation.session();
        let owner = &session.owner;
        let requester =
            Uuid::parse_str(&owner.user_id).map_err(|_| KeyExchangeError::InvalidInput)?;
        let token = self.exchange_token(activation).await?;
        let response = self
            .key_message(activation, &token, target, response_id)
            .await?;
        let Some((response_target, outcome)) = response_details(&response.payload) else {
            return Err(KeyExchangeError::InvalidResponse);
        };
        if response_target.recipient_device_id != session.device_id.to_string() {
            return Ok(None);
        }
        let request = self
            .key_message(
                activation,
                &token,
                target,
                response_target.request_message_uuid,
            )
            .await?;
        if request.author_id != requester
            || response.id == request.id
            || !response.matches_request(&request)
        {
            return Err(KeyExchangeError::InvalidResponse);
        }
        let public_request_id = request.id;
        // Уже сохранённая квитанция подтверждает исторический исход даже после смены роли.
        // Повтор не делает новую запись и не возвращает удалённый ключ.
        let (historical, recovery) = self
            .with_request_store(activation, target, move |encryption, address| {
                let store = encryption.store.as_ref();
                let historical =
                    store.load_request_completion(address, target.topic_id, public_request_id)?;
                let recovery = if historical.is_none() {
                    store
                        .load_request(address, target.topic_id)?
                        .filter(|request| {
                            target.public_id(request.canonical_id) == public_request_id
                        })
                        .and_then(|request| request.recovery)
                } else {
                    None
                };
                Ok((historical, recovery))
            })
            .await?;
        if let Some(receipt) = historical {
            return Ok((receipt.response_id == response.id).then_some(receipt));
        }
        let recovery_message = if let Some(recovery) = &recovery {
            Some(
                self.verify_recovery_source(activation, &token, target, &recovery.source())
                    .await?,
            )
        } else {
            None
        };
        // Сетевая проверка конверта могла задержаться. Состав и права читаем после неё.
        let policy = self
            .messenger
            .rest
            .key_sharing_policy(owner, &token, target)
            .await;
        self.require_current(activation)?;
        if !policy?.permits_reply(response.author_id, request.author_id, outcome, false) {
            return Err(KeyExchangeError::Forbidden);
        }

        let current = activation.clone();
        let sync = self.sync.clone();
        self.with_request_store(activation, target, move |encryption, address| {
            if let Some(message) = &recovery_message
                && !sync.matches_key_recovery_source(&address.owner, message)?
            {
                return Err(KeyExchangeError::Stale);
            }
            let result = finish_response(
                encryption.store.as_ref(),
                address,
                target,
                &request,
                &response,
                recovery.as_ref(),
            )?;
            if let Some((receipt, published)) = result {
                forget_pending_request(encryption, &current, target, receipt.canonical_id);
                if published {
                    encryption.publish_key_change(&current, target.stream_id, target.key_id, true);
                }
            }
            Ok(result.map(|(receipt, _)| receipt))
        })
        .await
    }

    /// Получает токен Messenger только для выбранной активации.
    async fn exchange_token(
        &self,
        activation: &EncryptionActivation,
    ) -> Result<String, KeyExchangeError> {
        self.require_current(activation)?;
        let session = activation.session();
        let token = self
            .sync
            .auth()
            .access_token_for_generation(&session.owner, Service::Messenger, session.generation)
            .await?;
        self.require_current(activation)?;
        Ok(token)
    }

    /// Читает серверное сообщение и проверяет, что после сети выбран тот же вход.
    async fn key_message(
        &self,
        activation: &EncryptionActivation,
        token: &str,
        target: KeyRequestTarget,
        message_id: Uuid,
    ) -> Result<KeyServiceMessage, KeyExchangeError> {
        if message_id.is_nil()
            || target.stream_id.is_nil()
            || target.topic_id.is_nil()
            || target.key_id.is_nil()
            || activation.session().device_id.is_nil()
        {
            return Err(KeyExchangeError::InvalidInput);
        }
        let message = self
            .messenger
            .rest
            .lookup_key_service_message(&activation.session().owner, token, target, message_id)
            .await;
        self.require_current(activation)?;
        Ok(message?.ok_or(SendError::Unknown)?)
    }
}

/// Проверяет тип запроса, получателя и ключ HPKE до показа или выполнения действия.
fn reply_recipient(
    request: &KeyServiceMessage,
    issuer_device: &str,
) -> Result<KeyResponseTarget, KeyExchangeError> {
    let KeyServicePayload::Request { public_key, .. } = &request.payload else {
        return Err(KeyExchangeError::InvalidResponse);
    };
    RequestPublicKey::from_base64(public_key)?;
    request
        .reply_target(issuer_device)
        .ok_or(KeyExchangeError::InvalidResponse)
}

/// Записывает один неизменяемый ответ или восстанавливает его после перезапуска.
fn prepare_reply(
    store: &dyn ChatKeyStore,
    address: &ChatKeyAddress,
    request: &KeyServiceMessage,
    outcome: KeyRequestOutcome,
) -> Result<StoredKeyReply, KeyExchangeError> {
    let KeyServicePayload::Request { public_key, .. } = &request.payload else {
        return Err(KeyExchangeError::InvalidResponse);
    };
    let recipient = RequestPublicKey::from_base64(public_key)?;
    let target = request
        .reply_target(&address.device_id.to_string())
        .ok_or(KeyExchangeError::InvalidResponse)?;
    if let Some(saved) = store.load_reply(address, request.topic_id, request.id)? {
        validate_saved_reply(&saved, request, &address.device_id.to_string(), outcome)?;
        return Ok(saved);
    }
    let (payload, key_fingerprint) = if outcome == KeyRequestOutcome::Granted {
        let key = store.load(address)?.ok_or(KeyExchangeError::MissingKey)?;
        let content = seal_group_key(&key, recipient, target.grant_context(address.stream_id))?;
        (
            KeyServicePayload::Grant { target, content },
            Some(fingerprint(&key)),
        )
    } else {
        (KeyServicePayload::Reject(target), None)
    };
    let prepared = StoredKeyReply {
        canonical_id: fresh_message_uuid()?,
        payload,
        key_fingerprint,
        request_public_key: public_key.clone(),
    };
    store.save_reply(address, request.topic_id, request.id, &prepared)?;
    Ok(prepared)
}

/// Проверяет, что восстановленный ответ принадлежит точному запросу и выбранному действию.
fn validate_saved_reply(
    saved: &StoredKeyReply,
    request: &KeyServiceMessage,
    issuer_device: &str,
    outcome: KeyRequestOutcome,
) -> Result<(), KeyExchangeError> {
    let KeyServicePayload::Request { public_key, .. } = &request.payload else {
        return Err(KeyExchangeError::InvalidResponse);
    };
    let target = request
        .reply_target(issuer_device)
        .ok_or(KeyExchangeError::InvalidResponse)?;
    if response_details(&saved.payload) != Some((&target, outcome))
        || saved.canonical_id.is_nil()
        || saved.request_public_key != *public_key
        || saved.key_fingerprint.is_some() != (outcome == KeyRequestOutcome::Granted)
    {
        return Err(KeyExchangeError::KeyConflict);
    }
    Ok(())
}

/// Завершает только точный локальный запрос по авторизованному ответу из той же темы.
fn finish_response(
    store: &dyn ChatKeyStore,
    address: &ChatKeyAddress,
    target: KeyRequestTarget,
    request: &KeyServiceMessage,
    response: &KeyServiceMessage,
    verified_recovery: Option<&KeyRecovery>,
) -> Result<Option<(KeyRequestCompletion, bool)>, KeyExchangeError> {
    let (response_target, outcome) =
        response_details(&response.payload).ok_or(KeyExchangeError::InvalidResponse)?;
    if response_target.recipient_device_id != address.device_id.to_string()
        || !response.matches_request(request)
    {
        return Err(KeyExchangeError::InvalidResponse);
    }
    if let Some(receipt) = store.load_request_completion(address, target.topic_id, request.id)? {
        return Ok((receipt.response_id == response.id).then_some((receipt, false)));
    }
    let Some(pending) = store.load_request(address, target.topic_id)? else {
        return Ok(None);
    };
    let expected_request = target.public_id(pending.canonical_id);
    if request.id != expected_request
        || pending
            .confirmed_request_id
            .is_some_and(|id| id != expected_request)
    {
        return Ok(None);
    }
    if pending.recovery.as_ref() != verified_recovery {
        return Err(KeyExchangeError::Stale);
    }
    let author =
        Uuid::parse_str(&address.owner.user_id).map_err(|_| KeyExchangeError::InvalidInput)?;
    let expected = PreparedKeyRequest {
        canonical_id: pending.canonical_id,
        payload: request_payload(address, &pending),
        confirmed: pending.confirmed_request_id.is_some(),
        recovery: pending.recovery.clone(),
    };
    verify_request(
        request,
        author,
        KeyRequestReceipt {
            target,
            request_id: expected_request,
        },
        &expected,
    )?;

    let key = match &response.payload {
        KeyServicePayload::Grant { content, .. } => Some(open_group_key(
            &pending.pair,
            response_target.grant_context(target.stream_id),
            content,
        )?),
        _ => None,
    };
    let (key, publish, outcome) = match (key, verified_recovery) {
        (Some(key), Some(recovery)) => {
            let current = store.load(address)?;
            if authenticates_recovery(address, recovery, current.as_ref(), &key)? {
                (Some(key), true, outcome)
            } else {
                // Неверная выдача закрывает попытку, но не заменяет локальный ключ.
                (None, false, KeyRequestOutcome::Rejected)
            }
        }
        (Some(key), None) => {
            let current = store.load(address)?;
            if current
                .as_ref()
                .is_some_and(|existing| !bool::from(existing.as_bytes().ct_eq(key.as_bytes())))
            {
                return Err(KeyExchangeError::KeyConflict);
            }
            (Some(key), current.is_none(), outcome)
        }
        (None, _) => (None, false, outcome),
    };
    let receipt = KeyRequestCompletion {
        canonical_id: pending.canonical_id,
        request_id: request.id,
        response_id: response.id,
        outcome,
    };
    if !store.complete_request(address, target.topic_id, &receipt, key.as_ref())? {
        return Ok(None);
    }
    Ok(Some((receipt, publish)))
}

/// Выбирает адрес и исход служебного ответа, если тело является ответом.
fn response_details(
    payload: &KeyServicePayload,
) -> Option<(&KeyResponseTarget, KeyRequestOutcome)> {
    match payload {
        KeyServicePayload::Grant { target, .. } => Some((target, KeyRequestOutcome::Granted)),
        KeyServicePayload::Reject(target) => Some((target, KeyRequestOutcome::Rejected)),
        _ => None,
    }
}

/// Привязывает повтор зашифрованной выдачи к тому же локальному материалу ключа.
pub(super) fn fingerprint(key: &GroupKey) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}
