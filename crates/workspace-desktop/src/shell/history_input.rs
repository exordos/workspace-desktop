//! Передаёт ввод списку истории и сообщает оболочке о пользовательской прокрутке.

use iced::advanced::layout;
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Layout, Shell};
use iced::{Element, Event, Length, Rectangle, Size, mouse, touch};

use super::Message;

/// Наблюдает за жестом, не меняя обработку событий дочерним списком.
pub(super) struct HistoryInput<'a> {
    content: Element<'a, Message>, // Штатный прокручиваемый список.
    revision: u64,                 // Открытие, которому принадлежит действие.
}

/// Состояние кнопки для наблюдения за перетаскиванием полосы прокрутки.
#[derive(Default)]
struct InputState {
    dragging: bool, // Кнопка мыши нажата внутри списка.
}

impl<'a> HistoryInput<'a> {
    /// Оборачивает список, сохраняя его обычное поведение.
    pub(super) fn new(content: Element<'a, Message>, revision: u64) -> Self {
        Self { content, revision }
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for HistoryInput<'_> {
    /// Хранит состояние нажатия между событиями.
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<InputState>()
    }
    /// Создаёт пустое состояние ввода.
    fn state(&self) -> tree::State {
        tree::State::new(InputState::default())
    }
    /// Сохраняет дерево дочернего списка.
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    /// Сопоставляет новое представление с прежним деревом списка.
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }
    /// Использует размер списка.
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    /// Передаёт расчёт размеров списку.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    /// Передаёт операции измерения дочернему списку.
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    /// Сначала обрабатывает ввод списком, затем сообщает о жесте оболочке.
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        let state = tree.state.downcast_mut::<InputState>();
        let over = cursor.is_over(layout.bounds());
        let gesture = match event {
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if over => match delta {
                mouse::ScrollDelta::Lines { y, .. } | mouse::ScrollDelta::Pixels { y, .. } => {
                    if *y < 0.0 {
                        shell.publish(Message::HistoryManualScroll(self.revision));
                    }
                    *y > 0.0
                }
            },
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                // Полоса захватывает нажатие; обычный клик по тексту не запускает догрузку.
                state.dragging = over && shell.is_event_captured();
                if state.dragging {
                    shell.publish(Message::HistoryManualScroll(self.revision));
                }
                false
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.dragging = false;
                false
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => state.dragging,
            Event::Touch(touch::Event::FingerMoved { position, .. }) => {
                layout.bounds().contains(*position)
            }
            _ => false,
        };
        if gesture {
            shell.publish(Message::HistoryUserScrolled {
                revision: self.revision,
            });
        }
    }
    /// Сохраняет указатель, выбранный дочерним списком.
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    /// Рисует дочерний список без изменений.
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    /// Передаёт всплывающие элементы дочернего списка.
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: iced::Vector,
    ) -> Option<iced::advanced::overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}
