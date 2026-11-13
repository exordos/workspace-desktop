//! Переключатель Workspace. Здесь описаны его внешний вид и подключение к управлению с клавиатуры.

use std::{cell::Cell, rc::Rc};

use iced::alignment::Horizontal;
use iced::widget::button::{Status, Style};
use iced::widget::{Space, button, container, row, text};
use iced::{Border, Center, Color, Element, Shadow, Theme, Vector};

use super::CONTROL_TEXT_SIZE;
use super::button::keyboard_button;
use super::theme::text_muted;

/// Показывает переключатель с подписью и переданным состоянием.
/// При нажатии отправляет сообщение о новом состоянии. С `None` переключатель недоступен.
pub fn switch<'a, Message: Clone + 'a>(
    label: &'a str,
    is_on: bool,
    on_toggle: Option<impl FnOnce(bool) -> Message>,
) -> Element<'a, Message> {
    let on_press = on_toggle.map(|toggle| toggle(!is_on));
    let is_enabled = on_press.is_some();
    let keyboard_pressed = Rc::new(Cell::new(false));
    let pressed_for_style = Rc::clone(&keyboard_pressed);
    // Штатный Toggler рисует дорожку в пропорции 2:1. Размер в макете — 30 на 18.
    let thumb = container(Space::new())
        .width(14)
        .height(14)
        .style(move |_: &Theme| container::Style {
            background: Some(
                Color::WHITE
                    .scale_alpha(if is_enabled { 1.0 } else { 0.65 })
                    .into(),
            ),
            border: Border::default().rounded(7),
            shadow: Shadow {
                color: Color::BLACK.scale_alpha(if is_enabled { 0.15 } else { 0.0 }),
                offset: Vector::new(0.0, 2.0),
                blur_radius: 2.0,
            },
            ..container::Style::default()
        });
    let track = container(thumb)
        .width(30)
        .height(18)
        .padding(2)
        .align_y(Center)
        .align_x(if is_on {
            Horizontal::Right
        } else {
            Horizontal::Left
        })
        .style(move |theme| {
            let color = if is_on {
                theme.palette().primary
            } else {
                text_muted(theme)
            };
            container::Style {
                background: Some(
                    color
                        .scale_alpha(if is_enabled { 1.0 } else { 0.45 })
                        .into(),
                ),
                border: Border::default().rounded(9),
                ..container::Style::default()
            }
        });

    let inner = button(
        row![text(label).size(CONTROL_TEXT_SIZE), track]
            .align_y(Center)
            .spacing(8),
    )
    .padding([4, 6])
    .on_press_maybe(on_press.clone())
    .style(move |theme, status| {
        let palette = theme.palette();
        let background = match status {
            Status::Disabled => None,
            _ if pressed_for_style.get() || status == Status::Pressed => {
                Some(palette.primary.scale_alpha(0.18).into())
            }
            Status::Hovered => Some(palette.primary.scale_alpha(0.10).into()),
            _ => None,
        };
        Style {
            background,
            text_color: if status == Status::Disabled {
                text_muted(theme)
            } else {
                palette.text
            },
            border: Border::default().rounded(8),
            ..Style::default()
        }
    });
    keyboard_button(inner, on_press, keyboard_pressed)
}
