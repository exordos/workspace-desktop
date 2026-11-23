//! Подключает ключи чата к связке ключей macOS и хранит UUID локального устройства.
//! Изолированный профиль использует отдельное хранилище в памяти.

use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::Path,
    sync::Mutex,
};

use security_framework::passwords::{get_generic_password, set_generic_password};
use uuid::Uuid;
use workspace_core::{
    context::OwnerContext,
    messenger::encryption::{
        ChatKeyAddress, ChatKeyStore, GroupKey, HeldChatKey, KeyStoreError,
        key_exchange::{KeyRequestCompletion, KeyRequestOutcome, StoredKeyReply, StoredKeyRequest},
    },
};
use zeroize::Zeroizing;

mod record;
use record::{ChatRecord, Pending};

const SERVICE: &str = "com.workspace.desktop.chat-keys";
const ITEM_NOT_FOUND: i32 = -25300;

/// Форматирует время первого сохранения ключа локальными средствами macOS.
pub(crate) fn key_date(unix_ms: u64) -> String {
    use objc2_foundation::{NSDate, NSDateFormatter, NSDateFormatterStyle};
    // Foundation принимает секунды и возвращает строки с часовым поясом системы.
    unsafe {
        let date = NSDate::dateWithTimeIntervalSince1970(unix_ms as f64 / 1000.);
        NSDateFormatter::localizedStringFromDate_dateStyle_timeStyle(
            &date,
            NSDateFormatterStyle::NSDateFormatterShortStyle,
            NSDateFormatterStyle::NSDateFormatterShortStyle,
        )
        .to_string()
    }
}

/// Возвращает стабильный UUID профиля. Вызывать под блокировкой открытого профиля.
pub(crate) fn device_id(directory: &Path) -> io::Result<Uuid> {
    let path = directory.join("chat-device-id");
    match fs::read_to_string(&path) {
        Ok(value) => Uuid::parse_str(value.trim())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let id = Uuid::new_v4();
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            writeln!(file, "{id}")?;
            file.as_file().sync_all()?;
            file.persist_noclobber(path)?;
            Ok(id)
        }
        Err(error) => Err(error),
    }
}

/// Одна запись на владельца, устройство и чат нужна, чтобы выдача и её квитанция менялись атомарно.
#[derive(Clone, Copy)]
struct Scope<'a> {
    owner: &'a OwnerContext, // Аккаунт, сервер, организация и проект из подтверждённого входа.
    device: Uuid,            // Локальное устройство, не передаёт ключи другим профилям.
    stream: Uuid,            // Точный чат, в котором используются поколения и темы.
}

impl<'a> From<&'a ChatKeyAddress> for Scope<'a> {
    /// Исключает UUID поколения из адреса общей транзакции.
    fn from(address: &'a ChatKeyAddress) -> Self {
        Self {
            owner: &address.owner,
            device: address.device_id,
            stream: address.stream_id,
        }
    }
}

impl Scope<'_> {
    /// Кодирует адрес Keychain с сохранением границ полей, независимо от символов в имени.
    fn account(self) -> Result<String, KeyStoreError> {
        serde_json::to_string(&(
            "v1",
            &self.owner.account_id.0,
            &self.owner.server,
            &self.owner.organization_id,
            &self.owner.project_id,
            &self.owner.user_id,
            self.device,
            self.stream,
        ))
        .map_err(|_| KeyStoreError)
    }
}

/// Реальная и изолированная платформенная реализации одной защищённой записи.
enum Records {
    Keychain,
    Memory(HashMap<String, Zeroizing<Vec<u8>>>), // Изолированный профиль не пишет в Keychain и файлы.
}

impl Records {
    /// Читает байты одной записи, которая очищается после десериализации.
    fn read(&self, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>, KeyStoreError> {
        match self {
            Self::Memory(current) => Ok(current.get(account).cloned()),
            Self::Keychain => match get_generic_password(SERVICE, account) {
                Ok(value) => Ok(Some(Zeroizing::new(value))),
                Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
                Err(_) => Err(KeyStoreError),
            },
        }
    }

    /// Заменяет целую запись до возврата успешного результата.
    fn write(&mut self, account: String, data: Zeroizing<Vec<u8>>) -> Result<(), KeyStoreError> {
        match self {
            Self::Memory(current) => {
                current.insert(account, data);
                Ok(())
            }
            Self::Keychain => {
                set_generic_password(SERVICE, &account, &data).map_err(|_| KeyStoreError)
            }
        }
    }

    /// Читает текущую запись, отличая неизвестный чат от повреждённого значения.
    fn chat(&self, account: &str) -> Result<ChatRecord, KeyStoreError> {
        match self.read(account)? {
            Some(secret) => serde_json::from_slice(&secret).map_err(|_| KeyStoreError),
            None => Ok(ChatRecord::default()),
        }
    }
}

/// Системный адаптер, который сериализует изменения внутри открытого процесса профиля.
pub(crate) struct ProfileChatKeys {
    records: Mutex<Records>, // Одна блокировка покрывает read, изменение и атомарную запись Keychain.
}

impl ProfileChatKeys {
    /// Подключает отдельную службу Keychain для шифрования чатов.
    pub(crate) fn keychain() -> Self {
        Self {
            records: Mutex::new(Records::Keychain),
        }
    }

    /// Подключает тот же формат в отдельной памяти временного или тестового профиля.
    pub(crate) fn temporary() -> Self {
        Self {
            records: Mutex::new(Records::Memory(HashMap::new())),
        }
    }

    /// Читает транзакционный снимок только для запросившего его владельца и устройства.
    fn query<T>(
        &self,
        scope: Scope<'_>,
        read: impl FnOnce(&ChatRecord) -> Result<T, KeyStoreError>,
    ) -> Result<T, KeyStoreError> {
        let records = self.records.lock().map_err(|_| KeyStoreError)?;
        let record = records.chat(&scope.account()?)?;
        read(&record)
    }

    /// После изменения пишет весь снимок, а при отказе оставляет старую запись для повтора.
    fn update<T>(
        &self,
        scope: Scope<'_>,
        change: impl FnOnce(&mut ChatRecord) -> Result<(T, bool), KeyStoreError>,
    ) -> Result<T, KeyStoreError> {
        let mut records = self.records.lock().map_err(|_| KeyStoreError)?;
        let account = scope.account()?;
        let mut record = records.chat(&account)?;
        let (result, changed) = change(&mut record)?;
        if changed {
            let data = Zeroizing::new(serde_json::to_vec(&record).map_err(|_| KeyStoreError)?);
            records.write(account, data)?;
        }
        Ok(result)
    }
}

impl ChatKeyStore for ProfileChatKeys {
    /// Загружает только активный ключ; запись подготовки остаётся закрытой.
    fn load(&self, address: &ChatKeyAddress) -> Result<Option<GroupKey>, KeyStoreError> {
        self.query(address.into(), |chat| {
            Ok(chat
                .generations
                .get(&address.key_id)
                .and_then(|key| key.material.as_ref())
                .filter(|key| !key.pending_lifecycle)
                .map(|key| key.secret.group()))
        })
    }

    /// Сохраняет активный ключ и его порядок в той же записи чата.
    fn save(&self, address: &ChatKeyAddress, key: &GroupKey) -> Result<(), KeyStoreError> {
        self.update(address.into(), |chat| {
            chat.put(address.key_id, key, false)?;
            Ok(((), true))
        })
    }

    /// Убирает ключ и ожидающие запросы; повторы завершённых ответов остаются распознаваемыми.
    fn remove(&self, address: &ChatKeyAddress) -> Result<(), KeyStoreError> {
        self.update(address.into(), |chat| {
            let Some(generation) = chat.generations.get_mut(&address.key_id) else {
                return Ok(((), false));
            };
            let had_material = generation.material.take().is_some();
            generation.lifecycle_removed |= had_material;
            let changed = had_material || !generation.requests.is_empty();
            generation.requests.clear();
            Ok(((), changed))
        })
    }

    /// Перечисляет полученные ключи определённого чата и устройства с устойчивым порядком.
    fn list(
        &self,
        owner: &OwnerContext,
        device_id: Uuid,
        stream_id: Uuid,
    ) -> Result<Vec<HeldChatKey>, KeyStoreError> {
        self.query(
            Scope {
                owner,
                device: device_id,
                stream: stream_id,
            },
            |chat| {
                Ok(chat
                    .generations
                    .iter()
                    .filter_map(|(&key_id, generation)| {
                        let key = generation
                            .material
                            .as_ref()
                            .filter(|key| !key.pending_lifecycle)?;
                        Some(HeldChatKey {
                            key_id,
                            send_order: key.send_order,
                            received_order: key.received_order,
                            added_at_unix_ms: key.added_at_unix_ms,
                        })
                    })
                    .collect())
            },
        )
    }

    /// Меняет порядок выбора действующего ключа, когда Core готовит новую отправку.
    fn mark_for_sending(&self, address: &ChatKeyAddress) -> Result<(), KeyStoreError> {
        self.update(address.into(), |chat| {
            let order = chat.next_order()?;
            let key = chat
                .generations
                .get_mut(&address.key_id)
                .and_then(|entry| entry.material.as_mut())
                .filter(|key| !key.pending_lifecycle)
                .ok_or(KeyStoreError)?;
            key.send_order = order;
            Ok(((), true))
        })
    }

    /// Оставляет подготовленный ключ закрытым до подтверждения размещения или ротации.
    fn stage_lifecycle_key(
        &self,
        address: &ChatKeyAddress,
        key: &GroupKey,
    ) -> Result<(), KeyStoreError> {
        self.update(address.into(), |chat| {
            if let Some(generation) = chat.generations.get(&address.key_id) {
                if generation.lifecycle_removed {
                    return Err(KeyStoreError);
                }
                if generation.material.is_some() {
                    return Ok(((), false));
                }
            }
            chat.put(address.key_id, key, true)?;
            Ok(((), true))
        })
    }

    /// Проверяет, осталась ли подготовленная запись до сетевого подтверждения.
    fn has_staged_lifecycle_key(&self, address: &ChatKeyAddress) -> Result<bool, KeyStoreError> {
        self.query(address.into(), |chat| {
            Ok(chat
                .generations
                .get(&address.key_id)
                .and_then(|key| key.material.as_ref())
                .is_some_and(|key| key.pending_lifecycle))
        })
    }

    /// Открывает текущую подготовку. Когда ключ уже удалён, не создаёт его из квитанций.
    fn activate_staged_lifecycle_key(
        &self,
        address: &ChatKeyAddress,
    ) -> Result<bool, KeyStoreError> {
        self.update(address.into(), |chat| {
            let Some(key) = chat
                .generations
                .get_mut(&address.key_id)
                .and_then(|key| key.material.as_mut())
            else {
                return Ok((false, false));
            };
            let changed = std::mem::replace(&mut key.pending_lifecycle, false);
            Ok((true, changed))
        })
    }

    /// Возвращает пару и идентификаторы для неизменяемого запроса той же темы.
    fn load_request(
        &self,
        address: &ChatKeyAddress,
        topic_id: Uuid,
    ) -> Result<Option<StoredKeyRequest>, KeyStoreError> {
        self.query(address.into(), |chat| {
            chat.generations
                .get(&address.key_id)
                .and_then(|entry| entry.requests.get(&topic_id))
                .map(Pending::open)
                .transpose()
        })
    }

    /// Сохраняет подготовку или подтверждает тот же запрос, если его не отменили.
    fn save_request(
        &self,
        address: &ChatKeyAddress,
        topic_id: Uuid,
        request: &StoredKeyRequest,
    ) -> Result<(), KeyStoreError> {
        self.update(address.into(), |chat| {
            let public_id = Uuid::new_v5(&topic_id, request.canonical_id.to_string().as_bytes());
            if request.canonical_id.is_nil()
                || request
                    .confirmed_request_id
                    .is_some_and(|id| id != public_id)
            {
                return Err(KeyStoreError);
            }
            let entry = chat.generations.entry(address.key_id).or_default();
            if let Some(known) = entry.requests.get(&topic_id) {
                if !known.matches(request) {
                    return Err(KeyStoreError);
                }
            } else if request.confirmed_request_id.is_some() {
                return Err(KeyStoreError);
            }
            entry.requests.insert(topic_id, Pending::new(request));
            Ok(((), true))
        })
    }

    /// Заменяет только точную прежнюю попытку восстановления в одной записи защищённого чата.
    fn replace_request(
        &self,
        address: &ChatKeyAddress,
        topic_id: Uuid,
        expected_canonical_id: Uuid,
        request: &StoredKeyRequest,
    ) -> Result<bool, KeyStoreError> {
        if request.canonical_id.is_nil()
            || request.canonical_id == expected_canonical_id
            || request.confirmed_request_id.is_some()
            || request.recovery.is_none()
        {
            return Err(KeyStoreError);
        }
        self.update(address.into(), |chat| {
            let Some(entry) = chat.generations.get_mut(&address.key_id) else {
                return Ok((false, false));
            };
            let Some(current) = entry.requests.get_mut(&topic_id) else {
                return Ok((false, false));
            };
            if current.canonical_id != expected_canonical_id {
                return Ok((false, false));
            }
            *current = Pending::new(request);
            Ok((true, true))
        })
    }

    /// Возвращает квитанцию закрытого запроса даже после удаления ключа и повторной доставки ответа.
    fn load_request_completion(
        &self,
        address: &ChatKeyAddress,
        topic_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<KeyRequestCompletion>, KeyStoreError> {
        self.query(address.into(), |chat| {
            Ok(chat
                .generations
                .get(&address.key_id)
                .and_then(|entry| entry.receipts.get(&topic_id))
                .and_then(|topic| topic.get(&request_id))
                .copied())
        })
    }

    /// Одной транзакцией заменяет запрос квитанцией, сохраняет ключ и прекращает хранить приватную пару.
    fn complete_request(
        &self,
        address: &ChatKeyAddress,
        topic_id: Uuid,
        receipt: &KeyRequestCompletion,
        grant: Option<&GroupKey>,
    ) -> Result<bool, KeyStoreError> {
        self.update(address.into(), |chat| {
            if grant.is_some() != (receipt.outcome == KeyRequestOutcome::Granted)
                || Uuid::new_v5(&topic_id, receipt.canonical_id.to_string().as_bytes())
                    != receipt.request_id
            {
                return Err(KeyStoreError);
            }
            let pending = chat
                .generations
                .get(&address.key_id)
                .and_then(|entry| entry.requests.get(&topic_id));
            if !pending.is_some_and(|pending| {
                pending.canonical_id == receipt.canonical_id
                    && pending
                        .confirmed_request_id
                        .is_none_or(|id| id == receipt.request_id)
            }) {
                return Ok((false, false));
            }
            if let Some(key) = grant {
                chat.put(address.key_id, key, false)?;
            }
            let entry = chat.generations.entry(address.key_id).or_default();
            entry.requests.remove(&topic_id);
            entry
                .receipts
                .entry(topic_id)
                .or_default()
                .insert(receipt.request_id, *receipt);
            Ok((true, true))
        })
    }

    /// Восстанавливает точный подготовленный ответ на исходный публичный запрос.
    fn load_reply(
        &self,
        address: &ChatKeyAddress,
        topic_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<StoredKeyReply>, KeyStoreError> {
        self.query(address.into(), |chat| {
            Ok(chat
                .generations
                .get(&address.key_id)
                .and_then(|entry| entry.replies.get(&topic_id))
                .and_then(|topic| topic.get(&request_id))
                .cloned())
        })
    }

    /// Записывает подготовленный ответ перед первым POST, не заменяя его на повторе.
    fn save_reply(
        &self,
        address: &ChatKeyAddress,
        topic_id: Uuid,
        request_id: Uuid,
        reply: &StoredKeyReply,
    ) -> Result<(), KeyStoreError> {
        self.update(address.into(), |chat| {
            use std::collections::hash_map::Entry;
            let replies = chat
                .generations
                .entry(address.key_id)
                .or_default()
                .replies
                .entry(topic_id)
                .or_default();
            match replies.entry(request_id) {
                Entry::Vacant(slot) => {
                    slot.insert(reply.clone());
                    Ok(((), true))
                }
                Entry::Occupied(known)
                    if known.get().canonical_id == reply.canonical_id
                        && known.get().request_public_key == reply.request_public_key
                        && known.get().key_fingerprint == reply.key_fingerprint
                        && known.get().payload == reply.payload =>
                {
                    Ok(((), false))
                }
                Entry::Occupied(_) => Err(KeyStoreError),
            }
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Сохраняет UUID через повторное открытие профиля и отделяет другой профиль.
    #[test]
    fn device_id_belongs_to_the_profile() {
        let original = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let id = device_id(original.path()).unwrap();
        assert_eq!(device_id(original.path()).unwrap(), id);
        assert_ne!(device_id(other.path()).unwrap(), id);
    }

    /// Готовит адрес защищённой записи, который легко сменить по отдельному измерению.
    fn address() -> ChatKeyAddress {
        ChatKeyAddress {
            owner: OwnerContext {
                account_id: workspace_core::context::AccountId("A".into()),
                server: "https://workspace.test".into(),
                organization_id: "org".into(),
                project_id: "project".into(),
                user_id: Uuid::from_u128(1).to_string(),
            },
            device_id: Uuid::from_u128(2),
            stream_id: Uuid::from_u128(3),
            key_id: Uuid::from_u128(4),
        }
    }

    /// Проверяет запись, повторное открытие и удаление в настоящей связке ключей.
    #[test]
    #[ignore = "требует разблокированную связку ключей macOS"]
    fn keychain_roundtrip_uses_isolated_address() {
        struct TestItem(String);
        impl Drop for TestItem {
            /// Удаляет только синтетическую запись этого вызова, в том числе при сбое проверки.
            fn drop(&mut self) {
                let _ = security_framework::passwords::delete_generic_password(SERVICE, &self.0);
            }
        }

        let mut target = address();
        target.device_id = Uuid::new_v4();
        let item = TestItem(Scope::from(&target).account().unwrap());
        let key = GroupKey::from_bytes([7; 32]);
        ProfileChatKeys::keychain().save(&target, &key).unwrap();
        let reopened = ProfileChatKeys::keychain();
        assert!(
            reopened
                .load(&target)
                .unwrap()
                .is_some_and(|value| { value.as_bytes() == key.as_bytes() })
        );
        reopened.remove(&target).unwrap();
        assert!(ProfileChatKeys::keychain().load(&target).unwrap().is_none());
        drop(item);
    }

    /// Проверяет закрытую подготовку, приоритет последней отправки и отсутствие повторного открытия после удаления.
    #[test]
    fn staged_keys_only_open_after_confirmation_and_cannot_reappear() {
        let store = ProfileChatKeys::temporary();
        let staged = address();
        let key = GroupKey::from_bytes([7; 32]);
        let mut other = staged.clone();
        other.key_id = Uuid::from_u128(9);
        store.stage_lifecycle_key(&staged, &key).unwrap();
        assert!(store.has_staged_lifecycle_key(&staged).unwrap());
        assert!(store.load(&staged).unwrap().is_none());
        assert!(
            store
                .list(&staged.owner, staged.device_id, staged.stream_id)
                .unwrap()
                .is_empty()
        );
        store.save(&other, &key).unwrap();
        assert!(store.activate_staged_lifecycle_key(&staged).unwrap());
        assert!(store.activate_staged_lifecycle_key(&staged).unwrap());
        let keys = || {
            store
                .list(&staged.owner, staged.device_id, staged.stream_id)
                .unwrap()
        };
        let initial = keys()
            .into_iter()
            .find(|item| item.key_id == staged.key_id)
            .unwrap();
        store.mark_for_sending(&staged).unwrap();
        let current = keys()
            .into_iter()
            .find(|item| item.key_id == staged.key_id)
            .unwrap();
        assert_eq!(current.received_order, initial.received_order);
        assert!(current.send_order > current.received_order);
        store.remove(&staged).unwrap();
        assert!(!store.activate_staged_lifecycle_key(&staged).unwrap());
        assert!(store.stage_lifecycle_key(&staged, &key).is_err());
        assert!(store.load(&staged).unwrap().is_none());
    }

    /// Подтверждает одноатомный приём выдачи, очистку пары и защиту от повтора после отзыва.
    #[test]
    fn finishing_request_drops_pair_and_keeps_receipt_after_removal() {
        use workspace_core::messenger::encryption::grant::RequestKeyPair;
        let store = ProfileChatKeys::temporary();
        let target = address();
        let topic = Uuid::from_u128(9);
        let request = StoredKeyRequest {
            canonical_id: Uuid::from_u128(10),
            confirmed_request_id: None,
            device_name: "Desktop".into(),
            pair: RequestKeyPair::from_private_bytes(&[5; 32]).unwrap(),
            recovery: None,
        };
        store.save_request(&target, topic, &request).unwrap();
        let loaded = store.load_request(&target, topic).unwrap().unwrap();
        assert_eq!(loaded.pair.public_key(), request.pair.public_key());
        let mut same_ids_other_device = target.clone();
        same_ids_other_device.device_id = Uuid::from_u128(19);
        assert!(
            store
                .load_request(&same_ids_other_device, topic)
                .unwrap()
                .is_none()
        );
        let receipt = KeyRequestCompletion {
            canonical_id: request.canonical_id,
            request_id: Uuid::new_v5(&topic, request.canonical_id.to_string().as_bytes()),
            response_id: Uuid::from_u128(11),
            outcome: KeyRequestOutcome::Granted,
        };
        let key = GroupKey::from_bytes([7; 32]);
        assert!(
            store
                .complete_request(&target, topic, &receipt, Some(&key))
                .unwrap()
        );
        assert!(store.load_request(&target, topic).unwrap().is_none());
        assert_eq!(
            store.load(&target).unwrap().unwrap().as_bytes(),
            key.as_bytes()
        );
        store.remove(&target).unwrap();
        assert!(
            !store
                .complete_request(&target, topic, &receipt, Some(&key))
                .unwrap()
        );
        assert_eq!(
            store
                .load_request_completion(&target, topic, receipt.request_id)
                .unwrap(),
            Some(receipt)
        );
        assert!(store.load(&target).unwrap().is_none());
    }

    /// Замена сохраняет новый запрос только после совпадения исходной версии и не открывает старую выдачу.
    #[test]
    fn replacing_pending_key_request_requires_exact_stored_revision() {
        use workspace_core::messenger::encryption::{
            grant::RequestKeyPair, key_exchange::KeyRecovery,
        };
        let store = ProfileChatKeys::temporary();
        let target = address();
        let topic = Uuid::from_u128(17);
        let original = StoredKeyRequest {
            canonical_id: Uuid::from_u128(18),
            confirmed_request_id: None,
            device_name: "Desktop".into(),
            pair: RequestKeyPair::from_private_bytes(&[5; 32]).unwrap(),
            recovery: None,
        };
        let replacement = StoredKeyRequest {
            canonical_id: Uuid::from_u128(19),
            confirmed_request_id: None,
            device_name: "Desktop".into(),
            pair: RequestKeyPair::from_private_bytes(&[6; 32]).unwrap(),
            recovery: Some(KeyRecovery {
                message_id: Uuid::from_u128(20),
                ciphertext: "sample".into(),
                previous_key_fingerprint: [9; 32],
            }),
        };
        store.save_request(&target, topic, &original).unwrap();
        assert!(
            !store
                .replace_request(&target, topic, Uuid::new_v4(), &replacement)
                .unwrap()
        );
        assert!(store.save_request(&target, topic, &replacement).is_err());
        assert!(
            store
                .replace_request(&target, topic, original.canonical_id, &replacement)
                .unwrap()
        );
        assert_eq!(
            store
                .load_request(&target, topic)
                .unwrap()
                .unwrap()
                .canonical_id,
            replacement.canonical_id
        );
        let previous = KeyRequestCompletion {
            canonical_id: original.canonical_id,
            request_id: Uuid::new_v5(&topic, original.canonical_id.to_string().as_bytes()),
            response_id: Uuid::from_u128(21),
            outcome: KeyRequestOutcome::Granted,
        };
        assert!(
            !store
                .complete_request(
                    &target,
                    topic,
                    &previous,
                    Some(&GroupKey::from_bytes([7; 32]))
                )
                .unwrap()
        );
        assert!(
            !store
                .replace_request(&target, topic, original.canonical_id, &replacement)
                .unwrap()
        );
        assert!(store.load(&target).unwrap().is_none());
    }
}
