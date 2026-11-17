//! Собирает представление папок из одного снимка SQLite.
//! После догонки журнала сверяет помеченные папки и каталог активного аккаунта.

use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::{sync::watch, time::sleep};
use uuid::Uuid;

use crate::{
    auth::{Auth, SessionSnapshot},
    context::{OwnerContext, SessionGeneration},
    storage::StorageError,
    sync::Sync,
};

use super::{
    MessengerSource,
    catalog::Chat,
    folders::{Folder, FolderError, FolderItem, Folders},
};

const BATCH_DELAY: Duration = Duration::from_millis(150);
const INITIAL_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

/// Данные папок и доступных чатов одного входа, прочитанные вместе.
#[derive(Debug)]
pub struct FolderSnapshot {
    pub owner: OwnerContext, // Аккаунт и проект, которым принадлежат данные.
    pub generation: SessionGeneration, // Поколение входа, в котором снимок был прочитан.
    pub folders: Option<Vec<Folder>>, // Полный сохранённый список. None означает отсутствие снимка.
    pub chats: Vec<Chat>,    // Каталог доступных разговоров этого владельца.
    pub all_chats_unread: Option<u32>, // Общий счётчик, который использует Dock.
    pub needs_refresh: bool, // Есть папки, по которым ещё нужна сверка с сервером.
}

/// Строка разговора с настройками именно выбранной папки.
#[derive(Debug)]
pub struct FolderRow<'a> {
    pub chat: &'a Chat,               // Имя, UUID и доступные темы из общего каталога.
    pub item: Option<&'a FolderItem>, // Связь, счётчики и закрепление. None для дополнения «Все чаты».
}

impl FolderSnapshot {
    /// Возвращает подтверждённое непрочитанное, используя для «Все чаты» счётчик Dock.
    pub fn unread_count(&self, folder_id: Uuid) -> Option<u32> {
        if folder_id.is_nil() {
            self.all_chats_unread
        } else {
            self.folder(folder_id).map(|folder| folder.unread_count)
        }
    }

    /// Возвращает строки доступной папки. «Все чаты» дополняет активным каталогом.
    pub fn rows(&self, folder_id: Uuid) -> Option<Vec<FolderRow<'_>>> {
        let folder = self.folder(folder_id);
        if folder.is_none() && !folder_id.is_nil() {
            return None;
        }
        let mut chats: HashMap<_, _> = self.chats.iter().map(|chat| (chat.id, chat)).collect();
        let mut rows: Vec<_> = folder
            .into_iter()
            .flat_map(|folder| &folder.items)
            .filter_map(|item| {
                chats.remove(&item.stream_id).map(|chat| FolderRow {
                    chat,
                    item: Some(item),
                })
            })
            .collect();
        // Внутри групп сохраняем порядок серверного снимка, а дополнение берём в порядке каталога.
        rows.sort_by_key(|row| row.item.is_none_or(|item| item.pinned_at.is_none()));
        if folder_id.is_nil() {
            rows.extend(
                self.chats
                    .iter()
                    .filter(|chat| chats.contains_key(&chat.id))
                    .map(|chat| FolderRow { chat, item: None }),
            );
        }
        Some(rows)
    }

    /// Находит папку в сохранённом полном снимке.
    fn folder(&self, folder_id: Uuid) -> Option<&Folder> {
        self.folders
            .as_ref()?
            .iter()
            .find(|folder| folder.id == folder_id)
    }
}

impl Folders {
    /// Читает папки, доступные разговоры и счётчик под защитой текущего поколения сессии.
    pub fn snapshot(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<FolderSnapshot, FolderError> {
        auth.with_confirmed_generation(owner, generation, || {
            let view = self.storage.folder_view(owner)?;
            let chats = view
                .catalog
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .map_err(StorageError::from)?
                .unwrap_or_default();
            Ok(FolderSnapshot {
                owner: owner.clone(),
                generation,
                folders: view.folders,
                chats,
                all_chats_unread: view.unread,
                needs_refresh: view.needs_refresh,
            })
        })?
    }

    /// Сообщает о фиксации данных. Подписчик перечитывает снимок для своего активного входа.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.storage.folder_changes()
    }

    /// Запрашивает полную серверную сверку после подключения или ошибки.
    pub fn retry(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
    ) -> Result<(), FolderError> {
        auth.with_active_generation(owner, generation, || {
            self.storage.request_folder_refresh(owner, None)
        })??;
        Ok(())
    }
}

/// Дожидается готовности журнала и сверяет папки с ограниченной частотой запросов.
pub(super) async fn run(
    source: &MessengerSource,
    sync: Arc<Sync>,
    session: SessionSnapshot,
    mut ready: watch::Receiver<bool>,
) {
    let folders = source.folders(sync.storage().clone());
    let catalog = source.catalog(sync.storage().clone());
    let auth = sync.auth();
    let mut changes = folders.subscribe();
    let mut refresh_on_start = true;
    let mut retry_delay = INITIAL_RETRY;
    loop {
        let online = *ready.borrow_and_update();
        if !online {
            if ready.changed().await.is_err() {
                return;
            }
            retry_delay = INITIAL_RETRY;
            continue;
        }
        changes.borrow_and_update();
        let (folders_result, catalog_result) = tokio::select! {
            biased;
            result = ready.changed() => {
                if result.is_err() { return; }
                continue;
            }
            result = async {
                tokio::join!(
                    refresh_folders(&folders, auth, &session, &mut refresh_on_start),
                    catalog.refresh_if_needed(auth, &session),
                )
            } => result,
        };

        let folders_result =
            folders_result.inspect_err(|error| log::warn!("Не удалось сверить папки: {error}"));
        let catalog_result = catalog_result
            .inspect_err(|error| log::warn!("Не удалось сверить каталог для папок: {error}"));
        let pause = match (folders_result, catalog_result) {
            (Ok(folders_worked), Ok(catalog_worked)) => {
                retry_delay = INITIAL_RETRY;
                // За один проход обрабатываем одну область, чтобы объединять частые события.
                (folders_worked || catalog_worked).then_some(BATCH_DELAY)
            }
            _ => {
                let pause = retry_delay;
                retry_delay = (retry_delay * 2).min(MAX_RETRY);
                Some(pause)
            }
        };
        tokio::select! {
            result = ready.changed() => if result.is_err() { return; },
            result = changes.changed(), if pause.is_none() => if result.is_err() { return; },
            () = sleep(pause.unwrap_or_default()), if pause.is_some() => {}
        }
    }
}

/// Сверяет одну ожидающую область и записывает ответ только в её исходной ревизии.
async fn refresh_folders(
    folders: &Folders,
    auth: &Auth,
    session: &SessionSnapshot,
    refresh_on_start: &mut bool,
) -> Result<bool, FolderError> {
    let owner = &session.context;
    if *refresh_on_start {
        folders.retry(auth, owner, session.generation)?;
        *refresh_on_start = false;
    }
    let Some(plan) = folders.storage.folder_refresh_plan(owner)? else {
        return Ok(false);
    };
    let rows = match plan.folder_id() {
        Some(id) => folders
            .fetch_one(auth, session, id)
            .await?
            .into_iter()
            .collect(),
        None => folders.fetch_all(auth, session).await?,
    };
    let applied = auth.with_active_generation(owner, session.generation, || {
        folders.storage.save_folder_refresh(owner, &plan, rows)
    })??;
    log::debug!(
        "Сверка папок: область={}, запись={}",
        if plan.folder_id().is_some() {
            "папка"
        } else {
            "список"
        },
        applied
    );
    Ok(true)
}
