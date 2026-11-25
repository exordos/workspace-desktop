//! Подтверждает удаление под историей и связывает ответ сервера с исходным разговором.

use std::{collections::HashSet, time::Duration};

use gpui::{Context, Div, FocusHandle, KeyDownEvent, Window, div, prelude::*, px};
use uuid::Uuid;
use workspace_core::{
    messenger::message_actions::{DeleteError, MessageTarget, MutationReceipt, MutationStatus},
    storage::{HistoryMessage, HistoryScope},
};

use crate::gpui_client::{
    contracts::ConversationAddress,
    preferences::Language,
    runtime::{RequestError, WorkerServices},
    ui::{Button, ButtonAppearance, Palette, SURFACE_RADIUS, TextRole, label},
};

use super::{Conversation, State};

/// Подтверждение в текущем разговоре и команды, для которых ждём сверку истории.
#[derive(Default)]
pub(super) struct MessageDelete {
    prompt: Option<DeletePrompt>, // Сообщение, для которого сейчас открыта плашка.
    awaiting_history: HashSet<Uuid>, // Запрещает повтор, пока по удалению ждём ответ или событие.
}

/// Фокус и адрес, сохранённые при открытии подтверждения.
struct DeletePrompt {
    target: MessageTarget,     // Оригинальная область, тема и UUID сообщения.
    stage: Stage,              // Состояние команды для этой цели.
    focus: FocusHandle,        // Область подтверждения, чтобы Escape закрывал её.
    cancel_focus: FocusHandle, // Безопасная кнопка для начального фокуса.
}

/// Стадия подтверждения и условия повторной попытки.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Stage {
    Confirm,       // Пользователь ещё не отправил команду.
    Pending(Uuid), // Идентификатор текущего запроса.
    Retry,         // Сервер отказал или команда ещё не была отправлена.
    Unavailable,   // Сообщение или его права изменились.
    Unknown,       // Результат неопределён, ждём сверку с историей.
}

impl Stage {
    /// Разрешает подтверждение и повтор после определённого отказа.
    fn can_submit(self) -> bool {
        matches!(self, Self::Confirm | Self::Retry)
    }

    /// Возвращает подпись без содержимого сетевой ошибки.
    fn description(self) -> &'static str {
        match self {
            Self::Confirm => "message.deleteCannotUndo",
            Self::Pending(_) => "desktop.conversation.delete.pending",
            Self::Retry => "message.deleteError",
            Self::Unavailable => "desktop.conversation.delete.unavailable",
            Self::Unknown => "desktop.conversation.delete.unknown",
        }
    }
}

/// Результат запроса и чтения выданной сервером квитанции.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Applied,     // Сервер подтвердил удаление.
    Rejected,    // Можно повторить с новой проверкой адреса и прав.
    Unavailable, // Серверу не подходит цель или доступ к ней потерян.
    Unknown,     // Сервер мог принять команду, повторять её вслепую нельзя.
}

impl MessageDelete {
    /// Сообщает, перекрывает ли подтверждение часть видимой истории.
    pub(super) fn is_open(&self) -> bool {
        self.prompt.is_some()
    }

    /// Закрывает локальную плашку и сохраняет цель операций, у которых ещё нет результата в истории.
    pub(super) fn clear(&mut self) {
        self.prompt = None;
    }
}

impl Conversation {
    /// Проверяет доступность удаления по снимку. Core проверяет доступ ещё раз перед DELETE.
    pub(super) fn delete_message_enabled(&self, id: Uuid) -> bool {
        self.delete_target(id).is_some()
            && self
                .message_delete
                .prompt
                .as_ref()
                .is_none_or(|prompt| !matches!(prompt.stage, Stage::Pending(_)))
    }

    /// Открывает подтверждение и ставит фокус на кнопку отмены.
    pub(super) fn open_message_delete(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.delete_message_enabled(id) {
            return;
        }
        let Some(target) = self.delete_target(id) else {
            return;
        };
        let cancel_focus = cx.focus_handle();
        window.focus(&cancel_focus);
        self.message_delete.prompt = Some(DeletePrompt {
            target,
            stage: Stage::Confirm,
            focus: cx.focus_handle(),
            cancel_focus,
        });
        cx.notify();
    }

    /// Закрепляет допустимое размещение строки текущего автора в открытой истории.
    fn delete_target(&self, id: Uuid) -> Option<MessageTarget> {
        let (State::Ready(history), Some(_)) = (&self.state, &self.avatar_runtime) else {
            return None;
        };
        let scope = self.address.route.scope;
        if self.is_archived()
            || !self.heading.native_markdown
            || (scope.topic_id.is_none() && !self.heading.direct)
            || self.message_delete.awaiting_history.contains(&id)
        {
            return None;
        }
        let author = self.address.route.owner.user_id.parse().ok()?;
        let row = history.messages.iter().find(|row| row.id == id)?;
        eligible_target(scope, author, row)
    }

    /// Закрывает плашку и возвращает управление истории, если фокус ещё в плашке.
    fn close_message_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(prompt) = self.message_delete.prompt.take() {
            if prompt.focus.contains_focused(window, cx) {
                window.focus(&self.history_focus);
            }
            cx.notify();
        }
    }

    /// Сверяет цель перед отправкой и держит одну команду до завершения.
    fn submit_message_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.message_delete.prompt.as_ref() else {
            return;
        };
        let target = prompt.target;
        if !prompt.stage.can_submit() {
            return;
        }
        let Some(runtime) = self.avatar_runtime.clone() else {
            return;
        };
        let still_available = self.delete_target(target.message_id) == Some(target);
        let Some(prompt) = self.message_delete.prompt.as_mut() else {
            return;
        };
        if !still_available {
            prompt.stage = Stage::Unavailable;
            cx.notify();
            return;
        }

        let op_id = Uuid::new_v4();
        prompt.stage = Stage::Pending(op_id);
        self.message_delete
            .awaiting_history
            .insert(target.message_id);
        window.focus(&prompt.focus);
        cx.notify();
        let address = self.address.clone();
        let window = window.window_handle();
        log::debug!(
            "gpui_history op_id={op_id} event=message_delete_requested message_id={}",
            target.message_id
        );
        cx.spawn(async move |entity, cx| {
            let reply = runtime
                .conversation(address, move |services, address| {
                    delete(services, address, target)
                })
                .await;
            let _ = window.update(cx, |_, window, app| {
                let _ = entity.update(app, |view, cx| {
                    let Some(outcome) = reply.for_current(Some(view.address())) else {
                        return;
                    };
                    let outcome = match outcome {
                        Ok(outcome) => outcome,
                        Err(RequestError::Busy) => Outcome::Rejected,
                        Err(RequestError::Stopped) => Outcome::Unknown,
                    };
                    view.finish_message_delete(target, op_id, outcome, window, cx);
                });
            });
        })
        .detach();
    }

    /// Убирает подтверждение при успехе и сохраняет понятное состояние при сбое.
    fn finish_message_delete(
        &mut self,
        target: MessageTarget,
        op_id: Uuid,
        outcome: Outcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::debug!(
            "gpui_history op_id={op_id} event=message_delete_completed outcome={outcome:?}"
        );
        if matches!(outcome, Outcome::Rejected | Outcome::Unavailable) {
            self.message_delete
                .awaiting_history
                .remove(&target.message_id);
        }
        if matches!(
            outcome,
            Outcome::Applied | Outcome::Unavailable | Outcome::Unknown
        ) {
            // Событие может уже быть в хранилище. Обычное чтение держит позицию списка.
            self.queue_read(cx);
        }
        let Some(prompt) = self.message_delete.prompt.as_mut() else {
            return;
        };
        if prompt.stage != Stage::Pending(op_id) {
            return;
        }
        let stage = match outcome {
            Outcome::Applied => return self.close_message_delete(window, cx),
            Outcome::Rejected => Stage::Retry,
            Outcome::Unavailable => Stage::Unavailable,
            Outcome::Unknown => Stage::Unknown,
        };
        prompt.stage = stage;
        if prompt.focus.contains_focused(window, cx) {
            window.focus(&prompt.cancel_focus);
        }
        cx.notify();
    }

    /// Показывает подтверждение над редактором на языке владельца разговора.
    pub(super) fn render_message_delete(
        &self,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> Option<gpui::Stateful<Div>> {
        let prompt = self.message_delete.prompt.as_ref()?;
        let stage = prompt.stage;
        let pending = matches!(stage, Stage::Pending(_));
        let can_submit = stage.can_submit()
            && self.delete_target(prompt.target.message_id) == Some(prompt.target);
        let mut confirm = Button::new("history-delete-confirm", language.tr("message.delete"));
        confirm.appearance = ButtonAppearance::Primary;
        confirm.disabled = !can_submit;
        let mut cancel = Button::new(
            "history-delete-cancel",
            language.tr(if stage.can_submit() || pending {
                "common.cancel"
            } else {
                "common.close"
            }),
        );
        cancel.disabled = pending;
        let confirm_target = cx.entity().downgrade();
        let cancel_target = confirm_target.clone();
        let description = if stage.can_submit() && !can_submit {
            Stage::Unavailable.description()
        } else {
            stage.description()
        };
        let actions = div()
            .flex()
            .flex_none()
            .gap_2()
            .ml_auto()
            .child(
                confirm
                    .render(
                        Palette {
                            accent: palette.danger,
                            on_accent: if palette.danger.l > 0.6 {
                                palette.on_accent
                            } else {
                                gpui::white()
                            },
                            ..palette
                        },
                        None,
                        move |window, app| {
                            let _ = confirm_target.update(app, |view, cx| {
                                view.submit_message_delete(window, cx);
                            });
                        },
                    )
                    .h(px(36.))
                    .px_4()
                    .rounded(px(8.))
                    .debug_selector(|| "history-delete-confirm".into()),
            )
            .child(
                cancel
                    .render(palette, Some(&prompt.cancel_focus), move |window, app| {
                        let _ = cancel_target.update(app, |view, cx| {
                            view.close_message_delete(window, cx);
                        });
                    })
                    .h(px(36.))
                    .px_4()
                    .rounded(px(8.))
                    .debug_selector(|| "history-delete-cancel".into()),
            );
        Some(
            delete_confirmation(palette, language, description, actions)
                .id("history-delete-confirmation")
                .track_focus(&prompt.focus)
                .on_key_down(cx.listener(move |view, event: &KeyDownEvent, window, cx| {
                    if !pending && event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        view.close_message_delete(window, cx);
                    }
                }))
                .debug_selector(|| "history-delete-confirmation".into()),
        )
    }
}

/// Рисует плашку подтверждения и переносит кнопки в узком окне.
fn delete_confirmation(
    palette: Palette,
    language: Language,
    description: &'static str,
    actions: Div,
) -> Div {
    div()
        .relative()
        .flex()
        .flex_wrap()
        .flex_none()
        .items_center()
        .gap_2()
        .px(px(24.))
        .py(px(10.))
        .min_w_0()
        .rounded_t(SURFACE_RADIUS)
        .border_1()
        .border_color(palette.border)
        .bg(palette.surface)
        .child(
            div()
                .absolute()
                .left_0()
                .top(px(10.))
                .bottom(px(10.))
                .w(px(4.))
                .rounded_r(px(2.))
                .bg(palette.danger),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(192.))
                .max_w_full()
                .gap_1()
                .pl_1()
                .child(label(
                    palette,
                    TextRole::Heading,
                    language.tr("message.deleteConfirm"),
                ))
                .child(
                    label(palette, TextRole::Body, language.tr(description))
                        .text_color(palette.text_muted),
                ),
        )
        .child(actions)
}

/// Проверяет размещение, автора и формат, который поддерживает команда Core.
fn eligible_target(
    scope: HistoryScope,
    author: Uuid,
    row: &HistoryMessage,
) -> Option<MessageTarget> {
    (!row.id.is_nil()
        && row.stream_id == scope.stream_id
        && !row.topic_id.is_nil()
        && scope.topic_id.is_none_or(|topic| topic == row.topic_id)
        && row.metadata.author_id == Some(author)
        && row.body.markdown().is_some())
    .then_some(MessageTarget {
        scope,
        topic_id: row.topic_id,
        message_id: row.id,
    })
}

/// Отправляет единственный DELETE и ограниченно читает выданную сервером квитанцию.
async fn delete(
    services: WorkerServices,
    address: ConversationAddress,
    target: MessageTarget,
) -> Outcome {
    let receipt = services
        .messenger
        .delete_message(
            &services.auth,
            &address.route.owner,
            address.generation,
            target,
        )
        .await;
    match receipt {
        Ok(MutationReceipt::Applied) => Outcome::Applied,
        Ok(MutationReceipt::Accepted(ticket)) => {
            let checks = async {
                loop {
                    match services
                        .messenger
                        .mutation_status(&services.auth, &ticket)
                        .await
                    {
                        Ok(MutationStatus::Applied) => return Outcome::Applied,
                        Ok(MutationStatus::Rejected) => return Outcome::Rejected,
                        Ok(MutationStatus::Pending) => {}
                        Err(_) => return Outcome::Unknown,
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            };
            tokio::time::timeout(Duration::from_secs(8), checks)
                .await
                .unwrap_or(Outcome::Unknown)
        }
        Err(DeleteError::Unknown | DeleteError::Auth(_)) => Outcome::Unknown,
        Err(DeleteError::Rejected(_) | DeleteError::Source(_)) => Outcome::Rejected,
        Err(_) => Outcome::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use workspace_core::storage::{HistoryMetadata, HistoryTime, MessageBody};

    use super::*;

    /// Использует фактическую тему лички и запрещает чужие и защищённые строки.
    #[test]
    fn deletion_target_belongs_to_the_owner_and_open_scope() {
        let author = Uuid::from_u128(1);
        let mut row = HistoryMessage {
            read: None,
            id: Uuid::from_u128(2),
            stream_id: Uuid::from_u128(3),
            topic_id: Uuid::from_u128(4),
            body: MessageBody::Markdown("Текст".into()),
            reactions: Vec::new(),
            metadata: HistoryMetadata {
                native_source: None,
                author_id: Some(author),
                created_at: HistoryTime::parse("2026-01-01T00:00:00Z").unwrap(),
                updated_at: None,
            },
        };
        let direct = HistoryScope {
            stream_id: row.stream_id,
            topic_id: None,
        };
        let topic = HistoryScope {
            topic_id: Some(row.topic_id),
            ..direct
        };
        assert_eq!(
            eligible_target(direct, author, &row).unwrap().topic_id,
            row.topic_id
        );
        assert_eq!(eligible_target(topic, author, &row).unwrap().scope, topic);
        assert!(eligible_target(direct, Uuid::from_u128(5), &row).is_none());

        row.topic_id = Uuid::from_u128(6);
        assert!(eligible_target(topic, author, &row).is_none());

        row.body = MessageBody::Encrypted {
            key_id: Uuid::from_u128(7),
            ciphertext: "test".into(),
        };
        assert!(eligible_target(direct, author, &row).is_none());
    }
}
