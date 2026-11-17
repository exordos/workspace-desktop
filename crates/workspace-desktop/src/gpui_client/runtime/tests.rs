//! Проверяет запуск, восстановление после пропуска и остановку диспетчера без графической сессии.

mod login;

use std::{
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc as blocking,
    },
    time::Duration,
};

use futures::{future::BoxFuture, stream};
use uuid::Uuid;
use workspace_core::{
    auth::{IamClient, IamError, IamProject, IamToken, SecretStore, StoreError},
    runtime::{EventSource, EventStream, SourceError, SourceSession},
    storage::Cursor,
    sync::Event,
};

use super::*;

const TIMEOUT: Duration = Duration::from_secs(10);
const PROFILE_SERVER: &str = "https://127.0.0.1:1";

/// Задерживает только восстановление A; токены и учётные данные синтетические.
struct TestIam {
    release_a: Mutex<blocking::Receiver<()>>, // Продолжить первый запрос для A.
}

impl IamClient for TestIam {
    /// В тесте аккаунты уже сохранены и адрес сформирован.
    fn normalize_server(&self, _: &str) -> Result<String, IamError> {
        unreachable!()
    }

    /// Новый вход не выполняется в сценарии восстановления.
    fn login(&self, _: &str, _: &str, _: &str, _: Option<&str>) -> Result<IamToken, IamError> {
        unreachable!()
    }

    /// Сохранённые аккаунты не запрашивают список проектов.
    fn projects(&self, _: &str, _: &str) -> Result<Vec<IamProject>, IamError> {
        unreachable!()
    }

    /// Для сохранённого аккаунта проект уже выбран.
    fn select_project(&self, _: &str, _: &str, _: &str) -> Result<IamToken, IamError> {
        unreachable!()
    }

    /// Возвращает подтверждение проекта, задерживая только A.
    fn refresh(&self, _: &str, refresh: &str) -> Result<IamToken, IamError> {
        if refresh == "a" {
            self.release_a
                .lock()
                .unwrap()
                .recv_timeout(TIMEOUT)
                .map_err(|_| IamError::Temporary)?;
        }
        Ok(IamToken {
            access_token: String::from("test-access"),
            refresh_token: (refresh == "a").then(|| String::from("rotated-test-refresh")),
            expires_in: Duration::from_secs(3600),
            user_id: refresh.into(),
            project_id: Some("test-project".into()),
            organization_id: Some("test-organization".into()),
        })
    }
}

/// Имитирует Keychain без системного хранилища.
struct TestSecrets {
    values: Mutex<Vec<(String, String)>>, // Ключи и синтетические значения профиля.
    writes: Arc<AtomicUsize>,             // Сколько обновлений успели сохранить.
}

impl SecretStore for TestSecrets {
    /// Возвращает тестовый маркер для сохранённого аккаунта.
    fn load(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .values
            .lock()
            .unwrap()
            .iter()
            .find(|(stored, _)| stored == key)
            .map(|(_, marker)| marker.clone()))
    }

    /// Имитирует запись нового секрета или замену существующего и подтверждает её тесту.
    fn save(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let mut values = self.values.lock().unwrap();
        if let Some((_, stored)) = values.iter_mut().find(|(stored, _)| stored == key) {
            *stored = value.to_owned();
        } else {
            values.push((key.to_owned(), value.to_owned()));
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Тестовые аккаунты не удаляются во время этого прогона.
    fn remove(&self, _: &str) -> Result<(), StoreError> {
        unreachable!()
    }
}

/// Имитирует открытое соединение и возвращает следующее число после переподключения.
struct TestSource;

impl EventSource for TestSource {
    /// Использует собственное имя для позиций тестового журнала.
    fn id(&self) -> &str {
        "test-runtime"
    }

    /// Позволяет проверить восстановление с последней позиции после wake.
    fn connect(&self, session: SourceSession) -> BoxFuture<'_, Result<EventStream, SourceError>> {
        Box::pin(async move {
            let count = session
                .cursor
                .map_or(1, |cursor| cursor.epoch_version as u32 + 1);
            let source = stream::iter([Ok(unread_event(count))]).chain(stream::pending());
            Ok(Box::pin(source) as EventStream)
        })
    }
}

/// Готовит событие одного тестового журнала так, как его получает Sync.
fn unread_event(count: u32) -> Event {
    Event::UnreadChanged {
        cursor: Cursor {
            epoch_generation: "test-epoch".into(),
            epoch_version: count.into(),
        },
        count,
    }
}

/// Готовит запись в формате сохранённого профиля Workspace.
fn record(user: &str) -> AccountRecord {
    let account_id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("{PROFILE_SERVER}\0{user}").as_bytes(),
    );
    AccountRecord {
        context: OwnerContext {
            account_id: AccountId(account_id.to_string()),
            server: PROFILE_SERVER.into(),
            organization_id: "test-organization".into(),
            project_id: "test-project".into(),
            user_id: user.into(),
        },
        secret_key: format!("{account_id}:{}", Uuid::nil()),
        signed_out: false,
        retired_secret_keys: Vec::new(),
    }
}

/// Настраивает настоящий Bootstrap с временной базой и управляемыми IAM и потоком событий.
fn test_parts(
    directory: PathBuf,
    release_a: blocking::Receiver<()>,
    secret_writes: Arc<AtomicUsize>,
) -> (WorkerParts, oneshot::Receiver<()>) {
    let (accounts, released) =
        ProfileAccounts::open(&directory).expect("открытие тестового профиля");
    let secrets = ["a", "b"]
        .into_iter()
        .map(|user| {
            let record = record(user);
            accounts
                .save(&record)
                .expect("создание сохранённого аккаунта");
            (record.secret_key, user.to_owned())
        })
        .collect();
    let auth = Auth::new(
        Arc::new(TestIam {
            release_a: Mutex::new(release_a),
        }),
        Arc::new(accounts),
        Arc::new(TestSecrets {
            values: Mutex::new(secrets),
            writes: secret_writes,
        }),
    );
    let storage = Storage::open(directory.join("messenger.sqlite3")).expect("тестовая SQLite");
    let messenger = MessengerSource::new().expect("адаптер каталога");
    let parts =
        WorkerParts::with_sources(auth, Arc::new(storage), Arc::new(messenger), |services| {
            Runtime::new(services.sync.clone(), vec![Arc::new(TestSource)])
        })
        .expect("настройка фонового цикла");
    (parts, released)
}

/// Ждёт наблюдаемое состояние с общей границей времени, даже если приходят другие события.
async fn wait_event<T>(
    events: &mut RuntimeEvents,
    step: &str,
    mut find: impl FnMut(RuntimeEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(TIMEOUT, async {
        while let Some(event) = events.recv().await {
            if let Some(value) = find(event) {
                return value;
            }
        }
        panic!("подписка закрылась: {step}");
    })
    .await
    .unwrap_or_else(|_| panic!("не дождались: {step}"))
}

/// Проверяет начальный снимок, независимое восстановление, wake и перечитывание после пропуска.
#[tokio::test(flavor = "current_thread")]
async fn bootstrap_delivers_independent_sessions_and_recovers_lag() {
    let profile = tempfile::tempdir().unwrap();
    let directory = profile.path().to_owned();
    let (release_a, hold_a) = blocking::channel();
    let (client, mut events) =
        RuntimeClient::start_using(move || Ok(test_parts(directory, hold_a, Arc::default())))
            .unwrap();
    assert_eq!(client.ready().await, Ok(()));
    let Some(RuntimeEvent::Accounts(first_accounts)) = events.recv().await else {
        panic!("первый снимок с сохранёнными аккаунтами не получен");
    };
    assert_eq!(first_accounts.len(), 2);
    assert!(first_accounts.iter().any(|account| {
        account.session.owner.user_id == "a" && account.status == SessionStatus::Restoring
    }));
    assert!(matches!(
        events.recv().await,
        Some(RuntimeEvent::Catalog(_))
    ));

    let mut known_b = false;
    let b = wait_event(
        &mut events,
        "готовность B, пока A ждёт IAM",
        |event| {
            match event {
                RuntimeEvent::Accounts(accounts) => {
                    let a = accounts
                        .iter()
                        .find(|item| item.session.owner.user_id == "a");
                    assert!(a.is_none_or(|item| item.status != SessionStatus::Active));
                    known_b = accounts.iter().any(|item| {
                        item.session.owner.user_id == "b" && item.status == SessionStatus::Active
                    });
                }
                RuntimeEvent::Sync(update) if update.context.user_id == "b" => {
                    assert!(known_b, "обновление B пришло раньше активной сессии");
                    assert_eq!(update.change, SyncChange::Unread(1));
                    return Some(AccountSession {
                        owner: update.context,
                        generation: update.generation,
                    });
                }
                _ => {}
            }
            None
        },
    )
    .await;

    client.wake_sender().try_send(()).unwrap();
    wait_event(
        &mut events,
        "счётчик B после пробуждения",
        |event| match event {
            RuntimeEvent::Sync(update)
                if update.context == b.owner && update.change == SyncChange::Unread(2) =>
            {
                Some(())
            }
            _ => None,
        },
    )
    .await;

    release_a.send(()).unwrap();
    wait_event(&mut events, "готовность A", |event| match event {
        RuntimeEvent::Accounts(accounts)
            if accounts
                .iter()
                .filter(|item| item.status == SessionStatus::Active)
                .count()
                == 2 =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // Один запрос фиксирует больше событий, чем помещается в broadcast. Мост получит Lagged.
    client
        .request(move |services| async move {
            for count in 3..=220 {
                services
                    .sync
                    .apply(&b.owner, b.generation, "test-runtime", unread_event(count))
                    .unwrap();
            }
        })
        .await
        .unwrap();
    let mut recovered = false;
    wait_event(
        &mut events,
        "актуальный снимок перед SyncLagged",
        |event| {
            match event {
                RuntimeEvent::Accounts(accounts)
                    if accounts.iter().any(|item| {
                        item.session.owner.user_id == "b" && item.unread == Some(220)
                    }) =>
                {
                    recovered = true
                }
                RuntimeEvent::SyncLagged => {
                    assert!(recovered, "при пропуске не перечитан сохранённый счётчик");
                    return Some(());
                }
                _ => {}
            }
            None
        },
    )
    .await;

    assert_eq!(
        tokio::time::timeout(TIMEOUT, client.shutdown())
            .await
            .unwrap(),
        RuntimeExit::Clean
    );
    wait_event(&mut events, "завершение Bootstrap", |event| match event {
        RuntimeEvent::Stopped(exit) => {
            assert_eq!(exit, RuntimeExit::Clean);
            Some(())
        }
        _ => None,
    })
    .await;
    assert!(events.recv().await.is_none());
    assert!(
        JsonAccountStore::open(profile.path()).is_ok(),
        "профиль всё ещё заблокирован"
    );
}

/// Проверяет, что начатую блокирующую запись нельзя пропустить при выходе.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_waits_for_started_work_and_releases_profile() {
    let profile = tempfile::tempdir().unwrap();
    let (client, _events) = RuntimeClient::start_in(profile.path().to_owned()).unwrap();
    assert_eq!(client.ready().await, Ok(()));
    let path = profile.path().join("written");
    let write_to = path.clone();
    let (started, working) = oneshot::channel();
    let (release, wait) = blocking::channel();
    let writer = client.clone();
    let write = tokio::spawn(async move {
        writer
            .request(move |_| async move {
                let _ = started.send(());
                wait.recv_timeout(TIMEOUT).expect("разрешить запись");
                std::fs::write(write_to, "committed").expect("записать файл");
            })
            .await
    });
    tokio::time::timeout(TIMEOUT, working)
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), client.shutdown())
            .await
            .is_err()
    );
    assert_eq!(
        client.request(|_| async {}).await,
        Err(RequestError::Stopped)
    );
    release.send(()).unwrap();
    assert_eq!(write.await.unwrap(), Ok(()));
    assert_eq!(
        tokio::time::timeout(TIMEOUT, client.shutdown())
            .await
            .unwrap(),
        RuntimeExit::Clean
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "committed");
    assert!(JsonAccountStore::open(profile.path()).is_ok());
}

/// Проверяет, что поток Auth завершает работу до снятия блокировки профиля и выхода.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_waits_for_auth_after_bootstrap_is_cancelled() {
    let profile = tempfile::tempdir().unwrap();
    let directory = profile.path().to_owned();
    let (release_a, hold_a) = blocking::channel();
    let secret_writes = Arc::new(AtomicUsize::new(0));
    let worker_writes = secret_writes.clone();
    let (client, mut events) =
        RuntimeClient::start_using(move || Ok(test_parts(directory, hold_a, worker_writes)))
            .unwrap();
    wait_event(
        &mut events,
        "восстановление A началось, пока B уже работает",
        |event| {
            if let RuntimeEvent::Accounts(accounts) = event
                && accounts.iter().any(|item| {
                    item.session.owner.user_id == "a" && item.status == SessionStatus::Restoring
                })
                && accounts.iter().any(|item| {
                    item.session.owner.user_id == "b" && item.status == SessionStatus::Active
                })
            {
                return Some(());
            }
            None
        },
    )
    .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(25), client.shutdown())
            .await
            .is_err()
    );
    assert!(
        JsonAccountStore::open(profile.path()).is_err(),
        "профилем ещё владеет Auth"
    );
    release_a.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(TIMEOUT, client.shutdown())
            .await
            .unwrap(),
        RuntimeExit::Clean
    );
    assert_eq!(
        secret_writes.load(Ordering::SeqCst),
        1,
        "ротация секрета завершена"
    );
    assert!(
        JsonAccountStore::open(profile.path()).is_ok(),
        "профиль освобождён"
    );
}

/// Возвращает окну ошибку открытия профиля и единичное событие завершения.
#[tokio::test(flavor = "current_thread")]
async fn startup_failure_is_reported_to_the_window() {
    let invalid_profile = tempfile::NamedTempFile::new().unwrap();
    let (client, mut events) = RuntimeClient::start_in(invalid_profile.path().to_owned()).unwrap();
    assert_eq!(
        tokio::time::timeout(TIMEOUT, client.ready()).await.unwrap(),
        Err(StartupError::Profile)
    );
    assert!(matches!(
        events.recv().await,
        Some(RuntimeEvent::Stopped(RuntimeExit::Failed))
    ));
    assert!(events.recv().await.is_none());
}
