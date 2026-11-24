//! Панель канала: заголовок и прокручиваемые блоки настроек, тем и участников.

use std::{cell::Cell, rc::Rc};

use gpui::{
    Div, FocusHandle, FontStyle, FontWeight, HighlightStyle, MouseButton, SharedString, Stateful,
    StyledText, div, point, prelude::*, px,
};
use workspace_core::messenger::details::TopicSummaryBlock;

use super::{
    Availability, ChannelView, Dispatch, MemberView, NotificationLevel, NotificationsView,
    PanelAction, SelectedTopicView, TopicView,
};
use crate::gpui_client::{
    preferences::Language,
    ui::{
        BadgeTone, Icon, Palette, PersonRow, TextRole, activate, ellipsized_label, label, mention_badge,
        popup::tooltip, text_role, unread_badge,
    },
};

/// Рисует сведения о канале с закреплённым аватаром и счётчиками.
pub(super) fn render(
    channel: &ChannelView,
    gallery: Option<Div>,
    palette: Palette,
    dispatch: &Dispatch,
) -> Div {
    let language = channel.language;
    let header = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(16.))
        .mx(px(8.))
        .px(px(8.))
        .pt(px(4.))
        .pb(px(12.))
        .border_b_1()
        .border_color(palette.border)
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
                    ellipsized_label(palette, TextRole::Label, channel.name.clone())
                        .debug_selector(|| "conversation-details-channel".into()),
                )
                .child(
                    label(
                        palette,
                        TextRole::Caption,
                        member_counts(language, channel.member_count, channel.online_count),
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
                            language,
                            palette,
                            dispatch,
                        ))
                        .children(
                            channel.selected_topic.iter().map(|topic| {
                                render_topic_context(topic, language, palette, dispatch)
                            }),
                        )
                        .children(channel.description.iter().map(|text| {
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(12.))
                                .child(
                                    label(
                                        palette,
                                        TextRole::Label,
                                        language.tr("chatInfo.description"),
                                    )
                                    .px(px(8.)),
                                )
                                .child(
                                    label(palette, TextRole::Body, text.clone())
                                        .rounded(px(8.))
                                        .bg(palette.surface)
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
fn render_topic_context(
    topic: &SelectedTopicView,
    language: Language,
    palette: Palette,
    dispatch: &Dispatch,
) -> Div {
    let text = |key| language.tr(key);
    let placeholder = if topic.summary_enabled {
        text("workspaceMessenger.topicSummary.pending")
    } else {
        text("workspaceMessenger.topicSummary.disabledEmpty")
    };
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .px(px(8.))
        .when(topic.done, |section| {
            section.child(
                div()
                    .child(
                        ellipsized_label(palette, TextRole::Label, topic.title.clone())
                            .debug_selector(|| "conversation-details-topic".into()),
                    )
                    .child(label(
                        palette,
                        TextRole::Caption,
                        text("desktop.conversation.channel.topicCompleted"),
                    )),
            )
        })
        .child(
            div()
                .flex()
                .h(px(24.))
                .items_center()
                .gap(px(6.))
                .child(
                    label(
                        palette,
                        TextRole::Label,
                        text("workspaceMessenger.topicSummary.title"),
                    )
                    .truncate(),
                )
                .child(label(palette, TextRole::Caption, "(AI ✨)").flex_none())
                .child(div().flex_1())
                .children(topic.summary_settings.as_ref().map(|focus| {
                    icon_button(
                        IconAction {
                            id: "details-summary-settings".into(),
                            icon: Icon::Settings,
                            description: text("topicSummarySettings.title"),
                            edge: 24.,
                            glyph: 16.,
                            availability: Availability::Enabled,
                            action: PanelAction::OpenSummarySettings,
                        },
                        Some(focus),
                        palette,
                        dispatch,
                    )
                }))
                .child(icon_button(
                    IconAction {
                        id: "details-summary-toggle".into(),
                        icon: if topic.expanded {
                            Icon::ChevronUp
                        } else {
                            Icon::ChevronDown
                        },
                        description: if topic.expanded {
                            text("workspaceMessenger.topicSummary.collapse")
                        } else {
                            text("workspaceMessenger.topicSummary.expand")
                        },
                        edge: 24.,
                        glyph: 16.,
                        availability: Availability::Enabled,
                        action: PanelAction::ToggleSummary,
                    },
                    None,
                    palette,
                    dispatch,
                )),
        )
        .when(topic.expanded, |section| {
            section.child(
                div()
                    .id("details-summary-scroll")
                    .debug_selector(|| "conversation-details-topic-summary".into())
                    .max_h(px(218.))
                    .overflow_y_scroll()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.surface)
                    .p(px(8.))
                    .child(render_summary(
                        topic.summary.as_deref(),
                        placeholder,
                        palette,
                    ))
                    .when(
                        topic.summary.is_some() && topic.summary_has_new_messages,
                        |summary| {
                            summary.child(
                                label(
                                    palette,
                                    TextRole::Caption,
                                    text("workspaceMessenger.topicSummary.hasNewMessages"),
                                )
                                .mt(px(8.))
                                .line_height(px(20.)),
                            )
                        },
                    )
                    .when(!topic.summary_enabled, |summary| {
                        summary.child(
                            label(
                                palette,
                                TextRole::Caption,
                                text("workspaceMessenger.topicSummary.disabled"),
                            )
                            .mt(px(8.))
                            .line_height(px(20.)),
                        )
                    }),
            )
        })
}

/// Рисует абзацы, пункты списка и начертания без обработки HTML.
fn render_summary(
    blocks: Option<&[TopicSummaryBlock]>,
    placeholder: &'static str,
    palette: Palette,
) -> Div {
    let Some(blocks) = blocks else {
        return label(palette, TextRole::Caption, placeholder)
            .line_height(px(20.))
            .text_color(palette.text_muted);
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .children(blocks.iter().map(|block| {
            text_role(palette, TextRole::Caption)
                .line_height(px(20.))
                .text_color(palette.text)
                .pl(px(f32::from(block.indent) * 10.))
                .child(StyledText::new(block.text.clone()).with_highlights(
                    block.styles.iter().map(|(range, style)| {
                        (
                            range.clone(),
                            HighlightStyle {
                                font_weight: style.bold.then_some(FontWeight::SEMIBOLD),
                                font_style: style.italic.then_some(FontStyle::Italic),
                                ..HighlightStyle::default()
                            },
                        )
                    }),
                ))
        }))
}

/// Формирует строку числа участников для шапки и сведений.
pub(in crate::gpui_client) fn member_counts(
    language: Language,
    members: usize,
    online: usize,
) -> String {
    let members = language.plural("channel.participants", members as u64);
    let online = language.plural("channel.online", online as u64);
    language.tr_with(
        "desktop.messenger.channel.memberCounts",
        &[("members", &members), ("online", &online)],
    )
}

/// Показывает состояние трёх уровней уведомлений и результат операции.
fn render_notifications(
    notifications: Option<&NotificationsView>,
    language: Language,
    palette: Palette,
    dispatch: &Dispatch,
) -> Div {
    let text = |key| language.tr(key);
    let title = div()
        .flex()
        .flex_wrap()
        .gap(px(4.))
        .px(px(8.))
        .child(label(
            palette,
            TextRole::Label,
            text("channel.notifications"),
        ))
        .child(
            label(
                palette,
                TextRole::Label,
                text("desktop.conversation.channel.forTheEntireChat"),
            )
            .text_color(palette.text_muted),
        );
    let Some(notifications) = notifications else {
        return div().child(title).child(
            label(
                palette,
                TextRole::Body,
                text("desktop.conversation.channel.notificationsUnavailable"),
            )
            .px(px(8.))
            .py(px(8.))
            .text_color(palette.text_muted),
        );
    };
    let levels = [
        (
            NotificationLevel::Default,
            "details-notification-mentions",
            text("desktop.conversation.channel.mentionsOnly"),
            Icon::At,
        ),
        (
            NotificationLevel::Muted,
            "details-notification-muted",
            text("channel.notificationMuted"),
            Icon::BellOff,
        ),
        (
            NotificationLevel::Subscribed,
            "details-notification-all",
            text("desktop.conversation.channel.allNotifications"),
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
            .rounded(px(8.))
            .border_1()
            .border_color(gpui::transparent_black())
            .when(is_selected, |segment| segment.bg(palette.card))
            .child(icon.element(
                px(28.),
                if is_selected {
                    palette.icon_hover
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
            segment.opacity(0.5)
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
                .rounded(px(8.))
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
        .rounded(px(4.))
        .border_1()
        .border_color(gpui::transparent_black())
        .hover(|style| style.bg(palette.card_hover))
        .child(label(
            palette,
            TextRole::Label,
            channel.language.tr("channel.topics"),
        ))
        .child(chevron.element(px(16.), palette.text_muted));
    let toggle = {
        let dispatch = dispatch.clone();
        activate(toggle, None, palette, move |window, app| {
            dispatch(PanelAction::ToggleTopics, window, app);
        })
    };
    let list = if channel.topics.is_empty() {
        label(
            palette,
            TextRole::Body,
            channel.language.tr("channel.noTopics"),
        )
        .px(px(8.))
        .py(px(8.))
        .text_color(palette.text_muted)
    } else {
        div()
            .overflow_hidden()
            .rounded(px(8.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .children(channel.topics.iter().enumerate().map(|(index, topic)| {
                div()
                    .when(index != 0, |row| {
                        row.border_t_1().border_color(palette.border)
                    })
                    .child(render_topic(topic, channel.language, palette, dispatch))
            }))
    };
    div()
        .child(toggle)
        .when(channel.topics_expanded, |section| section.child(list))
        .children(channel.topic_error.iter().map(|error| {
            label(palette, TextRole::Caption, error.clone())
                .px(px(8.))
                .text_color(palette.danger)
        }))
}

/// Рисует строку темы, которая открывает историю из текущего аккаунта.
fn render_topic(
    topic: &TopicView,
    language: Language,
    palette: Palette,
    dispatch: &Dispatch,
) -> Div {
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
            div()
                .id((gpui::ElementId::Uuid(id), "details-topic-notification"))
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .child(icon.element(px(16.), palette.text_muted))
                .tooltip(move |_, app| {
                    let description = if muted {
                        language.tr("desktop.conversation.channel.topicNotificationsMuted")
                    } else {
                        language.tr("desktop.conversation.channel.topicNotificationsOn")
                    };
                    tooltip(palette, description.into(), app)
                })
        }));
    let anchor = Rc::new(Cell::new(None));
    let keyboard_anchor = anchor.clone();
    let click = dispatch.clone();
    let menu = dispatch.clone();
    let keyboard_menu = dispatch.clone();
    let row = activate(row, Some(&topic.focus), palette, move |window, app| {
        click(PanelAction::OpenTopic(id), window, app);
    })
    .on_mouse_down(MouseButton::Right, move |event, window, app| {
        app.stop_propagation();
        menu(
            PanelAction::OpenTopicMenu {
                topic: id,
                anchor: event.position,
            },
            window,
            app,
        );
    })
    .on_key_down(move |event, window, app| {
        let key = &event.keystroke;
        if key.key == "f10"
            && key.modifiers.shift
            && let Some(anchor) = keyboard_anchor.get()
        {
            app.stop_propagation();
            keyboard_menu(
                PanelAction::OpenTopicMenu { topic: id, anchor },
                window,
                app,
            );
        }
    });
    div()
        .child(row)
        .on_children_prepainted(move |children, _, _| {
            if let Some(bounds) = children.first() {
                anchor.set(Some(point(bounds.left(), bounds.bottom())));
            }
        })
}

/// Рисует участников, действие добавления, пустой список и ошибку.
pub(super) fn render_members(channel: &ChannelView, palette: Palette, dispatch: &Dispatch) -> Div {
    let text = |key| channel.language.tr(key);
    let title = div()
        .flex()
        .h(px(32.))
        .items_center()
        .justify_between()
        .px(px(8.))
        .child(label(palette, TextRole::Label, text("channel.members")))
        .when(channel.add_members != Availability::Hidden, |title| {
            title.child(icon_button(
                IconAction {
                    id: "details-add-members".into(),
                    icon: Icon::PersonAdd,
                    description: text("channel.addMembers"),
                    edge: 32.,
                    glyph: 24.,
                    availability: channel.add_members,
                    action: PanelAction::AddMembers,
                },
                channel.add_members_focus.as_ref(),
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
            label(
                palette,
                TextRole::Body,
                text("desktop.conversation.channel.noMembers"),
            )
            .text_center()
            .text_color(palette.text_muted)
            .py(px(12.)),
        );
    } else {
        members = members.children(
            channel
                .members
                .iter()
                .map(|member| render_member(member, channel.language, palette, dispatch)),
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
fn render_member(
    member: &MemberView,
    language: Language,
    palette: Palette,
    dispatch: &Dispatch,
) -> Div {
    PersonRow {
        avatar: member.avatar.avatar(),
        name: member.name.clone(),
        secondary: member.secondary.clone(),
        role: member.role.clone(),
        selected: false,
    }
    .render(palette)
    .group("details-member")
    .when(member.remove != Availability::Hidden, |row| {
        row.child(
            icon_button(
                IconAction {
                    id: format!("details-remove-member-{}", member.id).into(),
                    icon: Icon::Close,
                    description: language.tr("desktop.conversation.channel.removeMember"),
                    edge: 24.,
                    glyph: 14.,
                    availability: member.remove,
                    action: PanelAction::RemoveMember(member.id),
                },
                None,
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

/// Размер и действие компактной кнопки в строке правой панели.
struct IconAction {
    id: SharedString,           // Стабильный ID кнопки и тестовой области.
    icon: Icon,                 // Символ действия.
    description: &'static str,  // Текст подсказки.
    edge: f32,                  // Размер области нажатия в логических пикселях.
    glyph: f32,                 // Размер символа в логических пикселях.
    availability: Availability, // Принимает ли кнопка ввод.
    action: PanelAction,        // Сигнал, который получает экран.
}

/// Рисует кнопку с геометрией референса и общим поведением фокуса.
fn icon_button(
    item: IconAction,
    focus: Option<&FocusHandle>,
    palette: Palette,
    dispatch: &Dispatch,
) -> Stateful<Div> {
    let IconAction {
        id,
        icon,
        description,
        edge,
        glyph,
        availability,
        action,
    } = item;
    let button = div()
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .flex()
        .flex_none()
        .size(px(edge))
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .border_color(gpui::transparent_black())
        .child(icon.element(px(glyph), palette.text_muted))
        .tooltip(move |_, app| tooltip(palette, description.into(), app));
    if availability != Availability::Enabled {
        return button.opacity(0.5);
    }
    let dispatch = dispatch.clone();
    activate(
        button.hover(|style| style.bg(palette.card_hover)),
        focus,
        palette,
        move |window, app| {
            dispatch(action.clone(), window, app);
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет подпись счётчика в шапке канала с формами русского и английского.
    #[test]
    fn member_count_uses_the_channel_language() {
        for (total, expected) in [
            (1, "1 участник, 1 в сети"),
            (3, "3 участника, 1 в сети"),
            (5, "5 участников, 1 в сети"),
            (11, "11 участников, 1 в сети"),
            (21, "21 участник, 1 в сети"),
        ] {
            assert_eq!(member_counts(Language::Ru, total, 1), expected);
        }
        assert_eq!(member_counts(Language::En, 1, 0), "1 member, 0 online");
        assert_eq!(member_counts(Language::En, 2, 1), "2 members, 1 online");
    }
}
