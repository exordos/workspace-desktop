//! Открывает адресный диалог и передаёт Core сообщение или выбранный фрагмент для пересылки.

#[path = "forward/targets.rs"]
mod targets;

use gpui::{
    App, Context, FocusHandle, IntoElement, Render, WeakEntity, Window, div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::forward::{
    ForwardOrigin, MAX_FORWARD_DEPTH, can_forward_markdown, contains_protected_snapshot,
};

use super::{Conversation, MessageNavigationSource, State};
use crate::gpui_client::{
    composer::ReplyText,
    contracts::ConversationAddress,
    preferences::{Language, Preferences},
    right_panel::RightPanel,
    runtime::RuntimeClient,
    ui::{Button, ButtonAppearance, Icon, Palette, SURFACE_RADIUS, TextRole, label},
};
use targets::{Picker, Targets};

impl Conversation {
    /// Передаёт UUID оригинала пересылки общему переходу к сообщению.
    pub(super) fn open_forward_original(&mut self, row: Uuid, id: Uuid, cx: &mut Context<Self>) {
        self.emit_message_target(id, MessageNavigationSource::forward(row, id), cx);
    }

    /// Проверяет, что у подтверждённой строки есть поддерживаемое тело и доступен диалог.
    pub(super) fn can_forward_message(&self, id: Uuid) -> bool {
        let State::Ready(history) = &self.state else {
            return false;
        };
        self.details.is_some()
            && self.avatar_runtime.is_some()
            && history
                .messages
                .iter()
                .any(|row| row.id == id && row.body.markdown().is_some_and(can_forward_markdown))
    }

    /// Открывает диалог пересылки из меню, сохраняя исходные ссылки на защищённые фрагменты.
    pub(super) fn open_message_forward(
        &self,
        id: Uuid,
        fragment: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (State::Ready(history), Some(panel), Some(runtime)) =
            (&self.state, &self.details, &self.avatar_runtime)
        else {
            return;
        };
        let Some(markdown) = history
            .messages
            .iter()
            .find(|row| row.id == id)
            .and_then(|row| row.body.markdown())
            .filter(|text| can_forward_markdown(text))
        else {
            return;
        };
        let selection = if contains_protected_snapshot(markdown, MAX_FORWARD_DEPTH - 1) {
            ForwardSelection::Protected
        } else {
            ForwardSelection::Message(fragment)
        };
        let owner = &self.address.route.owner;
        let locale = Preferences::read_for(cx, owner).language;
        let author = self
            .viewport
            .source_author(id, self.reply_peer())
            .and_then(|author| match author {
                ReplyText::Source(text) => Some(text),
                _ => None,
            })
            .unwrap_or_default();
        let original = ForwardOrigin {
            scope: self.address.route.scope,
            message_id: id,
            author,
            chat: self.heading.chat.clone().unwrap_or_default(),
        };
        let targets =
            Targets::from_catalog(panel.catalog_chats(), owner.user_id.parse().ok(), locale);
        let runtime = runtime.clone();
        let address = self.address.clone();
        panel.modal_host().update(cx, |host, cx| {
            let weak_host = cx.entity().downgrade();
            let mut active = None;
            host.open_conversation_modal(
                &self.address,
                self.history_focus.clone(),
                window,
                cx,
                |_, cx| {
                    let dialog = cx.new(move |cx| ForwardDialog {
                        host: weak_host,
                        address,
                        runtime,
                        original,
                        picker: Picker::new(&selection, locale, cx),
                        selection,
                        locale,
                        targets,
                        submit_focus: cx.focus_handle(),
                        pending: false,
                        failed: false,
                    });
                    active = Some(dialog.downgrade());
                    dialog
                },
            );
            if let Some(dialog) = active {
                host.set_modal_dismiss(move |app| {
                    dialog
                        .upgrade()
                        .is_none_or(|dialog| !dialog.read(app).pending)
                });
            }
        });
    }
}

/// Сохраняет способ пересылки до подтверждения получателя.
#[derive(Clone)]
enum ForwardSelection {
    Protected,               // Пересылается полный снимок с исходными защищёнными ссылками.
    Message(Option<String>), // Пересылается всё сообщение или выделенный открытый текст.
}

/// Снимок вариантов, исходный адрес и состояние одной операции пересылки.
struct ForwardDialog {
    host: WeakEntity<RightPanel>, // Возвращает фокус и проверяет актуальность диалога.
    address: ConversationAddress, // Вход и разговор при открытии выбора.
    runtime: RuntimeClient,       // Доступ к Core после выбора цели.
    original: ForwardOrigin,      // Канонический UUID оригинала и подписи.
    selection: ForwardSelection,  // Вид источника и открытое выделение, если оно есть.
    locale: Language,             // Язык интерфейса выбранного входа.
    targets: Targets,             // Доступные цели из того же каталога.
    picker: Picker,               // Вкладка, фильтр и выбор показанной формы.
    submit_focus: FocusHandle,    // После выбора Enter подтверждает адрес.
    pending: bool,                // Команда уже передана исполнителю.
    failed: bool,                 // Core не смог поставить пересылку в очередь.
}

impl ForwardDialog {
    /// Отменяет выбор только до начала отправки.
    fn cancel(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.pending {
            self.close(window, cx);
        }
    }

    /// Закрывает только свой диалог, даже если окно успело открыть другой.
    fn close(&self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.host.update(cx, |host, cx| {
            if host.modal_is::<Self>() {
                host.close_modal(window, cx);
            }
        });
    }

    /// Отправляет явный выбор в Core. Состав тела определяется там по оригиналу.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.picker.selected().filter(|_| !self.pending) else {
            return;
        };
        self.pending = true;
        self.picker.set_enabled(false, cx);
        self.failed = false;
        cx.notify();
        let original = self.original.clone();
        let selection = self.selection.clone();
        let runtime = self.runtime.clone();
        let address = self.address.clone();
        let window = window.window_handle();
        cx.spawn(async move |entity, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
                    match selection {
                        ForwardSelection::Protected => {
                            services
                                .messenger
                                .prepare_protected_forward(
                                    &services.sync,
                                    &address.route.owner,
                                    address.generation,
                                    &original,
                                    target,
                                )
                                .await
                        }
                        ForwardSelection::Message(fragment) => {
                            services
                                .messenger
                                .prepare_message_forward(
                                    &services.sync,
                                    &address.route.owner,
                                    address.generation,
                                    &original,
                                    target,
                                    fragment.as_deref(),
                                )
                                .await
                        }
                    }
                })
                .await;
            let _ = window.update(cx, |_, window, cx| {
                let _ = entity.update(cx, |dialog, cx| {
                    if dialog.address != reply.address {
                        return;
                    }
                    if matches!(reply.value, Ok(Ok(_))) {
                        dialog.close(window, cx);
                    } else {
                        dialog.pending = false;
                        dialog.picker.set_enabled(true, cx);
                        dialog.failed = true;
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    /// Переносит фокус после перестройки элементов выбора, если диалог ещё открыт.
    fn focus_next(focus: FocusHandle, window: &mut Window, cx: &Context<Self>) {
        let dialog = cx.entity().downgrade();
        window.on_next_frame(move |window, _| {
            if dialog.upgrade().is_some() {
                window.focus(&focus);
            }
        });
    }
}

impl Render for ForwardDialog {
    /// Показывает один существующий чат или тему и подтверждение пересылки.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let viewport = window.viewport_size();
        let mut close = Button::new("fragment-forward-close", "");
        close.icon = Some(Icon::Close);
        close.appearance = ButtonAppearance::Ghost;
        close.disabled = self.pending;
        let mut cancel = Button::new("forward-cancel", self.locale.tr("common.cancel"));
        cancel.appearance = ButtonAppearance::Ghost;
        cancel.disabled = self.pending;
        let mut send = Button::new(
            "fragment-forward-submit",
            self.locale.tr(if self.pending {
                "message.forwardSending"
            } else {
                "message.forward"
            }),
        );
        send.appearance = ButtonAppearance::Primary;
        send.disabled = self.picker.selected().is_none() || self.pending;
        div()
            .id("fragment-forward-dialog")
            .debug_selector(|| "fragment-forward-dialog".into())
            .w((viewport.width - px(24.)).clamp(px(0.), px(448.)))
            .when_some(
                self.picker.dialog_height(viewport.height),
                |dialog, height| dialog.h(height),
            )
            .max_h(viewport.height * 0.8)
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(SURFACE_RADIUS)
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(palette.border)
                    .px_4()
                    .py_2()
                    .child(label(
                        palette,
                        TextRole::Label,
                        self.locale.tr(self.picker.title()),
                    ))
                    .child(close.render(
                        palette,
                        None,
                        action(cx, |view, window, cx| view.cancel(window, cx)),
                    )),
            )
            .children(self.picker.tabs(self, palette, cx))
            .child(self.picker.content(self, palette, cx))
            .children(self.failed.then(|| {
                label(
                    palette,
                    TextRole::Caption,
                    self.locale.tr("message.forwardUnavailable"),
                )
                .text_color(palette.danger)
                .bg(palette.danger.opacity(0.1))
                .mx_4()
                .px_3()
                .py_2()
                .rounded(SURFACE_RADIUS)
                .flex_shrink_0()
                .debug_selector(|| "fragment-forward-error".into())
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .flex_shrink_0()
                    .gap_2()
                    .px_4()
                    .pt_2()
                    .pb_4()
                    .child(cancel.render(
                        palette,
                        None,
                        action(cx, |view, window, cx| view.cancel(window, cx)),
                    ))
                    .child(
                        send.render(
                            palette,
                            Some(&self.submit_focus),
                            action(cx, ForwardDialog::submit),
                        )
                        .occlude()
                        .debug_selector(|| "fragment-forward-submit".into()),
                    ),
            )
    }
}

/// Передаёт действие кнопки живому экземпляру диалога.
fn action(
    cx: &Context<ForwardDialog>,
    handle: impl Fn(&mut ForwardDialog, &mut Window, &mut Context<ForwardDialog>) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = cx.entity().downgrade();
    move |window, app| {
        let _ = target.update(app, |view, cx| handle(view, window, cx));
    }
}
