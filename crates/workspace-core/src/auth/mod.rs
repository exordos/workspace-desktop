mod contracts;
mod refresh;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use futures::channel::{mpsc as async_mpsc, oneshot};
use futures::future::join_all;
use futures::{Stream, StreamExt};
use uuid::Uuid;

use crate::context::{AccountId, OwnerContext, SessionGeneration};

pub use contracts::*;
use refresh::{RefreshFlight, TimedToken};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

struct Inner {
    iam: Arc<dyn IamClient>,
    accounts: Arc<dyn AccountStore>,
    secrets: Arc<dyn SecretStore>,
    clock: Arc<dyn Clock>,
    sequence: AtomicU64,
    loaded: Mutex<bool>,
    slots: Mutex<HashMap<AccountId, Arc<Mutex<AccountState>>>>,
    attempts: Mutex<HashMap<LoginAttemptId, PendingLogin>>,
    publication: Mutex<Publication>,
}

struct PendingLogin {
    server: String,
    user_id: String,
    refresh_token: String,
    projects: Vec<IamProject>,
    expires_at: Instant,
    selecting: bool,
    _expiration: mpsc::Sender<()>,
}

#[derive(Default)]
struct Publication {
    sessions: HashMap<AccountId, SessionSnapshot>,
    subscribers: Vec<async_mpsc::Sender<()>>,
}

struct AccountState {
    record: Option<AccountRecord>,
    generation: SessionGeneration,
    status: SessionStatus,
    access: Option<TimedToken>,
    refresh_token: Option<String>,
    pending_token: Option<TimedToken>,
    token_version: u64,
    refresh: Option<RefreshFlight>,
    retry_at: Option<Instant>,
}

impl AccountState {
    fn new(record: Option<AccountRecord>) -> Self {
        let status = if record.as_ref().is_some_and(|record| !record.signed_out) {
            SessionStatus::Restoring
        } else {
            SessionStatus::SigningOut
        };
        Self {
            record,
            status,
            generation: SessionGeneration(0),
            access: None,
            refresh_token: None,
            pending_token: None,
            token_version: 0,
            refresh: None,
            retry_at: None,
        }
    }

    fn end(&mut self, status: SessionStatus) {
        self.status = status;
        self.access = None;
        self.refresh_token = None;
        self.pending_token = None;
        self.refresh = None;
        self.retry_at = None;
    }
}

impl Auth {
    pub fn new(
        iam: Arc<dyn IamClient>,
        accounts: Arc<dyn AccountStore>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        Self::with_clock(iam, accounts, secrets, Arc::new(SystemClock))
    }

    pub fn with_clock(
        iam: Arc<dyn IamClient>,
        accounts: Arc<dyn AccountStore>,
        secrets: Arc<dyn SecretStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                iam,
                accounts,
                secrets,
                clock,
                sequence: AtomicU64::new(0),
                loaded: Mutex::new(false),
                slots: Mutex::new(HashMap::new()),
                attempts: Mutex::new(HashMap::new()),
                publication: Mutex::new(Publication::default()),
            }),
        }
    }

    pub fn snapshot(&self) -> Vec<SessionSnapshot> {
        self.inner.publication.lock().unwrap().snapshot()
    }

    pub fn subscribe(&self) -> impl Stream<Item = Vec<SessionSnapshot>> + use<> {
        let (mut sender, receiver) = async_mpsc::channel(1);
        let mut publication = self.inner.publication.lock().unwrap();
        let _ = sender.try_send(());
        publication.subscribers.push(sender);
        let inner = Arc::downgrade(&self.inner);
        receiver.filter_map(move |()| {
            let snapshot = inner
                .upgrade()
                .map(|inner| inner.publication.lock().unwrap().snapshot());
            std::future::ready(snapshot)
        })
    }

    pub fn begin_login(
        &self,
        server: &str,
        login: &str,
        password: &str,
        otp: Option<&str>,
    ) -> Result<LoginAttempt, AuthError> {
        self.load_accounts()?;
        let id = LoginAttemptId(self.next_sequence());
        let server = self.inner.iam.normalize_server(server)?;
        let token = self.inner.iam.login(&server, login, password, otp)?;
        if token.user_id.is_empty() || token.access_token.is_empty() {
            return Err(IamError::InvalidResponse.into());
        }
        let refresh_token = token
            .refresh_token
            .filter(|token| !token.is_empty())
            .ok_or(IamError::InvalidResponse)?;
        let projects = self.inner.iam.projects(&server, &token.access_token)?;
        let (expiration, expiration_signal) = mpsc::channel();
        self.inner.attempts.lock().unwrap().insert(
            id,
            PendingLogin {
                server,
                user_id: token.user_id,
                refresh_token,
                projects: projects.clone(),
                expires_at: self.inner.clock.now() + LOGIN_TIMEOUT,
                selecting: false,
                _expiration: expiration,
            },
        );
        let inner = Arc::downgrade(&self.inner);
        if thread::Builder::new()
            .name("login-expiration".into())
            .spawn(move || {
                if expiration_signal.recv_timeout(LOGIN_TIMEOUT)
                    == Err(mpsc::RecvTimeoutError::Timeout)
                    && let Some(inner) = inner.upgrade()
                {
                    inner.attempts.lock().unwrap().remove(&id);
                }
            })
            .is_err()
        {
            self.cancel_login(id);
            return Err(AuthError::WorkerUnavailable);
        }
        Ok(LoginAttempt { id, projects })
    }

    pub fn cancel_login(&self, attempt: LoginAttemptId) {
        self.inner.attempts.lock().unwrap().remove(&attempt);
    }

    pub fn complete_login(
        &self,
        attempt: LoginAttemptId,
        project_id: &str,
    ) -> Result<OwnerContext, AuthError> {
        let (server, user_id, refresh_token, project) = {
            let mut attempts = self.inner.attempts.lock().unwrap();
            if attempts
                .get(&attempt)
                .is_some_and(|login| login.expires_at <= self.inner.clock.now())
            {
                attempts.remove(&attempt);
            }
            let login = attempts
                .get_mut(&attempt)
                .ok_or(AuthError::AttemptUnavailable)?;
            if login.selecting {
                return Err(AuthError::AttemptUnavailable);
            }
            let project = login
                .projects
                .iter()
                .find(|project| project.id == project_id)
                .cloned()
                .ok_or(AuthError::UnknownProject)?;
            login.selecting = true;
            (
                login.server.clone(),
                login.user_id.clone(),
                login.refresh_token.clone(),
                project,
            )
        };
        let requested_at = self.inner.clock.now();
        let selection = self
            .inner
            .iam
            .select_project(&server, &refresh_token, project_id);
        let mut attempts = self.inner.attempts.lock().unwrap();
        let login = attempts
            .get_mut(&attempt)
            .ok_or(AuthError::AttemptUnavailable)?;
        if login.expires_at <= self.inner.clock.now() {
            attempts.remove(&attempt);
            return Err(AuthError::AttemptUnavailable);
        }
        if matches!(selection, Err(IamError::Temporary)) {
            login.selecting = false;
        } else {
            attempts.remove(&attempt);
        }
        drop(attempts);

        let token = selection?;
        if let (Some(expected), Some(actual)) = (&project.organization_id, &token.organization_id)
            && expected != actual
        {
            return Err(IamError::InvalidResponse.into());
        }
        let account_id = Self::account_id(&server, &user_id);
        let context = OwnerContext {
            account_id: account_id.clone(),
            organization_id: project
                .organization_id
                .or_else(|| token.organization_id.clone())
                .unwrap_or_else(|| server.clone()),
            server,
            project_id: project_id.into(),
            user_id,
        };
        let token = TimedToken::new(token, &context, requested_at)?;
        let slot = self.slot_or_insert(&account_id);
        let mut state = slot.lock().unwrap();
        if state.generation.0 > attempt.0 {
            return Err(AuthError::AttemptUnavailable);
        }
        self.commit_login(&mut state, context.clone(), token, refresh_token)?;
        Ok(context)
    }

    pub async fn restore(&self) -> Result<(), AuthError> {
        self.load_accounts()?;
        let slots: Vec<_> = self.inner.slots.lock().unwrap().values().cloned().collect();
        let mut flights = Vec::new();
        let mut cleanups = Vec::new();
        for slot in slots {
            let mut state = slot.lock().unwrap();
            match state.status {
                SessionStatus::Restoring | SessionStatus::TemporarilyUnavailable => {
                    flights.push(self.refresh_flight(&slot, &mut state)?);
                }
                SessionStatus::SigningOut
                    if state
                        .record
                        .as_ref()
                        .is_some_and(|record| record.signed_out) =>
                {
                    let auth = self.clone();
                    let slot = slot.clone();
                    let generation = state.generation;
                    let (send, receive) = oneshot::channel();
                    thread::Builder::new()
                        .name("logout-cleanup".into())
                        .spawn(move || {
                            let mut state = slot.lock().unwrap();
                            let record = state.record.as_ref().filter(|record| record.signed_out);
                            if state.generation == generation
                                && let Some(record) = record
                            {
                                let account_id = record.context.account_id.clone();
                                if auth.finish_logout(&mut state).is_ok() {
                                    auth.publish(&account_id, &state);
                                }
                            }
                            let _ = send.send(());
                        })
                        .map_err(|_| AuthError::WorkerUnavailable)?;
                    cleanups.push(receive);
                }
                _ => {}
            }
        }
        futures::join!(join_all(flights), join_all(cleanups));
        Ok(())
    }

    pub fn logout(&self, account_id: &AccountId) -> Result<(), AuthError> {
        self.load_accounts()?;
        let slot = self.slot_or_insert(account_id);
        let mut state = slot.lock().unwrap();
        state.end(SessionStatus::SigningOut);
        state.generation = SessionGeneration(self.next_sequence());
        self.publish(account_id, &state);
        self.finish_logout(&mut state)?;
        self.publish(account_id, &state);
        Ok(())
    }

    fn account_id(server: &str, user_id: &str) -> AccountId {
        let identity = format!("{server}\0{user_id}");
        AccountId(Uuid::new_v5(&Uuid::NAMESPACE_URL, identity.as_bytes()).to_string())
    }

    fn check_secret_key(account_id: &AccountId, key: &str) -> bool {
        key.split_once(':')
            .is_some_and(|(owner, nonce)| owner == account_id.0 && Uuid::parse_str(nonce).is_ok())
    }

    fn next_sequence(&self) -> u64 {
        self.inner.sequence.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn slot_or_insert(&self, account_id: &AccountId) -> Arc<Mutex<AccountState>> {
        self.inner
            .slots
            .lock()
            .unwrap()
            .entry(account_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(AccountState::new(None))))
            .clone()
    }

    fn load_accounts(&self) -> Result<(), AuthError> {
        let mut loaded = self.inner.loaded.lock().unwrap();
        if !*loaded {
            let records = self.inner.accounts.load()?;
            if records.iter().any(|record| {
                let context = &record.context;
                context.account_id != Self::account_id(&context.server, &context.user_id)
                    || !Self::check_secret_key(&context.account_id, &record.secret_key)
                    || record
                        .retired_secret_keys
                        .iter()
                        .any(|key| !Self::check_secret_key(&context.account_id, key))
            }) {
                return Err(StoreError.into());
            }
            let mut slots = self.inner.slots.lock().unwrap();
            for record in records {
                let account_id = record.context.account_id.clone();
                let mut state = AccountState::new(Some(record));
                state.generation = SessionGeneration(self.next_sequence());
                self.publish(&account_id, &state);
                slots.insert(account_id, Arc::new(Mutex::new(state)));
            }
            *loaded = true;
        }
        Ok(())
    }

    fn commit_login(
        &self,
        state: &mut AccountState,
        context: OwnerContext,
        token: TimedToken,
        base_refresh: String,
    ) -> Result<(), AuthError> {
        let mut record = AccountRecord {
            secret_key: format!("{}:{}", context.account_id.0, Uuid::new_v4()),
            context,
            signed_out: false,
            retired_secret_keys: Vec::new(),
        };
        if let Some(previous) = &state.record {
            record
                .retired_secret_keys
                .clone_from(&previous.retired_secret_keys);
            record.retired_secret_keys.push(previous.secret_key.clone());
        }
        let refresh_token = token.replacement_refresh().cloned().unwrap_or(base_refresh);
        self.inner
            .secrets
            .save(&record.secret_key, &refresh_token)?;
        if let Err(error) = self.inner.accounts.save(&record) {
            let _ = self.inner.secrets.remove(&record.secret_key);
            return Err(error.into());
        }
        let account_id = record.context.account_id.clone();
        *state = AccountState::new(Some(record));
        state.generation = SessionGeneration(self.next_sequence());
        state.refresh_token = Some(refresh_token);
        state.install(token);
        self.publish(&account_id, state);
        if let Some(record) = &mut state.record {
            let mut cleaned = record.clone();
            cleaned
                .retired_secret_keys
                .retain(|key| self.inner.secrets.remove(key).is_err());
            if cleaned.retired_secret_keys != record.retired_secret_keys
                && self.inner.accounts.save(&cleaned).is_ok()
            {
                *record = cleaned;
            }
        }
        Ok(())
    }

    fn finish_logout(&self, state: &mut AccountState) -> Result<(), AuthError> {
        let Some(record) = &mut state.record else {
            return Ok(());
        };
        if !record.signed_out {
            let mut signed_out = record.clone();
            signed_out.signed_out = true;
            self.inner
                .accounts
                .save(&signed_out)
                .map_err(|_| AuthError::LogoutPending)?;
            *record = signed_out;
        }
        for key in record
            .retired_secret_keys
            .iter()
            .chain([&record.secret_key])
        {
            self.inner
                .secrets
                .remove(key)
                .map_err(|_| AuthError::LogoutPending)?;
        }
        self.inner
            .accounts
            .remove(&record.context.account_id)
            .map_err(|_| AuthError::LogoutPending)?;
        state.record = None;
        Ok(())
    }

    fn publish(&self, account_id: &AccountId, state: &AccountState) {
        let mut publication = self.inner.publication.lock().unwrap();
        if let Some(record) = &state.record {
            publication.sessions.insert(
                account_id.clone(),
                SessionSnapshot {
                    context: record.context.clone(),
                    generation: state.generation,
                    status: state.status,
                    token_version: state.token_version,
                },
            );
        } else {
            publication.sessions.remove(account_id);
        }
        publication.subscribers.retain_mut(|subscriber| {
            let _ = subscriber.try_send(());
            !subscriber.is_closed()
        });
    }
}

impl Publication {
    fn snapshot(&self) -> Vec<SessionSnapshot> {
        let mut snapshot: Vec<_> = self.sessions.values().cloned().collect();
        snapshot.sort_by(|left, right| left.context.account_id.cmp(&right.context.account_id));
        snapshot
    }
}
