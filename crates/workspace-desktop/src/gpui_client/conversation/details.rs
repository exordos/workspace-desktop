//! Управляет открытием, фокусом, загрузкой и действиями правой панели разговора.

mod picker;
mod presentation;
mod source;
mod view;

use std::{sync::Arc, time::Duration};

use gpui::{
    App, ClipboardItem, Context, Div, FocusHandle, KeyBinding, Pixels, Stateful, Task, Window,
    actions, div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::details::{SidebarDetails, sidebar_profile_link};

use super::{Conversation, Failure, OpenDetailsTopic};
use crate::gpui_client::{
    NextControl, PreviousControl,
    contracts::ConversationAddress,
    runtime::RuntimeClient,
    ui::{Button, ButtonAppearance, Icon, Palette, TextRole, label, popup::tooltip},
};
use picker::Picker;
use source::{Mutation, Problem};
use view::PanelAction;

const WIDTH: Pixels = px(315.);
const SPLIT_FROM: Pixels = px(1050.);
const PANEL_GAP: Pixels = px(4.);
const KEY_CONTEXT: &str = "WorkspaceConversationDetails";

actions!(conversation_details, [Dismiss]);

/// Сведения видны только между открытием панели и закрытием.
enum State {
    Loading {
        op_id: Uuid,        // Номер запроса, который принимает это открытие панели.
        _request: Task<()>, // Закрытие отменяет ожидание ответа в окне.
    },
    Ready,           // Есть подтверждённый снимок, который можно показывать.
    Failed(Problem), // Сбой и допустимый следующий шаг.
}

/// Действие, рядом с которым экран показывает ошибку сервера.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MutationKind {
    Notifications, // Изменение режима уведомлений.
    Add,           // Добавление выбранных участников.
    Remove,        // Удаление привязки.
}

/// Изменение, которое выполняется для открытой панели.
struct PendingMutation {
    id: Uuid, // Связывает ответ с запуском из текущего открытия.
}

/// Хранит состояние и фокус панели одного экземпляра экрана.
pub(super) struct Panel {
    runtime: RuntimeClient, // Тот же рабочий поток, что читает историю.
    state: Option<State>,   // None, пока панель закрыта.
    snapshot: Option<Arc<SidebarDetails>>, // Последний снимок до закрытия или сброса.
    reopen_on_access: bool, // Перенос открытой панели при переходе в другую тему канала.
    topics_expanded: bool,  // Показ списка тем в текущей панели.
    share_feedback: Option<Task<()>>, // Галочка после копирования ссылки.
    mutation: Option<PendingMutation>, // Текущее изменение, пока ждём подтверждение сервера.
    mutation_error: Option<MutationKind>, // Последняя ошибка рядом с действием.
    picker: Option<Picker>, // Выбор участников внутри панели владельца.
    refocus_close: bool,    // После успеха или потери прав поле поиска исчезает.
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
            snapshot: None,
            reopen_on_access: false,
            topics_expanded: true,
            share_feedback: None,
            mutation: None,
            mutation_error: None,
            picker: None,
            refocus_close: false,
            region: cx.focus_handle(),
            trigger: cx.focus_handle(),
            close: cx.focus_handle(),
            retry: cx.focus_handle(),
        }
    }

    /// Закрывает панель и отменяет ожидание её запросов в окне.
    pub(super) fn clear(&mut self) {
        if let Some(State::Loading { op_id, .. }) = self.state.as_ref() {
            log::debug!("gpui_details op_id={op_id} event=cancelled");
        }
        self.state = None;
        self.snapshot = None;
        self.reopen_on_access = false;
        self.topics_expanded = true;
        self.share_feedback = None;
        self.mutation = None;
        self.mutation_error = None;
        self.picker = None;
        self.refocus_close = false;
    }

    /// Рисует действие из шапки для мыши, Enter и пробела.
    pub(super) fn trigger(&self, palette: Palette, cx: &Context<Conversation>) -> Stateful<Div> {
        Button::new(
            "conversation-details-trigger",
            if self.state.is_some() {
                "Скрыть"
            } else {
                "Сведения"
            },
        )
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
        address: &ConversationAddress,
        cx: &Context<Conversation>,
    ) -> Option<Stateful<Div>> {
        let state = self.state.as_ref()?;
        let overlay = width < SPLIT_FROM;
        let mut close = Button::new("conversation-details-close", "");
        close.icon = Some(Icon::Close);
        close.appearance = ButtonAppearance::Ghost;
        let title = if self.picker.is_some() {
            "Добавить участников"
        } else if match self.snapshot.as_deref() {
            Some(details) => matches!(details, SidebarDetails::Channel(_)),
            None => address.route.scope.topic_id.is_some(),
        } {
            "Информация о канале"
        } else {
            "Информация"
        };
        let content = if let (Some(picker), Some(SidebarDetails::Channel(channel))) =
            (&self.picker, self.snapshot.as_deref())
        {
            picker.content(
                palette,
                cx,
                channel,
                self.mutation.is_some() || !matches!(self.state, Some(State::Ready)),
                self.mutation_error == Some(MutationKind::Add),
            )
        } else if let Some(details) = self.snapshot.as_deref() {
            let data = presentation::content(self, details, address);
            let target = cx.entity().downgrade();
            view::render(&data, palette, move |intent, window, app| {
                let _ = target.update(app, |screen, cx| {
                    screen.handle_panel_action(intent, window, cx)
                });
            })
            .debug_selector(|| "conversation-details-ready".into())
        } else {
            match state {
                State::Loading { .. } | State::Ready => {
                    label(palette, TextRole::Body, "Загружаем сведения…")
                        .debug_selector(|| "conversation-details-loading".into())
                }
                State::Failed(problem) => self
                    .render_problem(*problem, palette, cx)
                    .debug_selector(|| "conversation-details-error".into()),
            }
        };
        Some(
            div()
                .id("conversation-details-panel")
                .debug_selector(|| "conversation-details-panel".into())
                .track_focus(&self.region)
                .key_context(KEY_CONTEXT)
                .on_action(cx.listener(|view, _: &Dismiss, window, cx| {
                    view.dismiss_details(window, cx);
                }))
                .flex()
                .flex_col()
                .w(WIDTH)
                .min_h_0()
                .flex_none()
                .rounded(px(12.))
                .bg(palette.surface)
                .pt_2()
                .pb_5()
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
                            view.step_detail_focus(false, window, cx);
                        }))
                        .on_action(cx.listener(|view, _: &PreviousControl, window, cx| {
                            view.step_detail_focus(true, window, cx);
                        }))
                })
                .when(!overlay, |panel| panel.h_full().ml(PANEL_GAP))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .justify_between()
                        .items_center()
                        .h(px(40.))
                        .mx_2()
                        .px_2()
                        .child(label(palette, TextRole::Label, title))
                        .child(
                            close
                                .render(
                                    palette,
                                    Some(&self.close),
                                    action(cx, Conversation::dismiss_details),
                                )
                                .debug_selector(|| "conversation-details-close".into())
                                .tooltip(move |_, app| tooltip(palette, "Закрыть".into(), app)),
                        ),
                )
                .child(
                    div()
                        .id("conversation-details-content")
                        .min_h_0()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .overflow_hidden()
                        .px_2()
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
            .p_2()
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
    /// Открывает или закрывает панель из шапки рабочего разговора.
    fn toggle_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.details.as_ref().map(|panel| panel.state.is_some()) {
            Some(true) => self.close_details(window, cx),
            Some(false) if self.can_open_details() => self.load_details(window, cx),
            _ => {}
        }
    }

    /// Показывает кнопку только в разговоре, к которому подтверждён доступ.
    pub(super) fn can_open_details(&self) -> bool {
        self.has_access()
    }

    /// Начинает загрузку по действию пользователя и ставит фокус на закрытие.
    fn load_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &self.details {
            window.focus(&panel.close);
            self.request_details(cx);
        }
    }

    /// После перехода ждёт подтверждения доступа к выбранной теме.
    pub(in crate::gpui_client) fn restore_open_details(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.details {
            panel.reopen_on_access = true;
            cx.notify();
        }
    }

    /// Открывает перенесённую панель после подтверждения истории темы.
    pub(super) fn restore_details_for_verified_history(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_access()
            && self
                .details
                .as_mut()
                .is_some_and(|panel| std::mem::take(&mut panel.reopen_on_access))
        {
            self.load_details(window, cx);
        }
    }

    /// Возвращает фокус в панель после закрытия выбора по результату сервера.
    pub(super) fn restore_details_focus(&mut self, window: &mut Window) {
        if let Some(panel) = &mut self.details
            && std::mem::take(&mut panel.refocus_close)
        {
            window.focus(&panel.close);
        }
    }

    /// Обновляет открытый снимок после события изменений того же стрима.
    pub(super) fn refresh_open_details(&mut self, cx: &mut Context<Self>) {
        if self
            .details
            .as_ref()
            .is_some_and(|panel| panel.state.is_some())
        {
            self.request_details(cx);
        }
    }

    /// Читает снимок из core, сверяя адрес и последнее намерение перед показом.
    fn request_details(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.details else {
            return;
        };
        let op_id = Uuid::new_v4();
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
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
                log::debug!("gpui_details op_id={op_id} event=request_completed success={}", result.is_ok());
                match result {
                    Ok(details) => {
                        if !matches!(details.as_ref(), SidebarDetails::Channel(channel) if channel.can_add_members) {
                            panel.refocus_close |= panel.picker.take().is_some();
                        }
                        panel.snapshot = Some(details);
                        panel.state = Some(State::Ready);
                    }
                    Err(Problem::AccessLost) => view.fail(Failure::AccessLost, cx),
                    Err(error) => {
                        panel.snapshot = None;
                        panel.refocus_close |= panel.picker.take().is_some();
                        panel.state = Some(State::Failed(error));
                    }
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

    /// Закрывает выбор участников или саму панель.
    fn dismiss_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.details
            && panel.picker.take().is_some()
        {
            window.focus(&panel.close);
            cx.notify();
        } else {
            self.close_details(window, cx);
        }
    }

    /// Закрывает панель по явному действию и возвращает фокус её кнопке.
    fn close_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.details {
            panel.clear();
            window.focus(&panel.trigger);
            cx.notify();
        }
    }

    /// Выполняет доступное действие только по снимку этого разговора.
    fn handle_panel_action(
        &mut self,
        intent: PanelAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.details else {
            return;
        };
        match (intent, panel.snapshot.as_deref()) {
            (PanelAction::MessagePeer, Some(SidebarDetails::Peer(_))) => {
                if let Some(sending) = &self.sending {
                    panel.clear();
                    sending.view.read(cx).focus_editor(window, cx);
                    cx.notify();
                }
            }
            (PanelAction::Copy(value), Some(SidebarDetails::Peer(_))) => {
                cx.write_to_clipboard(ClipboardItem::new_string(value));
            }
            (PanelAction::SharePeer, Some(SidebarDetails::Peer(peer))) => {
                let Ok(link) = sidebar_profile_link(&self.address.route.owner, peer.user_id) else {
                    return;
                };
                cx.write_to_clipboard(ClipboardItem::new_string(link));
                let feedback = cx.background_executor().timer(Duration::from_secs(2));
                panel.share_feedback = Some(cx.spawn(async move |view, cx| {
                    feedback.await;
                    let _ = view.update(cx, |view, cx| {
                        if let Some(panel) = &mut view.details {
                            panel.share_feedback = None;
                            cx.notify();
                        }
                    });
                }));
                cx.notify();
            }
            (PanelAction::ToggleTopics, Some(SidebarDetails::Channel(_))) => {
                panel.topics_expanded = !panel.topics_expanded;
                cx.notify();
            }
            (PanelAction::OpenTopic(topic_id), Some(SidebarDetails::Channel(channel)))
                if channel.topics.iter().any(|topic| topic.id == topic_id) =>
            {
                cx.emit(OpenDetailsTopic {
                    address: self.address.clone(),
                    topic_id,
                });
            }
            (PanelAction::SetNotifications(level), Some(SidebarDetails::Channel(channel)))
                if channel.notification_mode != presentation::stream_mode(level) =>
            {
                self.start_sidebar_mutation(
                    Mutation::Notifications(presentation::stream_mode(level)),
                    cx,
                );
            }
            (PanelAction::AddMembers, Some(SidebarDetails::Channel(channel)))
                if channel.can_add_members =>
            {
                self.open_member_picker(window, cx);
            }
            (PanelAction::RemoveMember(binding_id), Some(SidebarDetails::Channel(channel)))
                if channel
                    .members
                    .iter()
                    .any(|member| member.binding_id == binding_id && member.can_remove) =>
            {
                self.start_sidebar_mutation(Mutation::RemoveMember(binding_id), cx);
            }
            _ => {}
        }
    }

    /// Запускает одно изменение и принимает результат только в текущем открытии панели.
    fn start_sidebar_mutation(&mut self, mutation: Mutation, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.details else {
            return;
        };
        if panel.mutation.is_some() || !matches!(panel.state, Some(State::Ready)) {
            return;
        }
        let kind = match &mutation {
            Mutation::Notifications(_) => MutationKind::Notifications,
            Mutation::AddMembers(_) => MutationKind::Add,
            Mutation::RemoveMember(_) => MutationKind::Remove,
        };
        let id = Uuid::new_v4();
        let address = self.address.clone();
        let runtime = panel.runtime.clone();
        panel.mutation = Some(PendingMutation { id });
        panel.mutation_error = None;
        log::debug!("gpui_details op_id={id} event=mutation_started");
        cx.spawn(async move |view, cx| {
            let reply = source::mutate(runtime, address, mutation).await;
            let _ = view.update(cx, |view, cx| {
                let Some(panel) = &mut view.details else {
                    return;
                };
                if !panel
                    .mutation
                    .as_ref()
                    .is_some_and(|current| current.id == id)
                {
                    return;
                }
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                panel.mutation = None;
                log::debug!(
                    "gpui_details op_id={id} event=mutation_completed success={}",
                    result.is_ok()
                );
                match result {
                    Ok(()) => {
                        if kind == MutationKind::Add {
                            panel.refocus_close |= panel.picker.take().is_some();
                        }
                        cx.notify();
                    }
                    Err(Problem::AccessLost) => view.fail(Failure::AccessLost, cx),
                    Err(_) => {
                        panel.mutation_error = Some(kind);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Привязывает Escape только к открытой панели, которая получила фокус.
    pub(in crate::gpui_client) fn bind_details_keys(app: &mut App) {
        app.bind_keys([KeyBinding::new("escape", Dismiss, Some(KEY_CONTEXT))]);
    }

    /// В узком окне направляет Tab к доступным элементам панели.
    fn step_detail_focus(&self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &self.details else {
            return;
        };
        cx.stop_propagation();
        if reverse {
            window.focus_prev();
        } else {
            window.focus_next();
        }
        if !panel.region.contains_focused(window, cx) {
            window.focus(&panel.close);
        }
    }
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

    /// Возвращает снимок, из которого сейчас построена видимая карточка.
    pub(in crate::gpui_client) fn tested_details(&self) -> Option<&SidebarDetails> {
        self.details.as_ref()?.snapshot.as_deref()
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
