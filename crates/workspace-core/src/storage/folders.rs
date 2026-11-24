//! Хранит папки и их связи с каталогом в SQLite. Описывает снимки, отметки для сверки
//! и изменения, которые фиксируются вместе с позицией журнала.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    messenger::{catalog::Chat, folders::Folder},
};

use super::{
    Storage, StorageError, bump_catalog_revision, read_chat_catalog, read_unread,
    remove_catalog_stream,
};

#[cfg(test)]
mod tests;

/// Изменение папок или каталога из события мессенджера.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolderChange {
    Refresh(Option<Uuid>), // Папка для сверки. None означает весь список.
    DeleteFolder(Uuid),    // UUID удалённой папки вместе с её составом.
    DeleteItem(Uuid),      // UUID удалённой связи с разговором.
    TopicCreated,          // Новая тема, для которой нужно перечитать каталог.
    StreamChanged {
        stream_id: Uuid, // UUID созданного или обновлённого разговора.
        archived: bool,  // Архивный разговор исключается из активного списка.
    },
}

/// Захватывает область сверки и версию данных до начала запроса.
#[derive(Clone, Debug)]
pub(crate) struct FolderRefreshPlan {
    owner: OwnerContext,     // Владелец, для которого запрашиваем данные.
    folder_id: Option<Uuid>, // Целевая папка. None означает весь список.
    revision: i64,           // Версия снимка и событий, сразу меняющих его содержимое.
    sequence: i64,           // До этой отметки включительно запрос покрывает известные изменения.
}

impl FolderRefreshPlan {
    /// Возвращает UUID целевой папки или None для полной сверки.
    pub(crate) fn folder_id(&self) -> Option<Uuid> {
        self.folder_id
    }
}

/// Данные, которые проекция папок читает из одной транзакции.
pub(crate) struct StoredFolderView {
    pub folders: Option<Vec<Folder>>, // Последний полный список или None до сверки.
    pub catalog: Option<String>,      // Сохранённые активные разговоры в формате каталога.
    pub unread: Option<u32>,          // Общий счётчик из источника Dock, если известен.
    pub needs_refresh: bool,          // Есть незавершённая сверка папок.
}

/// Состояние снимка и заказанных сверок одного владельца.
#[derive(Default)]
struct FolderState {
    complete: bool, // Полный снимок был сохранён после последнего сброса.
    revision: i64,  // Версия снимка и событий, сразу меняющих его содержимое.
    sequence: i64,  // Последняя отметка для сверки.
}

/// Определяет, какие подписки разбудить после фиксации события.
#[derive(Default)]
pub(super) struct FolderEffects {
    pub folders: bool, // Список папок или отметки для их сверки изменились.
    pub catalog: bool, // Каталог изменился или нужна его сверка.
}

/// Добавляет снимки, связи и долговечные отметки для сверки в профиль.
pub(super) fn initialize(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS messenger_folder_state (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL,
            complete INTEGER NOT NULL DEFAULT 0, revision INTEGER NOT NULL DEFAULT 0,
            sequence INTEGER NOT NULL DEFAULT 0, catalog_dirty INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id)
         );
         CREATE TABLE IF NOT EXISTS messenger_folders (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, folder_id TEXT NOT NULL,
            position INTEGER NOT NULL, snapshot TEXT NOT NULL,
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, folder_id)
         );
         CREATE TABLE IF NOT EXISTS messenger_folder_items (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, folder_id TEXT NOT NULL,
            item_id TEXT NOT NULL, stream_id TEXT NOT NULL, position INTEGER NOT NULL,
            snapshot TEXT NOT NULL,
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, item_id),
            FOREIGN KEY (account_id, server, organization_id, project_id, user_id, folder_id)
                REFERENCES messenger_folders ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS messenger_folder_items_by_folder
            ON messenger_folder_items(account_id, server, organization_id, project_id, user_id, folder_id, position);
         CREATE INDEX IF NOT EXISTS messenger_folder_items_by_stream
            ON messenger_folder_items(account_id, server, organization_id, project_id, user_id, stream_id);
         CREATE TABLE IF NOT EXISTS messenger_folder_refresh (
            account_id TEXT NOT NULL, server TEXT NOT NULL, organization_id TEXT NOT NULL,
            project_id TEXT NOT NULL, user_id TEXT NOT NULL, folder_id TEXT NOT NULL,
            sequence INTEGER NOT NULL,
            PRIMARY KEY (account_id, server, organization_id, project_id, user_id, folder_id)
         );",
    )?;
    Ok(())
}

impl Storage {
    /// Читает согласованный снимок папок, каталога и общего счётчика.
    pub(crate) fn folder_view(
        &self,
        owner: &OwnerContext,
    ) -> Result<StoredFolderView, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let state = read_state(&transaction, owner)?;
        let view = StoredFolderView {
            folders: read_snapshot(&transaction, owner, state.complete)?,
            catalog: read_chat_catalog(&transaction, owner)?,
            unread: read_unread(&transaction, owner)?.map(|saved| saved.count),
            needs_refresh: !state.complete || next_refresh(&transaction, owner)?.is_some(),
        };
        transaction.commit()?;
        Ok(view)
    }

    /// Подписывает обработчик на подтверждённые записи папок, каталога и счётчика.
    pub(crate) fn folder_changes(&self) -> watch::Receiver<u64> {
        self.folder_changes.subscribe()
    }

    /// Будит читателей после фиксации изменений в базе.
    pub(super) fn notify_folder_change(&self) {
        self.folder_changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Сохраняет запрос на сверку после команды. None запрашивает весь список.
    pub(crate) fn request_folder_refresh(
        &self,
        owner: &OwnerContext,
        folder_id: Option<Uuid>,
    ) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        mark_refresh(&transaction, owner, folder_id)?;
        transaction.commit()?;
        drop(connection);
        self.notify_folder_change();
        Ok(())
    }

    /// Ставит сверку каталога в очередь после подтверждённого изменения разговора или темы.
    pub(crate) fn request_catalog_refresh(&self, owner: &OwnerContext) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        mark_catalog_refresh(&transaction, owner)?;
        transaction.commit()?;
        drop(connection);
        self.notify_folder_change();
        Ok(())
    }

    /// Запрашивает согласованную сверку каталога и папок после восстановления чата.
    pub(crate) fn request_catalog_and_folder_refresh(
        &self,
        owner: &OwnerContext,
    ) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        mark_catalog_refresh(&transaction, owner)?;
        mark_refresh(&transaction, owner, None)?;
        transaction.commit()?;
        drop(connection);
        self.notify_catalog_change();
        Ok(())
    }

    /// Выбирает следующую область для запроса. Незагруженный профиль начинает со всего списка.
    pub(crate) fn folder_refresh_plan(
        &self,
        owner: &OwnerContext,
    ) -> Result<Option<FolderRefreshPlan>, StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let state = read_state(&transaction, owner)?;
        let folder_id = if state.complete {
            let Some(folder_id) = next_refresh(&transaction, owner)? else {
                return Ok(None);
            };
            folder_id
        } else {
            None
        };
        transaction.commit()?;
        Ok(Some(FolderRefreshPlan {
            owner: owner.clone(),
            folder_id,
            revision: state.revision,
            sequence: state.sequence,
        }))
    }

    /// Сохраняет полный или точечный результат. Пустой точечный снимок означает HTTP 404.
    /// Возвращает false, если снимок пересёк удаление, сброс или более новую запись.
    pub(crate) fn save_folder_refresh(
        &self,
        owner: &OwnerContext,
        plan: &FolderRefreshPlan,
        folders: Vec<Folder>,
    ) -> Result<bool, StorageError> {
        if &plan.owner != owner
            || plan.folder_id.is_some_and(|id| {
                folders.len() > 1 || folders.first().is_some_and(|folder| folder.id != id)
            })
        {
            return Err(StorageError::InvalidFolderSnapshot);
        }

        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = read_state(&transaction, owner)?;
        if state.revision != plan.revision {
            return Ok(false);
        }
        let catalog_changed = has_new_unknown_stream(&transaction, owner, &folders)?;
        if let Some(id) = plan.folder_id {
            let position = folder_position(&transaction, owner, id)?;
            delete_folders(&transaction, owner, Some(id))?;
            if let Some(folder) = folders.into_iter().next() {
                write_folder(&transaction, owner, position, folder)?;
            }
        } else {
            delete_folders(&transaction, owner, None)?;
            for (position, folder) in folders.into_iter().enumerate() {
                write_folder(&transaction, owner, position as i64, folder)?;
            }
        }
        // Более поздняя отметка остаётся на диске для следующего запроса.
        let folder_id = plan.folder_id.map(|id| id.to_string());
        transaction.execute(
            "DELETE FROM messenger_folder_refresh
             WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
               AND (?6 IS NULL OR folder_id = ?6) AND sequence <= ?7",
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                folder_id,
                plan.sequence
            ],
        )?;
        ensure_state(&transaction, owner)?;
        transaction.execute(
            "UPDATE messenger_folder_state SET complete = 1, revision = revision + 1
             WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)",
            owner_params(owner),
        )?;
        if catalog_changed {
            mark_catalog_refresh(&transaction, owner)?;
        }
        transaction.commit()?;
        drop(connection);
        if catalog_changed {
            self.notify_catalog_change();
        } else {
            self.notify_folder_change();
        }
        Ok(true)
    }

    /// Проверяет, надо ли перечитать каталог из-за сброса или нового разговора.
    pub(crate) fn catalog_needs_refresh(&self, owner: &OwnerContext) -> Result<bool, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        Ok(connection.query_row(
            "SELECT NOT EXISTS (
                SELECT 1 FROM messenger_catalog
                WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
             ) OR EXISTS (
                SELECT 1 FROM messenger_folder_state
                WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
                  AND catalog_dirty
             )",
            owner_params(owner),
            |row| row.get(0),
        )?)
    }
}

/// Применяет изменение папок внутри транзакции синхронизации.
pub(super) fn apply_change(
    connection: &Connection,
    owner: &OwnerContext,
    change: FolderChange,
) -> Result<FolderEffects, StorageError> {
    let mut catalog = false;
    match change {
        FolderChange::Refresh(folder_id) => mark_refresh(connection, owner, folder_id)?,
        FolderChange::DeleteFolder(id) => {
            delete_folders(connection, owner, Some(id))?;
            connection.execute(
                "DELETE FROM messenger_folder_refresh
                 WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
                   AND folder_id = ?6",
                params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id,
                    owner.user_id, id.to_string()],
            )?;
            bump_revision(connection, owner)?;
        }
        FolderChange::DeleteItem(id) => {
            let folder_id: Option<String> = connection
                .query_row(
                    "DELETE FROM messenger_folder_items
                     WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
                       AND item_id = ?6 RETURNING folder_id",
                    params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id,
                        owner.user_id, id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            let folder_id = folder_id.as_deref().map(parse_folder_id).transpose()?;
            bump_revision(connection, owner)?;
            mark_refresh(connection, owner, folder_id)?;
        }
        FolderChange::TopicCreated => {
            mark_catalog_refresh(connection, owner)?;
            catalog = true;
        }
        FolderChange::StreamChanged {
            stream_id,
            archived,
        } => {
            let stream = stream_id.to_string();
            if archived {
                remove_catalog_stream(connection, owner, &stream)?;
                set_catalog_dirty(connection, owner)?;
                remove_stream(connection, owner, &stream)?;
            } else if !contains_catalog_stream(connection, owner, &stream)? {
                mark_catalog_refresh(connection, owner)?;
                mark_refresh(connection, owner, None)?;
            } else {
                return Ok(FolderEffects::default());
            }
            catalog = true;
        }
    }
    Ok(FolderEffects {
        folders: true,
        catalog,
    })
}

/// Убирает связи недоступного разговора и блокирует уже начатые запросы папок.
pub(super) fn remove_stream(
    connection: &Connection,
    owner: &OwnerContext,
    stream: &str,
) -> Result<(), StorageError> {
    connection.execute(
        "DELETE FROM messenger_folder_items
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
           AND stream_id = ?6",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            stream
        ],
    )?;
    bump_revision(connection, owner)?;
    mark_refresh(connection, owner, None)
}

/// После сброса удаляет снимок владельца и делает ранее начатые запросы недействительными.
pub(super) fn reset(connection: &Connection, owner: &OwnerContext) -> Result<(), StorageError> {
    delete_folders(connection, owner, None)?;
    connection.execute(
        "DELETE FROM messenger_folder_refresh
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)",
        owner_params(owner),
    )?;
    ensure_state(connection, owner)?;
    connection.execute(
        "UPDATE messenger_folder_state SET complete = 0, revision = revision + 1
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)",
        owner_params(owner),
    )?;
    Ok(())
}

/// Снимает отметку каталога после принятой записи его полного списка.
pub(super) fn finish_catalog_refresh(
    connection: &Connection,
    owner: &OwnerContext,
) -> Result<(), StorageError> {
    connection.execute(
        "UPDATE messenger_folder_state SET catalog_dirty = 0
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)",
        owner_params(owner),
    )?;
    Ok(())
}

/// Ставит отметку каталога и отклоняет ответы, начатые до изменения.
fn mark_catalog_refresh(connection: &Connection, owner: &OwnerContext) -> Result<(), StorageError> {
    set_catalog_dirty(connection, owner)?;
    bump_catalog_revision(connection, owner)
}

/// Сохраняет запрос на обновление. Архив уже увеличил ревизию при удалении чата.
fn set_catalog_dirty(connection: &Connection, owner: &OwnerContext) -> Result<(), StorageError> {
    ensure_state(connection, owner)?;
    connection.execute(
        "UPDATE messenger_folder_state SET catalog_dirty = 1
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)",
        owner_params(owner),
    )?;
    Ok(())
}

/// Проверяет, есть ли в новом снимке ранее неизвестная связь с разговором вне каталога.
fn has_new_unknown_stream(
    connection: &Connection,
    owner: &OwnerContext,
    folders: &[Folder],
) -> Result<bool, StorageError> {
    let mut checked = HashSet::new();
    let mut streams = folders
        .iter()
        .flat_map(|folder| folder.items.iter().map(|item| item.stream_id))
        .peekable();
    if streams.peek().is_none() {
        return Ok(false);
    }
    let catalog: Vec<Chat> = read_chat_catalog(connection, owner)?
        .map(|snapshot| serde_json::from_str(&snapshot))
        .transpose()?
        .unwrap_or_default();
    checked.extend(catalog.into_iter().map(|chat| chat.id));
    let mut query = connection.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM messenger_folder_items
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
           AND stream_id = ?6)",
    )?;
    for stream in streams.filter(|id| checked.insert(*id)) {
        let known: bool = query.query_row(
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                stream.to_string()
            ],
            |row| row.get(0),
        )?;
        if !known {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Проверяет наличие активного разговора в кэше каталога.
fn contains_catalog_stream(
    connection: &Connection,
    owner: &OwnerContext,
    stream: &str,
) -> Result<bool, StorageError> {
    Ok(connection.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM messenger_catalog AS catalog, json_each(catalog.snapshot) AS chat
            WHERE (catalog.account_id, catalog.server, catalog.organization_id, catalog.project_id, catalog.user_id)
                  = (?1, ?2, ?3, ?4, ?5)
              AND json_extract(chat.value, '$.id') = ?6
         )",
        params![owner.account_id.0, owner.server, owner.organization_id, owner.project_id,
            owner.user_id, stream],
        |row| row.get(0),
    )?)
}

/// Добавляет строку состояния, если владелец впервые сохраняет данные папок.
fn ensure_state(connection: &Connection, owner: &OwnerContext) -> Result<(), StorageError> {
    connection.execute(
        "INSERT INTO messenger_folder_state (account_id, server, organization_id, project_id, user_id)
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
        owner_params(owner),
    )?;
    Ok(())
}

/// Читает полноту, ревизию и последнюю отметку для сверки.
fn read_state(connection: &Connection, owner: &OwnerContext) -> Result<FolderState, StorageError> {
    Ok(connection
        .query_row(
            "SELECT complete, revision, sequence FROM messenger_folder_state
             WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)",
            owner_params(owner),
            |row| Ok(FolderState {
                complete: row.get(0)?,
                revision: row.get(1)?,
                sequence: row.get(2)?,
            }),
        )
        .optional()?
        .unwrap_or_default())
}

/// Сохраняет новую отметку области, которую не покроет уже запущенный запрос.
fn mark_refresh(
    connection: &Connection,
    owner: &OwnerContext,
    folder_id: Option<Uuid>,
) -> Result<(), StorageError> {
    ensure_state(connection, owner)?;
    let sequence: i64 = connection.query_row(
        "UPDATE messenger_folder_state SET sequence = sequence + 1
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
         RETURNING sequence",
        owner_params(owner),
        |row| row.get(0),
    )?;
    // Пустой ключ обозначает сверку всего списка и не совпадает ни с одним UUID.
    connection.execute(
        "INSERT INTO messenger_folder_refresh VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (account_id, server, organization_id, project_id, user_id, folder_id)
         DO UPDATE SET sequence = excluded.sequence",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            folder_id.map_or_else(String::new, |id| id.to_string()),
            sequence
        ],
    )?;
    Ok(())
}

/// Выбирает полную сверку первой, иначе берёт папку с самой ранней отметкой.
fn next_refresh(
    connection: &Connection,
    owner: &OwnerContext,
) -> Result<Option<Option<Uuid>>, StorageError> {
    let scope = connection
        .query_row(
            "SELECT folder_id FROM messenger_folder_refresh
             WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
             ORDER BY folder_id <> '', sequence LIMIT 1",
            owner_params(owner),
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    scope
        .map(|id| (!id.is_empty()).then(|| parse_folder_id(&id)).transpose())
        .transpose()
}

/// Делает запущенные запросы папок недействительными после явного изменения.
fn bump_revision(connection: &Connection, owner: &OwnerContext) -> Result<(), StorageError> {
    ensure_state(connection, owner)?;
    connection.execute(
        "UPDATE messenger_folder_state SET revision = revision + 1
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)",
        owner_params(owner),
    )?;
    Ok(())
}

/// Читает папки и их состав в сохранённом серверном порядке.
fn read_snapshot(
    connection: &Connection,
    owner: &OwnerContext,
    complete: bool,
) -> Result<Option<Vec<Folder>>, StorageError> {
    if !complete {
        return Ok(None);
    }
    let mut folders = connection.prepare_cached(
        "SELECT snapshot FROM messenger_folders
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
         ORDER BY position, folder_id",
    )?;
    let mut items = connection.prepare_cached(
        "SELECT snapshot FROM messenger_folder_items
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
           AND folder_id = ?6 ORDER BY position",
    )?;
    let mut result = Vec::new();
    for snapshot in folders.query_map(owner_params(owner), |row| row.get::<_, String>(0))? {
        let mut folder: Folder =
            serde_json::from_str(&snapshot?).map_err(StorageError::FolderSnapshot)?;
        let rows = items.query_map(
            params![
                owner.account_id.0,
                owner.server,
                owner.organization_id,
                owner.project_id,
                owner.user_id,
                folder.id.to_string()
            ],
            |row| row.get::<_, String>(0),
        )?;
        for row in rows {
            folder
                .items
                .push(serde_json::from_str(&row?).map_err(StorageError::FolderSnapshot)?);
        }
        result.push(folder);
    }
    Ok(Some(result))
}

/// Удаляет папку либо весь список. Внешний ключ убирает связанные элементы.
fn delete_folders(
    connection: &Connection,
    owner: &OwnerContext,
    folder_id: Option<Uuid>,
) -> Result<(), StorageError> {
    connection.execute(
        "DELETE FROM messenger_folders
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)
           AND (?6 IS NULL OR folder_id = ?6)",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            folder_id.map(|id| id.to_string())
        ],
    )?;
    Ok(())
}

/// Сохраняет место известной папки, а новую ставит после сохранённых.
fn folder_position(
    connection: &Connection,
    owner: &OwnerContext,
    id: Uuid,
) -> Result<i64, StorageError> {
    Ok(connection.query_row(
        "SELECT COALESCE(MAX(CASE WHEN folder_id = ?6 THEN position END), MAX(position) + 1, 0)
         FROM messenger_folders
         WHERE (account_id, server, organization_id, project_id, user_id) = (?1, ?2, ?3, ?4, ?5)",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            id.to_string()
        ],
        |row| row.get(0),
    )?)
}

/// Записывает метаданные папки и её элементы отдельными строками.
fn write_folder(
    connection: &Connection,
    owner: &OwnerContext,
    position: i64,
    mut folder: Folder,
) -> Result<(), StorageError> {
    let folder_id = folder.id.to_string();
    let items = std::mem::take(&mut folder.items);
    let snapshot = serde_json::to_string(&folder).map_err(StorageError::FolderSnapshot)?;
    connection.execute(
        "INSERT INTO messenger_folders VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            folder_id,
            position,
            snapshot
        ],
    )?;
    let mut insert = connection.prepare_cached(
        "INSERT INTO messenger_folder_items VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for (position, item) in items.into_iter().enumerate() {
        insert.execute(params![
            owner.account_id.0,
            owner.server,
            owner.organization_id,
            owner.project_id,
            owner.user_id,
            folder_id,
            item.id.to_string(),
            item.stream_id.to_string(),
            position as i64,
            serde_json::to_string(&item).map_err(StorageError::FolderSnapshot)?,
        ])?;
    }
    Ok(())
}

/// Восстанавливает UUID из ключа сохранённой записи.
fn parse_folder_id(id: &str) -> Result<Uuid, StorageError> {
    Uuid::parse_str(id).map_err(|_| StorageError::InvalidFolderSnapshot)
}

/// Передаёт SQLite все части ключа владельца в порядке запроса.
fn owner_params(owner: &OwnerContext) -> [&str; 5] {
    [
        &owner.account_id.0,
        &owner.server,
        &owner.organization_id,
        &owner.project_id,
        &owner.user_id,
    ]
}
