//! Рисует шапки личного чата, канала, темы и избранного с действиями разговора.

use gpui::{
    App, Context, Div, FocusHandle, FontWeight, HighlightStyle, Hsla, SharedString, Stateful,
    StyledText, Window, div, prelude::*, px, rgb,
};
use workspace_core::{
    directory::ProfilePresence,
    messenger::details::{ChannelSidebar, SidebarDetails},
};

use super::Conversation;
use crate::gpui_client::{
    preferences::{Language, Preferences},
    ui::{
        Avatar, AvatarPresence, Icon, Palette, TextRole, activate, label, popup::tooltip, text_role,
    },
};

impl Conversation {
    /// Собирает шапку из подтверждённых сведений о текущем разговоре.
    pub(super) fn render_heading(&self, palette: Palette, cx: &Context<Self>) -> Div {
        let locale = Preferences::read_for(cx, &self.address.route.owner).language;
        let heading = div()
            .debug_selector(|| "conversation-heading".into())
            .flex()
            .flex_none()
            .min_w_0()
            .items_center()
            .px(px(20.))
            .py(px(8.))
            .rounded(px(8.))
            .bg(palette.surface);
        if self.favorites_presentation {
            return heading.min_h(px(52.)).child(
                label(
                    palette,
                    TextRole::Heading,
                    locale.tr("activity.favorites"),
                )
                .debug_selector(|| "conversation-title".into()),
            );
        }

        let protected = !self.heading.direct
            && self.heading.encrypted.unwrap_or_else(|| {
                self.sending
                    .as_ref()
                    .is_some_and(|sending| sending.view.read(cx).known_protected_destination())
            });
        let channel = match self.heading_details.as_deref() {
            Some(SidebarDetails::Channel(channel)) => Some(channel),
            _ => None,
        };
        let name = channel
            .map(|channel| channel.name.as_str())
            .or(self.heading.chat.as_deref())
            .unwrap_or_else(|| locale.tr("desktop.conversation.heading.conversation"));
        let title = if self.heading.direct {
            self.direct_heading(name, locale, palette, cx)
        } else {
            self.channel_heading(name, channel, protected, locale, palette)
        };
        let details = self.details.as_ref().filter(|_| self.can_open_details());
        heading
            .when(protected, |heading| heading.pt(px(12.)))
            .child(
                div()
                    .flex()
                    .min_w_0()
                    .flex_1()
                    .items_center()
                    .child(
                        div()
                            .min_w_0()
                            .when(!protected, |block| block.flex_1())
                            .child(if let Some(panel) = details {
                                panel.title_trigger(title, palette, cx)
                            } else {
                                title
                            }),
                    )
                    .children(protected.then(|| protected_badge(locale, palette))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .gap(px(4.))
                    .children(self.call_trigger(palette, cx))
                    .children(
                        details
                            .map(|panel| panel.trigger(locale, self.heading.direct, palette, cx)),
                    ),
            )
    }

    /// Показывает аватар и актуальное присутствие собеседника.
    fn direct_heading(&self, fallback: &str, locale: Language, palette: Palette, cx: &App) -> Div {
        let profile = match self.heading_details.as_deref() {
            Some(SidebarDetails::Peer(peer))
                if Some(peer.user_id) == self.heading.direct_peer_id =>
            {
                Some(&peer.profile)
            }
            Some(SidebarDetails::Channel(channel)) if channel.direct_peer.is_some() => channel
                .members
                .iter()
                .find(|member| Some(member.user_id) == self.heading.direct_peer_id)
                .and_then(|member| member.profile.as_ref()),
            _ => None,
        };
        let own_chat = self
            .heading
            .direct_peer_id
            .is_some_and(|peer| self.address.route.owner.user_id.parse().ok() == Some(peer));
        let name = if own_chat {
            locale.tr("activity.favorites")
        } else {
            profile.map_or(fallback, |profile| profile.display_name.as_str())
        };
        let initial = name
            .chars()
            .next()
            .map(|first| first.to_uppercase().collect::<String>())
            .unwrap_or_default();
        let mut avatar = Avatar::from_initials(initial);
        avatar.image = self
            .heading
            .direct_peer_id
            .and_then(|id| self.heading_avatars.image(id, cx));
        let presence = profile.and_then(|profile| profile.presence);
        let (dot, presence_label) = match presence {
            Some(ProfilePresence::Active) => (AvatarPresence::Online, locale.tr("a11y.online")),
            Some(ProfilePresence::Idle) => (AvatarPresence::Away, locale.tr("a11y.away")),
            Some(ProfilePresence::DoNotDisturb) => {
                (AvatarPresence::Away, locale.tr("presence.doNotDisturb"))
            }
            _ => (AvatarPresence::Offline, locale.tr("presence.offline")),
        };
        avatar.presence = dot;
        let status = profile
            .and_then(|profile| profile.status_label())
            .unwrap_or_else(|| presence_label.to_owned());
        div()
            .flex()
            .items_center()
            .min_w_0()
            .gap(px(12.))
            .child(
                avatar
                    .render(palette)
                    .debug_selector(|| "conversation-heading-avatar".into()),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .child(
                        label(palette, TextRole::Label, name.to_owned())
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .debug_selector(|| "conversation-title".into()),
                    )
                    .child(
                        label(palette, TextRole::Caption, status)
                            .line_height(px(16.))
                            .text_color(palette.text_muted)
                            .truncate()
                            .debug_selector(|| "conversation-heading-presence".into()),
                    ),
            )
    }

    /// Показывает название темы и канала в одной строке, состав — на второй.
    fn channel_heading(
        &self,
        name: &str,
        channel: Option<&ChannelSidebar>,
        protected: bool,
        locale: Language,
        palette: Palette,
    ) -> Div {
        let topic = self
            .address
            .route
            .scope
            .topic_id
            .and_then(|id| channel?.topics.iter().find(|topic| topic.id == id))
            .map(|topic| topic.name.as_str())
            .or(self.heading.topic.as_deref());
        let title = if let Some(topic) = topic {
            let separator = if protected { "  " } else { " · " };
            let full = format!("{topic}{separator}{name}");
            let length = full.len();
            StyledText::new(full).with_highlights([(
                topic.len()..length,
                HighlightStyle {
                    color: Some(palette.text_muted),
                    font_weight: Some(FontWeight::NORMAL),
                    ..Default::default()
                },
            )])
        } else {
            StyledText::new(name.to_owned())
        };
        let color: Hsla = self
            .heading
            .color
            .map_or(palette.card, |color| rgb(color).into());
        let (members, online) = channel.map_or((0, 0), |channel| {
            (
                channel.members.len(),
                channel
                    .members
                    .iter()
                    .filter(|member| {
                        member.profile.as_ref().is_some_and(|profile| {
                            profile.presence == Some(ProfilePresence::Active)
                        })
                    })
                    .count(),
            )
        });
        div()
            .flex()
            .min_w_0()
            .items_center()
            .gap(px(12.))
            .children(protected.then(|| {
                div()
                    .size(px(32.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(color)
                    .child(Icon::ChatLock.element(px(20.), palette.text))
            }))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .child(
                        text_role(palette, TextRole::Label)
                            .font_weight(if protected {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::SEMIBOLD
                            })
                            .when(protected, |title| title.text_size(px(16.)))
                            .w_full()
                            .truncate()
                            .debug_selector(|| "conversation-title".into())
                            .child(title),
                    )
                    .child(
                        label(
                            palette,
                            TextRole::Caption,
                            super::details::member_counts(locale, members, online),
                        )
                        .mt(px(2.))
                        .line_height(px(16.))
                        .text_color(palette.text_muted)
                        .truncate()
                        .debug_selector(|| "conversation-heading-members".into()),
                    ),
            )
    }
}

/// Показывает подсказку о защищённом режиме рядом с названием канала.
fn protected_badge(locale: Language, palette: Palette) -> Stateful<Div> {
    div()
        .id("conversation-protected-info")
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .mx(px(8.))
        .child(Icon::Info.element(px(14.), palette.accent))
        .child(label(palette, TextRole::Body, locale.tr("e2ee.protectedChat")).text_color(palette.accent))
        .tooltip(move |_, app| tooltip(palette, locale.tr("desktop.conversation.heading.messagesUseEndToEndEncryptionAttachmentsAreStored").into(), app))
}

/// Создаёт компактную кнопку шапки с подсказкой и клавиатурным фокусом.
pub(super) fn icon_button(
    id: &'static str,
    icon: Icon,
    hint: SharedString,
    focus: &FocusHandle,
    disabled: bool,
    palette: Palette,
    on_press: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let glyph = icon
        .element(
            px(20.),
            if disabled {
                palette.disabled
            } else {
                palette.text_muted
            },
        )
        .when(!disabled, |glyph| {
            glyph.group_hover(id, |style| style.text_color(palette.text))
        });
    let button = div()
        .id(id)
        .group(id)
        .debug_selector(move || id.into())
        .size(px(36.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .border_1()
        .border_color(gpui::transparent_black())
        .child(glyph)
        .tooltip(move |_, app| tooltip(palette, hint.clone(), app));
    if disabled {
        button
    } else {
        activate(
            button.hover(|style| style.bg(palette.card_selected)),
            Some(focus),
            palette,
            on_press,
        )
    }
}
