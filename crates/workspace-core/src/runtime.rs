use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use futures::{Stream, StreamExt, future::BoxFuture};
use thiserror::Error;
use tokio::{
    sync::oneshot,
    task::{JoinError, JoinSet, spawn_blocking},
    time::sleep,
};

use crate::{
    auth::{AuthError, Service, SessionSnapshot, SessionStatus},
    context::{OwnerContext, SessionGeneration},
    storage::{Cursor, StorageError},
    sync::{Event, Sync, SyncError},
};

const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);

pub type EventStream = Pin<Box<dyn Stream<Item = Result<Event, SourceError>> + Send>>;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("event source is unavailable")]
pub struct SourceError;

pub struct SourceSession {
    pub context: OwnerContext,
    pub generation: SessionGeneration,
    pub token_version: u64,
    pub cursor: Option<Cursor>,
    pub access_token: String,
}

pub trait EventSource: Send + std::marker::Sync {
    fn id(&self) -> &str;

    fn service(&self) -> Service {
        Service::Messenger
    }

    /// The runtime reconnects after errors or end of stream. Dropping this
    /// future or its returned stream must release the source's connection.
    fn connect(&self, session: SourceSession) -> BoxFuture<'_, Result<EventStream, SourceError>>;
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("duplicate event source: {0}")]
    DuplicateSource(String),
    #[error("background worker failed: {0}")]
    Worker(#[from] JoinError),
}

pub struct Runtime {
    sync: Arc<Sync>,
    sources: HashMap<String, Arc<dyn EventSource>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TaskKey {
    context: OwnerContext,
    source: String,
}

struct Task {
    generation: SessionGeneration,
    token_version: u64,
    cancel: Option<oneshot::Sender<()>>,
}

impl Task {
    fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

type WorkerResult = (TaskKey, Result<(), JoinError>);

impl Runtime {
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

    /// Signal shutdown and await this future to join all workers and pending
    /// writes. Dropping the future aborts workers without awaiting writes.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<(), RuntimeError> {
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
                    let worker = Worker {
                        sync: self.sync.clone(),
                        source: source.clone(),
                        key: key.clone(),
                        session: session.clone(),
                    };
                    workers.spawn(async move { (key, worker.run(cancelled).await) });
                    Task {
                        generation: session.generation,
                        token_version: session.token_version,
                        cancel: Some(cancel),
                    }
                });
            }
        }
    }
}

struct Worker {
    sync: Arc<Sync>,
    source: Arc<dyn EventSource>,
    key: TaskKey,
    session: SessionSnapshot,
}

impl Worker {
    async fn run(self, mut cancelled: oneshot::Receiver<()>) -> Result<(), JoinError> {
        let mut delay = INITIAL_RETRY_DELAY;
        loop {
            match self.consume(&mut cancelled, &mut delay).await? {
                ConnectionEnd::Stopped => break,
                ConnectionEnd::CursorRejected => {
                    // Wait for replacement or shutdown instead of repeatedly
                    // reconnecting at a cursor the source cannot advance.
                    let _ = cancelled.await;
                    break;
                }
                ConnectionEnd::Retry => {
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

    async fn consume(
        &self,
        cancelled: &mut oneshot::Receiver<()>,
        delay: &mut Duration,
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
        // Obtaining a token can advance its version and trigger reconciliation.
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
            Some(Err(_)) => return Ok(ConnectionEnd::Retry),
            None => return Ok(ConnectionEnd::Stopped),
        };
        loop {
            let event = match until_cancelled(cancelled, stream.next()).await {
                Some(Some(Ok(event))) => event,
                Some(Some(Err(_)) | None) => return Ok(ConnectionEnd::Retry),
                None => return Ok(ConnectionEnd::Stopped),
            };
            let sync = self.sync.clone();
            let key = self.key.clone();
            let generation = self.session.generation;
            // Await an already submitted write before releasing the worker's key.
            let applied =
                spawn_blocking(move || sync.apply(&key.context, generation, &key.source, event))
                    .await?;
            match applied {
                Ok(_) => *delay = INITIAL_RETRY_DELAY,
                Err(SyncError::Auth(_)) => return Ok(ConnectionEnd::Stopped),
                Err(SyncError::Storage(
                    StorageError::EpochChanged | StorageError::CursorVersionOutOfRange,
                )) => {
                    return Ok(ConnectionEnd::CursorRejected);
                }
                Err(SyncError::Storage(_)) => return Ok(ConnectionEnd::Retry),
            }
        }
    }
}

enum ConnectionEnd {
    Stopped,
    Retry,
    CursorRejected,
}

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
