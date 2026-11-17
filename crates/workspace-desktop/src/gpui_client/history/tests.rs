//! Проверяет действия в окне GPUI. Ответы истории приходят по управляемым каналам.

use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, px, size};
use workspace_core::{
    context::{AccountId, OwnerContext, SessionGeneration},
    storage::{DeliveryStatus, HistoryMessage, HistoryMetadata, HistoryTime, PreparedSend},
};

use crate::gpui_client::contracts::{ChatRoute, ConversationReply, NavigationRevision};

use super::*;

/// Хранит неотвеченный запрос, чтобы проверить клики до и после ответа.
struct Request {
    address: ConversationAddress,     // Адрес на момент запроса.
    load: Load,                       // Что экран просит загрузить.
    cache: oneshot::Sender<Snapshot>, // Канал раннего снимка из SQLite.
    response: oneshot::Sender<ConversationReply<LoadResult>>, // Канал последнего результата.
}

impl Request {
    /// Возвращает ответ с исходным адресом запроса.
    fn finish(self, value: LoadResult) {
        self.response
            .send(ConversationReply {
                address: self.address,
                value,
            })
            .unwrap_or_else(|_| panic!("Экран перестал ждать {:?}", self.load));
    }
}

/// Создаёт постоянный адрес тестового личного чата.
fn address(entry: ChatEntry) -> ConversationAddress {
    ConversationAddress {
        route: ChatRoute {
            owner: OwnerContext {
                account_id: AccountId("test-a".into()),
                server: "https://workspace.test".into(),
                organization_id: "org".into(),
                project_id: "project".into(),
                user_id: Uuid::from_u128(900).to_string(),
            },
            scope: HistoryScope {
                stream_id: Uuid::from_u128(1000),
                topic_id: None,
            },
            entry,
        },
        generation: SessionGeneration(1),
        revision: NavigationRevision(3),
    }
}

/// Создаёт несколько строк разной высоты в заданной области истории.
fn window(
    scope: HistoryScope,
    ids: std::ops::Range<u128>,
    older: HistoryEdge,
    newer: HistoryEdge,
) -> HistoryWindow {
    HistoryWindow {
        id: 41,
        scope,
        older,
        newer,
        needs_refresh: false,
        messages: ids
            .map(|id| HistoryMessage {
                id: Uuid::from_u128(id),
                stream_id: scope.stream_id,
                topic_id: Uuid::from_u128(1001),
                text: format!(
                    "Сообщение {id}{}",
                    "\nСинтетическая строка".repeat((id % 3) as usize)
                ),
                metadata: HistoryMetadata {
                    author_id: Some(Uuid::from_u128(910)),
                    created_at: HistoryTime::parse("2026-11-17T07:10:30Z").unwrap(),
                    updated_at: None,
                },
            })
            .collect(),
    }
}

/// Возвращает окно, допускающее подгрузку в обоих направлениях.
fn reading(address: &ConversationAddress, ids: std::ops::Range<u128>) -> HistoryWindow {
    window(
        address.route.scope,
        ids,
        HistoryEdge::More("older".into()),
        HistoryEdge::More("newer".into()),
    )
}

/// Запускает тот же экран с источником, из которого тест может выдавать снимки по одному.
fn scenario<'a>(
    cx: &'a mut TestAppContext,
    address: &ConversationAddress,
) -> (
    Entity<Conversation>,
    &'a mut VisualTestContext,
    mpsc::Receiver<Request>,
) {
    let (changes, requests) = mpsc::channel();
    let source: Source = Arc::new(move |address, load, _, cache| {
        let (response, ready) = oneshot::channel();
        changes
            .send(Request {
                address,
                load,
                cache,
                response,
            })
            .expect("приёмник запросов жив");
        Box::pin(async move {
            ready
                .await
                .unwrap_or_else(|_| panic!("Тест потерял ответ {load:?}"))
        })
    });
    let address = address.clone();
    let (view, ui) = cx.add_window_view(|_, cx| Conversation::with_source(address, source, cx));
    ui.simulate_resize(size(px(630.), px(400.)));
    ui.run_until_parked();
    (view, ui, requests)
}

/// Возвращает подготовленный приложением запрос, указывая ожидаемое действие при ошибке.
fn request(requests: &mpsc::Receiver<Request>, expected: Load) -> Request {
    let request = requests
        .recv_timeout(Duration::from_millis(100))
        .unwrap_or_else(|_| panic!("Экран не отправил {expected:?}"));
    assert_eq!(request.load, expected);
    request
}

/// Возвращает успешный ответ и даёт GPUI отрисовать принятый снимок.
fn answer(request: Request, window: HistoryWindow, ui: &mut VisualTestContext) {
    request.finish(Ok(Some(Snapshot {
        window,
        outbox: Some(Vec::new()),
    })));
    ui.run_until_parked();
}

/// Нажимает видимую кнопку истории.
fn click(ui: &mut VisualTestContext, selector: &'static str) {
    let center = ui
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("Кнопка {selector} отсутствует"))
        .center();
    ui.simulate_click(center, Modifiers::none());
    ui.run_until_parked();
}

/// Передаёт событие Sync как оболочка с точным адресом открытого экрана.
fn sync(
    view: &Entity<Conversation>,
    ui: &mut VisualTestContext,
    address: &ConversationAddress,
    change: SyncChange,
) {
    let update = SyncUpdate {
        context: address.route.owner.clone(),
        generation: address.generation,
        change,
    };
    ui.update(|_, cx| view.update(cx, |view, cx| view.apply_sync(address, update, cx)));
    ui.run_until_parked();
}

/// Проверяет открытие у цели, ввод пользователя и неизменность координаты после страницы и события.
#[gpui::test]
fn preserves_visible_uuid_during_pages_and_updates(cx: &mut TestAppContext) {
    let anchor = Uuid::from_u128(52);
    let address = address(ChatEntry::Around(anchor));
    let (view, ui, requests) = scenario(cx, &address);
    answer(
        request(&requests, Load::Open(ChatEntry::Around(anchor))),
        reading(&address, 20..80),
        ui,
    );
    let original = ui.read(|cx| view.read(cx).viewport.y(anchor).expect("цель видна"));
    assert!(
        original.abs() <= 1.,
        "Цель у верхнего края, получено {original} px"
    );

    // Частично читаем строку, чтобы проверить восстановление смещения внутри неё.
    ui.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.viewport.list().scroll_by(px(11.));
            cx.notify();
        })
    });
    ui.run_until_parked();
    let before = ui.read(|cx| view.read(cx).viewport.y(anchor).unwrap());
    assert!(
        (before + 11.).abs() <= 1.,
        "Часть опоры скрыта, получено {before} px"
    );
    click(ui, "history-older");
    let older = request(&requests, Load::Extend(41, HistoryDirection::Older));
    click(ui, "history-older");
    assert!(
        requests.try_recv().is_err(),
        "Повторный клик не запускает второй запрос"
    );
    older.finish(Err(Failure::Unavailable));
    ui.run_until_parked();
    assert!(
        ui.debug_bounds("history-error").is_some(),
        "Ошибка показана"
    );
    click(ui, "history-retry");
    answer(
        request(&requests, Load::Extend(41, HistoryDirection::Older)),
        reading(&address, 1..80),
        ui,
    );
    let after = ui.read(|cx| view.read(cx).viewport.y(anchor).unwrap());
    assert!(
        (before - after).abs() <= 1.,
        "После старой страницы {before} → {after} px"
    );

    click(ui, "history-newer");
    answer(
        request(&requests, Load::Extend(41, HistoryDirection::Newer)),
        reading(&address, 1..100),
        ui,
    );
    let after = ui.read(|cx| view.read(cx).viewport.y(anchor).unwrap());
    assert!(
        (before - after).abs() <= 1.,
        "После новой страницы {before} → {after} px"
    );
    sync(
        &view,
        ui,
        &address,
        SyncChange::HistoryMessageCreated {
            scope: address.route.scope,
            message_id: Uuid::from_u128(101),
        },
    );
    let mut updated = reading(&address, 1..102);
    updated.messages[49].text += "\nСтрока выше опоры выросла";
    answer(request(&requests, Load::Read(41)), updated, ui);
    let after = ui.read(|cx| view.read(cx).viewport.y(anchor).unwrap());
    assert!(
        (before - after).abs() <= 1.,
        "После обновления {before} → {after} px"
    );
}

/// Проверяет, что после сброса и отзыва доступа запоздалые страницы не возвращают старые строки.
#[gpui::test]
fn hides_revoked_and_reset_history_before_network_answers(cx: &mut TestAppContext) {
    let address = address(ChatEntry::Latest);
    let (view, ui, requests) = scenario(cx, &address);
    answer(
        request(&requests, Load::Open(ChatEntry::Latest)),
        reading(&address, 11..80),
        ui,
    );
    click(ui, "history-older");
    let delayed = request(&requests, Load::Extend(41, HistoryDirection::Older));
    sync(&view, ui, &address, SyncChange::MessengerReset);
    let reopening = request(&requests, Load::Open(ChatEntry::Latest));
    answer(delayed, reading(&address, 1..80), ui);
    assert!(ui.read(|cx| matches!(view.read(cx).state, State::Opening)));
    let mut old_address = address.clone();
    old_address.revision.0 -= 1;
    sync(
        &view,
        ui,
        &old_address,
        SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(address.route.scope.stream_id)),
    );
    assert!(ui.read(|cx| matches!(view.read(cx).state, State::Opening)));
    sync(
        &view,
        ui,
        &address,
        SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(address.route.scope.stream_id)),
    );
    answer(reopening, reading(&address, 31..80), ui);
    assert!(ui.read(|cx| matches!(view.read(cx).state, State::Failed(Failure::AccessLost))));
    assert!(
        ui.debug_bounds("history-error").is_some(),
        "Отзыв виден в окне"
    );
}

/// Проверяет исчезнувшую цель и пользовательский переход к хвосту из сообщения об ошибке.
#[gpui::test]
fn missing_target_can_open_latest_and_follow_tail(cx: &mut TestAppContext) {
    let target = Uuid::from_u128(35);
    let address = address(ChatEntry::Around(target));
    let (view, ui, requests) = scenario(cx, &address);
    request(&requests, Load::Open(ChatEntry::Around(target))).finish(Err(Failure::MissingMessage));
    ui.run_until_parked();
    assert!(ui.read(|cx| matches!(view.read(cx).state, State::Failed(Failure::MissingMessage))));
    click(ui, "history-latest");
    answer(
        request(&requests, Load::Open(ChatEntry::Latest)),
        window(
            address.route.scope,
            20..80,
            HistoryEdge::End,
            HistoryEdge::End,
        ),
        ui,
    );
    let last = Uuid::from_u128(79);
    assert!(
        ui.read(|cx| view.read(cx).viewport.y(last).is_some()),
        "Хвост сразу виден"
    );
    let new = Uuid::from_u128(80);
    sync(
        &view,
        ui,
        &address,
        SyncChange::HistoryMessageCreated {
            scope: address.route.scope,
            message_id: new,
        },
    );
    answer(
        request(&requests, Load::Read(41)),
        window(
            address.route.scope,
            20..81,
            HistoryEdge::End,
            HistoryEdge::End,
        ),
        ui,
    );
    assert!(
        ui.read(|cx| view.read(cx).viewport.y(new).is_some()),
        "Новое сообщение видно у хвоста"
    );
}

/// Показывает подтверждённый кэш до сети и убирает временную строку при подтверждении UUID.
#[gpui::test]
fn cached_tail_refreshes_and_deduplicates_outbox(cx: &mut TestAppContext) {
    let address = address(ChatEntry::Latest);
    let (view, ui, requests) = scenario(cx, &address);
    let pending = request(&requests, Load::Open(ChatEntry::Latest));
    let own = OutboxEntry {
        snapshot: PreparedSend {
            canonical_id: Uuid::from_u128(600),
            public_id: Uuid::from_u128(80),
            stream_id: address.route.scope.stream_id,
            topic_id: Uuid::from_u128(1001),
            markdown: "Тестовое исходящее".into(),
        },
        status: DeliveryStatus::Unknown,
    };
    pending
        .cache
        .send(Snapshot {
            window: window(
                address.route.scope,
                20..80,
                HistoryEdge::End,
                HistoryEdge::End,
            ),
            outbox: Some(vec![own.clone()]),
        })
        .unwrap_or_else(|_| panic!("Экран перестал ждать кэш"));
    ui.run_until_parked();
    assert!(
        ui.read(|cx| view.read(cx).viewport.y(own.snapshot.public_id).is_some()),
        "Очередь видна ещё до ответа сервера"
    );
    pending
        .response
        .send(ConversationReply {
            address: pending.address,
            value: Ok(Some(Snapshot {
                window: window(
                    address.route.scope,
                    20..81,
                    HistoryEdge::End,
                    HistoryEdge::End,
                ),
                outbox: Some(vec![own]),
            })),
        })
        .unwrap_or_else(|_| panic!("Экран перестал ждать сервер"));
    ui.run_until_parked();
    ui.read(|cx| {
        let view = view.read(cx);
        assert_eq!(
            view.viewport.list().item_count(),
            61,
            "После совпадения UUID остаётся одна строка"
        );
    });
}

/// Потеря подтверждённого хвоста не перемещает того, кто читает старые сообщения.
#[gpui::test]
fn losing_latest_edge_does_not_move_a_history_reader(cx: &mut TestAppContext) {
    let address = address(ChatEntry::Latest);
    let (view, ui, requests) = scenario(cx, &address);
    answer(
        request(&requests, Load::Open(ChatEntry::Latest)),
        window(
            address.route.scope,
            20..80,
            HistoryEdge::End,
            HistoryEdge::End,
        ),
        ui,
    );
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.viewport.list().scroll_to(gpui::ListOffset {
                item_ix: 3,
                offset_in_item: px(11.),
            });
            cx.notify();
        });
    });
    ui.run_until_parked();
    let anchor = Uuid::from_u128(23);
    let before = ui.read(|app| {
        let view = view.read(app);
        assert!(!view.viewport.at_tail());
        view.viewport.y(anchor).unwrap()
    });
    sync(
        &view,
        ui,
        &address,
        SyncChange::HistoryMessageCreated {
            scope: address.route.scope,
            message_id: Uuid::from_u128(80),
        },
    );
    answer(
        request(&requests, Load::Read(41)),
        window(
            address.route.scope,
            20..80,
            HistoryEdge::End,
            HistoryEdge::Unchecked,
        ),
        ui,
    );
    assert!(
        requests.try_recv().is_err(),
        "Чтение истории не запрашивает хвост автоматически"
    );
    let after = ui.read(|app| view.read(app).viewport.y(anchor).unwrap());
    assert!(
        (before - after).abs() <= 1.,
        "Позиция опоры: {before} → {after}"
    );
}
