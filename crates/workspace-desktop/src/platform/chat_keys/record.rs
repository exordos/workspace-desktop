//! Описывает одну защищённую запись чата: ключи поколений, текущие запросы и квитанции.
//! Секреты в записи стираются при освобождении рабочей копии.

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workspace_core::messenger::encryption::{
    GroupKey, KeyStoreError,
    grant::RequestKeyPair,
    key_exchange::{KeyRecovery, KeyRequestCompletion, StoredKeyReply, StoredKeyRequest},
};
use zeroize::Zeroize;

/// Данные одного чата и устройства, которые адаптер заменяет одной записью Keychain.
#[derive(Default, Deserialize, Serialize)]
pub(super) struct ChatRecord {
    clock: u64, // Счётчик операций для сортировки доступных ключей.
    pub generations: HashMap<Uuid, Generation>, // Данные по UUID поколения.
}

/// Доступ и обмен ключом одного поколения. После удаления остаются только записи повтора.
#[derive(Default, Deserialize, Serialize)]
pub(super) struct Generation {
    #[serde(default)]
    pub lifecycle_removed: bool, // Удалённое поколение нельзя снова открыть повтором подготовки.
    pub material: Option<KeyMaterial>, // None после удаления или до получения ключа.
    pub requests: HashMap<Uuid, Pending>, // Один текущий запрос для каждой темы.
    pub receipts: HashMap<Uuid, HashMap<Uuid, KeyRequestCompletion>>, // Тема и публичный UUID запроса.
    pub replies: HashMap<Uuid, HashMap<Uuid, StoredKeyReply>>, // Сохранённые ответы по тому же адресу.
}

/// Секрет и порядок его выбора. Подготовленный ключ не появляется в обычных чтениях.
#[derive(Deserialize, Serialize)]
pub(super) struct KeyMaterial {
    pub secret: SecretBytes,     // Групповой ключ защищённого чата.
    pub pending_lifecycle: bool, // Ожидается подтверждение создания или смены поколения.
    pub received_order: u64,     // Первое добавление ключа на это устройство.
    pub send_order: u64, // Последний успешный выбор для новой отправки; 0 означает отсутствие.
    #[serde(default)]
    pub added_at_unix_ms: Option<u64>, // Время первого сохранения после Unix epoch, если было зафиксировано.
}

/// Это временная копия массива секрета, которую использует сериализатор записи Keychain.
#[derive(Deserialize, Serialize)]
#[serde(transparent)]
pub(super) struct SecretBytes([u8; 32]); // Байты группового или приватного ключа.

impl Drop for SecretBytes {
    /// Стирает рабочие байты при завершении операции адаптера.
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl SecretBytes {
    /// Возвращает отдельный защищённый объект для операции Core.
    pub(super) fn group(&self) -> GroupKey {
        GroupKey::from_bytes(self.0)
    }
}

/// Формат подготовленного запроса, который переживает неизвестный результат POST.
#[derive(Deserialize, Serialize)]
pub(super) struct Pending {
    pub canonical_id: Uuid, // Исходный ID запроса перед первой отправкой.
    pub confirmed_request_id: Option<Uuid>, // Публичный ID в точной теме, если сервер подтвердил запрос.
    name: String,                           // Имя устройства из строки первоначального запроса.
    private: SecretBytes,                   // Секрет для приёма зашифрованного ответа.
    #[serde(default)]
    recovery: Option<KeyRecovery>, // Точная версия сообщения при запросе замены ключа.
}

impl Pending {
    /// Копирует только данные, нужные для того же запроса после перезапуска.
    pub(super) fn new(request: &StoredKeyRequest) -> Self {
        Self {
            canonical_id: request.canonical_id,
            confirmed_request_id: request.confirmed_request_id,
            name: request.device_name.clone(),
            private: SecretBytes(*request.pair.private_bytes()),
            recovery: request.recovery.clone(),
        }
    }

    /// Восстанавливает пару и точное состояние, понятные Core.
    pub(super) fn open(&self) -> Result<StoredKeyRequest, KeyStoreError> {
        let pair =
            RequestKeyPair::from_private_bytes(&self.private.0).map_err(|_| KeyStoreError)?;
        Ok(StoredKeyRequest {
            canonical_id: self.canonical_id,
            confirmed_request_id: self.confirmed_request_id,
            device_name: self.name.clone(),
            pair,
            recovery: self.recovery.clone(),
        })
    }

    /// Разрешает подтверждение публикации только для неизменной пары запроса.
    pub(super) fn matches(&self, request: &StoredKeyRequest) -> bool {
        self.canonical_id == request.canonical_id
            && self.name == request.device_name
            && self.private.0 == *request.pair.private_bytes()
            && self.recovery == request.recovery
            && self
                .confirmed_request_id
                .is_none_or(|id| request.confirmed_request_id == Some(id))
    }
}

impl ChatRecord {
    /// Следующий порядок внутри чата, общий у сохранений и выборов для отправки.
    pub(super) fn next_order(&mut self) -> Result<u64, KeyStoreError> {
        self.clock = self.clock.checked_add(1).ok_or(KeyStoreError)?;
        Ok(self.clock)
    }

    /// Заполняет поколение, сохраняя первоначальный порядок ранее известного ключа.
    pub(super) fn put(
        &mut self,
        id: Uuid,
        key: &GroupKey,
        pending_lifecycle: bool,
    ) -> Result<(), KeyStoreError> {
        let (received_order, send_order, added_at_unix_ms) = match self
            .generations
            .get(&id)
            .and_then(|item| item.material.as_ref())
        {
            Some(former) => (
                former.received_order,
                former.send_order,
                former.added_at_unix_ms,
            ),
            None => {
                let added_at = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .and_then(|duration| u64::try_from(duration.as_millis()).ok());
                (self.next_order()?, 0, added_at)
            }
        };
        self.generations.entry(id).or_default().material = Some(KeyMaterial {
            secret: SecretBytes(*key.as_bytes()),
            pending_lifecycle,
            received_order,
            send_order,
            added_at_unix_ms,
        });
        Ok(())
    }
}
