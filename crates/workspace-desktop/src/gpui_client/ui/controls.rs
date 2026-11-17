//! Кнопка и оболочка поля с общей геометрией, фокусом и состояниями оформления.

use gpui::{
    App, Div, ElementId, FocusHandle, IntoElement, SharedString, Stateful, Window, div, prelude::*,
    px,
};

use super::{Icon, Palette, TextRole, label};

/// Вид кнопки в зависимости от её роли в интерфейсе.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ButtonAppearance {
    #[default]
    Neutral, // Обычное действие на карточке.
    Primary, // Основное действие, например отправка.
    Ghost,   // Действие без подложки в обычном состоянии.
}

/// Кнопка с подписью и необязательной иконкой. Доступность решает экран.
pub struct Button {
    pub id: ElementId,       // Устойчивый идентификатор для событий и фокуса GPUI.
    pub label: SharedString, // Текст на кнопке. Пустой текст оставляет только иконку.
    pub appearance: ButtonAppearance, // Роль кнопки в интерфейсе.
    pub icon: Option<Icon>,  // Декоративная иконка перед текстом.
    pub disabled: bool,      // Недоступная кнопка пропускает указатель и клавиатуру.
}

impl Button {
    /// Готовит обычную текстовую кнопку с указанным идентификатором.
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: text.into(),
            appearance: ButtonAppearance::Neutral,
            icon: None,
            disabled: false,
        }
    }

    /// Рисует кнопку и вызывает действие при клике, Enter или пробеле.
    pub fn render(
        self,
        palette: Palette,
        focus: Option<&FocusHandle>,
        on_activate: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        let background = match self.appearance {
            ButtonAppearance::Neutral => palette.card,
            ButtonAppearance::Primary => palette.accent,
            ButtonAppearance::Ghost => gpui::transparent_black(),
        };
        let foreground = match (self.disabled, self.appearance) {
            (true, _) => palette.text_muted,
            (false, ButtonAppearance::Primary) => palette.on_accent,
            (false, _) => palette.text,
        };
        let icon_color = if self.disabled {
            palette.disabled
        } else {
            foreground
        };
        let hover = match self.appearance {
            ButtonAppearance::Primary => palette.accent.opacity(0.85),
            ButtonAppearance::Neutral | ButtonAppearance::Ghost => palette.card_hover,
        };
        let button = div()
            .id(self.id)
            .h(px(32.))
            .min_w(px(32.))
            .px(px(8.))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .rounded(px(8.))
            .border_1()
            .border_color(gpui::transparent_black())
            .bg(
                if self.disabled && self.appearance == ButtonAppearance::Primary {
                    palette.card
                } else {
                    background
                },
            )
            .children(self.icon.map(|icon| icon.element(px(16.), icon_color)))
            .when(!self.label.is_empty(), |button| {
                button.child(
                    label(palette, TextRole::Label, self.label)
                        .text_color(foreground)
                        .min_w_0()
                        .whitespace_nowrap()
                        .overflow_hidden(),
                )
            });
        if self.disabled {
            button
        } else {
            activate(
                button
                    .hover(|style| style.bg(hover))
                    .active(|style| style.bg(hover.opacity(0.78))),
                focus,
                palette,
                on_activate,
            )
        }
    }
}

/// Подключает одинаковые события мыши и клавиатуры у кнопок и строк.
pub(super) fn activate(
    element: Stateful<Div>,
    focus: Option<&FocusHandle>,
    palette: Palette,
    on_activate: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    // GPUI преобразует Enter и пробел у сфокусированного элемента в событие клика.
    element
        .map(|element| match focus {
            Some(focus) => element.track_focus(&focus.clone().tab_stop(true)),
            None => element.focusable(),
        })
        .tab_stop(true)
        .focus(|style| style.border_color(palette.text))
        .cursor_pointer()
        .on_click(move |_, window, cx| on_activate(window, cx))
}

/// Визуальное состояние оболочки поля.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FieldAppearance {
    #[default]
    Normal, // Обычное поле.
    Focused,  // Вложенный редактор получил клавиатурный фокус.
    Invalid,  // Экран передал ошибку ввода.
    Disabled, // Экран заблокировал изменение значения.
}

/// Рисует оболочку вокруг элемента ввода. Текстом и фокусом владеет экран.
pub fn field(palette: Palette, appearance: FieldAppearance, control: impl IntoElement) -> Div {
    let border = match appearance {
        FieldAppearance::Normal | FieldAppearance::Disabled => palette.border,
        FieldAppearance::Focused => palette.accent,
        FieldAppearance::Invalid => palette.danger,
    };
    div()
        .min_w_0()
        .min_h(px(32.))
        .px(px(8.))
        .flex()
        .items_center()
        .rounded(px(8.))
        .border_1()
        .border_color(border)
        .bg(palette.field)
        .text_color(match appearance {
            FieldAppearance::Disabled => palette.text_muted,
            _ => palette.text,
        })
        .overflow_hidden()
        .child(control)
}
