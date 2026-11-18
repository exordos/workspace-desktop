//! Показывает сведения о разговоре и загружает их для одного открытия правой панели.

mod source;

#[cfg(all(test, feature = "gpui-test"))]
use gpui::App;
use gpui::{
    Context, Div, Entity, FocusHandle, IntoElement, Render, ScrollHandle, Stateful, Task,
    WeakEntity, Window, div, prelude::*,
};
use uuid::Uuid;
use workspace_core::{directory::ProfilePresence, messenger::details::ConversationDetails};

use super::{Conversation, Failure};
use crate::gpui_client::{
    contracts::ConversationAddress,
    right_panel::{PanelKey, RightPanel, Scope},
    runtime::RuntimeClient,
    ui::{Avatar, Button, Palette, TextRole, ThemeMode, label},
};
use source::Problem;

/// Запрос или результат, который показывает открытый экран сведений.
enum State {
    Loading {
        op_id: Uuid,        // Номер запроса, ответ которого ждёт экран.
        _request: Task<()>, // Закрытие экрана отменяет ожидание ответа.
    },
    Ready(ConversationDetails), // Подтверждённый профиль или сведения темы.
    Failed(Problem),            // Сбой и допустимый следующий шаг.
}

/// Содержимое экрана сведений. Стек держит его живым под другими панелями.
struct DetailsView {
    runtime: RuntimeClient,                 // Рабочий поток для запроса сведений.
    address: ConversationAddress,           // Адрес, зафиксированный при открытии.
    host: WeakEntity<RightPanel>,           // Проверяет, что экран всё ещё находится в стеке.
    conversation: WeakEntity<Conversation>, // Экран для доставки отзыва доступа.
    state: State,                           // Запрос или его результат.
    scroll: ScrollHandle, // Положение прокрутки, пока экран прикрыт другой панелью.
    retry: FocusHandle,   // Кнопка повтора после временного сбоя.
}

/// Связывает шапку разговора со стеком окна.
pub(super) struct Panel {
    host: Entity<RightPanel>,          // Общий стек этого окна.
    key: PanelKey,                     // Сведения текущего открытия разговора.
    runtime: RuntimeClient,            // Рабочий поток для новых открытий.
    trigger: FocusHandle,              // Кнопка сведений в шапке разговора.
    _subscription: gpui::Subscription, // Обновляет подпись кнопки при навигации по стеку.
}

impl Panel {
    /// Подключает шапку к стеку без сетевого запроса.
    pub(super) fn new(
        host: Entity<RightPanel>,
        address: ConversationAddress,
        runtime: RuntimeClient,
        cx: &mut Context<Conversation>,
    ) -> Self {
        let subscription = cx.observe(&host, |_, _, cx| cx.notify());
        Self {
            host,
            key: PanelKey {
                scope: Scope::Conversation(address),
                id: "conversation-details".into(),
            },
            runtime,
            trigger: cx.focus_handle(),
            _subscription: subscription,
        }
    }

    /// Убирает из стека контекстные экраны при сбросе или отзыве доступа.
    pub(super) fn clear(
        &self,
        address: &ConversationAddress,
        fallback: Option<&FocusHandle>,
        cx: &mut Context<Conversation>,
    ) {
        self.host.update(cx, |host, cx| {
            host.invalidate_conversation(address, fallback.unwrap_or(&self.trigger).clone(), cx)
        });
    }

    /// Рисует действие из шапки для мыши, Enter и пробела.
    pub(super) fn trigger(&self, palette: Palette, cx: &Context<Conversation>) -> Stateful<Div> {
        let title = if self.host.read(cx).is_active(&self.key) {
            "Скрыть"
        } else {
            "Сведения"
        };
        let target = cx.entity().downgrade();
        Button::new("conversation-details-trigger", title)
            .render(palette, Some(&self.trigger), move |window, app| {
                let _ = target.update(app, |view, cx| view.toggle_details(window, cx));
            })
            .flex_none()
            .debug_selector(|| "conversation-details-trigger".into())
    }
}

impl Conversation {
    /// Показывает действие в процессе загрузки и после неё, пока доступ не отозван.
    pub(super) fn can_open_details(&self) -> bool {
        !matches!(self.state, super::State::Failed(Failure::AccessLost))
    }

    /// Открывает сведения через стек или снимает их, если они уже сверху.
    fn toggle_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_open_details() {
            return;
        }
        let Some(panel) = &self.details else {
            return;
        };
        let conversation = cx.entity().downgrade();
        let address = self.address.clone();
        panel.host.update(cx, |host, cx| {
            if host.is_active(&panel.key) {
                host.back(window, cx);
            } else {
                host.open(
                    panel.key.clone(),
                    "Сведения",
                    panel.trigger.clone(),
                    |cx| {
                        let host = cx.weak_entity();
                        cx.new(|cx| {
                            DetailsView::new(panel.runtime.clone(), address, conversation, host, cx)
                        })
                    },
                    window,
                    cx,
                );
            }
        });
    }
}

impl DetailsView {
    /// Запускает запрос для нового экрана сведений.
    fn new(
        runtime: RuntimeClient,
        address: ConversationAddress,
        conversation: WeakEntity<Conversation>,
        host: WeakEntity<RightPanel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = Self::request(runtime.clone(), address.clone(), cx);
        Self {
            runtime,
            address,
            conversation,
            host,
            state,
            scroll: ScrollHandle::new(),
            retry: cx.focus_handle(),
        }
    }

    /// Принимает ответ только для живого экрана и его последнего запроса.
    fn request(
        runtime: RuntimeClient,
        address: ConversationAddress,
        cx: &mut Context<Self>,
    ) -> State {
        let op_id = Uuid::new_v4();
        log::debug!("gpui_details op_id={op_id} event=request_started");
        let task = cx.spawn(async move |view, cx| {
            let reply = source::load(runtime, address).await;
            let _ = view.update(cx, |view, cx| {
                if !view
                    .host
                    .upgrade()
                    .is_some_and(|host| host.read(cx).contains_view(cx.entity_id()))
                {
                    log::debug!("gpui_details op_id={op_id} event=closed_screen");
                    return;
                }
                if !matches!(view.state, State::Loading { op_id: current, .. } if current == op_id)
                {
                    log::debug!("gpui_details op_id={op_id} event=stale_request");
                    return;
                }
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    log::debug!("gpui_details op_id={op_id} event=stale_address");
                    return;
                };
                log::debug!(
                    "gpui_details op_id={op_id} event=request_completed success={}",
                    result.is_ok()
                );
                view.state = match result {
                    Ok(details) => State::Ready(details),
                    Err(Problem::AccessLost) => {
                        let _ = view
                            .conversation
                            .update(cx, |chat, cx| chat.fail(Failure::AccessLost, cx));
                        State::Failed(Problem::AccessLost)
                    }
                    Err(error) => State::Failed(error),
                };
                cx.notify();
            });
        });
        State::Loading {
            op_id,
            _request: task,
        }
    }

    /// Показывает повтор только для временной ошибки.
    fn render_problem(&self, problem: Problem, palette: Palette, cx: &Context<Self>) -> Div {
        let message = match problem {
            Problem::AccessLost => "Разговор недоступен",
            Problem::Unsupported => "Сведения для этого разговора недоступны",
            Problem::ProfileUnavailable => "Профиль недоступен",
            Problem::Unavailable => "Не удалось загрузить сведения",
        };
        let target = cx.entity().downgrade();
        div()
            .flex()
            .flex_col()
            .items_start()
            .gap_3()
            .child(label(palette, TextRole::Body, message))
            .when(matches!(problem, Problem::Unavailable), |content| {
                content.child(
                    Button::new("conversation-details-retry", "Повторить")
                        .render(palette, Some(&self.retry), move |window, app| {
                            let _ = target.update(app, |view, cx| {
                                // Кнопка повтора исчезает во время загрузки. Передаём фокус в рамку.
                                if view.retry.is_focused(window) {
                                    window.focus_prev();
                                }
                                view.state =
                                    Self::request(view.runtime.clone(), view.address.clone(), cx);
                                cx.notify();
                            });
                        })
                        .debug_selector(|| "conversation-details-retry".into()),
                )
            })
    }
}

impl Drop for DetailsView {
    /// Отмечает отмену, если стек закрыл экран во время запроса.
    fn drop(&mut self) {
        if let State::Loading { op_id, .. } = self.state {
            log::debug!("gpui_details op_id={op_id} event=cancelled");
        }
    }
}

impl Render for DetailsView {
    /// Показывает текущее состояние сведений внутри общей рамки.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::from(window.appearance()));
        let content = match &self.state {
            State::Loading { .. } => label(palette, TextRole::Body, "Загружаем сведения…")
                .debug_selector(|| "conversation-details-loading".into()),
            State::Ready(details) => render_details(details, palette)
                .debug_selector(|| "conversation-details-ready".into()),
            State::Failed(problem) => self
                .render_problem(*problem, palette, cx)
                .debug_selector(|| "conversation-details-error".into()),
        };
        div()
            .id("conversation-details-content")
            .min_h_0()
            .min_w_0()
            .overflow_x_hidden()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_3()
            .child(content)
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

#[cfg(all(test, feature = "gpui-test"))]
impl Conversation {
    /// Проверяет текущую видимость. GPUI сохраняет старые debug bounds удалённых элементов.
    pub(in crate::gpui_client) fn tested_details_is_open(&self, app: &App) -> bool {
        self.details
            .as_ref()
            .is_some_and(|panel| panel.host.read(app).is_active(&panel.key))
    }

    /// Возвращает результат, из которого построена карточка сведений.
    pub(in crate::gpui_client) fn tested_details<R>(
        &self,
        app: &App,
        inspect: impl FnOnce(Option<&ConversationDetails>) -> R,
    ) -> R {
        let view = self
            .details
            .as_ref()
            .and_then(|panel| panel.host.read(app).tested_view::<DetailsView>(&panel.key));
        let details = view.as_ref().and_then(|view| match &view.read(app).state {
            State::Ready(details) => Some(details),
            _ => None,
        });
        inspect(details)
    }

    /// Возвращает координату строки из текущего кадра для замера открытия панели.
    pub(in crate::gpui_client) fn tested_details_row_y(&self, id: Uuid) -> Option<f32> {
        self.viewport.y(id)
    }

    /// Показывает, какая кнопка сведений сейчас владеет фокусом.
    pub(in crate::gpui_client) fn tested_details_focus(
        &self,
        window: &Window,
        app: &App,
    ) -> Option<&'static str> {
        let panel = self.details.as_ref()?;
        if panel.trigger.is_focused(window) {
            Some("сведения")
        } else if panel.host.read(app).close_focus(&panel.key, window) {
            Some("закрыть")
        } else {
            None
        }
    }
}
