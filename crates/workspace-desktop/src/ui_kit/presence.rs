//! Рисует состояние присутствия рядом с аватаром или именем.
//! Сначала описаны состояния и размеры, затем функция отрисовки.

use iced::Element;
use iced::border;
use iced::widget::{container, space};

use super::icon::{IconName, IconTone, icon};
use super::theme::{button_colors, presence_away_color, presence_online_color, text_muted};

/// Состояние пользователя, которое передаёт экран.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceStatus {
    Online,      // Пользователь в сети.
    Away,        // Пользователь отошёл.
    Offline,     // Пользователь вне сети.
    Deactivated, // Учётная запись отключена.
}

/// Размер точки присутствия без обводки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceSize {
    Xs, // 6 px.
    Sm, // 8 px.
    Md, // 10 px.
    Lg, // 12 px.
}

/// Показывает точку присутствия или значок отключённой учётной записи.
/// Для размещения поверх аватара можно включить обводку цвета фона.
pub fn presence_indicator<Message: 'static>(
    status: PresenceStatus,
    size: PresenceSize,
    with_border: bool,
) -> Element<'static, Message> {
    let diameter: u32 = match size {
        PresenceSize::Xs => 6,
        PresenceSize::Sm => 8,
        PresenceSize::Md => 10,
        PresenceSize::Lg => 12,
    };
    let (mark, inner_size) = if status == PresenceStatus::Deactivated {
        let icon_size = match size {
            PresenceSize::Xs | PresenceSize::Sm => 12,
            PresenceSize::Md | PresenceSize::Lg => 14,
        };
        (
            icon(IconName::Block, icon_size, IconTone::Muted, None),
            icon_size,
        )
    } else {
        (space().into(), diameter)
    };

    let border_width = match (with_border, size, status) {
        (false, _, _) => 0,
        (true, _, PresenceStatus::Deactivated) => 2,
        (true, PresenceSize::Xs, _) => 1,
        (true, _, _) => 2,
    };

    // Iced рисует обводку внутри границ. Увеличиваем размер, чтобы точка сохранила диаметр.
    let outer_size = inner_size + 2 * border_width;
    container(mark)
        .center(outer_size)
        .style(move |theme| {
            let fill = match status {
                PresenceStatus::Online => presence_online_color(),
                PresenceStatus::Away => presence_away_color(),
                PresenceStatus::Offline => text_muted(theme),
                PresenceStatus::Deactivated => button_colors(theme).card,
            };
            container::Style::default().background(fill).border(
                border::rounded(outer_size as f32 / 2.0)
                    .width(border_width)
                    .color(theme.palette().background),
            )
        })
        .into()
}
