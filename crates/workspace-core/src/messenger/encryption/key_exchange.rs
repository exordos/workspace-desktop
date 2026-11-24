//! Ведёт запросы ключей устройств и восстанавливает неизвестный результат отправки.
//! Пара запроса сохраняется до сети, а публичный UUID закрепляется после проверки ответа.

mod recovery;
mod responses;

use std::{future::poll_fn, pin::pin, sync::Arc, task::Poll};

use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{AuthError, Service},
    sync::Sync,
};

use super::super::{MessengerSource, history::OpenHistoryError, send::SendError};
use super::{
    ChatKeyAddress, ChatKeyStore, EncryptionActivation, KeyStoreError, MessageDecryption,
    MessageEncryption, key_address,
};
use super::{
    grant::{GrantError, RequestKeyPair},
    key_service::{KeyServiceMessage, KeyServicePayload},
};

pub use recovery::KeyRecovery;
pub use responses::{
    KeyReplyCapabilities, KeyRequestCompletion, KeyRequestOutcome, KeyRequestStatus, StoredKeyReply,
};

/// Размещение и поколение, для которых устройство просит доступ.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyRequestTarget {
    pub stream_id: Uuid, // Чат, где находится защищённое сообщение.
    pub topic_id: Uuid,  // Точная исходная тема запроса и его ответа.
    pub key_id: Uuid,    // Поколение ключа, даже если у чата уже есть более новое.
}

impl KeyRequestTarget {
    /// Вычисляет публичный UUID размещения по серверному контракту сообщений.
    pub(crate) fn public_id(self, canonical_id: Uuid) -> Uuid {
        Uuid::new_v5(&self.topic_id, canonical_id.to_string().as_bytes())
    }
}

/// Секрет незавершённого запроса. Его целиком сохраняет защищённый адаптер.
pub struct StoredKeyRequest {
    pub canonical_id: Uuid, // UUID первого POST, который используется при повторах.
    pub confirmed_request_id: Option<Uuid>, // Публичный UUID после проверки. `None` до подтверждения доставки.
    pub device_name: String,                // Имя устройства из первого запроса.
    pub pair: RequestKeyPair,               // Приватный ключ и его публичная часть для HPKE.
    pub recovery: Option<KeyRecovery>, // Проверяемый конверт и прежний ключ при запросе восстановления.
}

/// Подтверждение, что сервер разместил запрос в нужной теме.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyRequestReceipt {
    pub request_id: Uuid, // Публичный UUID сообщения, на который спроецирован запрос.
    pub target: KeyRequestTarget, // Точный чат, тема и поколение для ожидаемого ответа.
}

/// Причины, по которым запрос не удалось опубликовать или подтвердить.
#[derive(Debug, Error)]
pub enum KeyExchangeError {
    #[error("invalid key request target or device name")]
    InvalidInput, // Темы, поколения и устройства должны иметь реальные идентификаторы.
    #[error("the selected key exchange session changed")]
    Stale, // Выбран другой вход, поэтому старую работу нельзя применить.
    #[error("key exchange response did not match the stored request")]
    InvalidResponse, // Сервер вернул другой маршрут, автора или тело запроса.
    #[error("key exchange worker did not finish")]
    Worker, // Рабочий поток остановился до результата.
    #[error("another key operation is still in progress")]
    DispatchBusy, // Хранилище меняет ключи, запрос можно повторить.
    #[error("current key sharing policy disallows this response")]
    Forbidden, // Текущая роль или политика запрещает выдачу или отказ.
    #[error("the exact group key is not available on this device")]
    MissingKey, // Устройство потеряло ключ, которым собиралось поделиться.
    #[error("the received key differs from the current local key")]
    KeyConflict, // Ключ действующего поколения можно заменить только при явном восстановлении.
    #[error("the current local key already opens this message")]
    KeyAlreadyUsable, // Сообщение уже доступно, менять ключ не требуется.
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход завершился или сервер не выдал токен Messenger.
    #[error(transparent)]
    Access(#[from] OpenHistoryError), // Стрим или тема недоступны для запроса ключа.
    #[error(transparent)]
    Send(#[from] SendError), // Сервер отказал, либо результат POST пока неизвестен.
    #[error(transparent)]
    Store(#[from] KeyStoreError), // Не удалось прочитать или записать секрет.
    #[error(transparent)]
    History(#[from] crate::storage::StorageError), // Локальная версия сообщения недоступна.
    #[error(transparent)]
    Grant(#[from] GrantError), // Локальный генератор или ключ HPKE не готовы.
}

/// Штатные команды обмена ключами для выбранного аккаунта и локального устройства.
pub struct KeyExchange {
    messenger: Arc<MessengerSource>, // Общий сетевой адаптер мессенджера.
    sync: Arc<Sync>,                 // Текущие поколения и сервисные токены аккаунтов.
    encryption: Arc<MessageEncryption>, // Тот же менеджер ключей, который используют история и отправка.
}

impl KeyExchange {
    /// Связывает сетевые команды с авторизацией и защищёнными данными профиля.
    pub fn new(
        messenger: Arc<MessengerSource>,
        sync: Arc<Sync>,
        encryption: Arc<MessageEncryption>,
    ) -> Self {
        Self {
            messenger,
            sync,
            encryption,
        }
    }

    /// Готовит запрос один раз и подтверждает его размещение в исходной теме.
    pub async fn request_key(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        device_name: &str,
    ) -> Result<KeyRequestReceipt, KeyExchangeError> {
        self.request_generation(activation, target, device_name, None)
            .await
    }

    /// Общая отправка запроса с предварительной сверкой исходного конверта при восстановлении.
    async fn request_generation(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        device_name: &str,
        source: Option<recovery::RecoverySource>,
    ) -> Result<KeyRequestReceipt, KeyExchangeError> {
        let session = activation.session();
        if target.stream_id.is_nil()
            || target.topic_id.is_nil()
            || target.key_id.is_nil()
            || session.device_id.is_nil()
            || device_name.trim().is_empty()
            || device_name.chars().count() > 255
        {
            return Err(KeyExchangeError::InvalidInput);
        }
        let author = Uuid::parse_str(&session.owner.user_id)
            .ok()
            .filter(|id| !id.is_nil())
            .ok_or(KeyExchangeError::InvalidInput)?;
        self.require_current(activation)?;
        let owner = &session.owner;
        let token = self
            .sync
            .auth()
            .access_token_for_generation(owner, Service::Messenger, session.generation)
            .await?;
        self.require_current(activation)?;
        let access = self
            .messenger
            .rest
            .check_key_service_target(owner, &token, target)
            .await;
        self.require_current(activation)?;
        access?;
        let recovery = if let Some(source) = source {
            let previous = self
                .with_request_store(activation, target, move |encryption, address| {
                    Ok(encryption
                        .store
                        .load_request(address, target.topic_id)?
                        .map(|request| request.canonical_id))
                })
                .await?;
            let message = self
                .verify_recovery_source(activation, &token, target, &source)
                .await?;
            Some((source, message, previous))
        } else {
            None
        };

        let name = device_name.to_owned();
        let sync = self.sync.clone();
        let selected = activation.clone();
        let request = self
            .with_request_store(activation, target, move |encryption, address| {
                let store = encryption.store.as_ref();
                let recovery = if let Some((source, message, previous)) = recovery {
                    if !sync.matches_key_recovery_source(&address.owner, &message)? {
                        return Err(KeyExchangeError::Stale);
                    }
                    Some((
                        recovery::capture_recovery(store, address, source)?,
                        previous,
                    ))
                } else {
                    None
                };
                let prepared = prepare_request(store, address, target, name, recovery)?;
                remember_pending_request(encryption, &selected, target, prepared.canonical_id);
                Ok(prepared)
            })
            .await?;
        let receipt = KeyRequestReceipt {
            request_id: target.public_id(request.canonical_id),
            target,
        };
        if request.confirmed {
            return Ok(receipt);
        }
        log::debug!("key request delivery op_id={}", request.canonical_id);
        let probe = self
            .messenger
            .rest
            .lookup_key_service_message(owner, &token, target, receipt.request_id)
            .await;
        self.require_current(activation)?;
        let delivered = match probe? {
            Some(message) => message,
            None => {
                let key = if let Some(recovery) = &request.recovery {
                    self.verify_recovery_source(activation, &token, target, &recovery.source())
                        .await?;
                    Some(
                        self.prepare_delivery_key(
                            activation,
                            target,
                            recovery.previous_key_fingerprint,
                        )
                        .await?,
                    )
                } else {
                    None
                };
                self.post_key_message(
                    activation,
                    &token,
                    target,
                    request.canonical_id,
                    &request.payload,
                    key.as_ref(),
                )
                .await?
            }
        };
        verify_request(&delivered, author, receipt, &request)?;
        self.with_request_store(activation, target, move |encryption, address| {
            confirm_request(encryption.store.as_ref(), address, receipt, &request)
        })
        .await?;
        Ok(receipt)
    }

    /// Отправляет уже сохранённое тело один раз и после неизвестного результата сверяет GET.
    async fn post_key_message(
        &self,
        activation: &EncryptionActivation,
        token: &str,
        target: KeyRequestTarget,
        canonical_id: Uuid,
        payload: &KeyServicePayload,
        key: Option<&MessageDecryption>,
    ) -> Result<KeyServiceMessage, KeyExchangeError> {
        let session = activation.session();
        let owner = &session.owner;
        let pending =
            matches!(payload, KeyServicePayload::Request { .. }).then_some((target, canonical_id));
        let mut request = pin!(self.messenger.rest.post_key_service_once(
            owner,
            token,
            target,
            canonical_id,
            payload
        ));
        let mut dispatched = false;
        let result = poll_fn(|context| {
            if dispatched {
                return request.as_mut().poll(context).map(Ok);
            }
            // Первый poll начинает POST под теми же проверками, что разрешают выбранный вход и ключ.
            // После допуска завершение сети не держит блокировки и сверяется с тем же входом.
            let first = self
                .sync
                .auth()
                .with_active_generation(owner, session.generation, || {
                    admit_dispatch(&self.encryption, activation, key, pending, || {
                        request.as_mut().poll(context)
                    })
                });
            match first
                .map_err(KeyExchangeError::from)
                .and_then(|checked| checked)
            {
                Ok(result) => {
                    dispatched = true;
                    result.map(Ok)
                }
                Err(error) => Poll::Ready(Err(error)),
            }
        })
        .await?;
        self.require_current(activation)?;
        match result {
            Err(SendError::Unknown) => {
                let check = self
                    .messenger
                    .rest
                    .lookup_key_service_message(
                        owner,
                        token,
                        target,
                        target.public_id(canonical_id),
                    )
                    .await;
                self.require_current(activation)?;
                Ok(check?.ok_or(SendError::Unknown)?)
            }
            result => Ok(result?),
        }
    }

    /// Проверяет и выбранный профиль, и срок действия входа перед продолжением.
    fn require_current(&self, activation: &EncryptionActivation) -> Result<(), KeyExchangeError> {
        let session = activation.session();
        self.sync
            .auth()
            .with_active_generation(&session.owner, session.generation, || ())?;
        require_selected(&self.encryption, activation)
    }

    /// Выполняет защищённую операцию вне UI под общим барьером, который использует расшифровка.
    async fn with_request_store<T: Send + 'static>(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        operation: impl FnOnce(&MessageEncryption, &ChatKeyAddress) -> Result<T, KeyExchangeError>
        + Send
        + 'static,
    ) -> Result<T, KeyExchangeError> {
        let encryption = self.encryption.clone();
        let sync = self.sync.clone();
        let selected = activation.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _key_access = encryption.store_operations.lock().unwrap();
            let session = selected.session();
            let address = key_address(session, target.stream_id, target.key_id);
            sync.auth()
                .with_active_generation(&session.owner, session.generation, || {
                    require_selected(&encryption, &selected)?;
                    operation(&encryption, &address)
                })?
        })
        .await
        .map_err(|_| KeyExchangeError::Worker)?;
        self.require_current(activation)?;
        result
    }
}

/// Даёт начать POST только при действующем выборе и той же версии ключа.
fn admit_dispatch<T>(
    encryption: &MessageEncryption,
    activation: &EncryptionActivation,
    key: Option<&MessageDecryption>,
    pending: Option<(KeyRequestTarget, Uuid)>,
    dispatch: impl FnOnce() -> T,
) -> Result<T, KeyExchangeError> {
    // Активная операция Keychain может заменить или удалить ключ. Не ждём её на сетевом потоке.
    let _operation = encryption
        .store_operations
        .try_lock()
        .map_err(|_| KeyExchangeError::DispatchBusy)?;
    let selection = encryption.selection.lock().unwrap();
    let current = selection
        .as_ref()
        .filter(|current| &current.activation == activation)
        .ok_or(KeyExchangeError::Stale)?;
    if key.is_some_and(|key| !key.access.is_current(Some(current))) {
        return Err(KeyExchangeError::Stale);
    }
    if pending.is_some_and(|(target, id)| current.pending_requests.get(&target) != Some(&id)) {
        return Err(KeyExchangeError::Stale);
    }
    Ok(dispatch())
}

/// Устанавливает допуск POST после долговечной подготовки под тем же барьером операций ключа.
fn remember_pending_request(
    encryption: &MessageEncryption,
    activation: &EncryptionActivation,
    target: KeyRequestTarget,
    id: Uuid,
) {
    let mut selected = encryption.selection.lock().unwrap();
    if let Some(selected) = selected
        .as_mut()
        .filter(|value| &value.activation == activation)
    {
        selected.pending_requests.insert(target, id);
    }
}

/// Отзывает подготовленный POST после атомарного завершения того же запроса.
fn forget_pending_request(
    encryption: &MessageEncryption,
    activation: &EncryptionActivation,
    target: KeyRequestTarget,
    id: Uuid,
) {
    let mut selected = encryption.selection.lock().unwrap();
    if let Some(selected) = selected
        .as_mut()
        .filter(|value| &value.activation == activation)
        && selected.pending_requests.get(&target) == Some(&id)
    {
        selected.pending_requests.remove(&target);
    }
}

/// Снимок для сети. Приватный ключ остаётся в защищённом хранилище.
struct PreparedKeyRequest {
    canonical_id: Uuid,         // Стабильный UUID для POST и последующей сверки GET.
    payload: KeyServicePayload, // Точные публичные поля, сохранённые перед первой отправкой.
    confirmed: bool,            // Ранее подтверждённое размещение позволяет обойтись без POST.
    recovery: Option<KeyRecovery>, // Условия, при которых этот запрос ещё может восстановить ключ.
}

/// Проверяет разрешение от оболочки отдельно от сервисного токена аккаунта.
fn require_selected(
    encryption: &MessageEncryption,
    activation: &EncryptionActivation,
) -> Result<(), KeyExchangeError> {
    let active = encryption.selection.lock().unwrap();
    if active
        .as_ref()
        .is_some_and(|current| &current.activation == activation)
    {
        return Ok(());
    }
    Err(KeyExchangeError::Stale)
}

/// Создаёт пару и UUID до отправки или восстанавливает уже сохранённое намерение.
fn prepare_request(
    store: &dyn ChatKeyStore,
    address: &ChatKeyAddress,
    target: KeyRequestTarget,
    device_name: String,
    recovery: Option<(KeyRecovery, Option<Uuid>)>,
) -> Result<PreparedKeyRequest, KeyExchangeError> {
    let active = store.load_request(address, target.topic_id)?;
    let request = match active {
        Some(stored)
            if recovery
                .as_ref()
                .is_none_or(|(evidence, _)| stored.recovery.as_ref() == Some(evidence)) =>
        {
            stored
        }
        pending => {
            let pending = pending.map(|request| request.canonical_id);
            if recovery
                .as_ref()
                .is_some_and(|(_, previous)| *previous != pending)
            {
                return Err(KeyExchangeError::Stale);
            }
            let prepared = StoredKeyRequest {
                canonical_id: fresh_message_uuid()?,
                confirmed_request_id: None,
                device_name,
                pair: RequestKeyPair::generate()?,
                recovery: recovery.map(|(evidence, _)| evidence),
            };
            if let Some(previous) = pending {
                if !store.replace_request(address, target.topic_id, previous, &prepared)? {
                    return Err(KeyExchangeError::Stale);
                }
            } else {
                store.save_request(address, target.topic_id, &prepared)?;
            }
            prepared
        }
    };
    if request.canonical_id.is_nil()
        || request
            .confirmed_request_id
            .is_some_and(|id| id != target.public_id(request.canonical_id))
    {
        return Err(KeyStoreError.into());
    }
    Ok(PreparedKeyRequest {
        canonical_id: request.canonical_id,
        payload: request_payload(address, &request),
        confirmed: request.confirmed_request_id.is_some(),
        recovery: request.recovery,
    })
}

/// Создаёт UUID служебного сообщения и возвращает ошибку, если энтропия недоступна.
fn fresh_message_uuid() -> Result<Uuid, GrantError> {
    let mut bytes = [0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| GrantError::RandomnessUnavailable)?;
    Ok(uuid::Builder::from_random_bytes(bytes).into_uuid())
}

/// Восстанавливает неизменяемые публичные поля отправляемого запроса.
fn request_payload(address: &ChatKeyAddress, request: &StoredKeyRequest) -> KeyServicePayload {
    KeyServicePayload::Request {
        key_uuid: address.key_id,
        device_id: address.device_id.to_string(),
        device_name: request.device_name.clone(),
        public_key: request.pair.public_key().to_base64(),
    }
}

/// Проверяет, что ответ относится к запросу именно этого устройства и текущего автора.
fn verify_request(
    row: &KeyServiceMessage,
    author: Uuid,
    receipt: KeyRequestReceipt,
    request: &PreparedKeyRequest,
) -> Result<(), KeyExchangeError> {
    if row.id != receipt.request_id
        || row.stream_id != receipt.target.stream_id
        || row.topic_id != receipt.target.topic_id
        || row.author_id != author
        || row.payload != request.payload
    {
        return Err(KeyExchangeError::InvalidResponse);
    }
    Ok(())
}

/// Закрепляет публичный UUID в том же секрете. Пара остаётся доступной до ответа.
fn confirm_request(
    store: &dyn ChatKeyStore,
    address: &ChatKeyAddress,
    receipt: KeyRequestReceipt,
    prepared: &PreparedKeyRequest,
) -> Result<(), KeyExchangeError> {
    let topic_id = receipt.target.topic_id;
    // Realtime может завершить опубликованный запрос раньше прямого ответа POST.
    if store
        .load_request_completion(address, topic_id, receipt.request_id)?
        .is_some_and(|completion| {
            completion.canonical_id == prepared.canonical_id
                && completion.request_id == receipt.request_id
        })
    {
        return Ok(());
    }
    let mut request = store
        .load_request(address, topic_id)?
        .ok_or(KeyStoreError)?;
    if request.canonical_id != prepared.canonical_id
        || request_payload(address, &request) != prepared.payload
    {
        return Err(KeyExchangeError::Stale);
    }
    match request.confirmed_request_id {
        None => {
            request.confirmed_request_id = Some(receipt.request_id);
            store.save_request(address, topic_id, &request)?;
            Ok(())
        }
        Some(id) if id == receipt.request_id => Ok(()),
        Some(_) => Err(KeyStoreError.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use super::{super::GroupKey, *};
    use crate::context::{AccountId, OwnerContext};

    /// Хранит пары и квитанции так, как если бы следующий запуск прочитал их из Keychain.
    #[derive(Default)]
    struct TestRequestStore(Mutex<HashMap<(ChatKeyAddress, Uuid), StoredKeyRequest>>); // Запросы по полному адресу и теме.

    impl ChatKeyStore for TestRequestStore {
        /// В проверках отправки запроса групповой ключ не читается.
        fn load(&self, _: &ChatKeyAddress) -> Result<Option<GroupKey>, KeyStoreError> {
            Err(KeyStoreError)
        }

        /// В проверках отправки запроса групповой ключ не записывается.
        fn save(&self, _: &ChatKeyAddress, _: &GroupKey) -> Result<(), KeyStoreError> {
            Err(KeyStoreError)
        }

        /// В проверках отправки запроса групповой ключ не удаляется.
        fn remove(&self, _: &ChatKeyAddress) -> Result<(), KeyStoreError> {
            Err(KeyStoreError)
        }

        /// Восстанавливает пару тем же способом, что платформенный адаптер.
        fn load_request(
            &self,
            address: &ChatKeyAddress,
            topic_id: Uuid,
        ) -> Result<Option<StoredKeyRequest>, KeyStoreError> {
            let states = self.0.lock().unwrap();
            states
                .get(&(address.clone(), topic_id))
                .map(copy_request)
                .transpose()
        }

        /// Сохраняет независимую копию записи запроса.
        fn save_request(
            &self,
            address: &ChatKeyAddress,
            topic_id: Uuid,
            request: &StoredKeyRequest,
        ) -> Result<(), KeyStoreError> {
            self.0
                .lock()
                .unwrap()
                .insert((address.clone(), topic_id), copy_request(request)?);
            Ok(())
        }

        /// Этот тестовый адаптер не получает ответы на подготовленные запросы.
        fn load_request_completion(
            &self,
            _: &ChatKeyAddress,
            _: Uuid,
            _: Uuid,
        ) -> Result<Option<KeyRequestCompletion>, KeyStoreError> {
            Ok(None)
        }
    }

    /// Имитирует восстановление приватного ключа из сохранённых байтов.
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

    /// Возвращает полный адрес и размещение тестового запроса.
    fn fixture() -> (ChatKeyAddress, KeyRequestTarget) {
        let target = KeyRequestTarget {
            stream_id: Uuid::new_v4(),
            topic_id: Uuid::new_v4(),
            key_id: Uuid::new_v4(),
        };
        let address = ChatKeyAddress {
            owner: OwnerContext {
                account_id: AccountId("test".into()),
                server: "https://example.test".into(),
                organization_id: "org".into(),
                project_id: "project".into(),
                user_id: Uuid::new_v4().to_string(),
            },
            device_id: Uuid::new_v4(),
            stream_id: target.stream_id,
            key_id: target.key_id,
        };
        (address, target)
    }

    /// После неизвестного POST повтор использует сохранённый UUID, пару и имя.
    #[test]
    fn reuses_exact_pending_request_and_remembers_confirmed_placement() {
        let (address, target) = fixture();
        let store = TestRequestStore::default();
        let first = prepare_request(&store, &address, target, "first name".into(), None).unwrap();
        assert!(!first.confirmed);
        let stored = store
            .load_request(&address, target.topic_id)
            .unwrap()
            .unwrap();
        assert!(stored.confirmed_request_id.is_none());
        let retry =
            prepare_request(&store, &address, target, "a different name".into(), None).unwrap();
        assert_eq!(retry.canonical_id, first.canonical_id);
        assert_eq!(retry.payload, first.payload);
        let receipt = KeyRequestReceipt {
            target,
            request_id: target.public_id(first.canonical_id),
        };
        confirm_request(&store, &address, receipt, &first).unwrap();
        confirm_request(&store, &address, receipt, &retry).unwrap();
        let confirmed =
            prepare_request(&store, &address, target, "after restart".into(), None).unwrap();
        assert!(confirmed.confirmed);
        assert_eq!(confirmed.canonical_id, first.canonical_id);
    }

    /// Разные владельцы, устройства и темы никогда не переиспользуют приватную пару.
    #[test]
    fn scopes_pending_request_to_owner_device_chat_generation_and_topic() {
        let (address, target) = fixture();
        let store = TestRequestStore::default();
        let original = prepare_request(&store, &address, target, "device".into(), None).unwrap();
        let mut other_owner = address.clone();
        other_owner.owner.account_id = AccountId("other".into());
        for (different, other_target) in [
            (other_owner, target),
            (
                ChatKeyAddress {
                    device_id: Uuid::new_v4(),
                    ..address.clone()
                },
                target,
            ),
            (
                address.clone(),
                KeyRequestTarget {
                    topic_id: Uuid::new_v4(),
                    ..target
                },
            ),
        ] {
            let separate =
                prepare_request(&store, &different, other_target, "device".into(), None).unwrap();
            assert_ne!(original.canonical_id, separate.canonical_id);
            assert_ne!(original.payload, separate.payload);
        }
    }

    /// Привязывает размещение только к точному автору, маршруту и сохранённой паре.
    #[test]
    fn verifies_endpoint_echo_before_binding_public_uuid() {
        let (address, target) = fixture();
        let store = TestRequestStore::default();
        let prepared = prepare_request(&store, &address, target, "device".into(), None).unwrap();
        let author = Uuid::parse_str(&address.owner.user_id).unwrap();
        let receipt = KeyRequestReceipt {
            target,
            request_id: target.public_id(prepared.canonical_id),
        };
        let mut response = KeyServiceMessage {
            id: receipt.request_id,
            stream_id: target.stream_id,
            topic_id: target.topic_id,
            author_id: author,
            payload: prepared.payload.clone(),
        };
        assert!(verify_request(&response, author, receipt, &prepared).is_ok());
        assert!(verify_request(&response, Uuid::new_v4(), receipt, &prepared).is_err());
        let KeyServicePayload::Request { public_key, .. } = &mut response.payload else {
            panic!("expected request");
        };
        *public_key = RequestKeyPair::generate().unwrap().public_key().to_base64();
        assert!(verify_request(&response, author, receipt, &prepared).is_err());
    }
}
