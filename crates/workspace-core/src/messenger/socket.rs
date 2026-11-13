use std::{pin::Pin, time::Duration};

use futures::{SinkExt, Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::Value;
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{
        Error as WebSocketError, Message,
        client::IntoClientRequest,
        handshake::client::Request,
        http::{HeaderValue, header::SEC_WEBSOCKET_PROTOCOL},
        protocol::{WebSocketConfig, frame::coding::CloseCode},
    },
};

use crate::{
    context::OwnerContext,
    runtime::{SourceError, SourceSession},
    storage::Cursor,
    sync::Event,
};

use super::rest::{validate_cursor, wire_event, workspace_origin};

const EVENTS_PATH: &str = "/api/workspace/v1/events/ws";
const EVENTS_PROTOCOL: &str = "workspace.events.v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const FRAME_IDLE_TIMEOUT: Duration = Duration::from_secs(35);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

pub(super) enum SocketFrame {
    Event(Event),
    Ready,
}

pub(super) type SocketStream = Pin<Box<dyn Stream<Item = Result<SocketFrame, SourceError>> + Send>>;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) async fn connect(session: &SourceSession) -> Result<SocketStream, SourceError> {
    let cursor = session
        .cursor
        .as_ref()
        .ok_or(SourceError::InvalidResponse)?;
    let request = request(&session.context, cursor, &session.access_token)?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_EVENT_BYTES))
        .max_frame_size(Some(MAX_EVENT_BYTES));
    // The async connector opens one connection and does not follow redirects.
    let (socket, response) = timeout(
        CONNECT_TIMEOUT,
        connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(|_| SourceError::Temporary)?
    .map_err(websocket_error)?;
    if response
        .headers()
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|header| header.to_str().ok())
        != Some(EVENTS_PROTOCOL)
    {
        return Err(SourceError::InvalidResponse);
    }

    let state = SocketState {
        socket,
        context: session.context.clone(),
        epoch: cursor.epoch_generation.clone(),
        ready: false,
    };
    Ok(Box::pin(stream::unfold(Some(state), |state| async move {
        let mut state = state?;
        match state.next_frame().await {
            Ok(frame) => Some((Ok(frame), Some(state))),
            Err(error) => Some((Err(error), None)),
        }
    })))
}

fn request(
    owner: &OwnerContext,
    cursor: &Cursor,
    access_token: &str,
) -> Result<Request, SourceError> {
    validate_cursor(cursor)?;
    let mut url = workspace_origin(owner)?;
    url.set_scheme("wss")
        .map_err(|()| SourceError::InvalidResponse)?;
    url.set_path(EVENTS_PATH);
    url.query_pairs_mut()
        .append_pair("last_epoch_version", &cursor.epoch_version.to_string())
        .append_pair("epoch_generation", &cursor.epoch_generation);

    // The bearer value is a WebSocket subprotocol token, which forbids
    // separators such as whitespace or commas. IAM issues compact JWTs.
    if access_token.is_empty()
        || !access_token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'~'))
    {
        return Err(SourceError::Unauthorized);
    }
    let mut protocol = HeaderValue::from_str(&format!("{EVENTS_PROTOCOL}, bearer.{access_token}"))
        .map_err(|_| SourceError::Unauthorized)?;
    protocol.set_sensitive(true);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(websocket_error)?;
    request
        .headers_mut()
        .insert(SEC_WEBSOCKET_PROTOCOL, protocol);
    Ok(request)
}

struct SocketState {
    socket: Socket,
    context: OwnerContext,
    epoch: String,
    ready: bool,
}

impl SocketState {
    async fn next_frame(&mut self) -> Result<SocketFrame, SourceError> {
        loop {
            match self.read_message().await? {
                Message::Text(text) => {
                    let frame = parse_text(&text, &self.context, &self.epoch)?;
                    if matches!(frame, SocketFrame::Ready) {
                        if self.ready {
                            return Err(SourceError::InvalidResponse);
                        }
                        self.ready = true;
                    }
                    return Ok(frame);
                }
                Message::Ping(_) => {
                    // Tungstenite queues the matching pong automatically.
                    timeout(HEARTBEAT_TIMEOUT, self.socket.flush())
                        .await
                        .map_err(|_| SourceError::Temporary)?
                        .map_err(websocket_error)?;
                }
                Message::Pong(_) => {}
                Message::Close(close) => {
                    let code = close.map(|frame| frame.code);
                    let _ = timeout(HEARTBEAT_TIMEOUT, self.socket.flush()).await;
                    return Err(close_error(code));
                }
                Message::Binary(_) | Message::Frame(_) => return Err(SourceError::InvalidResponse),
            }
        }
    }

    async fn read_message(&mut self) -> Result<Message, SourceError> {
        let next = match timeout(FRAME_IDLE_TIMEOUT, self.socket.next()).await {
            Ok(next) => next,
            Err(_) => {
                timeout(
                    HEARTBEAT_TIMEOUT,
                    self.socket.send(Message::Ping(Vec::new().into())),
                )
                .await
                .map_err(|_| SourceError::Temporary)?
                .map_err(websocket_error)?;
                timeout(HEARTBEAT_TIMEOUT, self.socket.next())
                    .await
                    .map_err(|_| SourceError::Temporary)?
            }
        };
        next.ok_or(SourceError::Temporary)?.map_err(websocket_error)
    }
}

fn parse_text(text: &str, context: &OwnerContext, epoch: &str) -> Result<SocketFrame, SourceError> {
    let value: Value = serde_json::from_str(text).map_err(|_| SourceError::InvalidResponse)?;
    if value.get("error").and_then(Value::as_str) == Some("epoch_pruned") {
        return Err(SourceError::CursorPruned);
    }
    if value.get("type").and_then(Value::as_str) == Some("ready") {
        let marker: ReadyMarker =
            serde_json::from_value(value).map_err(|_| SourceError::InvalidResponse)?;
        if marker.epoch_generation != epoch {
            return Err(SourceError::CursorPruned);
        }
        return Ok(SocketFrame::Ready);
    }
    Ok(SocketFrame::Event(wire_event(value, context, epoch)?))
}

#[derive(Deserialize)]
struct ReadyMarker {
    epoch_generation: String,
    #[serde(rename = "epoch_version")]
    _epoch_version: u64,
}

fn close_error(code: Option<CloseCode>) -> SourceError {
    match code.map(u16::from) {
        Some(4400) => SourceError::InvalidResponse,
        Some(4401) => SourceError::Unauthorized,
        Some(4403) => SourceError::Forbidden,
        Some(4410) => SourceError::CursorPruned,
        _ => SourceError::Temporary,
    }
}

fn websocket_error(error: WebSocketError) -> SourceError {
    match error {
        WebSocketError::Http(response) => match response.status().as_u16() {
            401 => SourceError::Unauthorized,
            403 => SourceError::Forbidden,
            410 => SourceError::CursorPruned,
            408 | 429 | 500..=599 => SourceError::Temporary,
            _ => SourceError::InvalidResponse,
        },
        WebSocketError::Capacity(_)
        | WebSocketError::Protocol(_)
        | WebSocketError::Utf8(_)
        | WebSocketError::AttackAttempt
        | WebSocketError::Url(_)
        | WebSocketError::HttpFormat(_) => SourceError::InvalidResponse,
        _ => SourceError::Temporary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AccountId;

    fn owner() -> OwnerContext {
        OwnerContext {
            account_id: AccountId("account".into()),
            server: "https://example.test:9443".into(),
            organization_id: "org".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        }
    }

    #[test]
    fn handshake_sends_cursor_and_sensitive_bearer_subprotocol() {
        let owner = owner();
        let cursor = Cursor {
            epoch_generation: "a+b&c".into(),
            epoch_version: 12,
        };
        let request = request(&owner, &cursor, "test-token").unwrap();
        assert_eq!(
            request.uri().to_string(),
            "wss://example.test:9443/api/workspace/v1/events/ws?last_epoch_version=12&epoch_generation=a%2Bb%26c"
        );
        let protocol = &request.headers()[SEC_WEBSOCKET_PROTOCOL];
        assert_eq!(protocol, "workspace.events.v1, bearer.test-token");
        assert!(protocol.is_sensitive());
        assert!(!request.headers().contains_key("authorization"));
        assert!(matches!(
            super::request(&owner, &cursor, "test-token, bearer.invalid"),
            Err(SourceError::Unauthorized)
        ));
    }

    #[test]
    fn ready_and_cursor_pruned_control_frames_are_not_events() {
        let owner = owner();
        assert!(matches!(
            parse_text(
                r#"{"type":"ready","epoch_generation":"one","epoch_version":124}"#,
                &owner,
                "one",
            ),
            Ok(SocketFrame::Ready)
        ));
        assert!(matches!(
            parse_text(
                r#"{"type":"ready","epoch_generation":"two","epoch_version":0}"#,
                &owner,
                "one",
            ),
            Err(SourceError::CursorPruned)
        ));
        assert!(matches!(
            parse_text(
                r#"{"type":"EventsCursorExpiredError","error":"epoch_pruned"}"#,
                &owner,
                "one",
            ),
            Err(SourceError::CursorPruned)
        ));
        assert_eq!(
            close_error(Some(4401_u16.into())),
            SourceError::Unauthorized
        );
        assert_eq!(
            close_error(Some(4410_u16.into())),
            SourceError::CursorPruned
        );
    }
}
