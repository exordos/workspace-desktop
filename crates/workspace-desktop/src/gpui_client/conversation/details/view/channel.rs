//! Панель канала: заголовок и прокручиваемые блоки настроек, тем и участников.

use gpui::{Div, SharedString, Stateful, div, prelude::*, px};

use super::{
    Availability, ChannelView, Dispatch, MemberView, NotificationLevel, NotificationsView,
    PanelAction, SelectedTopicView, TopicView,
};
use crate::gpui_client::ui::{
    BadgeTone, Button, ButtonAppearance, Icon, Palette, TextRole, activate, ellipsized_label,
    label, mention_badge, popup::tooltip, unread_badge,
};

/// Рисует сведения о канале с закреплённым аватаром и счётчиками.
pub(super) fn render(
    channel: &ChannelView,
    gallery: Option<Div>,
    palette: Palette,
    dispatch: &Dispatch,
) -> Div {
    let header = div()
        .flex()
        .flex_none()
        .w_full()
        .items_center()
        .gap(px(16.))
        .px(px(8.))
        .pt(px(12.))
        .pb(px(12.))
        .child(channel.avatar.avatar().render_at(
            palette,
            px(64.),
            channel.color.unwrap_or(palette.background),
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(
                    ellipsized_label(palette, TextRole::ProfileHeading, channel.name.clone())
                        .debug_selector(|| "conversation-details-channel".into()),
                )
                .child(
                    label(
                        palette,
                        TextRole::Caption,
                        format!(
                            "{} {}, {} в сети",
                            channel.member_count,
                            participant_word(channel.member_count),
                            channel.online_count
                        ),
                    )
                    .text_size(px(11.))
                    .line_height(px(16.)),
                ),
        );
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(header)
        .child(
            div()
                .id("details-channel-scroll")
                .min_h_0()
                .flex_1()
                .overflow_x_hidden()
                .overflow_y_scroll()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(20.))
                        .py(px(8.))
                        .child(render_notifications(
                            channel.notifications.as_ref(),
                            palette,
                            dispatch,
                        ))
                        .children(
                            channel
                                .selected_topic
                                .iter()
                                .map(|topic| render_topic_context(topic, palette)),
                        )
                        .children(channel.description.iter().map(|text| {
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(12.))
                                .child(label(palette, TextRole::Label, "Описание").px(px(8.)))
                                .child(
                                    label(palette, TextRole::Body, text.clone())
                                        .px(px(12.))
                                        .py(px(8.)),
                                )
                        }))
                        .children(gallery)
                        .child(render_topics(channel, palette, dispatch))
                        .child(render_members(channel, palette, dispatch)),
                ),
        )
}

/// Показывает готовый контекст из выбранной темы и состояние завершения.
fn render_topic_context(topic: &SelectedTopicView, palette: Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .when(topic.done, |section| {
            section.child(
                div()
                    .px(px(8.))
                    .child(
                        ellipsized_label(palette, TextRole::Label, topic.title.clone())
                            .debug_selector(|| "conversation-details-topic".into()),
                    )
                    .child(label(palette, TextRole::Caption, "Тема завершена")),
            )
        })
        .children(topic.summary.iter().map(|summary| {
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .debug_selector(|| "conversation-details-topic-summary".into())
                .child(label(palette, TextRole::Label, "Контекст темы").px(px(8.)))
                .child(
                    label(palette, TextRole::Body, summary.clone())
                        .rounded_md()
                        .border_1()
                        .border_color(palette.border)
                        .px(px(12.))
                        .py(px(8.)),
                )
        }))
}

/// Подбирает форму счётчика участников для русского интерфейса.
fn participant_word(count: usize) -> &'static str {
    if (11..=14).contains(&(count % 100)) {
        return "участников";
    }
    match count % 10 {
        1 => "участник",
        2..=4 => "участника",
        _ => "участников",
    }
}

/// Показывает состояние трёх уровней уведомлений и результат операции.
fn render_notifications(
    notifications: Option<&NotificationsView>,
    palette: Palette,
    dispatch: &Dispatch,
) -> Div {
    let title = div()
        .flex()
        .flex_wrap()
        .gap(px(4.))
        .px(px(8.))
        .child(label(palette, TextRole::Label, "Уведомления"))
        .child(label(palette, TextRole::Label, "(Для всего чата)").text_color(palette.text_muted));
    let Some(notifications) = notifications else {
        return div().child(title).child(
            label(palette, TextRole::Body, "Уведомления недоступны")
                .px(px(8.))
                .py(px(8.))
                .text_color(palette.text_muted),
        );
    };
    let levels = [
        (
            NotificationLevel::Default,
            "details-notification-mentions",
            "Только упоминания",
            Icon::At,
        ),
        (
            NotificationLevel::Muted,
            "details-notification-muted",
            "Без уведомлений",
            Icon::BellOff,
        ),
        (
            NotificationLevel::Subscribed,
            "details-notification-all",
            "Все уведомления",
            Icon::Bell,
        ),
    ];
    let selected = notifications.level;
    let segments = levels.map(|(level, id, name, icon)| {
        let is_selected = selected == level;
        let segment = div()
            .id(id)
            .debug_selector(move || id.into())
            .h(px(36.))
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_color(gpui::transparent_black())
            .when(is_selected, |segment| segment.bg(palette.card))
            .child(icon.element(
                px(28.),
                if is_selected {
                    palette.text
                } else {
                    palette.text_muted
                },
            ))
            .tooltip(move |_, app| tooltip(palette, name.into(), app));
        if notifications.editable {
            let dispatch = dispatch.clone();
            activate(
                segment.hover(|style| style.bg(palette.card_hover)),
                None,
                palette,
                move |window, app| {
                    if !is_selected {
                        dispatch(PanelAction::SetNotifications(level), window, app);
                    }
                },
            )
        } else {
            segment
        }
    });
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(title)
        .child(
            div()
                .flex()
                .gap(px(8.))
                .p(px(4.))
                .rounded_md()
                .bg(palette.background)
                .children(segments),
        )
        .children(notifications.error.iter().map(|error| {
            label(palette, TextRole::Caption, error.clone())
                .px(px(8.))
                .text_color(palette.danger)
        }))
}

/// Отображает сворачиваемый список тем с уведомлениями и счётчиками.
fn render_topics(channel: &ChannelView, palette: Palette, dispatch: &Dispatch) -> Div {
    let chevron = if channel.topics_expanded {
        Icon::ChevronUp
    } else {
        Icon::ChevronDown
    };
    let toggle = div()
        .id("details-topics-toggle")
        .debug_selector(|| "details-topics-toggle".into())
        .h(px(32.))
        .px(px(8.))
        .flex()
        .items_center()
        .justify_between()
        .rounded_md()
        .border_1()
        .border_color(gpui::transparent_black())
        .hover(|style| style.bg(palette.card_hover))
        .child(label(palette, TextRole::Label, "Темы").text_color(palette.text_muted))
        .child(chevron.element(px(16.), palette.text_muted));
    let toggle = {
        let dispatch = dispatch.clone();
        activate(toggle, None, palette, move |window, app| {
            dispatch(PanelAction::ToggleTopics, window, app);
        })
    };
    let list = if channel.topics.is_empty() {
        label(palette, TextRole::Body, "Нет тем")
            .px(px(8.))
            .py(px(8.))
            .text_color(palette.text_muted)
    } else {
        div()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(palette.border)
            .children(channel.topics.iter().enumerate().map(|(index, topic)| {
                div()
                    .when(index != 0, |row| {
                        row.border_t_1().border_color(palette.border)
                    })
                    .child(render_topic(topic, palette, dispatch))
            }))
    };
    div()
        .child(toggle)
        .when(channel.topics_expanded, |section| section.child(list))
}

/// Рисует строку темы, которая открывает историю из текущего аккаунта.
fn render_topic(topic: &TopicView, palette: Palette, dispatch: &Dispatch) -> Stateful<Div> {
    let id = topic.id;
    let name = ellipsized_label(palette, TextRole::Body, topic.title.clone());
    let badge = if topic.unread_muted {
        BadgeTone::Muted
    } else {
        BadgeTone::Accent
    };
    let row = div()
        .id(SharedString::from(format!("details-topic-{id}")))
        .debug_selector(move || format!("details-topic-{id}"))
        .h(px(36.))
        .px(px(12.))
        .flex()
        .items_center()
        .gap(px(8.))
        .border_1()
        .border_color(gpui::transparent_black())
        .hover(|style| style.bg(palette.card_hover))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .child(name.when(topic.selected, |name| {
                    name.debug_selector(|| "conversation-details-topic".into())
                })),
        )
        .children(topic.mentioned.then(|| mention_badge(palette)))
        .children(
            unread_badge(palette, topic.unread, badge)
                .map(|badge| badge.debug_selector(move || format!("details-topic-unread-{id}"))),
        )
        .children(topic.muted.map(|muted| {
            let icon = if muted { Icon::BellOff } else { Icon::Bell };
            icon.element(px(16.), palette.icon)
        }));
    let dispatch = dispatch.clone();
    activate(row, None, palette, move |window, app| {
        dispatch(PanelAction::OpenTopic(id), window, app);
    })
}

/// Рисует участников, действие добавления, пустой список и ошибку.
fn render_members(channel: &ChannelView, palette: Palette, dispatch: &Dispatch) -> Div {
    let title = div()
        .flex()
        .h(px(32.))
        .items_center()
        .justify_between()
        .px(px(8.))
        .child(label(palette, TextRole::Label, "Участники"))
        .when(channel.add_members != Availability::Hidden, |title| {
            title.child(icon_button(
                "details-add-members".into(),
                Icon::PersonAdd,
                "Добавить участников",
                channel.add_members,
                PanelAction::AddMembers,
                palette,
                dispatch,
            ))
        });
    let mut members = div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .debug_selector(|| "conversation-details-members".into());
    if channel.members.is_empty() {
        members = members.child(
            label(palette, TextRole::Body, "Нет участников")
                .text_center()
                .text_color(palette.text_muted)
                .py(px(12.)),
        );
    } else {
        members = members.children(
            channel
                .members
                .iter()
                .map(|member| render_member(member, palette, dispatch)),
        );
    }
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(title)
        .child(members)
        .children(channel.member_error.iter().map(|error| {
            label(palette, TextRole::Caption, error.clone())
                .px(px(8.))
                .text_color(palette.danger)
        }))
}

/// Рисует карточку участника и отдельное разрешённое действие рядом.
fn render_member(member: &MemberView, palette: Palette, dispatch: &Dispatch) -> Div {
    let name = div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(div().min_w_0().flex_1().child(ellipsized_label(
            palette,
            TextRole::Body,
            member.name.clone(),
        )))
        .children(member.role.iter().map(|role| {
            label(palette, TextRole::Caption, role.clone())
                .text_size(px(10.))
                .line_height(px(14.))
                .flex_none()
        }));
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(12.))
        .py(px(8.))
        .rounded_md()
        .bg(palette.card)
        .group("details-member")
        .hover(|style| style.bg(palette.card_hover))
        .child(
            member
                .avatar
                .avatar()
                .render_at(palette, px(32.), palette.surface),
        )
        .child(
            div().flex_1().min_w_0().child(name).children(
                member
                    .secondary
                    .iter()
                    .map(|text| ellipsized_label(palette, TextRole::Caption, text.clone())),
            ),
        )
        .when(member.remove != Availability::Hidden, |row| {
            row.child(
                icon_button(
                    format!("details-remove-member-{}", member.id).into(),
                    Icon::Close,
                    "Удалить участника",
                    member.remove,
                    PanelAction::RemoveMember(member.id),
                    palette,
                    dispatch,
                )
                .opacity(0.)
                .group_hover("details-member", |style| style.opacity(1.))
                .when(palette.keyboard_focus_visible(), |button| {
                    button.focus(|style| style.border_color(palette.focus).opacity(1.))
                }),
            )
        })
}

/// Использует кнопку UI-кита для действий над участниками.
fn icon_button(
    id: SharedString,
    icon: Icon,
    description: &'static str,
    availability: Availability,
    action: PanelAction,
    palette: Palette,
    dispatch: &Dispatch,
) -> Stateful<Div> {
    let mut button = Button::new(id.clone(), "");
    button.icon = Some(icon);
    button.appearance = ButtonAppearance::Ghost;
    button.disabled = availability != Availability::Enabled;
    let dispatch = dispatch.clone();
    button
        .render(palette, None, move |window, app| {
            dispatch(action.clone(), window, app);
        })
        .flex_none()
        .debug_selector(move || id.to_string())
        .tooltip(move |_, app| tooltip(palette, description.into(), app))
}
