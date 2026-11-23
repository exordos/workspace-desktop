//! Действия истории над ключами. Решение о доступе и обмен защищённым материалом остаются в Core.

use std::{collections::HashMap, sync::Arc};

use gpui::Context;
use uuid::Uuid;
use workspace_core::{
    messenger::encryption::key_exchange::{
        KeyExchange, KeyExchangeError, KeyRequestCompletion, KeyRequestOutcome, KeyRequestTarget,
    },
    storage::{HistoryMessage, HistoryWindow, KeyServicePayload, MessageBody},
};

use super::{Conversation, State, encryption::EncryptedPresentation};
use crate::gpui_client::runtime::IncomingKey;

/// Намерение пользователя для текущей строки истории.
#[derive(Clone, Copy)]
pub(super) enum KeyAction {
    Request, // Запрашивает отсутствующее поколение ключа.
    Recover, // Просит заменить ключ, который не открыл этот конверт.
    Grant,   // Выдаёт поколение устройству из служебной строки.
    Reject,  // Отказывает этому устройству.
}

/// Короткий итог действия возле строки, откуда оно началось.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum KeyActionState {
    Running(Uuid),                // Идентификатор вызова, с которым сверяем завершение.
    Waiting(Uuid), // Публичный UUID размещённого запроса, пока ожидаем проверенную квитанцию.
    Rejected,      // Core подтвердил отказ устройства, можно создать новый запрос.
    ReplySent(KeyRequestOutcome), // Сервер подтвердил выдачу или отказ на этот точный запрос.
    Unavailable,   // Роль, устройство или локальный ключ не разрешают команду.
    Failed,        // Действие можно повторить с тем же точным размещением.
}

/// Привязка видимого действия к точному исходному телу и теме.
pub(super) struct KeyActionRecord {
    body: MessageBody, // Только шифротекст или публичные параметры служебного запроса.
    target: KeyRequestTarget, // Точное размещение и поколение, с которыми запустили действие.
    pub(super) state: KeyActionState, // Состояние до подтверждения ответа либо изменения исходной строки.
}

/// Временные действия для видимого окна, без секретов или отдельной записи на диск.
pub(super) type KeyActions = HashMap<Uuid, KeyActionRecord>;

/// Действия, права на которые Core проверил для текущего участника и поколения.
#[derive(Clone, Copy, Default)]
pub(super) struct KeyPermission {
    pub grant: bool,  // Устройство может выдать точный ключ.
    pub reject: bool, // Участник может отклонить точный запрос.
}

/// Показывает права только после ответа для этой версии служебной строки.
pub(super) struct CheckedKeyRequest {
    pub target: KeyRequestTarget, // Оригинальная область запроса и его поколение.
    replies: Vec<HistoryMessage>, // Версии видимых ответов. Изменение требует проверки Core.
    pub operation: Uuid,          // Сверка с ответом после отмены и смены ключа.
    pub permission: Option<KeyPermission>, // None пока запрос прав не подтверждён.
}

impl KeyActionRecord {
    /// После изменения текста или темы прежнее действие больше не относится к этой строке.
    pub(super) fn still_visible(&self, id: Uuid, window: &HistoryWindow) -> bool {
        window.messages.iter().any(|message| {
            message.id == id
                && message.topic_id == self.target.topic_id
                && message.body == self.body
        })
    }

    /// Проверяет поколение, для которого сбросили локальное состояние ключа.
    pub(super) fn uses_key(&self, key_id: Option<Uuid>) -> bool {
        matches!(self.body, MessageBody::Encrypted { .. })
            && key_id.is_none_or(|key_id| key_id == self.target.key_id)
    }

    /// Применяет только квитанцию Core об ожидающем запросе этого же размещения.
    pub(super) fn received(
        &mut self,
        target: KeyRequestTarget,
        completion: KeyRequestCompletion,
    ) -> bool {
        if self.target != target || self.state != KeyActionState::Waiting(completion.request_id) {
            return false;
        }
        self.state = match completion.outcome {
            KeyRequestOutcome::Rejected => KeyActionState::Rejected,
            KeyRequestOutcome::Granted => return false,
        };
        true
    }
}

impl Conversation {
    /// Проверяет права один раз для каждого открытого запроса в текущем снимке истории.
    pub(super) fn check_key_reply_permissions(&mut self, cx: &mut Context<Self>) {
        let (State::Ready(history), Some(encryption), Some(runtime)) =
            (&self.state, &mut self.encryption, &self.avatar_runtime)
        else {
            return;
        };
        let device = encryption.activation.session().device_id.to_string();
        let mut replies = HashMap::<_, Vec<_>>::new();
        for row in &history.messages {
            if let MessageBody::KeyService {
                payload: KeyServicePayload::Grant { target, .. } | KeyServicePayload::Reject(target),
                ..
            } = &row.body
            {
                replies
                    .entry((
                        row.stream_id,
                        row.topic_id,
                        target.key_uuid,
                        target.request_message_uuid,
                        target.recipient_device_id.as_str(),
                    ))
                    .or_default()
                    .push(row.clone());
            }
        }
        let relevant: HashMap<_, _> = history
            .messages
            .iter()
            .filter_map(|row| match &row.body {
                MessageBody::KeyService {
                    payload:
                        KeyServicePayload::Request {
                            device_id,
                            key_uuid,
                            ..
                        },
                    ..
                } if *device_id != device => Some((
                    row.id,
                    (
                        KeyRequestTarget {
                            stream_id: row.stream_id,
                            topic_id: row.topic_id,
                            key_id: *key_uuid,
                        },
                        replies
                            .remove(&(
                                row.stream_id,
                                row.topic_id,
                                *key_uuid,
                                row.id,
                                device_id.as_str(),
                            ))
                            .unwrap_or_default(),
                    ),
                )),
                _ => None,
            })
            .collect();
        encryption.key_permissions.retain(|id, entry| {
            let current = relevant.get(id).is_some_and(|(target, replies)| {
                target == &entry.target && replies == &entry.replies
            });
            if !current {
                self.viewport.invalidate_row_height(*id);
            }
            current
        });
        for (id, (target, replies)) in relevant {
            if encryption.key_permissions.contains_key(&id) {
                continue;
            }
            let operation = Uuid::new_v4();
            encryption.key_permissions.insert(
                id,
                CheckedKeyRequest {
                    target,
                    replies,
                    operation,
                    permission: None,
                },
            );
            let activation = encryption.activation.clone();
            let engine = encryption.engine.clone();
            let runtime = runtime.clone();
            let address = self.address.clone();
            let background = cx.background_executor().clone();
            cx.spawn(async move |view, cx| {
                let mut delay = std::time::Duration::from_secs(1);
                loop {
                    let active = view
                        .read_with(cx, |view, _| {
                            view.address == address
                                && view.encryption.as_ref().is_some_and(|encryption| {
                                    encryption
                                        .key_permissions
                                        .get(&id)
                                        .is_some_and(|entry| entry.operation == operation)
                                })
                        })
                        .unwrap_or(false);
                    if !active {
                        break;
                    }
                    let captured = activation.clone();
                    let engine = engine.clone();
                    let reply = runtime
                        .conversation(address.clone(), move |services, _| async move {
                            KeyExchange::new(services.messenger, services.sync, engine)
                                .key_reply_capabilities(&captured, target, id)
                                .await
                        })
                        .await;
                    let permission = match reply.value {
                        Ok(Ok(value)) => KeyPermission {
                            grant: value.can_grant,
                            reject: value.can_reject,
                        },
                        Ok(Err(ref error))
                            if crate::gpui_client::runtime::key_exchange_is_permanent(error) =>
                        {
                            KeyPermission::default()
                        }
                        _ => {
                            background.timer(delay).await;
                            delay = (delay * 2).min(std::time::Duration::from_secs(30));
                            continue;
                        }
                    };
                    let _ = view.update(cx, |view, cx| {
                        let Some(entry) = view
                            .encryption
                            .as_mut()
                            .and_then(|encryption| encryption.key_permissions.get_mut(&id))
                            .filter(|entry| entry.operation == operation)
                        else {
                            return;
                        };
                        entry.permission = Some(permission);
                        if permission.grant || permission.reject {
                            view.viewport.invalidate_row_height(id);
                            cx.notify();
                        }
                    });
                    break;
                }
            })
            .detach();
        }
    }

    /// Проверяет ответы из REST и SQLite так же, как realtime, включая восстановление после пропуска.
    pub(super) fn reconcile_key_responses(&self, history: &HistoryWindow, cx: &mut Context<Self>) {
        let (Some(runtime), Some(encryption)) = (&self.avatar_runtime, &self.encryption) else {
            return;
        };
        let activation = encryption.activation.clone();
        let replies: Vec<_> = history
            .messages
            .iter()
            .filter_map(IncomingKey::message)
            .collect();
        if replies.is_empty() {
            return;
        }
        let runtime = runtime.clone();
        let background = cx.background_executor().clone();
        for reply in replies {
            let runtime = runtime.clone();
            let activation = activation.clone();
            let background = background.clone();
            cx.spawn(async move |view, cx| {
                if let Some((target, receipt)) = runtime
                    .apply_incoming_key(activation.clone(), reply, background.clone())
                    .await
                {
                    let _ = view.update(cx, |view, cx| {
                        view.key_request_completed(&activation, target, receipt, cx)
                    });
                }
            })
            .detach();
        }
    }

    /// Повторно привязывает кнопку к текущей строке и передаёт точное намерение в Core.
    pub(super) fn key_action(&mut self, id: Uuid, action: KeyAction, cx: &mut Context<Self>) {
        let (State::Ready(history), Some(encryption), Some(runtime)) =
            (&self.state, &mut self.encryption, &self.avatar_runtime)
        else {
            return;
        };
        if encryption.key_actions.get(&id).is_some_and(|record| {
            matches!(
                record.state,
                KeyActionState::Running(_)
                    | KeyActionState::Waiting(_)
                    | KeyActionState::ReplySent(_)
            )
        }) || encryption.message_change_pending(id)
        {
            return;
        }
        let Some(row) = history.messages.iter().find(|row| row.id == id) else {
            return;
        };
        let (key_id, ciphertext) = match (&row.body, action) {
            (MessageBody::Encrypted { key_id, .. }, KeyAction::Request)
                if !matches!(
                    encryption.presentation(id),
                    Some(EncryptedPresentation::Ready(_))
                ) =>
            {
                (*key_id, None)
            }
            (MessageBody::Encrypted { key_id, ciphertext }, KeyAction::Recover)
                if matches!(
                    encryption.presentation(id),
                    Some(EncryptedPresentation::Failed)
                ) =>
            {
                (*key_id, Some(ciphertext.clone()))
            }
            (
                MessageBody::KeyService {
                    payload:
                        KeyServicePayload::Request {
                            key_uuid,
                            device_id,
                            ..
                        },
                    ..
                },
                KeyAction::Grant | KeyAction::Reject,
            ) if *device_id != encryption.activation.session().device_id.to_string() => {
                let permission = encryption
                    .key_permissions
                    .get(&id)
                    .and_then(|entry| entry.permission);
                if !permission.is_some_and(|permission| match action {
                    KeyAction::Grant => permission.grant,
                    KeyAction::Reject => permission.reject,
                    _ => false,
                }) {
                    return;
                }
                (*key_uuid, None)
            }
            _ => return,
        };
        let target = KeyRequestTarget {
            stream_id: row.stream_id,
            topic_id: row.topic_id,
            key_id,
        };
        let engine = Arc::clone(&encryption.engine);
        let activation = encryption.activation.clone();
        let runtime = runtime.clone();
        let address = self.address.clone();
        let op_id = Uuid::new_v4();
        encryption.key_actions.insert(
            id,
            KeyActionRecord {
                body: row.body.clone(),
                target,
                state: KeyActionState::Running(op_id),
            },
        );
        self.viewport.invalidate_row_height(id);
        log::debug!("event=gpui_key_action_started op_id={op_id} message_id={id}");
        let captured = activation.clone();
        cx.spawn(async move |view, cx| {
            let reply = runtime
                .conversation(address, move |services, _| async move {
                    let exchange = KeyExchange::new(services.messenger, services.sync, engine);
                    let device_name = "Workspace Desktop (Mac)";
                    match action {
                        KeyAction::Request => exchange
                            .request_key(&activation, target, device_name)
                            .await
                            .map(|receipt| KeyActionState::Waiting(receipt.request_id)),
                        KeyAction::Recover => {
                            let ciphertext = ciphertext
                                .as_deref()
                                .ok_or(KeyExchangeError::InvalidInput)?;
                            exchange
                                .request_recovery(&activation, target, id, ciphertext, device_name)
                                .await
                                .map(|receipt| KeyActionState::Waiting(receipt.request_id))
                        }
                        KeyAction::Grant | KeyAction::Reject => {
                            let outcome = if matches!(action, KeyAction::Grant) {
                                KeyRequestOutcome::Granted
                            } else {
                                KeyRequestOutcome::Rejected
                            };
                            exchange
                                .respond_to_key_request(&activation, target, id, outcome)
                                .await
                                .map(|_| KeyActionState::ReplySent(outcome))
                        }
                    }
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                let Some(outcome) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                let Some(encryption) = &mut view.encryption else {
                    return;
                };
                let Some(record) = encryption
                    .key_actions
                    .get_mut(&id)
                    .filter(|record| record.state == KeyActionState::Running(op_id))
                else {
                    return;
                };
                let state = match outcome {
                    Ok(Ok(state)) => state,
                    Ok(Err(KeyExchangeError::Forbidden | KeyExchangeError::MissingKey)) => {
                        KeyActionState::Unavailable
                    }
                    failure => {
                        log::debug!(
                            "event=gpui_key_action_failed op_id={op_id} cause={}",
                            match &failure {
                                Ok(Err(KeyExchangeError::InvalidResponse)) => "invalid_response",
                                Ok(Err(KeyExchangeError::Access(_))) => "access",
                                Ok(Err(KeyExchangeError::Send(_))) => "request",
                                Ok(Err(KeyExchangeError::Store(_))) => "store",
                                Ok(Err(KeyExchangeError::Grant(_))) => "grant",
                                _ => "unavailable",
                            },
                        );
                        KeyActionState::Failed
                    }
                };
                record.state = state;
                if let KeyActionState::Waiting(request) = state
                    && let Some(runtime) = &view.avatar_runtime
                    && let Some(receipt) =
                        runtime.key_request_completion(&captured, target, request)
                {
                    record.received(target, receipt);
                }
                log::debug!(
                    "event=gpui_key_action_finished op_id={op_id} ok={}",
                    matches!(
                        state,
                        KeyActionState::Waiting(_) | KeyActionState::ReplySent(_)
                    )
                );
                view.viewport.invalidate_row_height(id);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Обновляет только строки, где ожидали подтверждённый Core отказ для этого запроса.
    pub(in crate::gpui_client) fn key_request_completed(
        &mut self,
        activation: &workspace_core::messenger::encryption::EncryptionActivation,
        target: KeyRequestTarget,
        receipt: KeyRequestCompletion,
        cx: &mut Context<Self>,
    ) {
        let Some(encryption) = &mut self.encryption else {
            return;
        };
        if *activation != encryption.activation {
            return;
        }
        let mut changed = false;
        for (id, record) in &mut encryption.key_actions {
            if record.received(target, receipt) {
                self.viewport.invalidate_row_height(*id);
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
        for id in self.viewport.fragment_views.receive(target, receipt, cx) {
            self.viewport.invalidate_row_height(id);
        }
        self.key_management_receipt(target, receipt, cx);
    }
}

#[cfg(all(test, feature = "gpui-test"))]
impl Conversation {
    /// Возвращает состояние точного запроса для проверки отмены и повторного действия через окно.
    pub(in crate::gpui_client) fn tested_key_request_status(
        &self,
        id: Uuid,
    ) -> Option<&'static str> {
        match self.encryption.as_ref()?.key_actions.get(&id)?.state {
            KeyActionState::Waiting(_) => Some("waiting"),
            KeyActionState::Rejected => Some("rejected"),
            _ => None,
        }
    }
}
