//! Читает начальную позицию и страницы журнала мессенджера по HTTPS.
//! Здесь же разбирается общий формат событий, который используют HTTP и WebSocket.

use std::{collections::HashSet, time::Duration};

use reqwest::{
    Certificate, Client, RequestBuilder, StatusCode, Url, header::ACCEPT, redirect::Policy,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    runtime::SourceError,
    storage::{Cursor, HistoryChange, HistoryMetadata, HistoryTime, Message, MessagePlacement},
    sync::Event,
};

const EPOCH_PATH: &str = "/api/workspace/v1/epoch/";
const EVENTS_PATH: &str = "/api/workspace/v1/events/";
const ALL_CHATS_FOLDER_ID: &str = "00000000-0000-0000-0000-000000000000";
const ALL_CHATS_PATH: &str =
    "/api/workspace/v1/messenger/folders/00000000-0000-0000-0000-000000000000";
pub(super) const STREAMS_PATH: &str = "/api/workspace/v1/messenger/streams";
pub(super) const TOPICS_PATH: &str = "/api/workspace/v1/messenger/stream_topics";
const PAGE_LIMIT: usize = 100;

/// HTTP-клиент для чтения журнала мессенджера.
pub(super) struct RestClient(Client); // Настроенный HTTP-клиент для запросов к журналу.

impl RestClient {
    /// Создаёт HTTPS-клиент с тайм-аутами и запретом перенаправлений.
    pub(super) fn new(private_ca: Option<&[u8]>) -> Result<Self, SourceError> {
        let mut builder = Client::builder()
            .https_only(true)
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20));
        if let Some(private_ca) = private_ca {
            let certificate =
                Certificate::from_der(private_ca).map_err(|_| SourceError::InvalidResponse)?;
            builder = builder
                .tls_built_in_root_certs(false)
                .add_root_certificate(certificate);
        }
        let client = builder.build().map_err(|_| SourceError::Temporary)?;
        Ok(Self(client))
    }

    /// Получает текущую позицию журнала, с которой начинается первый запуск.
    pub(super) async fn current_cursor(
        &self,
        owner: &OwnerContext,
        access_token: &str,
    ) -> Result<Cursor, SourceError> {
        let epoch: Epoch = get_json(self.request(owner, access_token, EPOCH_PATH)?).await?;
        epoch.into_cursor()
    }

    /// Загружает готовый счётчик системной папки «Все чаты» из контекста токена.
    pub(super) async fn unread_snapshot(
        &self,
        owner: &OwnerContext,
        access_token: &str,
    ) -> Result<u32, SourceError> {
        let folder: WireFolder =
            get_json(self.request(owner, access_token, ALL_CHATS_PATH)?).await?;
        folder.into_count(owner)
    }

    /// Читает все страницы каталога по маркерам продолжения от сервера.
    pub(super) async fn collection<T: DeserializeOwned>(
        &self,
        owner: &OwnerContext,
        token: &str,
        path: &str,
    ) -> Result<Vec<T>, SourceError> {
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        let mut marker: Option<String> = None;
        loop {
            let mut request = self
                .request(owner, token, path)?
                .query(&[("page_limit", PAGE_LIMIT)]);
            if let Some(marker) = &marker {
                request = request.query(&[("page_marker", marker)]);
            }
            let response = request.send().await.map_err(request_error)?;
            check_status(response.status())?;
            marker = response
                .headers()
                .get("X-Pagination-Marker")
                .map(|value| value.to_str().map(str::to_owned))
                .transpose()
                .map_err(|_| SourceError::InvalidResponse)?;
            let mut page: Vec<T> = response.json().await.map_err(request_error)?;
            rows.append(&mut page);
            let Some(next) = &marker else {
                return Ok(rows);
            };
            if next.is_empty() || !seen.insert(next.clone()) {
                return Err(SourceError::InvalidResponse);
            }
        }
    }

    /// Проверяет принадлежность к личному разговору и применяет настройки его уведомлений.
    pub(super) async fn can_notify(
        &self,
        owner: &OwnerContext,
        access_token: &str,
        incoming: &IncomingMessage,
    ) -> Result<bool, SourceError> {
        let stream_path = format!("{STREAMS_PATH}/{}", incoming.stream);
        let stream: WireStream = get_json(self.request(owner, access_token, &stream_path)?).await?;
        if !stream.matches(owner, incoming) {
            return Err(SourceError::InvalidResponse);
        }
        if !stream.private
            || stream.is_archived
            || stream.direct_user_uuid.as_deref().and_then(parse_uuid) != Some(incoming.author)
        {
            return Ok(false);
        }
        let topic_path = format!("{TOPICS_PATH}/{}", incoming.topic);
        let topic: WireTopic = get_json(self.request(owner, access_token, &topic_path)?).await?;
        if !topic.matches(owner, incoming) {
            return Err(SourceError::InvalidResponse);
        }
        Ok(topic.notification_mode.allows(stream.notification_mode))
    }

    /// Загружает и проверяет одну страницу событий строго после заданной позиции.
    pub(super) async fn page(
        &self,
        owner: &OwnerContext,
        access_token: &str,
        after: &Cursor,
    ) -> Result<Vec<Event>, SourceError> {
        let request = self.page_request(owner, access_token, after)?;
        let wire: Vec<Value> = get_json(request).await?;
        decode_page(wire, owner, after)
    }

    /// Добавляет к запросу метку журнала, нижнюю границу позиции и размер страницы.
    fn page_request(
        &self,
        owner: &OwnerContext,
        access_token: &str,
        after: &Cursor,
    ) -> Result<RequestBuilder, SourceError> {
        validate_cursor(after)?;
        Ok(self.request(owner, access_token, EVENTS_PATH)?.query(&[
            ("epoch_version>", after.epoch_version.to_string()),
            ("epoch_generation", after.epoch_generation.clone()),
            ("page_limit", PAGE_LIMIT.to_string()),
        ]))
    }

    /// Собирает GET-запрос к серверу владельца и добавляет токен доступа.
    fn request(
        &self,
        owner: &OwnerContext,
        access_token: &str,
        path: &str,
    ) -> Result<RequestBuilder, SourceError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(path);
        Ok(self
            .0
            .get(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(access_token))
    }
}

/// Проверяет, что адрес владельца содержит базовый HTTPS-адрес сервера.
pub(super) fn workspace_origin(owner: &OwnerContext) -> Result<Url, SourceError> {
    let url = Url::parse(&owner.server).map_err(|_| SourceError::InvalidResponse)?;
    if url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SourceError::InvalidResponse);
    }
    Ok(url)
}

/// Выполняет запрос, проверяет HTTP-статус и разбирает JSON.
async fn get_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, SourceError> {
    let response = request.send().await.map_err(request_error)?;
    check_status(response.status())?;
    response.json().await.map_err(request_error)
}

/// Переводит статус HTTP в ошибку источника, которую понимает синхронизация.
fn check_status(status: StatusCode) -> Result<(), SourceError> {
    match status {
        StatusCode::OK => Ok(()),
        StatusCode::GONE => Err(SourceError::CursorPruned),
        StatusCode::UNAUTHORIZED => Err(SourceError::Unauthorized),
        StatusCode::FORBIDDEN => Err(SourceError::Forbidden),
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => Err(SourceError::Temporary),
        status if status.is_server_error() => Err(SourceError::Temporary),
        _ => Err(SourceError::InvalidResponse),
    }
}

/// Различает неверный запрос или ответ и временный сбой транспорта.
fn request_error(error: reqwest::Error) -> SourceError {
    if error.is_decode() || error.is_builder() {
        SourceError::InvalidResponse
    } else {
        SourceError::Temporary
    }
}

/// Ответ сервера с текущей позицией в журнале.
#[derive(Deserialize)]
struct Epoch {
    epoch_generation: String, // Метка текущего журнала.
    epoch_version: u64,       // Последний номер события в журнале.
}

impl Epoch {
    /// Преобразует ответ сервера в позицию и проверяет допустимый диапазон.
    fn into_cursor(self) -> Result<Cursor, SourceError> {
        let cursor = Cursor {
            epoch_generation: self.epoch_generation,
            epoch_version: self.epoch_version,
        };
        validate_cursor(&cursor)?;
        Ok(cursor)
    }
}

/// Разбирает страницу и проверяет рост позиций. Пропуски между видимыми событиями допустимы.
fn decode_page(
    wire: Vec<Value>,
    owner: &OwnerContext,
    after: &Cursor,
) -> Result<Vec<Event>, SourceError> {
    let mut last_version = after.epoch_version;
    wire.into_iter()
        .map(|value| {
            let (event, _) = wire_event(value, owner, &after.epoch_generation)?;
            let version = event.cursor().epoch_version;
            if version <= last_version {
                return Err(SourceError::InvalidResponse);
            }
            last_version = version;
            Ok(event)
        })
        .collect()
}

/// Внешние поля события в JSON, полученном от сервера.
#[derive(Deserialize)]
struct WireEnvelope {
    schema_version: u64, // Версия формата события. Поддерживается версия 1.
    uuid: String,        // Идентификатор события в журнале.
    epoch_generation: Option<String>, // Метка журнала, если сервер прислал её в самом событии.
    epoch_version: u64,  // Номер события внутри журнала.
    project_id: String,  // Идентификатор проекта, которому принадлежит событие.
    user_uuid: String,   // Идентификатор пользователя, для которого выдано событие.
    object_type: String, // Тип объекта, например сообщение.
    action: String,      // Что произошло с объектом, например создание.
    payload: WirePayload, // Данные события и признак его вида.
}

/// Вложенные данные события из JSON.
#[derive(Deserialize)]
struct WirePayload {
    kind: String,                // Название события, например `message.created`.
    uuid: Option<String>,        // Идентификатор объекта. Для нового сообщения обязателен.
    project_id: Option<String>, // Идентификатор проекта, если сервер повторил его во вложенных данных.
    user_uuid: Option<String>, // Идентификатор пользователя, если сервер повторил его во вложенных данных.
    payload: Option<Value>, // Тело объекта. Для нового сообщения здесь лежат формат и содержимое.
    unread_count: Option<u32>, // Готовый счётчик в событии обновления папки.
    source_name: Option<String>, // Источник сообщения. Первый срез показывает баннеры Workspace.
    is_own: Option<bool>,   // Признак, что владелец сам отправил сообщение.
    read: Option<bool>,     // Признак, что владелец уже прочитал сообщение.
    author_uuid: Option<String>, // Идентификатор автора сообщения.
    stream_uuid: Option<String>, // Идентификатор разговора.
    topic_uuid: Option<String>, // Идентификатор темы внутри разговора.
    created_at: Option<String>, // Серверное время создания сообщения.
    updated_at: Option<String>, // Серверное время последней правки.
}

/// Тело сообщения в ответе сервера.
#[derive(Deserialize)]
struct WireMessageBody {
    kind: String,           // Формат содержимого. Текст сохраняется для формата `markdown`.
    content: Option<Value>, // Содержимое сообщения. Для Markdown это обязательная строка.
}

/// Данные для проверки права на баннер по живому сообщению.
pub(super) struct IncomingMessage {
    pub(super) stream: Uuid, // Идентификатор разговора из события.
    topic: Uuid,             // Идентификатор темы, чьи настройки могут переопределить разговор.
    author: Uuid,            // Идентификатор автора. Для личного разговора это собеседник.
}

impl IncomingMessage {
    /// Возвращает данные входящего непрочитанного сообщения из Workspace.
    fn from_wire(payload: &WirePayload, owner: &OwnerContext) -> Option<Self> {
        if payload.source_name.as_deref() != Some("native")
            || payload.is_own != Some(false)
            || payload.read != Some(false)
        {
            return None;
        }
        let author = parse_uuid(payload.author_uuid.as_deref()?)?;
        if parse_uuid(&owner.user_id)? == author {
            return None;
        }
        Some(Self {
            stream: parse_uuid(payload.stream_uuid.as_deref()?)?,
            topic: parse_uuid(payload.topic_uuid.as_deref()?)?,
            author,
        })
    }
}

impl WirePayload {
    /// Извлекает поля для упорядоченной истории. Старые снимки могут не содержать даты.
    fn history_metadata(&self) -> Option<HistoryMetadata> {
        Some(HistoryMetadata {
            author_id: self.author_uuid.as_deref().and_then(parse_uuid),
            created_at: HistoryTime::parse(self.created_at.as_deref()?)?,
            updated_at: self.updated_at.as_deref().and_then(HistoryTime::parse),
        })
    }

    /// Возвращает адрес, если снимок содержит стрим и тему.
    fn placement(&self) -> Option<MessagePlacement> {
        Some(MessagePlacement {
            stream_id: parse_uuid(self.stream_uuid.as_deref()?)?.to_string(),
            topic_id: parse_uuid(self.topic_uuid.as_deref()?)?.to_string(),
        })
    }
}

/// Счётчик и идентификаторы папки в ответе REST.
#[derive(Deserialize)]
struct WireFolder {
    uuid: String,               // Идентификатор системной папки для проверки.
    project_id: Option<String>, // Проект. Публичный запрос может не возвращать это поле.
    user_uuid: Option<String>,  // Пользователь. Публичный запрос может не возвращать это поле.
    unread_count: u32,          // Готовое число непрочитанных.
}

impl WireFolder {
    /// Проверяет папку и возвращает её счётчик.
    fn into_count(self, owner: &OwnerContext) -> Result<u32, SourceError> {
        if self.uuid != ALL_CHATS_FOLDER_ID
            || !matches_owner(owner, self.project_id.as_deref(), self.user_uuid.as_deref())
        {
            return Err(SourceError::InvalidResponse);
        }
        Ok(self.unread_count)
    }
}

/// Сведения о разговоре, которые нужны для проверки системного уведомления.
#[derive(Deserialize)]
struct WireStream {
    uuid: String,                              // Идентификатор запрошенного разговора.
    project_id: Option<String>,                // Проект, если он есть в ответе.
    user_uuid: Option<String>,                 // Пользователь, если он есть в ответе.
    private: bool,                             // Признак личного разговора.
    is_archived: bool,                         // Признак архивного разговора.
    direct_user_uuid: Option<String>,          // Идентификатор собеседника для личного разговора.
    notification_mode: StreamNotificationMode, // Настройка уведомлений разговора.
}

impl WireStream {
    /// Проверяет идентификатор разговора и владельца данных.
    fn matches(&self, owner: &OwnerContext, incoming: &IncomingMessage) -> bool {
        parse_uuid(&self.uuid) == Some(incoming.stream)
            && matches_owner(owner, self.project_id.as_deref(), self.user_uuid.as_deref())
    }
}

/// Сведения о теме, чья настройка может переопределить уведомления разговора.
#[derive(Deserialize)]
struct WireTopic {
    uuid: String,                             // Идентификатор запрошенной темы.
    stream_uuid: String,                      // Идентификатор разговора, которому принадлежит тема.
    project_id: Option<String>,               // Проект, если он есть в ответе.
    user_uuid: Option<String>,                // Пользователь, если он есть в ответе.
    notification_mode: TopicNotificationMode, // Настройка уведомлений темы.
}

impl WireTopic {
    /// Проверяет идентификаторы темы, разговора и владельца данных.
    fn matches(&self, owner: &OwnerContext, incoming: &IncomingMessage) -> bool {
        parse_uuid(&self.uuid) == Some(incoming.topic)
            && parse_uuid(&self.stream_uuid) == Some(incoming.stream)
            && matches_owner(owner, self.project_id.as_deref(), self.user_uuid.as_deref())
    }
}

/// Настройка уведомлений отдельного разговора на сервере.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum StreamNotificationMode {
    AllMessages,  // Включены уведомления о сообщениях.
    MentionsOnly, // Настройка канала. В личном разговоре сообщения уведомляют.
    Muted,        // Уведомления отключены, если тема не переопределяет настройку.
}

/// Настройка уведомлений отдельной темы на сервере.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TopicNotificationMode {
    Default, // Использовать настройку разговора.
    Mute,    // Отключить уведомления темы.
    Unmute,  // Включить уведомления темы.
    Follow,  // Следить за темой и получать её уведомления.
}

impl TopicNotificationMode {
    /// Решает, разрешают ли настройки уведомление о входящем личном сообщении.
    fn allows(self, stream: StreamNotificationMode) -> bool {
        match self {
            Self::Mute => false,
            Self::Default => !matches!(stream, StreamNotificationMode::Muted),
            Self::Unmute | Self::Follow => true,
        }
    }
}

/// Проверяет поля владельца, когда сервер повторяет их в полезной нагрузке.
fn matches_owner(owner: &OwnerContext, project: Option<&str>, user: Option<&str>) -> bool {
    project.is_none_or(|id| id == owner.project_id) && user.is_none_or(|id| id == owner.user_id)
}

/// Разбирает серверный UUID для сравнения идентификаторов и построения URL.
fn parse_uuid(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value).ok()
}

/// Проверяет контекст и преобразует событие журнала в изменение для Sync.
pub(super) fn wire_event(
    value: Value,
    owner: &OwnerContext,
    epoch: &str,
) -> Result<(Event, Option<IncomingMessage>), SourceError> {
    let wire: WireEnvelope =
        serde_json::from_value(value).map_err(|_| SourceError::InvalidResponse)?;
    let cursor = Cursor {
        epoch_generation: epoch.to_owned(),
        epoch_version: wire.epoch_version,
    };
    validate_cursor(&cursor)?;
    if cursor.epoch_version == 0
        || wire.schema_version != 1
        || !nonempty(&wire.uuid)
        || !nonempty(&wire.object_type)
        || !nonempty(&wire.action)
        || !nonempty(&wire.payload.kind)
        || wire.project_id != owner.project_id
        || wire.user_uuid != owner.user_id
    {
        return Err(SourceError::InvalidResponse);
    }
    if wire
        .epoch_generation
        .as_deref()
        .is_some_and(|value| value != epoch)
    {
        return Err(SourceError::CursorPruned);
    }

    let message_created = wire.object_type == "message" && wire.action == "created";
    if message_created != (wire.payload.kind == "message.created") {
        return Err(SourceError::InvalidResponse);
    }
    if wire.object_type == "folder"
        && matches!(
            wire.payload.kind.as_str(),
            "folder.updated" | "folder.created"
        )
        && wire.payload.uuid.as_deref() == Some(ALL_CHATS_FOLDER_ID)
    {
        if wire.payload.kind.strip_prefix("folder.") != Some(wire.action.as_str()) {
            return Err(SourceError::InvalidResponse);
        }
        let folder = WireFolder {
            uuid: wire.payload.uuid.ok_or(SourceError::InvalidResponse)?,
            project_id: wire.payload.project_id,
            user_uuid: wire.payload.user_uuid,
            unread_count: wire
                .payload
                .unread_count
                .ok_or(SourceError::InvalidResponse)?,
        };
        let count = folder.into_count(owner)?;
        return Ok((Event::UnreadChanged { cursor, count }, None));
    }
    let message_updated = wire.object_type == "message" && wire.action == "updated";
    let history = match (wire.object_type.as_str(), wire.action.as_str()) {
        ("message" | "topic" | "stream", "deleted") => {
            let payload = &wire.payload;
            if payload.kind.strip_suffix(".deleted") != Some(wire.object_type.as_str())
                || !matches_owner(
                    owner,
                    payload.project_id.as_deref(),
                    payload.user_uuid.as_deref(),
                )
            {
                return Err(SourceError::InvalidResponse);
            }
            // Отзыв стрима приходит только удалённому участнику. Событие stream_binding.deleted получают остальные.
            let id = parse_uuid(
                payload
                    .uuid
                    .as_deref()
                    .ok_or(SourceError::InvalidResponse)?,
            )
            .ok_or(SourceError::InvalidResponse)?
            .to_string();
            Some(match wire.object_type.as_str() {
                "stream" => HistoryChange::RemoveStream(id),
                "topic" => HistoryChange::RemoveTopic(MessagePlacement {
                    stream_id: payload
                        .stream_uuid
                        .as_deref()
                        .and_then(parse_uuid)
                        .ok_or(SourceError::InvalidResponse)?
                        .to_string(),
                    topic_id: id,
                }),
                _ => HistoryChange::RemoveMessage(id),
            })
        }
        _ => None,
    };
    if let Some(change) = history {
        return Ok((Event::HistoryChanged { cursor, change }, None));
    }
    if !message_created && !message_updated {
        return Ok((Event::Checkpoint(cursor), None));
    }
    if message_updated && wire.payload.kind != "message.updated" {
        return Err(SourceError::InvalidResponse);
    }

    let payload = wire.payload;
    let placement = payload.placement();
    let metadata = payload.history_metadata();
    let incoming = IncomingMessage::from_wire(&payload, owner);
    let id = payload
        .uuid
        .filter(|id| nonempty(id))
        .ok_or(SourceError::InvalidResponse)?;
    let id = parse_uuid(&id).map_or(id, |uuid| uuid.to_string());
    if !matches_owner(
        owner,
        payload.project_id.as_deref(),
        payload.user_uuid.as_deref(),
    ) {
        return Err(SourceError::InvalidResponse);
    }
    let body: WireMessageBody =
        serde_json::from_value(payload.payload.ok_or(SourceError::InvalidResponse)?)
            .map_err(|_| SourceError::InvalidResponse)?;
    if !nonempty(&body.kind) {
        return Err(SourceError::InvalidResponse);
    }
    if body.kind != "markdown" {
        if message_updated {
            return Ok((
                Event::HistoryChanged {
                    cursor,
                    change: HistoryChange::RemoveMessage(id),
                },
                None,
            ));
        }
        return Ok((Event::Checkpoint(cursor), None));
    }
    let Some(Value::String(text)) = body.content else {
        return Err(SourceError::InvalidResponse);
    };
    let message = Message { id, text };
    if message_updated {
        return Ok((
            Event::HistoryChanged {
                cursor,
                change: HistoryChange::Edit { message, placement },
            },
            None,
        ));
    }
    Ok((
        Event::MessageCreated {
            cursor,
            message,
            placement,
            metadata,
        },
        incoming,
    ))
}

/// Проверяет метку журнала и то, что номер позиции помещается в SQLite.
pub(super) fn validate_cursor(cursor: &Cursor) -> Result<(), SourceError> {
    if !nonempty(&cursor.epoch_generation) || i64::try_from(cursor.epoch_version).is_err() {
        return Err(SourceError::InvalidResponse);
    }
    Ok(())
}

/// Проверяет, что строка непустая и без пробелов по краям.
fn nonempty(value: &str) -> bool {
    !value.is_empty() && value == value.trim()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::context::AccountId;

    /// Создаёт владельца данных для тестов журнала.
    fn owner() -> OwnerContext {
        OwnerContext {
            account_id: AccountId("local-account".into()),
            server: "https://example.test".into(),
            organization_id: "organization".into(),
            project_id: "project".into(),
            user_id: "recipient".into(),
        }
    }

    /// Создаёт позицию тестового журнала с заданным номером.
    fn cursor(version: u64) -> Cursor {
        Cursor {
            epoch_generation: "epoch".to_owned(),
            epoch_version: version,
        }
    }

    /// Создаёт JSON события о новом сообщении для теста.
    fn message(version: u64) -> Value {
        json!({
            "schema_version": 1,
            "uuid": "event-uuid",
            "epoch_version": version,
            "project_id": "project",
            "user_uuid": "recipient",
            "object_type": "message",
            "action": "created",
            "payload": {
                "kind": "message.created",
                "uuid": "message-uuid",
                "project_id": "project",
                "user_uuid": "recipient",
                "payload": {"kind": "markdown", "content": "hello"}
            }
        })
    }

    /// Проверяет, что запрос страницы передаёт токен и строгое ограничение по позиции.
    #[test]
    fn pages_request_strict_cursor_and_bearer_token() {
        let client = RestClient::new(None).unwrap();
        let request = client
            .page_request(&owner(), "test-token", &cursor(42))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.url().path(), EVENTS_PATH);
        assert_eq!(
            request.url().query_pairs().into_owned().collect::<Vec<_>>(),
            [
                ("epoch_version>".to_owned(), "42".to_owned()),
                ("epoch_generation".to_owned(), "epoch".to_owned()),
                ("page_limit".to_owned(), "100".to_owned())
            ]
        );
        let authorization = &request.headers()[reqwest::header::AUTHORIZATION];
        assert_eq!(authorization, "Bearer test-token");
        assert!(authorization.is_sensitive());
        assert_eq!(
            check_status(StatusCode::GONE),
            Err(SourceError::CursorPruned)
        );
    }

    /// Проверяет чтение событий и допустимость пропусков в их номерах.
    #[test]
    fn decodes_flat_events_and_allows_gaps_in_visible_versions() {
        let owner = owner();
        let mut update = message(100);
        update["action"] = json!("updated");
        update["payload"]["kind"] = json!("message.updated");

        assert_eq!(
            decode_page(vec![message(20), update], &owner, &cursor(3)).unwrap(),
            [
                Event::MessageCreated {
                    cursor: cursor(20),
                    message: Message {
                        id: "message-uuid".to_owned(),
                        text: "hello".to_owned(),
                    },
                    placement: None,
                    metadata: None,
                },
                Event::HistoryChanged {
                    cursor: cursor(100),
                    change: HistoryChange::Edit {
                        message: Message {
                            id: "message-uuid".to_owned(),
                            text: "hello".to_owned()
                        },
                        placement: None,
                    },
                }
            ]
        );
    }

    /// Проверяет счётчик системной папки и исключает подмену другой папкой или аккаунтом.
    #[test]
    fn applies_only_all_chats_folder_snapshots_for_the_current_owner() {
        let owner = owner();
        let mut event = json!({
            "schema_version": 1,
            "uuid": "event-uuid",
            "epoch_version": 15,
            "project_id": "project",
            "user_uuid": "recipient",
            "object_type": "folder",
            "action": "updated",
            "payload": {
                "kind": "folder.updated",
                "uuid": ALL_CHATS_FOLDER_ID,
                "project_id": "project",
                "user_uuid": "recipient",
                "unread_count": 30,
                "updated_at": "2026-11-13T14:30:00.123Z"
            }
        });
        assert_eq!(
            decode_page(vec![event.clone()], &owner, &cursor(14)).unwrap(),
            [Event::UnreadChanged {
                cursor: cursor(15),
                count: 30,
            },]
        );
        event["payload"]["user_uuid"] = json!("other");
        assert_eq!(
            decode_page(vec![event.clone()], &owner, &cursor(14)),
            Err(SourceError::InvalidResponse)
        );
        event["payload"]["uuid"] = json!("another-folder");
        assert_eq!(
            decode_page(vec![event], &owner, &cursor(14)).unwrap(),
            [Event::Checkpoint(cursor(15))]
        );
    }

    /// Проверяет признаки сообщения, которые разрешают проверку правил уведомлений.
    #[test]
    fn only_incoming_unread_workspace_markdown_can_trigger_notification() {
        let mut owner = owner();
        owner.user_id = "ef59d055-6d90-4a63-9e32-0e7aac92bd03".into();
        let mut value = message(5);
        value["user_uuid"] = json!(owner.user_id);
        let payload = &mut value["payload"];
        payload["user_uuid"] = json!(owner.user_id);
        payload["source_name"] = json!("native");
        payload["author_uuid"] = json!("5ec0cc41-b59e-4b1c-96f0-8104180c09fb");
        payload["is_own"] = json!(false);
        payload["read"] = json!(false);
        payload["stream_uuid"] = json!("e4b290f4-9608-4103-a2a0-e55be0e21f5e");
        payload["topic_uuid"] = json!("32908e49-81f4-4c50-8bc3-2bb141038181");
        assert!(
            wire_event(value.clone(), &owner, "epoch")
                .unwrap()
                .1
                .is_some()
        );
        for (field, unsupported) in [
            ("is_own", json!(true)),
            ("read", json!(true)),
            ("source_name", json!("zulip")),
            ("author_uuid", json!(owner.user_id)),
            ("payload", json!({"kind": "e2ee"})),
        ] {
            let mut excluded = value.clone();
            excluded["payload"][field] = unsupported;
            assert!(wire_event(excluded, &owner, "epoch").unwrap().1.is_none());
        }
        // REST использует тот же разбор сообщения, но не передаёт данные для уведомлений.
        assert!(matches!(
            decode_page(vec![value], &owner, &cursor(4))
                .unwrap()
                .as_slice(),
            [Event::MessageCreated { .. }]
        ));
    }

    /// Проверяет, что неверное сообщение или чужие данные не продвигают позицию.
    #[test]
    fn refuses_malformed_created_messages_or_wrong_scope_before_advancing() {
        let owner = owner();
        let mut invalid = message(5);
        invalid["payload"]["payload"]["content"] = Value::Null;
        assert_eq!(
            decode_page(vec![message(4), invalid, message(6)], &owner, &cursor(3)),
            Err(SourceError::InvalidResponse)
        );

        let mut wrong_owner = message(4);
        wrong_owner["user_uuid"] = json!("other");
        assert_eq!(
            wire_event(wrong_owner, &owner, "epoch").map(|(event, _)| event),
            Err(SourceError::InvalidResponse)
        );
        let mut wrong_project = message(4);
        wrong_project["payload"]["project_id"] = json!("other");
        assert_eq!(
            wire_event(wrong_project, &owner, "epoch").map(|(event, _)| event),
            Err(SourceError::InvalidResponse)
        );
        assert_eq!(
            decode_page(vec![message(5), message(4)], &owner, &cursor(3)),
            Err(SourceError::InvalidResponse)
        );
        let mut future_schema = message(4);
        future_schema["schema_version"] = json!(2);
        assert_eq!(
            wire_event(future_schema, &owner, "epoch").map(|(event, _)| event),
            Err(SourceError::InvalidResponse)
        );
    }
}
