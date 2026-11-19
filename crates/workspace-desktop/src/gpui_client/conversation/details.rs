//! Управляет открытием, фокусом, загрузкой и действиями правой панели разговора.

mod picker;
mod presentation;
mod source;
mod view;

use std::{sync::Arc, time::Duration};

use gpui::{
    App, ClipboardItem, Context, Div, Entity, EntityId, FocusHandle, IntoElement, Render, Stateful,
    Task, WeakEntity, Window, div, prelude::*,
};
use uuid::Uuid;
use workspace_core::messenger::details::{SidebarDetails, sidebar_profile_link};

use super::{Conversation, Failure, OpenDetailsTopic};
use crate::gpui_client::{
    avatars::AvatarImages,
    contracts::{AccountSession, ConversationAddress},
    right_panel::{PanelKey, RightPanel, Scope},
    runtime::RuntimeClient,
    ui::{Button, Palette, TextRole, ThemeMode, label},
};
use picker::Picker;
use source::{Mutation, Problem};
use view::PanelAction;

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
    host: Entity<RightPanel>,              // Общий стек правых панелей окна.
    key: PanelKey,                         // Ключ текущего открытия разговора.
    body: Option<EntityId>,                // Идентификатор живого содержимого в стеке.
    _host_changes: gpui::Subscription,     // Сбрасывает данные после закрытия через общую рамку.
    runtime: RuntimeClient,                // Тот же рабочий поток, что читает историю.
    state: Option<State>,                  // None, пока панель закрыта.
    avatars: AvatarImages,                 // Подписки панели на общие фото по UUID человека.
    snapshot: Option<Arc<SidebarDetails>>, // Последний снимок до закрытия или сброса.
    reopen_on_access: bool, // Перенос открытой панели при переходе в другую тему канала.
    topics_expanded: bool,  // Показ списка тем в текущей панели.
    share_feedback: Option<Task<()>>, // Галочка после копирования ссылки.
    mutation: Option<PendingMutation>, // Текущее изменение, пока ждём подтверждение сервера.
    mutation_error: Option<MutationKind>, // Последняя ошибка рядом с действием.
    picker: Option<Picker>, // Выбор участников внутри панели владельца.
    refocus_close: bool,    // После успеха или потери прав поле поиска исчезает.
    trigger: FocusHandle,   // Кнопка сведений в шапке разговора.
    retry: FocusHandle,     // Кнопка повтора после временного сбоя.
}

impl Panel {
    /// Готовит закрытую панель без сетевого запроса.
    pub(super) fn new(
        host: Entity<RightPanel>,
        address: ConversationAddress,
        runtime: RuntimeClient,
        cx: &mut Context<Conversation>,
    ) -> Self {
        let host_changes = cx.observe(&host, |view, host, cx| {
            if let Some(panel) = view.details.as_mut()
                && panel
                    .body
                    .is_some_and(|id| !host.read(cx).contains_view(id))
            {
                panel.reset();
            }
            cx.notify();
        });
        Self {
            host,
            key: PanelKey {
                scope: Scope::Conversation(address),
                id: "conversation-details".into(),
            },
            body: None,
            _host_changes: host_changes,
            runtime,
            state: None,
            avatars: AvatarImages::default(),
            snapshot: None,
            reopen_on_access: false,
            topics_expanded: true,
            share_feedback: None,
            mutation: None,
            mutation_error: None,
            picker: None,
            refocus_close: false,
            trigger: cx.focus_handle(),
            retry: cx.focus_handle(),
        }
    }

    /// Убирает контекстные экраны из стека при сбросе или отзыве доступа.
    pub(super) fn clear(
        &mut self,
        address: &ConversationAddress,
        fallback: Option<&FocusHandle>,
        cx: &mut Context<Conversation>,
    ) {
        self.reset();
        self.host.update(cx, |host, cx| {
            host.invalidate_conversation(address, fallback.unwrap_or(&self.trigger).clone(), cx)
        });
    }

    /// Проверяет, жив ли экран, в том числе пока он прикрыт другим экраном.
    fn is_present(&self, app: &App) -> bool {
        self.body
            .is_some_and(|id| self.host.read(app).contains_view(id))
    }

    /// Закрывает панель и отменяет ожидание её запросов в окне.
    fn reset(&mut self) {
        self.body = None;
        if let Some(State::Loading { op_id, .. }) = self.state.as_ref() {
            log::debug!("gpui_details op_id={op_id} event=cancelled");
        }
        self.state = None;
        self.snapshot = None;
        self.avatars.clear();
        self.reopen_on_access = false;
        self.topics_expanded = true;
        self.share_feedback = None;
        self.mutation = None;
        self.mutation_error = None;
        self.picker = None;
        self.refocus_close = false;
    }

    /// Оставляет подписки собеседника или участников текущего канала.
    fn refresh_avatars(
        &mut self,
        details: &SidebarDetails,
        address: &ConversationAddress,
        cx: &mut Context<Conversation>,
    ) {
        let users = match details {
            SidebarDetails::Peer(peer) => vec![peer.user_id],
            SidebarDetails::Channel(channel) => channel
                .members
                .iter()
                .map(|member| member.user_id)
                .collect(),
        };
        let session = AccountSession {
            owner: address.route.owner.clone(),
            generation: address.generation,
        };
        self.avatars.refresh(users, &self.runtime, &session, cx);
    }

    /// Рисует действие из шапки для мыши, Enter и пробела.
    pub(super) fn trigger(&self, palette: Palette, cx: &Context<Conversation>) -> Stateful<Div> {
        Button::new(
            "conversation-details-trigger",
            if self.host.read(cx).is_active(&self.key) {
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

    /// Рисует содержимое внутри общей рамки стека.
    fn render_content(
        &self,
        palette: Palette,
        address: &ConversationAddress,
        cx: &Context<Conversation>,
    ) -> Div {
        let Some(state) = self.state.as_ref() else {
            return div();
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
            let data = presentation::content(self, details, address, cx);
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
        div()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .flex_1()
            .overflow_hidden()
            .px_2()
            .child(content)
    }

    /// Перерисовывает заголовок общей рамки после смены содержимого.
    fn refresh_frame(&self, cx: &mut Context<Conversation>) {
        self.host.update(cx, |_, cx| cx.notify());
    }

    /// Выбирает заголовок видимого представления.
    fn title(&self, address: &ConversationAddress) -> &'static str {
        if self.picker.is_some() {
            "Добавить участников"
        } else if match self.snapshot.as_deref() {
            Some(details) => matches!(details, SidebarDetails::Channel(_)),
            None => address.route.scope.topic_id.is_some(),
        } {
            "Информация о канале"
        } else {
            "Информация"
        }
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
                            action(cx, |view, window, cx| {
                                if let Some(panel) = &view.details {
                                    panel.host.read(cx).focus_close(&panel.key, window);
                                }
                                view.request_details(cx);
                            }),
                        )
                        .debug_selector(|| "conversation-details-retry".into()),
                )
            })
    }
}

/// Содержимое общей рамки, связанное с живым разговором.
struct DetailsBody {
    conversation: WeakEntity<Conversation>, // Закрывается вместе с исходным маршрутом.
    _changes: gpui::Subscription,           // Перерисовывает содержимое после обновления данных.
}

impl Render for DetailsBody {
    /// Перерисовывает содержимое и возвращает фокус в рамку после исчезновения выбора.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::from(window.appearance()));
        self.conversation
            .update(cx, |view, cx| {
                let Some(panel) = view.details.as_mut() else {
                    return div();
                };
                if std::mem::take(&mut panel.refocus_close) {
                    panel.host.read(cx).focus_close(&panel.key, window);
                }
                panel.render_content(palette, &view.address, cx)
            })
            .unwrap_or_else(|_| div())
    }
}

impl Conversation {
    /// Открывает сведения через общую рамку или закрывает экран, если он сверху.
    fn toggle_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_open_details() {
            return;
        }
        let Some(panel) = &mut self.details else {
            return;
        };
        if panel.host.read(cx).is_active(&panel.key) {
            panel.reset();
            panel.host.update(cx, |host, cx| host.pop(window, cx));
        } else {
            self.load_details(window, cx);
        }
    }

    /// Показывает кнопку только в разговоре, к которому подтверждён доступ.
    pub(super) fn can_open_details(&self) -> bool {
        self.has_access()
    }

    /// Добавляет содержимое в стек и начинает загрузку только для нового открытия.
    fn load_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.details else {
            return;
        };
        let conversation = cx.entity();
        let mut opened = None;
        panel.host.update(cx, |host, cx| {
            host.open(
                panel.key.clone(),
                panel.title(&self.address),
                panel.trigger.clone(),
                |cx| {
                    let body = cx.new(|cx| DetailsBody {
                        conversation: conversation.downgrade(),
                        _changes: cx.observe(&conversation, |_, _, cx| cx.notify()),
                    });
                    opened = Some(body.entity_id());
                    body
                },
                window,
                cx,
            );
            if opened.is_some() {
                let title = conversation.downgrade();
                let back = conversation.downgrade();
                host.configure_conversation(
                    &panel.key,
                    move |app| {
                        title
                            .upgrade()
                            .and_then(|conversation| {
                                let view = conversation.read(app);
                                view.details
                                    .as_ref()
                                    .map(|panel| panel.title(&view.address))
                            })
                            .unwrap_or("Информация")
                    },
                    move |app| {
                        back.update(app, |view, cx| {
                            let cancelled = view
                                .details
                                .as_mut()
                                .is_some_and(|panel| panel.picker.take().is_some());
                            if cancelled {
                                cx.notify();
                            }
                            cancelled
                        })
                        .unwrap_or(false)
                    },
                );
            }
        });
        if let Some(id) = opened {
            panel.body = Some(id);
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

    /// Обновляет открытый снимок после события изменений того же стрима.
    pub(super) fn refresh_open_details(&mut self, cx: &mut Context<Self>) {
        if self
            .details
            .as_ref()
            .is_some_and(|panel| panel.is_present(cx))
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
                if !panel.is_present(cx) || !matches!(panel.state, Some(State::Loading { op_id: current, .. }) if current == op_id) {
                    log::debug!("gpui_details op_id={op_id} event=stale_request");
                    return;
                }
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    log::debug!("gpui_details op_id={op_id} event=stale_address");
                    return;
                };
                log::debug!("gpui_details op_id={op_id} event=request_completed success={}", result.is_ok());
                let previous_title = panel.title(&view.address);
                match result {
                    Ok(details) => {
                        if !matches!(details.as_ref(), SidebarDetails::Channel(channel) if channel.can_add_members) {
                            panel.refocus_close |= panel.picker.take().is_some();
                        }
                        panel.refresh_avatars(&details, &view.address, cx);
                        panel.snapshot = Some(details);
                        panel.state = Some(State::Ready);
                    }
                    Err(Problem::AccessLost) => view.fail(Failure::AccessLost, cx),
                    Err(error) => {
                        panel.snapshot = None;
                        panel.avatars.clear();
                        panel.refocus_close |= panel.picker.take().is_some();
                        panel.state = Some(State::Failed(error));
                    }
                }
                if let Some(panel) = view.details.as_ref()
                    && panel.is_present(cx)
                    && panel.title(&view.address) != previous_title
                {
                    panel.refresh_frame(cx);
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
            panel.host.read(cx).focus_close(&panel.key, window);
            panel.refresh_frame(cx);
            cx.notify();
        } else {
            self.close_details(window, cx);
        }
    }

    /// Закрывает панель по явному действию и возвращает фокус её кнопке.
    fn close_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.details {
            panel.reset();
            panel.host.update(cx, |host, cx| host.pop(window, cx));
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
        if !panel.host.read(cx).is_active(&panel.key) {
            return;
        }
        match (intent, panel.snapshot.as_deref()) {
            (PanelAction::MessagePeer, Some(SidebarDetails::Peer(_))) => {
                if let Some(sending) = &self.sending {
                    panel.reset();
                    panel.host.update(cx, |host, cx| host.pop(window, cx));
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
                if let Some(panel) = &self.details
                    && panel.picker.is_some()
                {
                    panel.refresh_frame(cx);
                }
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
                if !panel.is_present(cx)
                    || !panel
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
                        if kind == MutationKind::Add && panel.picker.take().is_some() {
                            panel.refocus_close = true;
                            panel.refresh_frame(cx);
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
    pub(in crate::gpui_client) fn tested_details_is_open(&self, app: &App) -> bool {
        self.details
            .as_ref()
            .is_some_and(|panel| panel.host.read(app).is_active(&panel.key))
    }

    /// Возвращает снимок, из которого сейчас построена видимая карточка.
    pub(in crate::gpui_client) fn tested_details<R>(
        &self,
        app: &App,
        inspect: impl FnOnce(Option<&SidebarDetails>) -> R,
    ) -> R {
        let details = self.details.as_ref().filter(|panel| panel.is_present(app));
        inspect(details.and_then(|panel| panel.snapshot.as_deref()))
    }

    /// Возвращает идентификатор изображения для проверки замены фото после события.
    pub(in crate::gpui_client) fn tested_details_avatar(&self, app: &App) -> Option<gpui::ImageId> {
        self.tested_details(app, |details| {
            let Some(SidebarDetails::Peer(peer)) = details else {
                return None;
            };
            let gpui::ImageSource::Render(image) =
                self.details.as_ref()?.avatars.image(peer.user_id, app)?
            else {
                return None;
            };
            Some(image.id)
        })
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
        } else if panel.retry.is_focused(window) {
            Some("повторить")
        } else {
            None
        }
    }
}
