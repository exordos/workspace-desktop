//! Рисует подпись дня между группами сообщений в истории.

use iced::widget::{container, text};
use iced::{Element, Fill, border};

use super::theme::button_colors;

/// Показывает переданную дату по центру списка сообщений.
pub fn day_divider<Message: 'static>(label: String) -> Element<'static, Message> {
    let badge = container(text(label).size(14))
        .padding([8, 12])
        .style(|theme| {
            let colors = button_colors(theme);
            container::Style::default()
                .background(colors.card)
                .color(theme.palette().text)
                .border(border::rounded(100))
        });
    container(badge).center_x(Fill).into()
}
