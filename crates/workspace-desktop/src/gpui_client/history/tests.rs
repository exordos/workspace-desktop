//! Проверяет действия в окне GPUI. Ответы истории приходят по управляемым каналам.

use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, px, size};
use workspace_core::{
    bootstrap::ChatListStatus,
    context::{AccountId, OwnerContext, SessionGeneration},
    messenger::catalog::{Chat, Topic},
    storage::{DeliveryStatus, HistoryMessage, HistoryMetadata, HistoryTime, PreparedSend},
};

use crate::gpui_client::contracts::{ChatRoute, ConversationReply, NavigationRevision};

use super::*;

impl Conversation {
    /// Даёт измерения рабочего списка для прогона перехода и ранней догрузки на HTTPS.
    pub(in crate::gpui_client) fn tested_older_distance(&self) -> (f32, f32) {
        (
            f32::from(self.viewport.distance_to_edge(HistoryDirection::Older)),
            f32::from(self.viewport.list().viewport_bounds().size.height),
        )
    }

    /// Возвращает верх пузыря относительно рабочего окна, включая положение самого списка.
    pub(in crate::gpui_client) fn tested_message_window_y(&self, id: Uuid) -> Option<gpui::Pixels> {
        self.viewport
            .y(id)
            .map(|y| self.viewport.list().viewport_bounds().top() + px(y))
    }

    /// Сверяет применение страницы вместе с её границей после полного ответа Core.
    pub(in crate::gpui_client) fn tested_older_page(&self) -> Option<(usize, HistoryEdge)> {
        let State::Ready(window) = &self.state else {
            return None;
        };
        Some((window.messages.len(), window.older.clone()))
    }

    /// Возвращает фото шапки и видимой истории для общей приёмки аватаров.
    pub(in crate::gpui_client) fn tested_chat_avatars(
        &self,
        author: Uuid,
        app: &App,
    ) -> (Option<gpui::ImageId>, Option<gpui::ImageId>) {
        let image_id = |source| match source {
            Some(gpui::ImageSource::Render(image)) => Some(image.id),
            _ => None,
        };
        (
            image_id(
                self.heading
                    .direct_peer_id
                    .and_then(|peer| self.heading_avatars.image(peer, app)),
            ),
            image_id(self.history_avatars.image(author, app)),
        )
    }
}

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
    let (view, ui) =
        cx.add_window_view(|_, cx| Conversation::with_source(address, source, None, cx));
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
        media_access: None,
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

/// Проверяет, что UUID собеседника берётся из каталога правильного входа и маршрута.
#[gpui::test]
fn heading_uses_direct_peer_from_current_catalog(cx: &mut TestAppContext) {
    let address = address(ChatEntry::Latest);
    let peer = Uuid::from_u128(910);
    let mut catalog = ChatList {
        generation: address.generation,
        chats: Some(Arc::from([Chat {
            id: address.route.scope.stream_id,
            name: "Собеседник".into(),
            direct: true,
            topics: Vec::new(),
            metadata: workspace_core::messenger::catalog::ChatMetadata {
                direct_peer_id: Some(peer),
                ..Default::default()
            },
        }])),
        status: ChatListStatus::Ready,
    };
    let session = AccountSession {
        owner: address.route.owner.clone(),
        generation: address.generation,
    };
    let (view, ui, requests) = scenario(cx, &address);
    answer(
        request(&requests, Load::Open(ChatEntry::Latest)),
        window(
            address.route.scope,
            0..0,
            HistoryEdge::End,
            HistoryEdge::End,
        ),
        ui,
    );
    let mut other = session.clone();
    other.owner.account_id = AccountId("other".into());
    for (current, expected) in [(&other, None), (&session, Some(peer))] {
        ui.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.set_catalog(Some(current), Some(&catalog), cx)
            })
        });
        assert_eq!(
            ui.read(|app| view.read(app).heading.direct_peer_id),
            expected
        );
    }
    ui.run_until_parked();
    assert_eq!(
        ui.debug_bounds("conversation-heading-avatar")
            .unwrap()
            .size
            .width,
        px(40.)
    );

    catalog.generation.0 += 1;
    ui.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.set_catalog(Some(&session), Some(&catalog), cx)
        })
    });
    assert!(ui.read(|app| view.read(app).heading.direct_peer_id.is_none()));
}

/// Проверяет выбор UUID по реально видимым входящим группам при первом кадре и переносе опоры.
#[gpui::test]
fn visible_history_selects_only_incoming_group_avatars(cx: &mut TestAppContext) {
    let first = Uuid::from_u128(40);
    let address = address(ChatEntry::Around(first));
    let (view, ui, requests) = scenario(cx, &address);
    let mut page = window(
        address.route.scope,
        1..80,
        HistoryEdge::End,
        HistoryEdge::End,
    );
    for (index, message) in page.messages.iter_mut().enumerate() {
        message.metadata.author_id = Some(Uuid::from_u128(1001 + index as u128));
    }
    let group_author = Uuid::from_u128(1041);
    let current_user: Uuid = address.route.owner.user_id.parse().unwrap();
    page.messages[39].metadata.author_id = Some(group_author);
    page.messages[41].metadata.author_id = Some(current_user);
    page.messages[42].metadata.author_id = None;
    answer(
        request(&requests, Load::Open(ChatEntry::Around(first))),
        page,
        ui,
    );
    let visible = ui.read(|app| view.read(app).viewport.tested_visible_avatars());
    assert!(
        visible.contains(&group_author),
        "Последняя строка входящей группы видна"
    );
    for hidden in [Uuid::from_u128(1001), Uuid::from_u128(1079), current_user] {
        assert!(
            !visible.contains(&hidden),
            "Аватар {hidden} вне видимых входящих групп"
        );
    }

    ui.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.viewport.list().scroll_to(gpui::ListOffset {
                item_ix: 66,
                offset_in_item: px(0.),
            });
            cx.notify();
        })
    });
    ui.run_until_parked();
    let scrolled = ui.read(|app| view.read(app).viewport.tested_visible_avatars());
    assert!(scrolled.contains(&Uuid::from_u128(1067)));
    assert!(!scrolled.contains(&group_author));
}

/// Проверяет открытие у цели, ввод пользователя и неизменность координаты после страницы и события.
#[gpui::test]
fn preserves_visible_uuid_during_pages_and_updates(cx: &mut TestAppContext) {
    let anchor = Uuid::from_u128(52);
    let address = address(ChatEntry::Around(anchor));
    let (view, ui, requests) = scenario(cx, &address);
    answer(
        request(&requests, Load::Open(ChatEntry::Around(anchor))),
        reading(&address, 52..80),
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
        "После старой страницы дата переехала к началу дня, опора {before} → {after} px"
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

/// Проверяет название темы, одну группу соседних сообщений и перенос в ширине из UI-05.
#[gpui::test]
fn fixed_topic_groups_and_wraps_at_both_panel_sizes(cx: &mut TestAppContext) {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/plans/roadmap/gpui-ui-parity-fixtures/scenes.json"
    )))
    .unwrap();
    let topic_id = Uuid::from_u128(1001);
    let mut address = address(ChatEntry::Latest);
    address.route.scope.topic_id = Some(topic_id);
    let topic = &fixture["channel"]["topics"][0];
    let channel_name = fixture["channel"]["title"].as_str().unwrap();
    let topic_name = topic["title"].as_str().unwrap();
    let catalog = ChatList {
        generation: address.generation,
        chats: Some(Arc::from([Chat {
            id: address.route.scope.stream_id,
            name: channel_name.to_owned(),
            direct: false,
            topics: vec![Topic {
                id: topic_id,
                name: topic_name.to_owned(),
                metadata: Default::default(),
            }],
            metadata: Default::default(),
        }])),
        status: ChatListStatus::Ready,
    };
    let mut session = AccountSession {
        owner: address.route.owner.clone(),
        generation: address.generation,
    };
    let (view, ui, requests) = scenario(cx, &address);
    session.owner.account_id = AccountId("other".into());
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_catalog(Some(&session), Some(&catalog), cx)
        })
    });
    assert!(ui.read(|app| view.read(app).heading.topic.is_none()));
    session.owner = address.route.owner.clone();
    session.generation.0 += 1;
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_catalog(Some(&session), Some(&catalog), cx)
        })
    });
    assert!(ui.read(|app| view.read(app).heading.topic.is_none()));
    session.generation = address.generation;
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_catalog(Some(&session), Some(&catalog), cx)
        })
    });
    assert!(ui.read(|app| view.read(app).heading.topic.as_deref() == Some(topic_name)));

    let mut page = window(
        address.route.scope,
        90..93,
        HistoryEdge::End,
        HistoryEdge::End,
    );
    for (message, row) in page
        .messages
        .iter_mut()
        .zip(topic["messages"].as_array().unwrap())
    {
        message.text = row["text"].as_str().unwrap().to_owned();
        message.metadata.author_id = if row["author"] == "a" {
            address.route.owner.user_id.parse().ok()
        } else {
            Some(Uuid::from_u128(910))
        };
    }
    // Ещё одно длинное сообщение от того же автора в следующем дне.
    let mut next_day = page.messages[1].clone();
    next_day.id = Uuid::from_u128(93);
    next_day.metadata.created_at = HistoryTime::parse("2026-11-18T07:10:30Z").unwrap();
    assert_eq!(next_day.topic_id, topic_id);
    page.messages.push(next_day);
    answer(request(&requests, Load::Open(ChatEntry::Latest)), page, ui);
    for (width, height) in [(1061., 819.), (340., 519.)] {
        ui.simulate_resize(size(px(width), px(height)));
        ui.run_until_parked();
        assert!(ui.debug_bounds("conversation-title").unwrap().size.width <= px(width));
        assert!(ui.debug_bounds("conversation-channel").is_some());
        assert!(
            ui.debug_bounds("history-avatar-00000000-0000-0000-0000-00000000005a")
                .is_none()
        );
        assert!(
            ui.debug_bounds("history-avatar-00000000-0000-0000-0000-00000000005b")
                .is_some()
        );
        assert!(
            ui.debug_bounds("history-avatar-00000000-0000-0000-0000-00000000005c")
                .is_none()
        );
        for id in [
            "history-date-00000000-0000-0000-0000-00000000005a",
            "history-date-00000000-0000-0000-0000-00000000005d",
        ] {
            assert!(ui.debug_bounds(id).is_some());
        }
    }
    let last_message = "history-message-00000000-0000-0000-0000-00000000005d";
    let narrow = ui.debug_bounds(last_message).unwrap();
    ui.simulate_resize(size(px(1061.), px(819.)));
    ui.run_until_parked();
    let wide = ui.debug_bounds(last_message).unwrap();
    assert!(
        narrow.size.height > wide.size.height,
        "Длинная строка переносится: {wide:?} → {narrow:?}"
    );
}

/// Проверяет, что первичная ошибка сменяется по кнопке загрузкой и пустой темой.
#[gpui::test]
fn initial_error_retries_into_empty_topic(cx: &mut TestAppContext) {
    let mut address = address(ChatEntry::Latest);
    address.route.scope.topic_id = Some(Uuid::from_u128(1001));
    let (view, ui, requests) = scenario(cx, &address);
    assert!(ui.debug_bounds("conversation-heading").is_some());
    assert!(ui.debug_bounds("history-loading").is_some());
    request(&requests, Load::Open(ChatEntry::Latest)).finish(Err(Failure::Unavailable));
    ui.run_until_parked();
    assert!(ui.debug_bounds("history-error").is_some());
    click(ui, "history-retry");
    assert!(ui.debug_bounds("history-loading").is_some());
    answer(
        request(&requests, Load::Open(ChatEntry::Latest)),
        window(
            address.route.scope,
            0..0,
            HistoryEdge::End,
            HistoryEdge::End,
        ),
        ui,
    );
    assert!(ui.read(
        |app| matches!(&view.read(app).state, State::Ready(history) if history.messages.is_empty())
    ));
    assert!(ui.debug_bounds("history-empty").is_some());
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
    let anchor = ui.read(|app| view.read(app).reading_position().unwrap());
    sync(&view, ui, &address, SyncChange::MessengerReset);
    let reopening = request(&requests, Load::Open(ChatEntry::Around(anchor.id)));
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
            media_access: None,
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
                media_access: None,
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

impl Conversation {
    /// Проверяет, была ли строка измерена в последнем кадре тестового окна.
    pub(in crate::gpui_client) fn message_visible_for_test(&self, id: Uuid) -> bool {
        self.viewport.y(id).is_some()
    }
}
