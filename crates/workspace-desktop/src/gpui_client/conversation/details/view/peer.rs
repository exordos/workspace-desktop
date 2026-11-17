//! Карточка личного чата: аватар, имя, действия и поля профиля.

use gpui::{Div, div, prelude::*, px};

use super::{Availability, Dispatch, PanelAction, PeerView, ProfileField};
use crate::gpui_client::ui::{
    AvatarPresence, Button, ButtonAppearance, Icon, Palette, TextRole, activate, ellipsized_label,
    label, popup::tooltip,
};

/// Рисует карточку, которую можно прокрутить вместе со всеми строками профиля.
pub(super) fn render(peer: &PeerView, palette: Palette, dispatch: &Dispatch) -> Div {
    let avatar = peer.avatar.avatar();
    let presence_color = match avatar.presence {
        AvatarPresence::Online => palette.presence_online,
        AvatarPresence::Away => palette.presence_away,
        AvatarPresence::Hidden => palette.text_secondary,
    };
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
                        .map(|text| ellipsized_label(palette, TextRole::Caption, text)),
                )
                .children(peer.presence.iter().cloned().map(|text| {
                    label(palette, TextRole::Caption, text).text_color(presence_color)
                })),
        );
    let share = if peer.actions.share_copied {
        ("Скопировано", Icon::Check, 28.)
    } else {
        ("Поделиться", Icon::Links, 40.)
    };
    let actions = [
        (
            peer.actions.message,
            "details-peer-message",
            "Написать",
            Icon::Chat,
            28.,
            PanelAction::MessagePeer,
        ),
        (
            peer.actions.call,
            "details-peer-call",
            "Позвонить",
            Icon::Phone,
            30.,
            PanelAction::CallPeer,
        ),
        (
            peer.actions.favorites,
            "details-peer-favorites",
            "Избранное",
            Icon::Home,
            26.,
            PanelAction::Favorites,
        ),
        (
            peer.actions.edit,
            "details-peer-edit",
            "Редактировать",
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
                let chip = div()
                    .id(id)
                    .debug_selector(move || id.into())
                    .when(id != "details-peer-edit", |chip| chip.flex_1())
                    .when(id == "details-peer-edit", |chip| chip.flex_none())
                    .min_w_0()
                    .h(px(72.))
                    .px(px(8.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .rounded_md()
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
                            .w_full()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_center()
                            .line_height(px(16.)),
                    );
                Some(match availability {
                    Availability::Enabled => {
                        let dispatch = dispatch.clone();
                        activate(
                            chip.hover(|style| style.bg(palette.card_hover)),
                            None,
                            palette,
                            move |window, app| {
                                dispatch(action.clone(), window, app);
                            },
                        )
                    }
                    _ => chip.opacity(0.5).tooltip(move |_, app| {
                        tooltip(palette, "Действие пока недоступно".into(), app)
                    }),
                })
            });
    let content = div()
        .flex()
        .flex_col()
        .gap(px(20.))
        .py(px(8.))
        .child(header)
        .when(has_actions, |content| {
            content.child(div().flex().gap(px(8.)).children(visible_actions))
        })
        .child(
            div().flex().flex_col().gap(px(12.)).children(
                peer.fields
                    .iter()
                    .enumerate()
                    .map(|(index, field)| render_field(field, index, palette, dispatch)),
            ),
        );
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

/// Рисует значение профиля и действие копирования для доступного поля.
fn render_field(field: &ProfileField, index: usize, palette: Palette, dispatch: &Dispatch) -> Div {
    let value = if field.multiline {
        label(palette, TextRole::Body, field.value.clone())
    } else if field.value.is_empty() {
        label(palette, TextRole::Body, "–")
    } else {
        ellipsized_label(palette, TextRole::Body, field.value.clone())
    };
    let row = div()
        .flex()
        .items_center()
        .min_h(px(36.))
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
                .child(ellipsized_label(
                    palette,
                    TextRole::Caption,
                    field.label.clone(),
                ))
                .child(value),
        );
    if field.copy == Availability::Hidden {
        return row;
    }
    let mut copy = Button::new(("details-profile-copy", index), "");
    copy.icon = Some(Icon::Copy);
    copy.appearance = ButtonAppearance::Ghost;
    copy.disabled = field.copy == Availability::Disabled;
    let dispatch = dispatch.clone();
    let value = field.value.clone();
    row.child(
        copy.render(palette, None, move |window, app| {
            dispatch(PanelAction::Copy(value.clone()), window, app);
        })
        .flex_none()
        .tooltip(move |_, app| tooltip(palette, "Копировать".into(), app)),
    )
}
