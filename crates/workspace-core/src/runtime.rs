//! Поддерживает фоновые подключения к источникам событий.
//! По состоянию сессий запускает и останавливает задачи, возобновляет чтение и передаёт события в Sync.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::{Stream, StreamExt, future::BoxFuture};
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::{JoinError, JoinSet, spawn_blocking},
    time::sleep,
};

use crate::{
    auth::{Auth, AuthError, Service, SessionSnapshot, SessionStatus},
    context::{OwnerContext, SessionGeneration},
    storage::{Cursor, StorageError},
    sync::{Event, Sync, SyncError},
};

const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Поток, который выдаёт по одному событию или ошибке источника.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<Event, SourceError>> + Send>>;

/// Причины, по которым источник событий не смог продолжить чтение.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SourceError {
    #[error("event source is unavailable")]
    Temporary, // Временный сбой. Соединение можно открыть заново.
    #[error("event source rejected the session token")]
    Unauthorized, // Сервер отклонил токен доступа.
    #[error("event source denied access to this project")]
    Forbidden, // У пользователя нет права читать события.
    #[error("event source returned an invalid response")]
    InvalidResponse, // Ответ источника не соответствует ожидаемому формату.
    #[error("saved source cursor expired and requires recovery")]
    CursorPruned, // Источник больше не хранит события с указанной позиции.
}

/// Данные, которые источник получает при открытии соединения.
pub struct SourceSession {
    pub context: OwnerContext, // Аккаунт и проект, для которых нужно читать события.
    pub generation: SessionGeneration, // Поколение сессии, к которому относится подключение.
    pub token_version: u64,    // Версия переданного токена доступа.
    pub cursor: Option<Cursor>, // Последняя сохранённая позиция. `None` означает первое подключение.
    pub access_token: String,   // Токен для подключения к источнику.
}

/// Интерфейс источника, который умеет открывать поток событий для сессии.
pub trait EventSource: Send + std::marker::Sync {
    /// Возвращает постоянное имя источника для задач и сохранённых позиций.
    fn id(&self) -> &str;

    /// Возвращает сервис, для которого нужен токен доступа.
    fn service(&self) -> Service {
        Service::Messenger
    }

    /// Открывает поток. После ошибки или конца потока Runtime может открыть его снова.
    /// Отмена запроса или уничтожение потока должны освобождать соединение.
    fn connect(&self, session: SourceSession) -> BoxFuture<'_, Result<EventStream, SourceError>>;

    /// Запускает дополнительную фоновую работу того же источника на срок жизни задачи.
    fn run_session(
        &self,
        _sync: Arc<Sync>,
        _session: SessionSnapshot,
        _ready: watch::Receiver<bool>,
    ) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

/// Ошибка настройки или работы фоновой синхронизации.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("duplicate event source: {0}")]
    DuplicateSource(String), // Имя, которое использовали два источника.
    #[error("background worker failed: {0}")]
    Worker(#[from] JoinError), // Причина аварийного завершения фоновой задачи Tokio.
}

/// Согласует фоновые задачи получения событий с состояниями сессий.
pub struct Runtime {
    sync: Arc<Sync>, // Синхронизация, которая читает позицию и сохраняет события.
    sources: HashMap<String, Arc<dyn EventSource>>, // Источники событий по их уникальным именам.
}

/// Ключ отдельной задачи чтения событий.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TaskKey {
    context: OwnerContext, // Аккаунт и проект, для которых читаются события.
    source: String,        // Имя источника событий.
}

/// Сведения о запущенной задаче для проверки и отмены.
struct Task {
    generation: SessionGeneration, // Поколение сессии, с которым запустили задачу.
    token_version: u64,            // Версия токена, с которой запустили задачу.
    connected: Arc<AtomicBool>, // Источник уже открыл поток. Пока идёт подключение, повторный сигнал не нужен.
    cancel: Option<oneshot::Sender<()>>, // Канал остановки. `None` означает, что сигнал уже отправлен.
}

impl Task {
    /// Отправляет сигнал остановки один раз.
    fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

/// Ключ завершившейся задачи и результат её работы.
type WorkerResult = (TaskKey, Result<(), JoinError>);

impl Runtime {
    /// Создаёт Runtime и проверяет, что имена источников уникальны.
    pub fn new(sync: Arc<Sync>, sources: Vec<Arc<dyn EventSource>>) -> Result<Self, RuntimeError> {
        let mut by_id = HashMap::new();
        for source in sources {
            let id = source.id().to_owned();
            if by_id.insert(id.clone(), source).is_some() {
                return Err(RuntimeError::DuplicateSource(id));
            }
        }
        Ok(Self {
            sync,
            sources: by_id,
        })
    }

    /// Даёт Bootstrap ту же авторизацию, за которой следят фоновые задачи.
    pub(crate) fn auth(&self) -> &Auth {
        self.sync.auth()
    }

    /// Следит за сессиями и управляет задачами до сигнала остановки.
    /// Чтобы дождаться задач и начатых записей, передай сигнал и дождись результата этой функции.
    /// Если отменить ожидание функции, задачи прервутся, а начатые записи могут ещё выполняться.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<(), RuntimeError> {
        self.run_with_wake(shutdown, None).await
    }

    /// Принимает сигналы пробуждения и перезапускает открытые потоки с сохранённых позиций.
    /// Закрытие канала отключает сигналы, фоновая работа при этом продолжается.
    pub async fn run_with_wake(
        self,
        shutdown: impl Future<Output = ()>,
        mut wake: Option<mpsc::Receiver<()>>,
    ) -> Result<(), RuntimeError> {
        let snapshots = self.sync.auth().subscribe();
        tokio::pin!(snapshots, shutdown);
        let mut tasks = HashMap::new();
        let mut workers = JoinSet::new();
        let result = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break Ok(()),
                snapshot = snapshots.next() => {
                    let Some(snapshot) = snapshot else { break Ok(()); };
                    self.reconcile(snapshot, &mut tasks, &mut workers);
                }
                signal = async { wake.as_mut().expect("wake channel").recv().await }, if wake.is_some() => {
                    if signal.is_some() {
                        for task in tasks.values_mut() {
                            if task.connected.load(Ordering::Relaxed) {
                                task.cancel();
                            }
                        }
                    } else {
                        wake = None;
                    }
                }
                completed = workers.join_next(), if !workers.is_empty() => match completed {
                    Some(Ok((key, Ok(())))) => {
                        tasks.remove(&key);
                        self.reconcile(self.sync.auth().snapshot(), &mut tasks, &mut workers);
                    }
                    Some(Ok((_, Err(error))) | Err(error)) => {
                        break Err(RuntimeError::Worker(error));
                    }
                    None => {}
                }
            }
        };

        for task in tasks.values_mut() {
            task.cancel();
        }
        let mut result = result;
        while let Some(completed) = workers.join_next().await {
            if let Ok((_, Err(error))) | Err(error) = completed
                && result.is_ok()
            {
                result = Err(RuntimeError::Worker(error));
            }
        }
        result
    }

    /// Останавливает устаревшие задачи и запускает недостающие для текущих сессий.
    fn reconcile(
        &self,
        snapshots: Vec<SessionSnapshot>,
        tasks: &mut HashMap<TaskKey, Task>,
        workers: &mut JoinSet<WorkerResult>,
    ) {
        let desired: HashMap<_, _> = snapshots
            .into_iter()
            .filter(|session| {
                matches!(
                    session.status,
                    SessionStatus::Active | SessionStatus::TemporarilyUnavailable
                )
            })
            .map(|session| (session.context.clone(), session))
            .collect();
        for (key, task) in tasks.iter_mut() {
            let current = desired.get(&key.context);
            if key.source == crate::messenger::SOURCE_ID
                && current.is_none_or(|session| session.generation != task.generation)
            {
                self.sync
                    .sidebar_cache
                    .clear_session(&key.context, task.generation);
            }
            if current.is_none_or(|session| {
                session.generation != task.generation || session.token_version != task.token_version
            }) {
                task.cancel();
            }
        }
        for session in desired.into_values() {
            for (id, source) in &self.sources {
                let key = TaskKey {
                    context: session.context.clone(),
                    source: id.clone(),
                };
                tasks.entry(key.clone()).or_insert_with(|| {
                    let (cancel, cancelled) = oneshot::channel();
                    let connected = Arc::new(AtomicBool::new(false));
                    let worker = Worker {
                        connected: connected.clone(),
                        sync: self.sync.clone(),
                        source: source.clone(),
                        key: key.clone(),
                        session: session.clone(),
                    };
                    workers.spawn(async move { (key, worker.run(cancelled).await) });
                    Task {
                        generation: session.generation,
                        token_version: session.token_version,
                        connected,
                        cancel: Some(cancel),
                    }
                });
            }
        }
    }
}

/// Фоновая работа с одним источником событий для одной сессии.
struct Worker {
    connected: Arc<AtomicBool>, // Показывает Runtime, что источник уже открыл поток.
    sync: Arc<Sync>,            // Доступ к авторизации и локальному сохранению событий.
    source: Arc<dyn EventSource>, // Источник, у которого задача открывает поток.
    key: TaskKey,               // Аккаунт, проект и имя источника этой задачи.
    session: SessionSnapshot,   // Снимок сессии и версии токена на момент запуска задачи.
}

impl Worker {
    /// Читает события, повторяет временные сбои с паузой и реагирует на отмену.
    async fn run(self, cancelled: oneshot::Receiver<()>) -> Result<(), JoinError> {
        let (ready, state) = watch::channel(false);
        tokio::select! {
            biased;
            result = self.run_events(cancelled, &ready) => result,
            () = self.source.run_session(self.sync.clone(), self.session.clone(), state) => Ok(()),
        }
    }

    /// Обновляет готовность соединения после догрузки событий и при каждом отключении.
    async fn run_events(
        &self,
        mut cancelled: oneshot::Receiver<()>,
        ready: &watch::Sender<bool>,
    ) -> Result<(), JoinError> {
        let mut delay = INITIAL_RETRY_DELAY;
        loop {
            let end = self.consume(&mut cancelled, &mut delay, ready).await?;
            ready.send_if_modified(std::mem::take);
            self.connected.store(false, Ordering::Relaxed);
            match end {
                ConnectionEnd::Stopped => break,
                ConnectionEnd::CursorRejected => {
                    // Повтор с той же позицией не поможет. Ждём замены или остановки задачи.
                    let _ = cancelled.await;
                    break;
                }
                end @ (ConnectionEnd::Retry | ConnectionEnd::Recover) => {
                    if matches!(end, ConnectionEnd::Recover) {
                        let sync = self.sync.clone();
                        let key = self.key.clone();
                        let generation = self.session.generation;
                        let reset = spawn_blocking(move || {
                            sync.reset_source(&key.context, generation, &key.source)
                        })
                        .await?;
                        match reset {
                            Ok(()) => {}
                            Err(SyncError::Auth(_)) => break,
                            Err(SyncError::Storage(error)) => {
                                log::warn!("{} reset failed: {error}", self.key.source)
                            }
                        }
                    }
                    if until_cancelled(&mut cancelled, sleep(delay))
                        .await
                        .is_none()
                    {
                        break;
                    }
                    delay = (delay * 2).min(MAX_RETRY_DELAY);
                }
            }
        }
        Ok(())
    }

    /// Загружает позицию и токен, открывает поток и сохраняет события до остановки или ошибки.
    async fn consume(
        &self,
        cancelled: &mut oneshot::Receiver<()>,
        delay: &mut Duration,
        ready: &watch::Sender<bool>,
    ) -> Result<ConnectionEnd, JoinError> {
        let sync = self.sync.clone();
        let key = self.key.clone();
        let cursor = match spawn_blocking(move || sync.cursor(&key.context, &key.source)).await? {
            Ok(cursor) => cursor,
            Err(_) => return Ok(ConnectionEnd::Retry),
        };
        let auth = self.sync.auth();
        let access = auth.access_token_for_generation(
            &self.key.context,
            self.source.service(),
            self.session.generation,
        );
        let token = match until_cancelled(cancelled, access).await {
            Some(Ok(token)) => token,
            Some(Err(AuthError::SessionUnavailable | AuthError::ReauthenticationRequired))
            | None => {
                return Ok(ConnectionEnd::Stopped);
            }
            Some(Err(_)) => return Ok(ConnectionEnd::Retry),
        };
        // Получение токена могло сменить его версию. Тогда подключаться будет уже новая задача.
        let status = auth
            .snapshot()
            .into_iter()
            .find(|session| {
                session.context == self.session.context
                    && session.generation == self.session.generation
                    && session.token_version == self.session.token_version
            })
            .map(|session| session.status);
        match status {
            Some(SessionStatus::Active) => {}
            Some(SessionStatus::TemporarilyUnavailable) => return Ok(ConnectionEnd::Retry),
            _ => return Ok(ConnectionEnd::Stopped),
        }
        let session = SourceSession {
            context: self.key.context.clone(),
            generation: self.session.generation,
            token_version: self.session.token_version,
            cursor,
            access_token: token,
        };
        let mut stream = match until_cancelled(cancelled, self.source.connect(session)).await {
            Some(Ok(stream)) => stream,
            Some(Err(error)) => return Ok(self.source_failure(error)),
            None => return Ok(ConnectionEnd::Stopped),
        };
        self.connected.store(true, Ordering::Relaxed);
        loop {
            let event = match until_cancelled(cancelled, stream.next()).await {
                Some(Some(Ok(event))) => event,
                Some(Some(Err(error))) => return Ok(self.source_failure(error)),
                Some(None) => return Ok(ConnectionEnd::Retry),
                None => return Ok(ConnectionEnd::Stopped),
            };
            let sync = self.sync.clone();
            let key = self.key.clone();
            let generation = self.session.generation;
            let caught_up = matches!(event, Event::UnreadBaseline { .. } | Event::SourceReady(_));
            // Ждём начатую запись, чтобы следующая задача не прочитала старую позицию.
            let applied =
                spawn_blocking(move || sync.apply(&key.context, generation, &key.source, event))
                    .await?;
            match applied {
                Ok(_) => {
                    *delay = INITIAL_RETRY_DELAY;
                    if caught_up {
                        ready.send_if_modified(|state| !std::mem::replace(state, true));
                    }
                }
                Err(SyncError::Auth(_)) => return Ok(ConnectionEnd::Stopped),
                Err(SyncError::Storage(StorageError::EpochChanged)) => {
                    return Ok(ConnectionEnd::Recover);
                }
                Err(SyncError::Storage(error @ StorageError::CursorVersionOutOfRange)) => {
                    log::warn!("{} paused: {error}", self.key.source);
                    return Ok(ConnectionEnd::CursorRejected);
                }
                Err(SyncError::Storage(_)) => return Ok(ConnectionEnd::Retry),
            }
        }
    }

    /// Решает, можно ли повторить подключение, и сбрасывает токен при отказе сервера.
    fn source_failure(&self, error: SourceError) -> ConnectionEnd {
        match error {
            SourceError::Unauthorized => {
                self.sync.auth().reject_access_token(
                    &self.key.context,
                    self.session.generation,
                    self.session.token_version,
                );
                ConnectionEnd::Retry
            }
            SourceError::Temporary => ConnectionEnd::Retry,
            SourceError::CursorPruned => ConnectionEnd::Recover,
            SourceError::InvalidResponse | SourceError::Forbidden => {
                log::warn!("{} paused: {error}", self.key.source);
                ConnectionEnd::CursorRejected
            }
        }
    }
}

/// Решение о том, что делать после завершения попытки чтения.
enum ConnectionEnd {
    Stopped,        // Завершить задачу.
    Retry,          // Подождать и подключиться снова.
    Recover,        // Сбросить непригодную позицию перед повтором.
    CursorRejected, // Приостановить чтение до отмены задачи. Простое переподключение не поможет.
}

/// Ждёт операцию или отмену. При отмене возвращает `None`.
async fn until_cancelled<T>(
    cancelled: &mut oneshot::Receiver<()>,
    operation: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = cancelled => None,
        result = operation => Some(result),
    }
}
