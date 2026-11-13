//! Строка участника. Здесь собраны карточка и её содержимое для списка выбора.

use iced::advanced::text::Wrapping;
use iced::widget::{column, container, row, text};
use iced::{Border, Center, Element, Fill, Pixels, Theme};

use super::theme::{button_colors, text_secondary};
use super::{CONTROL_RADIUS, CONTROL_TEXT_SIZE};

/// Показывает карточку участника. Правый слот может содержать роль или отдельное действие.
pub fn member_row<'a, Message: 'a>(
    avatar: impl Into<Element<'a, Message>>,
    name: &'a str,
    secondary: Option<&'a str>,
    trailing: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    container(member_row_content(avatar, name, secondary, trailing))
        .width(Fill)
        .padding([8, 12])
        .style(|theme: &Theme| container::Style {
            background: Some(button_colors(theme).card.into()),
            border: Border::default().rounded(CONTROL_RADIUS),
            ..container::Style::default()
        })
        .into()
}

/// Собирает аватар, имя и подпись без фона и отступов карточки.
/// В выбираемой строке правый слот используется как декоративный.
pub fn member_row_content<'a, Message: 'a>(
    avatar: impl Into<Element<'a, Message>>,
    name: &'a str,
    secondary: Option<&'a str>,
    trailing: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let name = text(name.lines().next().unwrap_or_default())
        .size(CONTROL_TEXT_SIZE)
        .line_height(Pixels(20.0))
        .wrapping(Wrapping::None);
    let mut labels = column![name].spacing(2).width(Fill);
    if let Some(secondary) = secondary.filter(|value| !value.is_empty()) {
        labels = labels.push(
            text(secondary.lines().next().unwrap_or_default())
                .size(12)
                .line_height(Pixels(16.0))
                .wrapping(Wrapping::None)
                .style(|theme: &Theme| text::Style {
                    color: Some(text_secondary(theme)),
                }),
        );
    }

    // Средняя часть занимает свободное место, а длинные строки обрезаются до правого слота.
    let labels = container(labels).width(Fill).clip(true);
    let mut member = row![avatar.into(), labels]
        .width(Fill)
        .align_y(Center)
        .spacing(12);
    if let Some(trailing) = trailing {
        member = member.push(trailing);
    }
    member.into()
}
