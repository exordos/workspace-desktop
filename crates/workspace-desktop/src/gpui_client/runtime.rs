//! Запускает Bootstrap вне потока GPUI и связывает его с окном.
//! Сначала описаны команды и подписка окна, затем настройка профиля и фоновый цикл.

mod folders;
mod login;
mod new_chat_catalog;
mod new_chat_creation;

pub use login::{LoginAttempt, LoginError};

use std::{
    fmt,
    future::Future,
    io,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use futures::{Stream, StreamExt, stream::FuturesUnordered};
use gpui::{App, Context, Entity, Task};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use uuid::Uuid;
use workspace_core::{
    auth::{
        AccountRecord, AccountStore, Auth, SecretStore, SessionSnapshot, SessionStatus, StoreError,
    },
    bootstrap::{self, CatalogStartup, ChatCatalog, ChatLists},
    context::{AccountId, OwnerContext, SessionGeneration},
    directory::{AvatarState, AvatarStore},
    iam_http::WorkspaceIamHttp,
    messenger::{MessengerSource, folders::Folders},
    runtime::{Runtime, RuntimeError},
    storage::{HistoryScope, Storage, media::MediaCachePolicy},
    sync::{Sync, SyncChange, SyncUpdate},
};

use super::contracts::{
    AccountSession, AccountSnapshot, ChatEntry, ChatRoute, ConversationAddress, ConversationReply,
    NotificationReply, NotificationRequest, NotificationRouteError, RuntimeEvent, RuntimeExit,
};
use super::test_profile::{MemorySecrets, TestLogin};
use crate::platform::{JsonAccountStore, MacKeychain, state_directory};

use folders::{FolderPreferenceStore, folder_snapshot, send_folder_state};

const EVENT_QUEUE: usize = 32;
const REQUEST_QUEUE: usize = 32;
const ACTIVE_REQUESTS: usize = 16;
static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

type JobFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type Job = Box<dyn FnOnce(WorkerServices) -> JobFuture + Send>;

/// Безопасное описание этапа, на котором остановился запуск.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupError {
    Profile,   // Не удалось определить или открыть каталог профиля.
    Database,  // Не удалось открыть SQLite.
    Worker,    // Не удалось настроить или запустить фонового исполнителя.
    TestLogin, // Не удалось войти на тестовый стенд.
}

impl fmt::Display for StartupError {
    /// Даёт текст для окна без путей профиля, сетевых адресов и содержимого ошибок.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Profile => "Не удалось открыть профиль Workspace",
            Self::Database => "Не удалось открыть базу Workspace",
            Self::Worker => "Не удалось запустить фоновую работу Workspace",
            Self::TestLogin => {
                "Не удалось войти в тестовый Workspace. Причина указана в журнале запуска."
            }
        })
    }
}

impl std::error::Error for StartupError {}

/// Причина, по которой штатный запрос для экрана не закончен.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestError {
    Busy,    // Очередь занята, операцию можно повторить после получения ответов.
    Stopped, // Фоновая работа остановлена или ответ уже не будет получен.
}

/// Безопасная причина, по которой окно не смогло подтвердить выход.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LogoutError {
    Worker, // Фоновая работа недоступна.
    Auth,   // Core отклонил сессию либо не закончил очистку её секретов.
}

/// Отдаёт подписку Core или ожидание восстановления выбранного входа.
pub(super) enum AvatarSubscription {
    Ready(watch::Receiver<AvatarState>), // Изменения аватара в действующем входе.
    Waiting(futures::future::BoxFuture<'static, bool>), // Истина разрешает попробовать снова.
}

/// Вход в фоновые службы для истории, редактора и системных действий.
#[derive(Clone)]
pub struct RuntimeClient {
    pub(super) drafts: Arc<super::composer::DraftBuffer>, // Последний ввод тем, ожидающих диска.
    avatar_runtime_id: u64, // Отделяет изображения разных запусков рабочего профиля в одном App.
    commands: mpsc::Sender<Job>, // Ограниченная очередь запросов экрана.
    wake: mpsc::Sender<()>, // Несколько сигналов пробуждения можно объединить.
    stop: watch::Sender<bool>, // Истина запрещает новые запросы и будит цикл остановки.
    started: watch::Receiver<Option<Result<(), StartupError>>>, // Итог инициализации; None, пока она идёт.
    ended: watch::Receiver<Option<RuntimeExit>>, // Итог остановки; None, пока исполнитель работает.
    clear_cache: Arc<AtomicBool>, // После остановки убрать данные мессенджера под блокировкой профиля.
}

impl RuntimeClient {
    /// Готовит каналы и запускает чтение рабочего профиля на отдельном потоке.
    pub fn start() -> io::Result<(Self, RuntimeEvents)> {
        Self::start_using(|| {
            let directory = state_directory().map_err(|_| StartupError::Profile)?;
            WorkerParts::open(directory, None)
        })
    }

    /// Запускает тот же цикл с явно выбранным каталогом, например для проверки на отдельном профиле.
    pub fn start_in(directory: PathBuf) -> io::Result<(Self, RuntimeEvents)> {
        Self::start_using(move || WorkerParts::open(directory, None))
    }

    /// Запускает обычное окно после входа на стенд с секретами в памяти процесса.
    pub(super) fn start_test_profile(
        directory: PathBuf,
        login: TestLogin,
    ) -> io::Result<(Self, RuntimeEvents)> {
        Self::start_using(move || WorkerParts::open(directory, Some(login)))
    }

    /// Собирает рабочую подписку с отдельной базой и тестовым центром сертификации.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn start_for_test(
        directory: PathBuf,
        account: AccountRecord,
        iam: Arc<dyn workspace_core::auth::IamClient>,
        secrets: Arc<dyn workspace_core::auth::SecretStore>,
        ca: Vec<u8>,
    ) -> io::Result<(Self, RuntimeEvents)> {
        Self::start_using(move || {
            let (accounts, released) =
                ProfileAccounts::open(&directory).map_err(|_| StartupError::Profile)?;
            accounts.save(&account).map_err(|_| StartupError::Profile)?;
            let auth = Auth::new(iam, Arc::new(accounts), secrets);
            let storage = Storage::open(directory.join("messenger.sqlite3"))
                .map_err(|_| StartupError::Database)?;
            let messenger =
                MessengerSource::with_private_ca(&ca).map_err(|_| StartupError::Worker)?;
            let storage = Arc::new(storage);
            let sync = Sync::with_media_cache(
                auth.clone(),
                storage.clone(),
                directory.join("messenger-media"),
                MediaCachePolicy::default(),
            )
            .map_err(|_| StartupError::Profile)?;
            let mut parts =
                WorkerParts::with_sync(auth, storage, Arc::new(messenger), sync, |services| {
                    Runtime::new(services.sync.clone(), vec![services.messenger.clone()])
                })?;
            parts.services.folder_preferences = Arc::new(FolderPreferenceStore::open(&directory));
            parts.cache_directory = Some(directory);
            Ok((parts, released))
        })
    }

    /// Передаёт настройку на рабочий поток; на стороне окна создаются только каналы и поток.
    fn start_using(
        setup: impl FnOnce() -> Result<(WorkerParts, oneshot::Receiver<()>), StartupError>
        + Send
        + 'static,
    ) -> io::Result<(Self, RuntimeEvents)> {
        let (commands, requests) = mpsc::channel(REQUEST_QUEUE);
        let (events, incoming) = mpsc::channel(EVENT_QUEUE);
        let (wake, wake_signals) = mpsc::channel(1);
        let (stop, stopping) = watch::channel(false);
        let (publish_start, started) = watch::channel(None);
        let (publish_exit, ended) = watch::channel(None);

        let drafts = Arc::new(super::composer::DraftBuffer::default());
        let worker_drafts = drafts.clone();
        let clear_cache = Arc::new(AtomicBool::new(false));
        let worker_clear_cache = clear_cache.clone();
        thread::Builder::new()
            .name("workspace-gpui-sync".into())
            .spawn(move || {
                let drive = || -> Result<RuntimeExit, StartupError> {
                    let executor = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| StartupError::Worker)?;
                    let (mut parts, profile_released) = setup()?;
                    parts.services.drafts = worker_drafts;
                    let cache_directory = parts.cache_directory.take();
                    // Поздние ответы ещё должны уметь отменить попытку, пока Tokio ждёт
                    // окончания начатых блокирующих запросов.
                    let login_auth = Arc::clone(&parts.services.auth);
                    let exit = executor.block_on(parts.run(
                        publish_start.clone(),
                        requests,
                        events,
                        wake_signals,
                        stopping,
                    ));
                    drop(executor);
                    let reset_ok = !worker_clear_cache.load(Ordering::Acquire)
                        || cache_directory
                            .as_deref()
                            .is_some_and(|directory| remove_cache(directory).is_ok());
                    drop(login_auth);
                    // Auth продлевает токены в отдельных потоках. Пока такой поток держит
                    // профиль и может писать в Keychain, выход ещё не подтверждён.
                    let _ = profile_released.blocking_recv();
                    Ok(if reset_ok { exit } else { RuntimeExit::Failed })
                };
                let exit = match drive() {
                    Ok(exit) => exit,
                    Err(error) => {
                        publish_start.send_replace(Some(Err(error)));
                        RuntimeExit::Failed
                    }
                };
                publish_exit.send_replace(Some(exit));
            })?;

        Ok((
            Self {
                drafts,
                avatar_runtime_id: NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed),
                commands,
                wake,
                stop,
                started,
                ended: ended.clone(),
                clear_cache,
            },
            RuntimeEvents {
                incoming,
                ended,
                exit_delivered: false,
            },
        ))
    }

    /// Ждёт открытия профиля и подготовки подписок. Запросы сервера после этого идут независимо.
    pub async fn ready(&self) -> Result<(), StartupError> {
        let mut started = self.started.clone();
        loop {
            if let Some(result) = *started.borrow_and_update() {
                return result;
            }
            if started.changed().await.is_err() {
                return Err(StartupError::Worker);
            }
        }
    }

    /// Даёт адаптеру macOS отправитель сигнала после пробуждения.
    pub fn wake_sender(&self) -> mpsc::Sender<()> {
        self.wake.clone()
    }

    /// Отменяет фоновую работу и ждёт Bootstrap, начатые записи и освобождение профиля.
    pub async fn shutdown(&self) -> RuntimeExit {
        self.stop.send_replace(true);
        wait_for_exit(&mut self.ended.clone()).await
    }

    /// Останавливает фоновые задачи и стирает только кэш и вид папок в закрытом профиле.
    pub(super) async fn reset_cache(&self) -> RuntimeExit {
        self.clear_cache.store(true, Ordering::Release);
        self.shutdown().await
    }

    /// Передаёт Core выход по выбранному поколению. Начатый отзыв доводится до конца.
    pub(super) async fn logout(&self, session: AccountSession) -> Result<(), LogoutError> {
        let reply = self
            .enqueue(move |services, reply| async move {
                let result = tokio::task::spawn_blocking(move || {
                    services
                        .auth
                        .logout_generation(&session.owner, session.generation)
                })
                .await
                .map_err(|_| LogoutError::Worker)
                .and_then(|result| result.map_err(|_| LogoutError::Auth));
                let _ = reply.send(result);
            })
            .map_err(|_| LogoutError::Worker)?;
        reply.await.map_err(|_| LogoutError::Worker)?
    }

    /// Даёт ключ общего исполнителя всем копиям клиентского дескриптора.
    pub(super) fn avatar_runtime_id(&self) -> u64 {
        self.avatar_runtime_id
    }

    /// Создаёт подписку в Core и сразу освобождает место в очереди фонового потока.
    pub(super) async fn subscribe_avatar(
        &self,
        session: AccountSession,
        user_id: Uuid,
    ) -> Result<AvatarSubscription, RequestError> {
        self.request(move |services| async move {
            match AvatarStore::new(services.sync, services.messenger).subscribe(
                &session.owner,
                session.generation,
                user_id,
            ) {
                Ok(receiver) => AvatarSubscription::Ready(receiver),
                Err(_) => {
                    let mut updates = Box::pin(services.auth.subscribe());
                    // Ожидание живёт у подписчика GPUI и не занимает очередь команд.
                    AvatarSubscription::Waiting(Box::pin(async move {
                        while let Some(accounts) = updates.next().await {
                            let status = accounts
                                .iter()
                                .find(|account| {
                                    account.context == session.owner
                                        && account.generation == session.generation
                                })
                                .map(|account| account.status);
                            match status {
                                Some(SessionStatus::Active) => return true,
                                Some(
                                    SessionStatus::Restoring
                                    | SessionStatus::TemporarilyUnavailable,
                                ) => {}
                                _ => return false,
                            }
                        }
                        false
                    }))
                }
            }
        })
        .await
    }

    /// Ставит запрос в ограниченную очередь. Службы доступны только в фоне.
    pub async fn request<T, F, Fut>(&self, action: F) -> Result<T, RequestError>
    where
        T: Send + 'static,
        F: FnOnce(WorkerServices) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let value = self.enqueue(move |services, mut reply| async move {
            // При закрытии экрана нет причины продолжать отменяемый сетевой запрос.
            tokio::select! {
                biased;
                _ = reply.closed() => {}
                result = async { action(services).await } => { let _ = reply.send(result); }
            }
        })?;
        value.await.map_err(|_| RequestError::Stopped)
    }

    /// Ставит задачу в общую очередь. Задача сама выбирает, отменяться ли при закрытии получателя.
    fn enqueue<T, F, Fut>(&self, action: F) -> Result<oneshot::Receiver<T>, RequestError>
    where
        T: Send + 'static,
        F: FnOnce(WorkerServices, oneshot::Sender<T>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if *self.stop.borrow() {
            return Err(RequestError::Stopped);
        }
        let (reply, value) = oneshot::channel();
        self.commands
            .try_send(Box::new(move |services| Box::pin(action(services, reply))))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => RequestError::Busy,
                mpsc::error::TrySendError::Closed(_) => RequestError::Stopped,
            })?;
        Ok(value)
    }

    /// Возвращает ответ с тем же адресом, который экран передал перед запуском запроса.
    pub async fn conversation<T, F, Fut>(
        &self,
        address: ConversationAddress,
        action: F,
    ) -> ConversationReply<Result<T, RequestError>>
    where
        T: Send + 'static,
        F: FnOnce(WorkerServices, ConversationAddress) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let original = address.clone();
        let value = self
            .request(move |services| action(services, address))
            .await;
        ConversationReply {
            address: original,
            value,
        }
    }

    /// Просит Bootstrap обновить каталог указанного входа.
    pub async fn retry_catalog(&self, session: AccountSession) -> Result<bool, RequestError> {
        self.request(move |services| async move {
            services.catalog.retry(session.owner, session.generation)
        })
        .await
    }

    /// Проверяет сохранённую цель через Sync и сохраняет ревизию исходного клика.
    pub async fn resolve_notification(&self, request: NotificationRequest) -> NotificationReply {
        let click = request.click.clone();
        let result = self
            .request(move |services| async move {
                let target = services
                    .sync
                    .resolve_notification_target(click.target, click.generation)?;
                let stream_id = target
                    .conversation_id
                    .parse()
                    .map_err(|_| NotificationRouteError::TargetUnavailable)?;
                let message_id = target
                    .message_id
                    .parse()
                    .map_err(|_| NotificationRouteError::TargetUnavailable)?;
                Ok(ChatRoute {
                    owner: target.context,
                    scope: HistoryScope {
                        stream_id,
                        topic_id: None,
                    },
                    entry: ChatEntry::Around(message_id),
                })
            })
            .await
            .unwrap_or(Err(NotificationRouteError::LookupFailed));
        NotificationReply { request, result }
    }
}

/// Единственный получатель событий фоновой работы.
pub struct RuntimeEvents {
    incoming: mpsc::Receiver<RuntimeEvent>, // Снимки и события из ограниченной очереди.
    ended: watch::Receiver<Option<RuntimeExit>>, // Завершение не занимает место в очереди.
    exit_delivered: bool, // Итог передаётся только один раз, затем подписка закрыта.
}

impl RuntimeEvents {
    /// Ждёт следующее событие. После закрытия выдаёт результат остановки и завершает подписку.
    pub async fn recv(&mut self) -> Option<RuntimeEvent> {
        if let Some(event) = self.incoming.recv().await {
            return Some(event);
        }
        if self.exit_delivered {
            return None;
        }
        let exit = wait_for_exit(&mut self.ended).await;
        self.exit_delivered = true;
        Some(RuntimeEvent::Stopped(exit))
    }

    /// Применяет события на главном потоке. Окно должно хранить Task до закрытия.
    pub fn attach<T: 'static>(
        mut self,
        target: &Entity<T>,
        app: &mut App,
        mut apply: impl FnMut(&mut T, RuntimeEvent, &mut Context<T>) + 'static,
    ) -> Task<()> {
        let target = target.downgrade();
        app.spawn(async move |cx| {
            while let Some(event) = self.recv().await {
                if target
                    .update(cx, |target, cx| apply(target, event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
    }
}

/// Службы одного профиля, которые запрос получает на выделенном потоке.
#[derive(Clone)]
pub struct WorkerServices {
    pub(super) drafts: Arc<super::composer::DraftBuffer>, // Локальный ввод, не привязанный к Entity окна.
    pub auth: Arc<Auth>, // Сессии и проверка поколения; окно держит только слабую ссылку для отмены входа.
    pub sync: Arc<Sync>, // История, счётчики, проверка целей и запись отправок.
    pub messenger: Arc<MessengerSource>, // Сетевые запросы к истории и разрешение адреса отправки.
    pub catalog: Arc<ChatCatalog>, // Снимки и повторы загрузки каталога.
    folders: Folders,    // Снимок папок и серверные команды мессенджера.
    folder_preferences: Arc<FolderPreferenceStore>, // Локальные настройки вида по аккаунтам.
    #[cfg(all(test, feature = "gpui-test"))]
    registration_url_for_test: Option<fn(&str) -> Option<String>>, // Подмена публичных настроек для формы.
}

/// Настроенные зависимости одного запуска Bootstrap.
struct WorkerParts {
    services: WorkerServices,         // Входы для запросов окна и подписок.
    runtime: Runtime,                 // Владелец задач синхронизации.
    catalog: CatalogStartup,          // Владелец начальной загрузки каталога.
    cache_directory: Option<PathBuf>, // Каталог для очистки после закрытия SQLite.
}

/// Удаляет только известные локальные данные мессенджера, сохраняя аккаунты и токены.
fn remove_cache(directory: &std::path::Path) -> io::Result<()> {
    let storage = Storage::open(directory.join("messenger.sqlite3")).map_err(io::Error::other)?;
    storage
        .clear_rebuildable_messenger_cache()
        .map_err(io::Error::other)?;
    for file in ["folder-rail.json", "composer-toolbar.json"] {
        match std::fs::remove_file(directory.join(file)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
/// Передаёт обычные операции хранилищу и закрывает сигнал после освобождения файла профиля.
struct ProfileAccounts {
    store: JsonAccountStore, // Общий JSON и блокировка профиля; освобождается первой.
    _release: oneshot::Sender<()>, // Закрывается следом, когда последний владелец Auth ушёл.
}

impl ProfileAccounts {
    /// Открывает профиль и возвращает ожидание освобождения его блокировки.
    fn open(directory: &std::path::Path) -> io::Result<(Self, oneshot::Receiver<()>)> {
        let store = JsonAccountStore::open(directory)?;
        let (_release, released) = oneshot::channel();
        Ok((Self { store, _release }, released))
    }
}

impl AccountStore for ProfileAccounts {
    /// Читает сохранённые аккаунты профиля.
    fn load(&self) -> Result<Vec<AccountRecord>, StoreError> {
        self.store.load()
    }

    /// Сохраняет сведения об аккаунте в общем JSON.
    fn save(&self, record: &AccountRecord) -> Result<(), StoreError> {
        self.store.save(record)
    }

    /// Удаляет завершённый аккаунт из того же файла.
    fn remove(&self, account_id: &AccountId) -> Result<(), StoreError> {
        self.store.remove(account_id)
    }
}

/// Причина завершения Bootstrap, которой достаточно фоновому мосту.
enum BootstrapFailure {
    Restore, // Не удалось прочитать сохранённые сессии.
    Runtime, // Runtime не смог закончить работу штатно.
}

impl From<RuntimeError> for BootstrapFailure {
    /// Скрывает детали внутреннего сообщения от событий для окна.
    fn from(_: RuntimeError) -> Self {
        Self::Runtime
    }
}

impl WorkerParts {
    /// Открывает профиль и выбирает хранилище токенов для обычного или тестового запуска.
    fn open(
        directory: PathBuf,
        test_login: Option<TestLogin>,
    ) -> Result<(Self, oneshot::Receiver<()>), StartupError> {
        let (accounts, released) =
            ProfileAccounts::open(&directory).map_err(|_| StartupError::Profile)?;
        let secrets: Arc<dyn SecretStore> = if test_login.is_some() {
            Arc::new(MemorySecrets::default())
        } else {
            Arc::new(MacKeychain)
        };
        let auth = Auth::new(
            Arc::new(WorkspaceIamHttp::new()),
            Arc::new(accounts),
            secrets,
        );
        let storage = Storage::open(directory.join("messenger.sqlite3"))
            .map_err(|_| StartupError::Database)?;
        let messenger = MessengerSource::new().map_err(|_| StartupError::Worker)?;
        if let Some(login) = test_login {
            login.activate(&auth).map_err(|error| {
                log::error!("event=workspace_test_login result=failed reason={error}");
                StartupError::TestLogin
            })?;
            log::info!("event=workspace_test_login result=active");
        }
        let storage = Arc::new(storage);
        let sync = Sync::with_media_cache(
            auth.clone(),
            storage.clone(),
            directory.join("messenger-media"),
            MediaCachePolicy::default(),
        )
        .unwrap_or_else(|error| {
            log::warn!("Не удалось открыть кэш вложений: {error}");
            Sync::new(auth.clone(), storage.clone())
        });
        let mut parts = Self::with_sync(auth, storage, Arc::new(messenger), sync, |services| {
            Runtime::new(services.sync.clone(), vec![services.messenger.clone()])
        })?;
        parts.services.folder_preferences = Arc::new(FolderPreferenceStore::open(&directory));
        parts.cache_directory = Some(directory);
        Ok((parts, released))
    }

    /// Собирает тестовые сервисы, которым не требуется дисковый кэш вложений.
    #[cfg(test)]
    fn with_sources(
        auth: Auth,
        storage: Arc<Storage>,
        messenger: Arc<MessengerSource>,
        runtime: impl FnOnce(&WorkerServices) -> Result<Runtime, RuntimeError>,
    ) -> Result<Self, StartupError> {
        let sync = Sync::new(auth.clone(), storage.clone());
        Self::with_sync(auth, storage, messenger, sync, runtime)
    }

    /// Собирает сервисы вокруг того же профиля и выбранного источника событий.
    fn with_sync(
        auth: Auth,
        storage: Arc<Storage>,
        messenger: Arc<MessengerSource>,
        sync: Sync,
        runtime: impl FnOnce(&WorkerServices) -> Result<Runtime, RuntimeError>,
    ) -> Result<Self, StartupError> {
        let (catalog, startup) = bootstrap::chat_catalog(messenger.catalog(storage.clone()));
        let folders = messenger.folders(storage.clone());
        let services = WorkerServices {
            drafts: Arc::default(),
            sync: Arc::new(sync),
            auth: Arc::new(auth),
            messenger,
            catalog: Arc::new(catalog),
            folders,
            folder_preferences: Arc::default(),
            #[cfg(all(test, feature = "gpui-test"))]
            registration_url_for_test: None,
        };
        let runtime = runtime(&services).map_err(|_| StartupError::Worker)?;
        Ok(Self {
            services,
            runtime,
            catalog: startup,
            cache_directory: None,
        })
    }

    /// Подписывается перед Bootstrap и прекращает фоновые запросы при закрытии окна.
    async fn run(
        self,
        started: watch::Sender<Option<Result<(), StartupError>>>,
        requests: mpsc::Receiver<Job>,
        events: mpsc::Sender<RuntimeEvent>,
        wake: mpsc::Receiver<()>,
        mut stopping: watch::Receiver<bool>,
    ) -> RuntimeExit {
        // Если окно исчезло ещё при открытии профиля, не начинаем восстановление по сети.
        if *stopping.borrow_and_update() || events.is_closed() || requests.is_closed() {
            return RuntimeExit::Clean;
        }
        // Пустой снимок открывает форму входа. Сначала читаем локальные записи, чтобы
        // окно могло отличить пустой профиль от ещё не начавшегося восстановления.
        if self.services.auth.load_accounts().is_err() {
            started.send_replace(Some(Err(StartupError::Profile)));
            return RuntimeExit::Failed;
        }
        let sessions = self.services.auth.subscribe();
        let catalog_updates = self.services.catalog.subscribe();
        let sync_updates = self.services.sync.subscribe();
        started.send_replace(Some(Ok(())));

        let (stop, stopped) = oneshot::channel();
        let auth = self.services.auth.clone();
        let background = bootstrap::run_with_wake(
            self.runtime,
            Some(self.catalog),
            Some(wake),
            async move {
                let _ = stopped.await;
            },
            |result| -> Result<(), BootstrapFailure> {
                result.map_err(|_| BootstrapFailure::Restore)?;
                let sessions = auth.snapshot();
                let active = sessions
                    .iter()
                    .filter(|session| session.status == SessionStatus::Active)
                    .count();
                log::debug!(
                    "event=workspace_auth_restored active={active} total={}",
                    sessions.len()
                );
                Ok(())
            },
        );
        tokio::pin!(background);

        let transport = async {
            tokio::select! {
                biased;
                () = wait_for_stop(&mut stopping) => {}
                () = events.closed() => {}
                () = forward_events(sessions, catalog_updates, sync_updates, &self.services, &events) => {}
                () = run_requests(requests, &self.services) => {}
            }
        };

        let result = tokio::select! {
            result = &mut background => result,
            () = transport => {
                self.services.drafts.flush(&self.services.sync);
                let _ = stop.send(());
                background.await
            }
        };
        match result {
            Ok(()) => RuntimeExit::Clean,
            Err(_) => RuntimeExit::Failed,
        }
    }
}

/// Ждёт запрос остановки или закрытие последнего управляющего дескриптора.
async fn wait_for_stop(stopping: &mut watch::Receiver<bool>) {
    while !*stopping.borrow_and_update() {
        if stopping.changed().await.is_err() {
            break;
        }
    }
}

/// Ждёт отметку после освобождения ресурсов; неожиданный разрыв считается ошибкой.
async fn wait_for_exit(ended: &mut watch::Receiver<Option<RuntimeExit>>) -> RuntimeExit {
    loop {
        if let Some(exit) = *ended.borrow_and_update() {
            return exit;
        }
        if ended.changed().await.is_err() {
            return RuntimeExit::Failed;
        }
    }
}

/// Выполняет ограниченное число задач; при остановке незавершённые ожидания отменяются.
async fn run_requests(mut requests: mpsc::Receiver<Job>, services: &WorkerServices) {
    let mut active = FuturesUnordered::new();
    loop {
        tokio::select! {
            request = requests.recv(), if active.len() < ACTIVE_REQUESTS => match request {
                Some(request) => active.push(request(services.clone())),
                None => return,
            },
            Some(()) = active.next(), if !active.is_empty() => {}
        }
    }
}

/// Читает актуальный счётчик с диска только для доступного поколения.
fn account_snapshot(sessions: Vec<SessionSnapshot>, sync: &Sync) -> Vec<AccountSnapshot> {
    sessions
        .iter()
        .map(|session| {
            let unread = match session.status {
                SessionStatus::Active | SessionStatus::TemporarilyUnavailable => {
                    sync.unread(&session.context).unwrap_or_else(|_| {
                        log::debug!("event=workspace_unread_snapshot result=unavailable");
                        None
                    })
                }
                _ => None,
            };
            AccountSnapshot::from_session(session, unread)
        })
        .collect()
}

/// Возвращает новое состояние каталога без удержания блокировки во время отправки.
fn catalog_snapshot(catalog: &mut watch::Receiver<ChatLists>) -> RuntimeEvent {
    RuntimeEvent::Catalog(catalog.borrow_and_update().clone())
}

/// Сверяет, что окно уже получило текущий вход с состоянием, пригодным для события.
fn knows_session(
    accounts: &[AccountSnapshot],
    owner: &OwnerContext,
    generation: SessionGeneration,
    allow_temporary: bool,
) -> bool {
    accounts.iter().any(|account| {
        account.session.owner == *owner
            && account.session.generation == generation
            && (account.status == SessionStatus::Active
                || (allow_temporary && account.status == SessionStatus::TemporarilyUnavailable))
    })
}

/// Публикует новый снимок и запоминает, какие входы окно увидит перед следующими событиями.
async fn send_accounts(
    accounts: &mut Vec<AccountSnapshot>,
    sessions: Vec<SessionSnapshot>,
    sync: &Sync,
    events: &mpsc::Sender<RuntimeEvent>,
) -> bool {
    *accounts = account_snapshot(sessions, sync);
    events
        .send(RuntimeEvent::Accounts(accounts.clone()))
        .await
        .is_ok()
}

/// Передаёт начальные снимки, сигналы обновления и восстановление после пропуска событий.
async fn forward_events(
    sessions: impl Stream<Item = Vec<SessionSnapshot>>,
    mut catalog: watch::Receiver<ChatLists>,
    mut updates: broadcast::Receiver<SyncUpdate>,
    services: &WorkerServices,
    events: &mpsc::Sender<RuntimeEvent>,
) {
    let mut accounts = Vec::new();
    let mut folders = services.folders.subscribe();
    let mut preferences = services.folder_preferences.subscribe();
    if !send_accounts(
        &mut accounts,
        services.auth.snapshot(),
        &services.sync,
        events,
    )
    .await
        || events.send(catalog_snapshot(&mut catalog)).await.is_err()
        || !send_folder_state(&accounts, services, events).await
    {
        return;
    }

    futures::pin_mut!(sessions);
    loop {
        let event = tokio::select! {
            snapshot = sessions.next() => {
                let Some(snapshot) = snapshot else { return; };
                if !send_accounts(&mut accounts, snapshot, &services.sync, events).await
                    || !send_folder_state(&accounts, services, events).await
                {
                    return;
                }
                continue;
            }
            changed = catalog.changed() => {
                if changed.is_err() { return; }
                catalog_snapshot(&mut catalog)
            }
            changed = folders.changed() => {
                if changed.is_err() { return; }
                folder_snapshot(&accounts, services)
            }
            changed = preferences.changed() => {
                if changed.is_err() { return; }
                services.folder_preferences.snapshot(&accounts)
            }
            update = updates.recv() => match update {
                Ok(update) => RuntimeEvent::Sync(update),
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if !send_accounts(&mut accounts, services.auth.snapshot(), &services.sync, events).await
                        || events.send(catalog_snapshot(&mut catalog)).await.is_err()
                        || !send_folder_state(&accounts, services, events).await
                    {
                        return;
                    }
                    // Экран перечитает актуальную историю и очередь по собственному адресу.
                    RuntimeEvent::SyncLagged
                }
            }
        };

        // Auth и источники публикуют независимо. Опередивший снимок баннер или каталог
        // получит актуальные сведения о своём входе раньше самого события.
        let needs_accounts = match &event {
            RuntimeEvent::Catalog(lists) => lists
                .iter()
                .any(|(owner, list)| !knows_session(&accounts, owner, list.generation, true)),
            RuntimeEvent::Sync(update) => {
                !knows_session(&accounts, &update.context, update.generation, false)
            }
            _ => false,
        };
        if needs_accounts
            && (!send_accounts(
                &mut accounts,
                services.auth.snapshot(),
                &services.sync,
                events,
            )
            .await
                || !send_folder_state(&accounts, services, events).await)
        {
            return;
        }
        if let RuntimeEvent::Sync(update) = &event
            && !knows_session(&accounts, &update.context, update.generation, false)
        {
            continue;
        }
        let event = refresh_unread_event(event, services);
        if events.send(event).await.is_err() {
            return;
        }
    }
}

/// Перечитывает счётчик для сигналов, которые могли ждать в очереди до восстановления снимка.
fn refresh_unread_event(event: RuntimeEvent, services: &WorkerServices) -> RuntimeEvent {
    let RuntimeEvent::Sync(mut update) = event else {
        return event;
    };
    if matches!(update.change, SyncChange::Unread(_))
        && let Ok(Some(unread)) = services.sync.unread(&update.context)
    {
        update.change = SyncChange::Unread(unread);
    }
    RuntimeEvent::Sync(update)
}

#[cfg(test)]
mod tests;
