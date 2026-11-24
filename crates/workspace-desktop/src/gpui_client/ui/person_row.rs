//! Общая карточка человека для сведений о канале и выбора участников.

use gpui::{App, Div, ElementId, FocusHandle, SharedString, Stateful, Window, div, prelude::*, px};

use super::{
    Avatar, AvatarPresence, Icon, Palette, TextRole, activate, ellipsized_label, label,
    theme::CONTROL_BORDER,
};

/// Подписи, аватар и состояние выбора одной карточки.
pub struct PersonRow {
    pub avatar: Avatar,                  // Аватар, который подготовил экран.
    pub name: SharedString,              // Имя для основной строки.
    pub secondary: Option<SharedString>, // Статус или другая подпись под именем.
    pub role: Option<SharedString>,      // Роль рядом с именем, если она нужна на экране.
    pub selected: bool,                  // Показывать метку множественного выбора.
}

impl PersonRow {
    /// Рисует карточку человека и оставляет место для соседних действий.
    pub fn render(self, palette: Palette) -> Div {
        let Self {
            mut avatar,
            name,
            secondary,
            role,
            selected,
        } = self;
        let mut name_palette = palette;
        if selected {
            name_palette.text = palette.accent;
            // Метка выбора занимает место индикатора присутствия.
            avatar.presence = AvatarPresence::Hidden;
        }
        let avatar_box = div()
            .relative()
            .flex_none()
            .child(avatar.render_at(palette, px(32.), palette.surface))
            .when(selected, |avatar_box| {
                avatar_box
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .rounded_full()
                            .border(CONTROL_BORDER)
                            .border_color(palette.accent),
                    )
                    .child(
                        div()
                            .absolute()
                            .bottom(px(-2.))
                            .right(px(-2.))
                            .size(px(16.))
                            .rounded_full()
                            .bg(palette.accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::Check.element(px(11.), palette.on_accent))
                            .debug_selector(|| "person-row-selected".into()),
                    )
            });
        let name = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(div().min_w_0().flex_1().child(ellipsized_label(
                name_palette,
                TextRole::Body,
                name,
            )))
            .children(role.map(|role| {
                label(palette, TextRole::Caption, role)
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
            .hover(|style| style.bg(palette.card_hover))
            .child(avatar_box)
            .child(
                div().flex_1().min_w_0().child(name).children(
                    secondary.map(|text| ellipsized_label(palette, TextRole::Caption, text)),
                ),
            )
    }

    /// Позволяет выбрать человека мышью, Enter или пробелом.
    pub fn render_action(
        self,
        id: impl Into<ElementId>,
        palette: Palette,
        focus: Option<&FocusHandle>,
        on_activate: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        activate(
            self.render(palette)
                .id(id)
                .border(CONTROL_BORDER)
                .border_color(gpui::transparent_black()),
            focus,
            palette,
            on_activate,
        )
    }
}
