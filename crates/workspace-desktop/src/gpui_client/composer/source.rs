//! Обращается к core с рабочего потока и возвращает только безопасный итог для редактора.

use std::sync::Arc;

use futures::future::BoxFuture;
use uuid::Uuid;
use workspace_core::{
    messenger::{quote::SourceCheckError, send::DirectSendError},
    runtime::SourceError,
    storage::{DraftAccess, DraftScope, PlaintextDraft},
};

use super::Submission;
use crate::gpui_client::{
    contracts::{ConversationAddress, ConversationReply},
    runtime::{RuntimeClient, WorkerServices},
};

/// Уточняет причину недоступности отправки без деталей сетевой ошибки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gpui_client) enum DestinationError {
    AccessLost,            // Стрим или тема недоступны исходному входу.
    EncryptionUnavailable, // Сервер не разрешил отправку открытого текста.
    TopicUnavailable,      // Сервер не предоставил пригодный UUID темы.
    TopicClosed,           // Отправку ограничивает завершённая тема канала.
    TopicChanged,          // Сервер указал другую тему для новой отправки.
    Unavailable,           // Проверка цели временно не завершилась.
    DraftUnavailable,      // SQLite не смогла безопасно открыть текущую версию поля.
}

impl From<DirectSendError> for DestinationError {
    /// Отбрасывает детали токенов, адресов и HTTP-ответов.
    fn from(error: DirectSendError) -> Self {
        match error {
            DirectSendError::AccessLost => Self::AccessLost,
            DirectSendError::EncryptionUnavailable => Self::EncryptionUnavailable,
            DirectSendError::TopicUnavailable => Self::TopicUnavailable,
            DirectSendError::TopicClosed => Self::TopicClosed,
            DirectSendError::Auth(_) | DirectSendError::Source(_) => Self::Unavailable,
        }
    }
}

/// Проверенная тема открытого текста и её локальный черновик.
pub(in crate::gpui_client) struct ResolvedDestination {
    pub topic: Uuid,  // Проверенная тема обычного текста.
    pub closed: bool, // Для чтения черновика доступ есть, а отправка запрещена.
    pub draft: Option<(DraftAccess, Option<PlaintextDraft>)>, // Проверка доступа и снимок поля.
}

/// Возвращает проверенную тему с исходным адресом разговора.
type Resolve = Arc<
    dyn Fn(
        ConversationAddress,
        Uuid,
    )
        -> BoxFuture<'static, ConversationReply<Result<ResolvedDestination, DestinationError>>>,
>;

/// Результат проверки цели и записи снимка в очередь.
#[derive(Clone, Copy, Debug)]
pub(in crate::gpui_client) enum QueueWriteOutcome {
    Saved(Uuid),                              // Публичный UUID подготовленного сообщения.
    DestinationUnavailable(DestinationError), // Заново проверяем цель перед повторной отправкой.
    WriteFailed,                              // Запись снимка не подтверждена.
    QuoteUnavailable(SourceCheckError),       // Источники ответа не прошли проверку.
}

/// Возвращает результат записи с адресом разговора, для которого её запустили.
type Save = Arc<dyn Fn(Submission) -> BoxFuture<'static, ConversationReply<QueueWriteOutcome>>>;

/// Запускает операции отправки; в рабочем окне обе выполняет RuntimeClient.
pub(in crate::gpui_client) struct SendSource {
    pub resolve: Resolve, // Проверяет, куда можно отправить обычный текст.
    pub save: Save,       // Сохраняет конкретный снимок отправки в очередь.
}

impl SendSource {
    /// Связывает отправку с тем же исполнителем, на котором работает мессенджер.
    pub fn runtime(runtime: RuntimeClient) -> Self {
        let worker = runtime.clone();
        Self {
            resolve: Arc::new(move |address, op_id| {
                let worker = worker.clone();
                Box::pin(async move {
                    let reply = worker
                        .conversation(address, move |services, address| async move {
                            log::debug!("gpui_send op_id={op_id} event=destination_worker_started");
                            let owner = &address.route.owner;
                            let generation = address.generation;
                            let stream_id = address.route.scope.stream_id;
                            let opening = services.sync.prepare_draft_opening(owner, generation, stream_id);
                            let (topic, closed) = match resolve_destination(&services, &address).await {
                                Ok(topic) => (topic, false),
                                // Этот ответ появляется после проверки владельца, темы и encryption=false.
                                Err(DirectSendError::TopicClosed) if let Some(topic) = address.route.scope.topic_id => (topic, true),
                                Err(error) => return Err(DestinationError::from(error)),
                            };
                            let scope = DraftScope { stream_id, topic_id: topic };
                            services.drafts.wait_for_send(&address, scope).await;
                            let pending = services.drafts.current(&address, scope);
                            let draft = opening.and_then(|opening| services.sync.open_draft(owner, generation, opening, scope));
                            let draft = match draft {
                                Ok((access, saved)) => {
                                    let pending = pending
                                        .filter(|(pending_access, _)| *pending_access == access)
                                        .map(|(_, pending)| pending);
                                    let latest = match (saved, pending) {
                                        (Some(saved), Some(pending)) if saved.id == pending.id && pending.version > saved.version => Some(pending),
                                        (Some(saved), _) => Some(saved),
                                        (None, pending) => pending,
                                    };
                                    Some((access, latest))
                                }
                                Err(error) => {
                                    let stale = matches!(
                                        error,
                                        workspace_core::sync::SyncError::Auth(_)
                                        | workspace_core::sync::SyncError::Storage(workspace_core::storage::StorageError::StaleDraftAccess)
                                    );
                                    if stale {
                                        return Err(DestinationError::Unavailable);
                                    }
                                    log::warn!("gpui_send op_id={op_id} event=draft_read_failed");
                                    return Err(DestinationError::DraftUnavailable);
                                }
                            };
                            Ok(ResolvedDestination { topic, closed, draft })
                        })
                        .await;
                    ConversationReply {
                        address: reply.address,
                        value: reply.value.unwrap_or(Err(DestinationError::Unavailable)),
                    }
                })
            }),
            save: Arc::new(move |submission| {
                let worker = runtime.clone();
                Box::pin(async move {
                    let op_id = submission.attempt.op_id;
                    let draft = submission.draft.clone();
                    let reply = worker
                        .conversation(submission.address, move |services, address| async move {
                            log::debug!("gpui_send op_id={op_id} event=queue_write_started");
                            // Тема могла смениться или закрыться, пока пользователь набирал текст.
                            match resolve_destination(&services, &address).await {
                                Ok(topic) if topic == submission.topic => {}
                                Ok(_) => {
                                    return QueueWriteOutcome::DestinationUnavailable(
                                        DestinationError::TopicChanged,
                                    );
                                }
                                Err(DirectSendError::Source(SourceError::Temporary))
                                    if address.route.scope.topic_id.is_none() && submission.quote_sources.is_empty() =>
                                {
                                    // Личка сохраняет подтверждённую ранее тему. Перед первым POST
                                    // очередь снова проверит её доступность и разрешение на обычный текст.
                                    log::debug!(
                                        "gpui_send op_id={op_id} event=queue_frozen_topic_after_temporary_failure"
                                    );
                                }
                                Err(error) => {
                                    return QueueWriteOutcome::DestinationUnavailable(error.into());
                                }
                            }
                            if let Err(error) = services.messenger.verify_reply_sources(
                                &services.sync,
                                &address.route.owner,
                                address.generation,
                                address.route.scope,
                                &submission.quote_sources,
                            ).await {
                                return QueueWriteOutcome::QuoteUnavailable(error);
                            }
                            services
                                .sync
                                .prepare_send_with_draft(
                                    &address.route.owner,
                                    address.generation,
                                    address.route.scope.stream_id,
                                    submission.topic,
                                    submission.markdown,
                                    submission.draft.as_ref().map(|draft| (&draft.access, draft.version)),
                                )
                                .map(|record| QueueWriteOutcome::Saved(record.public_id))
                                .unwrap_or(QueueWriteOutcome::WriteFailed)
                        })
                        .await;
                    let outcome = reply.value.unwrap_or(QueueWriteOutcome::WriteFailed);
                    if let Some(draft) = draft {
                        draft.finish(matches!(outcome, QueueWriteOutcome::Saved(_)));
                    }
                    let public_id = match outcome {
                        QueueWriteOutcome::Saved(id) => Some(id),
                        _ => None,
                    };
                    log::debug!(
                        "gpui_send op_id={op_id} event=queue_write_finished saved={} message_id={public_id:?} outcome={outcome:?}",
                        public_id.is_some(),
                    );
                    ConversationReply {
                        address: reply.address,
                        value: outcome,
                    }
                })
            }),
        }
    }
}

/// Проверяет серверную цель в контексте разговора и текущего поколения входа.
async fn resolve_destination(
    services: &WorkerServices,
    address: &ConversationAddress,
) -> Result<Uuid, DirectSendError> {
    let scope = address.route.scope;
    let owner = &address.route.owner;
    let generation = address.generation;
    if let Some(topic) = scope.topic_id {
        services
            .messenger
            .check_topic_send(&services.sync, owner, generation, scope.stream_id, topic)
            .await
            .map(|()| topic)
    } else {
        services
            .messenger
            .resolve_direct_send(&services.sync, owner, generation, scope.stream_id)
            .await
    }
}
