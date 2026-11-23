//! Фиксированные данные для отдельных кадров компонентов и интерактивной галереи.

use gpui::{App, Div, Window, div, prelude::*, px};
use workspace_desktop::gpui_client::ui::{
    Avatar, AvatarPresence, Button, ButtonAppearance, FieldAppearance, FilterGroup, Palette,
    PersonRow, TextRole, dialog_card, field, filter_button, label,
};

/// Показывает обычную строку, строку с выбором и длинное имя без аватара.
pub fn people(
    palette: Palette,
    selected: [bool; 3],
    on_pick: impl Fn(usize, &mut Window, &mut App) + Clone + 'static,
) -> Div {
    let people = [
        ("Анна Котова", "АК", "В сети", None, AvatarPresence::Online),
        ("Михаил Петров", "МП", "Инженер", None, AvatarPresence::Away),
        (
            "Александра с длинным отображаемым именем",
            "",
            "Участник команды",
            Some("Автор"),
            AvatarPresence::Hidden,
        ),
    ];
    div()
        .flex()
        .flex_col()
        .gap_1()
        .children(people.into_iter().enumerate().map(
            |(index, (name, initials, secondary, role, presence))| {
                let mut avatar = Avatar::from_initials(initials);
                avatar.presence = presence;
                let pick = on_pick.clone();
                PersonRow {
                    avatar,
                    name: name.into(),
                    secondary: Some(secondary.into()),
                    role: role.map(Into::into),
                    selected: selected[index],
                }
                .render_action(
                    ("scene-person", index),
                    palette,
                    None,
                    move |window, app| {
                        pick(index, window, app);
                    },
                )
            },
        ))
}

/// Показывает порядок, обычное состояние и выбранный фильтр.
pub fn channels(
    palette: Palette,
    selected: usize,
    group: &FilterGroup,
    window: &Window,
    on_pick: impl Fn(usize, &mut Window, &mut App) + Clone + 'static,
) -> Div {
    div().bg(palette.surface).child(
        group.render(
            palette,
            window,
            ["С подпиской", "Без подписки", "Все", "Архив"]
                .into_iter()
                .enumerate()
                .map(|(index, title)| {
                    let pick = on_pick.clone();
                    filter_button(
                        ("scene-filter", index),
                        title,
                        index == selected,
                        palette,
                        Some(group.focus(index)),
                        move |window, app| pick(index, window, app),
                    )
                }),
        ),
    )
}

/// Показывает заголовок, поля и закреплённые действия общей модальной карточки.
pub fn modal(palette: Palette, window: &Window) -> Div {
    let mut create = Button::new("scene-dialog-confirm", "Создать");
    create.appearance = ButtonAppearance::Primary;
    div().child(
        dialog_card(
            "scene-dialog",
            "Создать канал",
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(label(palette, TextRole::Caption, "Название канала"))
                        .child(field(
                            palette,
                            FieldAppearance::Normal,
                            label(palette, TextRole::Body, "Проектная команда"),
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(label(
                            palette,
                            TextRole::Caption,
                            "Описание (необязательно)",
                        ))
                        .child(field(palette, FieldAppearance::Normal, div())),
                ),
            div()
                .flex()
                .gap_2()
                .child(create.render(palette, None, |_, _| {}).h(px(40.)))
                .child(
                    Button::new("scene-dialog-cancel", "Отменить")
                        .render(palette, None, |_, _| {})
                        .h(px(40.)),
                ),
            palette,
            window,
        )
        .max_w_full(),
    )
}
