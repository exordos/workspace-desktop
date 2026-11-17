//! Проверяет фокус при перестановке и прокрутку при смене входа на виджетах Iced.

use std::sync::Arc;

use iced::advanced::widget::{
    Operation,
    operation::{
        Focusable, Scrollable,
        scrollable::{AbsoluteOffset, scroll_to},
    },
};
use iced::keyboard::{
    self, Key, Location, Modifiers,
    key::{Named, NativeCode, Physical},
};
use iced::{Event, Font, Pixels, Point, Rectangle, Renderer, Size, Theme, Vector, mouse};
use iced_runtime::{
    core::clipboard,
    user_interface::{Cache, UserInterface},
};
use workspace_core::auth::SessionStatus;
use workspace_core::context::{AccountId, OwnerContext};

use super::*;

/// Готовит активный вход с отдельным владельцем и номером поколения.
fn session(generation: u64) -> SessionSnapshot {
    SessionSnapshot {
        context: OwnerContext {
            account_id: AccountId(format!("Аккаунт {generation}")),
            server: "https://workspace.example".into(),
            organization_id: "organization".into(),
            project_id: "project".into(),
            user_id: format!("user-{generation}"),
        },
        generation: SessionGeneration(generation),
        status: SessionStatus::Active,
        token_version: 0,
    }
}

/// Создаёт каталог с устойчивыми UUID и заданным порядком строк.
fn catalog(session: &SessionSnapshot, ids: impl IntoIterator<Item = u128>) -> ChatList {
    let chats: Vec<_> = ids
        .into_iter()
        .map(|id| Chat {
            id: Uuid::from_u128(id),
            name: format!("Чат {id}"),
            direct: true,
            topics: Vec::new(),
        })
        .collect();
    ChatList {
        generation: session.generation,
        chats: Some(Arc::from(chats)),
        status: ChatListStatus::Ready,
    }
}

/// Строит каталог в той же обёртке, которую использует экран приложения.
fn panel<'a>(session: &SessionSnapshot, catalog: &'a ChatList) -> Element<'a, Message> {
    keyed::Column::new()
        .push(session.generation, view(session, Some(catalog), None, None))
        .width(320)
        .height(Fill)
        .into()
}

/// Находит координаты полной подписи в строке каталога.
struct FindLabel(
    &'static str,      // Искомая подпись.
    Option<Rectangle>, // Границы найденной подписи или None.
);

impl Operation for FindLabel {
    /// Обходит вложенные кнопки и подписи.
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    /// Запоминает место строки, когда подпись совпала.
    fn text(&mut self, _: Option<&Id>, bounds: Rectangle, value: &str) {
        if value == self.0 {
            self.1 = Some(bounds);
        }
    }
}

/// Переводит фокус на кнопку в известной точке окна.
struct FocusAt(Point); // Точка внутри проверяемой кнопки.

impl Operation for FocusAt {
    /// Обходит вложенные виджеты, чтобы найти кнопку.
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    /// Фокусирует кнопку, которую пользователь видит под этой точкой.
    fn focusable(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn Focusable) {
        if bounds.contains(self.0) {
            state.focus();
        } else {
            state.unfocus();
        }
    }
}

/// Активирует текущий фокус штатным нажатием Enter.
fn press_enter(
    ui: &mut UserInterface<'_, Message, Theme, Renderer>,
    renderer: &mut Renderer,
) -> Vec<Message> {
    let mut messages = Vec::new();
    let _ = ui.update(
        &[Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::Enter),
            modified_key: Key::Named(Named::Enter),
            physical_key: Physical::Unidentified(NativeCode::Unidentified),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            text: None,
            repeat: false,
        })],
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    messages
}

/// Проверяет, что Enter не открывает другой чат, который занял старое место строки.
#[test]
fn reordered_catalog_never_activates_the_wrong_chat() {
    let session = session(1);
    let initial = catalog(&session, 1..=8);
    let reordered = catalog(&session, [2, 1, 3, 4, 5, 6, 7, 8]);
    let mut renderer = Renderer::new(Font::DEFAULT, Pixels(16.0));
    let size = Size::new(320.0, 340.0);
    let mut ui = UserInterface::build(panel(&session, &initial), size, Cache::new(), &mut renderer);
    let mut label = FindLabel("Чат 2", None);
    ui.operate(&renderer, &mut label);
    ui.operate(&renderer, &mut FocusAt(label.1.unwrap().center()));

    let mut same = UserInterface::build(
        panel(&session, &initial),
        size,
        ui.into_cache(),
        &mut renderer,
    );
    assert!(matches!(
        press_enter(&mut same, &mut renderer).as_slice(),
        [Message::OpenChat { route, .. }] if route.scope.stream_id == Uuid::from_u128(2)
    ));

    let mut after = UserInterface::build(
        panel(&session, &reordered),
        size,
        same.into_cache(),
        &mut renderer,
    );
    let messages = press_enter(&mut after, &mut renderer);
    assert!(
        matches!(messages.as_slice(), [])
            || matches!(
                messages.as_slice(),
                [Message::OpenChat { route, .. }] if route.scope.stream_id == Uuid::from_u128(2)
            ),
        "фокус старой строки не должен открыть соседний чат: {messages:?}"
    );
}

/// Читает положение прокрутки нужного входа после разметки.
struct CatalogOffset(
    Id,          // Идентификатор списка.
    Option<f32>, // Абсолютный сдвиг найденного списка по вертикали.
);

impl Operation for CatalogOffset {
    /// Доходит до прокручиваемой области.
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    /// Не смешивает прокрутку разных областей.
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        translation: Vector,
        _: &mut dyn Scrollable,
    ) {
        if id == Some(&self.0) {
            self.1 = Some(translation.y);
        }
    }
}

/// Проверяет, что позиция остаётся в том же входе и сбрасывается при смене аккаунта.
#[test]
fn another_account_starts_at_the_top_of_its_catalog() {
    let first = session(1);
    let second = session(2);
    let first_list = catalog(&first, 1..=14);
    let second_list = catalog(&second, 101..=114);
    let mut renderer = Renderer::new(Font::DEFAULT, Pixels(16.0));
    let size = Size::new(320.0, 340.0);
    let mut before = UserInterface::build(
        panel(&first, &first_list),
        size,
        Cache::new(),
        &mut renderer,
    );
    before.operate(
        &renderer,
        &mut scroll_to::<()>(
            scroll_id(first.generation),
            AbsoluteOffset {
                x: None,
                y: Some(140.0),
            },
        ),
    );

    let mut first_again = UserInterface::build(
        panel(&first, &first_list),
        size,
        before.into_cache(),
        &mut renderer,
    );
    let mut original_offset = CatalogOffset(scroll_id(first.generation), None);
    first_again.operate(&renderer, &mut original_offset);
    assert_eq!(original_offset.1, Some(140.0));

    let mut after = UserInterface::build(
        panel(&second, &second_list),
        size,
        first_again.into_cache(),
        &mut renderer,
    );
    let mut new_offset = CatalogOffset(scroll_id(second.generation), None);
    after.operate(&renderer, &mut new_offset);
    assert_eq!(new_offset.1, Some(0.0));
}
