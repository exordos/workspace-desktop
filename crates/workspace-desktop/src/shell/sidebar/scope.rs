//! Привязывает состояние виджета к сессии или строке каталога.

use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Layout, Shell, layout, overlay, renderer};
use iced::{Element, Event, Length, Rectangle, Renderer, Size, Theme, Vector, mouse};

use super::Message;

/// Передаёт работу дочернему виджету и пересоздаёт его состояние при смене ключа.
struct Scoped<'a, Key> {
    key: Key,                      // Сессия или строка, которой принадлежит состояние Iced.
    content: Element<'a, Message>, // Виджет каталога или его строки.
}

/// Отдельный тип помечает дерево именно этой обёрткой.
struct ScopeState<Key>(Key); // Ключ разметки, создавшей дочернее состояние.

/// Не переносит фокус, удержанное нажатие или прокрутку между разными ключами.
pub(super) fn bind<'a, Key: Clone + Eq + 'static>(
    key: Key,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    Element::new(Scoped { key, content })
}

impl<Key: Clone + Eq + 'static> Widget<Message, Theme, Renderer> for Scoped<'_, Key> {
    /// Различает деревья с разными типами ключей.
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<ScopeState<Key>>()
    }

    /// Запоминает владельца дочернего дерева.
    fn state(&self) -> tree::State {
        tree::State::new(ScopeState(self.key.clone()))
    }

    /// Создаёт дочернее дерево для первой разметки.
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    /// При смене ключа отбрасывает старое состояние, даже если Iced передал то же место в списке.
    fn diff(&self, tree: &mut Tree) {
        let key = &mut tree.state.downcast_mut::<ScopeState<Key>>().0;
        if *key == self.key {
            tree.children[0].diff(&self.content);
        } else {
            *key = self.key.clone();
            tree.children[0] = Tree::new(&self.content);
        }
    }

    /// Использует размер дочернего виджета.
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    /// Сохраняет подсказки дочернего виджета для раскладки родителя.
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    /// Передаёт разметку дочернему виджету без собственного отступа.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    /// Сохраняет доступность дочернего виджета для измерения, фокуса и прокрутки.
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    /// Передаёт ввод дочернему виджету.
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
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
    }

    /// Возвращает указатель, который выбрал дочерний виджет.
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    /// Рисует дочерний виджет с той же геометрией и стилем.
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

    /// Передаёт наложения дочернего виджета поверх каталога.
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}
