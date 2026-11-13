//! Создаёт текстовое поле Iced в стиле Workspace.

use iced::Pixels;
use iced::widget::text_input as iced_text_input;
use iced::widget::text_input::{Status, TextInput, default};

use super::{CONTROL_RADIUS, CONTROL_TEXT_SIZE, FieldSize};

/// Создаёт поле с подсказкой и текущим текстом. Если обработчика ввода нет, поле отключено.
pub fn text_input<'a, Message: Clone + 'a>(
    placeholder: &str,
    value: &str,
    size: FieldSize,
    on_input: Option<impl Fn(String) -> Message + 'a>,
) -> TextInput<'a, Message> {
    text_input_with_error(placeholder, value, size, on_input, false)
}

/// Создаёт поле и выделяет рамку, если экран передал состояние ошибки.
pub fn text_input_with_error<'a, Message: Clone + 'a>(
    placeholder: &str,
    value: &str,
    size: FieldSize,
    on_input: Option<impl Fn(String) -> Message + 'a>,
    invalid: bool,
) -> TextInput<'a, Message> {
    iced_text_input(placeholder, value)
        .padding([size.vertical_padding(), size.horizontal_padding()])
        .line_height(Pixels(f32::from(FieldSize::LINE_HEIGHT)))
        .size(CONTROL_TEXT_SIZE)
        .on_input_maybe(on_input)
        .style(move |theme, status| {
            let mut style = default(theme, status);
            style.border.radius = CONTROL_RADIUS.into();
            if invalid && status != Status::Disabled {
                style.border.color = theme.palette().danger;
                if matches!(status, Status::Focused { .. }) {
                    style.border.width = 2.0;
                }
            } else if matches!(status, Status::Focused { .. }) {
                style.border.color = theme.palette().primary;
            }
            style
        })
}
