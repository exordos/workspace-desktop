//! Проверяет создание и ротацию через локальный HTTPS с потерей ответов и повторным открытием базы.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use reqwest::Url;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Notify,
    task::JoinHandle,
};
use uuid::Uuid;
use workspace_core::{
    context::OwnerContext,
    messenger::{
        MessengerSource, SOURCE_ID,
        encryption::lifecycle::{
            CreatedEncryptedChannel, EncryptedChannelInput, KeyLifecycleError,
        },
        encryption::{
            ChatKeyAddress, ChatKeyStore, EncryptionActivation, EncryptionSession, GroupKey,
            HeldChatKey, KeyStoreError, MessageEncryption,
        },
        history::OpenHistoryError,
        send::SendError,
    },
    storage::{Cursor, HistoryChange, HistoryScope, Storage},
    sync::{Event, Sync},
};

use super::{CA, Fixture, HistoryApi, HttpsSocket, serve};

const OWNER: Uuid = Uuid::from_u128(801);
const OTHER: Uuid = Uuid::from_u128(802);
const MEMBER: Uuid = Uuid::from_u128(803);
const TOPIC: Uuid = Uuid::from_u128(804);
const DEVICE: Uuid = Uuid::from_u128(805);

/// Сохраняет серверные изменения независимо от того, дошёл ли ответ до клиента.
#[derive(Default)]
pub(super) struct Api {
    stream: Mutex<Option<Value>>, // Единственный нативный защищённый канал.
    creates: Mutex<Vec<Value>>,   // Все запросы создания.
    announces: Mutex<Vec<Value>>, // Все попытки объявления ключа.
    receipts: Mutex<HashMap<Uuid, Value>>, // Применённые объявления по публичному UUID.
    members: Mutex<HashSet<Uuid>>, // Участники, которых сервер уже добавил.
    invitations: Mutex<Vec<Value>>, // Все попытки приглашения.
    drop_create: AtomicBool,      // Потерять ответ после создания.
    reject_create: AtomicBool,    // Отклонить создание до записи канала на сервере.
    hide_stream: AtomicBool,      // Временно не видеть запись при сверке.
    drop_invite: AtomicBool,      // Потерять ответ после добавления участника.
    rotate_uncertain: AtomicBool, // Первое объявление не применяем, второе применяем без ответа.
    pause_bindings: AtomicBool,   // Остановить ответ о составе до смены входа.
    bindings_started: Notify,     // Сервер получил запрос состава.
    bindings_continue: Notify,    // Тест закончил смену входа.
}

impl Api {
    /// Принимает один HTTPS-запрос и при необходимости имитирует неизвестный результат.
    pub(super) async fn reply(
        &self,
        mut socket: HttpsSocket,
        uri: Url,
        method: &str,
        headers: &str,
    ) {
        let header = |expected: &str| {
            headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case(expected).then(|| value.trim())
            })
        };
        let user: Uuid = header("authorization")
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap()
            .parse()
            .unwrap();
        let length = header("content-length").map_or(0, |length| length.parse::<usize>().unwrap());
        let mut bytes = vec![0; length];
        socket.read_exact(&mut bytes).await.unwrap();
        let request = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        if method == "GET"
            && uri.path().ends_with("/stream_bindings/")
            && self.pause_bindings.swap(false, Ordering::Relaxed)
        {
            self.bindings_started.notify_one();
            self.bindings_continue.notified().await;
        }
        let Some((status, body)) = self.route(uri.path(), method, user, request) else {
            return;
        };
        let body = body.to_string();
        socket.write_all(format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len(),
        ).as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    }

    /// Выполняет точное серверное действие. Отсутствие результата закрывает соединение.
    fn route(
        &self,
        path: &str,
        method: &str,
        user: Uuid,
        request: Value,
    ) -> Option<(&'static str, Value)> {
        if method == "POST" && path.ends_with("/streams/") {
            assert_eq!(user, OWNER);
            assert_eq!(request["source_name"], "native");
            assert_eq!(request["encryption"], true);
            assert_eq!(request["invite_only"], false);
            assert_eq!(request["announce"], false);
            self.creates.lock().unwrap().push(request.clone());
            if self.reject_create.swap(false, Ordering::Relaxed) {
                return Some(("400 Bad Request", Value::Null));
            }
            let mut row = request;
            row["project_id"] = json!("project");
            row["user_uuid"] = json!(user);
            row["owner"] = json!(user);
            row["private"] = json!(false);
            row["is_archived"] = json!(false);
            row["default_topic_uuid"] = json!(TOPIC);
            row["notification_mode"] = json!("all_messages");
            *self.stream.lock().unwrap() = Some(row.clone());
            return (!self.drop_create.swap(false, Ordering::Relaxed))
                .then_some(("201 Created", row));
        }
        let remote = self.stream.lock().unwrap().clone();
        let stream = remote.as_ref().and_then(|row| row["uuid"].as_str());
        let id = path.rsplit('/').next().unwrap();
        if method == "GET" && path.contains("/streams/") {
            if self.hide_stream.load(Ordering::Relaxed) || stream != Some(id) {
                return Some(("404 Not Found", Value::Null));
            }
            let mut row = remote.unwrap();
            row["user_uuid"] = json!(user);
            return Some(("200 OK", row));
        }
        if path.contains("/stream_topics/") {
            assert_eq!(id, TOPIC.to_string());
            return Some((
                "200 OK",
                json!({
                    "uuid": TOPIC, "stream_uuid": stream.unwrap(), "project_id": "project",
                    "user_uuid": user, "notification_mode": "default", "is_done": false,
                }),
            ));
        }
        if path.ends_with("/stream_bindings/") {
            let members = self.members.lock().unwrap();
            return Some((
                "200 OK",
                json!(
                    members
                        .iter()
                        .map(|member| json!({
                            "uuid": member, "stream_uuid": stream.unwrap(), "project_id": "project",
                            "user_uuid": member, "role": "member",
                        }))
                        .collect::<Vec<_>>()
                ),
            ));
        }
        if path.ends_with("/actions/add_users/invoke") {
            assert_eq!(user, OWNER);
            self.invitations.lock().unwrap().push(request.clone());
            self.members.lock().unwrap().extend(
                request["member"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|id| id.as_str().unwrap().parse::<Uuid>().unwrap()),
            );
            return (!self.drop_invite.swap(false, Ordering::Relaxed))
                .then_some(("200 OK", json!([])));
        }
        if path.ends_with("/messages/") && method == "POST" {
            assert_eq!(user, OWNER);
            assert_eq!(request["stream_uuid"], stream.unwrap());
            assert_eq!(request["topic_uuid"], json!(TOPIC));
            assert_eq!(request["payload"]["kind"], "key_announced");
            let mut announces = self.announces.lock().unwrap();
            announces.push(request.clone());
            let uncertain = self.rotate_uncertain.load(Ordering::Relaxed);
            if uncertain && announces.len() == 1 {
                return None;
            }
            let public_id = Uuid::new_v5(&TOPIC, request["uuid"].as_str().unwrap().as_bytes());
            let row = json!({
                "uuid": public_id, "stream_uuid": stream.unwrap(), "topic_uuid": TOPIC,
                "project_id": "project", "user_uuid": user, "author_uuid": user, "source_name": "native",
                "payload": request["payload"],
            });
            self.stream.lock().unwrap().as_mut().unwrap()["current_encryption_key_uuid"] =
                request["payload"]["key_uuid"].clone();
            self.receipts.lock().unwrap().insert(public_id, row.clone());
            return (!uncertain).then_some(("201 Created", row));
        }
        if method == "GET" && path.contains("/messages/") {
            let id = id.parse().unwrap();
            let row = self.receipts.lock().unwrap().get(&id).cloned();
            return Some(row.map_or(("404 Not Found", Value::Null), |row| ("200 OK", row)));
        }
        panic!("неожиданный запрос {method} {path}");
    }
}

/// Держит статус ключа в одной записи, как требует контракт системного адаптера.
#[derive(Default)]
struct Keys(Mutex<HashMap<ChatKeyAddress, KeyEntry>>); // Защищённые записи тестового профиля.

/// Точное поколение, ещё скрытое или уже доступное для использования.
struct KeyEntry {
    key: GroupKey, // Хранимые байты ключа.
    active: bool,  // Сервер подтвердил создание или объявление.
}

impl ChatKeyStore for Keys {
    /// Возвращает только ключ, который уже открыт для чтения.
    fn load(&self, address: &ChatKeyAddress) -> Result<Option<GroupKey>, KeyStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(address)
            .filter(|entry| entry.active)
            .map(|entry| GroupKey::from_bytes(*entry.key.as_bytes())))
    }

    /// Сохраняет активный ключ при отдельном подтверждённом импорте.
    fn save(&self, address: &ChatKeyAddress, key: &GroupKey) -> Result<(), KeyStoreError> {
        self.0.lock().unwrap().insert(
            address.clone(),
            KeyEntry {
                key: GroupKey::from_bytes(*key.as_bytes()),
                active: true,
            },
        );
        Ok(())
    }

    /// Удаляет запись целиком, не оставляя ключ для восстановления.
    fn remove(&self, address: &ChatKeyAddress) -> Result<(), KeyStoreError> {
        self.0.lock().unwrap().remove(address);
        Ok(())
    }

    /// Перечисляет только открытые записи точного профиля, устройства и стрима.
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
            .iter()
            .filter_map(|(address, entry)| {
                (entry.active
                    && &address.owner == owner
                    && address.device_id == device_id
                    && address.stream_id == stream_id)
                    .then_some(HeldChatKey {
                        key_id: address.key_id,
                        received_order: 0,
                        send_order: 0,
                        added_at_unix_ms: Some(19),
                    })
            })
            .collect())
    }

    /// Резервирует скрытую запись без замены её ключа при повторе намерения.
    fn stage_lifecycle_key(
        &self,
        address: &ChatKeyAddress,
        key: &GroupKey,
    ) -> Result<(), KeyStoreError> {
        self.0
            .lock()
            .unwrap()
            .entry(address.clone())
            .or_insert_with(|| KeyEntry {
                key: GroupKey::from_bytes(*key.as_bytes()),
                active: false,
            });
        Ok(())
    }

    /// Проверяет наличие скрытой записи.
    fn has_staged_lifecycle_key(&self, address: &ChatKeyAddress) -> Result<bool, KeyStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(address)
            .is_some_and(|entry| !entry.active))
    }

    /// Меняет только признак существующей записи.
    fn activate_staged_lifecycle_key(
        &self,
        address: &ChatKeyAddress,
    ) -> Result<bool, KeyStoreError> {
        let mut keys = self.0.lock().unwrap();
        let Some(entry) = keys.get_mut(address) else {
            return Ok(false);
        };
        entry.active = true;
        Ok(true)
    }
}

/// Сохраняет один профиль и хранилище ключей при пересоздании Core.
struct Test {
    api: Arc<Api>,              // Управление локальным сервером.
    fixture: Fixture,           // Реестр двух синтетических входов.
    owner: OwnerContext,        // Канонический владелец канала.
    other: OwnerContext,        // Пользователь, который не владеет каналом.
    keys: Arc<Keys>,            // Аналог Keychain в тесте.
    profile: tempfile::TempDir, // SQLite на диске для повторного открытия.
    server: JoinHandle<()>,     // Сетевой процесс теста.
}

impl Test {
    /// Запускает локальный HTTPS и подготавливает два входа того же проекта.
    async fn new() -> Self {
        let api = Arc::new(Api::default());
        let (origin, server) = serve(Arc::new(HistoryApi {
            key_lifecycle: Some(api.clone()),
            ..HistoryApi::default()
        }))
        .await;
        let fixture = Fixture::new(|_, _| unreachable!());
        let owner = fixture.login_at(&origin, &OWNER.to_string());
        let other = fixture.login_at(&origin, &OTHER.to_string());
        Self {
            api,
            fixture,
            owner,
            other,
            keys: Arc::default(),
            profile: tempfile::tempdir().unwrap(),
            server,
        }
    }

    /// Пересоздаёт Core и выбирает новое окно того же устройства.
    fn reopen(&self, owner: &OwnerContext) -> (Sync, MessengerSource, EncryptionActivation) {
        let sync = Sync::new(
            self.fixture.auth.clone(),
            Arc::new(Storage::open(self.profile.path().join("workspace.sqlite")).unwrap()),
        );
        let encryption = Arc::new(MessageEncryption::new(self.keys.clone()));
        let generation = self
            .fixture
            .auth
            .snapshot()
            .iter()
            .find(|session| session.context == *owner)
            .unwrap()
            .generation;
        let activation = encryption
            .select_session(Some(EncryptionSession {
                owner: owner.clone(),
                generation,
                device_id: DEVICE,
            }))
            .unwrap();
        let source =
            MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
                .unwrap()
                .with_chat_encryption(encryption, DEVICE);
        (sync, source, activation)
    }

    /// Возвращает точный адрес ключа, который открывается после серверной сверки.
    fn address(&self, stream_id: Uuid, key_id: Uuid) -> ChatKeyAddress {
        ChatKeyAddress {
            owner: self.owner.clone(),
            device_id: DEVICE,
            stream_id,
            key_id,
        }
    }
}

impl Drop for Test {
    /// Останавливает локальный HTTPS после сценария.
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Не отправляет приглашения, если вход завершился во время чтения состава.
#[tokio::test]
async fn encrypted_creation_checks_auth_before_invitation_post() {
    let test = Test::new().await;
    let (sync, source, activation) = test.reopen(&test.owner);
    let operation = source
        .stage_encrypted_channel(
            &sync,
            &activation,
            EncryptedChannelInput {
                name: "Защищённый канал".into(),
                description: String::new(),
                members: vec![MEMBER],
            },
        )
        .unwrap();
    test.api.pause_bindings.store(true, Ordering::Relaxed);
    let complete = source.create_encrypted_channel(&sync, &activation, operation);
    let revoke = async {
        tokio::time::timeout(Duration::from_secs(5), test.api.bindings_started.notified())
            .await
            .unwrap();
        test.fixture.auth.logout(&test.owner.account_id).unwrap();
        test.api.bindings_continue.notify_one();
    };
    let (result, ()) = tokio::join!(complete, revoke);
    assert!(matches!(result, Err(KeyLifecycleError::Auth(_))));
    assert_eq!(test.api.creates.lock().unwrap().len(), 1);
    assert!(test.api.invitations.lock().unwrap().is_empty());
}

/// Проверяет потерю POST, точное восстановление и повтор приглашений без второго создания.
#[tokio::test]
async fn encrypted_creation_restores_exact_stream_and_installs_key_before_inviting() {
    let test = Test::new().await;
    let (sync, source, activation) = test.reopen(&test.owner);
    let operation = source
        .stage_encrypted_channel(
            &sync,
            &activation,
            EncryptedChannelInput {
                name: "  Тестовый канал  ".into(),
                description: "описание".into(),
                members: vec![OWNER, MEMBER, MEMBER],
            },
        )
        .unwrap();
    let pending = source
        .pending_key_lifecycle(&sync, &activation)
        .unwrap()
        .pop()
        .unwrap();
    let address = test.address(pending.stream_id, pending.key_id);
    assert!(test.keys.has_staged_lifecycle_key(&address).unwrap());
    assert!(test.keys.load(&address).unwrap().is_none());
    let (other_sync, other_source, other_activation) = test.reopen(&test.other);
    assert!(
        other_source
            .pending_key_lifecycle(&other_sync, &other_activation)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        other_source
            .create_encrypted_channel(&other_sync, &other_activation, operation)
            .await,
        Err(KeyLifecycleError::MissingOperation)
    ));
    drop(other_sync);
    test.api.drop_create.store(true, Ordering::Relaxed);
    assert!(matches!(
        source
            .create_encrypted_channel(&sync, &activation, operation)
            .await,
        Err(KeyLifecycleError::Send(SendError::Unknown))
    ));
    assert_eq!(test.api.creates.lock().unwrap().len(), 1);
    drop(sync);
    let (sync, source, activation) = test.reopen(&test.owner);
    test.api.hide_stream.store(true, Ordering::Relaxed);
    assert!(matches!(
        source
            .create_encrypted_channel(&sync, &activation, operation)
            .await,
        Err(KeyLifecycleError::Uncertain)
    ));
    assert!(test.keys.load(&address).unwrap().is_none());
    test.api.hide_stream.store(false, Ordering::Relaxed);
    test.api.stream.lock().unwrap().as_mut().unwrap()["current_encryption_key_uuid"] = json!(OTHER);
    assert!(matches!(
        source
            .create_encrypted_channel(&sync, &activation, operation)
            .await,
        Err(KeyLifecycleError::Changed)
    ));
    assert!(test.keys.load(&address).unwrap().is_none());
    assert!(test.api.invitations.lock().unwrap().is_empty());
    test.api.stream.lock().unwrap().as_mut().unwrap()["current_encryption_key_uuid"] =
        json!(pending.key_id);
    test.api.drop_invite.store(true, Ordering::Relaxed);
    assert!(matches!(
        source
            .create_encrypted_channel(&sync, &activation, operation)
            .await,
        Err(KeyLifecycleError::Invitations(_))
    ));
    assert!(test.keys.load(&address).unwrap().is_some());
    drop(sync);
    let (sync, source, activation) = test.reopen(&test.owner);
    let result = source
        .create_encrypted_channel(&sync, &activation, operation)
        .await
        .unwrap();
    assert_eq!(
        result,
        CreatedEncryptedChannel {
            stream_id: pending.stream_id,
            topic_id: TOPIC,
            key_id: pending.key_id
        }
    );
    assert_eq!(
        *test.api.invitations.lock().unwrap(),
        [json!({"member": [MEMBER]})]
    );
    assert_eq!(test.api.creates.lock().unwrap().len(), 1);
    assert!(
        source
            .pending_key_lifecycle(&sync, &activation)
            .unwrap()
            .is_empty()
    );
    test.keys.remove(&address).unwrap();
    assert_eq!(
        source
            .create_encrypted_channel(&sync, &activation, operation)
            .await
            .unwrap(),
        result
    );
    assert!(test.keys.load(&address).unwrap().is_none());
}

/// После окончательного отказа удаляет прежнее намерение и разрешает свежие UUID.
#[tokio::test]
async fn encrypted_creation_releases_rejected_operation_after_reopen() {
    let test = Test::new().await;
    let input = EncryptedChannelInput {
        name: "Тест отклонённого создания".into(),
        description: String::new(),
        members: Vec::new(),
    };
    let (sync, source, activation) = test.reopen(&test.owner);
    let rejected_id = source
        .stage_encrypted_channel(&sync, &activation, input.clone())
        .unwrap();
    let rejected = source
        .pending_key_lifecycle(&sync, &activation)
        .unwrap()
        .pop()
        .unwrap();
    let rejected_key = test.address(rejected.stream_id, rejected.key_id);
    assert!(test.keys.has_staged_lifecycle_key(&rejected_key).unwrap());
    test.api.reject_create.store(true, Ordering::Relaxed);
    assert!(matches!(
        source
            .create_encrypted_channel(&sync, &activation, rejected_id)
            .await,
        Err(KeyLifecycleError::CreateRejected(400))
    ));
    assert!(!test.keys.has_staged_lifecycle_key(&rejected_key).unwrap());
    assert!(test.keys.load(&rejected_key).unwrap().is_none());
    assert!(test.api.stream.lock().unwrap().is_none());
    drop(sync);

    let (sync, source, activation) = test.reopen(&test.owner);
    assert!(
        source
            .pending_key_lifecycle(&sync, &activation)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        source
            .create_encrypted_channel(&sync, &activation, rejected_id)
            .await,
        Err(KeyLifecycleError::MissingOperation)
    ));
    assert_eq!(test.api.creates.lock().unwrap().len(), 1);
    let next = source
        .stage_encrypted_channel(&sync, &activation, input)
        .unwrap();
    let created = source
        .create_encrypted_channel(&sync, &activation, next)
        .await
        .unwrap();
    assert_ne!(next, rejected_id);
    assert_ne!(created.stream_id, rejected.stream_id);
    assert_ne!(created.key_id, rejected.key_id);
    assert_eq!(test.api.creates.lock().unwrap().len(), 2);
}

/// Проверяет owner-only, повтор того же объявления и сохранение прежнего поколения.
#[tokio::test]
async fn rotation_retries_same_announcement_and_recovers_receipt_without_deleting_old_key() {
    let test = Test::new().await;
    let (sync, source, activation) = test.reopen(&test.owner);
    let creation = source
        .stage_encrypted_channel(
            &sync,
            &activation,
            EncryptedChannelInput {
                name: "Ротация".into(),
                description: String::new(),
                members: Vec::new(),
            },
        )
        .unwrap();
    let created = source
        .create_encrypted_channel(&sync, &activation, creation)
        .await
        .unwrap();
    let scope = HistoryScope {
        stream_id: created.stream_id,
        topic_id: Some(TOPIC),
    };
    let (other_sync, other_source, other_activation) = test.reopen(&test.other);
    let local = source
        .local_stream_keys(&sync, &activation, scope)
        .await
        .unwrap();
    assert!(local.can_rotate);
    assert_eq!(local.topic_id, TOPIC);
    assert_eq!(local.current_key_id, created.key_id);
    assert_eq!(local.keys.len(), 1);
    assert_eq!(local.keys[0].key_id, created.key_id);
    assert_eq!(local.keys[0].added_at_unix_ms, Some(19));
    assert!(
        other_source
            .local_stream_keys(&other_sync, &other_activation, scope)
            .await
            .is_err()
    );
    test.api.members.lock().unwrap().insert(OTHER);
    let member = other_source
        .local_stream_keys(&other_sync, &other_activation, scope)
        .await
        .unwrap();
    assert!(!member.can_rotate);
    assert_eq!(member.current_key_id, created.key_id);
    assert!(member.keys.is_empty());
    test.api.pause_bindings.store(true, Ordering::Relaxed);
    let pending_list = other_source.local_stream_keys(&other_sync, &other_activation, scope);
    let revoke = async {
        tokio::time::timeout(Duration::from_secs(5), test.api.bindings_started.notified())
            .await
            .unwrap();
        other_sync
            .apply(
                &test.other,
                other_activation.session().generation,
                SOURCE_ID,
                Event::HistoryChanged {
                    cursor: Cursor {
                        epoch_generation: "test".into(),
                        epoch_version: 1,
                    },
                    change: HistoryChange::RemoveStream(scope.stream_id.to_string()),
                },
            )
            .unwrap();
        test.api.bindings_continue.notify_one();
    };
    let (stale, ()) = tokio::join!(pending_list, revoke);
    assert!(matches!(
        stale,
        Err(KeyLifecycleError::History(OpenHistoryError::AccessLost))
    ));
    assert!(matches!(
        other_source
            .stage_key_rotation(&other_sync, &other_activation, scope)
            .await,
        Err(KeyLifecycleError::NotOwner)
    ));
    drop(other_sync);

    let operation = source
        .stage_key_rotation(&sync, &activation, scope)
        .await
        .unwrap();
    let pending = source
        .pending_key_lifecycle(&sync, &activation)
        .unwrap()
        .pop()
        .unwrap();
    let old = test.address(created.stream_id, created.key_id);
    let new = test.address(created.stream_id, pending.key_id);
    let staged_bytes = *test
        .keys
        .0
        .lock()
        .unwrap()
        .get(&new)
        .unwrap()
        .key
        .as_bytes();
    assert!(test.keys.load(&old).unwrap().is_some());
    assert!(test.keys.load(&new).unwrap().is_none());
    test.api.rotate_uncertain.store(true, Ordering::Relaxed);
    assert!(matches!(
        source
            .rotate_encrypted_key(&sync, &activation, operation)
            .await,
        Err(KeyLifecycleError::Send(SendError::Unknown))
    ));
    test.api.stream.lock().unwrap().as_mut().unwrap()["current_encryption_key_uuid"] = json!(OTHER);
    assert!(matches!(
        source
            .rotate_encrypted_key(&sync, &activation, operation)
            .await,
        Err(KeyLifecycleError::Changed)
    ));
    assert_eq!(test.api.announces.lock().unwrap().len(), 1);
    assert!(test.keys.load(&new).unwrap().is_none());
    test.api.stream.lock().unwrap().as_mut().unwrap()["current_encryption_key_uuid"] =
        json!(created.key_id);
    assert!(matches!(
        source
            .rotate_encrypted_key(&sync, &activation, operation)
            .await,
        Err(KeyLifecycleError::Send(SendError::Unknown))
    ));
    let requests = test.api.announces.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0]["uuid"], json!(operation));
    assert_eq!(requests[0]["payload"]["key_uuid"], json!(pending.key_id));
    assert!(test.keys.load(&new).unwrap().is_none());
    drop(sync);
    let (sync, source, activation) = test.reopen(&test.owner);
    let result = source
        .rotate_encrypted_key(&sync, &activation, operation)
        .await
        .unwrap();
    assert_eq!(
        (result.key_id, result.current_key_id),
        (pending.key_id, pending.key_id)
    );
    assert_eq!(test.api.announces.lock().unwrap().len(), 2);
    assert!(test.keys.load(&new).unwrap().is_some());
    assert_eq!(
        test.keys.load(&new).unwrap().unwrap().as_bytes(),
        &staged_bytes
    );
    assert!(test.keys.load(&old).unwrap().is_some());
    assert!(
        source
            .pending_key_lifecycle(&sync, &activation)
            .unwrap()
            .is_empty()
    );
}
