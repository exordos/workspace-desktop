//! Поле поиска Workspace. Здесь собраны обычный ввод, лупа и кнопка очистки.

use std::{cell::Cell, rc::Rc};

use iced::advanced::widget::Id;
use iced::alignment::Horizontal;
use iced::widget::button::{Status, Style};
use iced::widget::{button, container, row, stack};
use iced::{Border, Center, Element, Fill, Padding};

use super::button::keyboard_button;
use super::text_input::text_input;
use super::theme::text_muted;
use super::{FieldSize, IconName, IconTone, icon};

const ACTION_SPACING: u32 = 4;

/// Создаёт поле поиска заданного размера. Без обработчика ввода поле отключено.
/// Обработчик сообщения `on_clear` очищает значение и возвращает фокус полю по `id`.
pub fn search_input<'a, Message: Clone + 'a>(
    id: impl Into<Id>,
    placeholder: &str,
    value: &str,
    size: FieldSize,
    on_input: Option<impl Fn(String) -> Message + 'a>,
    on_clear: Message,
) -> Element<'a, Message> {
    let (search_icon_size, clear_button_size, action_inset) = match size {
        FieldSize::Compact => (18, 20, 6),
        FieldSize::Regular => (20, 28, 8),
    };
    let show_clear = on_input.is_some() && !value.is_empty();
    let mut right_padding = search_icon_size + action_inset + ACTION_SPACING;
    if show_clear {
        right_padding += clear_button_size + ACTION_SPACING;
    }
    // Оставляем место справа под иконки, чтобы длинный запрос прокручивался внутри поля.
    let vertical_padding = f32::from(size.vertical_padding());
    let input = text_input(placeholder, value, size, on_input)
        .id(id)
        .padding(Padding {
            top: vertical_padding,
            right: right_padding as f32,
            bottom: vertical_padding,
            left: f32::from(size.horizontal_padding()),
        });

    let mut actions = row![icon(
        IconName::Search,
        search_icon_size,
        IconTone::Muted,
        None
    )]
    .spacing(ACTION_SPACING)
    .align_y(Center);
    if show_clear {
        actions = actions.push(clear_button(clear_button_size, on_clear));
    }

    // Штатное поле рисует рамку и сохраняет обработку ввода, а Stack даёт кнопке принять нажатие первой.
    let trailing = container(actions)
        .padding(Padding::default().right(action_inset))
        .width(Fill)
        .height(Fill)
        .align_y(Center)
        .align_x(Horizontal::Right);
    stack![input, trailing].width(Fill).into()
}

/// Создаёт маленькую кнопку очистки с активацией мышью, Space и Enter.
fn clear_button<'a, Message: Clone + 'a>(size: u32, on_clear: Message) -> Element<'a, Message> {
    let keyboard_pressed = Rc::new(Cell::new(false));
    let pressed_for_style = Rc::clone(&keyboard_pressed);
    let glyph = container(icon(IconName::Close, 14, IconTone::Inherit, None))
        .center_x(size)
        .center_y(size);
    let inner = button(glyph)
        .padding(0)
        .on_press(on_clear.clone())
        .style(move |theme, status| {
            let palette = theme.palette();
            let tint = if status == Status::Pressed || pressed_for_style.get() {
                0.18
            } else if status == Status::Hovered {
                0.10
            } else {
                0.0
            };
            Style {
                background: (tint > 0.0).then(|| palette.primary.scale_alpha(tint).into()),
                text_color: if tint > 0.0 {
                    palette.text
                } else {
                    text_muted(theme)
                },
                border: Border::default().rounded(4),
                ..Style::default()
            }
        });
    keyboard_button(inner, Some(on_clear), keyboard_pressed)
}
