//! Связывает имена иконок с файлами SVG и рисует их средствами Iced.

use std::sync::LazyLock;

use iced::advanced::svg::{Handle, Renderer as _, Svg};
use iced::advanced::widget::Tree;
use iced::advanced::{Layout, Widget, layout, renderer};
use iced::{Element, Length, Rectangle, Renderer, Size, Theme, mouse};

use super::theme::text_muted;

/// Имя иконки, по которому виджет выбирает встроенный SVG.
#[derive(Debug, Clone, Copy)]
pub enum IconName {
    Plus,              // Знак добавления.
    Search,            // Лупа для поиска.
    Close,             // Крестик для закрытия.
    Attach,            // Скрепка для вложения.
    Check,             // Галочка выбора.
    Block,             // Значок отключённой учётной записи.
    ChatBubbleOutline, // Контур чата для строки канала.
    Profile,           // Заглушка аватара.
    VolumeUp,          // Настройка громкости.
    ChevronRight,      // Стрелка перехода вправо.
    Settings,          // Шестерёнка настроек.
    Language,          // Настройка языка.
    ChevronDown,       // Стрелка раскрытия списка.
    ChevronUp,         // Стрелка сворачивания списка.
    Schedule,          // Ожидание отправки сообщения.
    Send,              // Отправка сообщения.
    Warning,           // Ошибка отправки сообщения.
    LogoutCompact,     // Выход из аккаунта.
}

impl IconName {
    /// Загружает встроенный SVG при первом обращении и возвращает ссылку на него.
    fn handle(self) -> &'static Handle {
        // Keep the original Workspace UI SVGs and their viewBox.
        static PLUS: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/plus.svg")[..])
        });
        static SEARCH: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/search.svg")[..])
        });
        static CLOSE: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/close.svg")[..])
        });
        static ATTACH: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/attach.svg")[..])
        });

        static CHECK: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/check.svg")[..])
        });
        static BLOCK: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/block.svg")[..])
        });

        static CHAT_BUBBLE_OUTLINE: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/chat_bubble_outline.svg")[..])
        });
        static PROFILE: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/profile.svg")[..])
        });
        static VOLUME_UP: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/volume_up.svg")[..])
        });
        static CHEVRON_RIGHT: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/chevron-right.svg")[..])
        });
        static SETTINGS: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/settings.svg")[..])
        });
        static LANGUAGE: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/language.svg")[..])
        });
        static LOGOUT_COMPACT: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/logout_compact.svg")[..])
        });

        static CHEVRON_DOWN: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/chevron-down.svg")[..])
        });

        static CHEVRON_UP: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/chevron-up.svg")[..])
        });

        static SCHEDULE: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/schedule.svg")[..])
        });

        static SEND: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/send.svg")[..])
        });

        static WARNING: LazyLock<Handle> = LazyLock::new(|| {
            Handle::from_memory(&include_bytes!("../../assets/icons/warning.svg")[..])
        });

        match self {
            Self::Plus => &PLUS,
            Self::Search => &SEARCH,
            Self::Close => &CLOSE,
            Self::Attach => &ATTACH,
            Self::Check => &CHECK,
            Self::Block => &BLOCK,
            Self::ChatBubbleOutline => &CHAT_BUBBLE_OUTLINE,
            Self::Profile => &PROFILE,
            Self::VolumeUp => &VOLUME_UP,
            Self::ChevronRight => &CHEVRON_RIGHT,
            Self::Settings => &SETTINGS,
            Self::Language => &LANGUAGE,
            Self::ChevronDown => &CHEVRON_DOWN,
            Self::ChevronUp => &CHEVRON_UP,
            Self::Schedule => &SCHEDULE,
            Self::Send => &SEND,
            Self::Warning => &WARNING,
            Self::LogoutCompact => &LOGOUT_COMPACT,
        }
    }
}

/// Способ выбрать цвет иконки из темы.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IconTone {
    #[default]
    Inherit, // Цвет окружающего текста.
    Muted,  // Приглушённый цвет темы.
    Accent, // Основной акцент темы.
    Danger, // Цвет опасного действия.
}

/// Создаёт декоративную иконку заданного размера и цвета.
pub fn icon<Message>(name: IconName, size: u32, tone: IconTone) -> Element<'static, Message> {
    Element::new(Icon { name, size, tone })
}

/// Виджет Iced, который рисует одну иконку без собственного состояния.
struct Icon {
    name: IconName, // Имя, по которому выбирается файл SVG.
    size: u32,      // Сторона квадрата иконки в логических пикселях.
    tone: IconTone, // Способ подобрать цвет иконки.
}

impl<Message> Widget<Message, Theme, Renderer> for Icon {
    /// Запрашивает квадрат заданного размера.
    fn size(&self) -> Size<Length> {
        Size::new(self.size.into(), self.size.into())
    }

    /// Рассчитывает место под квадрат с учётом ограничений родительского виджета.
    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, self.size, self.size)
    }

    /// Рисует SVG в доступном квадрате и применяет выбранный цвет.
    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let side = bounds.width.min(bounds.height);
        if side <= 0.0 || !bounds.intersects(viewport) {
            return;
        }
        let color = match self.tone {
            IconTone::Inherit => style.text_color,
            IconTone::Muted => text_muted(theme),
            IconTone::Accent => theme.palette().primary,
            IconTone::Danger => theme.palette().danger,
        };
        let centered = Rectangle {
            x: bounds.center_x() - side / 2.0,
            y: bounds.center_y() - side / 2.0,
            width: side,
            height: side,
        };
        renderer.draw_svg(
            Svg::from(self.name.handle()).color(color),
            centered,
            *viewport,
        );
    }
}
