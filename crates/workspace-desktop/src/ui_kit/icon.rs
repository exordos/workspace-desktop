use std::sync::LazyLock;

use iced::advanced::svg::{Handle, Renderer as _, Svg};
use iced::advanced::widget::Tree;
use iced::advanced::{Layout, Widget, layout, renderer};
use iced::{Element, Length, Rectangle, Renderer, Size, Theme, mouse};

use super::theme::text_muted;

#[derive(Debug, Clone, Copy)]
pub enum IconName {
    Plus,
    Search,
    Close,
    Attach,
}

impl IconName {
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

        match self {
            Self::Plus => &PLUS,
            Self::Search => &SEARCH,
            Self::Close => &CLOSE,
            Self::Attach => &ATTACH,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IconTone {
    #[default]
    Inherit,
    Muted,
    Accent,
    Danger,
}

/// Render a decorative icon. Inherit uses the surrounding widget's text color.
pub fn icon<Message>(name: IconName, size: u32, tone: IconTone) -> Element<'static, Message> {
    Element::new(Icon { name, size, tone })
}

struct Icon {
    name: IconName,
    size: u32,
    tone: IconTone,
}

impl<Message> Widget<Message, Theme, Renderer> for Icon {
    fn size(&self) -> Size<Length> {
        Size::new(self.size.into(), self.size.into())
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, self.size, self.size)
    }

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
