//! Работает с HTTPS API мессенджера: читает настройки, журнал и историю, отправляет сообщения.
//! Также разбирает общий формат событий, который используют HTTP и WebSocket.

mod folders;
mod message_actions;
pub(super) mod mutations;
pub(super) mod sidebar;

use std::{collections::HashSet, time::Duration};

use reqwest::{
    Certificate, Client, RequestBuilder, StatusCode, Url, header::ACCEPT, redirect::Policy,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    directory::normalize_avatar_reference,
    runtime::SourceError,
    storage::{
        Cursor, FolderChange, HistoryChange, HistoryEdge, HistoryMessage, HistoryMetadata,
        HistoryScope, HistoryTime, Message, MessagePlacement,
    },
    sync::{AvatarUpdate, Event},
};

use super::catalog::CatalogCounts;
use super::details::{DetailsError, StreamNotificationMode, TopicNotificationMode};
use super::history::OpenHistoryError;
use super::read::ReadError;
use super::send::{DirectSendError, SendError};
use crate::storage::PreparedSend;

const MESSAGES_PATH: &str = "/api/workspace/v1/messenger/messages/";
const WINDOW_LIMIT: &str = "50";
const EPOCH_PATH: &str = "/api/workspace/v1/epoch/";
const EVENTS_PATH: &str = "/api/workspace/v1/events/";
const SERVER_SETTINGS_PATH: &str = "/api/workspace/v1/messenger/server_settings";
const ALL_CHATS_FOLDER_ID: &str = "00000000-0000-0000-0000-000000000000";
const ALL_CHATS_PATH: &str =
    "/api/workspace/v1/messenger/folders/00000000-0000-0000-0000-000000000000";
pub(super) const STREAMS_PATH: &str = "/api/workspace/v1/messenger/streams";
pub(super) const TOPICS_PATH: &str = "/api/workspace/v1/messenger/stream_topics";
const PAGE_LIMIT: usize = 100;

/// HTTP-клиенты для чтения и однократной отправки сообщений.
pub(super) struct RestClient {
    read: Client, // Клиент для чтения журнала и истории.
    send: Client, // Клиент отправки с полностью отключёнными повторами.
}

impl RestClient {
    /// Однократно отправляет отметку и проверяет адрес возвращённого сообщения.
    pub(super) async fn read_up_to(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        message_id: Uuid,
    ) -> Result<(), ReadError> {
        let mut url = workspace_origin(owner).map_err(|_| ReadError::InvalidInput)?;
        url.set_path(&format!(
            "{MESSAGES_PATH}{message_id}/actions/read_up_to/invoke"
        ));
        let response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| ReadError::Unknown)?;
        if response.status() != StatusCode::OK {
            return Err(ReadError::Http(response.status().as_u16()));
        }
        let row: WireHistoryMessage = response.json().await.map_err(|_| ReadError::Unknown)?;
        if row.uuid != message_id
            || row.stream_uuid != scope.stream_id
            || scope.topic_id.is_some_and(|topic| topic != row.topic_uuid)
            || !matches_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref())
            || HistoryTime::parse(&row.created_at).is_none()
            || row
                .updated_at
                .as_deref()
                .is_some_and(|time| HistoryTime::parse(time).is_none())
            || row.payload.kind.is_empty()
        {
            return Err(ReadError::Unknown);
        }
        Ok(())
    }

    /// Читает одно сообщение и принимает его только при совпадении владельца и маршрута.
    pub(super) async fn lookup_prepared(
        &self,
        owner: &OwnerContext,
        token: &str,
        snapshot: &PreparedSend,
    ) -> Result<HistoryMessage, SendError> {
        let mut url = workspace_origin(owner).map_err(|_| SendError::InvalidInput)?;
        let author = Uuid::parse_str(&owner.user_id).map_err(|_| SendError::InvalidInput)?;
        if snapshot.public_id
            != Uuid::new_v5(
                &snapshot.topic_id,
                snapshot.canonical_id.to_string().as_bytes(),
            )
        {
            return Err(SendError::InvalidInput);
        }
        url.set_path(&format!("{MESSAGES_PATH}{}", snapshot.public_id));
        let response = self
            .send
            .get(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| SendError::Unknown)?;
        if response.status() != StatusCode::OK {
            return Err(SendError::Unknown);
        }
        let row: WireHistoryMessage = response.json().await.map_err(|_| SendError::Unknown)?;
        if row.uuid != snapshot.public_id
            || row.stream_uuid != snapshot.stream_id
            || row.topic_uuid != snapshot.topic_id
            || row.author_uuid != author
        {
            return Err(SendError::Unknown);
        }
        row.into_history(owner, snapshot.stream_id)
            .map_err(|_| SendError::Unknown)?
            .ok_or(SendError::Unknown)
    }

    /// Выполняет ровно одну попытку отправки снимка и разбирает ответ сервера.
    pub(super) async fn post_prepared(
        &self,
        owner: &OwnerContext,
        token: &str,
        snapshot: &PreparedSend,
    ) -> Result<HistoryMessage, SendError> {
        let mut url = workspace_origin(owner).map_err(|_| SendError::InvalidInput)?;
        if snapshot.public_id
            != Uuid::new_v5(
                &snapshot.topic_id,
                snapshot.canonical_id.to_string().as_bytes(),
            )
        {
            return Err(SendError::InvalidInput);
        }
        url.set_path(MESSAGES_PATH);
        let response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .json(&serde_json::json!({
                "uuid": snapshot.canonical_id,
                "stream_uuid": snapshot.stream_id,
                "topic_uuid": snapshot.topic_id,
                "payload": { "kind": "markdown", "content": snapshot.markdown },
            }))
            .send()
            .await
            .map_err(|_| SendError::Unknown)?;
        let status = response.status();
        // Тайм-аут и ошибки сервера не доказывают, что запись не состоялась.
        if status.is_client_error()
            && status != StatusCode::REQUEST_TIMEOUT
            && status != StatusCode::TOO_MANY_REQUESTS
        {
            return Err(SendError::Rejected(status.as_u16()));
        }
        if status != StatusCode::CREATED {
            return Err(SendError::Unknown);
        }
        let row: WireHistoryMessage = response.json().await.map_err(|_| SendError::Unknown)?;
        if row.uuid != snapshot.public_id {
            return Err(SendError::Unknown);
        }
        // Маршрут ответа сохраняем как пришёл: сообщение могло быть перенесено.
        let stream = row.stream_uuid;
        row.into_history(owner, stream)
            .map_err(|_| SendError::Unknown)?
            .ok_or(SendError::Unknown)
    }

    /// Создаёт HTTPS-клиент с тайм-аутами и запретом перенаправлений.
    pub(super) fn new(private_ca: Option<&[u8]>) -> Result<Self, SourceError> {
        Ok(Self {
            read: Self::client(private_ca, false)?,
            send: Self::client(private_ca, true)?,
        })
    }

    /// Настраивает TLS и при необходимости запрещает даже внутренние повторы транспорта.
    fn client(private_ca: Option<&[u8]>, no_retry: bool) -> Result<Client, SourceError> {
        let mut builder = Client::builder()
            .https_only(true)
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20));
        if no_retry {
            builder = builder.retry(reqwest::retry::never());
        }
        if let Some(private_ca) = private_ca {
            let certificate =
                Certificate::from_der(private_ca).map_err(|_| SourceError::InvalidResponse)?;
            builder = builder
                .tls_built_in_root_certs(false)
                .add_root_certificate(certificate);
        }
        let client = builder.build().map_err(|_| SourceError::Temporary)?;
        Ok(client)
    }

    /// Читает ссылку регистрации из публичных настроек, доступных до входа.
    pub(super) async fn registration_url(
        &self,
        server: &str,
    ) -> Result<Option<String>, SourceError> {
        let mut url = https_origin(server)?;
        url.set_path(SERVER_SETTINGS_PATH);
        let settings: Value =
            get_json(self.read.get(url).header(ACCEPT, "application/json")).await?;
        Ok(settings
            .get("registration_url")
            .and_then(Value::as_str)
            .and_then(registration_target))
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
        self.filtered_collection(owner, token, path, &[]).await
    }

    /// Сохраняет заданный фильтр при переходе ко всем страницам ресурса.
    async fn filtered_collection<T: DeserializeOwned>(
        &self,
        owner: &OwnerContext,
        token: &str,
        path: &str,
        filters: &[(&str, &str)],
    ) -> Result<Vec<T>, SourceError> {
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        let mut marker: Option<String> = None;
        loop {
            let mut request = self
                .request(owner, token, path)?
                .query(&[("page_limit", PAGE_LIMIT)])
                .query(filters);
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

    /// Читает один пакет последних сообщений по повторяющимся параметрам uuid.
    pub(super) async fn sidebar_messages(
        &self,
        owner: &OwnerContext,
        token: &str,
        ids: &[Uuid],
    ) -> Result<Vec<Value>, SourceError> {
        let mut query = vec![
            ("project_id", owner.project_id.clone()),
            ("page_limit", ids.len().to_string()),
        ];
        query.extend(ids.iter().map(|id| ("uuid", id.to_string())));
        get_json(self.request(owner, token, MESSAGES_PATH)?.query(&query)).await
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
        if !topic.matches(owner, incoming.stream, incoming.topic) {
            return Err(SourceError::InvalidResponse);
        }
        Ok(topic.notification_mode.allows(stream.notification_mode))
    }

    /// Подтверждает, что личный разговор виден владельцу токена.
    pub(super) async fn check_direct_access(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream: Uuid,
    ) -> Result<(), OpenHistoryError> {
        self.direct_stream(owner, token, stream).await?;
        Ok(())
    }

    /// Читает сведения о личном разговоре и проверяет владельца токена.
    async fn direct_stream(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream: Uuid,
    ) -> Result<WireStream, OpenHistoryError> {
        let row = self.stream_access(owner, token, stream).await?;
        if !row.is_direct() {
            return Err(OpenHistoryError::AccessLost);
        }
        Ok(row)
    }

    /// Читает сведения о стриме и сверяет его UUID и владельца.
    async fn stream_access(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream: Uuid,
    ) -> Result<WireStream, OpenHistoryError> {
        let row: WireStream = self
            .access_row(owner, token, &format!("{STREAMS_PATH}/{stream}"))
            .await?;
        if parse_uuid(&row.uuid) != Some(stream)
            || !matches_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref())
        {
            return Err(SourceError::InvalidResponse.into());
        }
        Ok(row)
    }

    /// Разрешает обычный текст в теме лички по умолчанию.
    pub(super) async fn direct_send_topic(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream: Uuid,
    ) -> Result<Uuid, DirectSendError> {
        let row = self.direct_stream(owner, token, stream).await?;
        row.check_plain_send()?;
        row.default_topic_uuid
            .as_ref()
            .and_then(Value::as_str)
            .and_then(parse_uuid)
            .filter(|topic| !topic.is_nil())
            .ok_or(DirectSendError::TopicUnavailable)
    }

    /// Проверяет явную тему канала или сохранённую цель отправки.
    pub(super) async fn check_send_topic(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream: Uuid,
        topic: Uuid,
        queued: bool,
    ) -> Result<(), DirectSendError> {
        if topic.is_nil() {
            return Err(DirectSendError::TopicUnavailable);
        }
        let topic = self.topic_access(owner, token, stream, topic).await?;
        // Проверяем шифрование после чтения темы, прямо перед записью или отправкой.
        let row = self.stream_access(owner, token, stream).await?;
        if row.is_direct() {
            // Сохранённую тему лички перепроверяем без пересчёта темы по умолчанию.
            return if queued {
                row.check_plain_send()
            } else {
                Err(DirectSendError::AccessLost)
            };
        }
        if row.direct_user_uuid.is_some()
            || row.project_id.as_deref() != Some(owner.project_id.as_str())
            || row.user_uuid.as_deref() != Some(owner.user_id.as_str())
            || topic.project_id.as_deref() != Some(owner.project_id.as_str())
            || topic.user_uuid.as_deref() != Some(owner.user_id.as_str())
        {
            return Err(SourceError::InvalidResponse.into());
        }
        row.check_plain_send()?;
        match topic.is_done {
            Some(false) => Ok(()),
            Some(true) => Err(DirectSendError::TopicClosed),
            None => Err(SourceError::InvalidResponse.into()),
        }
    }

    /// Подтверждает, что тема нужного стрима видна владельцу токена.
    pub(super) async fn check_topic_access(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream: Uuid,
        topic: Uuid,
    ) -> Result<(), OpenHistoryError> {
        self.topic_access(owner, token, stream, topic).await?;
        Ok(())
    }

    /// Читает тему и сверяет её адрес с контекстом запроса.
    async fn topic_access(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream: Uuid,
        topic: Uuid,
    ) -> Result<WireTopic, OpenHistoryError> {
        let row: WireTopic = self
            .access_row(owner, token, &format!("{TOPICS_PATH}/{topic}"))
            .await?;
        if !row.matches(owner, stream, topic) {
            return Err(SourceError::InvalidResponse.into());
        }
        Ok(row)
    }

    /// Читает отдельную запись и отличает потерю доступа от других ошибок REST.
    async fn access_row<T: DeserializeOwned>(
        &self,
        owner: &OwnerContext,
        token: &str,
        path: &str,
    ) -> Result<T, OpenHistoryError> {
        let request = self.request(owner, token, path)?;
        let response = request.send().await.map_err(request_error)?;
        if matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            return Err(OpenHistoryError::AccessLost);
        }
        check_status(response.status())?;
        Ok(response.json().await.map_err(request_error)?)
    }

    /// Повторно проверяет доступ к выбранной области после ошибки сообщения.
    async fn check_history_scope(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
    ) -> Result<(), OpenHistoryError> {
        match scope.topic_id {
            Some(topic) => {
                self.check_topic_access(owner, token, scope.stream_id, topic)
                    .await
            }
            None => {
                self.check_direct_access(owner, token, scope.stream_id)
                    .await
            }
        }
    }

    /// Проверяет, что целевое текстовое сообщение ещё доступно в этом разговоре.
    pub(super) async fn history_anchor(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        anchor: Uuid,
    ) -> Result<HistoryMessage, OpenHistoryError> {
        let stream = scope.stream_id;
        let request = self.request(owner, token, &format!("{MESSAGES_PATH}{anchor}"))?;
        let response = request.send().await.map_err(request_error)?;
        match response.status() {
            StatusCode::NOT_FOUND => {
                // API скрывает недоступные сообщения за 404. Перечитываем доступ к чату.
                self.check_history_scope(owner, token, scope).await?;
                return Err(OpenHistoryError::MessageMissing);
            }
            StatusCode::FORBIDDEN => return Err(OpenHistoryError::AccessLost),
            status => check_status(status)?,
        }
        let row: WireHistoryMessage = response.json().await.map_err(request_error)?;
        if row.uuid != anchor
            || row.stream_uuid != stream
            || scope.topic_id.is_some_and(|topic| row.topic_uuid != topic)
        {
            return Err(OpenHistoryError::MessageMissing);
        }
        row.into_history(owner, stream)?
            .ok_or(OpenHistoryError::MessageMissing)
    }

    /// Читает соседние страницы по публичному UUID и сохраняет маркер каждого края.
    pub(super) async fn anchor_pages(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        anchor: Uuid,
    ) -> Result<(HistoryPage, HistoryPage), OpenHistoryError> {
        tokio::try_join!(
            self.anchor_page(owner, token, scope, anchor, "desc"),
            self.anchor_page(owner, token, scope, anchor, "asc")
        )
    }

    /// Запрашивает страницу с одной стороны от сообщения.
    async fn anchor_page(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        anchor: Uuid,
        direction: &str,
    ) -> Result<HistoryPage, OpenHistoryError> {
        let response = self
            .history_page_request(owner, token, scope, direction)?
            .query(&[("page_marker", anchor)])
            .send()
            .await
            .map_err(request_error)?;
        match response.status() {
            StatusCode::FORBIDDEN => return Err(OpenHistoryError::AccessLost),
            StatusCode::NOT_FOUND => {
                self.check_history_scope(owner, token, scope).await?;
                return Err(OpenHistoryError::MessageMissing);
            }
            status => check_status(status)?,
        }
        Ok(Self::decode_history_page(response, owner, scope).await?)
    }

    /// Запрашивает страницу от конца истории или от маркера старого края.
    pub(super) async fn descending_page(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        marker: Option<&str>,
    ) -> Result<HistoryPage, OpenHistoryError> {
        let request = self.history_page_request(owner, token, scope, "desc")?;
        let request = match marker {
            Some(marker) => request.query(&[("page_marker", marker)]),
            None => request,
        };
        let response = request.send().await.map_err(request_error)?;
        if matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            return Err(OpenHistoryError::AccessLost);
        }
        check_status(response.status())?;
        Ok(Self::decode_history_page(response, owner, scope).await?)
    }

    /// Запрашивает следующую страницу от маркера нового края.
    pub(super) async fn ascending_page(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        marker: &str,
    ) -> Result<HistoryPage, OpenHistoryError> {
        let response = self
            .history_page_request(owner, token, scope, "asc")?
            .query(&[("page_marker", marker)])
            .send()
            .await
            .map_err(request_error)?;
        if matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            return Err(OpenHistoryError::AccessLost);
        }
        check_status(response.status())?;
        Ok(Self::decode_history_page(response, owner, scope).await?)
    }

    /// Собирает запрос страницы для стрима или отдельной темы.
    fn history_page_request(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        direction: &str,
    ) -> Result<RequestBuilder, SourceError> {
        let request = self.request(owner, token, MESSAGES_PATH)?.query(&[
            ("stream_uuid", scope.stream_id.to_string().as_str()),
            ("page_limit", WINDOW_LIMIT),
            ("sort_key", "created_at"),
            ("sort_dir", direction),
        ]);
        Ok(match scope.topic_id {
            Some(topic) => request.query(&[("topic_uuid", topic)]),
            None => request,
        })
    }

    /// Читает маркер и проверяет область каждой строки перед сохранением.
    async fn decode_history_page(
        response: reqwest::Response,
        owner: &OwnerContext,
        scope: HistoryScope,
    ) -> Result<HistoryPage, SourceError> {
        let edge = match response.headers().get("X-Pagination-Marker") {
            None => HistoryEdge::End,
            Some(marker) => HistoryEdge::More(
                marker
                    .to_str()
                    .ok()
                    .filter(|value| !value.is_empty())
                    .ok_or(SourceError::InvalidResponse)?
                    .to_owned(),
            ),
        };
        let rows: Vec<WireHistoryMessage> = response.json().await.map_err(request_error)?;
        let messages = rows
            .into_iter()
            .filter_map(|row| {
                if scope.topic_id.is_some_and(|topic| row.topic_uuid != topic) {
                    return Some(Err(SourceError::InvalidResponse));
                }
                row.into_history(owner, scope.stream_id).transpose()
            })
            .collect::<Result<_, _>>()?;
        Ok(HistoryPage { messages, edge })
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

    /// Запрашивает полный оригинал напрямую. Отказ доступа возвращается без чтения тела.
    pub(super) async fn media_download(
        &self,
        owner: &OwnerContext,
        token: &str,
        file_id: Uuid,
    ) -> Result<Option<reqwest::Response>, SourceError> {
        let mut url = workspace_origin(owner)?;
        url.set_path(&format!(
            "/api/workspace/v1/messenger/files/{file_id}/actions/download"
        ));
        let response = self
            .read
            .get(url)
            .header(ACCEPT, "*/*")
            .bearer_auth(token)
            .send()
            .await
            .map_err(request_error)?;
        if matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            return Ok(None);
        }
        check_status(response.status())?;
        Ok(Some(response))
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
            .read
            .get(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(access_token))
    }
}

/// Преобразует отказ в чтении стрима или темы в причину закрытия панели.
fn details_access_error(error: OpenHistoryError) -> DetailsError {
    match error {
        OpenHistoryError::AccessLost => DetailsError::AccessLost,
        OpenHistoryError::Source(error) => DetailsError::Source(error),
        _ => SourceError::InvalidResponse.into(),
    }
}

/// Требует явно указанные проект и пользователя в ответе API владельца.
fn check_explicit_owner(
    owner: &OwnerContext,
    project: Option<&str>,
    user: Option<&str>,
) -> Result<(), SourceError> {
    if project == Some(owner.project_id.as_str()) && user == Some(owner.user_id.as_str()) {
        Ok(())
    } else {
        Err(SourceError::InvalidResponse)
    }
}

/// Проверяет обязательное название в сведениях, сохраняя чтение старых ответов истории.
fn required_title(value: Option<String>) -> Result<String, SourceError> {
    value
        .filter(|name| !name.trim().is_empty())
        .ok_or(SourceError::InvalidResponse)
}

/// Проверяет, что адрес владельца содержит базовый HTTPS-адрес сервера.
pub(super) fn workspace_origin(owner: &OwnerContext) -> Result<Url, SourceError> {
    https_origin(&owner.server)
}

/// Проверяет базовый адрес Workspace, который можно использовать для загрузки данных.
fn https_origin(server: &str) -> Result<Url, SourceError> {
    let url = Url::parse(server).map_err(|_| SourceError::InvalidResponse)?;
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

/// Возвращает абсолютную HTTPS-ссылку без учётных данных для системного браузера.
fn registration_target(value: &str) -> Option<String> {
    let value = value.trim();
    let (scheme, address) = value.split_once("://")?;
    let authority = address.split(['/', '?', '#']).next()?;
    // Разбор URL может удалить пустую запись о пользователе и преобразовать обратную косую черту.
    if !scheme.eq_ignore_ascii_case("https")
        || authority.is_empty()
        || authority.contains('@')
        || value.contains(|ch: char| ch.is_control() || ch == '\\')
    {
        return None;
    }
    let url = Url::parse(value).ok()?;
    url.host().is_some().then(|| url.into())
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
    kind: String,                      // Название события, например `message.created`.
    uuid: Option<String>,              // Идентификатор объекта. Для нового сообщения обязателен.
    project_id: Option<String>, // Идентификатор проекта, если сервер повторил его во вложенных данных.
    user_uuid: Option<String>, // Идентификатор пользователя, если сервер повторил его во вложенных данных.
    payload: Option<Value>, // Тело объекта. Для нового сообщения здесь лежат формат и содержимое.
    unread_count: Option<u32>, // Готовый счётчик в событии обновления папки.
    is_archived: Option<bool>, // Признак архива в снимке разговора.
    folder_uuid: Option<Uuid>, // Папка, если событие элемента называет её.
    folder: Option<Uuid>,   // Имя того же поля в системной проекции.
    active_unread_count: Option<u32>, // Активные непрочитанные для канала или темы.
    passive_unread_count: Option<u32>, // Пассивные непрочитанные для канала или темы.
    source_name: Option<String>, // Источник сообщения. Первый срез показывает баннеры Workspace.
    is_own: Option<bool>,   // Признак, что владелец сам отправил сообщение.
    read: Option<bool>,     // Признак, что владелец уже прочитал сообщение.
    author_uuid: Option<String>, // Идентификатор автора сообщения.
    stream_uuid: Option<String>, // Идентификатор разговора.
    topic_uuid: Option<String>, // Идентификатор темы внутри разговора.
    created_at: Option<String>, // Серверное время создания сообщения.
    updated_at: Option<String>, // Серверное время последней правки.
}

/// Текстовые записи и серверная граница одной страницы.
pub(super) struct HistoryPage {
    pub(super) messages: Vec<HistoryMessage>, // Сообщения поддерживаемого формата.
    pub(super) edge: HistoryEdge,             // Серверный маркер продолжения или конец истории.
}

/// Поля ответа ресурса messages, используемые для записи истории.
#[derive(Deserialize)]
struct WireHistoryMessage {
    uuid: Uuid,                  // Публичный идентификатор сообщения.
    stream_uuid: Uuid,           // Разговор сообщения.
    topic_uuid: Uuid,            // Тема внутри разговора.
    project_id: Option<String>,  // Проект из ответа.
    user_uuid: Option<String>,   // Владелец проекции из ответа.
    author_uuid: Uuid,           // Автор сообщения.
    created_at: String,          // Серверное время создания.
    updated_at: Option<String>,  // Серверное время последней правки.
    source_name: Option<String>, // Серверный носитель сообщения, если он известен.
    payload: WireMessageBody,    // Формат и содержимое сообщения.
}

impl WireHistoryMessage {
    /// Проверяет область ответа и преобразует обычный Markdown в запись истории.
    fn into_history(
        self,
        owner: &OwnerContext,
        stream: Uuid,
    ) -> Result<Option<HistoryMessage>, SourceError> {
        if self.stream_uuid != stream
            || !matches_owner(owner, self.project_id.as_deref(), self.user_uuid.as_deref())
        {
            return Err(SourceError::InvalidResponse);
        }
        if self.payload.kind != "markdown" {
            return Ok(None);
        }
        let text = match self.payload.content {
            Some(Value::String(text)) => text,
            _ => return Err(SourceError::InvalidResponse),
        };
        let created_at =
            HistoryTime::parse(&self.created_at).ok_or(SourceError::InvalidResponse)?;
        let updated_at = self
            .updated_at
            .as_deref()
            .map(|value| HistoryTime::parse(value).ok_or(SourceError::InvalidResponse))
            .transpose()?;
        Ok(Some(HistoryMessage {
            id: self.uuid,
            stream_id: self.stream_uuid,
            topic_id: self.topic_uuid,
            text,
            metadata: HistoryMetadata {
                author_id: Some(self.author_uuid),
                created_at,
                updated_at,
            },
        }))
    }
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
    name: Option<String>,        // Название, которое нужно только панели сведений.
    description: Option<String>, // Описание канала, если оно заполнено.
    color: Option<u32>,          // Цвет значка стрима.
    owner: Option<String>,       // UUID владельца стрима.
    invite_only: Option<bool>,   // Доступ по приглашению.
    uuid: String,                // Идентификатор запрошенного разговора.
    project_id: Option<String>,  // Проект, если он есть в ответе.
    user_uuid: Option<String>,   // Пользователь, если он есть в ответе.
    source_name: Option<String>, // Нативный стрим или внешний провайдер, если сервер указал.
    private: bool,               // Признак личного разговора.
    is_archived: bool,           // Признак архивного разговора.
    direct_user_uuid: Option<String>, // Идентификатор собеседника для личного разговора.
    default_topic_uuid: Option<Value>, // Тема отправки, её формат проверяем отдельно от чтения.
    encryption: Option<Value>,   // Только JSON false разрешает подготовить открытый текст.
    notification_mode: StreamNotificationMode, // Настройка уведомлений разговора.
}

impl WireStream {
    /// Определяет личку по признаку приватности и подтверждённому UUID собеседника.
    fn is_direct(&self) -> bool {
        self.private
            && self
                .direct_user_uuid
                .as_deref()
                .and_then(parse_uuid)
                .is_some()
    }

    /// Разрешает открытый текст только в рабочем стриме с явным отключением шифрования.
    fn check_plain_send(&self) -> Result<(), DirectSendError> {
        if self.is_archived {
            return Err(DirectSendError::AccessLost);
        }
        if !matches!(self.encryption, Some(Value::Bool(false))) {
            return Err(DirectSendError::EncryptionUnavailable);
        }
        Ok(())
    }

    /// Проверяет идентификатор разговора и владельца данных.
    fn matches(&self, owner: &OwnerContext, incoming: &IncomingMessage) -> bool {
        parse_uuid(&self.uuid) == Some(incoming.stream)
            && matches_owner(owner, self.project_id.as_deref(), self.user_uuid.as_deref())
    }
}

/// Сведения о теме, чья настройка может переопределить уведомления разговора.
#[derive(Deserialize)]
struct WireTopic {
    name: Option<String>,       // Название, которое нужно только панели сведений.
    uuid: String,               // Идентификатор запрошенной темы.
    stream_uuid: String,        // Идентификатор разговора, которому принадлежит тема.
    project_id: Option<String>, // Проект, если он есть в ответе.
    user_uuid: Option<String>,  // Пользователь, если он есть в ответе.
    is_done: Option<bool>,      // Закрытие темы, неизвестно у старых ответов.
    notification_mode: TopicNotificationMode, // Настройка уведомлений темы.
    unread_count: Option<u32>,  // Общий счётчик, может отсутствовать в старом ответе.
    active_unread_count: Option<u32>, // Счётчик сообщений с уведомлениями.
    passive_unread_count: Option<u32>, // Счётчик остальных непрочитанных.
    summary: Option<String>,    // Текст последней серверной сводки.
}

impl WireTopic {
    /// Проверяет идентификаторы темы, разговора и владельца данных.
    fn matches(&self, owner: &OwnerContext, stream: Uuid, topic: Uuid) -> bool {
        parse_uuid(&self.uuid) == Some(topic)
            && parse_uuid(&self.stream_uuid) == Some(stream)
            && matches_owner(owner, self.project_id.as_deref(), self.user_uuid.as_deref())
    }
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

/// Разбирает изменения папок и видимости разговоров из журнала Messenger.
fn folder_event(
    wire: &WireEnvelope,
    owner: &OwnerContext,
    cursor: &Cursor,
) -> Result<Option<Event>, SourceError> {
    let resource = wire.object_type.as_str();
    let action = wire.action.as_str();
    let payload = &wire.payload;
    let archived = payload
        .is_archived
        .or((action == "created").then_some(false));
    if !matches!(
        (resource, action),
        ("folder" | "folder_item", "created" | "updated" | "deleted")
    ) && !(resource == "stream" && matches!(action, "created" | "updated") && archived.is_some())
    {
        return Ok(None);
    }
    if payload.kind.split_once('.') != Some((resource, action))
        || !matches_owner(
            owner,
            payload.project_id.as_deref(),
            payload.user_uuid.as_deref(),
        )
    {
        return Err(SourceError::InvalidResponse);
    }
    let id = payload
        .uuid
        .as_deref()
        .and_then(parse_uuid)
        .ok_or(SourceError::InvalidResponse)?;
    let change = match (resource, action) {
        ("folder", "deleted") => FolderChange::DeleteFolder(id),
        ("folder", _) if id.is_nil() => {
            let count = payload.unread_count.ok_or(SourceError::InvalidResponse)?;
            return Ok(Some(Event::UnreadChanged {
                cursor: cursor.clone(),
                count,
            }));
        }
        ("folder", _) => FolderChange::Refresh(Some(id)),
        ("folder_item", "deleted") => FolderChange::DeleteItem(id),
        ("folder_item", _) => FolderChange::Refresh(payload.folder_uuid.or(payload.folder)),
        ("stream", _) => FolderChange::StreamChanged {
            stream_id: id,
            archived: archived.expect("проверено наличие признака архива"),
        },
        _ => unreachable!(),
    };
    Ok(Some(Event::FolderChanged {
        cursor: cursor.clone(),
        change,
        sidebar: None,
    }))
}

/// Разбирает серверный UUID для сравнения идентификаторов и построения URL.
fn parse_uuid(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value).ok()
}

/// Различает подтверждённую ссылку, отсутствие фото и необходимость сверки профиля.
fn wire_avatar_update(value: Option<&Value>) -> AvatarUpdate {
    match value {
        Some(Value::Null) => AvatarUpdate::Reference(None),
        Some(Value::String(reference)) => normalize_avatar_reference(reference)
            .map_or(AvatarUpdate::Reconcile, |reference| {
                AvatarUpdate::Reference(Some(reference))
            }),
        _ => AvatarUpdate::Reconcile,
    }
}

/// Проверяет контекст и преобразует событие журнала в изменение для Sync.
pub(super) fn wire_event(
    value: Value,
    owner: &OwnerContext,
    epoch: &str,
) -> Result<(Event, Option<IncomingMessage>), SourceError> {
    let wire: WireEnvelope =
        WireEnvelope::deserialize(&value).map_err(|_| SourceError::InvalidResponse)?;
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
    if wire.object_type == "file"
        && matches!(
            (wire.action.as_str(), wire.payload.kind.as_str()),
            ("deleted", "file.deleted") | ("updated", "file.updated")
        )
    {
        let deleted = wire.action == "deleted";
        // У file.updated поле user_uuid внутри payload — загрузивший файл. Получатель уже сверен
        // по конверту, а file.deleted не передаёт в payload автора файла.
        if !matches_owner(
            owner,
            wire.payload.project_id.as_deref(),
            wire.payload.user_uuid.as_deref().filter(|_| deleted),
        ) {
            return Err(SourceError::InvalidResponse);
        }
        let file_id = wire
            .payload
            .uuid
            .as_deref()
            .and_then(parse_uuid)
            .filter(|id| !id.is_nil());
        let event = match file_id {
            Some(file_id) if deleted => Event::FileDeleted { cursor, file_id },
            Some(file_id) => Event::FileUpdated { cursor, file_id },
            None => Event::Checkpoint(cursor),
        };
        return Ok((event, None));
    }
    if wire.object_type == "user" && wire.action == "updated" && wire.payload.kind == "user.updated"
    {
        if !matches_owner(
            owner,
            wire.payload.project_id.as_deref(),
            wire.payload.user_uuid.as_deref(),
        ) {
            return Err(SourceError::InvalidResponse);
        }
        let Some(user_id) = wire
            .payload
            .uuid
            .as_deref()
            .and_then(parse_uuid)
            .filter(|id| !id.is_nil())
        else {
            // Без UUID нельзя определить чей профиль сверять. Позиция журнала всё равно пригодна.
            return Ok((Event::Checkpoint(cursor), None));
        };
        let payload = &value["payload"];
        // Старое событие может содержать UUID и аватар без обязательных полей карточки.
        return Ok((
            Event::UserUpdated {
                cursor,
                user_id,
                avatar: wire_avatar_update(payload.get("avatar")),
                sidebar: sidebar::wire_sidebar_change(&wire, payload, owner).map(Box::new),
            },
            None,
        ));
    }
    // Стрим и тема могут одним снимком обновить папки, счётчики и правый сайдбар.
    let sidebar_change = sidebar::wire_sidebar_change(&wire, &value["payload"], owner);
    let mut folder_event = folder_event(&wire, owner, &cursor)?;
    if let Some(Event::FolderChanged { sidebar, .. }) = &mut folder_event {
        *sidebar = sidebar_change.clone().map(Box::new);
    }
    let fallback_event = || {
        folder_event
            .clone()
            .unwrap_or_else(|| match &sidebar_change {
                Some(change) => Event::SidebarChanged {
                    cursor: cursor.clone(),
                    change: change.clone(),
                },
                None => Event::Checkpoint(cursor.clone()),
            })
    };
    let folder = match &folder_event {
        Some(Event::FolderChanged {
            change:
                change @ FolderChange::StreamChanged {
                    archived: false, ..
                },
            ..
        }) => Some(*change),
        Some(event) => return Ok((event.clone(), None)),
        None => None,
    };
    if matches!(wire.object_type.as_str(), "stream" | "topic")
        && matches!(wire.action.as_str(), "created" | "updated" | "read")
    {
        let payload = &wire.payload;
        let (Some(total), Some(active), Some(passive)) = (
            payload.unread_count,
            payload.active_unread_count,
            payload.passive_unread_count,
        ) else {
            return Ok((fallback_event(), None));
        };
        let Some(id) = payload.uuid.as_deref().and_then(parse_uuid) else {
            return Ok((fallback_event(), None));
        };
        let (stream_id, topic_id) = if wire.object_type == "topic" {
            let Some(stream_id) = payload.stream_uuid.as_deref().and_then(parse_uuid) else {
                return Ok((fallback_event(), None));
            };
            (stream_id, Some(id))
        } else {
            (id, None)
        };
        if payload.kind.split_once('.') != Some((wire.object_type.as_str(), wire.action.as_str()))
            || !matches_owner(
                owner,
                payload.project_id.as_deref(),
                payload.user_uuid.as_deref(),
            )
        {
            return Err(SourceError::InvalidResponse);
        }
        return Ok((
            Event::CatalogCountsChanged {
                cursor,
                counts: CatalogCounts {
                    stream_id,
                    topic_id,
                    total,
                    active,
                    passive,
                },
                folder,
                sidebar: sidebar_change.map(Box::new),
            },
            None,
        ));
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
        return Ok((fallback_event(), None));
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

    /// Удаление привязки для оставшегося участника не означает отзыв канала.
    #[test]
    fn legacy_stream_binding_deletion_identifies_binding_without_revoking_history() {
        let event = json!({
            "schema_version": 1, "uuid": "event", "epoch_version": 1,
            "project_id": "project", "user_uuid": "recipient",
            "object_type": "stream_binding", "action": "deleted",
            "payload": { "kind": "stream_binding.deleted", "project_id": "project", "user_uuid": "recipient", "uuid": "00000000-0000-0000-0000-000000000004" }
        });
        assert!(matches!(
            wire_event(event, &owner(), "epoch").unwrap().0,
            Event::SidebarChanged { change: crate::messenger::details::SidebarChange::MembershipBinding(id), .. }
                if id == Uuid::from_u128(4)
        ));
    }

    /// Создаёт событие для получателя, отличного от пользователя в снимке.
    fn user_updated(version: u64, fields: Value) -> Value {
        let mut payload = json!({
            "kind": "user.updated",
            "uuid": Uuid::from_u128(4),
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        json!({
            "schema_version": 1, "uuid": "event", "epoch_version": version,
            "project_id": "project", "user_uuid": "recipient",
            "object_type": "user", "action": "updated", "payload": payload,
        })
    }

    /// Снимок сохраняет карточку, а старые события обновляют аватар даже без имени пользователя.
    #[test]
    fn user_events_decode_avatar_independently_from_sidebar_profile() {
        let file = format!("urn:image:{}", Uuid::from_u128(0xabc));
        let wire_file = format!(
            "urn:image:{}",
            Uuid::from_u128(0xabc).to_string().to_uppercase()
        );
        let fallback = "urn:gravatar:abcdabcdabcdabcdabcdabcdabcdabcd";
        let events = decode_page(
            vec![
                user_updated(1, json!({"username": "avatar-owner", "avatar": wire_file})),
                user_updated(2, json!({"avatar": fallback})),
                user_updated(3, json!({"avatar": null})),
                user_updated(4, json!({})),
                user_updated(5, json!({"avatar": "urn:image:wrong"})),
            ],
            &owner(),
            &cursor(0),
        )
        .unwrap();
        let [
            Event::UserUpdated {
                user_id,
                avatar: AvatarUpdate::Reference(Some(reference)),
                sidebar: Some(sidebar),
                ..
            },
            Event::UserUpdated {
                avatar: AvatarUpdate::Reference(Some(external)),
                sidebar: None,
                ..
            },
            Event::UserUpdated {
                avatar: AvatarUpdate::Reference(None),
                ..
            },
            Event::UserUpdated {
                avatar: AvatarUpdate::Reconcile,
                ..
            },
            Event::UserUpdated {
                avatar: AvatarUpdate::Reconcile,
                ..
            },
        ] = events.as_slice()
        else {
            panic!("неожиданное преобразование событий профиля: {events:?}");
        };
        assert_eq!(*user_id, Uuid::from_u128(4));
        assert_eq!(reference, &file);
        assert_eq!(external, fallback);
        assert!(matches!(
            sidebar.as_ref(),
            crate::messenger::details::SidebarChange::Profile { user_id: target, profile }
                if target == user_id && profile.display_name == "avatar-owner"
        ));
    }

    /// Отклоняет событие, которое во вложенных полях указывает чужой контекст.
    #[test]
    fn refuses_user_events_from_a_different_context() {
        for fields in [
            json!({"project_id": "other"}),
            json!({"user_uuid": "other"}),
        ] {
            assert_eq!(
                wire_event(user_updated(1, fields), &owner(), "epoch").map(|(event, _)| event),
                Err(SourceError::InvalidResponse)
            );
        }
    }

    /// Неполное событие без пользователя оставляет позицию журнала доступной для продолжения.
    #[test]
    fn user_events_without_a_subject_advance_as_checkpoints() {
        let event = user_updated(1, json!({"uuid": null}));
        assert_eq!(
            wire_event(event, &owner(), "epoch").unwrap().0,
            Event::Checkpoint(cursor(1))
        );
    }

    /// Передаёт удаление файла в кэш, даже когда событие не содержит стрим и профиль.
    #[test]
    fn file_deletion_identifies_avatar_file_in_the_recipient_context() {
        let file_id = Uuid::from_u128(7);
        let event = json!({
            "schema_version": 1, "uuid": "event", "epoch_version": 1,
            "project_id": "project", "user_uuid": "recipient",
            "object_type": "file", "action": "deleted",
            "payload": { "kind": "file.deleted", "uuid": file_id, "stream_uuid": null },
        });
        assert_eq!(
            decode_page(vec![event.clone()], &owner(), &cursor(0)).unwrap(),
            [Event::FileDeleted {
                cursor: cursor(1),
                file_id
            }],
        );
        let mut foreign = event;
        foreign["payload"]["project_id"] = json!("other");
        assert_eq!(
            wire_event(foreign, &owner(), "epoch").map(|(event, _)| event),
            Err(SourceError::InvalidResponse)
        );
    }

    /// В file.updated автор файла может отличаться от получателя, а проект должен совпадать.
    #[test]
    fn file_update_distinguishes_uploader_from_recipient() {
        let file_id = Uuid::from_u128(8);
        let mut event = json!({
            "schema_version": 1, "uuid": "event", "epoch_version": 1,
            "project_id": "project", "user_uuid": "recipient",
            "object_type": "file", "action": "updated",
            "payload": {"kind": "file.updated", "uuid": file_id,
                        "user_uuid": Uuid::from_u128(9), "project_id": "project"},
        });
        assert_eq!(
            decode_page(vec![event.clone()], &owner(), &cursor(0)).unwrap(),
            [Event::FileUpdated {
                cursor: cursor(1),
                file_id
            }],
        );
        event["payload"]["project_id"] = json!("other");
        assert_eq!(
            wire_event(event, &owner(), "epoch").map(|(event, _)| event),
            Err(SourceError::InvalidResponse),
        );
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

    /// Проверяет общий счётчик, обновление другой папки и подмену владельца.
    #[test]
    fn decodes_folder_updates_for_the_current_owner() {
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
        let personal = Uuid::from_u128(1);
        event["payload"]["user_uuid"] = json!("recipient");
        event["payload"]["uuid"] = json!(personal);
        assert_eq!(
            decode_page(vec![event.clone()], &owner, &cursor(14)).unwrap(),
            [Event::FolderChanged {
                cursor: cursor(15),
                change: FolderChange::Refresh(Some(personal)),
                sidebar: None,
            }]
        );
        event["action"] = json!("deleted");
        event["payload"] = json!({"kind":"folder.deleted", "uuid":personal});
        assert_eq!(
            decode_page(vec![event.clone()], &owner, &cursor(14)).unwrap(),
            [Event::FolderChanged {
                cursor: cursor(15),
                change: FolderChange::DeleteFolder(personal),
                sidebar: None,
            }]
        );
        event["object_type"] = json!("folder_item");
        event["payload"]["kind"] = json!("folder_item.deleted");
        assert_eq!(
            decode_page(vec![event.clone()], &owner, &cursor(14)).unwrap(),
            [Event::FolderChanged {
                cursor: cursor(15),
                change: FolderChange::DeleteItem(personal),
                sidebar: None,
            }]
        );
        event["object_type"] = json!("stream");
        event["action"] = json!("updated");
        event["payload"] = json!({"kind":"stream.updated", "uuid":personal, "is_archived":true});
        assert_eq!(
            decode_page(vec![event], &owner, &cursor(14)).unwrap(),
            [Event::FolderChanged {
                cursor: cursor(15),
                change: FolderChange::StreamChanged {
                    stream_id: personal,
                    archived: true
                },
                sidebar: None,
            }]
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
