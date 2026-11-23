//! Ведёт создание защищённого чата и смену поколения ключа от намерения до подтверждения.
//! Хранит этапы и идентификаторы в базе, а скрытый ключ открывает в защищённом адаптере после сервера.

use std::{future::poll_fn, pin::pin, task::Poll};

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    ChatKeyStore, EncryptionActivation, EncryptionError, GroupKey, HeldChatKey, KEY_BYTES,
    KeyStoreError, MessageEncryption, key_address,
    key_exchange::KeyRequestTarget,
    key_service::{KeyServiceMessage, KeyServicePayload},
};
use crate::{
    auth::{AuthError, Service},
    messenger::{
        MessengerSource, details::DetailsError, history::OpenHistoryError, send::SendError,
    },
    runtime::SourceError,
    storage::{HistoryScope, LifecyclePhase, StorageError, StoredKeyLifecycle},
    sync::Sync,
};

/// Ввод общей формы создания защищённого канала.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EncryptedChannelInput {
    pub name: String,        // Имя нового канала.
    pub description: String, // Описание, если его заполнили.
    pub members: Vec<Uuid>,  // Пользователи, которым нужны приглашения.
}

/// Исходное намерение, которое сохраняется для безопасного продолжения.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum KeyLifecycleIntent {
    CreateChannel(EncryptedChannelInput), // Создание с тем же именем и участниками.
    Rotate {
        topic_id: Uuid,        // Точная тема размещения объявления.
        previous_key_id: Uuid, // Исходное поколение, сверенное перед созданием операции.
    },
}

/// Сохранённое незавершённое намерение для восстановления после закрытия окна.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingKeyLifecycle {
    pub operation_id: Uuid, // UUID намерения, для ротации также канонический UUID POST.
    pub stream_id: Uuid,    // Точный стрим или заранее выбранный UUID нового стрима.
    pub key_id: Uuid,       // UUID нового поколения. Байты ключа остаются в адаптере.
    pub intent: KeyLifecycleIntent, // Параметры продолжаемой операции.
}

impl PendingKeyLifecycle {
    /// Закрепляет идентификаторы, которые переживают любое повторное открытие и сетевой сбой.
    fn new(stream_id: Uuid, intent: KeyLifecycleIntent) -> Self {
        Self {
            operation_id: Uuid::new_v4(),
            stream_id,
            key_id: Uuid::new_v4(),
            intent,
        }
    }
}

/// Созданный канал, который можно открыть по штатному маршруту темы.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreatedEncryptedChannel {
    pub stream_id: Uuid, // UUID нового канала.
    pub topic_id: Uuid,  // Тема по умолчанию.
    pub key_id: Uuid,    // Первое подтверждённое поколение.
}

/// Сервер подтвердил объявление поколения и сохранил его для чтения на устройстве.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyRotationResult {
    pub stream_id: Uuid,      // Стрим объявления.
    pub topic_id: Uuid,       // Подтверждённое размещение объявления.
    pub key_id: Uuid,         // Поколение, объявленное этой операцией.
    pub current_key_id: Uuid, // Текущее поколение из последней серверной проверки.
}

/// Сведения для панели ключей. Байты поколений остаются в защищённом адаптере.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalStreamKeys {
    pub topic_id: Uuid, // Подтверждённая тема запроса ключа, даже если сообщений ещё нет.
    pub current_key_id: Uuid, // UUID поколения, которое объявлено сервером.
    pub can_rotate: bool, // Владелец стрима может начать ротацию.
    pub keys: Vec<HeldChatKey>, // Открытые локальные поколения того же устройства.
}

/// Причина остановки создания или ротации.
#[derive(Debug, Error)]
pub enum KeyLifecycleError {
    #[error("invalid chat key operation")]
    InvalidInput, // Параметры формы либо идентификатор операции не подходят.
    #[error("chat key operation is unavailable on this device")]
    MissingOperation, // Намерение отсутствует у выбранного владельца и устройства.
    #[error("the operation outcome is not confirmed")]
    Uncertain, // Пока не удалось найти подтверждение по закреплённому UUID.
    #[error("channel creation was rejected (HTTP {0})")]
    CreateRejected(u16), // Прежнее намерение закрыто, форма может начать создание заново.
    #[error("chat or key generation changed")]
    Changed, // Серверная идентичность или предыдущее поколение изменились.
    #[error("only the current owner can change the protected chat")]
    NotOwner, // Вызов пришёл не от действующего владельца нативного чата.
    #[error("the prepared key was removed")]
    MissingStagedKey, // После отправки сохраняем исходный UUID и ждём тот же ключ.
    #[error(transparent)]
    Auth(#[from] AuthError), // Вход завершился или сменилось его поколение.
    #[error(transparent)]
    Key(#[from] EncryptionError), // Активация устарела или случайность недоступна.
    #[error(transparent)]
    KeyStore(#[from] KeyStoreError), // Системный адаптер отказал в чтении или записи.
    #[error(transparent)]
    Storage(#[from] StorageError), // Сбой журнала намерений.
    #[error(transparent)]
    Source(#[from] SourceError), // Сбой чтения или неверная проекция сервера.
    #[error(transparent)]
    Send(#[from] SendError), // Отказ или неизвестный итог операции POST.
    #[error(transparent)]
    History(#[from] OpenHistoryError), // Стрим или тема недоступны.
    #[error(transparent)]
    Invitations(#[from] DetailsError), // Приглашения требуют повтора.
}

/// Связывает журнал и защищённый адаптер с текущей активацией.
struct ActiveLifecycle<'a> {
    sync: &'a Sync,                       // Журнал и проверка поколения авторизации.
    keys: &'a MessageEncryption,          // Системный адаптер выбранного профиля.
    activation: &'a EncryptionActivation, // Точный выбор пользователя.
}

impl ActiveLifecycle<'_> {
    /// Подтверждает поколение входа и выбор пользователя без доступа к ключам.
    fn current(&self) -> Result<(), KeyLifecycleError> {
        let session = self.activation.session();
        self.sync
            .auth()
            .with_active_generation(&session.owner, session.generation, || ())?;
        if !self
            .keys
            .selection
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|selection| &selection.activation == self.activation)
        {
            return Err(EncryptionError::Stale.into());
        }
        Ok(())
    }

    /// Получает токен того же входа и проверяет его выбор после ожидания.
    async fn token(&self) -> Result<String, KeyLifecycleError> {
        let session = self.activation.session();
        let token = self
            .sync
            .auth()
            .access_token_for_generation(&session.owner, Service::Messenger, session.generation)
            .await?;
        self.current()?;
        Ok(token)
    }

    /// Начинает запрос записи под действующим входом и выбором, затем отпускает блокировки.
    async fn dispatch_request<T>(
        &self,
        request: impl Future<Output = T>,
    ) -> Result<T, KeyLifecycleError> {
        let session = self.activation.session();
        let mut request = pin!(request);
        let mut admitted = false;
        let result = poll_fn(|context| {
            if admitted {
                return request.as_mut().poll(context).map(Ok);
            }
            let first =
                self.sync
                    .auth()
                    .with_active_generation(&session.owner, session.generation, || {
                        let selection = self.keys.selection.lock().unwrap();
                        if selection
                            .as_ref()
                            .is_some_and(|choice| &choice.activation == self.activation)
                        {
                            Ok(request.as_mut().poll(context))
                        } else {
                            Err(EncryptionError::Stale.into())
                        }
                    });
            match first
                .map_err(KeyLifecycleError::from)
                .and_then(|checked| checked)
            {
                Ok(result) => {
                    admitted = true;
                    result.map(Ok)
                }
                Err(error) => Poll::Ready(Err(error)),
            }
        })
        .await?;
        self.current()?;
        Ok(result)
    }

    /// Выполняет запись журнала только для ещё действующего поколения.
    fn in_storage<T>(
        &self,
        action: impl FnOnce(&crate::storage::Storage) -> Result<T, StorageError>,
    ) -> Result<T, KeyLifecycleError> {
        let session = self.activation.session();
        self.current()?;
        Ok(self.sync.auth().with_active_generation(
            &session.owner,
            session.generation,
            || action(self.sync.storage()),
        )??)
    }

    /// Закрепляет намерение, затем сохраняет ключ до любой сетевой операции.
    fn stage(&self, operation: &PendingKeyLifecycle) -> Result<Uuid, KeyLifecycleError> {
        let session = self.activation.session();
        let saved = self.in_storage(|storage| {
            storage.stage_key_lifecycle(&session.owner, session.device_id, operation)
        })?;
        if !saved {
            return Err(KeyLifecycleError::InvalidInput);
        }
        self.prepare_key(operation)?;
        Ok(operation.operation_id)
    }

    /// Создаёт 32 случайных байта только до разрешения первого запроса.
    fn prepare_key(&self, operation: &PendingKeyLifecycle) -> Result<(), KeyLifecycleError> {
        self.with_keys(|store| {
            let address = key_address(
                self.activation.session(),
                operation.stream_id,
                operation.key_id,
            );
            if !store.has_staged_lifecycle_key(&address)? {
                let mut bytes = Zeroizing::new([0_u8; KEY_BYTES]);
                SystemRandom::new()
                    .fill(bytes.as_mut())
                    .map_err(|_| EncryptionError::Seal)?;
                store.stage_lifecycle_key(&address, &GroupKey::from_bytes(*bytes))?;
            }
            Ok(())
        })
    }

    /// Закрепляет право на сетевой запрос, пока исходный ключ ещё хранится скрытым.
    fn dispatch(
        &self,
        operation: &PendingKeyLifecycle,
        previous: LifecyclePhase,
    ) -> Result<(), KeyLifecycleError> {
        self.with_keys(|store| {
            let address = key_address(
                self.activation.session(),
                operation.stream_id,
                operation.key_id,
            );
            if !store.has_staged_lifecycle_key(&address)? {
                return Err(KeyLifecycleError::MissingStagedKey);
            }
            if previous == LifecyclePhase::Prepared {
                self.advance(operation, previous, LifecyclePhase::Dispatched, None)?;
            }
            Ok(())
        })
    }

    /// Закрывает отклонённое создание под той же активацией и убирает скрытый ключ.
    fn discard_rejected_creation(
        &self,
        operation: &PendingKeyLifecycle,
    ) -> Result<(), KeyLifecycleError> {
        self.with_keys(|store| {
            let session = self.activation.session();
            self.sync
                .auth()
                .with_active_generation(&session.owner, session.generation, || {
                    let selection = self.keys.selection.lock().unwrap();
                    if !selection
                        .as_ref()
                        .is_some_and(|choice| &choice.activation == self.activation)
                    {
                        return Err(EncryptionError::Stale.into());
                    }
                    if !self.sync.storage().discard_rejected_creation(
                        &session.owner,
                        session.device_id,
                        operation,
                    )? {
                        return Err(KeyLifecycleError::Changed);
                    }
                    // Сначала закрываем намерение. При сбое адаптера скрытая запись остаётся недоступна.
                    let address = key_address(session, operation.stream_id, operation.key_id);
                    if store.remove(&address).is_err() {
                        log::warn!("event=encrypted_creation_rejected_key_cleanup_failed");
                    }
                    Ok(())
                })?
        })
    }

    /// Открывает только существующую запись после серверного подтверждения её UUID.
    fn activate(&self, operation: &PendingKeyLifecycle) -> Result<(), KeyLifecycleError> {
        self.with_keys(|store| {
            let address = key_address(
                self.activation.session(),
                operation.stream_id,
                operation.key_id,
            );
            let needs_publish = store.load(&address)?.is_none();
            if !store.activate_staged_lifecycle_key(&address)? {
                return Err(KeyLifecycleError::MissingStagedKey);
            }
            if needs_publish {
                self.keys.publish_key_change(
                    self.activation,
                    operation.stream_id,
                    operation.key_id,
                    true,
                );
            }
            Ok(())
        })
    }

    /// Упорядочивает локальные изменения скрытого ключа с чтением и его удалением.
    fn with_keys<T>(
        &self,
        action: impl FnOnce(&dyn ChatKeyStore) -> Result<T, KeyLifecycleError>,
    ) -> Result<T, KeyLifecycleError> {
        let _operation = self.keys.store_operations.lock().unwrap();
        self.current()?;
        action(self.keys.store.as_ref())
    }

    /// Читает только запись исходного владельца и устройства.
    fn load(&self, id: Uuid) -> Result<StoredKeyLifecycle, KeyLifecycleError> {
        let session = self.activation.session();
        self.in_storage(|storage| storage.key_lifecycle(&session.owner, session.device_id, id))?
            .ok_or(KeyLifecycleError::MissingOperation)
    }

    /// Возвращает исходные намерения без байтов ключей.
    fn pending(&self) -> Result<Vec<PendingKeyLifecycle>, KeyLifecycleError> {
        let session = self.activation.session();
        self.in_storage(|storage| storage.pending_key_lifecycle(&session.owner, session.device_id))
    }

    /// Фиксирует один переход состояния, который следующий вызов сможет продолжить.
    fn advance(
        &self,
        operation: &PendingKeyLifecycle,
        previous: LifecyclePhase,
        next: LifecyclePhase,
        topic_id: Option<Uuid>,
    ) -> Result<(), KeyLifecycleError> {
        let session = self.activation.session();
        let saved = self.in_storage(|storage| {
            storage.advance_key_lifecycle(
                &session.owner,
                session.device_id,
                operation.operation_id,
                previous,
                next,
                topic_id,
            )
        })?;
        if !saved {
            return Err(KeyLifecycleError::Changed);
        }
        Ok(())
    }
}

impl MessengerSource {
    /// Сверяет серверный доступ и возвращает только метаданные локальных ключей.
    pub async fn local_stream_keys(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
    ) -> Result<LocalStreamKeys, KeyLifecycleError> {
        let profile = self.encryption_profile(activation)?;
        let active = ActiveLifecycle {
            sync,
            keys: &profile.keys,
            activation,
        };
        let session = activation.session();
        let version = || {
            active.in_storage(|storage| {
                Ok((
                    storage.media_history_generation(&session.owner)?,
                    storage.media_access_revision(&session.owner, scope)?,
                ))
            })
        };
        let opened = version()?;
        let token = active.token().await?;
        let route = self
            .rest
            .local_keys_route(&session.owner, &token, scope)
            .await;
        active.current()?;
        let (topic_id, current_key_id, can_rotate) = route?;
        if version()? != opened {
            return Err(OpenHistoryError::AccessLost.into());
        }
        let mut keys = active.with_keys(|store| {
            Ok(store.list(&session.owner, session.device_id, scope.stream_id)?)
        })?;
        if version()? != opened {
            return Err(OpenHistoryError::AccessLost.into());
        }
        keys.sort_unstable_by_key(|key| (std::cmp::Reverse(key.received_order), key.key_id));
        Ok(LocalStreamKeys {
            topic_id,
            current_key_id,
            can_rotate,
            keys,
        })
    }

    /// Резервирует UUID канала и первого ключа и сохраняет ключ до отправки формы.
    pub fn stage_encrypted_channel(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        mut input: EncryptedChannelInput,
    ) -> Result<Uuid, KeyLifecycleError> {
        let profile = self.encryption_profile(activation)?;
        input.name = input.name.trim().to_owned();
        let user = owner_uuid(activation)?;
        if input.name.is_empty() || input.members.iter().any(Uuid::is_nil) {
            return Err(KeyLifecycleError::InvalidInput);
        }
        input.members.sort_unstable();
        input.members.dedup();
        input.members.retain(|member| *member != user);
        ActiveLifecycle {
            sync,
            keys: &profile.keys,
            activation,
        }
        .stage(&PendingKeyLifecycle::new(
            Uuid::new_v4(),
            KeyLifecycleIntent::CreateChannel(input),
        ))
    }

    /// Продолжает создание, сверяет точный UUID и открывает ключ до приглашений.
    pub async fn create_encrypted_channel(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        operation_id: Uuid,
    ) -> Result<CreatedEncryptedChannel, KeyLifecycleError> {
        let profile = self.encryption_profile(activation)?;
        let _operation = profile.lifecycle_gate.lock().await;
        let active = ActiveLifecycle {
            sync,
            keys: &profile.keys,
            activation,
        };
        let StoredKeyLifecycle {
            operation,
            phase,
            verified_topic_id,
        } = active.load(operation_id)?;
        let KeyLifecycleIntent::CreateChannel(ref input) = operation.intent else {
            return Err(KeyLifecycleError::InvalidInput);
        };
        let owner = &activation.session().owner;
        if phase == LifecyclePhase::Complete {
            return Ok(CreatedEncryptedChannel {
                stream_id: operation.stream_id,
                key_id: operation.key_id,
                topic_id: verified_topic_id.ok_or(KeyLifecycleError::Changed)?,
            });
        }
        let token = active.token().await?;
        if phase == LifecyclePhase::Prepared {
            active.prepare_key(&operation)?;
            active.dispatch(&operation, phase)?;
            let sent = active
                .dispatch_request(self.rest.create_encrypted_stream_once(
                    owner,
                    &token,
                    operation.stream_id,
                    operation.key_id,
                    input,
                ))
                .await?;
            if let Err(KeyLifecycleError::Send(SendError::Rejected(status))) = sent {
                active.discard_rejected_creation(&operation)?;
                return Err(KeyLifecycleError::CreateRejected(status));
            }
            sent?;
        }
        let expected_key = (phase != LifecyclePhase::Verified).then_some(operation.key_id);
        let confirmed = self
            .rest
            .confirmed_encrypted_creation(owner, &token, operation.stream_id, expected_key)
            .await;
        active.current()?;
        let topic_id = confirmed?;
        if phase == LifecyclePhase::Verified {
            if verified_topic_id != Some(topic_id) {
                return Err(KeyLifecycleError::Changed);
            }
        } else {
            active.advance(
                &operation,
                LifecyclePhase::Dispatched,
                LifecyclePhase::Verified,
                Some(topic_id),
            )?;
        }
        active.activate(&operation)?;
        let missing = self
            .rest
            .missing_encrypted_channel_members(owner, &token, operation.stream_id, &input.members)
            .await;
        active.current()?;
        let missing = missing?;
        if !missing.is_empty() {
            active
                .dispatch_request(self.rest.add_sidebar_members(
                    owner,
                    &token,
                    operation.stream_id,
                    &missing,
                ))
                .await??;
        }
        active.advance(
            &operation,
            LifecyclePhase::Verified,
            LifecyclePhase::Complete,
            Some(topic_id),
        )?;
        Ok(CreatedEncryptedChannel {
            stream_id: operation.stream_id,
            topic_id,
            key_id: operation.key_id,
        })
    }

    /// Показывает операции того же устройства после повторного входа или перезапуска.
    pub fn pending_key_lifecycle(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
    ) -> Result<Vec<PendingKeyLifecycle>, KeyLifecycleError> {
        let profile = self.encryption_profile(activation)?;
        ActiveLifecycle {
            sync,
            keys: &profile.keys,
            activation,
        }
        .pending()
    }

    /// Сохраняет новое поколение после проверки точного стрима, владельца и темы.
    pub async fn stage_key_rotation(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        scope: HistoryScope,
    ) -> Result<Uuid, KeyLifecycleError> {
        let profile = self.encryption_profile(activation)?;
        let _operation = profile.lifecycle_gate.lock().await;
        let active = ActiveLifecycle {
            sync,
            keys: &profile.keys,
            activation,
        };
        let token = active.token().await?;
        let owner = &activation.session().owner;
        let destination = self
            .rest
            .key_rotation_destination(owner, &token, scope)
            .await;
        active.current()?;
        let (topic_id, previous_key_id) = destination?;
        if let Some(pending) = active.pending()?.into_iter().find(|pending| {
            pending.stream_id == scope.stream_id && matches!(pending.intent, KeyLifecycleIntent::Rotate { previous_key_id: previous, .. }
                if previous == previous_key_id || pending.key_id == previous_key_id)
        }) {
            return Ok(pending.operation_id);
        }
        active.stage(&PendingKeyLifecycle::new(
            scope.stream_id,
            KeyLifecycleIntent::Rotate {
                topic_id,
                previous_key_id,
            },
        ))
    }

    /// Подтверждает ротацию через исходный UUID и оставляет прежние ключи локально доступными.
    pub async fn rotate_encrypted_key(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        operation_id: Uuid,
    ) -> Result<KeyRotationResult, KeyLifecycleError> {
        let profile = self.encryption_profile(activation)?;
        let _operation = profile.lifecycle_gate.lock().await;
        let active = ActiveLifecycle {
            sync,
            keys: &profile.keys,
            activation,
        };
        let StoredKeyLifecycle {
            operation, phase, ..
        } = active.load(operation_id)?;
        let KeyLifecycleIntent::Rotate {
            topic_id,
            previous_key_id,
        } = operation.intent
        else {
            return Err(KeyLifecycleError::InvalidInput);
        };
        let owner = &activation.session().owner;
        let scope = HistoryScope {
            stream_id: operation.stream_id,
            topic_id: Some(topic_id),
        };
        let token = active.token().await?;
        let destination = self
            .rest
            .key_rotation_destination(owner, &token, scope)
            .await;
        active.current()?;
        let (_, mut current_key_id) = destination?;
        if matches!(phase, LifecyclePhase::Prepared | LifecyclePhase::Dispatched) {
            let announcement = RotationAnnouncement {
                target: KeyRequestTarget {
                    stream_id: operation.stream_id,
                    topic_id,
                    key_id: operation.key_id,
                },
                operation_id,
                previous_key_id,
            };
            self.dispatch_rotation(
                &active,
                &operation,
                phase,
                &token,
                announcement,
                current_key_id,
            )
            .await?;
            active.advance(
                &operation,
                LifecyclePhase::Dispatched,
                LifecyclePhase::Verified,
                Some(topic_id),
            )?;
            let destination = self
                .rest
                .key_rotation_destination(owner, &token, scope)
                .await;
            active.current()?;
            current_key_id = destination?.1;
        }
        if phase != LifecyclePhase::Complete {
            if current_key_id == previous_key_id {
                return Err(KeyLifecycleError::Uncertain);
            }
            active.activate(&operation)?;
            active.advance(
                &operation,
                LifecyclePhase::Verified,
                LifecyclePhase::Complete,
                Some(topic_id),
            )?;
        }
        Ok(KeyRotationResult {
            stream_id: operation.stream_id,
            topic_id,
            key_id: operation.key_id,
            current_key_id,
        })
    }

    /// Сверяет неизвестный ответ ротации через точный GET или повторяет те же UUID и тело.
    async fn dispatch_rotation(
        &self,
        active: &ActiveLifecycle<'_>,
        operation: &PendingKeyLifecycle,
        phase: LifecyclePhase,
        token: &str,
        announcement: RotationAnnouncement,
        mut current_key_id: Uuid,
    ) -> Result<(), KeyLifecycleError> {
        let owner = &active.activation.session().owner;
        let public_id = announcement.target.public_id(announcement.operation_id);
        if phase == LifecyclePhase::Dispatched {
            let lookup = self
                .rest
                .lookup_key_service_message(owner, token, announcement.target, public_id)
                .await;
            active.current()?;
            if let Some(receipt) = lookup? {
                return announcement.confirm(receipt, owner_uuid(active.activation)?);
            }
            // Сверка объявления могла ждать сеть. Перечитываем текущее поколение перед повтором.
            let scope = HistoryScope {
                stream_id: announcement.target.stream_id,
                topic_id: Some(announcement.target.topic_id),
            };
            let destination = self
                .rest
                .key_rotation_destination(owner, token, scope)
                .await;
            active.current()?;
            current_key_id = destination?.1;
        }
        if current_key_id != announcement.previous_key_id {
            return Err(KeyLifecycleError::Changed);
        }
        if phase == LifecyclePhase::Prepared {
            active.prepare_key(operation)?;
        }
        active.dispatch(operation, phase)?;
        let payload = KeyServicePayload::Announced {
            key_uuid: announcement.target.key_id,
        };
        let sent = active
            .dispatch_request(self.rest.post_key_service_once(
                owner,
                token,
                announcement.target,
                announcement.operation_id,
                &payload,
            ))
            .await??;
        announcement.confirm(sent, owner_uuid(active.activation)?)
    }
}

/// Канонический UUID и цель объявления, которые нельзя менять между попытками.
#[derive(Clone, Copy)]
struct RotationAnnouncement {
    target: KeyRequestTarget, // Точная тема, стрим и новое поколение.
    operation_id: Uuid,       // Канонический UUID сообщения в POST.
    previous_key_id: Uuid,    // Поколение, которое инициатор видел перед ротацией.
}

impl RotationAnnouncement {
    /// Принимает только точное объявление, опубликованное исходным владельцем.
    fn confirm(self, receipt: KeyServiceMessage, owner_id: Uuid) -> Result<(), KeyLifecycleError> {
        if receipt.id != self.target.public_id(self.operation_id)
            || receipt.author_id != owner_id
            || receipt.stream_id != self.target.stream_id
            || receipt.topic_id != self.target.topic_id
            || receipt.payload
                != (KeyServicePayload::Announced {
                    key_uuid: self.target.key_id,
                })
        {
            return Err(KeyLifecycleError::Changed);
        }
        Ok(())
    }
}

/// Проверяет канонический UUID пользователя выбранного входа.
fn owner_uuid(activation: &EncryptionActivation) -> Result<Uuid, KeyLifecycleError> {
    Uuid::parse_str(&activation.session().owner.user_id)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or(KeyLifecycleError::InvalidInput)
}
