//! Кнопка и оболочка поля с общей геометрией, фокусом и состояниями оформления.

use gpui::{
    App, Div, ElementId, FocusHandle, Hsla, IntoElement, PathBuilder, Pixels, SharedString,
    Stateful, Window, canvas, div, point, prelude::*, px,
};

use super::{
    Icon, Palette, TextRole,
    icons::BUTTON_ICON_SIZE,
    label,
    theme::{COMPACT_CONTROL_INSET, COMPACT_CONTROL_SIZE, CONTROL_BORDER, CONTROL_RADIUS},
};

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
        self.render_event(palette, focus, move |_, window, app| {
            on_activate(window, app)
        })
    }

    /// Передаёт источнику клавиатурный или мышиный клик, если для фокуса они различаются.
    pub(in crate::gpui_client) fn render_event(
        self,
        palette: Palette,
        focus: Option<&FocusHandle>,
        on_activate: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
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
        let (hover, pressed) = match self.appearance {
            ButtonAppearance::Primary => (palette.accent.opacity(0.9), palette.accent.opacity(0.8)),
            ButtonAppearance::Neutral | ButtonAppearance::Ghost => {
                (palette.card_hover, palette.card_hover.opacity(0.78))
            }
        };
        let button = div()
            .id(self.id)
            .h(COMPACT_CONTROL_SIZE)
            .min_w(COMPACT_CONTROL_SIZE)
            .px(COMPACT_CONTROL_INSET)
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .rounded(CONTROL_RADIUS)
            .border(CONTROL_BORDER)
            .border_color(gpui::transparent_black())
            .bg(
                if self.disabled && self.appearance == ButtonAppearance::Primary {
                    palette.card
                } else {
                    background
                },
            )
            .children(
                self.icon
                    .map(|icon| icon.element(BUTTON_ICON_SIZE, icon_color)),
            )
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
            activate_click(
                button
                    .hover(|style| style.bg(hover))
                    .active(|style| style.bg(pressed)),
                focus,
                palette,
                on_activate,
            )
        }
    }
}

/// Подключает одинаковые события мыши и клавиатуры у кнопок и строк.
pub(in crate::gpui_client) fn activate(
    element: Stateful<Div>,
    focus: Option<&FocusHandle>,
    palette: Palette,
    on_activate: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    activate_click(element, focus, palette, move |_, window, app| {
        on_activate(window, app)
    })
}

/// Сохраняет тип клика у тех элементов, которые управляют переходом фокуса.
fn activate_click(
    element: Stateful<Div>,
    focus: Option<&FocusHandle>,
    palette: Palette,
    on_activate: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    // GPUI преобразует Enter и пробел у сфокусированного элемента в событие клика.
    element
        .map(|element| match focus {
            Some(focus) => element.track_focus(&focus.clone().tab_stop(true)),
            None => element.focusable(),
        })
        .tab_stop(true)
        .when(palette.keyboard_focus_visible(), |element| {
            element.focus(|style| style.border_color(palette.focus))
        })
        .cursor_pointer()
        .on_click(on_activate)
}

/// Визуальное состояние оболочки поля.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FieldAppearance {
    #[default]
    Normal, // Обычное поле.
    Focused,  // Вложенный редактор готов принимать ввод.
    Invalid,  // Экран передал ошибку ввода.
    Disabled, // Экран заблокировал изменение значения.
}

/// Рисует оболочку вокруг элемента ввода. Текстом и фокусом владеет экран.
pub fn field(palette: Palette, appearance: FieldAppearance, control: impl IntoElement) -> Div {
    let border = match appearance {
        FieldAppearance::Focused if palette.keyboard_focus_visible() => palette.focus,
        FieldAppearance::Normal | FieldAppearance::Disabled | FieldAppearance::Focused => {
            palette.border
        }
        FieldAppearance::Invalid => palette.danger,
    };
    div()
        .min_w_0()
        .min_h(COMPACT_CONTROL_SIZE)
        .px(COMPACT_CONTROL_INSET)
        .flex()
        .items_center()
        .rounded(CONTROL_RADIUS)
        .border(CONTROL_BORDER)
        .border_color(border)
        .bg(palette.field)
        .text_color(match appearance {
            FieldAppearance::Disabled => palette.text_muted,
            _ => palette.text,
        })
        .overflow_hidden()
        .child(control)
}

/// Закрывает четыре внешних угла цветом окружающей поверхности.
pub(in crate::gpui_client) fn rounded_surface_corners(
    radius: Pixels,
    surface: Hsla,
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            // GPUI ограничивает переполнение прямоугольником, поэтому закрываем углы отдельно.
            let control = radius * 0.447_715_25;
            let mut shape = PathBuilder::fill();
            for (corner, dx, dy) in [
                (bounds.origin, 1., 1.),
                (bounds.top_right(), -1., 1.),
                (bounds.bottom_left(), 1., -1.),
                (bounds.bottom_right(), -1., -1.),
            ] {
                let at = |x: Pixels, y: Pixels| point(corner.x + x * dx, corner.y + y * dy);
                shape.move_to(corner);
                shape.line_to(at(radius, px(0.)));
                shape.cubic_bezier_to(at(px(0.), radius), at(control, px(0.)), at(px(0.), control));
                shape.close();
            }
            if let Ok(path) = shape.build() {
                window.paint_path(path, surface);
            }
        },
    )
    .absolute()
    .inset_0()
    .size_full()
}
