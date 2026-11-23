//! Проверяет повторы выдачи и отказа, привязку устройств и атомарное завершение запроса.

use std::{collections::HashMap, sync::Mutex};

use super::*;
use crate::{
    context::{AccountId, OwnerContext, SessionGeneration},
    messenger::encryption::{
        EncryptionSession, KeyStoreError, MessageEncryption,
        grant::RequestKeyPair,
        key_exchange::{StoredKeyRequest, admit_dispatch, confirm_request},
    },
};

/// Записи адаптера, изменяемые за одну блокировку вместо одной записи Keychain.
#[derive(Default)]
struct Records {
    keys: HashMap<ChatKeyAddress, GroupKey>, // Текущие ключи чата.
    requests: HashMap<(ChatKeyAddress, Uuid), StoredKeyRequest>, // Активные пары устройств по теме.
    completed: HashMap<(ChatKeyAddress, Uuid, Uuid), KeyRequestCompletion>, // Квитанции по точному запросу.
    replies: HashMap<(ChatKeyAddress, Uuid, Uuid), StoredKeyReply>, // Снимки отправляемых ответов.
    fail_complete: bool, // Имитирует отказ атомарной записи.
}

/// Тестовое защищённое хранилище с атомарным изменением ключа и квитанции.
#[derive(Default)]
struct TestExchangeStore(Mutex<Records>); // Управляемые записи разных устройств.

impl ChatKeyStore for TestExchangeStore {
    /// Читает существующий групповой ключ.
    fn load(&self, address: &ChatKeyAddress) -> Result<Option<GroupKey>, KeyStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .keys
            .get(address)
            .map(|key| GroupKey::from_bytes(*key.as_bytes())))
    }

    /// Сохраняет ключ до тестового перезапуска.
    fn save(&self, address: &ChatKeyAddress, key: &GroupKey) -> Result<(), KeyStoreError> {
        self.0
            .lock()
            .unwrap()
            .keys
            .insert(address.clone(), GroupKey::from_bytes(*key.as_bytes()));
        Ok(())
    }

    /// Убирает ключ и ожидающие запросы, сохраняя квитанции против повтора.
    fn remove(&self, address: &ChatKeyAddress) -> Result<(), KeyStoreError> {
        let mut records = self.0.lock().unwrap();
        records.keys.remove(address);
        records.requests.retain(|(owner, _), _| owner != address);
        Ok(())
    }

    /// Восстанавливает изолированное состояние пары.
    fn load_request(
        &self,
        address: &ChatKeyAddress,
        topic: Uuid,
    ) -> Result<Option<StoredKeyRequest>, KeyStoreError> {
        self.0
            .lock()
            .unwrap()
            .requests
            .get(&(address.clone(), topic))
            .map(copy_request)
            .transpose()
    }

    /// Записывает подготовленную пару для указанной темы.
    fn save_request(
        &self,
        address: &ChatKeyAddress,
        topic: Uuid,
        request: &StoredKeyRequest,
    ) -> Result<(), KeyStoreError> {
        self.0
            .lock()
            .unwrap()
            .requests
            .insert((address.clone(), topic), copy_request(request)?);
        Ok(())
    }

    /// Меняет пару только если другой запрос или ответ её ещё не заменил.
    fn replace_request(
        &self,
        address: &ChatKeyAddress,
        topic: Uuid,
        expected_canonical_id: Uuid,
        request: &StoredKeyRequest,
    ) -> Result<bool, KeyStoreError> {
        let mut records = self.0.lock().unwrap();
        let Some(pending) = records.requests.get_mut(&(address.clone(), topic)) else {
            return Ok(false);
        };
        if pending.canonical_id != expected_canonical_id {
            return Ok(false);
        }
        *pending = copy_request(request)?;
        Ok(true)
    }

    /// Читает ответ, который уже завершил точный запрос.
    fn load_request_completion(
        &self,
        address: &ChatKeyAddress,
        topic: Uuid,
        request: Uuid,
    ) -> Result<Option<KeyRequestCompletion>, KeyStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .completed
            .get(&(address.clone(), topic, request))
            .copied())
    }

    /// Записывает ключ, квитанцию и удаление приватной части только одним изменением.
    fn complete_request(
        &self,
        address: &ChatKeyAddress,
        topic: Uuid,
        receipt: &KeyRequestCompletion,
        grant: Option<&GroupKey>,
    ) -> Result<bool, KeyStoreError> {
        let mut records = self.0.lock().unwrap();
        if records.fail_complete {
            return Err(KeyStoreError);
        }
        let scope = (address.clone(), topic);
        if !records
            .requests
            .get(&scope)
            .is_some_and(|request| request.canonical_id == receipt.canonical_id)
        {
            return Ok(false);
        }
        records.requests.remove(&scope);
        records
            .completed
            .insert((address.clone(), topic, receipt.request_id), *receipt);
        if let Some(key) = grant {
            records
                .keys
                .insert(address.clone(), GroupKey::from_bytes(*key.as_bytes()));
        }
        Ok(true)
    }

    /// Восстанавливает первое запечатанное тело ответа после неизвестного POST.
    fn load_reply(
        &self,
        address: &ChatKeyAddress,
        topic: Uuid,
        request: Uuid,
    ) -> Result<Option<StoredKeyReply>, KeyStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .replies
            .get(&(address.clone(), topic, request))
            .cloned())
    }

    /// Записывает точные данные первого ответа.
    fn save_reply(
        &self,
        address: &ChatKeyAddress,
        topic: Uuid,
        request: Uuid,
        reply: &StoredKeyReply,
    ) -> Result<(), KeyStoreError> {
        self.0
            .lock()
            .unwrap()
            .replies
            .insert((address.clone(), topic, request), reply.clone());
        Ok(())
    }
}

/// Восстанавливает пару таким же чтением байтов, как системный адаптер.
fn copy_request(request: &StoredKeyRequest) -> Result<StoredKeyRequest, KeyStoreError> {
    Ok(StoredKeyRequest {
        canonical_id: request.canonical_id,
        confirmed_request_id: request.confirmed_request_id,
        device_name: request.device_name.clone(),
        recovery: request.recovery.clone(),
        pair: RequestKeyPair::from_private_bytes(request.pair.private_bytes().as_ref())
            .map_err(|_| KeyStoreError)?,
    })
}

/// Подготавливает ожидающий локальный запрос без прямого подтверждения POST.
fn pending_request() -> (
    TestExchangeStore,
    ChatKeyAddress,
    KeyRequestTarget,
    KeyServiceMessage,
    PreparedKeyRequest,
) {
    let target = KeyRequestTarget {
        stream_id: Uuid::new_v4(),
        topic_id: Uuid::new_v4(),
        key_id: Uuid::new_v4(),
    };
    let member = Uuid::new_v4();
    let address = ChatKeyAddress {
        owner: OwnerContext {
            account_id: AccountId("test".into()),
            server: "https://example.test".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: member.to_string(),
        },
        device_id: Uuid::new_v4(),
        stream_id: target.stream_id,
        key_id: target.key_id,
    };
    let store = TestExchangeStore::default();
    let pending =
        super::super::prepare_request(&store, &address, target, "test".into(), None).unwrap();
    let row = KeyServiceMessage {
        id: target.public_id(pending.canonical_id),
        stream_id: target.stream_id,
        topic_id: target.topic_id,
        author_id: member,
        payload: pending.payload.clone(),
    };
    (store, address, target, row, pending)
}

/// Завершает отказ раньше прямого ответа POST, затем воспроизводит повтор события.
#[test]
fn rejected_request_loses_private_key_but_keeps_idempotent_receipt() {
    let (store, address, target, request, pending) = pending_request();
    let response = KeyServiceMessage {
        id: Uuid::new_v4(),
        author_id: Uuid::new_v4(),
        payload: KeyServicePayload::Reject(request.reply_target("remote-device").unwrap()),
        ..request.clone()
    };
    let result = finish_response(&store, &address, target, &request, &response, None)
        .unwrap()
        .unwrap();
    assert_eq!(result.0.outcome, KeyRequestOutcome::Rejected);
    assert!(!result.1);
    assert!(
        store
            .load_request(&address, target.topic_id)
            .unwrap()
            .is_none()
    );
    assert!(store.load(&address).unwrap().is_none());
    assert_eq!(
        finish_response(&store, &address, target, &request, &response, None).unwrap(),
        Some(result)
    );
    let receipt = KeyRequestReceipt {
        target,
        request_id: request.id,
    };
    confirm_request(&store, &address, receipt, &pending).unwrap();
}

/// Проверяет неизменяемый ответ после повтора и атомарность выдачи при сбое.
#[test]
fn grants_exact_key_once_and_does_not_restore_it_after_removal() {
    let (store, address, target, request, _) = pending_request();
    let issuer = ChatKeyAddress {
        device_id: Uuid::new_v4(),
        ..address.clone()
    };
    let key = GroupKey::from_bytes([19; 32]);
    store.save(&issuer, &key).unwrap();
    let outgoing = prepare_reply(&store, &issuer, &request, KeyRequestOutcome::Granted).unwrap();
    let retry = prepare_reply(&store, &issuer, &request, KeyRequestOutcome::Granted).unwrap();
    assert_eq!(outgoing.canonical_id, retry.canonical_id);
    assert_eq!(outgoing.payload, retry.payload);
    assert!(matches!(
        prepare_reply(&store, &issuer, &request, KeyRequestOutcome::Rejected),
        Err(KeyExchangeError::KeyConflict)
    ));
    let response = KeyServiceMessage {
        id: target.public_id(outgoing.canonical_id),
        author_id: Uuid::new_v4(),
        payload: outgoing.payload,
        ..request.clone()
    };
    let wrong = KeyServiceMessage {
        topic_id: Uuid::new_v4(),
        ..response.clone()
    };
    assert!(matches!(
        finish_response(&store, &address, target, &request, &wrong, None),
        Err(KeyExchangeError::InvalidResponse)
    ));
    store.0.lock().unwrap().fail_complete = true;
    assert!(matches!(
        finish_response(&store, &address, target, &request, &response, None),
        Err(KeyExchangeError::Store(_))
    ));
    assert!(store.load(&address).unwrap().is_none());
    assert!(
        store
            .load_request(&address, target.topic_id)
            .unwrap()
            .is_some()
    );
    store.0.lock().unwrap().fail_complete = false;
    let (receipt, inserted) = finish_response(&store, &address, target, &request, &response, None)
        .unwrap()
        .unwrap();
    assert!(inserted);
    assert_eq!(
        store.load(&address).unwrap().unwrap().as_bytes(),
        key.as_bytes()
    );
    assert_eq!(receipt.outcome, KeyRequestOutcome::Granted);
    assert!(
        store
            .load_request(&address, target.topic_id)
            .unwrap()
            .is_none()
    );
    store.remove(&address).unwrap();
    assert_eq!(
        finish_response(&store, &address, target, &request, &response, None).unwrap(),
        Some((receipt, false))
    );
    assert!(store.load(&address).unwrap().is_none());
}

/// До первого poll POST должна действовать та же активация и версия ключа.
#[test]
fn dispatch_checks_key_and_activation_at_the_point_of_admission() {
    use std::{cell::Cell, sync::Arc};

    let (store, address, target, _, _) = pending_request();
    let encryption = MessageEncryption::new(Arc::new(store));
    let session = EncryptionSession {
        owner: address.owner,
        generation: SessionGeneration(1),
        device_id: address.device_id,
    };
    let original = encryption.select_session(Some(session.clone())).unwrap();
    let key = GroupKey::from_bytes([12; 32]);
    encryption
        .save_key(&original, target.stream_id, target.key_id, &key)
        .unwrap();
    let prepared = encryption
        .prepare(&original, target.stream_id, target.key_id)
        .unwrap();
    let count = Cell::new(0);
    let post = || count.set(count.get() + 1);
    admit_dispatch(&encryption, &original, Some(&prepared), None, post).unwrap();
    assert_eq!(count.get(), 1);

    let operation = encryption.store_operations.lock().unwrap();
    assert!(matches!(
        admit_dispatch(&encryption, &original, Some(&prepared), None, post),
        Err(KeyExchangeError::DispatchBusy)
    ));
    drop(operation);
    // Даже повторная установка тех же байтов создаёт новую версию разрешения на выдачу.
    encryption
        .save_key(&original, target.stream_id, target.key_id, &key)
        .unwrap();
    assert!(matches!(
        admit_dispatch(&encryption, &original, Some(&prepared), None, post),
        Err(KeyExchangeError::Stale)
    ));
    let prepared = encryption
        .prepare(&original, target.stream_id, target.key_id)
        .unwrap();
    encryption
        .remove_key(&original, target.stream_id, target.key_id)
        .unwrap();
    assert!(matches!(
        admit_dispatch(&encryption, &original, Some(&prepared), None, post),
        Err(KeyExchangeError::Stale)
    ));

    encryption.select_session(None);
    let current = encryption.select_session(Some(session)).unwrap();
    assert!(matches!(
        admit_dispatch(&encryption, &original, None, None, post),
        Err(KeyExchangeError::Stale)
    ));
    admit_dispatch(&encryption, &current, None, None, post).unwrap();
    assert_eq!(count.get(), 2);
}

/// Старый POST не начинает сетевую операцию после замены запроса во время сверки GET.
#[test]
fn dispatch_checks_pending_identity_after_reconciliation() {
    use super::super::{forget_pending_request, remember_pending_request};
    use std::{cell::Cell, sync::Arc};

    let (store, address, target, request, old) = pending_request();
    let engine = MessageEncryption::new(Arc::new(store));
    let activation = engine
        .select_session(Some(EncryptionSession {
            owner: address.owner,
            generation: SessionGeneration(1),
            device_id: address.device_id,
        }))
        .unwrap();
    let current_id = Uuid::new_v4();
    let post_count = Cell::new(0);
    let post = || post_count.set(post_count.get() + 1);
    remember_pending_request(&engine, &activation, target, old.canonical_id);
    // Пока GET ожидал ответ, действие пользователя подготовило другой запрос этого же поколения.
    remember_pending_request(&engine, &activation, target, current_id);
    assert!(matches!(
        admit_dispatch(
            &engine,
            &activation,
            None,
            Some((target, old.canonical_id)),
            post
        ),
        Err(KeyExchangeError::Stale)
    ));
    assert!(matches!(
        admit_dispatch(&engine, &activation, None, Some((target, request.id)), post),
        Err(KeyExchangeError::Stale)
    ));
    forget_pending_request(&engine, &activation, target, old.canonical_id);
    admit_dispatch(&engine, &activation, None, Some((target, current_id)), post).unwrap();
    forget_pending_request(&engine, &activation, target, current_id);
    assert!(matches!(
        admit_dispatch(&engine, &activation, None, Some((target, current_id)), post),
        Err(KeyExchangeError::Stale)
    ));
    assert_eq!(post_count.get(), 1);
}

/// Перед заменой требует подтверждённый исходный конверт и прежние байты, затем проверяет новый ключ.
#[test]
fn recovery_rejects_stale_evidence_and_unproven_key_material() {
    use crate::messenger::encryption::{encrypt_message, key_exchange::recovery::capture_recovery};

    let (store, address, target, request, _) = pending_request();
    let previous = GroupKey::from_bytes([31; 32]);
    let correct = GroupKey::from_bytes([32; 32]);
    let wrong = GroupKey::from_bytes([33; 32]);
    store.save(&address, &previous).unwrap();
    let ciphertext = encrypt_message(
        &correct,
        target.stream_id,
        target.key_id,
        &[99; 32],
        "Текст для проверки",
    )
    .unwrap();
    let recovery = KeyRecovery {
        message_id: Uuid::new_v4(),
        ciphertext,
        previous_key_fingerprint: fingerprint(&previous),
    };
    assert_eq!(
        capture_recovery(&store, &address, recovery.source()).unwrap(),
        recovery
    );

    for candidate in [&wrong, &correct] {
        let previous_request = store
            .load_request(&address, target.topic_id)
            .unwrap()
            .map(|request| request.canonical_id);
        let pending = super::super::prepare_request(
            &store,
            &address,
            target,
            "test".into(),
            Some((recovery.clone(), previous_request)),
        )
        .unwrap();
        let request = KeyServiceMessage {
            id: target.public_id(pending.canonical_id),
            payload: pending.payload,
            ..request.clone()
        };
        let remote = request.reply_target("issuer-device").unwrap();
        let KeyServicePayload::Request { public_key, .. } = &request.payload else {
            panic!("expected a request");
        };
        let proof = seal_group_key(
            candidate,
            RequestPublicKey::from_base64(public_key).unwrap(),
            remote.grant_context(target.stream_id),
        )
        .unwrap();
        let response = KeyServiceMessage {
            id: Uuid::new_v4(),
            author_id: Uuid::new_v4(),
            payload: KeyServicePayload::Grant {
                target: remote,
                content: proof,
            },
            ..request.clone()
        };
        assert!(matches!(
            finish_response(&store, &address, target, &request, &response, None),
            Err(KeyExchangeError::Stale)
        ));
        store.save(&address, &wrong).unwrap();
        assert!(matches!(
            finish_response(
                &store,
                &address,
                target,
                &request,
                &response,
                Some(&recovery)
            ),
            Err(KeyExchangeError::Stale)
        ));
        store.save(&address, &previous).unwrap();
        let (receipt, published) = finish_response(
            &store,
            &address,
            target,
            &request,
            &response,
            Some(&recovery),
        )
        .unwrap()
        .unwrap();
        if candidate.as_bytes() == correct.as_bytes() {
            assert_eq!(receipt.outcome, KeyRequestOutcome::Granted);
            assert!(published);
            assert_eq!(
                store.load(&address).unwrap().unwrap().as_bytes(),
                correct.as_bytes()
            );
        } else {
            assert_eq!(receipt.outcome, KeyRequestOutcome::Rejected);
            assert!(!published);
            assert_eq!(
                store.load(&address).unwrap().unwrap().as_bytes(),
                previous.as_bytes()
            );
        }
    }
    assert!(matches!(
        capture_recovery(&store, &address, recovery.source()),
        Err(KeyExchangeError::KeyAlreadyUsable)
    ));
}

/// Отбрасывает восстановление со старым UUID после смены запроса во время сверки.
#[test]
fn replaces_pending_recovery_only_if_verified_request_identity_still_matches() {
    let (store, address, target, _, _) = pending_request();
    let canonical = store
        .load_request(&address, target.topic_id)
        .unwrap()
        .unwrap()
        .canonical_id;
    let evidence = KeyRecovery {
        message_id: Uuid::new_v4(),
        ciphertext: "first verified ciphertext".into(),
        previous_key_fingerprint: [17; 32],
    };
    let replacement = super::super::prepare_request(
        &store,
        &address,
        target,
        "device".into(),
        Some((evidence.clone(), Some(canonical))),
    )
    .unwrap();
    assert_ne!(replacement.canonical_id, canonical);
    let same = super::super::prepare_request(
        &store,
        &address,
        target,
        "device".into(),
        Some((evidence.clone(), Some(canonical))),
    )
    .unwrap();
    assert_eq!(same.canonical_id, replacement.canonical_id);
    let outdated = KeyRecovery {
        message_id: Uuid::new_v4(),
        ..evidence
    };
    assert!(matches!(
        super::super::prepare_request(
            &store,
            &address,
            target,
            "device".into(),
            Some((outdated, Some(canonical)))
        ),
        Err(KeyExchangeError::Stale)
    ));
    let current = store
        .load_request(&address, target.topic_id)
        .unwrap()
        .unwrap();
    assert_eq!(current.canonical_id, replacement.canonical_id);
}
