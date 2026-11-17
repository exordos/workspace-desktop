//! Проверяет новый вход и отмену выбора проекта через команды фонового исполнителя.

use super::*;

const USER: &str = "login-test-user";
const PROJECT: &str = "test-project";

/// Даёт тесту остановить запрос выбора проекта до получения ответа IAM.
struct ProjectGate {
    started: Mutex<Option<oneshot::Sender<()>>>, // Сигнал о входе в сетевой запрос.
    release: Mutex<blocking::Receiver<()>>,      // Разрешение вернуть ответ IAM.
}

/// Имитирует IAM с одноразовым кодом и несколькими доступными проектами.
struct LoginIam {
    project_gate: Option<ProjectGate>, // None означает обычный ответ без ожидания.
}

impl IamClient for LoginIam {
    /// Использует то же правило HTTPS, что рабочий адаптер.
    fn normalize_server(&self, server: &str) -> Result<String, IamError> {
        WorkspaceIamHttp::new().normalize_server(server)
    }

    /// Проверяет синтетические учётные данные и требует тестовый код.
    fn login(
        &self,
        server: &str,
        login: &str,
        password: &str,
        otp: Option<&str>,
    ) -> Result<IamToken, IamError> {
        assert_eq!(server, PROFILE_SERVER);
        if login != USER || password != "test-password" {
            return Err(IamError::InvalidCredentials);
        }
        if otp != Some("123456") {
            return Err(IamError::OtpRequired);
        }
        Ok(token_for(None))
    }

    /// Возвращает проекты, доступные после первого шага входа.
    fn projects(&self, _: &str, _: &str) -> Result<Vec<IamProject>, IamError> {
        Ok([
            (PROJECT, "Тестовый проект"),
            ("another-project", "Другой проект"),
        ]
        .into_iter()
        .map(|(id, name)| IamProject {
            id: id.into(),
            name: name.into(),
            organization_id: Some("test-organization".into()),
        })
        .collect())
    }

    /// Возвращает токен выбранного проекта, по запросу ждёт разрешения теста.
    fn select_project(&self, _: &str, _: &str, project_id: &str) -> Result<IamToken, IamError> {
        if let Some(gate) = &self.project_gate {
            let _ = gate.started.lock().unwrap().take().unwrap().send(());
            gate.release
                .lock()
                .unwrap()
                .recv_timeout(TIMEOUT)
                .map_err(|_| IamError::Temporary)?;
        }
        Ok(token_for(Some(project_id)))
    }

    /// В этих сценариях нет сохранённой сессии для восстановления.
    fn refresh(&self, _: &str, _: &str) -> Result<IamToken, IamError> {
        unreachable!()
    }
}

/// Готовит тестовый токен для первого шага или подтверждённого проекта.
fn token_for(project_id: Option<&str>) -> IamToken {
    IamToken {
        access_token: "test-access".into(),
        refresh_token: Some("test-refresh".into()),
        expires_in: Duration::from_secs(3600),
        user_id: USER.into(),
        project_id: project_id.map(str::to_owned),
        organization_id: Some("test-organization".into()),
    }
}

/// Запускает рабочий диспетчер с пустым профилем, отдельным IAM и секретами в памяти.
fn start_login(
    directory: PathBuf,
    project_gate: Option<ProjectGate>,
    writes: Arc<AtomicUsize>,
) -> (RuntimeClient, RuntimeEvents) {
    RuntimeClient::start_using(move || {
        let (accounts, released) = ProfileAccounts::open(&directory).unwrap();
        let auth = Auth::new(
            Arc::new(LoginIam { project_gate }),
            Arc::new(accounts),
            Arc::new(TestSecrets {
                values: Mutex::default(),
                writes,
            }),
        );
        let storage = Storage::open(directory.join("messenger.sqlite3")).unwrap();
        let messenger = MessengerSource::new().unwrap();
        let parts =
            WorkerParts::with_sources(auth, Arc::new(storage), Arc::new(messenger), |services| {
                Runtime::new(services.sync.clone(), vec![Arc::new(TestSource)])
            })?;
        #[cfg(feature = "gpui-test")]
        let parts = {
            let mut parts = parts;
            parts.services.registration_url_for_test = Some(|server| {
                (server == PROFILE_SERVER).then(|| "https://example.test/register".into())
            });
            parts
        };
        Ok((parts, released))
    })
    .unwrap()
}

#[cfg(feature = "gpui-test")]
impl RuntimeClient {
    /// Открывает пустой профиль с управляемым IAM для проверки формы из рабочего окна.
    pub(in crate::gpui_client) fn start_sign_in_for_test(
        directory: PathBuf,
    ) -> (RuntimeClient, RuntimeEvents) {
        start_login(directory, None, Arc::default())
    }
}

/// Передаёт диспетчеру данные с формы и ждёт результата с заданным тестом кодом.
async fn submit_credentials(
    client: &RuntimeClient,
    password: &str,
    otp: Option<&str>,
) -> Result<LoginAttempt, LoginError> {
    tokio::time::timeout(
        TIMEOUT,
        client.begin_login(
            PROFILE_SERVER.into(),
            USER.into(),
            password.into(),
            otp.map(str::to_owned),
        ),
    )
    .await
    .expect("дождаться результата ввода учётных данных")
}

/// Проверяет, что аккаунт появляется у подписчиков только после выбора проекта и записи секрета.
#[tokio::test(flavor = "current_thread")]
async fn login_requires_otp_then_publishes_saved_project() {
    let profile = tempfile::tempdir().unwrap();
    let writes = Arc::new(AtomicUsize::new(0));
    let (client, mut events) = start_login(profile.path().into(), None, writes.clone());
    assert_eq!(client.ready().await, Ok(()));
    let first = tokio::time::timeout(TIMEOUT, events.recv())
        .await
        .expect("дождаться первого снимка профиля");
    assert!(matches!(first, Some(RuntimeEvent::Accounts(accounts)) if accounts.is_empty()));
    assert_eq!(
        client.validate_login_server(" 127.0.0.1:1/login "),
        Ok(PROFILE_SERVER.into()),
    );
    assert_eq!(
        client.validate_login_server("http://127.0.0.1:1"),
        Err(LoginError::InvalidServer),
    );
    assert!(matches!(
        submit_credentials(&client, "other-password", None).await,
        Err(LoginError::InvalidCredentials)
    ));
    assert!(matches!(
        submit_credentials(&client, "test-password", None).await,
        Err(LoginError::OtpRequired)
    ));
    assert!(matches!(
        submit_credentials(&client, "test-password", Some("111111")).await,
        Err(LoginError::InvalidOtp)
    ));
    let attempt = submit_credentials(&client, "test-password", Some("123456"))
        .await
        .expect("принять верный код");
    assert_eq!(attempt.projects()[0].id, PROJECT);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    let owner = tokio::time::timeout(TIMEOUT, client.complete_login(attempt.id(), PROJECT.into()))
        .await
        .expect("дождаться выбора проекта")
        .expect("подтвердить выбор проекта");
    wait_event(
        &mut events,
        "активная сессия нового аккаунта",
        |event| {
            matches!(event, RuntimeEvent::Accounts(accounts) if accounts.iter().any(|account| {
                account.session.owner == owner && account.status == SessionStatus::Active
            }))
            .then_some(())
        },
    )
    .await;
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        tokio::time::timeout(TIMEOUT, client.shutdown())
            .await
            .expect("освободить профиль, пока экран всё ещё держит попытку"),
        RuntimeExit::Clean,
    );
    let saved = JsonAccountStore::open(profile.path())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].context, owner);
    drop(attempt);
}

/// Проверяет, что уход с формы во время запроса не даёт позднему ответу сохранить вход.
#[tokio::test(flavor = "current_thread")]
async fn dropping_project_selection_discards_late_success() {
    let profile = tempfile::tempdir().unwrap();
    let (started, waiting) = oneshot::channel();
    let (release, held) = blocking::channel();
    let writes = Arc::new(AtomicUsize::new(0));
    let (client, _events) = start_login(
        profile.path().into(),
        Some(ProjectGate {
            started: Mutex::new(Some(started)),
            release: Mutex::new(held),
        }),
        writes.clone(),
    );
    let attempt = submit_credentials(&client, "test-password", Some("123456"))
        .await
        .expect("принять верный код");
    let runtime = client.clone();
    let attempt_id = attempt.id();
    let request =
        tokio::spawn(async move { runtime.complete_login(attempt_id, PROJECT.into()).await });
    tokio::time::timeout(TIMEOUT, waiting)
        .await
        .expect("дождаться начала запроса проекта")
        .unwrap();
    drop(attempt);
    release.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(TIMEOUT, request)
            .await
            .expect("дождаться ответа по отменённой попытке")
            .unwrap(),
        Err(LoginError::AttemptUnavailable),
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(client.shutdown().await, RuntimeExit::Clean);
    assert!(
        JsonAccountStore::open(profile.path())
            .unwrap()
            .load()
            .unwrap()
            .is_empty()
    );
}
