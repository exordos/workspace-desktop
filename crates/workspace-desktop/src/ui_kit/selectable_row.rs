//! Выбираемая строка. Она подсвечивает переданное состояние и получает события мыши и клавиатуры.

use std::{cell::Cell, rc::Rc};

use iced::widget::button::{Status, Style};
use iced::widget::{button, container};
use iced::{Border, Element, Fill};

use super::CONTROL_RADIUS;
use super::button::keyboard_button;
use super::theme::button_colors;

/// Показывает строку на всю ширину и отправляет `on_select` при активации.
/// Внутренние элементы служат оформлением. При `None` строка недоступна.
pub fn selectable_row<'a, Message: Clone + 'a>(
    content: impl Into<Element<'a, Message>>,
    selected: bool,
    on_select: Option<Message>,
) -> Element<'a, Message> {
    let keyboard_pressed = Rc::new(Cell::new(false));
    let pressed_for_style = Rc::clone(&keyboard_pressed);
    let content = container(content).width(Fill).clip(true);
    let inner = button(content)
        .width(Fill)
        .padding([8, 12])
        .on_press_maybe(on_select.clone())
        .style(move |theme, status| {
            let colors = button_colors(theme);
            let disabled = status == Status::Disabled;
            let pressed = status == Status::Pressed || pressed_for_style.get();
            let opacity = if disabled { 0.55 } else { 1.0 };
            let background = if selected || pressed {
                Some(colors.card_active)
            } else if status == Status::Hovered {
                Some(colors.card)
            } else {
                None
            };
            Style {
                background: background.map(|color| color.scale_alpha(opacity).into()),
                text_color: if disabled {
                    colors.text_muted
                } else {
                    theme.palette().text
                },
                border: Border::default().rounded(CONTROL_RADIUS),
                ..Style::default()
            }
        });
    keyboard_button(inner, on_select, keyboard_pressed)
}
