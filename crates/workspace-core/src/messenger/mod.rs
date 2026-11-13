//! Подключает журнал мессенджера к фоновой синхронизации.
//! Сначала источник догоняет журнал по HTTP, затем читает новые события через WebSocket.

mod rest;
mod socket;

use std::{sync::Arc, time::Duration, vec::IntoIter};

use futures::{StreamExt, future::BoxFuture, stream};
use tokio::time::timeout;

use crate::{
    runtime::{EventSource, EventStream, SourceError, SourceSession},
    sync::Event,
};

use rest::RestClient;
use socket::SocketFrame;

pub const SOURCE_ID: &str = "messenger";
const NOTIFICATION_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Источник событий мессенджера. При первом подключении берёт текущую позицию журнала.
/// При следующих подключениях читает события после последней сохранённой позиции.
pub struct MessengerSource {
    rest: Arc<RestClient>, // Общий HTTP-клиент для чтения страниц журнала.
}

impl MessengerSource {
    /// Создаёт источник и подготавливает HTTP-клиент.
    pub fn new() -> Result<Self, SourceError> {
        Ok(Self {
            rest: Arc::new(RestClient::new()?),
        })
    }
}

impl EventSource for MessengerSource {
    /// Возвращает постоянное имя источника мессенджера.
    fn id(&self) -> &str {
        SOURCE_ID
    }

    /// Готовит поток для сессии. Сетевые запросы начнутся при чтении первого элемента.
    fn connect(&self, session: SourceSession) -> BoxFuture<'_, Result<EventStream, SourceError>> {
        Box::pin(async move {
            let connection = Connection {
                rest: self.rest.clone(),
                session,
                pending: Default::default(),
                socket: None,
                live: false,
            };
            let events = stream::try_unfold(connection, |mut connection| async move {
                Ok(connection.next().await?.map(|event| (event, connection)))
            });
            Ok(Box::pin(events) as EventStream)
        })
    }
}

/// Состояние чтения журнала в одном подключении.
struct Connection {
    rest: Arc<RestClient>, // HTTP-клиент для начальной позиции и пропущенных событий.
    session: SourceSession, // Параметры сессии и последняя позиция, выданная этим потоком.
    pending: IntoIter<Event>, // Остаток загруженной страницы, который ещё не отдали потребителю.
    socket: Option<socket::SocketStream>, // Открытый поток WebSocket. `None`, пока идёт чтение через HTTP.
    live: bool,                           // Сервер подтвердил завершение догрузки через WebSocket.
}

impl Connection {
    /// Выдаёт начальную позицию, следующее событие из REST или новое событие из WebSocket.
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
                SocketFrame::Event(event, incoming) => {
                    let cursor = self.session.cursor.as_ref().expect("baseline initialized");
                    if event.cursor().epoch_version > cursor.epoch_version {
                        self.session.cursor = Some(event.cursor().clone());
                        // Догрузка через REST и кадры до ready не создают баннеры.
                        if self.live
                            && let (Event::MessageCreated { cursor, message }, Some(incoming)) =
                                (&event, incoming)
                        {
                            match timeout(
                                NOTIFICATION_LOOKUP_TIMEOUT,
                                self.rest.can_notify(
                                    &self.session.context,
                                    &self.session.access_token,
                                    &incoming,
                                ),
                            )
                            .await
                            {
                                Ok(Ok(true)) => {
                                    return Ok(Some(Event::IncomingDirectMessage {
                                        cursor: cursor.clone(),
                                        message: message.clone(),
                                        conversation_id: incoming.stream.to_string(),
                                    }));
                                }
                                Ok(Ok(false)) => {}
                                Ok(Err(error)) => log::debug!(
                                    "Messenger notification metadata unavailable: {error}"
                                ),
                                Err(_) => {
                                    log::debug!("Messenger notification metadata request timed out")
                                }
                            }
                        }
                        return Ok(Some(event));
                    }
                }
                SocketFrame::Ready(cursor) => {
                    self.live = true;
                    let snapshot = self
                        .rest
                        .unread_snapshot(&self.session.context, &self.session.access_token)
                        .await;
                    match snapshot {
                        Ok(count) => {
                            // Курсор ready задаёт порядок для счётчика. Позиция журнала не меняется.
                            log::info!("Messenger WebSocket ready");
                            return Ok(Some(Event::UnreadBaseline { cursor, count }));
                        }
                        Err(error @ (SourceError::Unauthorized | SourceError::Temporary)) => {
                            return Err(error);
                        }
                        Err(error) => {
                            log::warn!("Messenger unread snapshot unavailable: {error}");
                            log::info!("Messenger WebSocket ready");
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}
