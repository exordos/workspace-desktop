//! Открывает адресный диалог пересылки и отправляет команду Core для защищённого оригинала.

use gpui::{
    App, Context, EventEmitter, FocusHandle, IntoElement, Render, SharedString, WeakEntity, Window,
    div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        catalog::Chat,
        forward::{MAX_FORWARD_DEPTH, ProtectedForwardOrigin, contains_protected_snapshot},
    },
    storage::HistoryScope,
};

use super::{Conversation, State};
use crate::gpui_client::{
    composer::ReplyText,
    contracts::ConversationAddress,
    preferences::{Language, Preferences},
    right_panel::RightPanel,
    runtime::RuntimeClient,
    ui::{Button, ButtonAppearance, Icon, Palette, SURFACE_RADIUS, TextRole, label},
};

/// Адрес уже существующей обычной нативной темы или личного чата.
struct Destination {
    scope: HistoryScope, // Канал и тема или личный чат с темой по умолчанию.
    label: SharedString, // Подпись каталога для выбора.
}

/// Переход к одному оригиналу, авторитетно проверенный в выбранной сессии.
pub(in crate::gpui_client) struct OpenForwardOriginal {
    pub address: ConversationAddress, // Точный экран, на котором нажали ссылку.
    pub scope: HistoryScope,          // Фактический чат и тема, проверенные сервером.
    pub id: Uuid,                     // Публичный UUID сообщения для открытия истории вокруг него.
}

impl EventEmitter<OpenForwardOriginal> for Conversation {}

impl Conversation {
    /// Открывает только последнюю выбранную ссылку, всё ещё видимую в прежней истории.
    pub(super) fn open_forward_original(&mut self, row: Uuid, id: Uuid, cx: &mut Context<Self>) {
        let Some(runtime) = self
            .avatar_runtime
            .clone()
            .filter(|_| self.viewport.has_forward_source(row, id))
        else {
            return;
        };
        let address = self.address.clone();
        self.forward_original = Some(cx.spawn(async move |entity, cx| {
            let result = runtime
                .conversation(address, move |services, address| async move {
                    services
                        .messenger
                        .resolve_forward_original(
                            &services.sync,
                            &address.route.owner,
                            address.generation,
                            id,
                        )
                        .await
                })
                .await;
            let _ = entity.update(cx, |view, cx| {
                view.forward_original = None;
                let Ok(Ok(scope)) = result.value else {
                    return;
                };
                if view.address == result.address
                    && view.has_access()
                    && view.viewport.has_forward_source(row, id)
                {
                    cx.emit(OpenForwardOriginal {
                        address: result.address,
                        scope,
                        id,
                    });
                }
            });
        }));
    }

    /// Открывает выбор для подтверждённого сообщения со ссылкой на защищённый фрагмент.
    pub(super) fn open_fragment_forward(
        &self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (State::Ready(history), Some(panel), Some(runtime)) =
            (&self.state, &self.details, &self.avatar_runtime)
        else {
            return;
        };
        let Some(row) = history.messages.iter().find(|row| row.id == id) else {
            return;
        };
        if !row
            .body
            .markdown()
            .is_some_and(|text| contains_protected_snapshot(text, MAX_FORWARD_DEPTH - 1))
        {
            return;
        }
        let owner = &self.address.route.owner;
        let locale = Preferences::read_for(cx, owner).language;
        let author = self
            .viewport
            .source_author(id, self.reply_peer())
            .and_then(|author| match author {
                ReplyText::Source(text) => Some(text),
                ReplyText::Label(_) | ReplyText::Mixed(_) => None,
            })
            .unwrap_or_default();
        let original = ProtectedForwardOrigin {
            scope: self.address.route.scope,
            message_id: id,
            author,
            chat: self.heading.chat.clone().unwrap_or_default(),
        };
        let options = destinations(panel.catalog_chats());
        let runtime = runtime.clone();
        let address = self.address.clone();
        panel.modal_host().update(cx, |host, cx| {
            let weak_host = cx.entity().downgrade();
            host.open_conversation_modal(
                &self.address,
                self.history_focus.clone(),
                window,
                cx,
                move |_, cx| {
                    cx.new(move |cx| ForwardDialog {
                        host: weak_host,
                        address,
                        runtime,
                        original,
                        locale,
                        options,
                        submit_focus: cx.focus_handle(),
                        selected: None,
                        pending: false,
                        failed: false,
                    })
                },
            );
        });
    }
}

/// Снимок вариантов, исходный адрес и состояние одной операции пересылки.
struct ForwardDialog {
    host: WeakEntity<RightPanel>, // Возвращает фокус и проверяет актуальность диалога.
    address: ConversationAddress, // Вход и разговор при открытии выбора.
    runtime: RuntimeClient,       // Доступ к Core после выбора цели.
    original: ProtectedForwardOrigin, // Канонический UUID оригинала и подписи.
    locale: Language,             // Язык интерфейса выбранного входа.
    options: Vec<Destination>,    // Доступные цели из того же каталога.
    submit_focus: FocusHandle,    // После выбора Enter подтверждает адрес.
    selected: Option<usize>,      // Индекс текущей цели.
    pending: bool,                // Команда уже передана исполнителю.
    failed: bool,                 // Core не смог поставить пересылку в очередь.
}

impl ForwardDialog {
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
        let Some(target) = self
            .selected
            .and_then(|index| self.options.get(index))
            .map(|option| option.scope)
            .filter(|_| !self.pending)
        else {
            return;
        };
        self.pending = true;
        self.failed = false;
        cx.notify();
        let original = self.original.clone();
        let runtime = self.runtime.clone();
        let address = self.address.clone();
        let window = window.window_handle();
        cx.spawn(async move |entity, cx| {
            let reply = runtime
                .conversation(address, move |services, address| async move {
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
                        dialog.failed = true;
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    /// Отображает группы выбора в порядке, в котором их возвращает каталог разговоров.
    fn destination_list(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("fragment-forward-targets")
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        for (index, destination) in self.options.iter().enumerate() {
            let selector = SharedString::from(format!(
                "fragment-forward-target-{}",
                destination
                    .scope
                    .topic_id
                    .unwrap_or(destination.scope.stream_id)
            ));
            let mut choice = Button::new(selector.clone(), destination.label.clone());
            choice.appearance = ButtonAppearance::Ghost;
            choice.disabled = self.pending;
            list = list.child(
                choice
                    .render(
                        palette,
                        None,
                        action(cx, move |view, window, cx| {
                            view.selected = Some(index);
                            view.failed = false;
                            let submit = view.submit_focus.clone();
                            let dialog = cx.entity().downgrade();
                            window.on_next_frame(move |window, _| {
                                if dialog.upgrade().is_some() {
                                    window.focus(&submit);
                                }
                            });
                            cx.notify();
                        }),
                    )
                    .when(self.selected == Some(index), |choice| {
                        choice.bg(palette.card_selected)
                    })
                    .debug_selector({
                        let chosen = self.selected == Some(index);
                        move || {
                            if chosen {
                                format!("{selector}-selected")
                            } else {
                                selector.to_string()
                            }
                        }
                    }),
            );
        }
        if self.options.is_empty() {
            list = list.child(label(
                palette,
                TextRole::Body,
                self.locale.tr("message.forwardNoDestinations"),
            ));
        }
        list
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
        let mut send = Button::new("fragment-forward-submit", self.locale.tr("message.forward"));
        send.appearance = ButtonAppearance::Primary;
        send.disabled = self.selected.is_none() || self.pending;
        div()
            .id("fragment-forward-dialog")
            .debug_selector(|| "fragment-forward-dialog".into())
            .w((viewport.width - px(24.)).clamp(px(0.), px(448.)))
            .h(viewport.height * 0.7)
            .max_h(px(500.))
            .min_h_0()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded(SURFACE_RADIUS)
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(label(
                        palette,
                        TextRole::Label,
                        self.locale.tr("message.forwardDestination"),
                    ))
                    .child(close.render(
                        palette,
                        None,
                        action(cx, |view, window, cx| view.close(window, cx)),
                    )),
            )
            .child(self.destination_list(palette, cx))
            .children(self.failed.then(|| {
                label(
                    palette,
                    TextRole::Caption,
                    self.locale.tr("message.forwardUnavailable"),
                )
                .debug_selector(|| "fragment-forward-error".into())
            }))
            .child(
                div().flex().justify_end().child(
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

/// Оставляет обычные нативные лички и открытые темы каналов.
fn destinations(chats: &[Chat]) -> Vec<Destination> {
    let mut destinations = Vec::new();
    for chat in chats.iter().filter(|chat| chat.metadata.native_markdown) {
        if chat.direct {
            destinations.push(Destination {
                scope: HistoryScope {
                    stream_id: chat.id,
                    topic_id: None,
                },
                label: chat.name.clone().into(),
            });
            continue;
        }
        destinations.extend(
            chat.topics
                .iter()
                .filter(|topic| !topic.metadata.is_done)
                .map(|topic| Destination {
                    scope: HistoryScope {
                        stream_id: chat.id,
                        topic_id: Some(topic.id),
                    },
                    label: format!("{} · {}", chat.name, topic.name).into(),
                }),
        );
    }
    destinations
}
