//! Определяет данные папок мессенджера и команды с проверкой владельца.
//! Сначала описаны папки и связи, затем чтение, изменение и сверка с сервером.

use std::sync::Arc;

use thiserror::Error;
use uuid::Uuid;

use crate::{
    auth::{Auth, AuthError, Service, SessionSnapshot},
    context::{OwnerContext, SessionGeneration},
    runtime::SourceError,
    storage::{Storage, StorageError},
};

use super::rest::RestClient;

/// Устойчивый серверный UUID «Все чаты».
pub const ALL_CHATS_ID: Uuid = Uuid::nil();

/// Правило состава папки и доступных для неё действий.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum FolderKind {
    All,      // Все доступные чаты.
    Personal, // Приватные разговоры по серверным правилам.
    Channels, // Каналы по серверным правилам.
    Custom,   // Папка и состав, которые меняет пользователь.
    Unknown,  // Папка неизвестного типа. Доступно чтение.
}

impl FolderKind {
    /// Берёт системный тип из фиксированного UUID даже при общем флаге «all».
    pub(super) fn from_server(id: Uuid, kind: Option<&str>) -> Self {
        match id.as_u128() {
            0 => Self::All,
            1 => Self::Personal,
            2 => Self::Channels,
            _ if kind == Some("created") => Self::Custom,
            _ => Self::Unknown,
        }
    }
}

/// Тип разговора для связи с папкой по контракту Messenger.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatType {
    Private, // Личка или приватный разговор в системном списке сервера.
    Group,   // Приватная группа в пользовательской папке.
    Stream,  // Канал.
}

/// Данные папки и её подтверждённый на сервере состав.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Folder {
    pub id: Uuid,                            // UUID папки внутри контекста владельца.
    pub kind: FolderKind,                    // Тип папки и доступные операции.
    pub title: String,                       // Название папки на сервере.
    pub background_color_value: Option<u32>, // Числовой цвет. None означает, что цвет не задан.
    pub unread_count: u32,                   // Серверное число активных непрочитанных для папки.
    pub created_at: String,                  // Время создания в нормализованном UTC.
    pub updated_at: String,                  // Время последнего обновления в нормализованном UTC.
    pub items: Vec<FolderItem>,              // Полный снимок элементов в серверном порядке.
}

/// Чат в конкретной папке со своим порядком, закреплением и счётчиками.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct FolderItem {
    pub id: Uuid,                  // UUID связи папки с чатом.
    pub stream_id: Uuid,           // UUID существующего разговора в каталоге.
    pub chat_type: ChatType,       // Тип связи по контракту API.
    pub pinned_at: Option<String>, // Время закрепления в UTC. None означает, что чат не закреплён.
    pub order_index: Option<i32>,  // Позиция, если её явно назначил сервер.
    pub unread_count: u32,         // Всего непрочитанных по данным сервера.
    pub active_unread_count: u32,  // Активная часть непрочитанного.
    pub passive_unread_count: u32, // Пассивная часть непрочитанного.
    pub created_at: String,        // Время добавления связи в нормализованном UTC.
    pub updated_at: String,        // Время последнего обновления связи в нормализованном UTC.
}

/// Поля, которые пользователь меняет у одной папки.
#[derive(Clone, Debug, Default)]
pub struct FolderChanges {
    pub title: Option<String>, // Новое название. None сохраняет прежнее.
    pub background_color_value: Option<Option<u32>>, // Some(None) сбрасывает цвет, None сохраняет.
}

/// Результат отказа при чтении папок или при выполнении команды.
#[derive(Debug, Error)]
pub enum FolderError {
    #[error(transparent)]
    Auth(#[from] AuthError), // Сессии уже нет или она ещё не готова.
    #[error(transparent)]
    Source(#[from] SourceError), // Чтение не удалось, команда ещё не отправлена.
    #[error(transparent)]
    Storage(#[from] StorageError), // Не удалось прочитать или записать снимок папок.
    #[error("provide at least one valid folder field to change")]
    InvalidChanges, // Пустое изменение или название за пределами 1–64 символов.
    #[error("provide a UUID outside the reserved system folder range")]
    InvalidFolderId, // UUID 0, 1 и 2 зарезервированы для системных папок.
    #[error("the folder or chat is no longer available")]
    Unavailable, // Папка, связь или разговор отсутствует в доступном снимке.
    #[error("this operation is unavailable for the folder kind")]
    ReadOnly, // Сервер управляет папкой или её тип не подтверждён.
    #[error("the folder command was rejected with HTTP {0}")]
    Rejected(u16), // Сервер явно отклонил команду.
    #[error("folder command outcome is unknown")]
    Unknown, // Команда могла изменить сервер. Перед повтором нужна сверка.
    #[error("folder command completed, but local refresh could not be scheduled: {0}")]
    Refresh(StorageError), // Сервер подтвердил действие, локальная отметка не записалась.
}

/// Загружает серверные папки и выполняет команды от имени выбранной сессии.
#[derive(Clone)]
pub struct Folders {
    rest: Arc<RestClient>,            // Общий HTTPS-клиент мессенджера.
    pub(super) storage: Arc<Storage>, // Снимки и очередь сверок хранятся в общей базе.
}

impl Folders {
    /// Подключает клиент и хранилище текущего профиля.
    pub(super) fn new(rest: Arc<RestClient>, storage: Arc<Storage>) -> Self {
        Self { rest, storage }
    }

    /// Читает все страницы папок для действующего входа.
    pub(crate) async fn fetch_all(
        &self,
        auth: &Auth,
        session: &SessionSnapshot,
    ) -> Result<Vec<Folder>, FolderError> {
        let owner = &session.context;
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, session.generation)
            .await?;
        let result = self.rest.folder_collection(owner, &token).await;
        auth.with_active_generation(owner, session.generation, || ())?;
        Ok(result?)
    }

    /// Читает одну папку. 404 означает, что сервер больше её не отдаёт этому входу.
    pub(crate) async fn fetch_one(
        &self,
        auth: &Auth,
        session: &SessionSnapshot,
        id: Uuid,
    ) -> Result<Option<Folder>, FolderError> {
        let owner = &session.context;
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, session.generation)
            .await?;
        let result = self.rest.folder(owner, &token, id).await;
        auth.with_active_generation(owner, session.generation, || ())?;
        Ok(result?)
    }

    /// Создаёт папку с заранее выбранным UUID. При повторе передаём тот же UUID, название и цвет.
    pub async fn create_folder(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
        id: Uuid,
        title: &str,
        background_color_value: Option<u32>,
    ) -> Result<Uuid, FolderError> {
        let title = validated_title(title)?;
        if FolderKind::from_server(id, None) != FolderKind::Unknown {
            return Err(FolderError::InvalidFolderId);
        }
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let body = serde_json::json!({
            "uuid": id,
            "title": title,
            "background_color_value": background_color_value,
        });
        let result = self.rest.create_folder(owner, &token, id, body).await;
        self.finish_command(auth, owner, generation, Some(id), result)
    }

    /// Меняет только указанные поля подтверждённой пользовательской папки.
    pub async fn update_folder(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
        folder_id: Uuid,
        changes: FolderChanges,
    ) -> Result<(), FolderError> {
        let body = changes.into_body()?;
        let (token, _) = self
            .prepare_folder(auth, owner, generation, folder_id, true)
            .await?;
        let result = self
            .rest
            .update_folder(owner, &token, folder_id, body)
            .await;
        self.finish_command(auth, owner, generation, Some(folder_id), result)
    }

    /// Удаляет пользовательскую папку. Сам разговор остаётся в каталоге.
    pub async fn delete_folder(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
        folder_id: Uuid,
    ) -> Result<(), FolderError> {
        let (token, _) = self
            .prepare_folder(auth, owner, generation, folder_id, true)
            .await?;
        let result = self.rest.delete_folder(owner, &token, folder_id).await;
        self.finish_command(auth, owner, generation, Some(folder_id), result)
    }

    /// Добавляет доступный активный разговор и отдаёт серверный UUID связи.
    pub async fn add_chat(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
        folder_id: Uuid,
        stream_id: Uuid,
    ) -> Result<Uuid, FolderError> {
        let (token, _) = self
            .prepare_folder(auth, owner, generation, folder_id, true)
            .await?;
        let chat_type = self.rest.folder_chat_type(owner, &token, stream_id).await;
        auth.with_active_generation(owner, generation, || ())?;
        let result = self
            .rest
            .add_folder_chat(owner, &token, folder_id, stream_id, chat_type?)
            .await;
        self.finish_command(auth, owner, generation, Some(folder_id), result)
    }

    /// Убирает связь только из указанной пользовательской папки.
    pub async fn remove_chat(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
        folder_id: Uuid,
        item_id: Uuid,
    ) -> Result<(), FolderError> {
        let (token, folder) = self
            .prepare_folder(auth, owner, generation, folder_id, true)
            .await?;
        folder.item(item_id)?;
        let result = self.rest.delete_folder_item(owner, &token, item_id).await;
        self.finish_command(auth, owner, generation, Some(folder_id), result)
    }

    /// Меняет закрепление одного элемента именно в этой папке.
    pub async fn set_pinned(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
        folder_id: Uuid,
        item_id: Uuid,
        pinned: bool,
    ) -> Result<(), FolderError> {
        let (token, folder) = self
            .prepare_folder(auth, owner, generation, folder_id, false)
            .await?;
        let item = folder.item(item_id)?;
        let result = self
            .rest
            .set_folder_item_pinned(owner, &token, folder_id, item, pinned)
            .await;
        self.finish_command(auth, owner, generation, Some(folder_id), result)
    }

    /// Подтверждает владельца и доступность папки перед отправкой команды.
    async fn prepare_folder(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
        id: Uuid,
        custom_only: bool,
    ) -> Result<(String, Folder), FolderError> {
        if custom_only && FolderKind::from_server(id, None) != FolderKind::Unknown {
            return Err(FolderError::ReadOnly);
        }
        let token = auth
            .access_token_for_generation(owner, Service::Messenger, generation)
            .await?;
        let folder = self.rest.folder(owner, &token, id).await;
        auth.with_active_generation(owner, generation, || ())?;
        let folder = folder?.ok_or(FolderError::Unavailable)?;
        if folder.kind == FolderKind::Unknown || (custom_only && folder.kind != FolderKind::Custom)
        {
            return Err(FolderError::ReadOnly);
        }
        Ok((token, folder))
    }

    /// Сохраняет отметку сверки после успеха или неоднозначного результата команды.
    fn finish_command<T>(
        &self,
        auth: &Auth,
        owner: &OwnerContext,
        generation: SessionGeneration,
        folder_id: Option<Uuid>,
        result: Result<T, FolderError>,
    ) -> Result<T, FolderError> {
        auth.with_active_generation(owner, generation, || {
            if result.is_ok() || matches!(result, Err(FolderError::Unknown)) {
                let refresh = self.storage.request_folder_refresh(owner, folder_id);
                if let Err(error) = refresh {
                    if result.is_ok() {
                        return Err(FolderError::Refresh(error));
                    }
                    log::warn!("Не удалось сохранить сверку после команды папок: {error}");
                }
            }
            result
        })?
    }
}

impl Folder {
    /// Подтверждает принадлежность элемента к папке из свежего ответа.
    fn item(&self, id: Uuid) -> Result<&FolderItem, FolderError> {
        self.items
            .iter()
            .find(|item| item.id == id)
            .ok_or(FolderError::Unavailable)
    }
}

impl FolderChanges {
    /// Передаёт только изменённые поля и отдельно сохраняет явный сброс цвета.
    fn into_body(self) -> Result<serde_json::Value, FolderError> {
        let mut fields = serde_json::Map::new();
        if let Some(title) = self.title.as_deref() {
            fields.insert("title".into(), validated_title(title)?.into());
        }
        if let Some(color) = self.background_color_value {
            fields.insert("background_color_value".into(), serde_json::json!(color));
        }
        if fields.is_empty() {
            return Err(FolderError::InvalidChanges);
        }
        Ok(serde_json::Value::Object(fields))
    }
}

/// Убирает пробелы по краям и проверяет серверное ограничение длины названия.
fn validated_title(title: &str) -> Result<&str, FolderError> {
    let title = title.trim();
    if (1..=64).contains(&title.chars().count()) {
        Ok(title)
    } else {
        Err(FolderError::InvalidChanges)
    }
}
