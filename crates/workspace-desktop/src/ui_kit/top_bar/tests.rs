//! Проверяет шапку в программном рендерере Iced через события мыши и клавиатуры.

use futures::executor::block_on;
use iced::advanced::{clipboard, renderer::Headless};
use iced::{Font, Point, event};
use iced_runtime::user_interface::{Cache, UserInterface};

use super::*;

/// Собирает тестовые аккаунты с разными состояниями счётчика.
fn accounts() -> Vec<TopBarAccount<'static, &'static str>> {
    [
        ("a", "Анна", Some(0)),
        ("b", "Борис", Some(1001)),
        (
            "c",
            "Слишком длинная подпись тестового аккаунта для короткой строки меню",
            None,
        ),
        ("d", "Дина", Some(4)),
    ]
    .into_iter()
    .map(|(id, label, unread)| TopBarAccount {
        id,
        generation: 1,
        label,
        detail: "workspace.example · project".to_owned(),
        selected: id == "a",
        unread,
        on_select: id,
    })
    .collect()
}

/// Создаёт программный рендерер без открытия окна.
fn renderer() -> Renderer {
    block_on(<Renderer as Headless>::new(
        Font::DEFAULT,
        iced::Pixels(14.0),
        Some("tiny-skia"),
    ))
    .expect("Tiny Skia доступна в тестах Workspace Desktop")
}

/// Собирает границы элементов шапки и текст, переданный для отрисовки.
#[derive(Default)]
struct Screen {
    regions: Vec<(Id, Rectangle)>, // Области с именами, включая открытую панель.
    labels: Vec<(String, Rectangle)>, // Текст и его область в текущей разметке.
    menu_focused: bool,            // Признак фокуса на стрелке меню.
}

impl Screen {
    /// Читает текущую разметку виджетов и видимой панели меню.
    fn capture(
        ui: &mut UserInterface<'_, &'static str, Theme, Renderer>,
        renderer: &Renderer,
    ) -> Self {
        let mut screen = Self::default();
        ui.operate(renderer, &mut screen);
        screen
    }

    /// Возвращает область найденного элемента.
    fn bounds(&self, key: &str) -> Option<Rectangle> {
        let key = Id::from(key.to_owned());
        self.regions
            .iter()
            .find_map(|(id, bounds)| (id == &key).then_some(*bounds))
    }

    /// Проверяет, что строка содержит нужный видимый текст.
    fn contains_label(&self, value: &str) -> bool {
        self.labels.iter().any(|(text, _)| text == value)
    }
}

impl Operation for Screen {
    /// Обходит детей контейнера и содержимое панели.
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    /// Запоминает именованную область контейнера.
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if let Some(id) = id {
            self.regions.push((id.clone(), bounds));
        }
    }

    /// Запоминает положение кнопки меню и наличие фокуса.
    fn focusable(&mut self, id: Option<&Id>, bounds: Rectangle, state: &mut dyn Focusable) {
        if id == Some(&Id::new(TOP_BAR_ACCOUNT_MENU_ID)) {
            self.container(id, bounds);
            self.menu_focused = state.is_focused();
        }
    }

    /// Запоминает отображаемую подпись для проверки открытого меню.
    fn text(&mut self, _: Option<&Id>, bounds: Rectangle, value: &str) {
        self.labels.push((value.to_owned(), bounds));
    }
}

/// Передаёт щелчок в Iced по координатам внутри нужной области.
fn click(
    ui: &mut UserInterface<'_, &'static str, Theme, Renderer>,
    renderer: &mut Renderer,
    point: Point,
    messages: &mut Vec<&'static str>,
) {
    for event in [
        mouse::Event::ButtonPressed(mouse::Button::Left),
        mouse::Event::ButtonReleased(mouse::Button::Left),
    ] {
        pointer_event(ui, renderer, point, event, messages);
    }
}

/// Передаёт отдельное событие мыши и возвращает результат обработки.
fn pointer_event(
    ui: &mut UserInterface<'_, &'static str, Theme, Renderer>,
    renderer: &mut Renderer,
    point: Point,
    event: mouse::Event,
    messages: &mut Vec<&'static str>,
) -> event::Status {
    let (_, statuses) = ui.update(
        &[Event::Mouse(event)],
        mouse::Cursor::Available(point),
        renderer,
        &mut clipboard::Null,
        messages,
    );
    statuses[0]
}

/// Передаёт нажатие клавиши и показывает, получил ли его другой виджет.
fn press(
    ui: &mut UserInterface<'_, &'static str, Theme, Renderer>,
    renderer: &mut Renderer,
    key: Named,
    modifiers: keyboard::Modifiers,
    messages: &mut Vec<&'static str>,
) -> event::Status {
    let event = Event::Keyboard(keyboard::Event::KeyPressed {
        key: Key::Named(key),
        modified_key: Key::Named(key),
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    });
    let (_, statuses) = ui.update(
        &[event],
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        messages,
    );
    statuses[0]
}

/// Проверяет геометрический центр и меню при разном количестве аккаунтов и ширине.
#[test]
fn top_bar_centers_chats_and_keeps_account_menu_reachable() {
    let mut renderer = renderer();
    for width in [100.0, 320.0, 620.0, 960.0] {
        for count in [0, 1, 3, 4] {
            let mut accounts = accounts();
            accounts.truncate(count);
            let mut ui = UserInterface::build(
                top_bar(accounts),
                Size::new(width, 360.0),
                Cache::new(),
                &mut renderer,
            );
            let screen = Screen::capture(&mut ui, &renderer);
            let chats = screen.bounds("top-bar-chats").expect("раздел «Чаты» виден");
            assert!(
                (chats.center_x() - width / 2.0).abs() < 1.0,
                "width={width}, count={count}, {chats:?}"
            );
            assert_eq!(screen.bounds(TOP_BAR_ACCOUNT_MENU_ID).is_some(), count > 0);
            if let Some(trigger) = screen.bounds(TOP_BAR_ACCOUNT_MENU_ID) {
                assert!(trigger.x + trigger.width <= chats.x);
            }
            if width >= 620.0 {
                assert_eq!(screen.contains_label("+1"), count == 4);
                assert_eq!(screen.bounds("top-bar-unread-dot-b").is_some(), count >= 3);
                assert!(screen.bounds("top-bar-unread-dot-a").is_none());
                assert!(screen.bounds("top-bar-unread-dot-c").is_none());
                assert!(screen.bounds("top-bar-unread-dot-d").is_none());
            }
        }
    }
}

/// Tab остаётся внутри меню, Escape возвращает к кнопке, а клавиши выбирают скрытый аккаунт.
#[test]
fn top_bar_account_menu_keeps_focus_until_escape_or_selection() {
    let mut renderer = renderer();
    let mut ui = UserInterface::build(
        top_bar(accounts()),
        Size::new(620.0, 360.0),
        Cache::new(),
        &mut renderer,
    );
    let mut messages = Vec::new();
    let screen = Screen::capture(&mut ui, &renderer);
    let trigger = screen.bounds(TOP_BAR_ACCOUNT_MENU_ID).unwrap().center();
    click(&mut ui, &mut renderer, trigger, &mut messages);
    assert!(Screen::capture(&mut ui, &renderer).contains_label("Дина"));
    for modifiers in [keyboard::Modifiers::empty(), keyboard::Modifiers::SHIFT] {
        assert_eq!(
            press(&mut ui, &mut renderer, Named::Tab, modifiers, &mut messages),
            event::Status::Captured
        );
        let open = Screen::capture(&mut ui, &renderer);
        assert!(open.menu_focused);
        assert!(open.contains_label("Дина"));
    }
    let no_modifiers = keyboard::Modifiers::empty();
    press(
        &mut ui,
        &mut renderer,
        Named::Escape,
        no_modifiers,
        &mut messages,
    );
    let closed = Screen::capture(&mut ui, &renderer);
    assert!(!closed.contains_label("Дина"));
    assert!(closed.menu_focused);

    press(
        &mut ui,
        &mut renderer,
        Named::Enter,
        no_modifiers,
        &mut messages,
    );
    let reopened = Screen::capture(&mut ui, &renderer);
    assert!(reopened.contains_label("1k+"));
    assert_eq!(
        reopened.bounds(TOP_BAR_SELECTED_ACCOUNT_ID),
        reopened.bounds("top-bar-account-a")
    );
    press(
        &mut ui,
        &mut renderer,
        Named::End,
        no_modifiers,
        &mut messages,
    );
    press(
        &mut ui,
        &mut renderer,
        Named::Enter,
        no_modifiers,
        &mut messages,
    );
    assert_eq!(messages, ["d"]);
    assert!(Screen::capture(&mut ui, &renderer).menu_focused);
}

/// Проверяет, что выбор мышью из меню отправляет событие скрытого аккаунта и закрывает меню.
#[test]
fn top_bar_account_menu_selects_hidden_account_with_pointer() {
    let mut renderer = renderer();
    let mut ui = UserInterface::build(
        top_bar(accounts()),
        Size::new(620.0, 360.0),
        Cache::new(),
        &mut renderer,
    );
    let mut messages = Vec::new();
    let trigger = Screen::capture(&mut ui, &renderer)
        .bounds(TOP_BAR_ACCOUNT_MENU_ID)
        .unwrap()
        .center();
    click(&mut ui, &mut renderer, trigger, &mut messages);
    let row = Screen::capture(&mut ui, &renderer)
        .bounds("top-bar-account-d")
        .unwrap()
        .center();
    click(&mut ui, &mut renderer, row, &mut messages);
    assert_eq!(messages, ["d"]);
    let closed = Screen::capture(&mut ui, &renderer);
    assert!(closed.bounds("top-bar-account-d").is_none());
    assert!(closed.menu_focused);
}

/// Первый щелчок по соседней кнопке закрывает меню. Второй выбирает её аккаунт.
#[test]
fn top_bar_menu_does_not_pass_outside_click_to_quick_account() {
    let mut renderer = renderer();
    let mut ui = UserInterface::build(
        top_bar(accounts()),
        Size::new(620.0, 360.0),
        Cache::new(),
        &mut renderer,
    );
    let screen = Screen::capture(&mut ui, &renderer);
    let dot = screen.bounds("top-bar-unread-dot-b").unwrap().center();
    let trigger = screen.bounds(TOP_BAR_ACCOUNT_MENU_ID).unwrap().center();
    let mut messages = Vec::new();
    click(&mut ui, &mut renderer, trigger, &mut messages);
    assert!(Screen::capture(&mut ui, &renderer).contains_label("Дина"));
    click(&mut ui, &mut renderer, dot, &mut messages);
    assert!(messages.is_empty());
    assert!(!Screen::capture(&mut ui, &renderer).contains_label("Дина"));

    click(&mut ui, &mut renderer, dot, &mut messages);
    assert_eq!(messages, ["b"]);
    assert!(!Screen::capture(&mut ui, &renderer).contains_label("Дина"));
}

/// Прокрутка за пределами меню остаётся у меню и не закрывает его.
#[test]
fn top_bar_menu_captures_wheel_outside_while_open() {
    let mut renderer = renderer();
    let mut ui = UserInterface::build(
        top_bar(accounts()),
        Size::new(620.0, 360.0),
        Cache::new(),
        &mut renderer,
    );
    let mut messages = Vec::new();
    let trigger = Screen::capture(&mut ui, &renderer)
        .bounds(TOP_BAR_ACCOUNT_MENU_ID)
        .unwrap()
        .center();
    click(&mut ui, &mut renderer, trigger, &mut messages);
    let outside = Point::new(600.0, 140.0);
    let wheel = mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
    };
    assert_eq!(
        pointer_event(&mut ui, &mut renderer, outside, wheel, &mut messages),
        event::Status::Captured
    );
    assert!(Screen::capture(&mut ui, &renderer).contains_label("Дина"));
    click(&mut ui, &mut renderer, outside, &mut messages);
    assert_eq!(
        pointer_event(&mut ui, &mut renderer, outside, wheel, &mut messages),
        event::Status::Ignored
    );
    assert!(messages.is_empty());
}

/// Проверяет выбор с обновлением данных между нажатием и отпусканием указателя.
fn select_across_snapshot(
    menu: bool,
    update: impl FnOnce(&mut Vec<TopBarAccount<'static, &'static str>>),
    at_pointer: &str,
) -> (Vec<&'static str>, bool) {
    let mut renderer = renderer();
    let make_accounts = || {
        let mut records = accounts();
        records[2].unread = Some(2);
        records
    };
    let size = Size::new(620.0, 360.0);
    let mut ui = UserInterface::build(top_bar(make_accounts()), size, Cache::new(), &mut renderer);
    let mut messages = Vec::new();
    let control = if menu {
        let trigger = Screen::capture(&mut ui, &renderer)
            .bounds(TOP_BAR_ACCOUNT_MENU_ID)
            .unwrap()
            .center();
        click(&mut ui, &mut renderer, trigger, &mut messages);
        TOP_BAR_ACCOUNT_ROW_ID_PREFIX
    } else {
        TOP_BAR_UNREAD_DOT_ID_PREFIX
    };
    let pressed = Screen::capture(&mut ui, &renderer)
        .bounds(&format!("{control}b"))
        .unwrap()
        .center();
    pointer_event(
        &mut ui,
        &mut renderer,
        pressed,
        mouse::Event::ButtonPressed(mouse::Button::Left),
        &mut messages,
    );
    let mut fresh = make_accounts();
    update(&mut fresh);
    let mut ui = UserInterface::build(top_bar(fresh), size, ui.into_cache(), &mut renderer);
    assert!(
        Screen::capture(&mut ui, &renderer)
            .bounds(&format!("{control}{at_pointer}"))
            .unwrap()
            .contains(pressed)
    );
    pointer_event(
        &mut ui,
        &mut renderer,
        pressed,
        mouse::Event::ButtonReleased(mouse::Button::Left),
        &mut messages,
    );
    let menu_still_open = Screen::capture(&mut ui, &renderer).contains_label("Дина");
    (messages, menu_still_open)
}

/// Перестановка или удаление аккаунта отменяет нажатие, если под мышью оказалась другая строка.
#[test]
fn top_bar_never_selects_another_account_after_snapshot_reorders() {
    for menu in [false, true] {
        let (reordered_messages, still_open) =
            select_across_snapshot(menu, |records| records.swap(1, 2), "c");
        assert!(reordered_messages.is_empty());
        assert_eq!(still_open, menu);

        let (removed_messages, still_open) = select_across_snapshot(
            menu,
            |records| {
                records.remove(0);
            },
            "c",
        );
        assert!(removed_messages.is_empty());
        assert_eq!(still_open, menu);
    }
}

/// Смена поколения отменяет нажатие, даже если тот же аккаунт остался на своём месте.
#[test]
fn top_bar_does_not_apply_pointer_release_to_another_session_generation() {
    for menu in [false, true] {
        let (messages, still_open) = select_across_snapshot(
            menu,
            |records| {
                records[1].generation += 1;
                records[1].on_select = "new-b";
            },
            "b",
        );
        assert!(messages.is_empty());
        assert_eq!(still_open, menu);
    }
}

/// Обновление счётчика не сбрасывает нажатие того же аккаунта и входа.
#[test]
fn top_bar_keeps_account_selection_when_only_unread_changes() {
    for menu in [false, true] {
        let (messages, still_open) = select_across_snapshot(
            menu,
            |records| {
                records[1].unread = Some(25);
            },
            "b",
        );
        assert_eq!(messages, ["b"]);
        assert!(!still_open);
    }
}
