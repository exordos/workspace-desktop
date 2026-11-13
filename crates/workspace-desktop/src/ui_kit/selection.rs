//! Круглый индикатор выбора и чекбокс на его основе. Здесь заданы два размера
//! индикатора и подключено управление чекбоксом мышью и клавиатурой.

use std::{cell::Cell, rc::Rc};

use iced::widget::button::{Status, Style};
use iced::widget::{button, container, row, space, text};
use iced::{Border, Center, Color, Element, Theme};

use super::button::keyboard_button;
use super::icon::{IconName, IconTone, icon};
use super::theme::button_colors;
use super::{CONTROL_RADIUS, CONTROL_TEXT_SIZE};

/// Размер видимого круга, который можно разместить рядом с подписью или на аватаре.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionSize {
    Small, // Круг 16 × 16 логических пикселей.
    Large, // Круг 20 × 20 логических пикселей.
}

/// Показывает состояние выбора. Нажатие на строку или аватар обрабатывает вызывающий код.
pub fn selection_indicator<'a, Message: 'a>(
    size: SelectionSize,
    selected: bool,
) -> Element<'a, Message> {
    indicator(size, selected, true)
}

/// Собирает чекбокс с подписью и галочкой. При нажатии отправляет сообщение для нового значения.
/// Передай `None` вместо обработчика, чтобы показать недоступный чекбокс.
pub fn selection_checkbox<'a, Message: Clone + 'a>(
    label: &'a str,
    size: SelectionSize,
    checked: bool,
    on_toggle: Option<impl FnOnce(bool) -> Message>,
) -> Element<'a, Message> {
    let on_press = on_toggle.map(|toggle| toggle(!checked));
    let content = row![
        indicator(size, checked, on_press.is_some()),
        text(label).size(CONTROL_TEXT_SIZE),
    ]
    .spacing(8)
    .align_y(Center);

    // Вся строка реагирует на нажатие, а круг сохраняет размер из макета.
    let keyboard_pressed = Rc::new(Cell::new(false));
    let pressed_for_style = Rc::clone(&keyboard_pressed);
    let inner = button(content)
        .padding([7, 8])
        .on_press_maybe(on_press.clone())
        .style(move |theme, status| {
            let status = if pressed_for_style.get() && status != Status::Disabled {
                Status::Pressed
            } else {
                status
            };
            checkbox_style(theme, status)
        });

    keyboard_button(inner, on_press, keyboard_pressed)
}

/// Рисует круг и, если элемент выбран, размещает в центре иконку из UI-кита.
fn indicator<'a, Message: 'a>(
    size: SelectionSize,
    selected: bool,
    enabled: bool,
) -> Element<'a, Message> {
    let (diameter, check_size) = match size {
        SelectionSize::Small => (16, 7),
        SelectionSize::Large => (20, 9),
    };
    let content: Element<'a, Message> = if selected {
        icon(IconName::Check, check_size, IconTone::Inherit, None)
    } else {
        space().into()
    };

    container(content)
        .center_x(diameter)
        .center_y(diameter)
        .style(move |theme: &Theme| {
            let palette = theme.palette();
            let colors = button_colors(theme);
            let opacity = if enabled { 1.0 } else { 0.5 };
            let fill = selected.then(|| palette.primary.scale_alpha(opacity).into());

            container::Style {
                background: fill,
                text_color: Some(colors.on_accent.scale_alpha(opacity)),
                border: Border {
                    radius: (diameter as f32 / 2.0).into(),
                    width: if selected { 0.0 } else { 1.0 },
                    color: palette.text.scale_alpha(0.65 * opacity),
                },
                ..container::Style::default()
            }
        })
        .into()
}

/// Окрашивает область нажатия при наведении, нажатии или недоступности чекбокса.
fn checkbox_style(theme: &Theme, status: Status) -> Style {
    let palette = theme.palette();
    let colors = button_colors(theme);
    let background = match status {
        Status::Hovered => Some(colors.card.into()),
        Status::Pressed => Some(colors.card_active.into()),
        Status::Active | Status::Disabled => None,
    };
    let text_color = if status == Status::Disabled {
        colors.text_muted
    } else {
        palette.text
    };

    Style {
        background,
        text_color,
        border: Border {
            radius: CONTROL_RADIUS.into(),
            color: Color::TRANSPARENT,
            ..Border::default()
        },
        ..Style::default()
    }
}
