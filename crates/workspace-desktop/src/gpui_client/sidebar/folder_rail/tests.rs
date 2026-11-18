//! Проверяет рабочую панель папок и всплывающие слои через события GPUI.

use std::{cell::RefCell, rc::Rc};

use gpui::{
    KeyUpEvent, Keystroke, Modifiers, MouseButton, TestAppContext, VisualTestContext, size,
};

use super::*;

/// Содержит панель, окно и действия, которые получает родитель.
type Harness<'a> = (
    Entity<FolderRail>,
    &'a mut VisualTestContext,
    Rc<RefCell<Vec<RailAction>>>,
);

/// Открывает всплывающие слои через те же привязки, что и рабочий каталог.
fn setup(cx: &mut TestAppContext) -> Harness<'_> {
    cx.update(|app| {
        TextInput::bind_keys(app);
        FolderRail::bind_keys(app);
        crate::gpui_client::ui::TopBar::bind_keys(app);
    });
    let (rail, ui) = cx.add_window_view(|_, cx| FolderRail::new(cx));
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorded = events.clone();
    ui.update(|_, app| {
        app.subscribe(&rail, move |_, action, _| {
            recorded.borrow_mut().push(*action)
        })
        .detach();
    });
    (rail, ui, events)
}

/// Создаёт папку из тестового снимка без обращений к серверу.
fn entry(id: u128, kind: FolderKind, title: &str) -> RailEntry {
    RailEntry {
        id: Uuid::from_u128(id),
        kind,
        title: title.into(),
        color: (kind == FolderKind::Custom).then_some(0xff0d_9488),
        unread: None,
    }
}

/// Публикует снимок в окне и дожидается разметки.
fn publish(rail: &Entity<FolderRail>, ui: &mut VisualTestContext, state: RailState) {
    ui.update(|_, app| rail.update(app, |rail, cx| rail.set_state(state, cx)));
    ui.run_until_parked();
}

/// Нажимает элемент, который виден в текущем кадре.
fn click(ui: &mut VisualTestContext, id: &'static str) {
    let bounds = ui
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("Нет элемента {id}"));
    ui.simulate_click(bounds.center(), Modifiers::none());
    ui.run_until_parked();
}

/// Системные пункты доступны, системное удаление пропускается, фокус возвращается кнопке.
#[gpui::test]
fn context_menu_respects_rights_and_preserves_focus(cx: &mut TestAppContext) {
    let (rail, ui, events) = setup(cx);
    ui.simulate_resize(size(px(520.), px(500.)));
    let mut state = RailState {
        entries: vec![
            entry(1, FolderKind::Personal, "Личные"),
            entry(18, FolderKind::Custom, "Сводки"),
        ],
        selected: ALL_CHATS_ID,
        vertical: true,
        show_system: true,
        can_manage: false,
        needs_refresh: true,
    };
    publish(&rail, ui, state.clone());
    let all = ui
        .debug_bounds("folder-rail-entry-00000000-0000-0000-0000-000000000000")
        .expect("«Все чаты» есть до получения системного снимка")
        .center();
    ui.simulate_mouse_down(all, MouseButton::Right, Modifiers::none());
    ui.simulate_mouse_up(all, MouseButton::Right, Modifiers::none());
    ui.run_until_parked();
    assert!(ui.debug_bounds("folder-rail-menu").is_some());
    // End пропускает удаление и повтор, пока серверные команды недоступны.
    ui.simulate_keystrokes("end");
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    ui.run_until_parked();
    assert_eq!(events.borrow().as_slice(), &[RailAction::ToggleSystem]);
    ui.update(|window, app| assert!(rail.read(app).focuses[&ALL_CHATS_ID].is_focused(window)));
    ui.simulate_keystrokes("shift-f10");
    ui.run_until_parked();
    ui.read(|app| assert!(rail.read(app).overlay.is_some()));

    state.show_system = false;
    state.vertical = false;
    state.can_manage = true;
    publish(&rail, ui, state);
    assert!(ui.debug_bounds("folder-rail-horizontal").is_some());
    ui.read(|app| {
        let rail = rail.read(app);
        assert!(rail.overlay.is_none());
        assert_eq!(
            rail.state
                .visible()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            [FolderKind::All, FolderKind::Custom]
        );
    });
    click(ui, "folder-rail-create");
    assert_eq!(
        events.borrow().as_slice(),
        &[RailAction::ToggleSystem, RailAction::Create]
    );
}

/// На узкой панели быстрый поиск выбирает нужную папку по Enter и пропадает при расширении.
#[gpui::test]
fn quick_list_uses_real_overflow_and_filters_folder_names(cx: &mut TestAppContext) {
    let (rail, ui, events) = setup(cx);
    ui.simulate_resize(size(px(520.), px(190.)));
    publish(
        &rail,
        ui,
        RailState {
            entries: vec![
                entry(41, FolderKind::Custom, "Сводки"),
                entry(42, FolderKind::Custom, "Пилот первый"),
                entry(43, FolderKind::Custom, "Пилот второй"),
            ],
            selected: ALL_CHATS_ID,
            vertical: true,
            show_system: true,
            can_manage: true,
            needs_refresh: false,
        },
    );
    click(ui, "folder-rail-quick");
    assert!(ui.debug_bounds("folder-rail-quick-list").is_some());
    ui.simulate_input("ПИЛОТ");
    ui.simulate_keystrokes("down enter");
    ui.run_until_parked();
    assert_eq!(
        events.borrow().as_slice(),
        &[RailAction::Select(Uuid::from_u128(43))]
    );
    ui.update(|window, app| {
        let rail = rail.read(app);
        assert!(rail.overlay.is_none());
        assert!(rail.quick_focus.is_focused(window));
    });
    ui.simulate_resize(size(px(520.), px(900.)));
    ui.run_until_parked();
    ui.read(|app| assert!(!rail.read(app).overflow));
}
