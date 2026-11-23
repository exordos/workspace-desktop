//! Управляет входом, восстановлением сессий и выходом из аккаунта.
//! Контракты вынесены в contracts, продление токенов — в refresh. Здесь хранятся состояния и публикуются их изменения.

mod contracts;
mod refresh;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
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

/// Источник времени, который в тестах можно заменить управляемыми часами.
pub trait Clock: Send + Sync + 'static {
    /// Возвращает текущий момент для проверки сроков действия.
    fn now(&self) -> Instant;
}

/// Часы приложения, которые используют монотонное время системы.
struct SystemClock;

impl Clock for SystemClock {
    /// Читает системное время для измерения интервалов.
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Точка входа для работы с авторизацией. Копии используют общее состояние.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>, // Общие зависимости и состояния сессий.
}

/// Разрешение на короткое чтение, которое не удерживает рабочий профиль при закрытии окна.
pub(crate) struct ActiveGeneration {
    inner: Weak<Inner>,            // Состояние Auth до освобождения рабочего профиля.
    owner: OwnerContext,           // Аккаунт, организация и проект подтверждённого доступа.
    generation: SessionGeneration, // Точное поколение входа.
}

impl ActiveGeneration {
    /// Выполняет чтение только если рабочий профиль и исходный вход ещё действуют.
    pub(crate) fn with_current<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        Auth {
            inner: self.inner.upgrade()?,
        }
        .with_active_generation(&self.owner, self.generation, read)
        .ok()
    }
}

/// Данные, которые используют все копии Auth.
struct Inner {
    iam: Arc<dyn IamClient>,         // Клиент запросов к серверу авторизации.
    accounts: Arc<dyn AccountStore>, // Хранилище записей аккаунтов.
    secrets: Arc<dyn SecretStore>,   // Защищённое хранилище токенов обновления.
    clock: Arc<dyn Clock>,           // Часы для сроков входа и действия токенов.
    sequence: AtomicU64,             // Общий счётчик попыток входа и поколений сессий.
    loaded: Mutex<bool>,             // Признак успешной загрузки сохранённых аккаунтов.
    slots: Mutex<HashMap<AccountId, Arc<Mutex<AccountState>>>>, // Состояния по аккаунтам. У каждого аккаунта своя блокировка.
    attempts: Mutex<HashMap<LoginAttemptId, PendingLogin>>, // Попытки, которые ещё ждут выбора проекта.
    publication: Mutex<Publication>, // Последние снимки сессий и подписчики на изменения.
}

/// Данные для завершения входа после выбора проекта.
struct PendingLogin {
    server: String,                // Проверенный адрес сервера.
    user_id: String,               // Идентификатор пользователя из ответа IAM.
    refresh_token: String,         // Токен обновления для обмена на токен выбранного проекта.
    projects: Vec<IamProject>,     // Проекты, доступные этой попытке входа.
    expires_at: Instant,           // Момент, после которого выбор проекта отклоняется.
    selecting: bool,               // Признак, что запрос выбора проекта уже выполняется.
    _expiration: mpsc::Sender<()>, // Канал закрывается при удалении попытки и останавливает ожидание таймера.
}

/// Состояния сессий, которые видны остальному приложению.
#[derive(Default)]
struct Publication {
    sessions: HashMap<AccountId, SessionSnapshot>, // Последний снимок по каждому сохранённому аккаунту.
    subscribers: Vec<async_mpsc::Sender<()>>, // Каналы, которые будят подписчиков при изменении снимков.
}

/// Текущее внутреннее состояние одного аккаунта.
struct AccountState {
    record: Option<AccountRecord>, // Сохранённые данные аккаунта. `None` означает, что записи нет.
    generation: SessionGeneration, // Номер входа, который защищает от результатов старых запросов.
    status: SessionStatus,         // Текущее состояние сессии.
    access: Option<TimedToken>,    // Токен доступа с датами обновления и истечения, если он есть.
    refresh_token: Option<String>, // Токен обновления в памяти, если его уже загрузили или получили.
    pending_token: Option<TimedToken>, // Ответ IAM, который ждёт успешного сохранения нового секрета.
    token_version: u64,                // Номер установленного токена доступа внутри текущей сессии.
    refresh: Option<RefreshFlight>,    // Текущая общая операция обновления, если она запущена.
    retry_at: Option<Instant>, // Ближайший момент повтора после временного сбоя, если он задан.
}

impl AccountState {
    /// Создаёт состояние для восстановления записи или для пустого аккаунта.
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

    /// Меняет статус и сбрасывает токены и ожидающие обновления в памяти.
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
    /// Создаёт авторизацию с системными часами и переданными хранилищами.
    pub fn new(
        iam: Arc<dyn IamClient>,
        accounts: Arc<dyn AccountStore>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        Self::with_clock(iam, accounts, secrets, Arc::new(SystemClock))
    }

    /// Создаёт авторизацию с заданными часами. В тестах ими можно управлять.
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

    /// Возвращает текущие состояния аккаунтов без токенов.
    pub fn snapshot(&self) -> Vec<SessionSnapshot> {
        self.inner.publication.lock().unwrap().snapshot()
    }

    /// Выполняет запись под той же блокировкой, что и отзыв сессии.
    /// Переданная функция не должна обращаться к авторизации или выполнять сетевые запросы.
    pub(crate) fn with_active_generation<T>(
        &self,
        context: &OwnerContext,
        generation: SessionGeneration,
        commit: impl FnOnce() -> T,
    ) -> Result<T, AuthError> {
        self.with_generation(
            context,
            generation,
            |state| state.status == SessionStatus::Active,
            commit,
        )
    }

    /// Даёт кадру отзываемую проверку поколения без владения рабочим профилем.
    pub(crate) fn active_generation(
        &self,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> ActiveGeneration {
        ActiveGeneration {
            inner: Arc::downgrade(&self.inner),
            owner: owner.clone(),
            generation,
        }
    }

    /// Разрешает чтение кэша во время временного сбоя у уже подтверждённого входа.
    pub(crate) fn with_confirmed_generation<T>(
        &self,
        context: &OwnerContext,
        generation: SessionGeneration,
        read: impl FnOnce() -> T,
    ) -> Result<T, AuthError> {
        self.with_generation(
            context,
            generation,
            |state| {
                state.status == SessionStatus::Active
                    || (state.status == SessionStatus::TemporarilyUnavailable
                        && state.token_version != 0)
            },
            read,
        )
    }

    /// Проверяет владельца, поколение и допустимый статус под блокировкой аккаунта.
    fn with_generation<T>(
        &self,
        context: &OwnerContext,
        generation: SessionGeneration,
        allows: impl FnOnce(&AccountState) -> bool,
        action: impl FnOnce() -> T,
    ) -> Result<T, AuthError> {
        let slot = self.account_slot(&context.account_id)?;
        let state = slot.lock().unwrap();
        if state.generation != generation
            || state.record.as_ref().map(|record| &record.context) != Some(context)
            || !allows(&state)
        {
            return Err(AuthError::SessionUnavailable);
        }
        Ok(action())
    }

    /// Сразу отдаёт текущий список сессий, а после изменений — его свежую версию.
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

    /// Проверяет данные для входа и возвращает попытку со списком проектов.
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

    /// Отменяет выбор проекта и освобождает временные данные попытки.
    pub fn cancel_login(&self, attempt: LoginAttemptId) {
        self.inner.attempts.lock().unwrap().remove(&attempt);
    }

    /// Проверяет выбранный проект, получает токен и сохраняет новую сессию.
    /// При временном сбое IAM ту же попытку можно повторить до истечения срока.
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
        // Запрос выполняется без блокировки. После него ещё раз проверяем, жива ли попытка.
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
        // Запоздавший ответ не должен заменить состояние, которое уже изменилось.
        if state.generation.0 > attempt.0 {
            return Err(AuthError::AttemptUnavailable);
        }
        self.commit_login(&mut state, context.clone(), token, refresh_token)?;
        Ok(context)
    }

    /// Загружает сохранённые аккаунты и ждёт обновления сессий или очистки после выхода.
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

    /// Останавливает сессию аккаунта и удаляет сохранённые данные. При сбое очистку можно повторить.
    pub fn logout(&self, account_id: &AccountId) -> Result<(), AuthError> {
        self.load_accounts()?;
        let slot = self.slot_or_insert(account_id);
        let mut state = slot.lock().unwrap();
        self.logout_state(account_id, &mut state)
    }

    /// Завершает только поколение, из которого окно подтвердило выход.
    pub fn logout_generation(
        &self,
        context: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<(), AuthError> {
        let slot = self.account_slot(&context.account_id)?;
        let mut state = slot.lock().unwrap();
        if state.generation != generation
            || state.record.as_ref().map(|record| &record.context) != Some(context)
        {
            return Err(AuthError::SessionUnavailable);
        }
        self.logout_state(&context.account_id, &mut state)
    }

    /// Публикует отзыв доступа до удаления секретов, пока заблокирован исходный вход.
    fn logout_state(
        &self,
        account_id: &AccountId,
        state: &mut AccountState,
    ) -> Result<(), AuthError> {
        state.end(SessionStatus::SigningOut);
        state.generation = SessionGeneration(self.next_sequence());
        self.publish(account_id, state);
        self.finish_logout(state)?;
        self.publish(account_id, state);
        Ok(())
    }

    /// Вычисляет одинаковый идентификатор для одной пары сервер и пользователь.
    fn account_id(server: &str, user_id: &str) -> AccountId {
        let identity = format!("{server}\0{user_id}");
        AccountId(Uuid::new_v5(&Uuid::NAMESPACE_URL, identity.as_bytes()).to_string())
    }

    /// Проверяет, что ключ секрета принадлежит аккаунту и оканчивается корректным UUID.
    fn check_secret_key(account_id: &AccountId, key: &str) -> bool {
        key.split_once(':')
            .is_some_and(|(owner, nonce)| owner == account_id.0 && Uuid::parse_str(nonce).is_ok())
    }

    /// Выдаёт следующий номер для попытки входа или поколения сессии.
    fn next_sequence(&self) -> u64 {
        self.inner.sequence.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Находит состояние аккаунта или создаёт пустое.
    fn slot_or_insert(&self, account_id: &AccountId) -> Arc<Mutex<AccountState>> {
        self.inner
            .slots
            .lock()
            .unwrap()
            .entry(account_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(AccountState::new(None))))
            .clone()
    }

    /// Находит существующее состояние или сообщает, что сессия недоступна.
    fn account_slot(&self, account_id: &AccountId) -> Result<Arc<Mutex<AccountState>>, AuthError> {
        self.inner
            .slots
            .lock()
            .unwrap()
            .get(account_id)
            .cloned()
            .ok_or(AuthError::SessionUnavailable)
    }

    /// Один раз читает и проверяет записи аккаунтов, затем публикует их состояния.
    /// Окно может вызвать этот шаг до первого снимка. Сетевое восстановление выполняет `restore`.
    pub fn load_accounts(&self) -> Result<(), AuthError> {
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

    /// Сохраняет новый секрет и запись аккаунта, затем включает сессию и удаляет старые секреты.
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
        // Храним новый секрет под отдельным ключом, чтобы сохранить старый вход при сбое записи.
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

    /// Помечает запись как вышедшую, удаляет секреты и только потом саму запись.
    fn finish_logout(&self, state: &mut AccountState) -> Result<(), AuthError> {
        let Some(record) = &mut state.record else {
            return Ok(());
        };
        // Сохраняем отметку о выходе до очистки, чтобы после перезапуска не восстановить сессию.
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

    /// Обновляет снимок аккаунта и уведомляет подписчиков об изменении.
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
    /// Собирает копии снимков в стабильном порядке по идентификаторам аккаунтов.
    fn snapshot(&self) -> Vec<SessionSnapshot> {
        let mut snapshot: Vec<_> = self.sessions.values().cloned().collect();
        snapshot.sort_by(|left, right| left.context.account_id.cmp(&right.context.account_id));
        snapshot
    }
}
