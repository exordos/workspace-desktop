mod rest;
mod socket;

use std::{sync::Arc, vec::IntoIter};

use futures::{StreamExt, future::BoxFuture, stream};

use crate::{
    runtime::{EventSource, EventStream, SourceError, SourceSession},
    sync::Event,
};

use rest::RestClient;
use socket::SocketFrame;

pub const SOURCE_ID: &str = "messenger";

/// The first connection establishes a baseline for new incoming messages.
/// Every subsequent connection backfills from the last committed cursor.
pub struct MessengerSource {
    rest: Arc<RestClient>,
}

impl MessengerSource {
    pub fn new() -> Result<Self, SourceError> {
        Ok(Self {
            rest: Arc::new(RestClient::new()?),
        })
    }
}

impl EventSource for MessengerSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    fn connect(&self, session: SourceSession) -> BoxFuture<'_, Result<EventStream, SourceError>> {
        Box::pin(async move {
            let connection = Connection {
                rest: self.rest.clone(),
                session,
                pending: Default::default(),
                socket: None,
            };
            let events = stream::try_unfold(connection, |mut connection| async move {
                Ok(connection.next().await?.map(|event| (event, connection)))
            });
            Ok(Box::pin(events) as EventStream)
        })
    }
}

struct Connection {
    rest: Arc<RestClient>,
    session: SourceSession,
    pending: IntoIter<Event>,
    socket: Option<socket::SocketStream>,
}

impl Connection {
    async fn next(&mut self) -> Result<Option<Event>, SourceError> {
        if self.session.cursor.is_none() {
            let cursor = self
                .rest
                .current_cursor(&self.session.context, &self.session.access_token)
                .await?;
            self.session.cursor = Some(cursor.clone());
            return Ok(Some(Event::Checkpoint(cursor)));
        }

        if self.socket.is_none() {
            if self.pending.as_slice().is_empty() {
                let cursor = self.session.cursor.as_ref().expect("baseline initialized");
                let page = self
                    .rest
                    .page(&self.session.context, &self.session.access_token, cursor)
                    .await?;
                if !page.is_empty() {
                    log::info!(
                        "Messenger REST catch-up after {}: {} events",
                        cursor.epoch_version,
                        page.len()
                    );
                }
                self.pending = page.into_iter();
            }
            if let Some(event) = self.pending.next() {
                self.session.cursor = Some(event.cursor().clone());
                return Ok(Some(event));
            }
            self.socket = Some(socket::connect(&self.session).await?);
        }

        let socket = self.socket.as_mut().expect("socket initialized");
        while let Some(frame) = socket.next().await {
            match frame? {
                SocketFrame::Event(event) => {
                    let cursor = self.session.cursor.as_ref().expect("baseline initialized");
                    if event.cursor().epoch_version > cursor.epoch_version {
                        self.session.cursor = Some(event.cursor().clone());
                        return Ok(Some(event));
                    }
                }
                SocketFrame::Ready => log::info!("Messenger WebSocket ready"),
            }
        }
        Ok(None)
    }
}
