//! Общая рамка экранов «Моей активности» и экран входящих.

use gpui::{App, Div, IntoElement, Window, div, prelude::*};

use super::{
    contracts::ActivityPage,
    preferences::Language,
    ui::{Button, ButtonAppearance, Icon, PANEL_RADIUS, Palette, TextRole, label, popup::tooltip},
};

pub(super) mod drafts;
pub(super) mod inbox;
pub(super) mod messages;

impl ActivityPage {
    /// Возвращает стабильный ключ пункта для кнопок и проверок окна.
    pub(super) const fn id(self) -> &'static str {
        match self {
            Self::Inbox => "activity-inbox",
            Self::Favorites => "activity-favorites",
            Self::Marked => "activity-marked",
            Self::Mentions => "activity-mentions",
            Self::Drafts => "activity-drafts",
        }
    }

    /// Возвращает подпись пункта на языке активного входа.
    pub(super) fn title(self, language: Language) -> &'static str {
        match self {
            Self::Inbox => language.tr("inbox.title"),
            Self::Favorites => language.tr("activity.favorites"),
            Self::Marked => language.tr("activity.starred"),
            Self::Mentions => language.tr("activity.mentions"),
            Self::Drafts => language.tr("activity.drafts"),
        }
    }
}

/// Показывает заголовок, переход к поиску чатов и содержимое страницы.
pub(super) fn frame(
    page: ActivityPage,
    palette: Palette,
    language: Language,
    body: impl IntoElement,
    search: impl Fn(&mut Window, &mut App) + 'static,
) -> Div {
    let mut button = Button::new("activity-search", "");
    button.icon = Some(Icon::Search);
    button.appearance = ButtonAppearance::Ghost;
    let search_label = language.tr("hotkeys.actions.navigation.chatSearch");
    div()
        .flex()
        .flex_col()
        .flex_1()
        .h_full()
        .min_h_0()
        .min_w_0()
        .overflow_hidden()
        .debug_selector(|| "activity-page".into())
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(PANEL_RADIUS)
                .bg(palette.surface)
                .child(label(palette, TextRole::Heading, page.title(language)).min_w_0())
                .when(page != ActivityPage::Favorites, |header| {
                    header.child(
                        button
                            .render(palette, None, search)
                            .tooltip(move |_, app| tooltip(palette, search_label.into(), app)),
                    )
                }),
        )
        .child(body)
}

/// Оформляет загрузку, ошибку и пустое состояние на месте списка.
pub(super) fn status(palette: Palette, message: &'static str, error: bool) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .flex_none()
        .p_3()
        .rounded(PANEL_RADIUS)
        .border_1()
        .border_color(palette.border)
        .bg(palette.surface)
        .child(
            label(palette, TextRole::Label, message).text_color(if error {
                palette.danger
            } else {
                palette.text_muted
            }),
        )
}
