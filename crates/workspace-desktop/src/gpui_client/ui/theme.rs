//! Переносит цвета Blue Mist в GPUI и задаёт общие радиусы и роли текста.

use gpui::{
    App, Div, FontWeight, Hsla, Pixels, ShapedLine, SharedString, Window, WindowAppearance, canvas,
    div, prelude::*, px, rgb,
};

use unicode_segmentation::UnicodeSegmentation;

/// Радиус панелей рабочего окна, строк каталога и меню действий.
pub(in crate::gpui_client) const PANEL_RADIUS: Pixels = px(6.);
/// Радиус композера, диалога, всплывающей панели и подсказки.
pub(in crate::gpui_client) const SURFACE_RADIUS: Pixels = px(10.);
/// Радиус кнопки раздела в верхней навигации.
pub(super) const NAVIGATION_CHIP_RADIUS: Pixels = px(12.);
/// Радиус внешних углов пузыря сообщения.
pub(super) const MESSAGE_BUBBLE_RADIUS: Pixels = px(18.);
/// Радиус компактных кнопок и полей ввода UI-кита.
pub(super) const CONTROL_RADIUS: Pixels = px(8.);
/// Толщина обычной и резервной границы компонентов UI-кита.
pub(super) const CONTROL_BORDER: Pixels = px(1.);
/// Высота стандартного компактного контрола и ширина кнопки с одной иконкой.
pub(super) const COMPACT_CONTROL_SIZE: Pixels = px(32.);
/// Горизонтальный отступ компактной кнопки и поля.
pub(super) const COMPACT_CONTROL_INSET: Pixels = px(8.);

/// Режим оформления окна Workspace.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThemeMode {
    #[default]
    Light, // Светлая палитра.
    Dark, // Тёмная палитра.
}

impl From<WindowAppearance> for ThemeMode {
    /// Выбирает палитру по оформлению, которое сообщает macOS.
    fn from(value: WindowAppearance) -> Self {
        match value {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
        }
    }
}

/// Цвета Blue Mist и видимость рамки фокуса для текущей отрисовки.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub background: Hsla,       // Фон окна и нейтральная подложка аватара.
    pub surface: Hsla,          // Верхняя панель, сайдбар и внешняя поверхность редактора.
    pub card: Hsla,             // Обычная строка или нейтральная кнопка.
    pub card_hover: Hsla,       // Строка чата под указателем.
    pub card_selected: Hsla,    // Выбранная строка чата или нажатая карточка.
    pub field: Hsla,            // Внутренний фон поля ввода.
    pub text: Hsla,             // Основной текст.
    pub text_secondary: Hsla,   // Второстепенный текст.
    pub text_muted: Hsla,       // Подсказка поля и неактивная подпись.
    pub accent: Hsla,           // Кнопка основного действия.
    pub focus: Hsla,            // Контрастная рамка клавиатурного фокуса.
    pub accent_soft: Hsla,      // Подложка мягкого акцента.
    pub on_accent: Hsla,        // Текст на кнопке основного действия.
    pub danger: Hsla,           // Ошибка или опасное действие.
    pub border: Hsla,           // Граница панелей и элементов.
    pub unread: Hsla,           // Бейдж непрочитанного.
    pub unread_muted: Hsla,     // Счётчик приглушённого разговора.
    pub badge_text: Hsla,       // Контрастный текст синего бейджа.
    pub muted_badge_text: Hsla, // Символы на приглушённом бейдже.
    pub message: Hsla,          // Фон входящего сообщения.
    pub message_own: Hsla,      // Фон исходящего сообщения.
    pub message_time: Hsla,     // Время в пузыре сообщения.
    pub icon: Hsla,             // Цвет обычной иконки.
    pub disabled: Hsla,         // Иконка недоступного действия.
    pub icon_hover: Hsla,       // Иконка под указателем.
    pub presence_online: Hsla,  // Индикатор «В сети».
    pub presence_away: Hsla,    // Индикатор «Отошёл».
    pub(in crate::gpui_client) keyboard_focus_visible: bool, // Можно показать рамку у элемента с фокусом.
}

impl Palette {
    /// Берёт оформление и видимость рамки фокуса из текущего окна.
    pub(in crate::gpui_client) fn for_window(window: &Window, app: &App) -> Self {
        Self::for_mode(ThemeMode::from(window.appearance())).with_window_input(window, app)
    }

    /// Добавляет способ ввода окна к выбранной палитре, например в галерее двух тем.
    pub fn with_window_input(mut self, window: &Window, app: &App) -> Self {
        self.keyboard_focus_visible = super::focus::is_visible(window, app);
        self
    }

    /// Проверяет, надо ли рисовать рамки у элементов с фокусом.
    pub fn keyboard_focus_visible(&self) -> bool {
        self.keyboard_focus_visible
    }

    /// Возвращает рабочую палитру для темы окна или галереи.
    pub fn for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Light => Self {
                background: rgb(0xe8f0f5).into(),
                surface: rgb(0xffffff).into(),
                card: rgb(0xeff5fb).into(),
                card_hover: rgb(0xe4edf7).into(),
                card_selected: rgb(0xd5e5f6).into(),
                field: rgb(0xf0f6fc).into(),
                text: rgb(0x1a2330).into(),
                text_secondary: rgb(0x526176).into(),
                text_muted: rgb(0x8d9bae).into(),
                accent: rgb(0x6f90d8).into(),
                focus: rgb(0x385fae).into(),
                accent_soft: rgb(0xdce8fb).into(),
                on_accent: rgb(0x101a28).into(),
                danger: rgb(0xc4241c).into(),
                border: rgb(0xd3dfec).into(),
                unread: rgb(0x6f90d8).into(),
                unread_muted: rgb(0x9ca9bb).into(),
                badge_text: rgb(0x101a28).into(),
                muted_badge_text: rgb(0x101a28).into(),
                message: rgb(0xffffff).into(),
                message_own: rgb(0xdbe8f8).into(),
                message_time: rgb(0x526176).into(),
                icon: rgb(0x8d9bae).into(),
                disabled: rgb(0xb8c3d1).into(),
                icon_hover: rgb(0x1a2330).into(),
                presence_online: rgb(0x26c038).into(),
                presence_away: rgb(0xff8900).into(),
                keyboard_focus_visible: false,
            },
            ThemeMode::Dark => Self {
                background: rgb(0x15181d).into(),
                surface: rgb(0x232c38).into(),
                card: rgb(0x243040).into(),
                card_hover: rgb(0x30465d).into(),
                card_selected: rgb(0x30465d).into(),
                field: rgb(0x232c38).into(),
                text: rgb(0xf7fbff).into(),
                text_secondary: rgb(0xb4c3d4).into(),
                text_muted: rgb(0x748396).into(),
                accent: rgb(0x7d9ddf).into(),
                focus: rgb(0xa6c0f4).into(),
                accent_soft: rgb(0x2f3f66).into(),
                on_accent: rgb(0x111823).into(),
                danger: rgb(0xff8777).into(),
                border: rgb(0x394a61).into(),
                unread: rgb(0x6f8fda).into(),
                unread_muted: rgb(0x646f7f).into(),
                badge_text: rgb(0x111823).into(),
                muted_badge_text: rgb(0xffffff).into(),
                message: rgb(0x323c4a).into(),
                message_own: rgb(0x2a3a50).into(),
                message_time: rgb(0xb4c3d4).into(),
                icon: rgb(0x748396).into(),
                disabled: rgb(0x4b596a).into(),
                icon_hover: rgb(0xaab9cc).into(),
                presence_online: rgb(0x26c038).into(),
                presence_away: rgb(0xff8900).into(),
                keyboard_focus_visible: false,
            },
        }
    }
}

/// Размер, вес и базовый цвет текста по роли.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextRole {
    Heading,        // Заголовок панели.
    ProfileHeading, // Крупное имя в карточке профиля.
    Label,          // Название строки или элемента.
    Body,           // Текст сообщения или поля.
    Caption,        // Подпись, время или превью.
    Initials,       // Инициалы внутри компактного аватара.
    Badge,          // Число или знак упоминания внутри бейджа.
}

impl TextRole {
    /// Возвращает размер, высоту строки, насыщенность и цвет для этой роли.
    fn style(self, palette: Palette) -> (Pixels, Pixels, FontWeight, Hsla) {
        let (size, height, weight, color) = match self {
            TextRole::Heading => (16., 22., FontWeight::SEMIBOLD, palette.text),
            TextRole::ProfileHeading => (20., 24., FontWeight::MEDIUM, palette.text),
            TextRole::Label => (14., 20., FontWeight::MEDIUM, palette.text),
            TextRole::Body => (14., 20., FontWeight::NORMAL, palette.text),
            TextRole::Caption => (12., 18., FontWeight::NORMAL, palette.text_secondary),
            TextRole::Initials => (12., 16., FontWeight::SEMIBOLD, palette.text),
            TextRole::Badge => (11., 11., FontWeight::MEDIUM, palette.badge_text),
        };
        (px(size), px(height), weight, color)
    }
}

/// Рисует текст системным шрифтом GPUI в заданной роли.
pub fn label(palette: Palette, role: TextRole, text: impl Into<SharedString>) -> Div {
    let (size, height, weight, color) = role.style(palette);
    div()
        .text_size(size)
        .line_height(height)
        .font_weight(weight)
        .text_color(color)
        .child(text.into())
}

/// Рисует одну строку уже после того, как GPUI выделил ширину под текст.
pub(in crate::gpui_client) fn ellipsized_label(
    palette: Palette,
    role: TextRole,
    text: impl Into<SharedString>,
) -> Div {
    let text = text.into();
    let (size, height, weight, color) = role.style(palette);
    div().w_full().h(height).overflow_hidden().child(
        canvas(
            move |bounds, window, _| {
                let mut style = window.text_style();
                style.font_weight = weight;
                style.color = color;
                fit_line(&text, bounds.size.width, |text| {
                    let run = style.to_run(text.len());
                    window.text_system().shape_line(text, size, &[run], None)
                })
            },
            move |bounds, line, window, app| {
                if let Some(line) = line {
                    let _ = line.paint(bounds.origin, height, window, app);
                }
            },
        )
        .size_full(),
    )
}

/// Оставляет целые графемы и добавляет многоточие, если полная строка не помещается.
fn fit_line(
    text: &SharedString,
    width: Pixels,
    shape: impl Fn(SharedString) -> ShapedLine,
) -> Option<ShapedLine> {
    if text.is_empty() || width <= px(0.) {
        return None;
    }
    let original = shape(text.clone());
    if original.width <= width {
        return Some(original);
    }

    // Встроенное усечение GPUI 0.2.2 может передать Core Text разрезанную UTF-8
    // последовательность. Собираем каждую пробную строку из целых графем.
    let mut best = shape("…".into());
    if best.width > width {
        return None;
    }
    let boundaries: Vec<_> = text
        .grapheme_indices(true)
        .map(|(start, _)| start)
        .skip(1)
        .collect();
    let (mut from, mut to) = (0, boundaries.len());
    while from < to {
        let middle = from + (to - from) / 2;
        let suffix = shape(format!("{}…", &text[..boundaries[middle]]).into());
        if suffix.width <= width {
            best = suffix;
            from = middle + 1;
        } else {
            to = middle;
        }
    }
    Some(best)
}
