//! Создаёт текстовое поле Iced в стиле Workspace.

use iced::widget::text_input as iced_text_input;
use iced::widget::text_input::{Status, TextInput, default};

use super::{CONTROL_RADIUS, CONTROL_TEXT_SIZE};

/// Создаёт поле с подсказкой и текущим текстом. Если обработчика ввода нет, поле отключено.
pub fn text_input<'a, Message: Clone + 'a>(
    placeholder: &str,
    value: &str,
    on_input: Option<impl Fn(String) -> Message + 'a>,
) -> TextInput<'a, Message> {
    iced_text_input(placeholder, value)
        .padding([8, 12])
        .size(CONTROL_TEXT_SIZE)
        .on_input_maybe(on_input)
        .style(|theme, status| {
            let mut style = default(theme, status);
            style.border.radius = CONTROL_RADIUS.into();
            if matches!(status, Status::Focused { .. }) {
                style.border.color = theme.palette().primary;
            }
            style
        })
}
