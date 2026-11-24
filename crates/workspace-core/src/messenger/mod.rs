//! Подключает журнал мессенджера к фоновой синхронизации.
//! Сначала источник догоняет журнал по HTTP, затем читает новые события через WebSocket.

pub mod activity;
pub mod calls;
pub mod catalog;
pub mod details;
pub mod encrypted_send;
pub mod encryption;
pub mod files;
mod folder_sync;
pub mod folders;
pub mod forward;
pub mod history;
pub mod message_actions;
mod outbox;
pub mod own_profile;
pub mod quick_reactions;
pub mod quote;
pub mod reactions;
pub mod read;
pub mod references;
mod rest;
pub mod send;
pub mod sidebar_actions;
mod socket;
pub mod summary_settings;

use std::{sync::Arc, time::Duration, vec::IntoIter};

use futures::{StreamExt, future::BoxFuture, stream};
use rustls::{ClientConfig, RootCertStore, crypto::ring, pki_types::CertificateDer};
use tokio::{sync::watch, time::timeout};
use tokio_tungstenite::Connector;

use crate::{
    auth::SessionSnapshot,
    runtime::{EventSource, EventStream, SourceError, SourceSession},
    sync::{Event, Sync},
};

pub use folder_sync::{FolderRow, FolderSnapshot};

use rest::RestClient;
use socket::SocketFrame;

pub const SOURCE_ID: &str = "messenger";
const NOTIFICATION_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Источник событий мессенджера. При первом подключении берёт текущую позицию журнала.
/// При следующих подключениях читает события после последней сохранённой позиции.
pub struct MessengerSource {
    rest: Arc<RestClient>,        // Общий HTTP-клиент для чтения страниц журнала.
    connector: Option<Connector>, // Настройки TLS для сервера с частным центром сертификации.
    catalog_refresh: Arc<catalog::RefreshGates>, // Общие запросы каталога одного владельца.
    chat_encryption: Option<encrypted_send::ChatEncryption>, // Локальные ключи этого профиля.
    self_chat_requests: activity::SelfChatRequests, // Общий запрос «Избранного» одного входа.
    reaction_requests: reactions::ReactionRequests, // Активные команды переключения реакций.
}

impl MessengerSource {
    /// Подключает каталог чатов к тому же HTTPS-клиенту, что и поток событий.
    pub fn catalog(&self, storage: Arc<crate::storage::Storage>) -> catalog::Catalog {
        catalog::Catalog::new(self.rest.clone(), storage, self.catalog_refresh.clone())
    }

    /// Даёт доступ к снимкам и командам папок через общий клиент Messenger.
    pub fn folders(&self, storage: Arc<crate::storage::Storage>) -> folders::Folders {
        folders::Folders::new(self.rest.clone(), storage)
    }

    /// Получает объявленную сервером HTTPS-ссылку регистрации, доступную до входа.
    pub async fn registration_url(&self, server: &str) -> Result<Option<String>, SourceError> {
        self.rest.registration_url(server).await
    }

    /// Создаёт источник и подготавливает HTTP-клиент.
    pub fn new() -> Result<Self, SourceError> {
        Ok(Self {
            rest: Arc::new(RestClient::new(None)?),
            connector: None,
            catalog_refresh: Arc::default(),
            chat_encryption: None,
            self_chat_requests: activity::SelfChatRequests::default(),
            reaction_requests: reactions::ReactionRequests::default(),
        })
    }

    /// Доверяет указанному сертификату центра сертификации вместо системных корней для HTTPS и WSS.
    pub fn with_private_ca(certificate_der: &[u8]) -> Result<Self, SourceError> {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(certificate_der))
            .map_err(|_| SourceError::InvalidResponse)?;
        let tls = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| SourceError::InvalidResponse)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            rest: Arc::new(RestClient::new(Some(certificate_der))?),
            connector: Some(Connector::Rustls(Arc::new(tls))),
            catalog_refresh: Arc::default(),
            chat_encryption: None,
            self_chat_requests: activity::SelfChatRequests::default(),
            reaction_requests: reactions::ReactionRequests::default(),
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
                connector: self.connector.clone(),
            };
            let events = stream::try_unfold(connection, |mut connection| async move {
                Ok(connection.next().await?.map(|event| (event, connection)))
            });
            Ok(Box::pin(events) as EventStream)
        })
    }

    /// Обрабатывает очередь, папки и удаление черновиков рядом с потоком событий.
    fn run_session(
        &self,
        sync: Arc<Sync>,
        session: SessionSnapshot,
        ready: watch::Receiver<bool>,
    ) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::join!(
                outbox::run(self, sync.clone(), session.clone(), ready.clone()),
                folder_sync::run(self, sync.clone(), session.clone(), ready.clone()),
                encryption::fragments::run_protected_draft_purges(self, &sync, &session, ready),
                sync.run_media_cleanup(),
            );
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
    connector: Option<Connector>, // Частный корневой сертификат либо обычное системное доверие.
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
            self.socket = Some(socket::connect(&self.session, self.connector.clone()).await?);
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
                            && let (
                                Event::MessageCreated {
                                    cursor,
                                    message,
                                    placement,
                                    metadata,
                                },
                                Some(incoming),
                            ) = (event.as_ref(), incoming)
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
                                        placement: placement.clone(),
                                        metadata: metadata.clone(),
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
                        return Ok(Some(*event));
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
                            return Ok(Some(Event::SourceReady(cursor)));
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}
