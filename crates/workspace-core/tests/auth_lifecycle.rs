use std::collections::HashMap;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::Context;
use std::thread;
use std::time::{Duration, Instant};

use futures::{StreamExt, executor::block_on, task::noop_waker_ref};
use workspace_core::auth::{
    AccountRecord, AccountStore, Auth, AuthError, Clock, IamClient, IamError, IamProject, IamToken,
    SecretStore, Service, SessionStatus, StoreError,
};
use workspace_core::context::{AccountId, OwnerContext};

const SERVER: &str = "https://workspace.test";

#[derive(Default)]
struct MemoryStores {
    accounts: Mutex<HashMap<AccountId, AccountRecord>>,
    secrets: Mutex<HashMap<String, String>>,
    fail_account_save: AtomicBool,
    fail_secret_save: AtomicBool,
    fail_secret_remove: AtomicBool,
}

impl AccountStore for MemoryStores {
    fn load(&self) -> Result<Vec<AccountRecord>, StoreError> {
        Ok(self.accounts.lock().unwrap().values().cloned().collect())
    }

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

    fn remove(&self, account_id: &AccountId) -> Result<(), StoreError> {
        self.accounts.lock().unwrap().remove(account_id);
        Ok(())
    }
}

impl SecretStore for MemoryStores {
    fn load(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.secrets.lock().unwrap().get(key).cloned())
    }

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

    fn remove(&self, key: &str) -> Result<(), StoreError> {
        if self.fail_secret_remove.swap(false, Ordering::SeqCst) {
            return Err(StoreError);
        }
        self.secrets.lock().unwrap().remove(key);
        Ok(())
    }
}

struct TestClock(Mutex<Instant>);

impl TestClock {
    fn advance(&self, seconds: u64) {
        *self.0.lock().unwrap() += Duration::from_secs(seconds);
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

type RefreshResponse = dyn Fn(&str, usize) -> Result<IamToken, IamError> + Send + Sync;

struct TestIam {
    refreshes: AtomicUsize,
    response: Box<RefreshResponse>,
}

impl IamClient for TestIam {
    fn normalize_server(&self, server: &str) -> Result<String, IamError> {
        Ok(server.to_owned())
    }

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

    fn projects(&self, _server: &str, _token: &str) -> Result<Vec<IamProject>, IamError> {
        Ok(vec![IamProject {
            id: "project".into(),
            name: "Project".into(),
            organization_id: Some("org".into()),
        }])
    }

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

    fn refresh(&self, _server: &str, token: &str) -> Result<IamToken, IamError> {
        let attempt = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        (self.response)(token, attempt)
    }
}

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

struct Fixture {
    iam: Arc<TestIam>,
    stores: Arc<MemoryStores>,
    clock: Arc<TestClock>,
    auth: Auth,
}

impl Fixture {
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

    fn restart(&self) -> Auth {
        Auth::with_clock(
            self.iam.clone(),
            self.stores.clone(),
            self.stores.clone(),
            self.clock.clone(),
        )
    }

    fn login(&self, user: &str) -> OwnerContext {
        let attempt = self
            .auth
            .begin_login(SERVER, user, "password", None)
            .unwrap();
        self.auth.complete_login(attempt.id, "project").unwrap()
    }
}

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
