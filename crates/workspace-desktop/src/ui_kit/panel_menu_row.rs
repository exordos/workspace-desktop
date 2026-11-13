//! Собирает строку действия для боковой панели и настроек.
//! Здесь заданы параметры строки, её раскладка и цвета для разных состояний.

use std::{cell::Cell, rc::Rc};

use iced::widget::button::{Status, Style};
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, row, space, text};
use iced::{Border, Center, Element, Fill, Font, Pixels, Theme, font};

use super::button::keyboard_button;
use super::theme::{button_colors, text_muted, text_secondary};
use super::{CONTROL_TEXT_SIZE, IconName, IconTone, icon};

/// Настройки строки, которые передаёт боковая панель.
pub struct PanelMenuRowOptions<'a, Message> {
    pub icon: Option<IconName>, // Иконка слева. При `None` место под неё не выделяется.
    pub subtitle: Option<&'a str>, // Дополнительное описание под подписью.
    pub trailing: Option<Element<'a, Message>>, // Значение, стрелка или другой декоративный элемент справа.
    pub compact: bool,                          // Уменьшает боковые и вертикальные отступы.
    pub danger: bool,                           // Выделяет действие с серьёзными последствиями.
}

impl<Message> Default for PanelMenuRowOptions<'_, Message> {
    /// Возвращает параметры обычной строки с одной подписью.
    fn default() -> Self {
        Self {
            icon: None,
            subtitle: None,
            trailing: None,
            compact: false,
            danger: false,
        }
    }
}

/// Создаёт строку действия на всю ширину. `None` вместо события отключает нажатие.
/// Элементы в правом слоте служат оформлением, действие обрабатывает сама строка.
pub fn panel_menu_row<'a, Message: Clone + 'a>(
    label: &'a str,
    options: PanelMenuRowOptions<'a, Message>,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let PanelMenuRowOptions {
        icon: leading,
        subtitle,
        trailing,
        compact,
        danger,
    } = options;

    let label = text(label)
        .size(CONTROL_TEXT_SIZE)
        .line_height(Pixels(20.0))
        .wrapping(Wrapping::WordOrGlyph)
        .font(Font {
            weight: if compact {
                font::Weight::Normal
            } else {
                font::Weight::Medium
            },
            ..Font::DEFAULT
        });
    let disabled = on_press.is_none();
    let mut labels = column![label].width(Fill).spacing(2);
    if let Some(subtitle) = subtitle.filter(|subtitle| !subtitle.is_empty()) {
        labels = labels.push(
            text(subtitle)
                .size(if compact { 12 } else { CONTROL_TEXT_SIZE })
                .line_height(Pixels(20.0))
                .wrapping(Wrapping::WordOrGlyph)
                .style(move |theme: &Theme| text::Style {
                    color: Some(if disabled {
                        text_muted(theme).scale_alpha(0.6)
                    } else {
                        text_secondary(theme)
                    }),
                }),
        );
    }

    let mut content = row![].width(Fill).spacing(8).align_y(Center);
    if let Some(name) = leading {
        let glyph = icon(name, 22, IconTone::Inherit, None);
        content = content.push(container(glyph).center(32).style(move |theme: &Theme| {
            let tint = if danger {
                theme.palette().danger
            } else {
                text_secondary(theme)
            };
            container::Style::default().color(tint.scale_alpha(if disabled { 0.6 } else { 1.0 }))
        }));
    }
    content = content.push(labels);
    if let Some(trailing) = trailing {
        content = content.push(trailing);
    }

    // Невидимый элемент задаёт минимум в 32 px, а длинная подпись может увеличить высоту.
    let content = row![space().height(32), content]
        .width(Fill)
        .align_y(Center);
    let keyboard_pressed = Rc::new(Cell::new(false));
    let pressed_for_style = Rc::clone(&keyboard_pressed);
    let inner = button(content)
        .width(Fill)
        .padding(if compact { [4, 8] } else { [6, 16] })
        .on_press_maybe(on_press.clone())
        .style(move |theme, status| {
            let colors = button_colors(theme);
            let background = match status {
                Status::Disabled => None,
                Status::Pressed => Some(colors.card_active.into()),
                _ if pressed_for_style.get() => Some(colors.card_active.into()),
                Status::Hovered => Some(colors.card.into()),
                Status::Active => None,
            };
            let text = if danger {
                theme.palette().danger
            } else {
                theme.palette().text
            };
            Style {
                background,
                text_color: text.scale_alpha(if disabled { 0.6 } else { 1.0 }),
                border: Border::default(),
                ..Style::default()
            }
        });
    keyboard_button(inner, on_press, keyboard_pressed)
}
