//! Обращается к core с рабочего потока и возвращает только безопасный итог для редактора.

use std::sync::Arc;

use futures::future::BoxFuture;
use uuid::Uuid;
use workspace_core::{
    messenger::{
        encrypted_send::EncryptedMessageError,
        encryption::{EncryptionActivation, EncryptionError, fragments::FragmentMessageError},
        message_actions::MessageTarget,
        quote::SourceCheckError,
        send::{DirectSendError, PlainSendTarget, SendError},
    },
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
    KeyUnavailable,        // Защищённая запись требует локального ключа точного чата.
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

impl From<EncryptedMessageError> for DestinationError {
    /// Оставляет только состояние, которое можно показать пользователю защищённого чата.
    fn from(error: EncryptedMessageError) -> Self {
        match error {
            EncryptedMessageError::Destination(error) => error.into(),
            EncryptedMessageError::Key(EncryptionError::MissingKey) => Self::KeyUnavailable,
            EncryptedMessageError::TopicChanged => Self::TopicChanged,
            _ => Self::Unavailable,
        }
    }
}

/// Проверенная цель отправки. Локальный открытый черновик есть только у обычной темы.
pub(in crate::gpui_client) struct ResolvedDestination {
    pub topic: Uuid,                           // Проверенная тема отправки.
    pub encrypted: bool,                       // Нужен конверт E2EE и ввод только в памяти.
    pub closed: bool, // Для чтения черновика доступ есть, а отправка запрещена.
    pub plain_target: Option<PlainSendTarget>, // Происхождение Markdown, подтверждённое сервером.
    pub restored_protected: bool, // Черновик пришёл из защищённого буфера этого входа.
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
    Saved(Uuid), // Публичный UUID подготовленного сообщения.
    Edited,      // Сервер подтвердил правку прежнего сообщения.
    StaleEdit,   // Прежняя версия стала недоступна.
    EditFailed {
        uncertain: bool, // Сервер мог принять правку и начать использовать новые вложения.
    },
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
    pub fn runtime(
        runtime: RuntimeClient,
        draft_id: Option<Uuid>,
        activation: Option<EncryptionActivation>,
    ) -> Self {
        let worker = runtime.clone();
        let resolve_activation = activation.clone();
        Self {
            resolve: Arc::new(move |address, op_id| {
                let worker = worker.clone();
                let activation = resolve_activation.clone();
                Box::pin(async move {
                    let reply = worker
                        .conversation(address, move |services, address| async move {
                            log::debug!("gpui_send op_id={op_id} event=destination_worker_started");
                            let owner = &address.route.owner;
                            let generation = address.generation;
                            let stream_id = address.route.scope.stream_id;
                            let opening = services.sync.prepare_draft_opening(owner, generation, stream_id);
                            let (topic, plain_target, closed) = match resolve_destination(&services, &address).await {
                                Ok(target) => (target.topic_id(), Some(target), false),
                                // Этот ответ появляется после проверки владельца, темы и encryption=false.
                                Err(DirectSendError::TopicClosed) if let Some(topic) = address.route.scope.topic_id => (topic, None, true),
                                Err(DirectSendError::EncryptionUnavailable) if draft_id.is_none() && let Some(activation) = activation.as_ref() => {
                                    let target = services.messenger.encrypted_send_key(&services.sync, activation, address.route.scope).await?;
                                    return Ok(ResolvedDestination { topic: target.topic_id, encrypted: true, closed: false, plain_target: None, restored_protected: false, draft: None });
                                }
                                Err(error) => return Err(DestinationError::from(error)),
                            };
                            let scope = DraftScope { stream_id, topic_id: topic };
                            services.drafts.wait_for_send(&address, scope).await;
                            let pending = services.drafts.current(&address, scope, draft_id);
                            let draft = opening.and_then(|opening| {
                                services.sync.open_draft_with_id(owner, generation, opening, scope, draft_id)
                            });
                            let (draft, restored_protected) = match draft {
                                Ok((access, mut saved)) => {
                                    let pending = if let Some(id) = draft_id && saved.is_none()
                                        && pending.as_ref().is_none_or(|(pending_access, pending, _)| *pending_access != access || pending.id != id)
                                    {
                                        // Сервер читаем только для точного UUID после проверки темы и обычного текста.
                                        saved = services.messenger.open_server_draft(
                                            &services.sync, owner, generation, &access, id,
                                        ).await.map_err(|_| DestinationError::DraftUnavailable)?;
                                        if saved.is_none() {
                                            return Err(DestinationError::DraftUnavailable);
                                        }
                                        services.drafts.current(&address, scope, draft_id).or(pending)
                                    } else {
                                        pending
                                    };
                                    let pending = pending
                                        .filter(|(pending_access, pending, _)| {
                                            *pending_access == access
                                                && draft_id.is_none_or(|id| pending.id == id)
                                        })
                                        .map(|(_, pending, protected)| (pending, protected));
                                    let (latest, protected) = match (saved, pending) {
                                        (Some(saved), Some((pending, protected))) if saved.id == pending.id && pending.version > saved.version => (Some(pending), protected),
                                        (Some(saved), Some((pending, protected))) if draft_id.is_none() && saved.id != pending.id => (Some(pending), protected),
                                        (Some(saved), _) => (Some(saved), false),
                                        (None, Some((pending, protected))) => (Some(pending), protected),
                                        (None, None) => (None, false),
                                    };
                                    (Some((access, latest)), protected)
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
                            Ok(ResolvedDestination { topic, encrypted: false, closed, plain_target, restored_protected, draft })
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
                let activation = activation.clone();
                Box::pin(async move {
                    let op_id = submission.attempt.op_id;
                    let draft = submission.draft.clone();
                    let editing = submission.edit.is_some();
                    let reply = worker
                        .conversation(submission.address, move |services, address| async move {
                            log::debug!("gpui_send op_id={op_id} event=queue_write_started");
                            if let Some(edit) = submission.fragment_edit.as_ref() {
                                return match services.messenger.edit_fragment_message(
                                    &services.sync, edit, submission.markdown,
                                ).await {
                                    Ok(_) => QueueWriteOutcome::Edited,
                                    Err(FragmentMessageError::StaleEdit) =>
                                        QueueWriteOutcome::StaleEdit,
                                    Err(error) => QueueWriteOutcome::EditFailed {
                                        uncertain: !matches!(error, FragmentMessageError::Edit(SendError::Rejected(_) | SendError::InvalidInput)),
                                    },
                                };
                            }
                            if submission.encrypted {
                                let Some(activation) = activation.as_ref() else {
                                    return QueueWriteOutcome::DestinationUnavailable(DestinationError::EncryptionUnavailable);
                                };
                                if let Some(expected) = submission.edit {
                                    let target = MessageTarget {
                                        scope: address.route.scope,
                                        topic_id: expected.topic_id,
                                        message_id: expected.id,
                                    };
                                    return match services.messenger.edit_encrypted_message(
                                        &services.sync, activation, target, &expected, submission.markdown,
                                    ).await {
                                        Ok(_) => QueueWriteOutcome::Edited,
                                        Err(EncryptedMessageError::StaleEdit | EncryptedMessageError::NotEditable) => QueueWriteOutcome::StaleEdit,
                                        Err(error) => QueueWriteOutcome::EditFailed {
                                            uncertain: !matches!(error, EncryptedMessageError::Transport(SendError::Rejected(_) | SendError::InvalidInput)),
                                        },
                                    };
                                }
                                if let Err(error) = services.messenger.verify_protected_reply_sources(
                                    &services.sync, activation, address.route.scope, &submission.protected_sources,
                                ).await {
                                    return QueueWriteOutcome::QuoteUnavailable(error);
                                }
                                let protected = services.messenger.prepare_encrypted_send_to_expected(
                                    &services.sync, activation, address.route.scope, submission.topic, submission.markdown,
                                ).await;
                                return match protected {
                                    Ok(record) => QueueWriteOutcome::Saved(record.public_id),
                                    Err(EncryptedMessageError::Queue(_)) => QueueWriteOutcome::WriteFailed,
                                    Err(error) => QueueWriteOutcome::DestinationUnavailable(error.into()),
                                };
                            }
                            // Тема могла смениться или закрыться, пока пользователь набирал текст.
                            let plain_target = match resolve_destination(&services, &address).await {
                                Ok(target) if target.topic_id() == submission.topic => Some(target),
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
                                    None
                                }
                                Err(error) => {
                                    return QueueWriteOutcome::DestinationUnavailable(error.into());
                                }
                            };
                            if let Err(error) = services.messenger.verify_reply_sources(
                                &services.sync,
                                &address.route.owner,
                                address.generation,
                                address.route.scope,
                                &submission.quote_sources,
                            ).await {
                                return QueueWriteOutcome::QuoteUnavailable(error);
                            }
                            if let Some(new_key) = submission.fragment_new_key {
                                let Some(activation) = activation.as_ref() else {
                                    return QueueWriteOutcome::DestinationUnavailable(DestinationError::EncryptionUnavailable);
                                };
                                if submission.draft.as_ref().is_some_and(|draft| !draft.flush_protection(&services.sync)) {
                                    return QueueWriteOutcome::WriteFailed;
                                }
                                let prepared = services.messenger.prepare_fragment_send(
                                    &services.sync, activation, address.route.scope, submission.topic, submission.markdown, new_key,
                                ).await;
                                return match prepared {
                                    Ok(record) => QueueWriteOutcome::Saved(record.public_id),
                                    Err(workspace_core::messenger::encryption::fragments::FragmentMessageError::Destination(error)) =>
                                        QueueWriteOutcome::DestinationUnavailable(error.into()),
                                    Err(workspace_core::messenger::encryption::fragments::FragmentMessageError::TopicChanged) =>
                                        QueueWriteOutcome::DestinationUnavailable(DestinationError::TopicChanged),
                                    Err(_) => QueueWriteOutcome::WriteFailed,
                                };
                            }
                            if workspace_core::messenger::encryption::fragments::has_protected_draft(&submission.markdown) {
                                let Some(target) = plain_target.as_ref().filter(|target| target.literal_markers_allowed()) else {
                                    return QueueWriteOutcome::WriteFailed;
                                };
                                return services.sync.prepare_external_send(target, submission.markdown,
                                    submission.draft.as_ref().map(|draft| (&draft.access, draft.version)))
                                    .map(|record| QueueWriteOutcome::Saved(record.public_id))
                                    .unwrap_or(QueueWriteOutcome::WriteFailed);
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
                    let outcome = reply.value.unwrap_or(if editing {
                        QueueWriteOutcome::EditFailed { uncertain: true }
                    } else {
                        QueueWriteOutcome::WriteFailed
                    });
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
) -> Result<PlainSendTarget, DirectSendError> {
    services
        .messenger
        .resolve_plain_send(
            &services.sync,
            &address.route.owner,
            address.generation,
            address.route.scope,
        )
        .await
}
