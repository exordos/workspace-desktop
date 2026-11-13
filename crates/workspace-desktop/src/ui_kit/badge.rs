//! Рисует счётчик непрочитанных сообщений и знак упоминания.
//! Модуль задаёт их размеры, оформление и сокращение больших чисел.

use iced::alignment::Horizontal;
use iced::border;
use iced::font::{Font, Weight};
use iced::widget::{container, text};
use iced::{Color, Element};

use super::theme::{button_colors, muted_badge_color, unread_badge_color};

/// Размер бейджа для обычной или компактной строки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeSize {
    Small,   // Высота 16 px.
    Regular, // Высота 20 px.
}

/// Цвет счётчика, который экран выбирает по состоянию разговора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeTone {
    Unread, // Акцентный счётчик.
    Muted,  // Приглушённый счётчик.
}

/// Показывает переданное число. Экран решает, когда показывать счётчик.
pub fn unread_count_badge<Message: 'static>(
    count: u64,
    tone: BadgeTone,
    size: BadgeSize,
) -> Element<'static, Message> {
    // Как в Electron, оставляем 1000 целиком, а дальше показываем целое число тысяч.
    let label = if count > 1_000 {
        format!("{}k+", count / 1_000)
    } else {
        count.to_string()
    };
    badge(label, tone, size)
}

/// Показывает знак непрочитанного упоминания.
pub fn mention_badge<Message: 'static>(size: BadgeSize) -> Element<'static, Message> {
    badge("@".to_owned(), BadgeTone::Unread, size)
}

/// Рисует общую форму счётчика и знака упоминания.
fn badge<Message: 'static>(
    label: String,
    tone: BadgeTone,
    size: BadgeSize,
) -> Element<'static, Message> {
    let (height, horizontal_padding, text_size): (u32, u16, u32) = match size {
        BadgeSize::Small => (16, 4, 10),
        BadgeSize::Regular => (20, 6, 11),
    };
    let single_character = label.len() == 1;
    let text = text(label).size(text_size).font(Font {
        weight: Weight::Medium,
        ..Font::DEFAULT
    });

    // Один символ центрируем в круге, для длинной подписи ширину подбирает Iced.
    let badge = container(text).center_y(height);
    let badge = if single_character {
        badge.center_x(height)
    } else {
        badge
            .padding([0, horizontal_padding])
            .align_x(Horizontal::Center)
    };

    badge
        .style(move |theme| {
            // Белый текст читается на тёмной приглушённой плашке Blue Mist.
            // На более светлых плашках берём тёмный цвет текста с акцентных кнопок.
            let (background, foreground) = match tone {
                BadgeTone::Unread => (unread_badge_color(theme), button_colors(theme).on_accent),
                BadgeTone::Muted if theme.extended_palette().is_dark => {
                    (muted_badge_color(theme), Color::WHITE)
                }
                BadgeTone::Muted => (muted_badge_color(theme), button_colors(theme).on_accent),
            };
            container::Style::default()
                .background(background)
                .color(foreground)
                .border(border::rounded(height as f32 / 2.0))
        })
        .into()
}
