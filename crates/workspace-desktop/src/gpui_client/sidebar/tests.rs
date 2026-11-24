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
                metadata: Default::default(),
            })
            .collect(),
        metadata: Default::default(),
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
        crate::gpui_client::ui::TopBar::bind_keys(app);
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
                SidebarAction::OpenActivity { .. } | SidebarAction::OpenCreatedTopic { .. } => {
                    return;
                }
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

const CHANNEL_CHEVRON: &str = "catalog-stream-expand-00000000-0000-0000-0000-000000000002";
const CHANNEL_PREVIEW: &str = "catalog-stream-preview-00000000-0000-0000-0000-000000000002";
const TOPICS_TOGGLE: &str = "catalog-topic-toggle-00000000-0000-0000-0000-000000000002";

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

    click(ui, CHANNEL_CHEVRON);
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
        assert!(
            active.presentation.expanded().is_empty(),
            "B получил раскрытие A"
        );
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
        assert!(active.presentation.is_expanded(Uuid::from_u128(2)));
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
    ui.simulate_keystrokes("escape");
    ui.read(|app| {
        let active = sidebar.read(app).active.as_ref().expect("аккаунт A");
        assert!(active.search.read(app).text().is_empty());
    });
}

/// Проверяет геометрию по сценам Electron и удержание строки при смене ширины.
#[gpui::test]
fn fixture_catalog_keeps_metadata_inside_rows_at_both_reference_widths(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let a = session("a", 11);
    let b = session("b", 12);
    let scenes: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/plans/roadmap/gpui-ui-parity-fixtures/scenes.json"
    )))
    .unwrap();
    let direct = scenes["direct"]["title"].as_str().unwrap();
    let channel = scenes["channel"]["title"].as_str().unwrap();
    let topic = &scenes["channel"]["topics"][0];
    let first = topic["title"].as_str().unwrap();
    let second = scenes["channel"]["topics"][1]["title"].as_str().unwrap();
    let catalog_a = catalog(
        &a,
        ChatListStatus::Ready,
        vec![
            chat(2, channel, false, &[(3, first), (4, second)]),
            chat(1, direct, true, &[]),
        ],
    );
    publish(&sidebar, ui, &a, &catalog_a);
    let scope = HistoryScope {
        stream_id: Uuid::from_u128(2),
        topic_id: Some(Uuid::from_u128(3)),
    };
    let route = ConversationAddress {
        route: ChatRoute {
            owner: a.owner.clone(),
            scope,
            entry: ChatEntry::Latest,
        },
        generation: a.generation,
        revision: Default::default(),
    };
    ui.update(|_, app| {
        sidebar.update(app, |sidebar, cx| {
            sidebar.update_snapshot(Some(&a), Some(&catalog_a), Some(&route), cx);
            let active = sidebar.active.as_mut().unwrap();
            active.fixture_details.insert(
                RowKey::Topic(scope.stream_id, scope.topic_id.unwrap()),
                RowDetails {
                    preview: Some(ChatRowPreview {
                        sender: Some("Вы".into()),
                        text: topic["messages"][2]["text"]
                            .as_str()
                            .unwrap()
                            .to_owned()
                            .into(),
                    }),
                    time: Some("13:13".into()),
                    unread: Some((topic["unread"].as_u64().unwrap(), BadgeTone::Accent)),
                    ..Default::default()
                },
            );
            cx.notify();
        });
    });
    ui.run_until_parked();

    // 299 и 280 — внутренние ширины каталога в окнах UI‑05 1440 и 700.
    for (window_width, catalog_width) in [(1440., 299.), (700., 280.)] {
        ui.simulate_resize(size(px(catalog_width), px(600.)));
        ui.run_until_parked();
        let direct_row = ui.debug_bounds(row(1, None)).unwrap();
        let topic_row = ui.debug_bounds(row(2, Some(3))).unwrap();
        let accent = ui.debug_bounds("catalog-topic-bar").unwrap();
        let preview = ui.debug_bounds("chat-row-preview").unwrap();
        let time = ui.debug_bounds("chat-row-time").unwrap();
        let badge = ui.debug_bounds("unread-badge").unwrap();
        assert_eq!(direct_row.left(), px(8.));
        assert_eq!(direct_row.size.width, px(catalog_width - 16.));
        assert_eq!(direct_row.size.height, px(64.));
        assert_eq!(topic_row.left(), direct_row.left());
        assert_eq!(accent.left(), topic_row.left() + px(38.));
        assert_eq!(accent.size.width, px(3.));
        assert!(preview.right() < badge.left() && badge.right() <= topic_row.right());
        assert_eq!(badge.size.height, px(20.));
        assert!(
            badge.bottom() <= time.top(),
            "бейдж {badge:?} перекрывает {time:?}"
        );
        assert!(time.bottom() <= topic_row.bottom());
        ui.read(|app| {
            assert_eq!(
                sidebar.read(app).active.as_ref().unwrap().selected,
                Some(scope)
            );
        });
        println!(
            "fixture_window_px={window_width} catalog_px={catalog_width} chat_width_px={:?} chat_height_px={:?} topic_width_px={:?} badge_height_px={:?}",
            direct_row.size.width, direct_row.size.height, topic_row.size.width, badge.size.height,
        );
    }
    click(ui, "catalog-search");
    ui.simulate_input("шаблон не совпадает");
    assert!(ui.debug_bounds("catalog-empty").is_some());
    click(ui, "catalog-clear");
    ui.simulate_input("ПРОВЕРКА");
    ui.simulate_keystrokes("down");
    enter(ui);
    assert_eq!(events.borrow().as_slice(), &[open(&a, 2, Some(3))]);

    // UUID темы такой же, но её проекция и счётчик принадлежали A.
    let catalog_b = catalog(
        &b,
        ChatListStatus::Ready,
        vec![chat(2, channel, false, &[(3, first)])],
    );
    publish(&sidebar, ui, &b, &catalog_b);
    click(ui, CHANNEL_CHEVRON);
    ui.read(|app| {
        let active = sidebar.read(app).active.as_ref().unwrap();
        assert!(
            active
                .row_details(
                    RowKey::Topic(scope.stream_id, scope.topic_id.unwrap()),
                    RowDetails::default()
                )
                .unread
                .is_none()
        );
        assert_ne!(active.selected, Some(scope));
    });
    click(ui, row(2, Some(3)));
    assert_eq!(
        events.borrow().as_slice(),
        &[open(&a, 2, Some(3)), open(&b, 2, Some(3))]
    );
}

/// Различает ожидание без списка, блокирующую ошибку с повтором и пустой ответ.
#[gpui::test]
fn catalog_status_follows_loading_retry_and_empty_snapshot(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let a = session("a", 11);
    ui.simulate_resize(size(px(280.), px(470.)));
    for (status, selector) in [
        (ChatListStatus::Loading, "catalog-loading"),
        (ChatListStatus::Failed, "catalog-error"),
        (ChatListStatus::Ready, "catalog-empty"),
    ] {
        publish(&sidebar, ui, &a, &catalog(&a, status, vec![]));
        let bounds = ui.debug_bounds(selector).unwrap();
        assert_eq!(bounds.left(), px(12.));
        assert_eq!(bounds.size.width, px(256.));
        if status == ChatListStatus::Failed {
            click(ui, "catalog-retry");
        }
    }
    assert_eq!(events.borrow().as_slice(), &[Recorded::Retry(a)]);
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

/// После прокрутки, поиска и смены входа подписки соответствуют видимым личным чатам.
#[gpui::test]
fn avatar_requests_follow_visible_direct_chat_rows(cx: &mut TestAppContext) {
    let (sidebar, ui, _) = initialize(cx);
    let a = session("a", 11);
    let b = session("b", 12);
    let chats = |first| {
        (1..=20)
            .map(|id| {
                let title = if id == 20 {
                    "Искомая личка"
                } else {
                    "Чат"
                };
                let mut row = chat(id, title, id != 2, &[]);
                row.metadata.direct_peer_id = Some(if id == 1 {
                    first
                } else {
                    Uuid::from_u128(100 + id)
                });
                row
            })
            .collect()
    };
    let users = |ui: &gpui::VisualTestContext| {
        ui.read(|app| {
            sidebar
                .read(app)
                .active
                .as_ref()
                .unwrap()
                .avatar_users
                .clone()
        })
    };
    let top = Uuid::from_u128(101);
    let channel = Uuid::from_u128(102);
    let last = Uuid::from_u128(120);

    ui.simulate_resize(size(px(300.), px(260.)));
    publish(
        &sidebar,
        ui,
        &a,
        &catalog(&a, ChatListStatus::Ready, chats(top)),
    );
    let viewport = ui
        .debug_bounds("catalog-scroll")
        .expect("область прокрутки");
    ui.run_until_parked();
    let initial = users(ui);
    assert!(initial.contains(&top));
    assert!(!initial.contains(&channel) && !initial.contains(&last));

    ui.simulate_event(ScrollWheelEvent {
        position: viewport.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-360.))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    ui.run_until_parked();
    let scrolled = users(ui);
    assert!(!scrolled.is_empty() && !scrolled.contains(&top) && !scrolled.contains(&last));

    click(ui, "catalog-search");
    ui.simulate_input("Искомая");
    ui.run_until_parked();
    assert_eq!(users(ui), vec![last]);

    let next_person = Uuid::from_u128(201);
    publish(
        &sidebar,
        ui,
        &b,
        &catalog(&b, ChatListStatus::Ready, chats(next_person)),
    );
    let switched = users(ui);
    assert!(switched.contains(&next_person) && !switched.contains(&last));
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

/// Проверяет правый клик, Shift+F10, границы, недоступное действие и смену входа.
#[gpui::test]
fn row_menu_uses_navigation_and_ignores_click_through(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let a = session("a", 11);
    let b = session("b", 12);
    let a_catalog = catalog(
        &a,
        ChatListStatus::Ready,
        (1..=9)
            .map(|id| chat(id, "Разговор", id != 9, &[]))
            .collect(),
    );
    ui.simulate_resize(size(px(700.), px(600.)));
    publish(&sidebar, ui, &a, &a_catalog);
    let direct = ui.debug_bounds(row(1, None)).unwrap().center();
    ui.simulate_mouse_down(direct, MouseButton::Right, Modifiers::none());
    ui.simulate_mouse_up(direct, MouseButton::Right, Modifiers::none());
    ui.run_until_parked();
    assert!(ui.debug_bounds("catalog-menu").is_some());
    assert!(events.borrow().is_empty());

    // Снаружи меню находится та же рабочая строка. Щелчок только закрывает список.
    ui.simulate_click(point(direct.x - px(24.), direct.y), Modifiers::none());
    assert!(events.borrow().is_empty());
    ui.update(|window, app| {
        let active = sidebar.read(app).active.as_ref().unwrap();
        assert!(active.menu.is_none());
        assert!(active.focus[&RowKey::Chat(Uuid::from_u128(1))].is_focused(window));
    });

    ui.simulate_keystrokes("shift-f10");
    ui.run_until_parked();
    ui.read(|app| {
        assert!(
            sidebar.read(app).active.as_ref().unwrap().menu.is_some(),
            "Shift+F10"
        )
    });
    ui.simulate_keystrokes("tab");
    ui.update(|window, app| assert!(sidebar.read(app).menu_focus.is_focused(window)));
    ui.simulate_keystrokes("escape");
    ui.simulate_keystrokes("shift-f10");
    enter(ui);
    assert_eq!(events.borrow().as_slice(), &[open(&a, 1, None)]);

    let last_row = ui
        .debug_bounds("catalog-chat-00000000-0000-0000-0000-000000000009")
        .unwrap();
    let edge = point(last_row.right() - px(3.), px(599.));
    assert!(last_row.contains(&edge), "{last_row:?}");
    ui.simulate_mouse_down(edge, MouseButton::Right, Modifiers::none());
    ui.simulate_mouse_up(edge, MouseButton::Right, Modifiers::none());
    ui.run_until_parked();
    let popup = ui.debug_bounds("catalog-menu").unwrap();
    assert!(
        popup.right() <= px(696.) && popup.bottom() <= px(596.),
        "{popup:?}"
    );
    click(ui, "catalog-menu-action");
    enter(ui);
    assert_eq!(events.borrow().as_slice(), &[open(&a, 1, None)]);
    ui.update(|window, app| assert!(sidebar.read(app).menu_focus.is_focused(window)));

    let b_catalog = catalog(
        &b,
        ChatListStatus::Ready,
        vec![chat(9, "Другой разговор", true, &[])],
    );
    publish(&sidebar, ui, &b, &b_catalog);
    ui.update(|window, app| {
        let active = sidebar.read(app).active.as_ref().unwrap();
        assert!(active.menu.is_none());
        assert!(active.search.read(app).is_focused(window));
    });
    enter(ui);
    assert_eq!(events.borrow().as_slice(), &[open(&a, 1, None)]);
}

/// Проверяет разные маршруты строки и превью, а также независимую стрелку канала.
#[gpui::test]
fn stream_header_preview_and_chevron_have_separate_actions(cx: &mut TestAppContext) {
    check_header_actions(cx, false);
}

/// Проверяет те же маршруты и кнопки в личном чате.
#[gpui::test]
fn personal_header_uses_the_same_topic_actions(cx: &mut TestAppContext) {
    check_header_actions(cx, true);
}

/// Проверяет раздельные действия общего заголовка в обоих видах разговоров.
fn check_header_actions(cx: &mut TestAppContext, direct: bool) {
    use workspace_core::storage::HistoryTime;

    let (sidebar, ui, events) = initialize(cx);
    let a = session("a", 11);
    let mut stream = chat(2, "Разговор", direct, &[(3, "Общая"), (4, "Обсуждение")]);
    let message_id = Uuid::from_u128(90);
    stream.metadata.default_topic_id = Some(Uuid::from_u128(3));
    stream.metadata.last_message_id = Some(message_id);
    stream.metadata.preview = Some(SidebarPreview {
        message_id,
        topic_id: Uuid::from_u128(4),
        author_id: Uuid::from_u128(91),
        author_name: Some("Собеседник".into()),
        created_at: HistoryTime::parse("2026-11-17T12:30:00Z").unwrap(),
        text: "Последнее сообщение".into(),
    });
    let snapshot = catalog(&a, ChatListStatus::Ready, vec![stream]);
    ui.simulate_resize(size(px(280.), px(490.)));
    publish(&sidebar, ui, &a, &snapshot);

    click(ui, CHANNEL_CHEVRON);
    assert!(events.borrow().is_empty(), "стрелка открыла разговор");
    assert!(ui.debug_bounds(row(2, Some(3))).is_some());
    click(ui, CHANNEL_CHEVRON);
    assert!(
        events.borrow().is_empty(),
        "стрелка открыла разговор при сворачивании"
    );

    let header = ui.debug_bounds(row(2, None)).unwrap();
    ui.simulate_click(
        point(header.left() + px(70.), header.top() + px(13.)),
        Modifiers::none(),
    );
    assert_eq!(events.borrow().as_slice(), &[open(&a, 2, Some(3))]);
    click(ui, CHANNEL_PREVIEW);
    assert_eq!(
        events.borrow().as_slice(),
        &[open(&a, 2, Some(3)), open(&a, 2, Some(4))]
    );

    ui.update(|window, app| {
        AppPreferences::select(app, Some(&a.owner));
        AppPreferences::update(app, |settings| settings.chat_click = ChatClick::LatestTopic)
            .unwrap();
        window.refresh();
    });
    ui.run_until_parked();
    let header = ui.debug_bounds(row(2, None)).unwrap();
    ui.simulate_click(
        point(header.left() + px(70.), header.top() + px(13.)),
        Modifiers::none(),
    );
    click(
        ui,
        "catalog-stream-avatar-00000000-0000-0000-0000-000000000002",
    );
    assert_eq!(
        events.borrow().as_slice(),
        &[
            open(&a, 2, Some(3)),
            open(&a, 2, Some(4)),
            open(&a, 2, Some(4)),
            open(&a, 2, Some(3))
        ]
    );
}

/// Раскрывает все темы личного чата и открывает скрытую тему через поиск.
#[gpui::test]
fn personal_topics_expand_and_remain_searchable_when_collapsed(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let a = session("a", 11);
    let mut direct = chat(
        2,
        "Коллега",
        true,
        &[
            (3, "Общая"),
            (4, "Дата"),
            (5, "Задача"),
            (6, "Итоги встречи"),
            (7, "Архив"),
        ],
    );
    direct.metadata.default_topic_id = Some(Uuid::from_u128(3));
    direct.topics.last_mut().unwrap().metadata.is_done = true;
    let snapshot = catalog(&a, ChatListStatus::Ready, vec![direct]);
    let meeting =
        "catalog-topic-00000000-0000-0000-0000-000000000002-00000000-0000-0000-0000-000000000006";
    let archived =
        "catalog-topic-00000000-0000-0000-0000-000000000002-00000000-0000-0000-0000-000000000007";
    ui.simulate_resize(size(px(280.), px(600.)));
    publish(&sidebar, ui, &a, &snapshot);

    click(ui, CHANNEL_CHEVRON);
    assert!(ui.debug_bounds(row(2, Some(3))).is_some());
    assert!(ui.debug_bounds(meeting).is_none());
    click(ui, TOPICS_TOGGLE);
    assert!(ui.debug_bounds(meeting).is_some());
    assert!(ui.debug_bounds(archived).is_none());
    click(ui, TOPICS_TOGGLE);
    assert!(ui.debug_bounds(archived).is_some());
    assert!(events.borrow().is_empty());

    click(ui, CHANNEL_CHEVRON);
    ui.run_until_parked();
    let meeting_key = RowKey::Topic(Uuid::from_u128(2), Uuid::from_u128(6));
    // GPUI хранит debug_bounds и для строк из прежних кадров.
    let topic_is_visible = |app: &gpui::App| {
        sidebar
            .read(app)
            .active
            .as_ref()
            .unwrap()
            .bounds
            .borrow()
            .contains_key(&meeting_key)
    };
    assert!(!ui.read(topic_is_visible));
    click(ui, "catalog-search");
    ui.simulate_input("ИТогИ");
    ui.run_until_parked();
    assert!(ui.read(topic_is_visible));
    click(ui, meeting);
    assert_eq!(events.borrow().as_slice(), &[open(&a, 2, Some(6))]);
}

/// Проверяет переход по результатам и возврат фокуса после показа и скрытия остальных тем.
#[gpui::test]
fn search_navigates_channel_topics_and_reveal_by_keyboard(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let a = session("a", 11);
    let mut stream = chat(2, "Канал", false, &[(3, "Общая"), (4, "Искомая")]);
    let stream_id = stream.id;
    let general_id = Uuid::from_u128(3);
    stream.metadata.default_topic_id = Some(general_id);
    let snapshot = catalog(&a, ChatListStatus::Ready, vec![stream]);
    ui.simulate_resize(size(px(280.), px(430.)));
    publish(&sidebar, ui, &a, &snapshot);

    click(ui, "catalog-search");
    ui.simulate_input("  ИСКОМАЯ  ");
    ui.simulate_keystrokes("down");
    enter(ui);
    ui.simulate_keystrokes("down");
    enter(ui);
    assert_eq!(
        events.borrow().as_slice(),
        &[open(&a, 2, Some(3)), open(&a, 2, Some(4))]
    );

    ui.simulate_keystrokes("down");
    enter(ui);
    assert!(ui.debug_bounds("catalog-other-topics").is_some());
    ui.update(|window, app| {
        let active = sidebar.read(app).active.as_ref().unwrap();
        assert!(active.focus[&RowKey::Topic(stream_id, general_id)].is_focused(window));
    });
    ui.simulate_keystrokes("down");
    enter(ui);
    ui.update(|window, app| {
        let active = sidebar.read(app).active.as_ref().unwrap();
        assert!(active.focus[&RowKey::TopicToggle(stream_id)].is_focused(window));
        assert!(!active.presentation.is_expanded(stream_id));
    });
    assert_eq!(events.borrow().len(), 2);
}

/// Нажатие из старого кадра не раскрывает другой набор тем с тем же числом строк.
#[gpui::test]
fn ignores_reveal_click_across_catalog_replacement(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let a = session("a", 11);
    let base = [(3, "Первая"), (4, "Вторая"), (5, "Третья"), (6, "Старая")];
    let next = [(3, "Первая"), (4, "Вторая"), (5, "Третья"), (7, "Новая")];
    let stream_id = Uuid::from_u128(2);
    ui.simulate_resize(size(px(280.), px(480.)));
    publish(
        &sidebar,
        ui,
        &a,
        &catalog(
            &a,
            ChatListStatus::Ready,
            vec![chat(2, "Канал", false, &base)],
        ),
    );
    click(ui, CHANNEL_CHEVRON);
    let location = ui.debug_bounds(TOPICS_TOGGLE).unwrap().center();
    ui.simulate_mouse_down(location, MouseButton::Left, Modifiers::none());
    publish(
        &sidebar,
        ui,
        &a,
        &catalog(
            &a,
            ChatListStatus::Ready,
            vec![chat(2, "Канал", false, &next)],
        ),
    );
    ui.simulate_mouse_up(location, MouseButton::Left, Modifiers::none());
    ui.run_until_parked();
    ui.read(|app| {
        let active = sidebar.read(app).active.as_ref().unwrap();
        let stream = active.chat(stream_id).unwrap();
        assert_eq!(
            active
                .presentation
                .group(stream, None)
                .unwrap()
                .topics
                .len(),
            3
        );
    });
    assert!(events.borrow().is_empty());
    click(ui, TOPICS_TOGGLE);
    ui.read(|app| {
        let active = sidebar.read(app).active.as_ref().unwrap();
        let stream = active.chat(stream_id).unwrap();
        let group = active.presentation.group(stream, None).unwrap();
        assert_eq!(group.topics.last().unwrap().name, "Новая");
    });
}

/// Внешняя тема раскрывается после загрузки каталога и при выходе из режима поиска.
#[gpui::test]
fn reveals_external_route_after_catalog_load_and_search_clear(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let owner = session("a", 11);
    let stream_id = Uuid::from_u128(2);
    let loading = ChatList {
        generation: owner.generation,
        status: ChatListStatus::Loading,
        chats: None,
    };
    let ready = catalog(
        &owner,
        ChatListStatus::Ready,
        vec![chat(
            2,
            "Канал",
            false,
            &[
                (3, "Первая"),
                (4, "Вторая"),
                (5, "Третья"),
                (6, "Четвёртая"),
                (7, "Пятая"),
            ],
        )],
    );
    let submit_route = |ui: &mut gpui::VisualTestContext, snapshot: &ChatList, id: u128| {
        let route = ConversationAddress {
            route: ChatRoute {
                owner: owner.owner.clone(),
                scope: HistoryScope {
                    stream_id,
                    topic_id: Some(Uuid::from_u128(id)),
                },
                entry: ChatEntry::Latest,
            },
            generation: owner.generation,
            revision: Default::default(),
        };
        ui.update(|_, app| {
            sidebar.update(app, |sidebar, cx| {
                sidebar.update_snapshot(Some(&owner), Some(snapshot), Some(&route), cx);
            });
        });
        ui.run_until_parked();
    };
    let target_is_visible = |ui: &mut gpui::VisualTestContext, id: u128| {
        ui.read(|app| {
            let active = sidebar.read(app).active.as_ref().unwrap();
            let stream = active.chat(stream_id).unwrap();
            let group = active.presentation.group(stream, active.selected).unwrap();
            group
                .topics
                .iter()
                .any(|topic| topic.id == Uuid::from_u128(id))
        })
    };

    ui.simulate_resize(size(px(280.), px(490.)));
    submit_route(ui, &loading, 6);
    submit_route(ui, &ready, 6);
    assert!(target_is_visible(ui, 6));
    click(ui, CHANNEL_CHEVRON);
    assert!(!target_is_visible(ui, 6));
    submit_route(ui, &ready, 6);
    assert!(!target_is_visible(ui, 6));

    click(ui, "catalog-search");
    ui.simulate_input("не совпадает");
    submit_route(ui, &ready, 7);
    assert!(ui.debug_bounds("catalog-empty").is_some());
    click(ui, "catalog-clear");
    assert!(target_is_visible(ui, 7));
    assert!(events.borrow().is_empty());
}

/// Передаёт настоящий снимок Core с пользовательской папкой и двумя доступными чатами.
fn publish_folders(
    sidebar: &Entity<Sidebar>,
    ui: &mut gpui::VisualTestContext,
    owner: &AccountSession,
    custom_order: &[u128],
) {
    use workspace_core::messenger::folders::{ChatType, Folder, FolderItem, FolderKind};
    let folder_id = Uuid::from_u128(10);
    let timestamp = "2026-01-01T12:00:00Z".to_owned();
    let folder = Folder {
        id: folder_id,
        kind: FolderKind::Custom,
        title: "Проверка".into(),
        background_color_value: Some(0xff44_7799),
        unread_count: 6,
        created_at: timestamp.clone(),
        updated_at: timestamp.clone(),
        items: vec![FolderItem {
            id: Uuid::from_u128(12),
            stream_id: Uuid::from_u128(1),
            chat_type: ChatType::Private,
            pinned_at: Some(timestamp.clone()),
            order_index: None,
            unread_count: 6,
            active_unread_count: 6,
            passive_unread_count: 0,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        }],
    };
    let snapshot = FolderSnapshot {
        owner: owner.owner.clone(),
        generation: owner.generation,
        folders: Some(
            custom_order
                .iter()
                .map(|id| {
                    if *id == 10 {
                        folder.clone()
                    } else {
                        Folder {
                            id: Uuid::from_u128(*id),
                            title: format!("Папка {id}"),
                            items: Vec::new(),
                            ..folder.clone()
                        }
                    }
                })
                .collect(),
        ),
        chats: vec![
            chat(1, "Общее первое", true, &[]),
            chat(2, "Общее второе", true, &[]),
        ],
        all_chats_unread: Some(9),
        needs_refresh: false,
    };
    ui.update(|_, app| {
        sidebar.update(app, |sidebar, cx| {
            sidebar.set_folder_online(true, cx);
            sidebar.set_folder_snapshots(
                HashMap::from([(owner.owner.clone(), Arc::new(snapshot))]),
                cx,
            );
        })
    });
    ui.run_until_parked();
}

/// Переключает папку мышью, проверяет группы поиска и выбирает «Все чаты» после удаления.
#[gpui::test]
fn folder_switch_search_and_remote_delete_use_the_core_snapshot(cx: &mut TestAppContext) {
    const CUSTOM: &str = "folder-rail-entry-00000000-0000-0000-0000-00000000000a";
    let (sidebar, ui, events) = initialize(cx);
    let owner = session("a", 1);
    let chats = ChatList {
        generation: owner.generation,
        status: ChatListStatus::Ready,
        chats: None,
    };
    publish(&sidebar, ui, &owner, &chats);
    publish_folders(&sidebar, ui, &owner, &[10]);
    click(ui, CUSTOM);
    ui.run_until_parked();
    assert!(ui.debug_bounds(row(1, None)).is_some());
    assert!(ui.read(|app| {
        let active = sidebar.read(app).active.as_ref().unwrap();
        active.folder == Uuid::from_u128(10) && !active.folder_contains(Uuid::from_u128(2))
    }));

    ui.update(|window, app| {
        sidebar
            .read(app)
            .active
            .as_ref()
            .unwrap()
            .search
            .read(app)
            .focus(window)
    });
    ui.simulate_input("общее");
    ui.run_until_parked();
    assert!(ui.debug_bounds("catalog-search-in-folder").is_some());
    assert!(ui.debug_bounds("catalog-search-outside-folder").is_some());
    ui.simulate_keystrokes("up");
    enter(ui);
    assert_eq!(events.borrow().as_slice(), &[open(&owner, 2, None)]);

    publish_folders(&sidebar, ui, &owner, &[]);
    assert!(ui.read(|app| sidebar.read(app).active.as_ref().unwrap().folder.is_nil()));
    assert!(ui.debug_bounds(row(1, None)).is_some());
    assert!(ui.debug_bounds(row(2, None)).is_some());
}

/// Папка отвечает за закрепление. Маршруты и темы приходят из свежего каталога.
#[gpui::test]
fn folder_search_uses_live_catalog_without_restoring_removed_rows(cx: &mut TestAppContext) {
    let (sidebar, ui, events) = initialize(cx);
    let owner = session("a", 1);
    let loading = ChatList {
        generation: owner.generation,
        status: ChatListStatus::Loading,
        chats: None,
    };
    publish(&sidebar, ui, &owner, &loading);
    publish_folders(&sidebar, ui, &owner, &[10]);
    click(ui, "folder-rail-entry-00000000-0000-0000-0000-00000000000a");

    let live = catalog(
        &owner,
        ChatListStatus::Ready,
        vec![
            chat(1, "Актуальная личка", true, &[]),
            chat(2, "Новый канал", false, &[(3, "Актуальная тема")]),
        ],
    );
    publish(&sidebar, ui, &owner, &live);
    assert!(ui.debug_bounds("catalog-row-pinned").is_some());
    click(ui, "catalog-search");
    ui.simulate_input("АКТУАЛЬНАЯ");
    ui.run_until_parked();
    assert!(ui.debug_bounds("catalog-search-in-folder").is_some());
    assert!(ui.debug_bounds("catalog-search-outside-folder").is_some());
    assert!(ui.debug_bounds(row(2, Some(3))).is_some());
    ui.simulate_keystrokes("up");
    enter(ui);
    assert_eq!(events.borrow().as_slice(), &[open(&owner, 2, Some(3))]);

    let empty = catalog(&owner, ChatListStatus::Ready, vec![]);
    publish(&sidebar, ui, &owner, &empty);
    publish_folders(&sidebar, ui, &owner, &[10]);
    ui.read(|app| {
        let active = sidebar.read(app).active.as_ref().unwrap();
        assert!(active.chats().is_some_and(<[_]>::is_empty));
        assert!(active.focus.is_empty());
    });
    assert!(ui.debug_bounds("catalog-empty").is_some());
}

/// Проверяет UUID связи закрепления и игнорирует ошибку после смены каталога.
#[gpui::test]
fn pin_intent_uses_folder_item_and_ignores_an_old_account_reply(cx: &mut TestAppContext) {
    let (sidebar, ui, _) = initialize(cx);
    let owner = session("a", 1);
    let next = session("b", 1);
    let requests = Rc::new(RefCell::new(Vec::new()));
    let recorded = requests.clone();
    ui.update(|_, app| {
        app.subscribe(&sidebar, move |_, intent: &FolderIntent, _| {
            recorded.borrow_mut().push(intent.clone());
        })
        .detach()
    });
    publish(
        &sidebar,
        ui,
        &owner,
        &ChatList {
            generation: owner.generation,
            status: ChatListStatus::Ready,
            chats: None,
        },
    );
    publish_folders(&sidebar, ui, &owner, &[10]);
    click(ui, "folder-rail-entry-00000000-0000-0000-0000-00000000000a");
    ui.run_until_parked();

    let center = ui.debug_bounds(row(1, None)).unwrap().center();
    ui.simulate_mouse_down(center, MouseButton::Right, Modifiers::none());
    ui.simulate_mouse_up(center, MouseButton::Right, Modifiers::none());
    ui.run_until_parked();
    ui.simulate_keystrokes("down");
    enter(ui);
    let mut recorded = requests.borrow_mut();
    assert_eq!(recorded.len(), 1);
    let FolderIntent::Command {
        session: requested,
        operation,
        command,
    } = recorded.remove(0)
    else {
        panic!("Ожидается команда закрепления");
    };
    drop(recorded);
    assert_eq!(requested, owner);
    assert!(
        matches!(command, FolderCommand::Pin { folder, item, pinned: false } if folder == Uuid::from_u128(10) && item == Uuid::from_u128(12))
    );

    publish(
        &sidebar,
        ui,
        &next,
        &catalog(&next, ChatListStatus::Ready, vec![]),
    );
    ui.update(|_, app| {
        sidebar.update(app, |sidebar, cx| {
            sidebar.finish_folder_command(
                &requested,
                operation,
                Err(FolderCommandError::Unknown),
                cx,
            )
        })
    });
    ui.run_until_parked();
    ui.read(|app| {
        let active = sidebar.read(app).active.as_ref().unwrap();
        assert_eq!(active.session, next);
        assert!(active.folder_error.is_none() && active.folder_view.is_none());
    });
}

/// Сохраняет точную цель, если порядок папок меняется под открытым меню.
#[gpui::test]
fn open_assignment_menu_keeps_the_folder_uuid_when_the_snapshot_reorders(cx: &mut TestAppContext) {
    let (sidebar, ui, _) = initialize(cx);
    let owner = session("a", 1);
    let choices = Rc::new(RefCell::new(Vec::new()));
    let observed = choices.clone();
    ui.update(|_, app| {
        app.subscribe(&sidebar, move |_, intent: &FolderIntent, _| {
            if let FolderIntent::Command { command, .. } = intent {
                observed.borrow_mut().push(command.clone());
            }
        })
        .detach();
    });
    publish(
        &sidebar,
        ui,
        &owner,
        &ChatList {
            generation: owner.generation,
            status: ChatListStatus::Ready,
            chats: None,
        },
    );
    publish_folders(&sidebar, ui, &owner, &[10, 11]);
    let center = ui.debug_bounds(row(2, None)).unwrap().center();
    ui.simulate_mouse_down(center, MouseButton::Right, Modifiers::none());
    ui.simulate_mouse_up(center, MouseButton::Right, Modifiers::none());
    ui.simulate_keystrokes("down");
    enter(ui);
    ui.simulate_keystrokes("down down");
    ui.run_until_parked();
    publish_folders(&sidebar, ui, &owner, &[11, 10]);
    enter(ui);
    assert!(
        choices.borrow().as_slice()
            == [FolderCommand::Add {
                folder: Uuid::from_u128(11),
                chat: Uuid::from_u128(2),
            }]
    );
}
