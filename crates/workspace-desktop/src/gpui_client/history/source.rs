//! Вызывает операции истории и чтение очереди на рабочем потоке. Ошибки переводит в безопасные состояния UI.

use std::sync::Arc;

use futures::{channel::oneshot, future::BoxFuture};
use uuid::Uuid;
use workspace_core::{
    messenger::history::{MediaAccess, OpenHistoryError},
    notification_target::NotificationTarget,
    storage::{HistoryDirection, HistoryWindow, OutboxEntry},
};

use crate::gpui_client::{
    contracts::{ChatEntry, ConversationAddress, ConversationReply},
    runtime::{RuntimeClient, WorkerServices},
};

/// Запрос, который экран отправляет рабочему потоку.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Load {
    Open(ChatEntry),               // Окно возле цели или подтверждённый хвост.
    Extend(i64, HistoryDirection), // ID окна и направление догрузки.
    Read(i64),                     // Перечитать подтверждённое окно и очередь из SQLite.
}

/// Снимок для экрана, связанный с проверенным окном истории.
pub(super) struct Snapshot {
    pub window: HistoryWindow,             // Подтверждённое окно с краями.
    pub outbox: Option<Vec<OutboxEntry>>,  // None при ошибке чтения очереди.
    pub media_access: Option<MediaAccess>, // Допуск после открытия разговора, если профиль настроил кэш.
}

/// Причина, которую можно показать без содержимого сетевой ошибки.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Failure {
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
    let (window, media_access) = match load {
        Load::Open(ChatEntry::Latest) => {
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
        Load::Open(ChatEntry::Around(id)) => {
            let opening = if let Some(topic) = scope.topic_id {
                source
                    .open_topic_history_with_media(
                        sync,
                        owner,
                        generation,
                        scope.stream_id,
                        topic,
                        id,
                    )
                    .await?
            } else {
                let target = NotificationTarget {
                    context: owner.clone(),
                    conversation_id: scope.stream_id.to_string(),
                    message_id: id.to_string(),
                };
                source
                    .open_direct_history_with_media(sync, &target, generation)
                    .await?
            };
            (opening.window, opening.media_access)
        }
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
    };
    Ok(read(services, address, window.id)?.map(|mut snapshot| {
        snapshot.media_access = media_access;
        snapshot
    }))
}

/// Читает очередь перед историей. Если подтверждение придёт между чтениями, UI сверит UUID.
fn read(services: &WorkerServices, address: &ConversationAddress, id: i64) -> LoadResult {
    let route = &address.route;
    let outbox = services
        .sync
        .outbox(&route.owner, address.generation, route.scope)
        .ok();
    let window = services
        .sync
        .refresh_history_window(&route.owner, address.generation, route.scope, id)
        .map_err(|_| Failure::Unavailable)?;
    Ok(window.map(|window| Snapshot {
        window,
        outbox,
        media_access: None,
    }))
}
