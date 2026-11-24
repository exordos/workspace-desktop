//! Открывает меню темы и диалог переименования. Команды проверяет и выполняет Core.

use std::collections::{HashMap, HashSet};

use gpui::{
    App, Context, Deferred, Entity, FocusHandle, IntoElement, Pixels, Point, Render, Subscription,
    WeakEntity, Window, div, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::details::{
    ChannelSidebar, SidebarDetails, SidebarTopic, SidebarTopicAction, StreamNotificationMode,
    TopicNotificationMode,
};

use super::{Conversation, Mutation, Panel, State, action};
use crate::gpui_client::{
    preferences::{Language, Preferences},
    right_panel::RightPanel,
    ui::{
        Button, ButtonAppearance, Icon, Palette, SURFACE_RADIUS, TextInput, TextInputChanged,
        TextInputSubmitted, TextRole, label,
        popup::{MenuInput, menu_layer, menu_navigation, menu_surface, tooltip},
    },
};

/// Пункт компактного меню.
#[derive(Clone, Copy)]
enum Item {
    Notifications(TopicNotificationMode), // Задать персональный режим.
    MarkRead,                             // Отметить сообщения прочитанными.
    Rename,                               // Открыть форму с названием.
    ToggleDone,                           // Отметить завершённость темы.
}

/// Адрес и положение открытого меню.
struct Target {
    topic: Uuid,           // UUID строки в подтверждённом снимке канала.
    anchor: Point<Pixels>, // Позиция окна для размещения меню.
    selected: usize,       // Номер выделенного пункта для клавиатуры.
}

/// Состояние меню в пределах одного открытия панели.
pub(super) struct TopicMenu {
    target: Option<Target>,           // None, пока меню скрыто.
    focus: FocusHandle,               // Фокус работает через общее всплывающее меню.
    rows: HashMap<Uuid, FocusHandle>, // Фокус строк переживает обновление снимка.
}

impl TopicMenu {
    /// Готовит фокус всплывающего меню.
    pub(super) fn new(app: &App) -> Self {
        Self {
            target: None,
            focus: app.focus_handle(),
            rows: HashMap::new(),
        }
    }

    /// Закрывает меню и отпускает фокус строк старого снимка.
    pub(super) fn reset(&mut self) {
        self.target = None;
        self.rows.clear();
    }

    /// Оставляет фокус строк, которые есть и в предыдущем, и в новом снимке.
    pub(super) fn sync(&mut self, snapshot: &SidebarDetails, app: &App) {
        let SidebarDetails::Channel(channel) = snapshot else {
            self.reset();
            return;
        };
        let visible: HashSet<_> = channel
            .topics
            .iter()
            .filter(|topic| !topic.done)
            .map(|topic| topic.id)
            .collect();
        self.rows.retain(|id, _| visible.contains(id));
        for id in visible {
            self.rows.entry(id).or_insert_with(|| app.focus_handle());
        }
    }

    /// Возвращает стабильный фокус строки для мыши и клавиатуры.
    pub(super) fn focus(&self, id: Uuid) -> Option<&FocusHandle> {
        self.rows.get(&id)
    }
}

impl Panel {
    /// Проверяет, что видны сведения о нужной теме и в этот момент можно начать действие.
    fn actionable_topic(&self, id: Uuid) -> Option<(&ChannelSidebar, &SidebarTopic)> {
        if self.mutation.is_some() || !matches!(self.state, Some(State::Ready)) {
            return None;
        }
        let Some(SidebarDetails::Channel(channel)) = self.snapshot.as_deref() else {
            return None;
        };
        let topic = channel
            .topics
            .iter()
            .find(|topic| topic.id == id && !topic.done)?;
        Some((channel, topic))
    }
}

impl Conversation {
    /// Открывает меню строки после повторной проверки доступного канала.
    pub(super) fn open_topic_menu(
        &mut self,
        topic: Uuid,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.details else {
            return;
        };
        if panel.actionable_topic(topic).is_none() {
            return;
        }
        panel.topic_menu.target = Some(Target {
            topic,
            anchor,
            selected: 0,
        });
        window.focus(&panel.topic_menu.focus);
        cx.notify();
    }

    /// Закрывает меню и возвращает фокус на исходную строку или рамку.
    fn close_topic_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.details else {
            return;
        };
        if let Some(target) = panel.topic_menu.target.take() {
            if let Some(opener) = panel.topic_menu.focus(target.topic) {
                window.focus(opener);
            } else {
                panel.host.read(cx).focus_close(&panel.key, window);
            }
            cx.notify();
        }
    }

    /// Строит общее всплывающее меню с режимами уведомлений и действиями.
    pub(super) fn render_topic_menu(
        &mut self,
        window: &mut Window,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Option<[Deferred; 2]> {
        let panel = self.details.as_ref()?;
        let target = panel.topic_menu.target.as_ref()?;
        let Some((channel, topic)) = panel.actionable_topic(target.topic) else {
            self.close_topic_menu(window, cx);
            return None;
        };
        let language = Preferences::read_for(cx, &self.address.route.owner).language;
        let viewport = window.viewport_size();
        let text = |key| language.tr(key);
        let surface = menu_surface("details-topic-menu", palette, viewport, px(220.), px(240.))
            .debug_selector(|| "details-topic-menu".into())
            .on_mouse_down_out(
                cx.listener(|view, _, window, cx| view.close_topic_menu(window, cx)),
            );
        let mut notifications = div()
            .flex()
            .gap(px(2.))
            .p(px(2.))
            .rounded_md()
            .bg(palette.background);
        let direct = channel.direct_peer.is_some();
        let items = items_for(topic, channel.notification_mode, direct);
        let mut commands = div().flex().flex_col();
        for (index, item) in items.into_iter().enumerate() {
            let (id, icon, name) = item.presentation(language, direct);
            let mut button = Button::new(id, "");
            button.appearance = ButtonAppearance::Ghost;
            button.icon = Some(icon);
            let active = target.selected == index && palette.keyboard_focus_visible();
            let click = action(cx, move |view, window, cx| {
                view.run_topic_menu(item, window, cx)
            });
            if let Item::Notifications(mode) = item {
                notifications = notifications.child(
                    button
                        .render(palette, None, click)
                        .h(px(28.))
                        .flex_1()
                        .tab_stop(false)
                        .when(active || mode == topic.notification_mode.displayed(direct), |row| {
                            row.bg(palette.card)
                        })
                        .when(active, |row| row.border_color(palette.focus))
                        .tooltip(move |_, app| tooltip(palette, name.into(), app))
                        .debug_selector(move || id.into()),
                );
            } else {
                button.label = name.into();
                commands = commands.child(
                    button
                        .render(palette, None, click)
                        .justify_start()
                        .tab_stop(false)
                        .when(active, |row| row.bg(palette.card_selected))
                        .debug_selector(move || id.into()),
                );
            }
        }
        let surface = surface
            .child(
                div()
                    .px_1()
                    .py_1()
                    .child(label(
                        palette,
                        TextRole::Caption,
                        text("channel.topicNotifications"),
                    ))
                    .child(notifications),
            )
            .child(commands);
        Some(menu_layer(
            viewport,
            target.anchor,
            menu_navigation(surface, &panel.topic_menu.focus, cx, Self::topic_menu_input),
        ))
    }

    /// Передвигает выбор и подтверждает его клавишей Enter или пробелом.
    fn topic_menu_input(&mut self, input: MenuInput, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(input, MenuInput::Close) {
            self.close_topic_menu(window, cx);
            return;
        }
        let Some(panel) = &mut self.details else {
            return;
        };
        let Some(target) = panel.topic_menu.target.as_ref() else {
            return;
        };
        let Some((channel, topic)) = panel.actionable_topic(target.topic) else {
            return;
        };
        let items = items_for(topic, channel.notification_mode, channel.direct_peer.is_some());
        let last = items.len() - 1;
        if matches!(input, MenuInput::Select) {
            let item = items[target.selected.min(last)];
            self.run_topic_menu(item, window, cx);
            return;
        }
        let target = panel.topic_menu.target.as_mut().expect("Меню уже открыто");
        target.selected = match input {
            MenuInput::First => 0,
            MenuInput::Last => last,
            MenuInput::Previous => target.selected.saturating_sub(1),
            MenuInput::Next => (target.selected + 1).min(last),
            MenuInput::Select | MenuInput::Close => unreachable!(),
        };
        cx.notify();
    }

    /// Выполняет выбранный пункт для темы, которая по-прежнему есть в снимке.
    fn run_topic_menu(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        let Some(topic_id) = self
            .details
            .as_ref()
            .and_then(|panel| panel.topic_menu.target.as_ref().map(|target| target.topic))
        else {
            return;
        };
        self.close_topic_menu(window, cx);
        let Some(panel) = &self.details else {
            return;
        };
        let Some((channel, topic)) = panel.actionable_topic(topic_id) else {
            return;
        };
        let command = match item {
            Item::Notifications(mode)
                if mode != topic.notification_mode.displayed(channel.direct_peer.is_some()) =>
            {
                SidebarTopicAction::SetNotifications(mode)
            }
            Item::MarkRead if topic.unread_count > 0 => SidebarTopicAction::MarkRead,
            Item::ToggleDone => {
                panel.host.read(cx).focus_close(&panel.key, window);
                SidebarTopicAction::ToggleDone
            }
            Item::Rename => {
                let Some(opener) = panel.topic_menu.focus(topic_id).cloned() else {
                    return;
                };
                let conversation = cx.entity();
                let host = panel.host.clone();
                let channel = channel.name.clone();
                let name = topic.name.clone();
                let language = Preferences::read_for(cx, &self.address.route.owner).language;
                host.update(cx, |host, cx| {
                    let weak_host = cx.entity().downgrade();
                    host.open_conversation_modal(&self.address, opener, window, cx, |_, cx| {
                        cx.new(|cx| {
                            RenameTopic::new(
                                topic_id,
                                name,
                                channel,
                                language,
                                &conversation,
                                weak_host,
                                cx,
                            )
                        })
                    });
                });
                return;
            }
            _ => return,
        };
        self.start_sidebar_mutation(Mutation::Topic(topic_id, command), window, cx);
    }
}

/// Сохраняет порядок пунктов и показывает явное включение только когда оно уместно.
fn items_for(topic: &SidebarTopic, stream_mode: StreamNotificationMode, direct: bool) -> Vec<Item> {
    let mut items: Vec<_> = TopicNotificationMode::choices(
        direct, stream_mode == StreamNotificationMode::Muted, topic.notification_mode,
    )
    .iter()
    .copied()
    .map(Item::Notifications)
    .collect();
    if topic.unread_count > 0 {
        items.push(Item::MarkRead);
    }
    items.extend([Item::Rename, Item::ToggleDone]);
    items
}

impl Item {
    /// Описывает видимое имя пункта и идентификатор для сценария проверки.
    fn presentation(self, language: Language, direct: bool) -> (&'static str, Icon, &'static str) {
        use TopicNotificationMode::{Default, Follow, Mute, Unmute};
        let text = |key| language.tr(key);
        match self {
            Self::Notifications(Mute) => (
                "details-topic-notification-mute",
                Icon::BellOff,
                text("channel.notificationMuted"),
            ),
            Self::Notifications(Default) => (
                "details-topic-notification-default",
                Icon::Bell,
                text(if direct {
                    "desktop.sidebar.actions.inheritFromChat"
                } else {
                    "desktop.conversation.topics.inheritFromChannel"
                }),
            ),
            Self::Notifications(Unmute) => (
                "details-topic-notification-unmute",
                Icon::At,
                text("desktop.conversation.topics.unmuted"),
            ),
            Self::Notifications(Follow) => (
                "details-topic-notification-follow",
                Icon::TopicFollow,
                text("desktop.conversation.topics.follow"),
            ),
            Self::MarkRead => (
                "details-topic-read",
                Icon::Check,
                text("desktop.conversation.topics.markAsRead"),
            ),
            Self::Rename => (
                "details-topic-rename",
                Icon::Stylus,
                text("channel.renameTopic"),
            ),
            Self::ToggleDone => (
                "details-topic-done",
                Icon::Check,
                text("desktop.conversation.topics.markAsCompleted"),
            ),
        }
    }
}

/// Форма редактирования названия одной открытой темы канала.
struct RenameTopic {
    id: Uuid,                               // Тема в снимке текущей панели.
    channel: String,                        // Название канала под заголовком диалога.
    language: Language,                     // Язык подписей при открытии.
    host: WeakEntity<RightPanel>,           // Закрывает окно и возвращает фокус.
    conversation: WeakEntity<Conversation>, // Снова проверяет тему перед отправкой.
    name: Entity<TextInput>,                // Текущее значение для сохранения.
    submitted: bool,                        // Ждём результата отправленной команды.
    first_frame: bool,                      // Ставит начальный фокус в поле.
    _submit: Option<Subscription>,          // Enter привязан к действующему окну.
    _changes: [Subscription; 2],            // Изменения поля и разговора.
}

impl RenameTopic {
    /// Готовит форму с исходным названием темы.
    fn new(
        id: Uuid,
        name: String,
        channel: String,
        language: Language,
        conversation: &Entity<Conversation>,
        host: WeakEntity<RightPanel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut input = TextInput::new(
                "details-topic-name",
                language.tr("channel.topicName"),
                false,
                cx,
            );
            input.set_text(&name, cx);
            input
        });
        let edit = cx.subscribe(&input, |_, _, _: &TextInputChanged, cx| cx.notify());
        let updates = cx.observe(conversation, |_, _, cx| cx.notify());
        Self {
            id,
            channel,
            language,
            host,
            conversation: conversation.downgrade(),
            name: input,
            submitted: false,
            first_frame: true,
            _submit: None,
            _changes: [edit, updates],
        }
    }

    /// Передаёт принятое название только исходной теме действующего разговора.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_owned();
        if name.is_empty() || self.submitted {
            return;
        }
        let submitted = self
            .conversation
            .update(cx, |view, cx| {
                let Some(topic) = view
                    .details
                    .as_ref()
                    .and_then(|panel| panel.actionable_topic(self.id))
                    .map(|(_, topic)| topic)
                else {
                    return false;
                };
                if name == topic.name {
                    return false;
                }
                view.start_sidebar_mutation(
                    Mutation::Topic(self.id, SidebarTopicAction::Rename(name)),
                    window,
                    cx,
                );
                true
            })
            .unwrap_or(false);
        if submitted {
            self.submitted = true;
            self.name.update(cx, |name, cx| name.set_enabled(false, cx));
            cx.notify();
        } else {
            self.close(window, cx);
        }
    }

    /// Закрывает диалог, если он ещё виден.
    fn close(&self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.host.update(cx, |host, cx| {
            if host.modal_is::<Self>() {
                host.close_modal(window, cx);
            }
        });
    }
}

impl Render for RenameTopic {
    /// Показывает поле и кнопки внутри модального слоя текущего разговора.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.first_frame) {
            self.name.read(cx).focus(window);
            self._submit = Some(cx.subscribe_in(
                &self.name,
                window,
                |view, _, _: &TextInputSubmitted, window, cx| {
                    view.save(window, cx);
                },
            ));
        }
        let conversation = self.conversation.upgrade();
        let busy = conversation.as_ref().is_some_and(|view| {
            view.read(cx)
                .details
                .as_ref()
                .is_some_and(|panel| panel.mutation.is_some())
        });
        if self.submitted && !busy {
            self.close(window, cx);
        }
        let palette = Palette::for_window(window, cx);
        let language = conversation.as_ref().map_or(self.language, |view| {
            Preferences::read_for(cx, &view.read(cx).address.route.owner).language
        });
        let text = |key| language.tr(key);
        self.name.update(cx, |input, cx| {
            input.set_placeholder(language.tr("channel.topicName"), cx);
        });
        let mut submit = Button::new("details-topic-rename-save", text("common.save"));
        submit.appearance = ButtonAppearance::Primary;
        submit.disabled = self.submitted || self.name.read(cx).text().trim().is_empty();
        let mut cancel = Button::new("details-topic-rename-cancel", text("common.cancel"));
        cancel.disabled = self.submitted;
        let target = cx.entity().downgrade();
        let dismiss = target.clone();
        div()
            .debug_selector(|| "details-topic-rename-dialog".into())
            .w((window.viewport_size().width - px(24.)).clamp(px(0.), px(420.)))
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .rounded(SURFACE_RADIUS)
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .child(label(
                palette,
                TextRole::Heading,
                text("channel.renameTopic"),
            ))
            .child(label(palette, TextRole::Caption, self.channel.clone()).truncate())
            .child(label(palette, TextRole::Label, text("channel.topicName")))
            .child(self.name.clone())
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(cancel.render(palette, None, move |window, app| {
                        let _ = dismiss.update(app, |view, cx| view.close(window, cx));
                    }))
                    .child(
                        submit
                            .render(palette, None, move |window, app| {
                                let _ = target.update(app, |view, cx| view.save(window, cx));
                            })
                            .debug_selector(|| "details-topic-rename-save".into()),
                    ),
            )
    }
}
