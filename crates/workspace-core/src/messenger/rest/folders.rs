//! Читает папки через REST и выполняет команды без автоматического повтора.
//! Ниже описано, как проверить владельца и привести серверные ответы к модели Core.

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use reqwest::{Method, Response, StatusCode, header::ACCEPT};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    messenger::folders::{ChatType, Folder, FolderError, FolderItem, FolderKind},
    runtime::SourceError,
    storage::HistoryTime,
};

use super::{
    RestClient, STREAMS_PATH, WireStream, check_status, matches_owner, request_error,
    workspace_origin,
};

const FOLDERS_PATH: &str = "/api/workspace/v1/messenger/folders/";
const ITEMS_PATH: &str = "/api/workspace/v1/messenger/folder_items/";

impl RestClient {
    /// Читает все страницы и проверяет согласованность повторившихся UUID.
    pub(in crate::messenger) async fn folder_collection(
        &self,
        owner: &OwnerContext,
        token: &str,
    ) -> Result<Vec<Folder>, SourceError> {
        let binding = FolderOwner::parse(owner)?;
        let rows = self.collection(owner, token, FOLDERS_PATH).await?;
        normalize_folders(rows, binding)
    }

    /// Загружает папку с её текущим составом. 404 сообщает об отсутствии папки у владельца.
    pub(in crate::messenger) async fn folder(
        &self,
        owner: &OwnerContext,
        token: &str,
        id: Uuid,
    ) -> Result<Option<Folder>, SourceError> {
        let binding = FolderOwner::parse(owner)?;
        let response = self
            .request(owner, token, &format!("{FOLDERS_PATH}{id}"))?
            .send()
            .await
            .map_err(request_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        check_status(response.status())?;
        let wire: WireFolder = response.json().await.map_err(request_error)?;
        if wire.uuid != id {
            return Err(SourceError::InvalidResponse);
        }
        wire.into_folder(binding).map(Some)
    }

    /// Определяет тип активного доступного разговора для команды добавления в папку.
    pub(in crate::messenger) async fn folder_chat_type(
        &self,
        owner: &OwnerContext,
        token: &str,
        id: Uuid,
    ) -> Result<ChatType, FolderError> {
        if id.is_nil() {
            return Err(FolderError::Unavailable);
        }
        let request = self.request(owner, token, &format!("{STREAMS_PATH}/{id}"))?;
        let response = request.send().await.map_err(request_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(FolderError::Unavailable);
        }
        check_status(response.status())?;
        let stream: WireStream = response.json().await.map_err(request_error)?;
        if Uuid::parse_str(&stream.uuid).ok() != Some(id)
            || !matches_owner(
                owner,
                stream.project_id.as_deref(),
                stream.user_uuid.as_deref(),
            )
            || stream
                .direct_user_uuid
                .as_deref()
                .is_some_and(|user| Uuid::parse_str(user).map_or(true, |user| user.is_nil()))
        {
            return Err(SourceError::InvalidResponse.into());
        }
        if stream.is_archived {
            return Err(FolderError::Unavailable);
        }
        Ok(if stream.is_direct() {
            ChatType::Private
        } else if stream.private {
            ChatType::Group
        } else {
            ChatType::Stream
        })
    }

    /// Создаёт папку и проверяет, что ответ подтверждает новый пользовательский UUID.
    pub(in crate::messenger) async fn create_folder(
        &self,
        owner: &OwnerContext,
        token: &str,
        id: Uuid,
        body: Value,
    ) -> Result<Uuid, FolderError> {
        let binding = FolderOwner::parse(owner)?;
        let response = self
            .folder_command(owner, token, Method::POST, FOLDERS_PATH, Some(body))
            .await?;
        let folder = custom_folder_response(response, binding).await?;
        if folder.id != id {
            return Err(FolderError::Unknown);
        }
        Ok(folder.id)
    }

    /// Отправляет частичное изменение папки и сверяет UUID результата.
    pub(in crate::messenger) async fn update_folder(
        &self,
        owner: &OwnerContext,
        token: &str,
        id: Uuid,
        body: Value,
    ) -> Result<(), FolderError> {
        let binding = FolderOwner::parse(owner)?;
        let response = self
            .folder_command(
                owner,
                token,
                Method::PUT,
                &format!("{FOLDERS_PATH}{id}"),
                Some(body),
            )
            .await?;
        let folder = custom_folder_response(response, binding).await?;
        if folder.id != id {
            return Err(FolderError::Unknown);
        }
        Ok(())
    }

    /// Удаляет папку по UUID одним запросом.
    pub(in crate::messenger) async fn delete_folder(
        &self,
        owner: &OwnerContext,
        token: &str,
        id: Uuid,
    ) -> Result<(), FolderError> {
        self.folder_command(
            owner,
            token,
            Method::DELETE,
            &format!("{FOLDERS_PATH}{id}"),
            None,
        )
        .await?;
        Ok(())
    }

    /// Добавляет связь и проверяет папку, разговор и его тип в ответе.
    pub(in crate::messenger) async fn add_folder_chat(
        &self,
        owner: &OwnerContext,
        token: &str,
        folder_id: Uuid,
        stream_id: Uuid,
        chat_type: ChatType,
    ) -> Result<Uuid, FolderError> {
        let binding = FolderOwner::parse(owner)?;
        let body =
            json!({ "folder_uuid": folder_id, "stream_uuid": stream_id, "chat_type": chat_type });
        let response = self
            .folder_command(owner, token, Method::POST, ITEMS_PATH, Some(body))
            .await?;
        let item = item_command_response(response, binding, folder_id, |item| {
            item.stream_id == stream_id && item.chat_type == chat_type
        })
        .await?;
        Ok(item.id)
    }

    /// Удаляет одну ранее проверенную связь по её собственному UUID.
    pub(in crate::messenger) async fn delete_folder_item(
        &self,
        owner: &OwnerContext,
        token: &str,
        id: Uuid,
    ) -> Result<(), FolderError> {
        self.folder_command(
            owner,
            token,
            Method::DELETE,
            &format!("{ITEMS_PATH}{id}"),
            None,
        )
        .await?;
        Ok(())
    }

    /// Меняет закрепление связи и проверяет, что ответ относится к исходному чату.
    pub(in crate::messenger) async fn set_folder_item_pinned(
        &self,
        owner: &OwnerContext,
        token: &str,
        folder_id: Uuid,
        item: &FolderItem,
        pinned: bool,
    ) -> Result<(), FolderError> {
        let binding = FolderOwner::parse(owner)?;
        let action = if pinned { "pin" } else { "unpin" };
        let path = format!("{ITEMS_PATH}{}/actions/{action}/invoke", item.id);
        let response = self
            .folder_command(owner, token, Method::POST, &path, None)
            .await?;
        item_command_response(response, binding, folder_id, |current| {
            current.id == item.id
                && current.stream_id == item.stream_id
                && current.pinned_at.is_some() == pinned
        })
        .await?;
        Ok(())
    }

    /// Выполняет один запрос без повторов. Различает явный отказ и неопределённый результат.
    async fn folder_command(
        &self,
        owner: &OwnerContext,
        token: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Response, FolderError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(path);
        let mut request = self
            .send
            .request(method.clone(), url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let operation = Uuid::new_v4();
        log::debug!("Команда папок отправляется: op_id={operation} method={method}");
        let response = request.send().await.map_err(|_| {
            log::debug!("Ответ на команду папок потерян: op_id={operation}");
            FolderError::Unknown
        })?;
        let status = response.status();
        log::debug!("Получен ответ на команду папок: op_id={operation} status={status}");
        match status {
            StatusCode::OK | StatusCode::CREATED => Ok(response),
            StatusCode::NO_CONTENT if method == Method::DELETE => Ok(response),
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => {
                Err(FolderError::Unknown)
            }
            status if status.is_client_error() => Err(FolderError::Rejected(status.as_u16())),
            _ => Err(FolderError::Unknown),
        }
    }
}

/// UUID проекта и пользователя, которые сверяем с данными API.
#[derive(Clone, Copy)]
struct FolderOwner {
    project_id: Uuid, // Проект, к которому выдан токен.
    user_id: Uuid,    // UUID пользователя из сессии.
}

impl FolderOwner {
    /// Разбирает UUID сессии один раз перед обработкой списка.
    fn parse(owner: &OwnerContext) -> Result<Self, SourceError> {
        let project_id =
            Uuid::parse_str(&owner.project_id).map_err(|_| SourceError::InvalidResponse)?;
        let user_id = Uuid::parse_str(&owner.user_id).map_err(|_| SourceError::InvalidResponse)?;
        Ok(Self {
            project_id,
            user_id,
        })
    }
}

/// Поля папки из ответа API. В некоторых версиях верхний уровень не содержит владельца.
#[derive(Deserialize)]
struct WireFolder {
    uuid: Uuid,                          // Устойчивый UUID папки.
    project_id: Option<Uuid>,            // Проект, если сервер его включил.
    user_uuid: Option<Uuid>,             // Пользователь, если сервер его включил.
    title: String,                       // Название на сервере.
    background_color_value: Option<u32>, // Числовой цвет или отсутствие цвета.
    system_type: Option<String>,         // Тип от сервера, который уточняют фиксированные UUID.
    unread_count: u32,                   // Готовый агрегат для папки.
    created_at: String,                  // Серверная отметка создания.
    updated_at: String,                  // Серверная отметка обновления.
    folder_items: Vec<WireFolderItem>,   // Присутствующий массив означает полный состав.
}

impl WireFolder {
    /// Проверяет владельца и полный состав, сохраняя исходный порядок.
    fn into_folder(self, owner: FolderOwner) -> Result<Folder, SourceError> {
        if self.project_id.is_some_and(|id| id != owner.project_id)
            || self.user_uuid.is_some_and(|id| id != owner.user_id)
        {
            return Err(SourceError::InvalidResponse);
        }
        let mut items: Vec<FolderItem> = Vec::with_capacity(self.folder_items.len());
        let mut item_indices = HashMap::<Uuid, usize>::new();
        let mut streams = HashSet::new();
        for row in self.folder_items {
            let item = row.into_item(owner, self.uuid)?;
            if let Some(index) = item_indices.get(&item.id) {
                if items[*index] != item {
                    return Err(SourceError::InvalidResponse);
                }
                continue;
            }
            if !streams.insert(item.stream_id) {
                return Err(SourceError::InvalidResponse);
            }
            item_indices.insert(item.id, items.len());
            items.push(item);
        }
        Ok(Folder {
            id: self.uuid,
            kind: FolderKind::from_server(self.uuid, self.system_type.as_deref()),
            title: self.title,
            background_color_value: self.background_color_value,
            unread_count: self.unread_count,
            created_at: folder_time(&self.created_at)?,
            updated_at: folder_time(&self.updated_at)?,
            items,
        })
    }
}

/// Серверные поля связи с чатом внутри папки или отдельного ресурса.
#[derive(Deserialize)]
struct WireFolderItem {
    uuid: Uuid,                // UUID связи папки с чатом.
    project_id: Uuid,          // Проект владельца связи.
    user_uuid: Uuid,           // Пользователь, которому видна связь.
    folder_uuid: Option<Uuid>, // UUID папки в обычном ответе.
    folder: Option<Uuid>,      // UUID папки с альтернативным именем поля в системном снимке.
    stream_uuid: Uuid,         // UUID разговора в каталоге.
    chat_type: ChatType,       // Тип группы, лички или канала.
    order_index: Option<i32>,  // Явная позиция сортировки.
    pinned_at: Option<String>, // Момент закрепления, если оно есть.
    unread_count: u32,         // Готовый счётчик строки.
    active_unread_count: u32,  // Активная часть счётчика.
    passive_unread_count: u32, // Пассивная часть счётчика.
    created_at: String,        // Серверная отметка создания связи.
    updated_at: String,        // Серверная отметка обновления связи.
}

impl WireFolderItem {
    /// Требует явного владельца и согласованной ссылки на родительскую папку.
    fn into_item(self, owner: FolderOwner, folder_id: Uuid) -> Result<FolderItem, SourceError> {
        if self.project_id != owner.project_id
            || self.user_uuid != owner.user_id
            || self.folder_uuid.or(self.folder) != Some(folder_id)
            || self.folder.is_some_and(|id| id != folder_id)
            || self.uuid.is_nil()
            || self.stream_uuid.is_nil()
        {
            return Err(SourceError::InvalidResponse);
        }
        Ok(FolderItem {
            id: self.uuid,
            stream_id: self.stream_uuid,
            chat_type: self.chat_type,
            pinned_at: self.pinned_at.as_deref().map(folder_time).transpose()?,
            order_index: self.order_index,
            unread_count: self.unread_count,
            active_unread_count: self.active_unread_count,
            passive_unread_count: self.passive_unread_count,
            created_at: folder_time(&self.created_at)?,
            updated_at: folder_time(&self.updated_at)?,
        })
    }
}

/// Формы ответа на изменение связи, которые поддерживает REST.
#[derive(Deserialize)]
#[serde(untagged)]
enum WireItemCommand {
    Item(WireFolderItem), // Ответ содержит изменённую связь.
    Folder(WireFolder),   // Ответ содержит папку со всей коллекцией связей.
}

/// Разбирает ответ на изменение папки без доверия непроверенному виду и составу.
async fn custom_folder_response(
    response: Response,
    owner: FolderOwner,
) -> Result<Folder, FolderError> {
    let row: WireFolder = response.json().await.map_err(|_| FolderError::Unknown)?;
    let folder = row.into_folder(owner).map_err(|_| FolderError::Unknown)?;
    if folder.kind != FolderKind::Custom {
        return Err(FolderError::Unknown);
    }
    Ok(folder)
}

/// Ищет подтверждение команды по ожидаемой папке и адресу конкретной связи.
async fn item_command_response(
    response: Response,
    owner: FolderOwner,
    folder_id: Uuid,
    matches: impl Fn(&FolderItem) -> bool,
) -> Result<FolderItem, FolderError> {
    let row: WireItemCommand = response.json().await.map_err(|_| FolderError::Unknown)?;
    match row {
        WireItemCommand::Item(row) => {
            let item = row
                .into_item(owner, folder_id)
                .map_err(|_| FolderError::Unknown)?;
            matches(&item).then_some(item).ok_or(FolderError::Unknown)
        }
        WireItemCommand::Folder(row) if row.uuid == folder_id => {
            let folder = row.into_folder(owner).map_err(|_| FolderError::Unknown)?;
            folder
                .items
                .into_iter()
                .find(matches)
                .ok_or(FolderError::Unknown)
        }
        WireItemCommand::Folder(_) => Err(FolderError::Unknown),
    }
}

/// Собирает полный список без повтора папки или одного UUID связи в двух папках.
fn normalize_folders(
    rows: Vec<WireFolder>,
    owner: FolderOwner,
) -> Result<Vec<Folder>, SourceError> {
    let mut folders: Vec<Folder> = Vec::with_capacity(rows.len());
    let mut folder_indices = HashMap::<Uuid, usize>::new();
    let mut item_ids = HashSet::new();
    for row in rows {
        let folder = row.into_folder(owner)?;
        if let Some(index) = folder_indices.get(&folder.id) {
            if folders[*index] != folder {
                return Err(SourceError::InvalidResponse);
            }
            continue;
        }
        if folder.items.iter().any(|item| !item_ids.insert(item.id)) {
            return Err(SourceError::InvalidResponse);
        }
        folder_indices.insert(folder.id, folders.len());
        folders.push(folder);
    }
    Ok(folders)
}

/// Приводит отметку к UTC. Старый SQL-снимок элемента может опускать суффикс UTC.
fn folder_time(raw: &str) -> Result<String, SourceError> {
    HistoryTime::parse(raw)
        .or_else(|| HistoryTime::parse(&format!("{raw}Z")))
        .map(|time| time.as_str().to_owned())
        .ok_or(SourceError::InvalidResponse)
}
