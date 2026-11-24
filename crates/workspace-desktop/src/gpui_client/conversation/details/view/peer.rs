//! Карточка личного чата: аватар, имя, действия и поля профиля.

use gpui::{Div, div, prelude::*, px};
use workspace_core::directory::ProfilePresence;

use super::{Availability, Dispatch, PanelAction, PeerView, ProfileField};
use crate::gpui_client::{
    preferences::Language,
    ui::{Icon, Palette, TextRole, activate, ellipsized_label, label, popup::tooltip},
};

/// Рисует карточку, которую можно прокрутить вместе со всеми строками профиля.
pub(super) fn render(
    peer: &PeerView,
    gallery: Option<Div>,
    palette: Palette,
    dispatch: &Dispatch,
) -> Div {
    let avatar = peer.avatar.avatar();
    let language = peer.language;
    let header = div()
        .flex()
        .items_center()
        .gap(px(16.))
        .px(px(8.))
        .child(avatar.render_at(palette, px(64.), palette.surface))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h(px(64.))
                .flex()
                .flex_col()
                .justify_between()
                .child(
                    ellipsized_label(palette, TextRole::ProfileHeading, peer.name.clone())
                        .debug_selector(|| "conversation-details-peer".into()),
                )
                .children(
                    peer.custom_status
                        .iter()
                        .cloned()
                        .map(|text| ellipsized_label(palette, TextRole::Caption, text).h(px(16.))),
                )
                .children(
                    peer.presence
                        .map(|presence| presence_label(presence, language, palette)),
                ),
        );
    let share = if peer.actions.share_copied {
        (language.tr("message.copied"), Icon::Check, 28.)
    } else {
        (language.tr("info.share"), Icon::Links, 40.)
    };
    let actions = [
        (
            peer.actions.message,
            "details-peer-message",
            language.tr("info.openDirectMessages"),
            Icon::ChatBubble,
            28.,
            PanelAction::MessagePeer,
        ),
        (
            peer.actions.call,
            "details-peer-call",
            language.tr("call.call"),
            Icon::Phone,
            30.,
            PanelAction::CallPeer,
        ),
        (
            peer.actions.favorites,
            "details-peer-favorites",
            language.tr("common.favorites"),
            Icon::Home,
            26.,
            PanelAction::Favorites,
        ),
        (
            peer.actions.edit,
            "details-peer-edit",
            language.tr("info.edit"),
            Icon::Stylus,
            30.,
            PanelAction::EditPeer,
        ),
        (
            peer.actions.share,
            "details-peer-share",
            share.0,
            share.1,
            share.2,
            PanelAction::SharePeer,
        ),
    ];
    let has_actions = actions
        .iter()
        .any(|(availability, ..)| *availability != Availability::Hidden);
    let visible_actions =
        actions
            .into_iter()
            .filter_map(|(availability, id, caption, icon, size, action)| {
                if availability == Availability::Hidden {
                    return None;
                }
                let hug = id == "details-peer-edit";
                let chip = div()
                    .id(id)
                    .debug_selector(move || id.into())
                    .when(!hug, |chip| chip.flex_1())
                    .when(hug, |chip| chip.flex_none())
                    .min_w_0()
                    .h(px(72.))
                    .px(px(8.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .bg(palette.card)
                    .child(
                        div()
                            .size(px(40.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon.element(px(size), palette.text)),
                    )
                    .child(
                        label(palette, TextRole::Caption, caption)
                            .max_w_full()
                            .text_center()
                            .line_height(px(16.))
                            .whitespace_nowrap()
                            .overflow_hidden(),
                    );
                Some(match availability {
                    Availability::Enabled => {
                        let dispatch = dispatch.clone();
                        activate(
                            chip.hover(|style| style.bg(palette.background)),
                            None,
                            palette,
                            move |window, app| {
                                dispatch(action.clone(), window, app);
                            },
                        )
                    }
                    _ => chip.opacity(0.5).tooltip(move |_, app| {
                        tooltip(
                            palette,
                            language
                                .tr("desktop.messenger.history.keyActionUnavailable")
                                .into(),
                            app,
                        )
                    }),
                })
            });
    let content =
        div()
            .flex()
            .flex_col()
            .gap(px(20.))
            .py(px(8.))
            .child(header)
            .when(has_actions, |content| {
                content.child(div().flex().gap(px(8.)).children(visible_actions))
            })
            .child(div().flex().flex_col().gap(px(12.)).children(
                peer.fields.iter().enumerate().map(|(index, field)| {
                    render_field(
                        field,
                        index,
                        peer.copied_fields.contains(&index),
                        language,
                        palette,
                        dispatch,
                    )
                }),
            ))
            .children(gallery);
    div().flex().flex_col().flex_1().min_h_0().child(
        div()
            .id("details-peer-scroll")
            .flex_1()
            .min_h_0()
            .overflow_x_hidden()
            .overflow_y_scroll()
            .child(content),
    )
}

/// Показывает присутствие с тем же цветом, что используется в оригинальном профиле.
fn presence_label(presence: ProfilePresence, language: Language, palette: Palette) -> Div {
    let (caption, color) = match presence {
        ProfilePresence::Active => (language.tr("a11y.online"), palette.presence_online),
        ProfilePresence::Idle => (language.tr("a11y.away"), palette.presence_away),
        ProfilePresence::Offline => (language.tr("presence.offline"), palette.text_muted),
        ProfilePresence::DoNotDisturb => (language.tr("presence.doNotDisturb"), palette.danger),
    };
    label(palette, TextRole::Caption, caption)
        .line_height(px(16.))
        .text_color(color)
        .debug_selector(|| "conversation-details-peer-presence".into())
}

/// Рисует значение профиля и действие копирования для доступного поля.
fn render_field(
    field: &ProfileField,
    index: usize,
    copied: bool,
    language: Language,
    palette: Palette,
    dispatch: &Dispatch,
) -> Div {
    let value = if field.multiline {
        label(palette, TextRole::Body, field.value.clone())
    } else if field.value.is_empty() {
        label(
            palette,
            TextRole::Body,
            language.tr("workspaceMessenger.temporarilyNotConnected"),
        )
        .line_height(px(16.))
        .italic()
        .text_color(palette.text_muted)
    } else {
        ellipsized_label(palette, TextRole::Body, field.value.clone()).h(px(16.))
    };
    let row = div()
        .flex()
        .min_h(px(36.))
        .when(!field.multiline, |row| row.items_end().h(px(36.)))
        .when(field.multiline, |row| row.items_start())
        .gap(px(12.))
        .child(
            div()
                .size(px(32.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .child(field.icon.element(px(24.), palette.icon)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(ellipsized_label(palette, TextRole::Caption, field.label.clone()).h(px(16.)))
                .child(value),
        );
    if field.copy == Availability::Hidden {
        return row;
    }
    let (icon, description) = if copied {
        (Icon::Check, language.tr("message.copied"))
    } else {
        (Icon::Copy, language.tr("composer.copy"))
    };
    let copy = div()
        .id(("details-profile-copy", index))
        .debug_selector(move || {
            if copied {
                format!("details-profile-copied-{index}")
            } else {
                format!("details-profile-copy-{index}")
            }
        })
        .size(px(24.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .border_color(gpui::transparent_black())
        .child(icon.element(px(24.), palette.text_muted));
    if field.copy != Availability::Enabled {
        return row.child(copy.opacity(0.5));
    }
    let dispatch = dispatch.clone();
    let value = field.value.clone();
    row.child(
        activate(
            copy.hover(|style| style.bg(palette.background)),
            None,
            palette,
            move |window, app| {
                dispatch(
                    PanelAction::Copy {
                        field: index,
                        value: value.clone(),
                    },
                    window,
                    app,
                );
            },
        )
        .tooltip(move |_, app| tooltip(palette, description.into(), app)),
    )
}
