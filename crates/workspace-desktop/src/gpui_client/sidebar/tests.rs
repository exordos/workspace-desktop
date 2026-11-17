//! Проверяет штатные действия каталога в тестовом окне GPUI без сети.

use std::{cell::RefCell, rc::Rc};

use gpui::{
    KeyBinding, KeyUpEvent, Keystroke, Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext,
    TouchPhase, point, size,
};
use workspace_core::context::{AccountId, OwnerContext, SessionGeneration};

use super::*;

gpui::actions!(sidebar_test, [NextFocus]);

/// Записывает только публичное действие, которое увидит оболочка.
#[derive(Debug, PartialEq)]
enum Recorded {
    Open(ChatRoute, SessionGeneration), // Конкретный разговор и поколение каталога.
    Retry(AccountSession),              // Вход, для которого будет перезапущена загрузка.
}

/// Готовит разные входы, у которых могут встречаться одинаковые UUID строк.
fn session(account: &str, generation: u64) -> AccountSession {
    AccountSession {
        owner: OwnerContext {
            account_id: AccountId(account.into()),
            server: "https://workspace.test".into(),
            organization_id: "org".into(),
            project_id: format!("project-{account}"),
            user_id: account.into(),
        },
        generation: SessionGeneration(generation),
    }
}

/// Создаёт длинную личную строку, канал или тему с фиксированным UUID.
fn chat(id: u128, name: &str, direct: bool, topics: &[(u128, &str)]) -> Chat {
    Chat {
        id: Uuid::from_u128(id),
        name: name.into(),
        direct,
        topics: topics
            .iter()
            .map(|(id, name)| Topic {
                id: Uuid::from_u128(*id),
                name: (*name).into(),
            })
            .collect(),
    }
}

/// Формирует снимок, который передаст оболочка.
fn catalog(session: &AccountSession, status: ChatListStatus, chats: Vec<Chat>) -> ChatList {
    ChatList {
        generation: session.generation,
        chats: Some(chats.into()),
        status,
    }
}

/// Окно теста подключает ту же команду Tab, которую предоставляет оболочка.
struct TestRoot {
    sidebar: Entity<Sidebar>, // Показанный в окне каталог.
}

impl Render for TestRoot {
    /// Окружает каталог обработчиком обхода фокуса.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .on_action(|_: &NextFocus, window, _| window.focus_next())
            .child(self.sidebar.clone())
    }
}

/// Содержит показанный экран и события, переданные его настоящим UI.
type Harness<'a> = (
    Entity<Sidebar>,
    &'a mut gpui::VisualTestContext,
    Rc<RefCell<Vec<Recorded>>>,
);

/// Подключает поиск и записывает действия, отправленные наверх.
fn initialize(cx: &mut TestAppContext) -> Harness<'_> {
    cx.update(|app| {
        Sidebar::bind_keys(app);
        app.bind_keys([KeyBinding::new("tab", NextFocus, None)]);
    });
    let (root, ui) = cx.add_window_view(|_, cx| TestRoot {
        sidebar: cx.new(Sidebar::new),
    });
    let sidebar = ui.read(|app| root.read(app).sidebar.clone());
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorded = events.clone();
    ui.update(|_, app| {
        app.subscribe(&sidebar, move |_, action, _| {
            let event = match action {
                SidebarAction::OpenChat { route, generation } => {
                    Recorded::Open(route.clone(), *generation)
                }
                SidebarAction::RetryCatalog(session) => Recorded::Retry(session.clone()),
            };
            recorded.borrow_mut().push(event);
        })
        .detach();
    });
    (sidebar, ui, events)
}

/// Передаёт выбранный вход и ждёт его отрисовку.
fn publish(
    sidebar: &Entity<Sidebar>,
    ui: &mut gpui::VisualTestContext,
    owner: &AccountSession,
    snapshot: &ChatList,
) {
    ui.update(|_, app| {
        sidebar.update(app, |sidebar, cx| {
            sidebar.update_snapshot(Some(owner), Some(snapshot), None, cx)
        })
    });
    ui.run_until_parked();
}

/// Нажимает рабочий элемент по его границам в кадре.
fn click(ui: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = ui
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Нет элемента {selector}"));
    ui.simulate_click(bounds.center(), Modifiers::none());
}

/// Активирует сфокусированную строку штатным обработчиком GPUI.
fn enter(ui: &mut gpui::VisualTestContext) {
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").expect("клавиша Enter"),
    });
    ui.run_until_parked();
}

/// Идентификатор тестового элемента по UUID, а не по индексу списка.
fn row(stream: u128, topic: Option<u128>) -> &'static str {
    match (stream, topic) {
        (1, None) => "catalog-chat-00000000-0000-0000-0000-000000000001",
        (2, None) => "catalog-chat-00000000-0000-0000-0000-000000000002",
        (2, Some(3)) => {
            "catalog-topic-00000000-0000-0000-0000-000000000002-00000000-0000-0000-0000-000000000003"
        }
        (2, Some(4)) => {
            "catalog-topic-00000000-0000-0000-0000-000000000002-00000000-0000-0000-0000-000000000004"
        }
        _ => panic!("Строка отсутствует в фикстуре"),
    }
}

/// Адрес, который должен получить обработчик открытия разговора.
fn open(owner: &AccountSession, stream: u128, topic: Option<u128>) -> Recorded {
    Recorded::Open(
        ChatRoute {
            owner: owner.owner.clone(),
            scope: HistoryScope {
                stream_id: Uuid::from_u128(stream),
                topic_id: topic.map(Uuid::from_u128),
            },
            entry: ChatEntry::Latest,
        },
        owner.generation,
    )
}

/// Проверяет выбор мышью и клавиатурой, поиск и его изоляцию при смене аккаунта.
#[gpui::test]
fn opens_and_filters_only_the_catalog_of_the_selected_session(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let a = session("a", 11);
    let b = session("b", 12);
    let name = "Очень длинное имя собеседника, которое должно помещаться в узкую строку";
    let a_catalog = catalog(
        &a,
        ChatListStatus::Ready,
        vec![
            chat(1, name, true, &[]),
            chat(
                2,
                "Команда",
                false,
                &[(3, "Важная тема"), (4, "Соседняя тема")],
            ),
        ],
    );
    let b_catalog = catalog(
        &b,
        ChatListStatus::Ready,
        vec![chat(
            2,
            "Другая команда",
            false,
            &[(3, "Тема другого аккаунта")],
        )],
    );
    ui.simulate_resize(size(px(190.), px(490.)));
    publish(&sidebar, ui, &a, &a_catalog);
    let narrow = ui.debug_bounds(row(1, None)).expect("видна длинная строка");
    let viewport = ui.debug_bounds("catalog-scroll").expect("область строк");
    assert!(
        narrow.size.width > px(0.) && narrow.right() <= viewport.right(),
        "{narrow:?} вышла за {viewport:?}"
    );
    click(ui, row(1, None));
    assert_eq!(events.borrow().as_slice(), &[open(&a, 1, None)]);

    click(ui, row(2, None));
    assert!(ui.debug_bounds(row(2, Some(3))).is_some());
    click(ui, "catalog-search");
    ui.simulate_input("ВАЖ");
    ui.read(|app| {
        let active = sidebar.read(app).active.as_ref().expect("аккаунт A");
        assert_eq!(active.search.read(app).text(), "ВАЖ");
    });
    assert!(ui.debug_bounds(row(2, Some(3))).is_some());
    // Узел от предыдущего кадра может ещё быть в карте debug_bounds GPUI.
    // Нажимаем его старое место: отфильтрованный чат не должен открыться.
    ui.simulate_click(narrow.center(), Modifiers::none());
    assert_eq!(events.borrow().as_slice(), &[open(&a, 1, None)]);
    click(ui, "catalog-search");
    // Первый Tab проходит по кнопке очистки; второй доходит до найденной темы.
    ui.simulate_keystrokes("tab tab");
    enter(ui);
    assert_eq!(
        events.borrow().as_slice(),
        &[open(&a, 1, None), open(&a, 2, Some(3))]
    );

    publish(&sidebar, ui, &b, &b_catalog);
    ui.read(|app| {
        let active = sidebar.read(app).active.as_ref().expect("аккаунт B");
        assert_eq!(active.search.read(app).text(), "", "B получил запрос A");
        assert!(active.expanded.is_empty(), "B получил раскрытие A");
    });
    enter(ui);
    assert_eq!(events.borrow().len(), 2, "B получил фокус темы A");

    publish(&sidebar, ui, &a, &a_catalog);
    ui.read(|app| {
        let active = sidebar
            .read(app)
            .active
            .as_ref()
            .expect("повторный выбор A");
        assert_eq!(active.search.read(app).text(), "ВАЖ");
        assert!(active.expanded.contains(&Uuid::from_u128(2)));
    });
    click(ui, "catalog-clear");
    assert!(
        ui.debug_bounds(row(2, Some(4))).is_some(),
        "потерялось ручное раскрытие выбранного входа"
    );
    ui.simulate_input("несуществующее название");
    assert!(
        ui.debug_bounds("catalog-empty").is_some(),
        "очистка не вернула фокус или нет пустого результата"
    );
}

/// Проверяет повтор запроса, привязку к поколению и сброс прокрутки между входами.
#[gpui::test]
fn retries_exact_generation_and_resets_scroll_on_account_switch(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let a = session("a", 11);
    let b = session("b", 12);
    let rows = || {
        (1..=14)
            .map(|id| chat(id, &format!("Разговор {id}"), true, &[]))
            .collect()
    };
    let failed = catalog(&a, ChatListStatus::Failed, rows());
    ui.simulate_resize(size(px(260.), px(245.)));
    publish(&sidebar, ui, &a, &failed);
    assert!(
        ui.debug_bounds(row(1, None)).is_some(),
        "ошибка скрыла кэш каталога"
    );
    click(ui, "catalog-retry");
    assert_eq!(events.borrow().as_slice(), &[Recorded::Retry(a.clone())]);

    let ready_a = catalog(&a, ChatListStatus::Ready, rows());
    publish(&sidebar, ui, &a, &ready_a);
    let viewport = ui
        .debug_bounds("catalog-scroll")
        .expect("область прокрутки");
    ui.simulate_event(ScrollWheelEvent {
        position: viewport.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-130.))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    ui.run_until_parked();
    ui.read(|app| assert!(sidebar.read(app).active.as_ref().unwrap().scroll.offset().y < px(0.)));

    // Запоздавший снимок предыдущего входа в B не выдаётся за текущий каталог.
    publish(&sidebar, ui, &b, &failed);
    click(ui, "catalog-retry");
    assert_eq!(events.borrow().as_slice(), &[Recorded::Retry(a.clone())]);
    ui.read(|app| assert!(sidebar.read(app).active.as_ref().unwrap().catalog.is_none()));
    publish(
        &sidebar,
        ui,
        &b,
        &catalog(&b, ChatListStatus::Ready, rows()),
    );
    ui.read(|app| {
        assert_eq!(
            sidebar.read(app).active.as_ref().unwrap().scroll.offset().y,
            px(0.)
        )
    });
    click(ui, row(1, None));
    assert_eq!(
        events.borrow().as_slice(),
        &[Recorded::Retry(a), open(&b, 1, None)]
    );
}

/// Удерживает фокус UUID при обновлении порядка и очищает его при повторном входе.
#[gpui::test]
fn keeps_keyed_focus_on_catalog_updates_but_not_after_sign_in(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let a = session("a", 11);
    let again = session("a", 13);
    let snapshot = catalog(
        &a,
        ChatListStatus::Ready,
        vec![chat(1, "Первый", true, &[]), chat(2, "Второй", true, &[])],
    );
    publish(&sidebar, ui, &a, &snapshot);
    click(ui, row(1, None));
    let swapped = catalog(
        &a,
        ChatListStatus::Ready,
        vec![chat(2, "Второй", true, &[]), chat(1, "Первый", true, &[])],
    );
    publish(&sidebar, ui, &a, &swapped);
    enter(ui);
    assert_eq!(
        events.borrow().as_slice(),
        &[open(&a, 1, None), open(&a, 1, None)]
    );
    let new_snapshot = catalog(
        &again,
        ChatListStatus::Ready,
        vec![chat(1, "Первый", true, &[])],
    );
    publish(&sidebar, ui, &again, &new_snapshot);
    enter(ui);
    assert_eq!(events.borrow().len(), 2);
}
