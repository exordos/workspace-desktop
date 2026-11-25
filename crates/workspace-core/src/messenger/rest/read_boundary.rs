//! Читает серверное состояние непрочитанного и отправляет точную границу темы.
//! Сохраняет порядок сырых строк, включая форматы, которых нет в окне истории.

use std::time::Duration;

use reqwest::{Response, StatusCode, header::ACCEPT};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    messenger::{
        history::OpenHistoryError,
        message_actions::MessageTarget,
        read::{ReadBoundary, ReadError, UnreadAnchor},
    },
    runtime::SourceError,
    storage::{HistoryScope, HistoryTime},
};

use super::{
    MESSAGES_PATH, RestClient, WireHistoryMessage, check_explicit_owner, check_status,
    mutations::mutation_id, request_error, workspace_origin,
};

const READ_PAGE_BYTES: usize = 8 * 1024 * 1024;
const READ_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Поля сырой строки. Незнакомое содержимое не должно скрывать непрочитанный UUID.
#[derive(Deserialize)]
struct WireUnreadRow {
    uuid: Uuid,                    // Публичный UUID размещения.
    stream_uuid: Uuid,             // Стрим размещения.
    topic_uuid: Uuid,              // Точная тема размещения.
    project_id: Option<String>,    // Проект, заявленный в ответе.
    user_uuid: Option<String>,     // Владелец флага чтения.
    created_at: String,            // Серверное время для порядка.
    source_name: Option<String>,   // Источник сообщения, если сервер его указал.
    read: Option<bool>,            // Флаг, который должен совпасть с запросом.
    payload: Option<WireReadKind>, // Формат, доступный даже для неизвестного тела.
}

/// Общее поле маршрутизации тела сообщения из Workspace.
#[derive(Deserialize)]
struct WireReadKind {
    kind: Option<String>, // Формат содержимого или None, если его не удалось определить.
}

impl RestClient {
    /// Однократно отправляет отметку. Квитанция и синхронное применение имеют разные результаты.
    pub(in crate::messenger) async fn read_up_to(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        message_id: Uuid,
        expected_time: Option<&HistoryTime>,
    ) -> Result<Option<Uuid>, ReadError> {
        let mut url = workspace_origin(owner).map_err(|_| ReadError::InvalidInput)?;
        url.set_path(&format!(
            "{MESSAGES_PATH}{message_id}/actions/read_up_to/invoke"
        ));
        let response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .timeout(READ_QUERY_TIMEOUT)
            .send()
            .await
            .map_err(|_| ReadError::Unknown)?;
        if response.status() != StatusCode::OK {
            return Err(ReadError::Http(response.status().as_u16()));
        }
        let ticket = mutation_id(response.headers()).map_err(|_| ReadError::Unknown)?;
        let row: WireHistoryMessage = bounded_json(response)
            .await
            .map_err(|_| ReadError::Unknown)?;
        let created_at = HistoryTime::parse(&row.created_at);
        check_explicit_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref())
            .map_err(|_| ReadError::Unknown)?;
        if row.uuid != message_id
            || row.stream_uuid != scope.stream_id
            || scope.topic_id != Some(row.topic_uuid)
            || created_at.is_none()
            || expected_time.is_some_and(|time| created_at.as_ref() != Some(time))
            || row
                .updated_at
                .as_deref()
                .is_some_and(|time| HistoryTime::parse(time).is_none())
            || row.payload.kind.is_empty()
            || (ticket.is_none() && row.read != Some(true))
        {
            return Err(ReadError::Unknown);
        }
        Ok(ticket)
    }

    /// Возвращает первую страницу непрочитанного в серверном порядке без фильтра тел.
    pub(in crate::messenger) async fn unread_page(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        limit: usize,
        through: Option<&HistoryTime>,
    ) -> Result<Vec<UnreadAnchor>, OpenHistoryError> {
        let request = self.request(owner, token, MESSAGES_PATH)?.query(&[
            ("stream_uuid", scope.stream_id.to_string().as_str()),
            ("read", "false"),
            ("sort_key", "created_at"),
            ("sort_dir", "asc"),
            ("page_limit", limit.to_string().as_str()),
        ]);
        let request = match scope.topic_id {
            Some(topic) => request.query(&[("topic_uuid", topic)]),
            None => request,
        };
        let request = match through {
            Some(time) => request.query(&[("created_at=<", time.as_str())]),
            None => request,
        };
        let response = request
            .timeout(READ_QUERY_TIMEOUT)
            .send()
            .await
            .map_err(request_error)?;
        if matches!(
            response.status(),
            StatusCode::NOT_FOUND | StatusCode::FORBIDDEN
        ) {
            return Err(OpenHistoryError::AccessLost);
        }
        check_status(response.status())?;
        let has_more = response.headers().contains_key("X-Pagination-Marker");
        let rows: Vec<WireUnreadRow> = bounded_json(response).await?;
        if rows.len() > limit || (has_more && rows.len() < limit) {
            return Err(SourceError::InvalidResponse.into());
        }
        let unread = rows
            .iter()
            .map(|row| {
                if row.read != Some(false) {
                    return Err(SourceError::InvalidResponse);
                }
                let anchor = read_anchor(owner, scope, row)?;
                if through.is_some_and(|time| &anchor.boundary.created_at > time) {
                    return Err(SourceError::InvalidResponse);
                }
                Ok(anchor)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if unread
            .windows(2)
            .any(|pair| pair[0].boundary.order() >= pair[1].boundary.order())
        {
            return Err(SourceError::InvalidResponse.into());
        }
        Ok(unread)
    }

    /// Подтверждает принадлежность активного нативного разговора без шифрования.
    pub(in crate::messenger) async fn verify_read_stream(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
    ) -> Result<(), OpenHistoryError> {
        let stream = tokio::time::timeout(
            READ_QUERY_TIMEOUT,
            self.stream_access(owner, token, scope.stream_id),
        )
        .await
        .map_err(|_| SourceError::Temporary)??;
        check_explicit_owner(
            owner,
            stream.project_id.as_deref(),
            stream.user_uuid.as_deref(),
        )?;
        if stream.is_archived
            || stream.source_name.as_deref() != Some("native")
            || stream.encryption.as_ref() != Some(&Value::Bool(false))
            || (scope.topic_id.is_none() && !stream.is_direct())
        {
            return Err(OpenHistoryError::AccessLost);
        }
        Ok(())
    }

    /// Проверяет точное серверное размещение цели, когда она уже прочитана и не попала в выборку.
    pub(in crate::messenger) async fn verify_read_target(
        &self,
        owner: &OwnerContext,
        token: &str,
        boundary: &ReadBoundary,
    ) -> Result<(), OpenHistoryError> {
        let (found, read) = self
            .load_read_target(owner, token, boundary.scope(), boundary.target.message_id)
            .await?;
        if found.order() != boundary.order() {
            return Err(OpenHistoryError::MessageMissing);
        }
        if read != Some(true) {
            return Err(OpenHistoryError::StaleWindow);
        }
        Ok(())
    }

    /// Читает точный адрес размещения до явного одиночного действия.
    pub(in crate::messenger) async fn load_read_target(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        id: Uuid,
    ) -> Result<(ReadBoundary, Option<bool>), OpenHistoryError> {
        let response = self
            .request(owner, token, &format!("{MESSAGES_PATH}{id}"))?
            .timeout(READ_QUERY_TIMEOUT)
            .send()
            .await
            .map_err(request_error)?;
        if matches!(
            response.status(),
            StatusCode::NOT_FOUND | StatusCode::FORBIDDEN
        ) {
            return Err(OpenHistoryError::MessageMissing);
        }
        check_status(response.status())?;
        let row: WireUnreadRow = bounded_json(response).await?;
        let found = read_anchor(owner, scope, &row)?;
        if !found.eligible || found.boundary.target.message_id != id {
            return Err(OpenHistoryError::MessageMissing);
        }
        Ok((found.boundary, row.read))
    }
}

/// Проверяет размещение до работы с форматом. Неизвестный формат остаётся видимым для проверки.
fn read_anchor(
    owner: &OwnerContext,
    scope: HistoryScope,
    row: &WireUnreadRow,
) -> Result<UnreadAnchor, SourceError> {
    check_explicit_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref())?;
    let target = MessageTarget {
        scope,
        topic_id: row.topic_uuid,
        message_id: row.uuid,
    };
    if !target.is_valid() || row.stream_uuid != scope.stream_id {
        return Err(SourceError::InvalidResponse);
    }
    let created_at = HistoryTime::parse(&row.created_at).ok_or(SourceError::InvalidResponse)?;
    Ok(UnreadAnchor {
        boundary: ReadBoundary { target, created_at },
        eligible: row.source_name.as_deref() == Some("native")
            && row
                .payload
                .as_ref()
                .and_then(|payload| payload.kind.as_deref())
                == Some("markdown"),
    })
}

/// Ограничивает тело по полученным байтам, даже если длина ответа не указана.
async fn bounded_json<T: DeserializeOwned>(mut response: Response) -> Result<T, SourceError> {
    if response
        .content_length()
        .is_some_and(|length| length > READ_PAGE_BYTES as u64)
    {
        return Err(SourceError::InvalidResponse);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if chunk.len() > READ_PAGE_BYTES - body.len() {
            return Err(SourceError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| SourceError::InvalidResponse)
}
