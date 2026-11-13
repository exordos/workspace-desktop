//! Рисует текстовый пузырь сообщения. Сначала описаны входящие и исходящие сообщения,
//! затем расположение автора, текста, времени и отметки доставки.

use iced::advanced::renderer::{self, Renderer as _};
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{Layout, Widget, layout};
use iced::widget::text::{Style, Wrapping};
use iced::widget::{pin, row, space, stack, text};
use iced::{
    Border, Color, Element, Font, Length, Pixels, Point, Rectangle, Renderer, Size, Theme, font,
    mouse,
};

use super::{IconName, IconTone, icon};

const MAX_WIDTH: f32 = 680.0;
const HORIZONTAL_PADDING: f32 = 12.0;
const VERTICAL_PADDING: f32 = 8.0;
const BODY_HEIGHT: f32 = 20.0;
const META_HEIGHT: f32 = 16.0;
const INLINE_GAP: f32 = 12.0;
const STACKED_GAP: f32 = 4.0;

/// Определяет фон пузыря и возможность показать состояние доставки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDirection {
    Incoming,                          // Сообщение от другого участника.
    Outgoing(Option<MessageDelivery>), // Своё сообщение. `None` скрывает отметку доставки.
}

/// Состояние исходящего сообщения, вычисленное вызывающим экраном.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDelivery {
    Sending,   // Сообщение отправляется.
    Sent,      // Сервер принял сообщение.
    Delivered, // Сообщение дошло до получателя.
    Read,      // Получатель прочитал сообщение.
    Failed,    // Не удалось отправить сообщение.
}

/// Показывает текст, время и, для исходящего сообщения, отметку доставки.
/// Размещение пузыря слева или справа задаёт вызывающий экран.
pub fn message_bubble<'a, Message: 'a>(
    body: &'a str,
    author: Option<&'a str>,
    time: &'a str,
    direction: MessageDirection,
) -> Element<'a, Message> {
    let author = author.filter(|name| !name.trim().is_empty()).map_or_else(
        || space().into(),
        |name| {
            text(name)
                .size(12)
                .line_height(Pixels(META_HEIGHT))
                .font(Font {
                    weight: font::Weight::Semibold,
                    ..Font::DEFAULT
                })
                .style(|theme: &Theme| Style {
                    color: Some(theme.palette().text),
                })
                .wrapping(Wrapping::WordOrGlyph)
                .into()
        },
    );
    let delivery = match direction {
        MessageDirection::Incoming => None,
        MessageDirection::Outgoing(delivery) => delivery,
    };
    let time = text(time)
        .size(12)
        .line_height(Pixels(META_HEIGHT))
        .wrapping(Wrapping::WordOrGlyph)
        .style(|theme| Style {
            color: Some(time_color(theme)),
        });
    let mut meta = row![time].spacing(4).align_y(iced::Center);
    if let Some(delivery) = delivery {
        meta = meta.push(delivery_mark(delivery));
    }
    let body = text(body)
        .size(14)
        .line_height(Pixels(BODY_HEIGHT))
        .wrapping(Wrapping::WordOrGlyph);
    Element::new(Bubble {
        direction,
        delivery,
        children: [author, body.into(), meta.into()],
    })
}

/// Собирает отметку доставки из иконок Workspace.
fn delivery_mark<'a, Message: 'a>(delivery: MessageDelivery) -> Element<'a, Message> {
    match delivery {
        MessageDelivery::Sending => icon(IconName::Schedule, 14, IconTone::Muted, None),
        MessageDelivery::Sent => icon(IconName::Check, 14, IconTone::Muted, None),
        MessageDelivery::Failed => icon(IconName::Warning, 14, IconTone::Danger, None),
        MessageDelivery::Delivered | MessageDelivery::Read => {
            let tone = if delivery == MessageDelivery::Read {
                IconTone::Accent
            } else {
                IconTone::Muted
            };
            let tick = || icon(IconName::Check, 11, tone, None);
            stack![
                space().width(15).height(14),
                pin(tick()).y(2),
                pin(tick()).x(4).y(2),
            ]
            .into()
        }
    }
}

/// Возвращает фон входящего или исходящего сообщения из палитры Blue Mist.
fn bubble_color(theme: &Theme, direction: MessageDirection) -> Color {
    match (theme.extended_palette().is_dark, direction) {
        (false, MessageDirection::Incoming) => Color::WHITE,
        (false, MessageDirection::Outgoing(_)) => Color::from_rgb8(0xdb, 0xe8, 0xf8),
        (true, MessageDirection::Incoming) => Color::from_rgb8(0x32, 0x3c, 0x4a),
        (true, MessageDirection::Outgoing(_)) => Color::from_rgb8(0x2a, 0x3a, 0x50),
    }
}

/// Возвращает цвет времени отправки из палитры Blue Mist.
fn time_color(theme: &Theme) -> Color {
    if theme.extended_palette().is_dark {
        Color::from_rgb8(0x9a, 0xa8, 0xb8)
    } else {
        Color::from_rgb8(0x8d, 0x9b, 0xae)
    }
}

/// Пузырь измеряет текст и время отдельно, чтобы определить, помещаются ли они на одной строке.
struct Bubble<'a, Message> {
    direction: MessageDirection,       // Цвет пузыря и сторона нижнего угла.
    delivery: Option<MessageDelivery>, // Подпись отметки. У входящего сообщения её нет.
    children: [Element<'a, Message>; 3], // Автор или пустое место, текст сообщения, время и отметка.
}

impl<Message> Widget<Message, Theme, Renderer> for Bubble<'_, Message> {
    /// Создаёт отдельное состояние для каждого из текстовых элементов.
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    /// Обновляет разметку дочерних виджетов после смены сообщения.
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    /// Значения и содержимое задают ширину и высоту пузыря.
    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    /// Помещает время справа от короткого текста или под многострочным сообщением.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let limits = limits.max_width(MAX_WIDTH);
        let padding = Size::new(HORIZONTAL_PADDING * 2.0, VERTICAL_PADDING * 2.0);
        let available = limits.shrink(padding);
        // Высоту сообщения определяет текст, даже если пузырь виден лишь частично при прокрутке.
        let content =
            layout::Limits::new(Size::ZERO, Size::new(available.max().width, f32::INFINITY));
        let [author, body, meta] = std::array::from_fn(|index| {
            self.children[index].as_widget_mut().layout(
                &mut tree.children[index],
                renderer,
                &content,
            )
        });

        let author_bottom = author.size().height;
        let body_y = if author_bottom > 0.0 {
            author_bottom + STACKED_GAP
        } else {
            0.0
        };
        let body_bottom = body_y + body.size().height;
        let inline_width = body.size().width + INLINE_GAP + meta.size().width;
        let inline = body.size().height <= BODY_HEIGHT && inline_width <= available.max().width;
        let body_meta_width = if inline {
            inline_width
        } else {
            body.size().width.max(meta.size().width)
        };
        let width = author
            .size()
            .width
            .max(body_meta_width)
            .max(available.min().width);
        let meta_y = if inline {
            (body_bottom - meta.size().height).max(body_y)
        } else {
            body_bottom + STACKED_GAP
        };
        let height = body_bottom.max(meta_y + meta.size().height);
        let meta_x = HORIZONTAL_PADDING + width - meta.size().width;

        layout::Node::with_children(
            limits.resolve(
                Length::Shrink,
                Length::Shrink,
                Size::new(width, height) + padding,
            ),
            vec![
                author.move_to(Point::new(HORIZONTAL_PADDING, VERTICAL_PADDING)),
                body.move_to(Point::new(HORIZONTAL_PADDING, VERTICAL_PADDING + body_y)),
                meta.move_to(Point::new(meta_x, VERTICAL_PADDING + meta_y)),
            ],
        )
    }

    /// Рисует общую поверхность и расположенные на ней текст и отметки.
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if !bounds.intersects(viewport) {
            return;
        }
        let radius = match self.direction {
            MessageDirection::Incoming => Border::default().rounded(8).radius.bottom_left(4),
            MessageDirection::Outgoing(_) => Border::default().rounded(8).radius.bottom_right(4),
        };
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: Border::default().rounded(radius),
                ..renderer::Quad::default()
            },
            bubble_color(theme, self.direction),
        );
        for ((child, state), layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child
                .as_widget()
                .draw(state, renderer, theme, style, layout, cursor, viewport);
        }
    }

    /// Даёт Iced время, текст и подпись отметки доставки при обходе виджетов.
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, state), layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child
                    .as_widget_mut()
                    .operate(state, layout, renderer, operation);
            }
            if let Some(delivery) = self.delivery {
                let label = match delivery {
                    MessageDelivery::Sending => "Отправляется",
                    MessageDelivery::Sent => "Отправлено",
                    MessageDelivery::Delivered => "Доставлено",
                    MessageDelivery::Read => "Прочитано",
                    MessageDelivery::Failed => "Ошибка отправки",
                };
                operation.text(None, layout.child(2).bounds(), label);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use iced::advanced::renderer::Headless;
    use iced::advanced::widget::Id;
    use iced_runtime::user_interface::{Cache, UserInterface};

    use super::*;

    /// Собирает координаты текстовых элементов из настоящей разметки Iced.
    #[derive(Default)]
    struct RenderedText(Vec<(String, Rectangle)>); // Видимый текст и его прямоугольник.

    impl Operation for RenderedText {
        /// Просматривает детей контейнера, включая время в строке отметок.
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }

        /// Запоминает текст и его координаты.
        fn text(&mut self, _: Option<&Id>, bounds: Rectangle, value: &str) {
            self.0.push((value.to_owned(), bounds));
        }
    }

    impl RenderedText {
        /// Размещает пузырь с помощью TinySkia без открытия окна.
        fn measure(view: Element<'_, ()>, width: f32) -> Self {
            let mut renderer = block_on(<Renderer as Headless>::new(
                Font::DEFAULT,
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
            let mut ui =
                UserInterface::build(view, Size::new(width, 480.0), Cache::new(), &mut renderer);
            let mut rendered = Self::default();
            ui.operate(&renderer, &mut rendered);
            rendered
        }

        /// Возвращает координаты указанной подписи.
        fn bounds(&self, label: &str) -> Rectangle {
            self.0.iter().find(|(text, _)| label == text).unwrap().1
        }
    }

    /// У короткого текста есть место для времени и отметки на той же строке.
    #[test]
    fn short_message_keeps_meta_beside_body() {
        let rendered = RenderedText::measure(
            message_bubble(
                "Короткое сообщение",
                Some("Анна"),
                "12:34",
                MessageDirection::Outgoing(Some(MessageDelivery::Read)),
            ),
            360.0,
        );
        let author = rendered.bounds("Анна");
        let body = rendered.bounds("Короткое сообщение");
        let time = rendered.bounds("12:34");
        let status = rendered.bounds("Прочитано");
        assert!(author.y + author.height < body.y);
        assert!(body.x + body.width < time.x);
        assert!(time.y < body.y + body.height);
        assert!(status.width > time.width);
    }

    /// Несколько строк и длинное слово укладываются по ширине, время идёт ниже текста.
    #[test]
    fn multiline_message_keeps_meta_below_wrapped_body() {
        let body = "Первая строка\nДлинноеСловоДлинноеСловоДлинноеСловоДлинноеСлово";
        let rendered = RenderedText::measure(
            message_bubble(body, None, "12:34", MessageDirection::Incoming),
            180.0,
        );
        let body = rendered.bounds(body);
        let time = rendered.bounds("12:34");
        assert!(body.height > BODY_HEIGHT * 2.0);
        assert!(body.x + body.width <= 180.0 - HORIZONTAL_PADDING + 1.0);
        assert!(time.y > body.y + body.height);
        assert_eq!(rendered.0.len(), 2);
    }
}
