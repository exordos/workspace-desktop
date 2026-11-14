//! Собирает порядок запуска Workspace без привязки к окну или терминалу.
//! Запускает восстановление сессий, каталоги чатов и фоновую работу, затем дожидается остановки.

use std::{collections::HashMap, future::Future, sync::Arc};

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    auth::{Auth, AuthError, SessionSnapshot, SessionStatus},
    context::{OwnerContext, SessionGeneration},
    messenger::catalog::{Catalog, CatalogError, Chat},
    runtime::{Runtime, RuntimeError},
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
        let mut current = HashMap::<OwnerContext, ChatList>::new();

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
                    for (owner, session) in sessions.into_iter()
                        .filter(|(_, session)| session.status == SessionStatus::Active)
                    {
                        current.entry(owner).or_insert_with(|| {
                            let cached = catalog.cached(&session.context)
                                .inspect_err(|error| log::warn!("Не удалось прочитать список чатов: {error}"))
                                .ok().flatten().map(Arc::from);
                            let list = ChatList {
                                generation: session.generation,
                                chats: cached,
                                status: ChatListStatus::Loading,
                            };
                            pending.push(load(&catalog, auth, session));
                            list
                        });
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
                Some((session, result)) = pending.next(), if !pending.is_empty() => {
                    if let Some(list) = current.get_mut(&session.context)
                        && list.generation == session.generation
                    {
                        list.status = match result {
                            Ok(chats) => {
                                list.chats = Some(Arc::from(chats));
                                ChatListStatus::Ready
                            }
                            Err(error) => {
                                log::warn!("Не удалось обновить список чатов: {error}");
                                ChatListStatus::Failed
                            }
                        };
                    }
                }
            }
            lists.send_replace(current.clone());
        }
    }
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
