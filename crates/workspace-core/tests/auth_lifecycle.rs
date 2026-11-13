//! Проверяет жизненный цикл авторизации и фоновой синхронизации без настоящего сервера.
//! Сначала идут подмены хранилищ, часов и IAM, затем сценарии входа, обновления токенов и чтения событий.

use std::collections::HashMap;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::Context;
use std::thread;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::{StreamExt, executor::block_on, task::noop_waker_ref};
use tokio::sync::{mpsc as async_mpsc, oneshot};
use workspace_core::auth::{
    AccountRecord, AccountStore, Auth, AuthError, Clock, IamClient, IamError, IamProject, IamToken,
    SecretStore, Service, SessionStatus, StoreError,
};
use workspace_core::context::{AccountId, OwnerContext};
use workspace_core::notification_target::{NotificationTarget, NotificationTargetError};
use workspace_core::runtime::{EventSource, EventStream, Runtime, SourceError, SourceSession};
use workspace_core::storage::{Cursor, Message, Storage};
use workspace_core::sync::{Event, Sync as EventSync, SyncChange, SyncUpdate};

const SERVER: &str = "https://workspace.test";

/// Хранилища аккаунтов и секретов в памяти с управляемыми сбоями.
#[derive(Default)]
struct MemoryStores {
    accounts: Mutex<HashMap<AccountId, AccountRecord>>, // Записи аккаунтов по локальному идентификатору.
    secrets: Mutex<HashMap<String, String>>,            // Токены по ключам защищённого хранилища.
    fail_account_save: AtomicBool, // Признак, что запись аккаунта должна завершиться ошибкой.
    fail_secret_save: AtomicBool,  // Признак, что запись секрета должна завершиться ошибкой.
    fail_secret_remove: AtomicBool, // Признак, что удаление секрета должно завершиться ошибкой.
}

impl AccountStore for MemoryStores {
    /// Возвращает копии всех сохранённых тестовых аккаунтов.
    fn load(&self) -> Result<Vec<AccountRecord>, StoreError> {
        Ok(self.accounts.lock().unwrap().values().cloned().collect())
    }

    /// Сохраняет тестовый аккаунт или имитирует ошибку записи.
    fn save(&self, record: &AccountRecord) -> Result<(), StoreError> {
        if self.fail_account_save.swap(false, Ordering::SeqCst) {
            return Err(StoreError);
        }
        self.accounts
            .lock()
            .unwrap()
            .insert(record.context.account_id.clone(), record.clone());
        Ok(())
    }

    /// Удаляет тестовый аккаунт по идентификатору.
    fn remove(&self, account_id: &AccountId) -> Result<(), StoreError> {
        self.accounts.lock().unwrap().remove(account_id);
        Ok(())
    }
}

impl SecretStore for MemoryStores {
    /// Возвращает копию секрета из памяти, если ключ существует.
    fn load(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.secrets.lock().unwrap().get(key).cloned())
    }

    /// Сохраняет секрет в памяти или имитирует ошибку записи.
    fn save(&self, key: &str, token: &str) -> Result<(), StoreError> {
        if self.fail_secret_save.swap(false, Ordering::SeqCst) {
            return Err(StoreError);
        }
        self.secrets
            .lock()
            .unwrap()
            .insert(key.to_owned(), token.to_owned());
        Ok(())
    }

    /// Удаляет секрет из памяти или имитирует ошибку очистки.
    fn remove(&self, key: &str) -> Result<(), StoreError> {
        if self.fail_secret_remove.swap(false, Ordering::SeqCst) {
            return Err(StoreError);
        }
        self.secrets.lock().unwrap().remove(key);
        Ok(())
    }
}

/// Часы, которые тест может двигать без ожидания.
struct TestClock(Mutex<Instant>); // Текущий момент тестовых часов.

impl TestClock {
    /// Переводит тестовые часы вперёд на заданное число секунд.
    fn advance(&self, seconds: u64) {
        *self.0.lock().unwrap() += Duration::from_secs(seconds);
    }
}

impl Clock for TestClock {
    /// Возвращает текущий момент тестовых часов.
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

/// Правило ответа тестового IAM по токену обновления и номеру вызова.
type RefreshResponse = dyn Fn(&str, usize) -> Result<IamToken, IamError> + Send + Sync;

/// Подмена IAM с предсказуемым входом и настраиваемым обновлением.
struct TestIam {
    refreshes: AtomicUsize,         // Счётчик запросов на обновление.
    response: Box<RefreshResponse>, // Функция, которая задаёт ответ на обновление.
}

impl IamClient for TestIam {
    /// Использует переданный адрес как есть для тестового сервера.
    fn normalize_server(&self, server: &str) -> Result<String, IamError> {
        Ok(server.to_owned())
    }

    /// Возвращает заготовленный токен для указанного пользователя.
    fn login(
        &self,
        _server: &str,
        user: &str,
        _password: &str,
        _otp: Option<&str>,
    ) -> Result<IamToken, IamError> {
        Ok(IamToken {
            access_token: user.to_owned(),
            refresh_token: Some(user.to_owned()),
            expires_in: Duration::from_secs(120),
            user_id: user.to_owned(),
            project_id: None,
            organization_id: None,
        })
    }

    /// Возвращает список проектов, доступных в тесте.
    fn projects(&self, _server: &str, _token: &str) -> Result<Vec<IamProject>, IamError> {
        Ok(vec![IamProject {
            id: "project".into(),
            name: "Project".into(),
            organization_id: Some("org".into()),
        }])
    }

    /// Выдаёт заготовленный токен с выбранным проектом.
    fn select_project(
        &self,
        _server: &str,
        token: &str,
        project: &str,
    ) -> Result<IamToken, IamError> {
        let mut issued = project_token(token, "sign-in", None);
        issued.project_id = Some(project.into());
        Ok(issued)
    }

    /// Считает вызов и возвращает ответ из сценария теста.
    fn refresh(&self, _server: &str, token: &str) -> Result<IamToken, IamError> {
        let attempt = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        (self.response)(token, attempt)
    }
}

/// Создаёт тестовый токен с указанным владельцем и секретами.
fn project_token(user: &str, access: &str, refresh: Option<&str>) -> IamToken {
    IamToken {
        access_token: access.into(),
        refresh_token: refresh.map(str::to_owned),
        expires_in: Duration::from_secs(120),
        user_id: user.into(),
        project_id: Some("project".into()),
        organization_id: Some("org".into()),
    }
}

/// Готовый набор зависимостей и авторизации для одного теста.
struct Fixture {
    iam: Arc<TestIam>,         // Подмена сервера, которая считает обновления.
    stores: Arc<MemoryStores>, // Хранилища аккаунтов и секретов в памяти.
    clock: Arc<TestClock>,     // Часы, которыми управляет тест.
    auth: Auth,                // Авторизация с тестовыми зависимостями.
}

impl Fixture {
    /// Создаёт окружение с заданным ответом на запрос обновления.
    fn new(
        response: impl Fn(&str, usize) -> Result<IamToken, IamError> + Send + Sync + 'static,
    ) -> Self {
        let iam = Arc::new(TestIam {
            refreshes: AtomicUsize::new(0),
            response: Box::new(response),
        });
        let stores = Arc::new(MemoryStores::default());
        let clock = Arc::new(TestClock(Mutex::new(Instant::now())));
        let auth = Auth::with_clock(iam.clone(), stores.clone(), stores.clone(), clock.clone());
        Self {
            iam,
            stores,
            clock,
            auth,
        }
    }

    /// Имитирует перезапуск авторизации с теми же сохранёнными данными.
    fn restart(&self) -> Auth {
        Auth::with_clock(
            self.iam.clone(),
            self.stores.clone(),
            self.stores.clone(),
            self.clock.clone(),
        )
    }

    /// Проходит оба шага входа и возвращает контекст тестового пользователя.
    fn login(&self, user: &str) -> OwnerContext {
        let attempt = self
            .auth
            .begin_login(SERVER, user, "password", None)
            .unwrap();
        self.auth.complete_login(attempt.id, "project").unwrap()
    }
}

/// Проверяет отказ при неизвестном проекте, отмене и истечении попытки.
#[test]
fn project_selection_rejects_unknown_cancelled_and_expired_attempts() {
    let fixture = Fixture::new(|_, _| unreachable!());
    let first = fixture
        .auth
        .begin_login(SERVER, "a", "password", None)
        .unwrap();
    assert_eq!(
        fixture.auth.complete_login(first.id, "another project"),
        Err(AuthError::UnknownProject)
    );
    fixture.auth.cancel_login(first.id);
    assert_eq!(
        fixture.auth.complete_login(first.id, "project"),
        Err(AuthError::AttemptUnavailable)
    );
    let second = fixture
        .auth
        .begin_login(SERVER, "a", "password", None)
        .unwrap();
    fixture.clock.advance(301);
    assert_eq!(
        fixture.auth.complete_login(second.id, "project"),
        Err(AuthError::AttemptUnavailable)
    );
    assert!(fixture.auth.snapshot().is_empty());
    assert!(fixture.stores.secrets.lock().unwrap().is_empty());
}

/// Проверяет, что сбой восстановления одного аккаунта не мешает другому и что отозванный токен требует входа.
#[test]
fn restore_is_independent_and_detects_rejected_credentials() {
    let fixture = Fixture::new(|user, _| {
        if user == "b" {
            Err(IamError::ReauthenticationRequired)
        } else {
            Ok(project_token(user, "restored", None))
        }
    });
    let a = fixture.login("a");
    let b = fixture.login("b");

    let restarted = fixture.restart();
    block_on(restarted.restore()).unwrap();
    assert_eq!(
        block_on(restarted.access_token(&a, Service::Messenger)).unwrap(),
        "restored"
    );
    assert_eq!(
        block_on(restarted.access_token(&b, Service::Messenger)),
        Err(AuthError::ReauthenticationRequired)
    );
    assert!(restarted.snapshot().into_iter().any(|session| {
        session.context == b && session.status == SessionStatus::ReauthenticationRequired
    }));
    let mut wrong_context = a;
    wrong_context.project_id = "tampered".into();
    assert_eq!(
        block_on(restarted.access_token(&wrong_context, Service::Messenger)),
        Err(AuthError::SessionUnavailable)
    );
}

/// Проверяет, что параллельные запросы делят обновление и сохраняют замену токена один раз даже при отмене ожидания.
#[test]
fn concurrent_refresh_survives_cancellation_and_persists_rotation_once() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let fixture = Fixture::new(move |user, _| {
        started_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        Ok(project_token(user, "new-access", Some("new-refresh")))
    });
    let owner = fixture.login("a");
    fixture.clock.advance(110);

    let mut task_cx = Context::from_waker(noop_waker_ref());
    {
        let mut cancelled = pin!(fixture.auth.access_token(&owner, Service::Messenger));
        assert!(cancelled.as_mut().poll(&mut task_cx).is_pending());
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    let mut retained = pin!(fixture.auth.access_token(&owner, Service::Messenger));
    assert!(retained.as_mut().poll(&mut task_cx).is_pending());
    release_tx.send(()).unwrap();
    assert_eq!(block_on(retained).unwrap(), "new-access");
    assert_eq!(fixture.iam.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .stores
            .secrets
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap(),
        "new-refresh"
    );
    assert_eq!(fixture.auth.snapshot()[0].token_version, 2);
}

/// Проверяет повтор записи нового секрета без второго запроса к IAM.
#[test]
fn failed_rotation_save_retries_storage_without_replaying_network() {
    let fixture = Fixture::new(|user, _| Ok(project_token(user, "next", Some("rotated"))));
    let owner = fixture.login("a");
    fixture.clock.advance(110);
    fixture
        .stores
        .fail_secret_save
        .store(true, Ordering::SeqCst);

    assert_eq!(
        block_on(fixture.auth.access_token(&owner, Service::Messenger)).unwrap(),
        "sign-in"
    );
    assert_eq!(fixture.auth.snapshot()[0].token_version, 1);
    fixture.clock.advance(3);
    assert_eq!(
        block_on(fixture.auth.access_token(&owner, Service::Messenger)).unwrap(),
        "next"
    );
    assert_eq!(fixture.iam.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.auth.snapshot()[0].token_version, 2);
}

/// Проверяет, что сбой записи аккаунта сохраняет прежнюю сессию и секрет.
#[test]
fn failed_account_save_preserves_previous_session_and_secret() {
    let fixture = Fixture::new(|_, _| unreachable!());
    let account = fixture.login("a");
    let prior_sessions = fixture.auth.snapshot();
    let prior_secrets = fixture.stores.secrets.lock().unwrap().clone();
    let attempt = fixture
        .auth
        .begin_login(SERVER, "a", "password", None)
        .unwrap();
    fixture
        .stores
        .fail_account_save
        .store(true, Ordering::SeqCst);
    assert_eq!(
        fixture.auth.complete_login(attempt.id, "project"),
        Err(AuthError::Storage(StoreError))
    );
    assert_eq!(fixture.auth.snapshot(), prior_sessions);
    assert_eq!(*fixture.stores.secrets.lock().unwrap(), prior_secrets);
    assert!(block_on(fixture.auth.access_token(&account, Service::Messenger)).is_ok());
}

/// Проверяет продолжение очистки после перезапуска без восстановления сессии.
#[test]
fn failed_logout_cleanup_survives_restart_without_reauthenticating() {
    let fixture = Fixture::new(|_, _| unreachable!());
    let b = fixture.login("b");
    let new_process = fixture.restart();
    fixture
        .stores
        .fail_secret_remove
        .store(true, Ordering::SeqCst);
    assert_eq!(
        new_process.logout(&b.account_id),
        Err(AuthError::LogoutPending)
    );
    assert!(AccountStore::load(&*fixture.stores).unwrap()[0].signed_out);

    let restarted = fixture.restart();
    block_on(restarted.restore()).unwrap();
    assert!(restarted.snapshot().is_empty());
    assert!(fixture.stores.secrets.lock().unwrap().is_empty());
    assert!(fixture.stores.accounts.lock().unwrap().is_empty());
}

/// Проверяет, что запоздавший ответ не перезаписывает новый вход и не затрагивает другой аккаунт.
#[test]
fn a_late_refresh_cannot_replace_relogin_or_affect_another_account() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let fixture = Fixture::new(move |user, _| {
        if user == "b" {
            started_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        Ok(project_token(user, "new-access", Some("new-refresh")))
    });
    let a = fixture.login("a");
    let b = fixture.login("b");
    fixture.clock.advance(110);

    let auth = fixture.auth.clone();
    let old_owner = b.clone();
    let request =
        thread::spawn(move || block_on(auth.access_token(&old_owner, Service::Messenger)));
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    fixture.auth.logout(&b.account_id).unwrap();
    assert_eq!(fixture.login("b"), b);
    let new_generation = fixture
        .auth
        .snapshot()
        .into_iter()
        .find(|session| session.context == b)
        .unwrap()
        .generation;
    release_tx.send(()).unwrap();

    assert_eq!(request.join().unwrap(), Err(AuthError::SessionUnavailable));
    assert_eq!(
        block_on(fixture.auth.access_token(&a, Service::Messenger)).unwrap(),
        "new-access"
    );
    assert_eq!(
        block_on(fixture.auth.access_token(&b, Service::Messenger)).unwrap(),
        "sign-in"
    );
    let b_session = fixture
        .auth
        .snapshot()
        .into_iter()
        .find(|session| session.context == b)
        .unwrap();
    assert_eq!(b_session.generation, new_generation);
    assert_eq!(b_session.token_version, 1);
}

/// Проверяет использование ещё действующего токена при сбое и завершение сессии при смене владельца.
#[test]
fn temporary_failure_keeps_valid_token_but_owner_mismatch_ends_session() {
    let fixture = Fixture::new(|_, attempt| {
        if attempt == 1 {
            Err(IamError::Temporary)
        } else {
            Ok(project_token("wrong-user", "bad-access", None))
        }
    });
    let owner = fixture.login("a");
    fixture.clock.advance(110);
    assert_eq!(
        block_on(fixture.auth.access_token(&owner, Service::Messenger)).unwrap(),
        "sign-in"
    );
    fixture.clock.advance(3);
    assert_eq!(
        block_on(fixture.auth.access_token(&owner, Service::Messenger)),
        Err(AuthError::ReauthenticationRequired)
    );
    assert_eq!(
        fixture.auth.snapshot()[0].status,
        SessionStatus::ReauthenticationRequired
    );
}

/// Проверяет, что медленный подписчик получает последнее состояние без секретов.
#[test]
fn slow_subscriber_receives_latest_snapshot_without_secrets() {
    let fixture = Fixture::new(|_, _| unreachable!());
    let mut changes = fixture.auth.subscribe();
    assert!(block_on(changes.next()).unwrap().is_empty());
    let a = fixture.login("a");
    let b = fixture.login("b");
    fixture.auth.logout(&a.account_id).unwrap();
    let latest = block_on(changes.next()).unwrap();
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].context, b);
}

/// Подключение для теста. Первый элемент — параметры сессии, второй — канал подачи событий.
type TestConnection = (
    SourceSession, // Параметры, с которыми источник открыли.
    async_mpsc::Sender<Result<Event, SourceError>>, // Канал для передачи событий или ошибок.
);

/// Подмена источника событий, сообщающая тесту о каждом подключении.
struct TestSource(async_mpsc::UnboundedSender<TestConnection>); // Канал уведомлений о новых подключениях.

impl EventSource for TestSource {
    /// Возвращает имя тестового источника мессенджера.
    fn id(&self) -> &str {
        "messenger"
    }

    /// Передаёт тесту параметры подключения и возвращает поток событий из канала.
    fn connect(&self, session: SourceSession) -> BoxFuture<'_, Result<EventStream, SourceError>> {
        Box::pin(async move {
            let (send, receive) = async_mpsc::channel(4);
            self.0
                .send((session, send))
                .map_err(|_| SourceError::Temporary)?;
            let stream = futures::stream::unfold(receive, |mut receive| async move {
                receive.recv().await.map(|event| (event, receive))
            });
            Ok(Box::pin(stream) as EventStream)
        })
    }
}

/// Создаёт тестовое событие с общим идентификатором сообщения.
fn incoming(version: u64, text: &str) -> Event {
    incoming_with_id(version, "shared-message-id", text)
}

/// Создаёт событие с заданной позицией, идентификатором и текстом сообщения.
fn incoming_with_id(version: u64, id: &str, text: &str) -> Event {
    Event::MessageCreated {
        cursor: Cursor {
            epoch_generation: "test-epoch".into(),
            epoch_version: version,
        },
        message: Message {
            id: id.into(),
            text: text.into(),
        },
        placement: None,
        metadata: None,
    }
}

/// Проверяет восстановление счётчика, порядок снимков, повторы сообщений и выход из сессии.
#[test]
fn messenger_updates_are_committed_deduplicated_and_scoped_to_the_session() {
    let fixture = Fixture::new(|_, _| unreachable!());
    let first = fixture.login("first");
    let second = fixture.login("second");
    let generation = |owner: &OwnerContext| {
        fixture
            .auth
            .snapshot()
            .into_iter()
            .find(|s| s.context == *owner)
            .unwrap()
            .generation
    };
    let first_generation = generation(&first);
    let second_generation = generation(&second);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sync.sqlite3");
    let storage = Arc::new(Storage::open(&path).unwrap());
    let sync = EventSync::new(fixture.auth.clone(), storage.clone());
    let mut updates = sync.subscribe();
    let cursor = |version| Cursor {
        epoch_generation: "test-epoch".into(),
        epoch_version: version,
    };

    // Существующая база уже содержит курсор, но ещё не содержит счётчики.
    sync.apply(
        &first,
        first_generation,
        "messenger",
        Event::Checkpoint(cursor(10)),
    )
    .unwrap();
    sync.apply(
        &first,
        first_generation,
        "messenger",
        Event::UnreadBaseline {
            cursor: cursor(10),
            count: 4,
        },
    )
    .unwrap();
    sync.apply(
        &second,
        second_generation,
        "messenger",
        Event::UnreadBaseline {
            cursor: cursor(5),
            count: 29,
        },
    )
    .unwrap();
    assert_eq!(
        updates.try_recv().unwrap(),
        SyncUpdate {
            context: first.clone(),
            generation: first_generation,
            change: SyncChange::Unread(4)
        }
    );
    assert_eq!(
        updates.try_recv().unwrap(),
        SyncUpdate {
            context: second.clone(),
            generation: second_generation,
            change: SyncChange::Unread(29)
        }
    );
    assert_eq!(sync.cursor(&first, "messenger").unwrap(), Some(cursor(10)));
    assert_eq!(sync.cursor(&second, "messenger").unwrap(), None);

    sync.apply(
        &second,
        second_generation,
        "messenger",
        Event::UnreadChanged {
            cursor: cursor(5),
            count: 28,
        },
    )
    .unwrap();
    assert_eq!(sync.cursor(&second, "messenger").unwrap(), Some(cursor(5)));
    assert_eq!(sync.unread(&second).unwrap(), Some(29));
    assert!(updates.try_recv().is_err());

    let live = |version| Event::IncomingDirectMessage {
        cursor: cursor(version),
        message: Message {
            id: "direct-message".into(),
            text: "hello".into(),
        },
        conversation_id: "conversation".into(),
        placement: None,
        metadata: None,
    };
    sync.apply(&second, second_generation, "messenger", live(6))
        .unwrap();
    assert_eq!(
        updates.try_recv().unwrap(),
        SyncUpdate {
            context: second.clone(),
            generation: second_generation,
            change: SyncChange::IncomingDirectMessage {
                conversation_id: "conversation".into(),
                message_id: "direct-message".into()
            },
        }
    );
    assert!(
        storage
            .message(&second, "direct-message")
            .unwrap()
            .is_some()
    );
    sync.apply(&second, second_generation, "messenger", live(6))
        .unwrap();
    sync.apply(&second, second_generation, "messenger", live(7))
        .unwrap();
    assert!(updates.try_recv().is_err());

    sync.apply(
        &second,
        second_generation,
        "messenger",
        Event::UnreadChanged {
            cursor: cursor(8),
            count: 30,
        },
    )
    .unwrap();
    assert_eq!(
        updates.try_recv().unwrap(),
        SyncUpdate {
            context: second.clone(),
            generation: second_generation,
            change: SyncChange::Unread(30)
        }
    );
    assert_eq!(sync.cursor(&second, "messenger").unwrap(), Some(cursor(8)));
    let reopened = Storage::open(&path).unwrap();
    assert_eq!(reopened.unread(&first).unwrap(), Some(4));
    assert_eq!(reopened.unread(&second).unwrap(), Some(30));

    fixture.auth.logout(&second.account_id).unwrap();
    assert!(
        sync.apply(
            &second,
            second_generation,
            "messenger",
            Event::UnreadChanged {
                cursor: cursor(9),
                count: 31
            }
        )
        .is_err()
    );
    assert!(updates.try_recv().is_err());
    assert_eq!(reopened.unread(&second).unwrap(), Some(30));
}

/// Проверяет, что клик открывает только сохранённую цель в исходной действующей сессии.
#[test]
fn notification_target_requires_matching_session_conversation_and_message() {
    let fixture = Fixture::new(|_, _| unreachable!());
    let first = fixture.login("first");
    let second = fixture.login("second");
    let generation = |owner: &OwnerContext| {
        fixture
            .auth
            .snapshot()
            .into_iter()
            .find(|session| session.context == *owner)
            .unwrap()
            .generation
    };
    let second_generation = generation(&second);
    let sync = EventSync::new(
        fixture.auth.clone(),
        Arc::new(Storage::in_memory().unwrap()),
    );
    let cursor = Cursor {
        epoch_generation: "test-epoch".into(),
        epoch_version: 1,
    };
    let message = Message {
        id: "direct-message".into(),
        text: "hello".into(),
    };
    // То же сообщение видно обоим участникам, но уведомление получил только второй.
    sync.apply(
        &first,
        generation(&first),
        "messenger",
        Event::MessageCreated {
            cursor: cursor.clone(),
            message: message.clone(),
            placement: None,
            metadata: None,
        },
    )
    .unwrap();
    sync.apply(
        &second,
        second_generation,
        "messenger",
        Event::IncomingDirectMessage {
            cursor,
            message,
            conversation_id: "direct-conversation".into(),
            placement: None,
            metadata: None,
        },
    )
    .unwrap();

    let target = NotificationTarget {
        context: second.clone(),
        conversation_id: "direct-conversation".into(),
        message_id: "direct-message".into(),
    };
    assert_eq!(
        sync.resolve_notification_target(target.clone(), second_generation)
            .unwrap(),
        target
    );
    let wrong_account = NotificationTarget {
        context: first.clone(),
        ..target.clone()
    };
    assert!(matches!(
        sync.resolve_notification_target(wrong_account, generation(&first)),
        Err(NotificationTargetError::UnknownTarget)
    ));
    let wrong_project = NotificationTarget {
        context: OwnerContext {
            project_id: "other-project".into(),
            ..second.clone()
        },
        ..target.clone()
    };
    assert!(matches!(
        sync.resolve_notification_target(wrong_project, second_generation),
        Err(NotificationTargetError::Auth(AuthError::SessionUnavailable))
    ));
    for wrong_target in [
        NotificationTarget {
            conversation_id: "other-conversation".into(),
            ..target.clone()
        },
        NotificationTarget {
            message_id: "other-message".into(),
            ..target.clone()
        },
    ] {
        assert!(matches!(
            sync.resolve_notification_target(wrong_target, second_generation),
            Err(NotificationTargetError::UnknownTarget)
        ));
    }

    fixture.auth.logout(&second.account_id).unwrap();
    assert_eq!(fixture.login("second"), second);
    assert_ne!(generation(&second), second_generation);
    assert!(matches!(
        sync.resolve_notification_target(target, second_generation),
        Err(NotificationTargetError::Auth(AuthError::SessionUnavailable))
    ));
}

/// Ждёт, пока условие выполнится, и ограничивает время ожидания.
async fn wait_for(test: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !test() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition did not become true");
}

/// Проверяет отдельные потоки двух аккаунтов и возобновление с последней записанной позиции.
#[tokio::test]
async fn runtime_tracks_two_accounts_and_resumes_from_the_committed_cursor() {
    let fixture = Fixture::new(|user, _| Ok(project_token(user, "refreshed", None)));
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("sync.sqlite3");
    let storage = Arc::new(Storage::open(&db_path).unwrap());
    let sync = Arc::new(EventSync::new(fixture.auth.clone(), storage.clone()));
    let (connections, mut receive) = async_mpsc::unbounded_channel();
    let runtime = Runtime::new(sync.clone(), vec![Arc::new(TestSource(connections))]).unwrap();
    let (shutdown, stopped) = oneshot::channel();
    let worker = tokio::spawn(runtime.run(async {
        let _ = stopped.await;
    }));

    let a = fixture.login("a");
    let b = fixture.login("b");
    let mut connected = HashMap::new();
    for _ in 0..2 {
        let (session, send) = tokio::time::timeout(Duration::from_secs(5), receive.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.access_token, "sign-in");
        assert!(session.cursor.is_none());
        assert!(
            connected
                .insert(session.context.account_id, (session.generation, send))
                .is_none()
        );
    }
    let (generation_a, send_a) = connected.remove(&a.account_id).unwrap();
    let (generation_b, send_b) = connected.remove(&b.account_id).unwrap();
    let event_a = incoming(4, "for A");
    let event_b = incoming(7, "for B");
    send_a.send(Ok(event_a.clone())).await.unwrap();
    send_b.send(Ok(event_b.clone())).await.unwrap();
    wait_for(|| {
        sync.cursor(&a, "messenger").unwrap().as_ref() == Some(event_a.cursor())
            && sync.cursor(&b, "messenger").unwrap().as_ref() == Some(event_b.cursor())
    })
    .await;
    assert_eq!(
        storage.message(&a, &event_a.message().unwrap().id).unwrap(),
        Some(event_a.message().unwrap().clone())
    );
    assert_eq!(
        storage.message(&b, &event_b.message().unwrap().id).unwrap(),
        Some(event_b.message().unwrap().clone())
    );

    let next_a = incoming_with_id(5, "next-message-id", "next A");
    let fault = rusqlite::Connection::open(&db_path).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER fail_cursor BEFORE UPDATE ON sync_cursors
         WHEN NEW.epoch_version = 5
         BEGIN SELECT RAISE(ABORT, 'test cursor write failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        sync.apply(&a, generation_a, "messenger", next_a.clone()),
        Err(workspace_core::sync::SyncError::Storage(_)),
    ));
    assert_eq!(
        storage.message(&a, &next_a.message().unwrap().id).unwrap(),
        None
    );
    assert_eq!(
        sync.cursor(&a, "messenger").unwrap().as_ref(),
        Some(event_a.cursor())
    );
    assert_eq!(
        sync.cursor(&b, "messenger").unwrap().as_ref(),
        Some(event_b.cursor())
    );
    fault.execute_batch("DROP TRIGGER fail_cursor").unwrap();

    fixture.clock.advance(110);
    assert_eq!(
        fixture
            .auth
            .access_token(&a, Service::Messenger)
            .await
            .unwrap(),
        "refreshed"
    );
    let (reconnected, send_a_new) = tokio::time::timeout(Duration::from_secs(5), receive.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reconnected.context, a);
    assert_eq!(reconnected.generation, generation_a);
    assert_eq!(reconnected.cursor.as_ref(), Some(event_a.cursor()));
    assert_eq!(reconnected.access_token, "refreshed");
    assert!(send_a.is_closed());
    assert!(!send_b.is_closed());

    fixture.auth.logout(&b.account_id).unwrap();
    tokio::time::timeout(Duration::from_secs(5), send_b.closed())
        .await
        .unwrap();
    assert!(!send_a_new.is_closed());

    assert_eq!(fixture.login("b"), b);
    let (restored_b, send_b_new) = tokio::time::timeout(Duration::from_secs(5), receive.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored_b.context, b);
    assert_ne!(restored_b.generation, generation_b);
    assert_eq!(restored_b.cursor.as_ref(), Some(event_b.cursor()));

    let stale_b = incoming_with_id(8, "stale-message-id", "stale B");
    assert!(matches!(
        sync.apply(&b, generation_b, "messenger", stale_b.clone()),
        Err(workspace_core::sync::SyncError::Auth(
            AuthError::SessionUnavailable
        )),
    ));
    send_a_new.send(Ok(next_a.clone())).await.unwrap();
    wait_for(|| sync.cursor(&a, "messenger").unwrap().as_ref() == Some(next_a.cursor())).await;

    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(send_a_new.is_closed());
    assert!(send_b_new.is_closed());
    let reopened = Storage::open(&db_path).unwrap();
    assert_eq!(
        reopened.message(&a, &next_a.message().unwrap().id).unwrap(),
        Some(next_a.message().unwrap().clone())
    );
    assert_eq!(
        reopened
            .message(&b, &event_b.message().unwrap().id)
            .unwrap(),
        Some(event_b.message().unwrap().clone())
    );
    assert_eq!(
        reopened.cursor(&a, "messenger").unwrap(),
        Some(next_a.cursor().clone())
    );
    assert_eq!(
        reopened.cursor(&b, "messenger").unwrap(),
        Some(event_b.cursor().clone())
    );
    assert_eq!(
        reopened
            .message(&b, &stale_b.message().unwrap().id)
            .unwrap(),
        None
    );
}

/// Проверяет, что чтение возобновляется после временного сбоя IAM при переподключении.
#[tokio::test]
async fn runtime_recovers_after_temporary_iam_failure_on_reconnect() {
    let fixture = Fixture::new(|user, attempt| {
        if attempt == 1 {
            Err(IamError::Temporary)
        } else {
            Ok(project_token(user, "recovered", None))
        }
    });
    let owner = fixture.login("a");
    let storage = Arc::new(Storage::in_memory().unwrap());
    let sync = Arc::new(EventSync::new(fixture.auth.clone(), storage));
    let (connections, mut receive) = async_mpsc::unbounded_channel();
    let runtime = Runtime::new(sync, vec![Arc::new(TestSource(connections))]).unwrap();
    let (shutdown, stopped) = oneshot::channel();
    let worker = tokio::spawn(runtime.run(async {
        let _ = stopped.await;
    }));

    let (initial, stream) = tokio::time::timeout(Duration::from_secs(5), receive.recv())
        .await
        .unwrap()
        .unwrap();
    fixture.clock.advance(121);
    drop(stream);
    wait_for(|| fixture.auth.snapshot()[0].status == SessionStatus::TemporarilyUnavailable).await;
    fixture.clock.advance(3);

    let (reconnected, _stream) = tokio::time::timeout(Duration::from_secs(5), receive.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reconnected.context, owner);
    assert_eq!(reconnected.generation, initial.generation);
    assert!(reconnected.token_version > initial.token_version);
    assert_eq!(reconnected.access_token, "recovered");
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// Проверяет, что после отказа сервера токен обновляется с задержкой даже до истечения срока.
#[tokio::test]
async fn runtime_refreshes_a_token_rejected_before_expiry() {
    let fixture = Fixture::new(|user, _| Ok(project_token(user, "replacement", None)));
    let owner = fixture.login("a");
    let storage = Arc::new(Storage::in_memory().unwrap());
    let sync = Arc::new(EventSync::new(fixture.auth.clone(), storage));
    let (connections, mut receive) = async_mpsc::unbounded_channel();
    let runtime = Runtime::new(sync, vec![Arc::new(TestSource(connections))]).unwrap();
    let (shutdown, stopped) = oneshot::channel();
    let worker = tokio::spawn(runtime.run(async {
        let _ = stopped.await;
    }));

    let (initial, stream) = tokio::time::timeout(Duration::from_secs(5), receive.recv())
        .await
        .unwrap()
        .unwrap();
    stream.send(Err(SourceError::Unauthorized)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), stream.closed())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(250), receive.recv())
            .await
            .is_err()
    );
    assert_eq!(fixture.iam.refreshes.load(Ordering::SeqCst), 0);

    let (replacement, _stream) = tokio::time::timeout(Duration::from_secs(5), receive.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement.context, owner);
    assert_eq!(replacement.generation, initial.generation);
    assert_eq!(replacement.token_version, initial.token_version + 1);
    assert_eq!(replacement.access_token, "replacement");
    assert_eq!(fixture.iam.refreshes.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
