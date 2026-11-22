//! Собирает порядок запуска Workspace без привязки к окну или терминалу.
//! Запускает восстановление сессий, каталоги чатов и фоновую работу, затем дожидается остановки.

use std::{collections::HashMap, future::Future, sync::Arc};

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    auth::{Auth, AuthError, SessionSnapshot, SessionStatus},
    context::{OwnerContext, SessionGeneration},
    messenger::catalog::{Catalog, CatalogError, Chat, SidebarPreviews, attach_previews},
    runtime::{Runtime, RuntimeError},
    storage::StorageError,
};

/// Последнее состояние каталога для одного входа в Workspace.
#[derive(Clone, Debug)]
pub struct ChatList {
    pub generation: SessionGeneration, // Вход, к которому относится состояние.
    pub chats: Option<Arc<[Chat]>>,    // Снимок, если его удалось получить из SQLite или сервера.
    pub status: ChatListStatus,        // Итог текущей попытки загрузки.
}

/// Состояние запроса на обновление списка чатов.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatListStatus {
    Loading, // Запрос выполняется.
    Ready,   // Получен свежий снимок.
    Failed,  // Доступен повтор запроса.
}

/// Списки чатов, которые оболочка получает одним снимком.
pub type ChatLists = HashMap<OwnerContext, ChatList>;

/// Даёт окну подписку на каталоги и отправляет команды повтора.
pub struct ChatCatalog {
    lists: watch::Receiver<ChatLists>, // Текущие состояния каталогов доступных аккаунтов.
    retry: mpsc::Sender<(OwnerContext, SessionGeneration)>, // Очередь повторов для Bootstrap.
}

impl ChatCatalog {
    /// Даёт оболочке текущий снимок и уведомления об изменениях.
    pub fn subscribe(&self) -> watch::Receiver<ChatLists> {
        self.lists.clone()
    }

    /// Ставит повтор для указанного входа в очередь и сообщает, если очередь недоступна.
    pub fn retry(&self, owner: OwnerContext, generation: SessionGeneration) -> bool {
        self.retry.try_send((owner, generation)).is_ok()
    }
}

/// Даёт Bootstrap каталог Messenger и каналы для связи с окном.
pub struct CatalogStartup {
    catalog: Catalog,                // Модуль, который читает и сохраняет списки.
    lists: watch::Sender<ChatLists>, // Канал снимков для окна.
    retry: mpsc::Receiver<(OwnerContext, SessionGeneration)>, // Команды повтора от окна.
}

/// Подключает каталог к запуску приложения и возвращает интерфейс для окна.
pub fn chat_catalog(catalog: Catalog) -> (ChatCatalog, CatalogStartup) {
    let (publish, lists) = watch::channel(HashMap::new());
    let (retry, requests) = mpsc::channel(16);
    (
        ChatCatalog { lists, retry },
        CatalogStartup {
            catalog,
            lists: publish,
            retry: requests,
        },
    )
}

impl CatalogStartup {
    /// Начинает загрузку для каждого активного входа и принимает запросы на повтор.
    async fn run(self, auth: &Auth) {
        let Self {
            catalog,
            lists,
            mut retry,
        } = self;
        let mut sessions = Box::pin(auth.subscribe());
        let mut pending = FuturesUnordered::new();
        let mut pending_previews = FuturesUnordered::new();
        let mut current = HashMap::<OwnerContext, ChatList>::new();
        let mut previews = HashMap::<OwnerContext, SidebarPreviews>::new();
        let mut catalog_changes = catalog.changes();
        let mut catalog_checkpoints = catalog.checkpoints();

        loop {
            tokio::select! {
                snapshot = sessions.next() => {
                    let Some(snapshot) = snapshot else { return; };
                    // Снимок привязан к входу. После выхода старый ответ не попадёт в окно.
                    let sessions: HashMap<_, _> = snapshot.into_iter()
                        .map(|session| (session.context.clone(), session))
                        .collect();
                    current.retain(|owner, list| sessions.get(owner).is_some_and(|session|
                        session.generation == list.generation && matches!(
                            session.status, SessionStatus::Active | SessionStatus::TemporarilyUnavailable
                        )));
                    previews.retain(|owner, _| current.contains_key(owner));
                    for (owner, session) in sessions.into_iter()
                        .filter(|(_, session)| session.status == SessionStatus::Active)
                    {
                        if let Some(list) = current.get_mut(&owner) {
                            // Курсор мог появиться, пока сессия не могла выполнять запросы.
                            if begin_reconciliation(&catalog, &session, list) {
                                pending.push(load(&catalog, auth, session));
                            }
                            continue;
                        }
                        let cached = catalog.cached(&owner)
                            .inspect_err(|error| log::warn!("Не удалось прочитать список чатов: {error}"))
                            .ok().flatten().map(Arc::from);
                        current.insert(owner, ChatList {
                            generation: session.generation,
                            chats: cached,
                            status: ChatListStatus::Loading,
                        });
                        pending.push(load(&catalog, auth, session));
                    }
                }
                request = retry.recv() => {
                    let Some((owner, generation)) = request else { return; };
                    if let Some(list) = current.get_mut(&owner)
                        && list.generation == generation && list.status == ChatListStatus::Failed
                        && let Some(session) = auth.snapshot().into_iter().find(|session|
                            session.context == owner && session.generation == generation
                            && session.status == SessionStatus::Active)
                    {
                        list.status = ChatListStatus::Loading;
                        pending.push(load(&catalog, auth, session));
                    }
                }
                changed = catalog_changes.changed() => {
                    if changed.is_err() { return; }
                    // Публикация ниже перечитает снимки, зафиксированные вместе с событием.
                }
                checkpoint = catalog_checkpoints.changed() => {
                    if checkpoint.is_err() { return; }
                    for session in auth.snapshot().into_iter()
                        .filter(|session| session.status == SessionStatus::Active)
                    {
                        if let Some(list) = current.get_mut(&session.context)
                            && begin_reconciliation(&catalog, &session, list)
                        {
                            pending.push(load(&catalog, auth, session));
                        }
                    }
                }
                Some((session, result)) = pending.next(), if !pending.is_empty() => {
                    if let Some(list) = current.get_mut(&session.context)
                        && list.generation == session.generation
                    {
                        list.status = match result {
                            Ok(_) if needs_reconciliation(&catalog, &session) => {
                                // Первый курсор мог появиться между сохранением и обработкой ответа.
                                pending.push(load(&catalog, auth, session));
                                ChatListStatus::Loading
                            }
                            Ok(chats) => {
                                previews.remove(&session.context);
                                pending_previews.push(load_previews(&catalog, auth, session, chats));
                                ChatListStatus::Ready
                            }
                            Err(CatalogError::Storage(StorageError::CatalogBeforeCheckpoint)) => {
                                pending.push(load(&catalog, auth, session));
                                ChatListStatus::Loading
                            }
                            Err(CatalogError::Storage(StorageError::StaleCatalog))
                                if needs_reconciliation(&catalog, &session) =>
                            {
                                pending.push(load(&catalog, auth, session));
                                ChatListStatus::Loading
                            }
                            Err(error) => {
                                log::warn!("Не удалось обновить список чатов: {error}");
                                ChatListStatus::Failed
                            }
                        };
                    }
                }
                Some((session, result)) = pending_previews.next(), if !pending_previews.is_empty() => {
                    if current.get(&session.context).is_some_and(|list| list.generation == session.generation)
                        && let Ok(loaded) = result
                    {
                        previews.insert(session.context, loaded);
                    }
                }
            }
            if let Err(error) = publish_catalog(&catalog, &mut current, &previews, &lists) {
                log::warn!("Не удалось опубликовать список чатов: {error}");
            }
        }
    }
}

/// Начинает сверку для текущего входа, если список уже загружался и REST отстал от журнала.
fn begin_reconciliation(catalog: &Catalog, session: &SessionSnapshot, list: &mut ChatList) -> bool {
    if list.generation == session.generation
        && list.status != ChatListStatus::Loading
        && needs_reconciliation(catalog, session)
    {
        list.status = ChatListStatus::Loading;
        return true;
    }
    false
}

/// Проверяет, нужно ли один раз получить список после появления первого курсора.
fn needs_reconciliation(catalog: &Catalog, session: &SessionSnapshot) -> bool {
    catalog
        .needs_reconciliation(&session.context)
        .inspect_err(|error| log::warn!("Не удалось сверить позицию каталога: {error}"))
        .unwrap_or(false)
}

/// Публикует снимки, перечитанные под общей с записью отзыва блокировкой базы.
pub(crate) fn publish_catalog(
    catalog: &Catalog,
    current: &mut ChatLists,
    previews: &HashMap<OwnerContext, SidebarPreviews>,
    lists: &watch::Sender<ChatLists>,
) -> Result<(), crate::storage::StorageError> {
    let owners: Vec<_> = current.keys().cloned().collect();
    catalog.with_snapshots(&owners, |snapshots| {
        for (owner, chats) in owners.iter().zip(snapshots) {
            let list = current
                .get_mut(owner)
                .expect("владелец взят из текущего снимка");
            let chats = chats.map(|mut chats| {
                if let Some(previews) = previews.get(owner) {
                    attach_previews(owner, &mut chats, previews);
                }
                chats
            });
            if list.chats.as_deref() != chats.as_deref() {
                list.chats = chats.map(Arc::from);
            }
        }
        lists.send_replace(current.clone());
    })
}

/// Загружает каталог и возвращает вход, к которому относится ответ.
async fn load(
    catalog: &Catalog,
    auth: &Auth,
    session: SessionSnapshot,
) -> (SessionSnapshot, Result<Vec<Chat>, CatalogError>) {
    let result = catalog.refresh(auth, &session).await;
    (session, result)
}

/// Загружает превью отдельно от списка и возвращает их только для исходного входа.
async fn load_previews(
    catalog: &Catalog,
    auth: &Auth,
    session: SessionSnapshot,
    chats: Vec<Chat>,
) -> (SessionSnapshot, Result<SidebarPreviews, CatalogError>) {
    let previews = catalog.load_previews(auth, &session, chats).await;
    (session, previews)
}

/// Запускает фоновую работу и восстановление сохранённых сессий до сигнала остановки.
/// Если окно подключило каталог, загружает список чатов после активации каждой сессии.
/// `after_restore` получает итог восстановления. Если он вернёт ошибку, сначала останавливаем фоновые задачи.
pub async fn run<E: From<RuntimeError>>(
    runtime: Runtime,
    catalog: Option<CatalogStartup>,
    shutdown: impl Future<Output = ()>,
    after_restore: impl FnOnce(Result<(), AuthError>) -> Result<(), E>,
) -> Result<(), E> {
    run_with_wake(runtime, catalog, None, shutdown, after_restore).await
}

/// Запускает тот же порядок загрузки и передаёт Runtime сигналы пробуждения от окна.
pub async fn run_with_wake<E: From<RuntimeError>>(
    runtime: Runtime,
    catalog: Option<CatalogStartup>,
    wake: Option<mpsc::Receiver<()>>,
    shutdown: impl Future<Output = ()>,
    after_restore: impl FnOnce(Result<(), AuthError>) -> Result<(), E>,
) -> Result<(), E> {
    let auth = runtime.auth().clone();
    let (stop, stopped) = oneshot::channel();
    let background = runtime.run_with_wake(
        async {
            let _ = stopped.await;
        },
        wake,
    );
    tokio::pin!(background, shutdown);

    // Runtime может запустить задачи первого готового аккаунта, пока остальные ещё восстанавливаются.
    let startup = async {
        tokio::select! {
            biased;
            _ = &mut shutdown => Ok(()),
            restored = auth.restore() => {
                after_restore(restored)?;
                shutdown.await;
                Ok(())
            }
        }
    };
    let startup = async {
        let Some(catalog) = catalog else {
            return startup.await;
        };
        tokio::pin!(startup);
        tokio::select! {
            result = &mut startup => result,
            () = catalog.run(&auth) => startup.await,
        }
    };

    tokio::select! {
        result = &mut background => result.map_err(E::from),
        result = startup => {
            let _ = stop.send(());
            background.await?;
            result
        }
    }
}
