//! Передаёт ответы служебных сообщений в Core и объединяет сигналы истории и realtime.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::Duration,
};

use gpui::BackgroundExecutor;
use uuid::Uuid;
use workspace_core::{
    context::{OwnerContext, SessionGeneration},
    messenger::encryption::{
        EncryptionActivation,
        key_exchange::{KeyExchange, KeyExchangeError, KeyRequestCompletion, KeyRequestTarget},
    },
    storage::{
        HistoryMessage, KeyResponseCursor, KeyResponseHint, KeyResponsePage, KeyServicePayload,
        MessageBody,
    },
    sync::{SyncChange, SyncUpdate},
};

use super::RuntimeClient;

/// Публичные ответы, которые проверили или поставили на проверку для выбранного входа.
#[derive(Default)]
pub(super) struct ReceivedKeys(Mutex<Received>); // Проверки только текущей активации.

/// Очередь хранит завершённые ответы отдельно от проверок, которые ещё выполняются.
#[derive(Default)]
struct Received {
    pending: HashMap<Uuid, Option<ReceivedReceipt>>, // UUID ответа; None пока выполняется проверка.
    finished: VecDeque<Uuid>, // Порядок завершённых проверок для ограничения памяти.
}

type ReceivedReceipt = (KeyRequestTarget, Option<KeyRequestCompletion>);

impl ReceivedKeys {
    /// Убирает результаты предыдущего выбора входа.
    pub(super) fn clear(&self) {
        *self.0.lock().unwrap() = Received::default();
    }

    /// Объединяет проверку по realtime и повторное получение того же ответа из истории.
    fn begin(&self, id: Uuid) -> bool {
        let mut entries = self.0.lock().unwrap();
        match entries.pending.entry(id) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(None);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    /// Запоминает только проверенный ответ. Выполняющиеся проверки не вытесняются.
    fn finish(&self, id: Uuid, result: ReceivedReceipt) {
        let mut entries = self.0.lock().unwrap();
        entries.pending.insert(id, Some(result));
        entries.finished.push_back(id);
        if entries.finished.len() > 512
            && let Some(old) = entries.finished.pop_front()
        {
            entries.pending.remove(&old);
        }
    }

    /// Учитывает ответ, который опередил возврат операции запроса в окно.
    fn receipt(&self, target: KeyRequestTarget, request: Uuid) -> Option<KeyRequestCompletion> {
        self.0.lock().unwrap().pending.values().find_map(|value| {
            let (placed, completion) = (*value)?;
            completion.filter(|completion| placed == target && completion.request_id == request)
        })
    }
}

/// Точное размещение ответа и локальное устройство, которым заинтересуется Core.
pub(in crate::gpui_client) struct IncomingKey {
    target: KeyRequestTarget, // Тема и поколение из события или снимка.
    id: Uuid,                 // Публичный UUID ответа, который перечитает Core.
    recipient: String,        // Устройство, указанное отправителем.
    persisted: Option<KeyResponseHint>, // Для архива Core сначала ищет точный ожидающий запрос устройства.
}

impl IncomingKey {
    /// Отбирает ответы из истории. Объявления и запросы не меняют локальные ключи.
    pub(in crate::gpui_client) fn message(message: &HistoryMessage) -> Option<Self> {
        let MessageBody::KeyService { payload, .. } = &message.body else {
            return None;
        };
        let target = match payload {
            KeyServicePayload::Grant { target, .. } | KeyServicePayload::Reject(target) => target,
            _ => return None,
        };
        Some(Self {
            target: KeyRequestTarget {
                stream_id: message.stream_id,
                topic_id: message.topic_id,
                key_id: target.key_uuid,
            },
            id: message.id,
            recipient: target.recipient_device_id.clone(),
            persisted: None,
        })
    }

    /// Использует сохранённое размещение только вместе с выбранным локальным устройством.
    pub(in crate::gpui_client) fn persisted(
        activation: &EncryptionActivation,
        hint: KeyResponseHint,
    ) -> Self {
        Self {
            target: hint.target(),
            id: hint.response_id,
            recipient: activation.session().device_id.to_string(),
            persisted: Some(hint),
        }
    }
}

impl RuntimeClient {
    /// Захватывает точный вход до постановки проверки ответа в очередь.
    pub(in crate::gpui_client) fn incoming_key_activation(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Option<EncryptionActivation> {
        self.encryption
            .get()?
            .active
            .lock()
            .unwrap()
            .as_ref()
            .filter(|active| {
                active.session().owner == *owner && active.session().generation == generation
            })
            .cloned()
    }

    /// Собирает точное направление из обновления, которое Sync опубликовал после записи истории.
    pub(in crate::gpui_client) fn incoming_key(
        &self,
        update: &SyncUpdate,
    ) -> Option<(EncryptionActivation, IncomingKey)> {
        let SyncChange::KeyServiceMessage {
            scope,
            message_id,
            key_id,
            recipient_device_id,
        } = &update.change
        else {
            return None;
        };
        let activation = self.incoming_key_activation(&update.context, update.generation)?;
        Some((
            activation,
            IncomingKey {
                target: KeyRequestTarget {
                    stream_id: scope.stream_id,
                    topic_id: scope.topic_id?,
                    key_id: *key_id,
                },
                id: *message_id,
                recipient: recipient_device_id.clone(),
                persisted: None,
            },
        ))
    }

    /// Даёт Core перечитать и проверить ответ, прежде чем хранилище сможет изменить ключ.
    pub(in crate::gpui_client) async fn apply_incoming_key(
        &self,
        activation: EncryptionActivation,
        incoming: IncomingKey,
        background: BackgroundExecutor,
    ) -> Option<(KeyRequestTarget, KeyRequestCompletion)> {
        let profile = self.encryption.get()?;
        {
            let active = profile.active.lock().unwrap();
            if active.as_ref() != Some(&activation)
                || incoming.recipient != activation.session().device_id.to_string()
                || !profile.received_keys.begin(incoming.id)
            {
                return None;
            }
        }
        let mut delay = Duration::from_millis(250);
        loop {
            let active = profile.active.lock().unwrap().as_ref() == Some(&activation);
            if !active || *self.stop.borrow() {
                return None;
            }
            let engine = profile.engine.clone();
            let captured = activation.clone();
            let IncomingKey { id, target, .. } = incoming;
            let persisted = incoming.persisted;
            let outcome = self
                .request(move |services| async move {
                    let exchange = KeyExchange::new(services.messenger, services.sync, engine);
                    match persisted {
                        Some(hint) => {
                            exchange
                                .apply_persisted_key_response(&captured, &hint)
                                .await
                        }
                        None => exchange.apply_key_response(&captured, target, id).await,
                    }
                })
                .await;
            let receipt = match outcome {
                Ok(Ok(receipt)) => receipt,
                Ok(Err(ref error)) if key_exchange_is_permanent(error) => None,
                _ => {
                    log::debug!("event=gpui_key_response_deferred message_id={id}");
                    background.timer(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                    continue;
                }
            };
            let active = profile.active.lock().unwrap();
            if active.as_ref() == Some(&activation) {
                profile.received_keys.finish(id, (target, receipt));
                return receipt.map(|receipt| (target, receipt));
            }
            return None;
        }
    }

    /// Читает очередную страницу сохранённых ответов для текущего устройства вне открытого разговора.
    pub(in crate::gpui_client) async fn saved_key_response_page(
        &self,
        activation: &EncryptionActivation,
        after: Option<KeyResponseCursor>,
        background: &BackgroundExecutor,
    ) -> Option<KeyResponsePage> {
        let session = activation.session();
        let mut delay = Duration::from_millis(250);
        while !*self.stop.borrow()
            && self
                .incoming_key_activation(&session.owner, session.generation)
                .as_ref()
                == Some(activation)
        {
            let session = session.clone();
            let cursor = after.clone();
            let result = self
                .request(move |services| async move {
                    services.sync.key_service_responses(
                        &session.owner,
                        session.generation,
                        session.device_id,
                        cursor,
                    )
                })
                .await;
            if let Ok(Ok(page)) = result {
                return Some(page);
            }
            background.timer(delay).await;
            delay = (delay * 2).min(Duration::from_secs(30));
        }
        None
    }

    /// Возвращает квитанцию только из ещё действующей активации для точного запроса.
    pub(in crate::gpui_client) fn key_request_completion(
        &self,
        activation: &EncryptionActivation,
        target: KeyRequestTarget,
        request: Uuid,
    ) -> Option<KeyRequestCompletion> {
        let profile = self.encryption.get()?;
        let active = profile.active.lock().unwrap();
        (active.as_ref() == Some(activation)).then_some(())?;
        profile.received_keys.receipt(target, request)
    }
}

/// Отсеивает отклонённые ответы. Сбой сети, рабочего потока и защищённого хранилища повторяются.
pub(in crate::gpui_client) fn key_exchange_is_permanent(error: &KeyExchangeError) -> bool {
    use workspace_core::messenger::history::OpenHistoryError;
    matches!(
        error,
        KeyExchangeError::Stale
            | KeyExchangeError::InvalidInput
            | KeyExchangeError::InvalidResponse
            | KeyExchangeError::Forbidden
            | KeyExchangeError::KeyConflict
            | KeyExchangeError::KeyAlreadyUsable
            | KeyExchangeError::MissingKey
            | KeyExchangeError::Grant(_)
            | KeyExchangeError::Access(
                OpenHistoryError::AccessLost | OpenHistoryError::MessageMissing
            )
    )
}
