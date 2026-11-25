//! Перечитывает серверные флаги чтения в уже загруженном диапазоне истории.

use reqwest::StatusCode;
use serde::Deserialize;
use uuid::Uuid;

use super::{RestClient, check_status, matches_owner, request_error};
use crate::{
    context::OwnerContext,
    messenger::history::OpenHistoryError,
    runtime::SourceError,
    storage::{HistoryReadRow, HistoryScope, HistoryTime},
};

/// Минимальные поля серверного снимка для сверки чтения.
#[derive(Deserialize)]
struct WireReadRow {
    uuid: Uuid,                 // Публичный UUID размещения.
    stream_uuid: Uuid,          // Стрим сообщения.
    topic_uuid: Uuid,           // Точная тема сообщения.
    project_id: Option<String>, // Проект, когда сервер его повторяет.
    user_uuid: Option<String>,  // Владелец признака, когда сервер его повторяет.
    created_at: String,         // Время создания из того же снимка.
    read: Option<bool>,         // Флаг владельца. None в старом ответе.
}

impl RestClient {
    /// Возвращает страницу флагов и следующий маркер, не загружая реакции отдельно.
    pub(in crate::messenger) async fn history_read_page(
        &self,
        owner: &OwnerContext,
        token: &str,
        scope: HistoryScope,
        bounds: (&HistoryTime, &HistoryTime),
        marker: Option<&str>,
    ) -> Result<(Vec<HistoryReadRow>, Option<String>), OpenHistoryError> {
        let (first, last) = bounds;
        let request = self
            .history_page_request(owner, token, scope, "asc")?
            .query(&[
                ("created_at=>", first.as_str()),
                ("created_at=<", last.as_str()),
            ]);
        let request = if let Some(marker) = marker {
            request.query(&[("page_marker", marker)])
        } else {
            request
        };
        let response = request.send().await.map_err(request_error)?;
        match response.status() {
            StatusCode::NOT_FOUND if marker.is_some() => {
                // Удалённая строка больше не служит маркером, но тема может оставаться доступной.
                self.check_history_scope(owner, token, scope).await?;
                return Err(OpenHistoryError::StaleWindow);
            }
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                return Err(OpenHistoryError::AccessLost);
            }
            status => check_status(status)?,
        }
        let next = response
            .headers()
            .get("X-Pagination-Marker")
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .filter(|value| !value.is_empty() && Some(*value) != marker)
                    .map(str::to_owned)
                    .ok_or(SourceError::InvalidResponse)
            })
            .transpose()?;
        let rows: Vec<WireReadRow> = response.json().await.map_err(request_error)?;
        let mut previous = None;
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let created_at =
                HistoryTime::parse(&row.created_at).ok_or(SourceError::InvalidResponse)?;
            if row.uuid.is_nil()
                || row.topic_uuid.is_nil()
                || row.stream_uuid != scope.stream_id
                || scope.topic_id.is_some_and(|id| id != row.topic_uuid)
                || !matches_owner(owner, row.project_id.as_deref(), row.user_uuid.as_deref())
                || &created_at < first
                || &created_at > last
                || previous
                    .as_ref()
                    .is_some_and(|key| key >= &(created_at.clone(), row.uuid))
            {
                return Err(SourceError::InvalidResponse.into());
            }
            previous = Some((created_at.clone(), row.uuid));
            result.push(HistoryReadRow {
                id: row.uuid,
                scope: HistoryScope {
                    stream_id: row.stream_uuid,
                    topic_id: Some(row.topic_uuid),
                },
                created_at,
                read: row.read,
            });
        }
        if let Some(next) = &next
            && next.parse::<Uuid>().ok() != result.last().map(|row| row.id)
        {
            return Err(SourceError::InvalidResponse.into());
        }
        Ok((result, next))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustls::{
        ServerConfig,
        crypto::ring,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    };
    use serde_json::json;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        time::{Duration, timeout},
    };
    use tokio_rustls::TlsAcceptor;

    use crate::context::AccountId;

    use super::*;

    const CA: &[u8] = include_bytes!("../../../../../tests/fixtures/tls/ca.pem");
    const CERT: &[u8] = include_bytes!("../../../../../tests/fixtures/tls/localhost.pem");
    const KEY: &[u8] = include_bytes!("../../../../../tests/fixtures/tls/localhost-key.pem");

    /// Удаление маркера требует сверки окна, а подтверждённый отказ доступа закрывает тему.
    #[tokio::test]
    async fn history_read_missing_marker_rechecks_topic_access() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let owner = OwnerContext {
            account_id: AccountId("local".into()),
            server: format!("https://{}", listener.local_addr().unwrap()),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let stream = Uuid::from_u128(10);
        let topic = Uuid::from_u128(11);
        let marker = Uuid::from_u128(12).to_string();
        let client =
            RestClient::new(Some(CertificateDer::from_pem_slice(CA).unwrap().as_ref())).unwrap();
        let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(CERT).unwrap()],
                PrivateKeyDer::from_pem_slice(KEY).unwrap(),
            )
            .unwrap();
        let tls = TlsAcceptor::from(Arc::new(config));
        let topic_body = json!({
            "uuid": topic, "stream_uuid": stream, "project_id": "project",
            "user_uuid": "user", "notification_mode": "default",
        })
        .to_string();
        let expected_marker = marker.clone();
        let server = tokio::spawn(async move {
            for (page, status, body) in [
                (true, "404 Not Found", ""),
                (false, "200 OK", topic_body.as_str()),
                (true, "404 Not Found", ""),
                (false, "403 Forbidden", ""),
            ] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(tls.accept(socket).await.unwrap());
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                if page {
                    assert!(line.starts_with("GET /api/workspace/v1/messenger/messages/?"));
                    assert!(line.contains(&format!("page_marker={expected_marker}")));
                } else {
                    assert_eq!(
                        line,
                        format!(
                            "GET /api/workspace/v1/messenger/stream_topics/{topic} HTTP/1.1\r\n"
                        )
                    );
                }
                loop {
                    line.clear();
                    assert!(socket.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        });
        let scope = HistoryScope {
            stream_id: stream,
            topic_id: Some(topic),
        };
        let time = HistoryTime::parse("2026-01-01T10:00:00Z").unwrap();
        let page =
            || client.history_read_page(&owner, "token", scope, (&time, &time), Some(&marker));
        assert!(matches!(page().await, Err(OpenHistoryError::StaleWindow)));
        assert!(matches!(page().await, Err(OpenHistoryError::AccessLost)));
        timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
}
