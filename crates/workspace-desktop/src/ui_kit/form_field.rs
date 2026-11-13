//! Добавляет подпись, подсказку и сообщение об ошибке к полю формы.
//! Содержимое поля и правила проверки передаёт вызывающий экран.

use iced::widget::text::{Style, Wrapping};
use iced::widget::{column, container, text};
use iced::{Element, Fill, Font, Pixels, Theme, font, padding};

use super::{CONTROL_TEXT_SIZE, theme::text_muted};

/// Размещает подпись над готовым полем ввода или селектором.
/// Под полем показывает ошибку, а при её отсутствии — подсказку. Пустые сообщения пропускает.
pub fn form_field<'a, Message: 'a>(
    label: &'a str,
    control: impl Into<Element<'a, Message>>,
    hint: Option<&'a str>,
    error: Option<&'a str>,
) -> Element<'a, Message> {
    let label = text(label)
        .size(CONTROL_TEXT_SIZE)
        .line_height(Pixels(20.0))
        .wrapping(Wrapping::WordOrGlyph)
        .font(Font {
            weight: font::Weight::Medium,
            ..Font::DEFAULT
        });
    let mut field = column![label, control.into()].width(Fill);

    let error = error.filter(|message| !message.trim().is_empty());
    let hint = hint.filter(|message| !message.trim().is_empty());
    // Ошибка занимает место подсказки, чтобы рядом с полем была одна понятная строка.
    if let Some(message) = error.or(hint) {
        let is_error = error.is_some();
        let annotation = text(message)
            .size(12)
            .line_height(Pixels(16.0))
            .wrapping(Wrapping::WordOrGlyph)
            .style(move |theme: &Theme| Style {
                color: Some(if is_error {
                    theme.palette().danger
                } else {
                    text_muted(theme)
                }),
            });
        field = field.push(container(annotation).padding(padding::top(4)));
    }

    field.into()
}
