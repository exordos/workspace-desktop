//! Вызывает операции истории и чтение очереди на рабочем потоке. Ошибки переводит в безопасные состояния UI.

use std::sync::Arc;

use futures::{channel::oneshot, future::BoxFuture};
use uuid::Uuid;
use workspace_core::{
    messenger::history::{MediaAccess, OpenHistoryError},
    notification_target::NotificationTarget,
    storage::{HistoryDirection, HistoryScope, HistoryWindow, OutboxEntry},
};

use crate::gpui_client::{
    contracts::{ChatEntry, ConversationAddress, ConversationReply},
    runtime::{RuntimeClient, WorkerServices},
};

/// Запрос, который экран отправляет рабочему потоку.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Load {
    Open(ChatEntry),               // Окно возле цели или подтверждённый хвост.
    OpenUnread,                    // Обычное открытие, начинающееся у первого непрочитанного.
    Extend(i64, HistoryDirection), // ID окна и направление догрузки.
    Read(i64),                     // Перечитать подтверждённое окно и очередь из SQLite.
    RevalidateRead(i64, Option<HistoryScope>), // Обновить отметки окна по журналу сервера.
}

impl Load {
    /// Отличает открытие новой области от перечитывания и догрузки существующего окна.
    pub(super) fn opens(self) -> bool {
        matches!(self, Self::Open(_) | Self::OpenUnread)
    }
}

/// Снимок для экрана, связанный с проверенным окном истории.
pub(super) struct Snapshot {
    pub window: HistoryWindow,             // Подтверждённое окно с краями.
    pub outbox: Option<Vec<OutboxEntry>>,  // None при ошибке чтения очереди.
    pub media_access: Option<MediaAccess>, // Допуск после открытия разговора, если профиль настроил кэш.
    pub first_unread: Option<Uuid>,        // UUID для начальной позиции обычного открытия.
    pub unread_checked: bool,              // Окно охватывает проверенную точку непрочитанных.
}

/// Причина, которую можно показать без содержимого сетевой ошибки.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::gpui_client) enum Failure {
    AccessLost,     // Доступ был отозван.
    MissingMessage, // Целевого сообщения больше нет в области.
    Changed,        // Окно менялось во время чтения, нужно проверить его заново.
    Unavailable,    // Запрос или чтение временно не удались.
}

impl From<OpenHistoryError> for Failure {
    /// Оставляет только различия, от которых зависит действие пользователя.
    fn from(error: OpenHistoryError) -> Self {
        match error {
            OpenHistoryError::AccessLost => Self::AccessLost,
            OpenHistoryError::MessageMissing | OpenHistoryError::InvalidTarget => {
                Self::MissingMessage
            }
            OpenHistoryError::StaleWindow => Self::Changed,
            _ => Self::Unavailable,
        }
    }
}

/// Результат открытия или обновления. None означает, что локальное окно больше нельзя показывать.
pub(super) type LoadResult = Result<Option<Snapshot>, Failure>;

/// Запускает один запрос. Сохранённый хвост может прийти по отдельному каналу до сети.
pub(super) type Source = Arc<
    dyn Fn(
        ConversationAddress,
        Load,
        Uuid,
        oneshot::Sender<Snapshot>,
    ) -> BoxFuture<'static, ConversationReply<LoadResult>>,
>;

/// Подключает рабочий поток к запросам разговора.
pub(super) fn runtime_source(runtime: RuntimeClient) -> Source {
    Arc::new(move |address, load, op_id, cached| {
        let runtime = runtime.clone();
        Box::pin(async move {
            let response = runtime
                .conversation(address, move |services, address| async move {
                    log::debug!("gpui_history op_id={op_id} event=worker_started");
                    let result = fetch(&services, &address, load, cached).await;
                    log::debug!(
                        "gpui_history op_id={op_id} event=worker_completed success={}",
                        result.is_ok()
                    );
                    result
                })
                .await;
            ConversationReply {
                address: response.address,
                value: response.value.unwrap_or(Err(Failure::Unavailable)),
            }
        })
    })
}

/// Читает подтверждённый хвост, запрашивает страницы либо перечитывает окно по событию Sync.
async fn fetch(
    services: &WorkerServices,
    address: &ConversationAddress,
    load: Load,
    cached: oneshot::Sender<Snapshot>,
) -> LoadResult {
    let route = &address.route;
    let generation = address.generation;
    let owner = &route.owner;
    let scope = route.scope;
    let source = &services.messenger;
    let sync = &services.sync;
    let mut unread_checked = false;
    if load == Load::OpenUnread && native_markdown(services, address) {
        // Сначала проверяем границу. Сохранённый хвост не должен мелькнуть перед переходом к ней.
        for _ in 0..2 {
            let boundary = match source.first_unread(sync, owner, generation, scope).await {
                Ok(Some(anchor)) if anchor.eligible => anchor.boundary,
                Ok(None) => {
                    unread_checked = true;
                    break;
                }
                Err(OpenHistoryError::AccessLost) => return Err(Failure::AccessLost),
                _ => break,
            };
            let id = boundary.target.message_id;
            match around(services, address, id).await {
                Ok((window, media_access)) => {
                    let Some(mut snapshot) = read(services, address, window.id)? else {
                        continue;
                    };
                    if snapshot.window.messages.iter().any(|row| {
                        row.id == id
                            && row.topic_id == boundary.target.topic_id
                            && row.metadata.created_at == boundary.created_at
                            && row.read == Some(false)
                    }) {
                        snapshot.media_access = media_access;
                        snapshot.first_unread = Some(id);
                        snapshot.unread_checked = true;
                        return Ok(Some(snapshot));
                    }
                }
                Err(Failure::AccessLost) => return Err(Failure::AccessLost),
                Err(Failure::Unavailable) => break,
                Err(Failure::MissingMessage | Failure::Changed) => {}
            }
            // Пока шла загрузка, строку могли прочитать в другом клиенте или удалить.
        }
    }
    let (window, media_access) = match load {
        Load::OpenUnread | Load::Open(ChatEntry::Latest | ChatEntry::Archived) => {
            let mut opening = source
                .open_latest_history(sync, owner, generation, scope)
                .await?;
            let media_access = opening.media_access();
            if let Some(window) = opening.cached.take() {
                if let Some(mut snapshot) = read(services, address, window.id)? {
                    snapshot.media_access = media_access.clone();
                    let _ = cached.send(snapshot);
                }
            } else {
                drop(cached);
            }
            (opening.refresh().await?, media_access)
        }
        Load::Open(ChatEntry::Around(id)) => around(services, address, id).await?,
        Load::Extend(id, direction) => {
            let result = match direction {
                HistoryDirection::Older => {
                    source
                        .load_older_history(sync, owner, generation, scope, id)
                        .await
                }
                HistoryDirection::Newer => {
                    source
                        .load_newer_history(sync, owner, generation, scope, id)
                        .await
                }
            };
            match result {
                // Один 404 страницы также может означать исчезновение маркера.
                Err(OpenHistoryError::AccessLost) => {
                    source
                        .verify_conversation_access(sync, owner, generation, scope)
                        .await?;
                    return Err(Failure::Changed);
                }
                other => (other?, None),
            }
        }
        Load::Read(id) => return read(services, address, id),
        Load::RevalidateRead(id, changed) => {
            source
                .refresh_history_read(sync, owner, generation, scope, id, changed)
                .await?;
            return read(services, address, id);
        }
    };
    Ok(read(services, address, window.id)?.map(|mut snapshot| {
        snapshot.media_access = media_access;
        // При конкурентном новом сообщении в хвосте границу подтвердит следующий вход
        // либо серверная проверка префикса перед отправкой отметки.
        snapshot.unread_checked = unread_checked
            && window
                .messages
                .iter()
                .all(|message| message.read == Some(true));
        snapshot
    }))
}

/// Использует подтверждение каталога, прежде чем искать непрочитанное в нативном разговоре.
fn native_markdown(services: &WorkerServices, address: &ConversationAddress) -> bool {
    services
        .catalog
        .subscribe()
        .borrow()
        .get(&address.route.owner)
        .filter(|catalog| catalog.generation == address.generation)
        .and_then(|catalog| catalog.chats.as_ref())
        .and_then(|chats| {
            chats
                .iter()
                .find(|chat| chat.id == address.route.scope.stream_id)
        })
        .is_some_and(|chat| chat.metadata.native_markdown)
}

/// Открывает точное серверное окно вокруг сообщения канала или лички.
async fn around(
    services: &WorkerServices,
    address: &ConversationAddress,
    id: Uuid,
) -> Result<(HistoryWindow, Option<MediaAccess>), Failure> {
    let route = &address.route;
    let source = &services.messenger;
    let opening = if let Some(topic) = route.scope.topic_id {
        source
            .open_topic_history_with_media(
                &services.sync,
                &route.owner,
                address.generation,
                route.scope.stream_id,
                topic,
                id,
            )
            .await?
    } else {
        let target = NotificationTarget {
            context: route.owner.clone(),
            conversation_id: route.scope.stream_id.to_string(),
            message_id: id.to_string(),
        };
        source
            .open_direct_history_with_media(&services.sync, &target, address.generation)
            .await?
    };
    Ok((opening.window, opening.media_access))
}

/// Читает очередь перед историей. Если подтверждение придёт между чтениями, UI сверит UUID.
fn read(services: &WorkerServices, address: &ConversationAddress, id: i64) -> LoadResult {
    let route = &address.route;
    let outbox = if route.entry == ChatEntry::Archived {
        None
    } else {
        services
            .sync
            .outbox(&route.owner, address.generation, route.scope)
            .ok()
    };
    let window = services
        .sync
        .refresh_history_window(&route.owner, address.generation, route.scope, id)
        .map_err(|_| Failure::Unavailable)?;
    Ok(window.map(|window| Snapshot {
        window,
        outbox,
        media_access: None,
        first_unread: None,
        unread_checked: false,
    }))
}
