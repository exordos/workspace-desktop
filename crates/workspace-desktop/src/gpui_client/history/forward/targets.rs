//! Собирает цели пересылки из каталога и рисует выбор канала, темы или личного чата.

use gpui::{
    Context, Div, Entity, FocusHandle, Pixels, SharedString, Stateful, Subscription, Window, div,
    prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{messenger::catalog::Chat, storage::HistoryScope};

use super::{ForwardDialog, ForwardSelection, action};
use crate::gpui_client::{
    preferences::Language,
    ui::{
        Button, ButtonAppearance, Icon, Palette, SURFACE_RADIUS, TextInput, TextInputChanged,
        TextRole, activate, label,
    },
};

/// Подтверждённый каталог целей для одного открытия диалога.
pub(super) struct Targets {
    channels: Vec<Channel>, // Нативные каналы с хотя бы одной доступной темой.
    direct: Vec<Direct>,    // Доступные лички, начиная с «Избранного».
}

/// Канал и его доступные пользователю незавершённые темы.
struct Channel {
    id: Uuid,           // UUID канала для адреса пересылки.
    name: SharedString, // Текущее название в каталоге.
    topics: Vec<Topic>, // Тема по умолчанию идёт первой, остальные по имени.
}

/// Одна тема выбранного канала.
struct Topic {
    id: Uuid,           // UUID темы для точного выбора адреса.
    name: SharedString, // Текущее название в каталоге.
}

/// Личный разговор, который сервер уже подтвердил в каталоге.
struct Direct {
    id: Uuid,                    // UUID лички с маршрутом по умолчанию.
    name: SharedString,          // Имя собеседника или «Избранное».
    search: String,              // Имя, логин и почта слиты в нижнем регистре для фильтра.
    favorites: bool,             // Собеседник совпадает с текущим пользователем.
    email: Option<SharedString>, // Доступная подпись для различения одноимённых собеседников.
}

impl Targets {
    /// Использует каталог владельца, оставляя нативные открытые чаты и темы.
    pub(super) fn from_catalog(chats: &[Chat], user: Option<Uuid>, locale: Language) -> Self {
        let mut channels = Vec::new();
        let mut direct = Vec::new();
        for chat in chats.iter().filter(|chat| chat.metadata.native_markdown) {
            if chat.direct {
                let favorites = user.is_some() && user == chat.metadata.direct_peer_id;
                let name = if favorites {
                    locale.tr("activity.favorites")
                } else {
                    &chat.name
                };
                let search = if favorites {
                    name.to_lowercase()
                } else {
                    format!(
                        "{}\n{}\n{}",
                        name,
                        chat.metadata
                            .direct_peer_username
                            .as_deref()
                            .unwrap_or_default(),
                        chat.metadata
                            .direct_peer_email
                            .as_deref()
                            .unwrap_or_default(),
                    )
                    .to_lowercase()
                };
                direct.push(Direct {
                    id: chat.id,
                    name: name.to_owned().into(),
                    search,
                    favorites,
                    email: chat
                        .metadata
                        .direct_peer_email
                        .as_deref()
                        .filter(|email| !favorites && !email.trim().is_empty())
                        .map(|email| SharedString::from(email.to_owned())),
                });
                continue;
            }
            let mut topics = chat
                .topics
                .iter()
                .filter(|topic| !topic.metadata.is_done)
                .collect::<Vec<_>>();
            topics.sort_by_cached_key(|topic| {
                (
                    !topic.metadata.is_default,
                    topic.name.to_lowercase(),
                    topic.id,
                )
            });
            if !topics.is_empty() {
                channels.push(Channel {
                    id: chat.id,
                    name: chat.name.clone().into(),
                    topics: topics
                        .into_iter()
                        .map(|topic| Topic {
                            id: topic.id,
                            name: topic.name.clone().into(),
                        })
                        .collect(),
                });
            }
        }
        channels.sort_by_cached_key(|channel| (channel.name.to_lowercase(), channel.id));
        direct.sort_by_cached_key(|chat| (!chat.favorites, chat.name.to_lowercase(), chat.id));
        Self { channels, direct }
    }
}

/// Вид получателя, который сейчас выбирает пользователь.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Channel, // Два явных выбора канала и темы.
    Direct,  // Поиск среди существующих личных чатов.
}

/// Поле, список которого раскрыт в выборе канала.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Channel, // Первый список выбирает канал и сбрасывает старую тему.
    Topic,   // Второй список требует выбранный канал.
}

/// Поиск среди существующих личек.
struct Search {
    input: Entity<TextInput>, // Поле с обычным вводом GPUI.
    _changed: Subscription,   // Перерисовывает результаты фильтра.
}

/// Локальные данные формы, пока открыто модальное окно.
pub(super) struct Picker {
    search: Option<Search>,  // None у отдельного диалога защищённого источника.
    tab: Tab,                // Какая вкладка сейчас подтверждается кнопкой отправки.
    expanded: Option<Field>, // None скрывает оба списка выбора канала.
    channel: Option<Uuid>,   // Выбранный канал, даже если тема ещё не указана.
    channel_choice: Option<HistoryScope>, // Выбранная область канала и темы.
    direct_choice: Option<HistoryScope>, // Выбор в личках или отдельном защищённом диалоге.
    topic_focus: FocusHandle, // Куда перейти с клавиатуры после выбора канала.
}

impl Picker {
    /// Оставляет у защищённого источника прежний выбор, а для обычного открывает вкладку канала.
    pub(super) fn new(
        selection: &ForwardSelection,
        locale: Language,
        cx: &mut Context<ForwardDialog>,
    ) -> Self {
        let search = matches!(selection, ForwardSelection::Message(_)).then(|| {
            let input = cx.new(|cx| {
                TextInput::new(
                    "forward-direct-query",
                    locale.tr("message.searchUsers"),
                    false,
                    cx,
                )
            });
            let changed = cx.subscribe(&input, |_, _, _: &TextInputChanged, cx| cx.notify());
            Search {
                input,
                _changed: changed,
            }
        });
        Self {
            search,
            tab: Tab::Channel,
            expanded: None,
            channel: None,
            channel_choice: None,
            direct_choice: None,
            topic_focus: cx.focus_handle(),
        }
    }

    /// Блокирует системный ввод, пока команда готовится в Core.
    pub(super) fn set_enabled(&self, enabled: bool, cx: &mut Context<ForwardDialog>) {
        if let Some(search) = &self.search {
            search
                .input
                .update(cx, |input, cx| input.set_enabled(enabled, cx));
        }
    }

    /// Возвращает цель активной вкладки, если в ней завершён выбор.
    pub(super) fn selected(&self) -> Option<HistoryScope> {
        if self.search.is_some() && self.tab == Tab::Channel {
            self.channel_choice
        } else {
            self.direct_choice
        }
    }

    /// Сохраняет прежний заголовок защищённого сценария.
    pub(super) fn title(&self) -> &'static str {
        if self.search.is_some() {
            "message.forwardToChannel"
        } else {
            "message.forwardDestination"
        }
    }

    /// Подгоняет вкладку канала под содержимое, отдавая спискам часть высоты окна.
    pub(super) fn dialog_height(&self, height: Pixels) -> Option<Pixels> {
        if self.search.is_none() {
            Some((height * 0.7).min(px(500.)))
        } else {
            (self.tab == Tab::Direct).then_some(height * 0.8)
        }
    }

    /// Собирает обе вкладки из общих кнопок и подсвечивает выбранную.
    pub(super) fn tabs(
        &self,
        dialog: &ForwardDialog,
        palette: Palette,
        cx: &Context<ForwardDialog>,
    ) -> Option<Div> {
        self.search.as_ref()?;
        let tab = |value, id: &'static str, key| {
            let selected = self.tab == value;
            let mut button = Button::new(id, dialog.locale.tr(key));
            button.appearance = ButtonAppearance::Ghost;
            button.disabled = dialog.pending;
            div()
                .flex_1()
                .min_w_0()
                .border_b_2()
                .border_color(if selected {
                    palette.accent
                } else {
                    gpui::transparent_black()
                })
                .child(
                    button
                        .render(
                            palette,
                            None,
                            action(cx, move |view, _, cx| {
                                if !view.pending {
                                    view.picker.tab = value;
                                    view.picker.expanded = None;
                                    view.failed = false;
                                    cx.notify();
                                }
                            }),
                        )
                        .w_full()
                        .debug_selector(move || id.into()),
                )
        };
        Some(
            div()
                .flex()
                .flex_shrink_0()
                .border_b_1()
                .border_color(palette.border)
                .child(tab(Tab::Channel, "forward-channel-tab", "message.channel"))
                .child(tab(
                    Tab::Direct,
                    "forward-direct-tab",
                    "message.directMessage",
                )),
        )
    }

    /// Рисует содержимое выбранной вкладки в одной области модального окна.
    pub(super) fn content(
        &self,
        dialog: &ForwardDialog,
        palette: Palette,
        cx: &Context<ForwardDialog>,
    ) -> Stateful<Div> {
        let body = div()
            .id("forward-target-picker")
            .flex()
            .flex_col()
            .min_h_0()
            .gap_3()
            .px_4()
            .pt_4();
        match (&self.search, self.tab) {
            (None, _) => body.flex_1().child(self.protected(dialog, palette, cx)),
            (Some(search), Tab::Direct) => {
                body.flex_1()
                    .child(search.input.clone())
                    .child(self.direct_options(
                        dialog,
                        palette,
                        search.input.read(cx).text().trim().to_lowercase(),
                        cx,
                    ))
            }
            (Some(_), Tab::Channel) => body
                .overflow_y_scroll()
                .child(self.select_field(Field::Channel, dialog, palette, cx))
                .child(self.select_field(Field::Topic, dialog, palette, cx))
                .children(
                    dialog
                        .targets
                        .channels
                        .is_empty()
                        .then(|| empty(dialog, palette)),
                ),
        }
    }

    /// Отображает список прежнего защищённого маршрута с теми же идентификаторами строк.
    fn protected(
        &self,
        dialog: &ForwardDialog,
        palette: Palette,
        cx: &Context<ForwardDialog>,
    ) -> Stateful<Div> {
        let direct = dialog.targets.direct.iter().map(|chat| {
            (
                HistoryScope {
                    stream_id: chat.id,
                    topic_id: None,
                },
                chat.name.clone(),
            )
        });
        let topics = dialog.targets.channels.iter().flat_map(|channel| {
            channel.topics.iter().map(|topic| {
                (
                    HistoryScope {
                        stream_id: channel.id,
                        topic_id: Some(topic.id),
                    },
                    format!("{} · {}", channel.name, topic.name).into(),
                )
            })
        });
        let mut entries = direct.chain(topics).peekable();
        option_list("fragment-forward-targets", palette)
            .children(entries.peek().is_none().then(|| empty(dialog, palette)))
            .children(
                entries.map(|(scope, name)| destination_button(dialog, palette, scope, name, cx)),
            )
    }

    /// Показывает доступные лички, включая Избранное только при наличии его в каталоге.
    fn direct_options(
        &self,
        dialog: &ForwardDialog,
        palette: Palette,
        query: String,
        cx: &Context<ForwardDialog>,
    ) -> Stateful<Div> {
        let mut chats = dialog
            .targets
            .direct
            .iter()
            .filter(|chat| chat.search.contains(&query))
            .peekable();
        option_list("forward-direct-targets", palette)
            .children(chats.peek().is_none().then(|| {
                label(
                    palette,
                    TextRole::Caption,
                    dialog.locale.tr("search.noResults"),
                )
                .text_center()
                .px_3()
                .py_4()
            }))
            .children(chats.map(|chat| direct_option(dialog, palette, chat, cx)))
    }

    /// Выбирает канал или тему и раскрывает только относящийся к нему список.
    fn select_field(
        &self,
        field: Field,
        dialog: &ForwardDialog,
        palette: Palette,
        cx: &Context<ForwardDialog>,
    ) -> Div {
        let channel = dialog
            .targets
            .channels
            .iter()
            .find(|channel| Some(channel.id) == self.channel);
        let (id, caption, placeholder, name, enabled) = match field {
            Field::Channel => (
                "forward-channel-select",
                "channel.name",
                "channel.selectChannel",
                channel.map(|channel| channel.name.clone()),
                !dialog.targets.channels.is_empty(),
            ),
            Field::Topic => (
                "forward-topic-select",
                "channel.topicName",
                "chat.selectTopic",
                channel
                    .and_then(|channel| {
                        channel.topics.iter().find(|topic| {
                            self.channel_choice.and_then(|scope| scope.topic_id) == Some(topic.id)
                        })
                    })
                    .map(|topic| topic.name.clone()),
                channel.is_some(),
            ),
        };
        let mut opener = Button::new(
            id,
            name.unwrap_or_else(|| dialog.locale.tr(placeholder).into()),
        );
        let icon = if self.expanded == Some(field) {
            Icon::ChevronUp
        } else {
            Icon::ChevronDown
        };
        let disabled = dialog.pending || !enabled;
        opener.disabled = disabled;
        let focus = (field == Field::Topic).then_some(&self.topic_focus);
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_2()
            .child(label(palette, TextRole::Caption, dialog.locale.tr(caption)))
            .child(
                opener
                    .render(
                        palette,
                        focus,
                        action(cx, move |view, _, cx| {
                            if !view.pending {
                                view.picker.expanded =
                                    (view.picker.expanded != Some(field)).then_some(field);
                                cx.notify();
                            }
                        }),
                    )
                    .relative()
                    .w_full()
                    .justify_start()
                    .pr(px(28.))
                    .child(div().absolute().top_2().right_2().child(icon.element(
                        px(16.),
                        if disabled {
                            palette.disabled
                        } else {
                            palette.icon
                        },
                    )))
                    .debug_selector(move || id.into()),
            )
            .children((self.expanded == Some(field)).then(|| {
                let list = option_list("forward-channel-options", palette)
                    .flex_none()
                    .max_h(px(144.));
                match field {
                    Field::Channel => {
                        list.children(dialog.targets.channels.iter().map(|channel| {
                            let id = channel.id;
                            choice_button(
                                dialog,
                                palette,
                                format!("forward-channel-{id}"),
                                channel.name.clone(),
                                self.channel == Some(id),
                                action(cx, move |view, window, cx| {
                                    if !view.pending {
                                        if view.picker.channel != Some(id) {
                                            view.picker.channel_choice = None;
                                        }
                                        view.picker.channel = Some(id);
                                        view.picker.expanded = None;
                                        view.failed = false;
                                        ForwardDialog::focus_next(
                                            view.picker.topic_focus.clone(),
                                            window,
                                            cx,
                                        );
                                        cx.notify();
                                    }
                                }),
                            )
                        }))
                    }
                    Field::Topic => list.children(channel.into_iter().flat_map(|channel| {
                        channel.topics.iter().map(|topic| {
                            destination_button(
                                dialog,
                                palette,
                                HistoryScope {
                                    stream_id: channel.id,
                                    topic_id: Some(topic.id),
                                },
                                topic.name.clone(),
                                cx,
                            )
                        })
                    })),
                }
            }))
    }
}

/// Рисует область прокрутки, общую для списков обеих вкладок.
fn option_list(id: &'static str, palette: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .rounded(SURFACE_RADIUS)
        .border_1()
        .border_color(palette.border)
}

/// Сообщает, что для показанного списка ничего выбрать нельзя.
fn empty(dialog: &ForwardDialog, palette: Palette) -> Div {
    label(
        palette,
        TextRole::Caption,
        dialog.locale.tr("message.forwardNoDestinations"),
    )
    .px_3()
    .py_2()
}

/// Выбирает точный адрес для пересылки и передаёт фокус кнопке подтверждения.
fn destination_button(
    dialog: &ForwardDialog,
    palette: Palette,
    scope: HistoryScope,
    name: SharedString,
    cx: &Context<ForwardDialog>,
) -> Stateful<Div> {
    let selector = format!(
        "fragment-forward-target-{}",
        scope.topic_id.unwrap_or(scope.stream_id)
    );
    choice_button(
        dialog,
        palette,
        selector,
        name,
        dialog.picker.selected() == Some(scope),
        action(cx, move |view, window, cx| view.choose(scope, window, cx)),
    )
}

/// Подписывает личку по имени и почте, выделяя выбранного получателя.
fn direct_option(
    dialog: &ForwardDialog,
    palette: Palette,
    chat: &Direct,
    cx: &Context<ForwardDialog>,
) -> Stateful<Div> {
    let scope = HistoryScope {
        stream_id: chat.id,
        topic_id: None,
    };
    let chosen = dialog.picker.selected() == Some(scope);
    let selector = format!("fragment-forward-target-{}", chat.id);
    let row = div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || {
            if chosen {
                format!("{selector}-selected")
            } else {
                selector.clone()
            }
        })
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .border_1()
        .border_color(gpui::transparent_black())
        .when(chosen, |row| row.bg(palette.card_selected))
        .children(
            chat.favorites
                .then(|| Icon::Home.element(px(18.), palette.accent)),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .child(label(palette, TextRole::Label, chat.name.clone()).truncate())
                .children(
                    chat.email
                        .iter()
                        .map(|email| label(palette, TextRole::Caption, email.clone()).truncate()),
                ),
        )
        .children(chosen.then(|| Icon::Check.element(px(14.), palette.accent)));
    if dialog.pending {
        row
    } else {
        activate(
            row.hover(|row| row.bg(palette.card_hover)),
            None,
            palette,
            action(cx, move |view, window, cx| view.choose(scope, window, cx)),
        )
    }
}

impl ForwardDialog {
    /// Сохраняет выбор в активной вкладке и переносит фокус на отправку.
    fn choose(&mut self, scope: HistoryScope, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }
        if self.picker.search.is_some() && self.picker.tab == Tab::Channel {
            self.picker.channel_choice = Some(scope);
            self.picker.expanded = None;
        } else {
            self.picker.direct_choice = Some(scope);
        }
        self.failed = false;
        Self::focus_next(self.submit_focus.clone(), window, cx);
        cx.notify();
    }
}

/// Рисует строку выбора с общей кнопкой и устойчивым идентификатором.
fn choice_button(
    dialog: &ForwardDialog,
    palette: Palette,
    selector: String,
    name: SharedString,
    chosen: bool,
    callback: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let mut button = Button::new(SharedString::from(selector.clone()), name);
    button.appearance = ButtonAppearance::Ghost;
    button.disabled = dialog.pending;
    button
        .render(palette, None, callback)
        .flex_shrink_0()
        .w_full()
        .justify_start()
        .when(chosen, |button| button.bg(palette.card_selected))
        .debug_selector(move || {
            if chosen {
                format!("{selector}-selected")
            } else {
                selector.clone()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_core::messenger::catalog::{ChatMetadata, Topic as CatalogTopic, TopicMetadata};

    /// Оставляет проверенные цели, ставит «Избранное» и тему по умолчанию первыми.
    #[test]
    fn forward_targets_use_existing_chats_and_open_topics() {
        let direct = |id, peer, name: &str, native_markdown| Chat {
            id: Uuid::from_u128(id),
            name: name.into(),
            direct: true,
            topics: Vec::new(),
            metadata: ChatMetadata {
                native_markdown,
                direct_peer_id: Some(Uuid::from_u128(peer)),
                direct_peer_username: Some("AccountLogin".into()),
                direct_peer_email: Some("Person@example.test".into()),
                ..ChatMetadata::default()
            },
        };
        let topic = |id, name: &str, is_default, is_done| CatalogTopic {
            id: Uuid::from_u128(id),
            name: name.into(),
            metadata: TopicMetadata {
                is_default,
                is_done,
                ..TopicMetadata::default()
            },
        };
        let channel = Chat {
            id: Uuid::from_u128(4),
            name: "Команда".into(),
            direct: false,
            topics: vec![
                topic(40, "Архив", false, true),
                topic(41, "Анонсы", false, false),
                topic(42, "Общее", true, false),
            ],
            metadata: ChatMetadata {
                native_markdown: true,
                ..ChatMetadata::default()
            },
        };
        let targets = Targets::from_catalog(
            &[
                direct(1, 21, "Алина", true),
                direct(2, 22, "Текущий пользователь", true),
                direct(3, 23, "Защищённая личка", false),
                channel,
            ],
            Some(Uuid::from_u128(22)),
            Language::Ru,
        );

        assert_eq!(targets.direct.len(), 2);
        assert_eq!(targets.direct[0].name.as_ref(), "Избранное");
        assert_eq!(targets.direct[0].id, Uuid::from_u128(2));
        assert!(targets.direct[0].email.is_none());
        assert!(targets.direct[1].search.contains("accountlogin"));
        assert!(targets.direct[1].search.contains("person@example.test"));
        assert_eq!(targets.channels.len(), 1);
        assert_eq!(
            targets.channels[0]
                .topics
                .iter()
                .map(|topic| topic.id)
                .collect::<Vec<_>>(),
            [Uuid::from_u128(42), Uuid::from_u128(41)],
        );
    }
}
