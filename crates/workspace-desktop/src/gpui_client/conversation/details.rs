//! Правая панель разговора. Хранит запрос текущего открытия и рисует его результат.

mod source;

use gpui::{
    App, Context, Div, FocusHandle, KeyBinding, Pixels, Stateful, Task, Window, actions, div,
    prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{directory::ProfilePresence, messenger::details::ConversationDetails};

use super::{Conversation, Failure};
use crate::gpui_client::{
    NextControl, PreviousControl,
    runtime::RuntimeClient,
    ui::{Avatar, Button, ButtonAppearance, Icon, Palette, TextRole, label},
};
use source::Problem;

const WIDTH: Pixels = px(315.);
const SPLIT_FROM: Pixels = px(1050.);
const PANEL_GAP: Pixels = px(4.);
const KEY_CONTEXT: &str = "WorkspaceConversationDetails";

actions!(conversation_details, [Dismiss]);

/// Сведения видны только между открытием панели и закрытием.
enum State {
    Loading {
        op_id: Uuid,        // Номер запроса, который принимается этим открытием панели.
        _request: Task<()>, // Закрытие панели отменяет ожидание ответа.
    },
    Ready(ConversationDetails), // Подтверждённый профиль или сведения темы.
    Failed(Problem),            // Сбой и допустимый следующий шаг.
}

/// Хранит состояние и фокус панели одного экземпляра экрана.
pub(super) struct Panel {
    runtime: RuntimeClient, // Тот же рабочий поток, что читает историю.
    state: Option<State>,   // None, пока панель закрыта.
    region: FocusHandle,    // Проверяет, был ли фокус внутри перед вынужденным закрытием.
    trigger: FocusHandle,   // Кнопка сведений в шапке разговора.
    close: FocusHandle,     // Первый фокус после открытия панели.
    retry: FocusHandle,     // Кнопка повтора после временного сбоя.
}

impl Panel {
    /// Готовит закрытую панель без сетевого запроса.
    pub(super) fn new(runtime: RuntimeClient, cx: &mut Context<Conversation>) -> Self {
        Self {
            runtime,
            state: None,
            region: cx.focus_handle(),
            trigger: cx.focus_handle(),
            close: cx.focus_handle(),
            retry: cx.focus_handle(),
        }
    }

    /// Закрывает панель и отменяет ожидающий запрос.
    pub(super) fn clear(&mut self) {
        if let Some(State::Loading { op_id, .. }) = self.state.as_ref() {
            log::debug!("gpui_details op_id={op_id} event=cancelled");
        }
        self.state = None;
    }

    /// Рисует действие из шапки для мыши, Enter и пробела.
    pub(super) fn trigger(&self, palette: Palette, cx: &Context<Conversation>) -> Stateful<Div> {
        let button = Button::new(
            "conversation-details-trigger",
            if self.state.is_some() {
                "Скрыть"
            } else {
                "Сведения"
            },
        );
        button
            .render(
                palette,
                Some(&self.trigger),
                action(cx, Conversation::toggle_details),
            )
            .flex_none()
            .debug_selector(|| "conversation-details-trigger".into())
    }

    /// Сохраняет клавиатуру в текущем разговоре, если панель исчезла при отзыве или сбросе.
    pub(super) fn return_focus_after_clear(
        &self,
        window: &mut Window,
        cx: &Context<Conversation>,
        fallback: &FocusHandle,
        can_open: bool,
    ) {
        if self.state.is_none() && self.region.contains_focused(window, cx) {
            window.focus(if can_open { &self.trigger } else { fallback });
        }
    }

    /// Добавляет панель справа. В узком окне она перекрывает историю и редактор.
    pub(super) fn render(
        &self,
        palette: Palette,
        width: Pixels,
        cx: &Context<Conversation>,
    ) -> Option<Stateful<Div>> {
        let state = self.state.as_ref()?;
        let overlay = width < SPLIT_FROM;
        let mut close = Button::new("conversation-details-close", "Закрыть");
        close.icon = Some(Icon::Close);
        close.appearance = ButtonAppearance::Ghost;
        let content = match state {
            State::Loading { .. } => label(palette, TextRole::Body, "Загружаем сведения…")
                .debug_selector(|| "conversation-details-loading".into()),
            State::Ready(details) => render_details(details, palette)
                .debug_selector(|| "conversation-details-ready".into()),
            State::Failed(problem) => self
                .render_problem(*problem, palette, cx)
                .debug_selector(|| "conversation-details-error".into()),
        };
        Some(
            div()
                .id("conversation-details-panel")
                .debug_selector(|| "conversation-details-panel".into())
                .track_focus(&self.region)
                .key_context(KEY_CONTEXT)
                .on_action(cx.listener(|view, _: &Dismiss, window, cx| {
                    view.close_details(window, cx);
                }))
                .flex()
                .flex_col()
                .w(WIDTH)
                .min_h_0()
                .flex_none()
                .rounded_md()
                .border_1()
                .border_color(palette.border)
                .bg(palette.surface)
                .when(overlay, |panel| {
                    panel
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .max_w_full()
                        .shadow_md()
                        .occlude()
                        .on_action(cx.listener(|view, _: &NextControl, window, cx| {
                            view.focus_next_detail(window, cx);
                        }))
                        .on_action(cx.listener(|view, _: &PreviousControl, window, cx| {
                            view.focus_next_detail(window, cx);
                        }))
                })
                .when(!overlay, |panel| panel.h_full().ml(PANEL_GAP))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .justify_between()
                        .items_center()
                        .gap_3()
                        .p_3()
                        .child(label(palette, TextRole::Label, "Сведения"))
                        .child(
                            close
                                .render(
                                    palette,
                                    Some(&self.close),
                                    action(cx, Conversation::close_details),
                                )
                                .debug_selector(|| "conversation-details-close".into()),
                        ),
                )
                .child(
                    div()
                        .id("conversation-details-content")
                        .min_h_0()
                        .min_w_0()
                        .overflow_x_hidden()
                        .overflow_y_scroll()
                        .p_3()
                        .child(content),
                ),
        )
    }

    /// Показывает повтор только для временной ошибки.
    fn render_problem(
        &self,
        problem: Problem,
        palette: Palette,
        cx: &Context<Conversation>,
    ) -> Div {
        let message = match problem {
            Problem::AccessLost => "Разговор недоступен",
            Problem::Unsupported => "Сведения для этого разговора недоступны",
            Problem::ProfileUnavailable => "Профиль недоступен",
            Problem::Unavailable => "Не удалось загрузить сведения",
        };
        div()
            .flex()
            .flex_col()
            .items_start()
            .gap_3()
            .child(label(palette, TextRole::Body, message))
            .when(matches!(problem, Problem::Unavailable), |content| {
                content.child(
                    Button::new("conversation-details-retry", "Повторить")
                        .render(
                            palette,
                            Some(&self.retry),
                            action(cx, Conversation::load_details),
                        )
                        .debug_selector(|| "conversation-details-retry".into()),
                )
            })
    }
}

impl Conversation {
    /// Открывает или закрывает панель, пока её шапка относится к рабочему разговору.
    fn toggle_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.details.as_ref().map(|panel| panel.state.is_some()) {
            Some(true) => self.close_details(window, cx),
            Some(false) if self.can_open_details() => self.load_details(window, cx),
            _ => {}
        }
    }

    /// Показывает действие в процессе загрузки и после неё, если доступ не был отозван.
    pub(super) fn can_open_details(&self) -> bool {
        !matches!(self.state, super::State::Failed(Failure::AccessLost))
    }

    /// Открывает панель и начинает новый запрос даже после возврата к тому же адресу.
    fn load_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.details else {
            return;
        };
        let op_id = Uuid::new_v4();
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        window.focus(&panel.close);
        log::debug!("gpui_details op_id={op_id} event=request_started");
        let task = cx.spawn(async move |view, cx| {
            let reply = source::load(runtime, address).await;
            let _ = view.update(cx, |view, cx| {
                let Some(panel) = &mut view.details else {
                    return;
                };
                if !matches!(panel.state, Some(State::Loading { op_id: current, .. }) if current == op_id) {
                    log::debug!("gpui_details op_id={op_id} event=stale_request");
                    return;
                }
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    log::debug!("gpui_details op_id={op_id} event=stale_address");
                    return;
                };
                log::debug!(
                    "gpui_details op_id={op_id} event=request_completed success={}",
                    result.is_ok(),
                );
                match result {
                    Ok(details) => panel.state = Some(State::Ready(details)),
                    Err(Problem::AccessLost) => {
                        panel.state = None;
                        view.fail(Failure::AccessLost, cx);
                    }
                    Err(error) => panel.state = Some(State::Failed(error)),
                }
                cx.notify();
            });
        });
        panel.state = Some(State::Loading {
            op_id,
            _request: task,
        });
        cx.notify();
    }

    /// Закрывает панель по явному действию и возвращает фокус её кнопке.
    fn close_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.details {
            panel.clear();
            window.focus(&panel.trigger);
            cx.notify();
        }
    }

    /// Привязывает Escape только к открытой панели, которая получила фокус.
    pub(in crate::gpui_client) fn bind_details_keys(app: &mut App) {
        app.bind_keys([KeyBinding::new("escape", Dismiss, Some(KEY_CONTEXT))]);
    }

    /// В узком окне оставляет Tab и Shift+Tab на видимых кнопках панели.
    fn focus_next_detail(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &self.details {
            cx.stop_propagation();
            let has_retry = matches!(panel.state, Some(State::Failed(Problem::Unavailable)));
            let next = if has_retry && panel.close.is_focused(window) {
                &panel.retry
            } else {
                &panel.close
            };
            window.focus(next);
        }
    }
}

/// Рисует подтверждённую карточку. Пустые поля профиля и темы пропускаются.
fn render_details(details: &ConversationDetails, palette: Palette) -> Div {
    let card = div().flex().flex_col().gap_2();
    match details {
        ConversationDetails::Peer(profile) => card
            .child(Avatar::from_initials(initials(&profile.display_name)).render(palette))
            .child(
                label(palette, TextRole::Label, profile.display_name.clone())
                    .debug_selector(|| "conversation-details-peer".into()),
            )
            .children(profile.presence.map(|presence| {
                let caption = match presence {
                    ProfilePresence::Active => "В сети",
                    ProfilePresence::Idle => "Отошёл",
                    ProfilePresence::Offline => "Не в сети",
                    ProfilePresence::DoNotDisturb => "Не беспокоить",
                };
                label(palette, TextRole::Caption, caption)
            }))
            .children(
                profile
                    .email
                    .as_ref()
                    .map(|email| label(palette, TextRole::Body, email.clone()).truncate()),
            ),
        ConversationDetails::Topic {
            channel,
            name,
            description,
            done,
        } => card
            .child(
                label(palette, TextRole::Label, name.clone())
                    .debug_selector(|| "conversation-details-topic".into()),
            )
            .child(
                label(palette, TextRole::Caption, channel.clone())
                    .debug_selector(|| "conversation-details-channel".into()),
            )
            .children(
                description
                    .as_ref()
                    .map(|description| label(palette, TextRole::Body, description.clone())),
            )
            .when(*done == Some(true), |card| {
                card.child(label(palette, TextRole::Caption, "Тема завершена"))
            }),
    }
}

/// Берёт две первые буквы непустых частей имени для аватара.
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|part| part.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

/// Передаёт нажатие живому экрану разговора вместе с системным окном.
fn action(
    cx: &Context<Conversation>,
    handle: impl Fn(&mut Conversation, &mut Window, &mut Context<Conversation>) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = cx.entity().downgrade();
    move |window, app| {
        let _ = target.update(app, |view, cx| handle(view, window, cx));
    }
}

#[cfg(all(test, feature = "gpui-test"))]
impl Conversation {
    /// Проверяет текущую видимость. GPUI сохраняет старые debug bounds удалённых элементов.
    pub(in crate::gpui_client) fn tested_details_is_open(&self) -> bool {
        self.details
            .as_ref()
            .is_some_and(|panel| panel.state.is_some())
    }

    /// Возвращает результат, из которого сейчас построена видимая карточка.
    pub(in crate::gpui_client) fn tested_details(&self) -> Option<&ConversationDetails> {
        match self.details.as_ref()?.state.as_ref()? {
            State::Ready(details) => Some(details),
            _ => None,
        }
    }

    /// Возвращает координату строки из текущего кадра для замера открытия панели.
    pub(in crate::gpui_client) fn tested_details_row_y(&self, id: Uuid) -> Option<f32> {
        self.viewport.y(id)
    }

    /// Показывает, какая кнопка сведений сейчас владеет фокусом.
    pub(in crate::gpui_client) fn tested_details_focus(
        &self,
        window: &Window,
    ) -> Option<&'static str> {
        let panel = self.details.as_ref()?;
        if panel.trigger.is_focused(window) {
            Some("сведения")
        } else if panel.close.is_focused(window) {
            Some("закрыть")
        } else if panel.retry.is_focused(window) {
            Some("повторить")
        } else {
            None
        }
    }
}
