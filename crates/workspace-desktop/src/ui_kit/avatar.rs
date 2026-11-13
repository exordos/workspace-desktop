//! Показывает аватар и группу аватаров. Сначала описаны данные и размеры,
//! затем сборка аватара с отметкой и отрисовка участников группы внахлёст.

use iced::advanced::image::Handle;
use iced::widget::{Image, container, pin, space, stack, text};
use iced::{Element, Font, Pixels, Theme, border, font};

use super::icon::{IconName, IconTone, icon};
use super::presence::{PresenceSize, PresenceStatus, presence_indicator};
use super::selection::{SelectionSize, selection_indicator};
use super::theme::button_colors;

const GROUP_DIAMETER: u32 = 28;
const GROUP_STEP: u32 = 20;

/// Фото, обрезанное по центру и готовое к отрисовке в любом размере аватара.
#[derive(Clone)]
pub struct AvatarPhoto {
    handle: Handle, // Квадратные RGBA-пиксели с прозрачностью за пределами круга.
}

impl AvatarPhoto {
    /// Готовит фото из RGBA-пикселей. Возвращает `None`, если размеры или длина массива неверны.
    /// Вызывай один раз после декодирования фото и сохраняй результат в состоянии экрана.
    pub fn from_rgba(width: u32, height: u32, pixels: Vec<u8>) -> Option<Self> {
        let stride = (width as usize).checked_mul(4)?;
        let side = width.min(height);
        if side == 0 || stride.checked_mul(height as usize)? != pixels.len() {
            return None;
        }

        let left = ((width - side) / 2) as usize * 4;
        let top = ((height - side) / 2) as usize;
        let side_in_pixels = side as usize;
        let mut square = Vec::with_capacity(side_in_pixels * side_in_pixels * 4);
        for row in pixels.chunks_exact(stride).skip(top).take(side_in_pixels) {
            square.extend_from_slice(&row[left..left + side_in_pixels * 4]);
        }

        // TinySkia не скругляет растровые картинки, поэтому задаём прозрачность края в пикселях.
        let radius = f64::from(side) / 2.0;
        for (index, pixel) in square.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = (index % side_in_pixels) as f64 + 0.5 - radius;
            let y = (index / side_in_pixels) as f64 + 0.5 - radius;
            let coverage = (radius + 0.5 - x.hypot(y)).clamp(0.0, 1.0);
            pixel[3] = (f64::from(pixel[3]) * coverage).round() as u8;
        }

        Some(Self {
            handle: Handle::from_rgba(side, side, square),
        })
    }
}

/// Размер круглой части аватара без учёта отметки поверх неё.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AvatarSize {
    Small, // 32 px, например в списке участников.
    #[default]
    Medium, // 40 px, например в списке чатов.
    Large, // 48 px для крупной строки.
    Profile, // 64 px для профиля.
}

/// Содержимое, которое передаёт экран после загрузки данных пользователя.
#[derive(Clone)]
pub enum AvatarContent<'a> {
    Photo(AvatarPhoto), // Готовое фото из `AvatarPhoto::from_rgba`.
    Initials(&'a str),  // Одна или две буквы, подготовленные экраном.
    Placeholder,        // Значок профиля, когда данных для фото и инициалов ещё нет.
}

/// Отметка в нижнем правом углу аватара. `None` у аватара скрывает её.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarDecoration {
    Presence(PresenceStatus), // Состояние присутствия или отключённой учётной записи.
    Selection(bool),          // Заполненная галочка при `true`, пустой круг при `false`.
}

/// Показывает аватар с фото, инициалами или заглушкой. Отметка получает состояние извне.
/// Нажатие и фокус обрабатывает строка или кнопка, в которую помещён аватар.
pub fn avatar<'a, Message: 'static>(
    size: AvatarSize,
    content: AvatarContent<'a>,
    decoration: Option<AvatarDecoration>,
) -> Element<'a, Message> {
    let (diameter, text_size) = match size {
        AvatarSize::Small => (32, 12),
        AvatarSize::Medium => (40, 14),
        AvatarSize::Large => (48, 18),
        AvatarSize::Profile => (64, 20),
    };
    let selected = decoration == Some(AvatarDecoration::Selection(true));
    let face = avatar_face(content, diameter, text_size, 1, selected, false);
    let Some(decoration) = decoration else {
        return face;
    };

    let (mark, offset, side) = match decoration {
        AvatarDecoration::Presence(status) => {
            // Центр точки лежит на окружности аватара по диагонали вправо и вниз.
            let mark_size = if status == PresenceStatus::Deactivated {
                16
            } else {
                8
            };
            let offset = (diameter as f32 * 0.8536).round() as u32 - mark_size / 2;
            (
                presence_indicator(status, PresenceSize::Xs, true),
                offset,
                diameter.max(offset + mark_size),
            )
        }
        AvatarDecoration::Selection(checked) => (
            selection_indicator(SelectionSize::Small, checked),
            diameter - 12,
            diameter + 4,
        ),
    };
    // Оставляем место для выступающих справа и снизу отметок.
    stack![
        space().width(side).height(side),
        face,
        pin(mark).x(offset).y(offset)
    ]
    .into()
}

/// Показывает выбранные экраном аватары внахлёст. Невидимых участников обозначает счётчиком.
pub fn avatar_group<'a, Message: 'a>(
    visible: &[AvatarContent<'a>],
    hidden_count: usize,
) -> Element<'a, Message> {
    let count = visible.len() + usize::from(hidden_count > 0);
    if count == 0 {
        return space().into();
    }
    let width = GROUP_DIAMETER as f32 + (count - 1) as f32 * GROUP_STEP as f32;
    let mut group = stack![space().width(width).height(GROUP_DIAMETER)];

    // Рисуем участников справа налево, чтобы левый аватар перекрывал соседний справа.
    for (index, content) in visible.iter().enumerate().rev() {
        let face = avatar_face(content.clone(), GROUP_DIAMETER, 10, 2, false, true);
        group = group.push(pin(face).x(index as f32 * GROUP_STEP as f32));
    }

    if hidden_count > 0 {
        let label = if hidden_count > 99 {
            "99+".to_owned()
        } else {
            format!("+{hidden_count}")
        };
        let offset = visible.len() as f32 * GROUP_STEP as f32;
        let count_label = circle(
            avatar_text(label, 10).into(),
            GROUP_DIAMETER,
            2,
            false,
            true,
        );
        group = group.push(pin(count_label).x(offset));
    }
    group.into()
}

/// Готовит фото, инициалы или значок к размещению внутри общей круглой рамки.
fn avatar_face<'a, Message: 'a>(
    content: AvatarContent<'a>,
    diameter: u32,
    text_size: u32,
    stroke_width: u32,
    selected: bool,
    grouped: bool,
) -> Element<'a, Message> {
    let inner_side = diameter - 2 * stroke_width;
    let content: Element<'a, Message> = match content {
        AvatarContent::Photo(photo) => Image::new(photo.handle)
            .width(inner_side)
            .height(inner_side)
            .into(),
        AvatarContent::Initials(initials) if !initials.trim().is_empty() => {
            avatar_text(initials, text_size).into()
        }
        AvatarContent::Initials(_) | AvatarContent::Placeholder => {
            icon(IconName::Profile, diameter / 2, IconTone::Muted)
        }
    };
    circle(content, diameter, stroke_width, selected, grouped)
}

/// Возвращает стиль короткой подписи в центре аватара или счётчика.
fn avatar_text<'a>(
    label: impl iced::widget::text::IntoFragment<'a>,
    size: u32,
) -> iced::widget::Text<'a> {
    text(label)
        .size(size)
        .line_height(Pixels(size as f32 + 4.0))
        .font(Font {
            weight: font::Weight::Semibold,
            ..Font::DEFAULT
        })
}

/// Задаёт цвет фона и обводки круглой части аватара по теме.
fn circle<'a, Message: 'a>(
    content: Element<'a, Message>,
    diameter: u32,
    stroke_width: u32,
    selected: bool,
    grouped: bool,
) -> Element<'a, Message> {
    container(content)
        .center(diameter)
        .style(move |theme: &Theme| {
            let palette = theme.palette();
            let colors = button_colors(theme);
            let outline = if selected {
                palette.primary
            } else if grouped {
                palette.background
            } else {
                colors.border_subtle
            };
            container::Style::default()
                .color(palette.text)
                .background(colors.card)
                .border(
                    border::rounded(diameter as f32 / 2.0)
                        .width(stroke_width)
                        .color(outline),
                )
        })
        .into()
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use iced::advanced::renderer::{Headless, Style};
    use iced::{Color, Renderer, Size, mouse};
    use iced_runtime::user_interface::{Cache, UserInterface};

    use super::*;
    use crate::ui_kit::{ThemeMode, workspace_theme};

    const SCREEN_SIZE: Size<u32> = Size::new(80, 40);

    /// Рисует виджет TinySkia без открытия окна и возвращает его пиксели.
    fn render(view: Element<'_, ()>) -> Vec<u8> {
        let mut renderer = block_on(<Renderer as Headless>::new(
            Font::DEFAULT,
            Pixels(14.0),
            Some("tiny-skia"),
        ))
        .unwrap();
        let theme = workspace_theme(ThemeMode::Dark);
        let mut ui = UserInterface::build(
            view,
            Size::new(SCREEN_SIZE.width as f32, SCREEN_SIZE.height as f32),
            Cache::new(),
            &mut renderer,
        );
        ui.draw(
            &mut renderer,
            &theme,
            &Style::default(),
            mouse::Cursor::Unavailable,
        );
        renderer.screenshot(SCREEN_SIZE, 1.0, Color::BLACK)
    }

    /// Возвращает цвет одной точки с координатами в пикселях.
    fn pixel(screenshot: &[u8], x: usize, y: usize) -> [u8; 4] {
        let index = (y * SCREEN_SIZE.width as usize + x) * 4;
        screenshot[index..index + 4].try_into().unwrap()
    }

    /// Проверяет, что широкий кадр обрезается по центру и не закрашивает углы аватара.
    #[test]
    fn wide_photo_keeps_its_center_and_transparent_corners() {
        let mut rgba = Vec::new();
        for _ in 0..30 {
            rgba.extend([255, 0, 0, 255].repeat(15));
            rgba.extend([0, 255, 0, 255].repeat(30));
            rgba.extend([0, 0, 255, 255].repeat(15));
        }
        let photo = AvatarPhoto::from_rgba(60, 30, rgba).unwrap();
        let screenshot = render(avatar(AvatarSize::Small, AvatarContent::Photo(photo), None));

        assert_eq!(pixel(&screenshot, 2, 2), [0, 0, 0, 255]);
        assert_eq!(pixel(&screenshot, 3, 16), [0, 255, 0, 255]);
        assert_eq!(pixel(&screenshot, 27, 16), [0, 255, 0, 255]);
    }

    /// Проверяет, что первый круг перекрывает следующий, а шаг между ними составляет 20 px.
    #[test]
    fn group_draws_the_first_avatar_above_the_second() {
        let photo = |color: [u8; 4]| {
            AvatarContent::Photo(AvatarPhoto::from_rgba(24, 24, color.repeat(24 * 24)).unwrap())
        };
        let screenshot = render(avatar_group(
            &[photo([255, 0, 0, 255]), photo([0, 0, 255, 255])],
            0,
        ));

        assert_eq!(pixel(&screenshot, 23, 14), [255, 0, 0, 255]);
        assert_eq!(pixel(&screenshot, 29, 14), [0, 0, 255, 255]);
        assert_eq!(pixel(&screenshot, 44, 14), [0, 0, 255, 255]);
        assert_eq!(pixel(&screenshot, 49, 14), [0, 0, 0, 255]);
    }
}
