//! Читает и готовит зашифрованные сообщения в выбранной сессии Messenger.
//! Сначала описаны ключи и системное хранилище, затем проверка сессии и формат сообщения.

pub mod fragments;
pub mod grant;
pub mod key_exchange;
pub mod key_service;
pub mod lifecycle;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::{
    aead, hkdf, hmac,
    rand::{SecureRandom, SystemRandom},
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{broadcast, watch};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    context::{OwnerContext, SessionGeneration},
    storage::{HistoryMessage, MessageBody},
    sync::FragmentOriginsPermit,
};

const KEY_BYTES: usize = 32;
const TAG_BYTES: usize = 16;
const KEY_CHANGE_BUFFER: usize = 64;

/// Выбранный вход и устройство, от имени которых можно читать ключи.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncryptionSession {
    pub owner: OwnerContext,           // Владелец данных Messenger.
    pub generation: SessionGeneration, // Поколение входа, выданное авторизацией.
    pub device_id: Uuid,               // Устойчивый UUID установки клиента.
}

/// Разрешение, полученное при выборе аккаунта. Рабочая задача сохраняет его до запуска.
#[derive(Clone, Debug)]
pub struct EncryptionActivation(Arc<EncryptionSession>); // Отдельный выбор даже при возврате к тому же входу.

impl EncryptionActivation {
    /// Возвращает владельца, устройство и поколение входа для этой активации.
    pub fn session(&self) -> &EncryptionSession {
        &self.0
    }
}

impl PartialEq for EncryptionActivation {
    /// Сравнивает сам выбор аккаунта, включая повторный переход к тому же входу.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for EncryptionActivation {}

/// Адрес записи в защищённом хранилище. Сохраняется между входами того же владельца.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChatKeyAddress {
    pub owner: OwnerContext, // Аккаунт, сервер и проект, которым принадлежит ключ.
    pub device_id: Uuid,     // Устройство, которому доступен ключ.
    pub stream_id: Uuid,     // Чат, для которого выпущен ключ.
    pub key_id: Uuid,        // Точное поколение ключа, указанное в сообщении.
}

/// Ключ чата. При освобождении память с его байтами очищается.
pub struct GroupKey(Zeroizing<[u8; KEY_BYTES]>); // 32 байта ключа, без кодирования.

impl GroupKey {
    /// Принимает байты ключа после чтения из хранилища или проверки выдачи.
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Открывает байты ключа на время записи в защищённое хранилище.
    pub fn as_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }
}

/// Системное хранилище не смогло прочитать или изменить ключ.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("encrypted chat key storage is unavailable")]
pub struct KeyStoreError;

/// Порядок доступного ключа в защищённом хранилище данного чата и устройства.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeldChatKey {
    pub key_id: Uuid,                  // UUID доступного поколения.
    pub send_order: u64, // Порядок выбора для новой отправки, 0 если ещё не выбирался.
    pub received_order: u64, // Порядок первого сохранения ключа на этом устройстве.
    pub added_at_unix_ms: Option<u64>, // Время первого сохранения в мс с Unix epoch, если известно.
}

/// Операции системного адаптера с отдельными записями ключей чата.
pub trait ChatKeyStore: Send + Sync + 'static {
    /// Читает ключ. `None` означает, что на устройстве нет такой записи.
    fn load(&self, address: &ChatKeyAddress) -> Result<Option<GroupKey>, KeyStoreError>;

    /// Сохраняет ключ или обновляет байты для того же адреса.
    fn save(&self, address: &ChatKeyAddress, key: &GroupKey) -> Result<(), KeyStoreError>;

    /// Удаляет ключ и отменяет ожидающие запросы его поколения, сохраняя квитанции уже принятых ответов.
    fn remove(&self, address: &ChatKeyAddress) -> Result<(), KeyStoreError>;

    /// Читает прерванный запрос из защищённой записи того же устройства и темы.
    fn load_request(
        &self,
        _address: &ChatKeyAddress,
        _topic_id: Uuid,
    ) -> Result<Option<key_exchange::StoredKeyRequest>, KeyStoreError> {
        Err(KeyStoreError)
    }

    /// Атомарно сохраняет пару ключей и состояние её запроса в защищённом хранилище.
    fn save_request(
        &self,
        _address: &ChatKeyAddress,
        _topic_id: Uuid,
        _request: &key_exchange::StoredKeyRequest,
    ) -> Result<(), KeyStoreError> {
        Err(KeyStoreError)
    }

    /// Атомарно заменяет незавершённый запрос только при совпадении прежнего канонического UUID.
    fn replace_request(
        &self,
        _address: &ChatKeyAddress,
        _topic_id: Uuid,
        _expected_canonical_id: Uuid,
        _request: &key_exchange::StoredKeyRequest,
    ) -> Result<bool, KeyStoreError> {
        Err(KeyStoreError)
    }

    /// Читает квитанцию завершённого запроса, чтобы повторное событие не возвращало ключ.
    fn load_request_completion(
        &self,
        _address: &ChatKeyAddress,
        _topic_id: Uuid,
        _request_id: Uuid,
    ) -> Result<Option<key_exchange::KeyRequestCompletion>, KeyStoreError> {
        Err(KeyStoreError)
    }

    /// В одной защищённой записи заменяет нужный запрос квитанцией и при выдаче хранит ключ.
    /// `false` означает, что соответствующего незавершённого запроса уже нет.
    fn complete_request(
        &self,
        _address: &ChatKeyAddress,
        _topic_id: Uuid,
        _receipt: &key_exchange::KeyRequestCompletion,
        _grant: Option<&GroupKey>,
    ) -> Result<bool, KeyStoreError> {
        Err(KeyStoreError)
    }

    /// Загружает сохранённое тело ответа на опубликованный запрос другого устройства.
    fn load_reply(
        &self,
        _address: &ChatKeyAddress,
        _topic_id: Uuid,
        _request_id: Uuid,
    ) -> Result<Option<key_exchange::StoredKeyReply>, KeyStoreError> {
        Err(KeyStoreError)
    }

    /// Записывает тело и UUID ответа перед сетью, чтобы повтор отправил те же байты.
    fn save_reply(
        &self,
        _address: &ChatKeyAddress,
        _topic_id: Uuid,
        _request_id: Uuid,
        _reply: &key_exchange::StoredKeyReply,
    ) -> Result<(), KeyStoreError> {
        Err(KeyStoreError)
    }

    /// Перечисляет доступные ключи ровно одного владельца, устройства и чата.
    fn list(
        &self,
        _owner: &OwnerContext,
        _device_id: Uuid,
        _stream_id: Uuid,
    ) -> Result<Vec<HeldChatKey>, KeyStoreError> {
        Err(KeyStoreError)
    }

    /// Повышает приоритет уже сохранённого ключа среди ключей этого чата.
    fn mark_for_sending(&self, _address: &ChatKeyAddress) -> Result<(), KeyStoreError> {
        Err(KeyStoreError)
    }

    /// Сохраняет ключ до сетевой операции, если этот адрес ещё свободен.
    /// Обычные чтение и список не возвращают запись до подтверждения сервера.
    fn stage_lifecycle_key(
        &self,
        _address: &ChatKeyAddress,
        _key: &GroupKey,
    ) -> Result<(), KeyStoreError> {
        Err(KeyStoreError)
    }

    /// Проверяет, что запись по адресу ещё ждёт серверного подтверждения.
    fn has_staged_lifecycle_key(&self, _address: &ChatKeyAddress) -> Result<bool, KeyStoreError> {
        Err(KeyStoreError)
    }

    /// Атомарно открывает уже сохранённую запись. Повтор безопасен, удалённая запись не создаётся.
    fn activate_staged_lifecycle_key(
        &self,
        _address: &ChatKeyAddress,
    ) -> Result<bool, KeyStoreError> {
        Err(KeyStoreError)
    }
}

/// Причина, по которой защищённое отправление не удалось подготовить.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum EncryptionError {
    #[error("message key is missing on this device")]
    MissingKey, // Отправляющее устройство не владеет нужным ключом.
    #[error("the selected session or message key changed")]
    Stale, // Выбранный вход или ключ сменился раньше подготовки отправки.
    #[error("failed to encrypt the message")]
    Seal, // Не удалось получить случайную соль или запечатать конверт.
    #[error(transparent)]
    KeyStore(#[from] KeyStoreError), // Защищённое хранилище отказало.
}

impl From<DecryptionError> for EncryptionError {
    /// Отображает причины общего чтения ключа в ошибки отправки.
    fn from(error: DecryptionError) -> Self {
        match error {
            DecryptionError::MissingKey => Self::MissingKey,
            DecryptionError::Stale => Self::Stale,
            DecryptionError::InvalidCiphertext => Self::Seal,
            DecryptionError::KeyStore(error) => Self::KeyStore(error),
        }
    }
}

/// Причина, по которой не получилось показать открытый текст.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DecryptionError {
    #[error("message key is missing on this device")]
    MissingKey, // Точное поколение ключа ещё не доступно.
    #[error("the selected session or message key changed")]
    Stale, // Сессия или ключ уже сменились, результат применять нельзя.
    #[error("invalid encrypted message")]
    InvalidCiphertext, // Конверт, тег подлинности или текст некорректны.
    #[error(transparent)]
    KeyStore(#[from] KeyStoreError), // Защищённое хранилище недоступно.
}

/// Изменение, по которому экран пересчитывает представление сообщений этого ключа.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyChange {
    pub activation: EncryptionActivation, // Выбор аккаунта, которому принадлежит изменение.
    pub stream_id: Uuid,                  // Чат, для которого нужно обновление.
    pub key_id: Uuid,                     // Поколение изменённого ключа.
    pub available: bool,                  // Ключ записан (`true`) или удалён (`false`).
}

/// Текущий выбор аккаунта и прочитанные для него ключи.
struct Selection {
    activation: EncryptionActivation, // Действующее разрешение на чтение и запись ключей.
    keys: HashMap<(Uuid, Uuid), Option<Arc<GroupKey>>>, // Чат и ключ. `None` значит, что запись уже искали.
    pending_requests: HashMap<key_exchange::KeyRequestTarget, Uuid>, // Канонические UUID подготовленных запросов.
}

/// Держит ключи только для выбранной сессии и проверяет результаты расшифровки.
pub struct MessageEncryption {
    store: Arc<dyn ChatKeyStore>, // Системный адаптер долговременных записей.
    store_operations: Mutex<()>,  // Упорядочивает чтение и запись ключей в рабочем потоке.
    selection: Arc<Mutex<Option<Selection>>>, // Короткая блокировка для выбора сессии и чтения текста.
    changes: broadcast::Sender<KeyChange>,    // События изменения записей выбранной сессии.
    selection_changes: watch::Sender<()>,     // Будит незавершённое чтение после смены выбора.
}

impl MessageEncryption {
    /// Подключает защищённое хранилище, не выбирая активный аккаунт.
    pub fn new(store: Arc<dyn ChatKeyStore>) -> Self {
        let (changes, _) = broadcast::channel(KEY_CHANGE_BUFFER);
        Self {
            store,
            store_operations: Mutex::new(()),
            selection: Arc::new(Mutex::new(None)),
            changes,
            selection_changes: watch::channel(()).0,
        }
    }

    /// Отзывает старую активацию и выдаёт текущую для постановки задач в очередь.
    pub fn select_session(
        &self,
        session: Option<EncryptionSession>,
    ) -> Option<EncryptionActivation> {
        let mut selection = self.selection.lock().unwrap();
        if selection
            .as_ref()
            .map(|current| current.activation.session())
            != session.as_ref()
        {
            *selection = session.map(|session| Selection {
                activation: EncryptionActivation(Arc::new(session)),
                keys: HashMap::new(),
                pending_requests: HashMap::new(),
            });
            self.selection_changes.send_replace(());
        }
        selection.as_ref().map(|current| current.activation.clone())
    }

    /// Дожидается начатого обращения к системному хранилищу. Вызывается после отзыва на рабочем потоке.
    pub fn wait_for_key_operations(&self) {
        drop(self.store_operations.lock().unwrap());
    }

    /// Подписывает экран на изменения ключей. При пропуске события экран обновляет снимок.
    pub fn subscribe(&self) -> broadcast::Receiver<KeyChange> {
        self.changes.subscribe()
    }

    /// Подписывает незавершённую сетевую работу на смену выбранного профиля.
    pub(super) fn observe_selection(&self) -> watch::Receiver<()> {
        self.selection_changes.subscribe()
    }

    /// Ищет ключ для уже доступного сообщения. Вызывается в рабочем потоке.
    pub fn prepare(
        &self,
        activation: &EncryptionActivation,
        stream_id: Uuid,
        key_id: Uuid,
    ) -> Result<MessageDecryption, DecryptionError> {
        let _operation = self.store_operations.lock().unwrap();
        self.prepare_locked(activation, stream_id, key_id)
    }

    /// Проверяет наличие локального ключа чата для показа редактора; вызывать в рабочем потоке.
    /// Результат не заменяет проверку режима, прав и ключа перед отправкой.
    pub fn has_local_key(
        &self,
        activation: &EncryptionActivation,
        stream_id: Uuid,
    ) -> Result<bool, EncryptionError> {
        let _operation = self.store_operations.lock().unwrap();
        let result = self.available_sending_key_locked(activation, stream_id, None);
        // Выбор аккаунта может измениться во время обращения к системному хранилищу.
        if !self
            .selection
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|selection| &selection.activation == activation)
        {
            return Err(EncryptionError::Stale);
        }
        match result {
            Ok(_) => Ok(true),
            Err(EncryptionError::MissingKey) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Читает точный ключ после входа в очередь защищённого хранилища.
    fn prepare_locked(
        &self,
        activation: &EncryptionActivation,
        stream_id: Uuid,
        key_id: Uuid,
    ) -> Result<MessageDecryption, DecryptionError> {
        let address = (stream_id, key_id);
        let cached = {
            let current = self.selection.lock().unwrap();
            let current = current
                .as_ref()
                .filter(|value| &value.activation == activation)
                .ok_or(DecryptionError::Stale)?;
            current.keys.get(&address).cloned()
        };
        let key = match cached {
            Some(key) => key,
            None => {
                let key = self
                    .store
                    .load(&key_address(activation.session(), stream_id, key_id))?
                    .map(Arc::new);
                let mut current = self.selection.lock().unwrap();
                let current = current
                    .as_mut()
                    .filter(|value| &value.activation == activation)
                    .ok_or(DecryptionError::Stale)?;
                current.keys.insert(address, key.clone());
                key
            }
        };
        let key = key.ok_or(DecryptionError::MissingKey)?;
        Ok(MessageDecryption {
            access: DecryptionAccess {
                activation: activation.clone(),
                stream_id,
                key_id,
                key: Arc::downgrade(&key),
            },
            key,
        })
    }

    /// Выбирает доступное текущее поколение или последний доступный ключ этого чата.
    pub(crate) fn sending_key(
        &self,
        activation: &EncryptionActivation,
        stream_id: Uuid,
        current: Uuid,
    ) -> Result<MessageDecryption, EncryptionError> {
        let _operation = self.store_operations.lock().unwrap();
        match self.prepare_locked(activation, stream_id, current) {
            Err(DecryptionError::MissingKey) => {}
            result => return result.map_err(EncryptionError::from),
        }
        self.available_sending_key_locked(activation, stream_id, Some(current))
    }

    /// Ищет последнее доступное поколение, пока чтение и запись ключей упорядочены.
    fn available_sending_key_locked(
        &self,
        activation: &EncryptionActivation,
        stream_id: Uuid,
        exclude: Option<Uuid>,
    ) -> Result<MessageDecryption, EncryptionError> {
        let session = activation.session();
        if !self
            .selection
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|selection| &selection.activation == activation)
        {
            return Err(EncryptionError::Stale);
        }
        let mut available = self
            .store
            .list(&session.owner, session.device_id, stream_id)?;
        available.sort_unstable_by_key(|key| {
            (
                std::cmp::Reverse(key.send_order),
                std::cmp::Reverse(key.received_order),
                key.key_id,
            )
        });
        for key in available {
            if Some(key.key_id) == exclude || key.key_id.is_nil() {
                continue;
            }
            match self.prepare_locked(activation, stream_id, key.key_id) {
                Err(DecryptionError::MissingKey) => {}
                result => return result.map_err(EncryptionError::from),
            }
        }
        Err(EncryptionError::MissingKey)
    }

    /// Подтверждает исходный выбор перед записью конверта или началом запроса.
    pub(crate) fn authorize_outgoing<T>(
        &self,
        key: &MessageDecryption,
        record_sending: bool,
        action: impl FnOnce() -> T,
    ) -> Result<T, EncryptionError> {
        let _operation = self.store_operations.lock().unwrap();
        if !key
            .access
            .is_current(self.selection.lock().unwrap().as_ref())
        {
            return Err(EncryptionError::Stale);
        }
        let address = key_address(
            key.access.activation.session(),
            key.access.stream_id,
            key.access.key_id,
        );
        let stored = self
            .store
            .load(&address)?
            .ok_or(EncryptionError::MissingKey)?;
        let context = b"workspace-chat-key-check";
        let expected = hmac::Key::new(hmac::HMAC_SHA256, stored.as_bytes());
        let prepared = hmac::Key::new(hmac::HMAC_SHA256, key.key.as_bytes());
        if hmac::verify(&expected, context, hmac::sign(&prepared, context).as_ref()).is_err() {
            return Err(EncryptionError::Stale);
        }
        if record_sending {
            self.store.mark_for_sending(&address)?;
        }
        if !key
            .access
            .is_current(self.selection.lock().unwrap().as_ref())
        {
            return Err(EncryptionError::Stale);
        }
        // Запись уже принята для этого входа. Удерживаем порядок операций с ключом,
        // пока сохраняем конверт, а выбор аккаунта остаётся свободен.
        Ok(action())
    }

    /// Проверяет конверт и закрепляет попытку до следующей операции с его ключом.
    pub(crate) fn authorize_prepared_envelope<T>(
        &self,
        address: &ChatKeyAddress,
        ciphertext: &str,
        admit: impl FnOnce() -> T,
    ) -> Result<T, EncryptionError> {
        let _operation = self.store_operations.lock().unwrap();
        let key = self
            .store
            .load(address)?
            .ok_or(EncryptionError::MissingKey)?;
        drop(decrypt_message(
            &key,
            address.stream_id,
            address.key_id,
            ciphertext,
        )?);
        Ok(admit())
    }

    /// Принимает целое сообщение и строит единую безопасную проекцию для всех экранов.
    pub fn accept(&self, result: DecryptionResult) -> Result<ProtectedMessage, DecryptionError> {
        let envelope = result.envelope;
        let accepted = self.accept_decryption(result)?;
        let (markdown, references) = super::quote::project_owned(accepted.plaintext);
        Ok(ProtectedMessage {
            permission: accepted.permission,
            envelope,
            markdown,
            references,
        })
    }

    /// Принимает только отдельный фрагмент, полученный через защищённую ссылку.
    pub fn accept_fragment(
        &self,
        result: FragmentDecryptionResult,
    ) -> Result<AcceptedDecryption, DecryptionError> {
        let mut accepted = self.accept_decryption(result.0)?;
        // Старый формат использует тот же AEAD, поэтому обычная ссылка может перенести целое сообщение.
        accepted.plaintext = super::quote::project_owned(accepted.plaintext).0;
        Ok(accepted)
    }

    /// Проверяет версию выбранного входа и ключа перед подготовкой проекции.
    fn accept_decryption(
        &self,
        result: DecryptionResult,
    ) -> Result<AcceptedDecryption, DecryptionError> {
        if result
            .origin
            .as_ref()
            .is_some_and(|origin| origin.with_current(|| ()).is_none())
            || !result
                .access
                .is_current(self.selection.lock().unwrap().as_ref())
        {
            return Err(DecryptionError::Stale);
        }
        Ok(AcceptedDecryption {
            permission: MessageReadPermission {
                selection: Arc::downgrade(&self.selection),
                access: result.access,
            },
            plaintext: result.plaintext?,
            origin: result.origin,
        })
    }

    /// Записывает уже проверенный ключ и сообщает экрану об изменении. Вызывается в рабочем потоке.
    pub fn save_key(
        &self,
        activation: &EncryptionActivation,
        stream_id: Uuid,
        key_id: Uuid,
        key: &GroupKey,
    ) -> Result<(), DecryptionError> {
        self.change_key(activation, stream_id, key_id, Some(key))
    }

    /// Удаляет локальный ключ, отзывает старые результаты и сообщает экрану об изменении.
    pub fn remove_key(
        &self,
        activation: &EncryptionActivation,
        stream_id: Uuid,
        key_id: Uuid,
    ) -> Result<(), DecryptionError> {
        self.change_key(activation, stream_id, key_id, None)
    }

    /// Упорядочивает системную запись с чтением и отзывает результат под новой версией ключа.
    fn change_key(
        &self,
        activation: &EncryptionActivation,
        stream_id: Uuid,
        key_id: Uuid,
        replacement: Option<&GroupKey>,
    ) -> Result<(), DecryptionError> {
        let _operation = self.store_operations.lock().unwrap();
        if !self
            .selection
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|current| &current.activation == activation)
        {
            return Err(DecryptionError::Stale);
        }
        // Мутация принята при проверке активации под общей блокировкой операций.
        // Она может закончиться после смены экрана. Новый вход прочитает ключи
        // только после её завершения, а старый экран сразу потеряет доступ к тексту.
        let address = key_address(activation.session(), stream_id, key_id);
        match replacement {
            Some(key) => self.store.save(&address, key)?,
            None => self.store.remove(&address)?,
        }
        self.publish_key_change(activation, stream_id, key_id, replacement.is_some());
        Ok(())
    }

    /// После подтверждённой записи отзывает старые представления и сообщает выбранному входу.
    fn publish_key_change(
        &self,
        activation: &EncryptionActivation,
        stream_id: Uuid,
        key_id: Uuid,
        available: bool,
    ) {
        let mut current = self.selection.lock().unwrap();
        if let Some(current) = current
            .as_mut()
            .filter(|value| &value.activation == activation)
        {
            current.keys.remove(&(stream_id, key_id));
            if !available {
                current
                    .pending_requests
                    .retain(|target, _| target.stream_id != stream_id || target.key_id != key_id);
            }
            let _ = self.changes.send(KeyChange {
                activation: activation.clone(),
                stream_id,
                key_id,
                available,
            });
        }
    }
}

/// Готовая работа для одного сообщения, которую можно завершить вне UI-потока.
pub struct MessageDecryption {
    access: DecryptionAccess, // Разрешение на показ открытого текста.
    key: Arc<GroupKey>,       // Прочитанный ключ, который нужен до конца работы.
}

impl MessageDecryption {
    /// Возвращает именно то поколение, байты которого удалось получить.
    pub(crate) fn key_id(&self) -> Uuid {
        self.access.key_id
    }

    /// Готовит конверт для сообщения со свежей случайной солью.
    pub(crate) fn seal(&self, plaintext: &str) -> Result<String, EncryptionError> {
        let mut salt = [0; KEY_BYTES];
        SystemRandom::new()
            .fill(&mut salt)
            .map_err(|_| EncryptionError::Seal)?;
        encrypt_message(
            &self.key,
            self.access.stream_id,
            self.access.key_id,
            &salt,
            plaintext,
        )
    }

    /// Расшифровывает один конверт. Перед показом результат должен пройти `accept`.
    pub fn decrypt(self, ciphertext: &str) -> DecryptionResult {
        DecryptionResult {
            plaintext: decrypt_message(
                &self.key,
                self.access.stream_id,
                self.access.key_id,
                ciphertext,
            ),
            access: self.access,
            envelope: Sha256::digest(ciphertext.as_bytes()).into(),
            origin: None,
        }
    }
}

/// Результат рабочего потока, закрытый до проверки его версии.
pub struct DecryptionResult {
    access: DecryptionAccess, // Разрешение, полученное до начала работы.
    plaintext: Result<Zeroizing<String>, DecryptionError>, // Текст до передачи в экран или ошибка.
    envelope: [u8; 32],       // Хэш точного конверта для сверки с текущей строкой истории.
    origin: Option<FragmentOriginsPermit>, // Версия доступа к исходному чату вставленного фрагмента.
}

/// Результат расшифровки отдельного фрагмента с его собственным исходным допуском.
pub struct FragmentDecryptionResult(DecryptionResult); // Закрытый результат до проверки Core.

/// Разрешает читать результат только до смены входа или точной записи ключа.
#[derive(Clone)]
struct DecryptionAccess {
    activation: EncryptionActivation, // Выбор аккаунта при запуске работы.
    stream_id: Uuid,                  // Чат сообщения.
    key_id: Uuid,                     // Точный UUID ключа сообщения.
    key: Weak<GroupKey>,              // Версия ключа без удержания его байтов.
}

impl DecryptionAccess {
    /// Сверяет активацию и версию ключа с состоянием Core под блокировкой выбора.
    fn is_current(&self, selection: Option<&Selection>) -> bool {
        selection
            .filter(|current| current.activation == self.activation)
            .and_then(|current| current.keys.get(&(self.stream_id, self.key_id)))
            .and_then(Option::as_ref)
            .is_some_and(|key| self.key.ptr_eq(&Arc::downgrade(key)))
    }
}

/// Представление отдельного фрагмента, текст которого можно отозвать.
pub struct AcceptedDecryption {
    permission: MessageReadPermission, // Проверка версии ключа при чтении.
    plaintext: Zeroizing<String>,      // Текст только для чтения под проверкой доступа.
    origin: Option<FragmentOriginsPermit>, // Версия доступа к исходному чату вставленного фрагмента.
}

/// Целое защищённое сообщение с единой безопасной проекцией Markdown и закрытыми подсказками цитат.
pub struct ProtectedMessage {
    permission: MessageReadPermission, // Выбранный вход и точное поколение ключа.
    envelope: [u8; 32],                // Хэш шифротекста, который дал эту проекцию.
    markdown: Zeroizing<String>,       // Тело без встроенных снимков оригиналов цитат.
    references: super::quote::ProtectedReferences, // Позиции ссылок и закрытые старые параметры.
}

impl ProtectedMessage {
    /// Подтверждает, что исходный конверт принят в той же активации, что и запрос оригинала.
    pub(in crate::messenger) fn matches_activation(
        &self,
        activation: &EncryptionActivation,
    ) -> bool {
        &self.permission.access.activation == activation
    }

    /// Читает стабильные байты, от которых зависят позиции разметки, копирование и редактор.
    pub fn with_body<T>(&self, read: impl FnOnce(&str) -> T) -> Option<T> {
        self.permission.with_current(|| read(&self.markdown))
    }

    /// Выполняет короткое действие под тем же разрешением без чтения текста.
    pub fn with_current<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        self.permission.with_current(action)
    }

    /// Сопоставляет прочитанный конверт точному шифротексту, стриму и ключу строки истории.
    pub(super) fn matches_envelope(&self, message: &HistoryMessage) -> bool {
        let MessageBody::Encrypted { key_id, ciphertext } = &message.body else {
            return false;
        };
        let envelope: [u8; 32] = Sha256::digest(ciphertext.as_bytes()).into();
        self.permission.access.stream_id == message.stream_id
            && self.permission.access.key_id == *key_id
            && self.envelope == envelope
    }

    /// Подтверждает точную позицию и даёт Core исключить скрытый старый параметр из показа.
    pub(in crate::messenger) fn reference_excerpt(
        &self,
        source: Uuid,
        position: usize,
    ) -> Option<Option<&str>> {
        self.references.excerpt(source, position)
    }

    /// Разрешает вложения только для этой позиции ссылки на весь оригинал.
    pub(in crate::messenger) fn reference_is_whole(&self, source: Uuid, position: usize) -> bool {
        self.references.whole_at(source, position)
    }

    /// Даёт производному результату проверять ключ без удержания открытого тела.
    pub(super) fn read_permission(&self) -> MessageReadPermission {
        self.permission.clone()
    }

    /// Оставляет разрешение для производного результата, освобождая тело и подсказки цитат.
    pub(super) fn into_permission(self) -> MessageReadPermission {
        self.permission
    }
}

/// Может подтвердить доступ к производному результату, не удерживая открытый текст.
#[derive(Clone)]
pub(super) struct MessageReadPermission {
    selection: Weak<Mutex<Option<Selection>>>, // Актуальный выбор Core без удержания сервиса.
    access: DecryptionAccess, // Активация и версия ключа, под которыми открыт текст.
}

impl MessageReadPermission {
    /// Сверяет, что производный результат используется в исходном выбранном входе.
    pub(super) fn matches_activation(&self, activation: &EncryptionActivation) -> bool {
        &self.access.activation == activation
    }

    /// Выполняет чтение до смены активации или версии ключа.
    pub(super) fn with_current<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        let selection = self.selection.upgrade()?;
        let selection = selection.lock().unwrap();
        self.access.is_current(selection.as_ref()).then(read)
    }

    /// Выполняет короткое чтение под одной проверкой двух ключей выбранного входа.
    pub(super) fn with_linked<T>(&self, other: &Self, read: impl FnOnce() -> T) -> Option<T> {
        if !self.selection.ptr_eq(&other.selection) {
            return None;
        }
        let selection = self.selection.upgrade()?;
        let selection = selection.lock().unwrap();
        (self.access.is_current(selection.as_ref()) && other.access.is_current(selection.as_ref()))
            .then(read)
    }
}

impl AcceptedDecryption {
    /// Передаёт текст на короткое время, если разрешение действует. После отзыва возвращает `None`.
    /// Замыкание выполняется под блокировкой выбора. Оно не должно вызывать методы шифрования.
    pub fn with_text<T>(&self, read: impl FnOnce(&str) -> T) -> Option<T> {
        let read = || {
            self.permission
                .with_current(|| read(self.plaintext.as_str()))
        };
        match &self.origin {
            Some(origin) => origin.with_current(read).flatten(),
            None => read(),
        }
    }
}

/// Вычисляет адрес ключа, общий для последовательных входов одного владельца.
fn key_address(session: &EncryptionSession, stream_id: Uuid, key_id: Uuid) -> ChatKeyAddress {
    ChatKeyAddress {
        owner: session.owner.clone(),
        device_id: session.device_id,
        stream_id,
        key_id,
    }
}

/// Формирует контекст для вывода ключа и проверки тега формата Electron.
fn message_context(stream_id: Uuid, key_id: Uuid) -> Vec<u8> {
    format!(r#"["workspace-chat-crypto-v1","message","{stream_id}","{key_id}"]"#).into_bytes()
}

/// Запечатывает конверт Electron и кодирует соль, шифротекст и тег вместе.
fn encrypt_message(
    group_key: &GroupKey,
    stream_id: Uuid,
    key_id: Uuid,
    salt: &[u8; KEY_BYTES],
    plaintext: &str,
) -> Result<String, EncryptionError> {
    let mut info = message_context(stream_id, key_id);
    let derived_key = message_key(group_key, salt, &info).map_err(|_| EncryptionError::Seal)?;
    info.extend_from_slice(salt);
    let mut ciphertext = Zeroizing::new(plaintext.as_bytes().to_vec());
    aead::LessSafeKey::new(derived_key)
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key([0; 12]),
            aead::Aad::from(info),
            &mut *ciphertext,
        )
        .map_err(|_| EncryptionError::Seal)?;
    let mut envelope = Vec::with_capacity(KEY_BYTES + ciphertext.len());
    envelope.extend_from_slice(salt);
    envelope.extend_from_slice(&ciphertext);
    Ok(STANDARD.encode(envelope))
}

/// Открывает конверт Electron и проверяет подлинность содержимого в заданном чате.
fn decrypt_message(
    group_key: &GroupKey,
    stream_id: Uuid,
    key_id: Uuid,
    encoded: &str,
) -> Result<Zeroizing<String>, DecryptionError> {
    let invalid = DecryptionError::InvalidCiphertext;
    let mut envelope = Zeroizing::new(STANDARD.decode(encoded).map_err(|_| invalid)?);
    if envelope.len() < KEY_BYTES + TAG_BYTES {
        return Err(invalid);
    }
    let (salt, ciphertext) = envelope.split_at_mut(KEY_BYTES);
    let mut info = message_context(stream_id, key_id);
    let derived_key = message_key(group_key, salt, &info)?;
    // У каждого конверта свой производный ключ. Тег проверяет и контекст, и соль.
    info.extend_from_slice(salt);
    let plaintext = aead::LessSafeKey::new(derived_key)
        .open_in_place(
            aead::Nonce::assume_unique_for_key([0; 12]),
            aead::Aad::from(info),
            ciphertext,
        )
        .map_err(|_| invalid)?;
    let text = std::str::from_utf8(plaintext).map_err(|_| invalid)?;
    // TextDecoder в Electron пропускает метку UTF-8 в начале сообщения.
    Ok(Zeroizing::new(
        text.strip_prefix('\u{feff}').unwrap_or(text).to_owned(),
    ))
}

/// Выводит отдельный 32-байтный ключ сообщения через HKDF-SHA256.
fn message_key(
    group_key: &GroupKey,
    salt: &[u8],
    info: &[u8],
) -> Result<aead::UnboundKey, DecryptionError> {
    let invalid = DecryptionError::InvalidCiphertext;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(group_key.as_bytes());
    let context = [info];
    let expanded = prk
        .expand(&context, &aead::CHACHA20_POLY1305)
        .map_err(|_| invalid)?;
    let mut bytes = Zeroizing::new([0; KEY_BYTES]);
    expanded.fill(bytes.as_mut()).map_err(|_| invalid)?;
    aead::UnboundKey::new(&aead::CHACHA20_POLY1305, bytes.as_ref()).map_err(|_| invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AccountId;
    use std::{sync::mpsc, thread, time::Duration};

    const STREAM_ID: Uuid = uuid::uuid!("956f084e-8992-4334-a6de-fe84ac055e46");
    const KEY_ID: Uuid = uuid::uuid!("e70f0d95-6784-4125-8dfd-83eac755cecf");
    const TEXT: &str = "Привет, **Workspace** 🔒";
    // Вектор получен через Node/OpenSSL с контекстом Electron на ревизии af099988.
    const ENVELOPE: &str = "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj970yFoPn4lFKoJp/oGE17NQ8FLxWTpUm8ndmdemp6aigBj2np1z3A+fzcJOGXqw2Q=";

    /// Хранит синтетические записи вместо системной связки ключей в тестах.
    #[derive(Default)]
    struct TestKeys(Mutex<HashMap<ChatKeyAddress, GroupKey>>); // Записи только на время теста.

    impl ChatKeyStore for TestKeys {
        /// Читает запись по полному адресу ключа.
        fn load(&self, address: &ChatKeyAddress) -> Result<Option<GroupKey>, KeyStoreError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(address)
                .map(|key| GroupKey::from_bytes(*key.as_bytes())))
        }

        /// Копирует запись ключа в тестовое хранилище.
        fn save(&self, address: &ChatKeyAddress, key: &GroupKey) -> Result<(), KeyStoreError> {
            self.0
                .lock()
                .unwrap()
                .insert(address.clone(), GroupKey::from_bytes(*key.as_bytes()));
            Ok(())
        }

        /// Удаляет запись по полному адресу ключа.
        fn remove(&self, address: &ChatKeyAddress) -> Result<(), KeyStoreError> {
            self.0.lock().unwrap().remove(address);
            Ok(())
        }

        /// Возвращает только поколения запрошенного чата владельца и устройства.
        fn list(
            &self,
            owner: &OwnerContext,
            device_id: Uuid,
            stream_id: Uuid,
        ) -> Result<Vec<HeldChatKey>, KeyStoreError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|address| {
                    &address.owner == owner
                        && address.device_id == device_id
                        && address.stream_id == stream_id
                })
                .map(|address| HeldChatKey {
                    key_id: address.key_id,
                    send_order: 0,
                    received_order: 0,
                    added_at_unix_ms: None,
                })
                .collect())
        }
    }

    /// Создаёт изолированное тестовое устройство для одного аккаунта.
    fn selection() -> EncryptionSession {
        EncryptionSession {
            owner: OwnerContext {
                account_id: AccountId("test-account".to_owned()),
                server: "https://workspace.example".to_owned(),
                organization_id: "test-organization".to_owned(),
                project_id: "test-project".to_owned(),
                user_id: "test-user".to_owned(),
            },
            generation: SessionGeneration(1),
            device_id: Uuid::new_v4(),
        }
    }

    /// Возвращает 32 синтетических байта из вектора Electron.
    fn sample_key() -> GroupKey {
        GroupKey::from_bytes(std::array::from_fn(|index| index as u8))
    }

    /// Использует исходный чат ссылки и отзывает правку при потере второго поколения.
    #[test]
    fn fragments_open_exact_origin_and_revoke_edit_with_each_key() {
        let keys = MessageEncryption::new(Arc::new(TestKeys::default()));
        let session = selection();
        let active = keys.select_session(Some(session.clone())).unwrap();
        let stream = Uuid::new_v4();
        let second = Uuid::new_v4();
        keys.save_key(&active, STREAM_ID, KEY_ID, &sample_key())
            .unwrap();
        keys.save_key(&active, stream, second, &sample_key())
            .unwrap();
        let incoming = format!(
            "urn:secret:v1:{STREAM_ID}:{KEY_ID}:{}",
            ENVELOPE
                .replace('+', "-")
                .replace('/', "_")
                .trim_end_matches('=')
        );
        let reference = fragments::FragmentReference::parse(&incoming).unwrap();
        let accepted = keys
            .accept_fragment(keys.prepare_fragment(&active, &reference).unwrap())
            .unwrap();
        assert_eq!(accepted.with_text(str::to_owned).as_deref(), Some(TEXT));
        let outbound = fragments::seal_fragments(
            ":::ещё один:::",
            &keys.prepare(&active, stream, second).unwrap(),
        )
        .unwrap();
        let mixed = format!("before [neutral]({incoming}) and {outbound}");
        let draft = keys
            .restore_fragment_draft(&active, &mixed)
            .unwrap()
            .unwrap();
        assert_eq!(
            draft.with_markdown(str::to_owned).unwrap(),
            format!("before :::{TEXT}::: and :::ещё один:::")
        );
        keys.remove_key(&active, stream, second).unwrap();
        assert!(draft.with_markdown(|_| ()).is_none());
        assert!(keys.restore_fragment_draft(&active, &mixed).is_err());
        assert!(accepted.with_text(|_| ()).is_some());
        keys.select_session(None);
        assert!(accepted.with_text(|_| ()).is_none());
    }

    /// Проверяет повторный выбор ключа и сохранение нового поколения по явной команде.
    #[test]
    fn fragment_sending_reuses_a_local_key_unless_new_requested() {
        let store = Arc::new(TestKeys::default());
        let keys = MessageEncryption::new(store.clone());
        let active = keys.select_session(Some(selection())).unwrap();
        let first = keys
            .fragment_sending_key(&active, STREAM_ID, false)
            .unwrap();
        let reuse = keys
            .fragment_sending_key(&active, STREAM_ID, false)
            .unwrap();
        assert_eq!(reuse.key_id(), first.key_id());
        assert_ne!(
            keys.fragment_sending_key(&active, STREAM_ID, true)
                .unwrap()
                .key_id(),
            first.key_id()
        );
        assert_eq!(store.0.lock().unwrap().len(), 2);
    }

    /// Наличие ключа относится только к чату и действующему выбору аккаунта.
    #[test]
    fn local_key_presence_tracks_chat_and_active_session() {
        let keys = MessageEncryption::new(Arc::new(TestKeys::default()));
        let active = keys.select_session(Some(selection())).unwrap();
        let other_stream = Uuid::new_v4();
        assert_eq!(keys.has_local_key(&active, STREAM_ID), Ok(false));
        keys.save_key(&active, STREAM_ID, KEY_ID, &sample_key())
            .unwrap();
        assert_eq!(keys.has_local_key(&active, STREAM_ID), Ok(true));
        assert_eq!(keys.has_local_key(&active, other_stream), Ok(false));
        keys.remove_key(&active, STREAM_ID, KEY_ID).unwrap();
        assert_eq!(keys.has_local_key(&active, STREAM_ID), Ok(false));
        keys.select_session(None);
        assert_eq!(
            keys.has_local_key(&active, STREAM_ID),
            Err(EncryptionError::Stale)
        );
    }

    /// Проверяет безопасный Markdown и раздельную случайную соль двух одинаковых фрагментов.
    #[test]
    fn fragments_seal_once_and_keep_only_typed_links() {
        let keys = MessageEncryption::new(Arc::new(TestKeys::default()));
        let active = keys.select_session(Some(selection())).unwrap();
        keys.save_key(&active, STREAM_ID, KEY_ID, &sample_key())
            .unwrap();
        let key = keys.prepare(&active, STREAM_ID, KEY_ID).unwrap();
        let prepared =
            fragments::seal_fragments("до :::private::: и :::private::: после", &key).unwrap();
        assert!(!prepared.contains("private") && !fragments::has_protected_draft(&prepared));
        let links = fragments::fragment_links(&prepared);
        assert_eq!(links.len(), 2);
        assert_ne!(links[0].reference, links[1].reference);
        for link in &links {
            assert_eq!(link.reference.stream_id(), STREAM_ID);
            assert_eq!(link.reference.key_id(), KEY_ID);
            let result = keys.prepare_fragment(&active, &link.reference).unwrap();
            assert_eq!(
                keys.accept_fragment(result)
                    .unwrap()
                    .with_text(str::to_owned)
                    .as_deref(),
                Some("private")
            );
        }
        for marker in [":::unsafe", "::::::", ":::  :::"] {
            assert!(fragments::has_protected_draft(marker));
            assert!(fragments::seal_fragments(marker, &key).is_err());
        }
        let literal = &prepared[links[0].range.clone()];
        let markdown = format!(
            "`{literal}`\n\n```ts\n{literal}\n```\n\n> {literal}\n\n```spoiler detail\n{literal}\n```"
        );
        let real = fragments::fragment_links(&markdown);
        assert_eq!(real.len(), 2);
        assert!(
            real.into_iter()
                .all(|link| &markdown[link.range] == literal)
        );
    }

    /// Шифрует в тот же конверт, что Electron, и использует свежую соль при каждой подготовке.
    #[test]
    fn writes_electron_envelope_with_fresh_salts() {
        let key = sample_key();
        let salt = std::array::from_fn(|index| 32 + index as u8);
        assert_eq!(
            encrypt_message(&key, STREAM_ID, KEY_ID, &salt, TEXT).unwrap(),
            ENVELOPE
        );
        let encryption = MessageEncryption::new(Arc::new(TestKeys::default()));
        let activation = encryption.select_session(Some(selection())).unwrap();
        encryption
            .save_key(&activation, STREAM_ID, KEY_ID, &key)
            .unwrap();
        let prepared = encryption.prepare(&activation, STREAM_ID, KEY_ID).unwrap();
        let first = STANDARD.decode(prepared.seal(TEXT).unwrap()).unwrap();
        let second = STANDARD.decode(prepared.seal(TEXT).unwrap()).unwrap();
        assert_ne!(&first[..KEY_BYTES], &second[..KEY_BYTES]);
    }

    /// Проверяет, что запись точного ключа открывает совместимое с Electron сообщение.
    #[test]
    fn reads_electron_message_after_exact_key_arrives() {
        let encryption = MessageEncryption::new(Arc::new(TestKeys::default()));
        let mut changes = encryption.subscribe();
        let activation = encryption.select_session(Some(selection())).unwrap();
        assert!(matches!(
            encryption.prepare(&activation, STREAM_ID, KEY_ID),
            Err(DecryptionError::MissingKey)
        ));

        encryption
            .save_key(&activation, STREAM_ID, KEY_ID, &sample_key())
            .unwrap();
        assert_eq!(
            changes.try_recv().unwrap(),
            KeyChange {
                activation: activation.clone(),
                stream_id: STREAM_ID,
                key_id: KEY_ID,
                available: true,
            }
        );
        let ready = encryption
            .prepare(&activation, STREAM_ID, KEY_ID)
            .unwrap()
            .decrypt(ENVELOPE);
        assert_eq!(
            encryption
                .accept(ready)
                .unwrap()
                .with_body(|text| text == TEXT),
            Some(true)
        );
        for (stream_id, key_id) in [(Uuid::new_v4(), KEY_ID), (STREAM_ID, Uuid::new_v4())] {
            assert!(matches!(
                encryption.prepare(&activation, stream_id, key_id),
                Err(DecryptionError::MissingKey)
            ));
        }
    }

    /// Отклоняет подмену чата, ключа и байтов и читает текст с теми же правилами, что и Electron.
    #[test]
    fn checks_context_and_canonical_utf8_envelope() {
        let key = sample_key();
        let mut changed = STANDARD.decode(ENVELOPE).unwrap();
        changed[KEY_BYTES] ^= 1;
        for (stream_id, key_id, envelope) in [
            (Uuid::new_v4(), KEY_ID, ENVELOPE),
            (STREAM_ID, Uuid::new_v4(), ENVELOPE),
            (STREAM_ID, KEY_ID, ENVELOPE.trim_end_matches('=')),
            (STREAM_ID, KEY_ID, ""),
            (STREAM_ID, KEY_ID, STANDARD.encode(changed).as_str()),
            // Корректный конверт из Node/OpenSSL с байтом `0xff` вместо UTF-8 внутри.
            (
                STREAM_ID,
                KEY_ID,
                "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj9URIl5Ly6+hi/EN96sLBKAgA==",
            ),
        ] {
            assert!(matches!(
                decrypt_message(&key, stream_id, key_id, envelope),
                Err(DecryptionError::InvalidCiphertext)
            ));
        }
        let with_bom = "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj9E90+HhYqdRXXvYY8UkaKRFyZ6Wdw=";
        assert_eq!(
            decrypt_message(&key, STREAM_ID, KEY_ID, with_bom)
                .unwrap()
                .as_str(),
            "ok"
        );
    }

    /// Просроченный результат нельзя показать после замены или удаления ключа.
    #[test]
    fn rejects_late_results_after_replacement_and_removal() {
        let encryption = MessageEncryption::new(Arc::new(TestKeys::default()));
        let activation = encryption.select_session(Some(selection())).unwrap();
        let key = sample_key();
        encryption
            .save_key(&activation, STREAM_ID, KEY_ID, &key)
            .unwrap();
        let original = encryption.prepare(&activation, STREAM_ID, KEY_ID).unwrap();
        let original_text = accepted(&encryption, &activation);
        encryption
            .save_key(&activation, STREAM_ID, KEY_ID, &key)
            .unwrap();
        let replacement = encryption.prepare(&activation, STREAM_ID, KEY_ID).unwrap();
        let replacement_text = accepted(&encryption, &activation);
        assert!(original_text.with_body(|_| ()).is_none());
        assert!(matches!(
            encryption.accept(original.decrypt(ENVELOPE)),
            Err(DecryptionError::Stale)
        ));

        let mut changes = encryption.subscribe();
        encryption
            .remove_key(&activation, STREAM_ID, KEY_ID)
            .unwrap();
        assert!(replacement_text.with_body(|_| ()).is_none());
        assert!(!changes.try_recv().unwrap().available);
        assert!(matches!(
            encryption.accept(replacement.decrypt(ENVELOPE)),
            Err(DecryptionError::Stale)
        ));
        assert!(matches!(
            encryption.prepare(&activation, STREAM_ID, KEY_ID),
            Err(DecryptionError::MissingKey)
        ));
    }

    /// Изолирует ключи другого устройства и отзывает текст при переключении и повторном входе.
    #[test]
    fn rejects_stale_selection_even_after_switching_back() {
        let session = selection();
        let mut other_device = session.clone();
        other_device.device_id = Uuid::new_v4();
        let mut other_owner = session.clone();
        other_owner.owner.account_id = AccountId("second-account".to_owned());
        let mut next_login = session.clone();
        next_login.generation = SessionGeneration(2);
        let encryption = MessageEncryption::new(Arc::new(TestKeys::default()));
        let initial = encryption.select_session(Some(session.clone())).unwrap();
        encryption
            .save_key(&initial, STREAM_ID, KEY_ID, &sample_key())
            .unwrap();
        let original = encryption.prepare(&initial, STREAM_ID, KEY_ID).unwrap();
        let retained_text = accepted(&encryption, &initial);

        for other in [other_device, other_owner] {
            let other = encryption.select_session(Some(other)).unwrap();
            assert!(matches!(
                encryption.prepare(&other, STREAM_ID, KEY_ID),
                Err(DecryptionError::MissingKey)
            ));
            assert!(matches!(
                encryption.remove_key(&initial, STREAM_ID, KEY_ID),
                Err(DecryptionError::Stale)
            ));
        }
        let reopened = encryption.select_session(Some(session)).unwrap();
        assert_ne!(initial, reopened);
        assert!(retained_text.with_body(|_| ()).is_none());
        let ready = original.decrypt(ENVELOPE);
        assert!(matches!(
            encryption.accept(ready),
            Err(DecryptionError::Stale)
        ));
        assert!(encryption.prepare(&reopened, STREAM_ID, KEY_ID).is_ok());

        let original = encryption.prepare(&reopened, STREAM_ID, KEY_ID).unwrap();
        let next_login = encryption.select_session(Some(next_login)).unwrap();
        assert!(matches!(
            encryption.accept(original.decrypt(ENVELOPE)),
            Err(DecryptionError::Stale)
        ));
        assert_eq!(
            accepted(&encryption, &next_login).with_body(|text| text == TEXT),
            Some(true)
        );
    }

    /// Расшифровывает вектор и возвращает отзываемое представление для дальнейшей проверки.
    fn accepted(
        encryption: &MessageEncryption,
        activation: &EncryptionActivation,
    ) -> ProtectedMessage {
        let ready = encryption
            .prepare(activation, STREAM_ID, KEY_ID)
            .unwrap()
            .decrypt(ENVELOPE);
        encryption.accept(ready).unwrap()
    }

    /// Останавливает принятую запись до явного разрешения теста.
    struct BlockingSave {
        keys: TestKeys,                    // Хранилище, которое меняется после разрешения.
        saving: mpsc::Sender<()>,          // Сообщает о входе в метод записи.
        resume: Mutex<mpsc::Receiver<()>>, // Не даёт завершить запись раньше теста.
    }

    impl ChatKeyStore for BlockingSave {
        /// Читает уже завершившуюся запись.
        fn load(&self, address: &ChatKeyAddress) -> Result<Option<GroupKey>, KeyStoreError> {
            self.keys.load(address)
        }

        /// Ждёт сигнала теста, затем сохраняет ключ.
        fn save(&self, address: &ChatKeyAddress, key: &GroupKey) -> Result<(), KeyStoreError> {
            self.saving.send(()).map_err(|_| KeyStoreError)?;
            self.resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| KeyStoreError)?;
            self.keys.save(address, key)
        }

        /// Удаляет запись из тестового хранилища.
        fn remove(&self, address: &ChatKeyAddress) -> Result<(), KeyStoreError> {
            self.keys.remove(address)
        }
    }

    /// Новый выбор сразу отзывает старый и ждёт начатую запись только в рабочем потоке.
    #[test]
    fn admitted_save_precedes_new_reads_but_queued_old_work_is_rejected() {
        let (saving, save_started) = mpsc::channel();
        let (resume, wait_before_commit) = mpsc::channel();
        let store = Arc::new(BlockingSave {
            keys: TestKeys::default(),
            saving,
            resume: Mutex::new(wait_before_commit),
        });
        let session = selection();
        let encryption = MessageEncryption::new(store);
        let mut changes = encryption.subscribe();
        let old = encryption.select_session(Some(session.clone())).unwrap();
        let encryption = &encryption;
        let old = &old;

        thread::scope(|scope| {
            let pending =
                scope.spawn(|| encryption.save_key(old, STREAM_ID, KEY_ID, &sample_key()));
            save_started.recv_timeout(Duration::from_secs(2)).unwrap();

            let (attempting, wait_for_attempt) = mpsc::channel();
            let queued = scope.spawn(move || {
                attempting.send(()).unwrap();
                assert!(matches!(
                    encryption.prepare(old, STREAM_ID, KEY_ID),
                    Err(DecryptionError::Stale)
                ));
                assert_eq!(
                    encryption.save_key(old, STREAM_ID, KEY_ID, &sample_key()),
                    Err(DecryptionError::Stale)
                );
                assert_eq!(
                    encryption.remove_key(old, STREAM_ID, KEY_ID),
                    Err(DecryptionError::Stale)
                );
            });
            wait_for_attempt
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            assert!(encryption.select_session(None).is_none());
            let current = encryption.select_session(Some(session)).unwrap();
            assert_ne!(old, &current);

            let (read_complete, wait_for_read) = mpsc::channel();
            let reader = scope.spawn(move || {
                encryption.wait_for_key_operations();
                let ready = accepted(encryption, &current);
                read_complete
                    .send(ready.with_body(|text| text == TEXT))
                    .unwrap();
            });
            assert_eq!(
                wait_for_read.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
            resume.send(()).unwrap();
            pending.join().unwrap().unwrap();
            queued.join().unwrap();
            reader.join().unwrap();
            assert_eq!(wait_for_read.try_recv().unwrap(), Some(true));
            assert_eq!(
                changes.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            );
        });
    }
}
