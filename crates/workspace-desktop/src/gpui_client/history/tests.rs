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
    storage::{
        DeliveryStatus, HistoryMessage, HistoryMetadata, HistoryTime, MessageBody, PreparedSend,
    },
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

    /// Даёт границы области и пузыря вместе с координатой нужного фрагмента текста.
    pub(in crate::gpui_client) fn tested_history_geometry(
        &self,
        id: Uuid,
        byte: usize,
    ) -> Option<(
        gpui::Bounds<gpui::Pixels>,
        gpui::Bounds<gpui::Pixels>,
        gpui::Pixels,
    )> {
        Some((
            self.viewport.list().viewport_bounds(),
            self.viewport.tested_bounds(id)?,
            self.viewport.tested_text_y(id, byte)?,
        ))
    }

    /// Даёт границы области и пузыря, включая состояния с закрытым текстом.
    pub(in crate::gpui_client) fn tested_history_row_bounds(
        &self,
        id: Uuid,
    ) -> Option<(gpui::Bounds<gpui::Pixels>, gpui::Bounds<gpui::Pixels>)> {
        Some((
            self.viewport.list().viewport_bounds(),
            self.viewport.tested_bounds(id)?,
        ))
    }

    /// Считает вызовы Render контрольного обычного сообщения в рабочем Viewport.
    pub(in crate::gpui_client) fn tested_bubble_renders(
        &self,
        id: Uuid,
        cx: &App,
    ) -> Option<usize> {
        self.viewport.tested_bubble_renders(id, cx)
    }

    /// Проверяет повторное использование подготовки обычного тела после события сети.
    pub(in crate::gpui_client) fn tested_history_body_identity(&self, id: Uuid) -> Option<usize> {
        self.viewport.tested_body_identity(id)
    }

    /// Читает состояние защищённой строки после обработки события настоящим экраном.
    pub(in crate::gpui_client) fn tested_history_encryption_state(
        &self,
        id: Uuid,
    ) -> Option<&'static str> {
        self.viewport.tested_encryption_state(id)
    }

    /// Сверяет конверт подтверждённой строки после перечитывания правки.
    pub(in crate::gpui_client) fn tested_history_envelope(&self, id: Uuid, expected: &str) -> bool {
        let State::Ready(history) = &self.state else {
            return false;
        };
        history
            .messages
            .iter()
            .find(|row| row.id == id)
            .is_some_and(|row| {
                matches!(
                    &row.body, MessageBody::Encrypted { ciphertext, .. } if ciphertext == expected
                )
            })
    }

    /// Находит начало визуальной строки, в которую попал указанный байт.
    pub(in crate::gpui_client) fn tested_history_line_start(
        &self,
        id: Uuid,
        byte: usize,
    ) -> Option<usize> {
        let layout = self.viewport.tested_text_layout(id)?;
        let y = layout.position_for_index(byte)?.y + px(1.);
        let left = layout.bounds().left() - px(1.);
        Some(
            layout
                .index_for_position(gpui::point(left, y))
                .unwrap_or_else(|index| index),
        )
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
                reactions: Vec::new(),
                body: MessageBody::Markdown(format!(
                    "Сообщение {id}{}",
                    "\nСинтетическая строка".repeat((id % 3) as usize)
                )),
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
        cx.add_window_view(|_, cx| Conversation::with_source(address, source, None, None, cx));
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

/// Нажатие по видимому моменту открывает панель времени, а клик вне её закрывает панель.
#[gpui::test]
fn references_open_shared_time_from_the_visible_history(cx: &mut TestAppContext) {
    let address = address(ChatEntry::Latest);
    let (view, ui, requests) = scenario(cx, &address);
    let mut snapshot = window(
        address.route.scope,
        10..11,
        HistoryEdge::End,
        HistoryEdge::End,
    );
    let id = snapshot.messages[0].id;
    snapshot.messages[0].body =
        MessageBody::Markdown("До urn:time:2026-10-13T10:02:00Z после".into());
    answer(
        request(&requests, Load::Open(ChatEntry::Latest)),
        snapshot,
        ui,
    );
    let position = ui.read(|cx| {
        view.read(cx)
            .viewport
            .tested_text_layout(id)
            .expect("отрисована строка ссылки")
            .position_for_index("До ".len() + 2)
            .expect("локальная подпись времени")
    }) + gpui::point(px(1.), px(4.));
    ui.simulate_click(position, Modifiers::none());
    ui.run_until_parked();
    assert!(ui.debug_bounds("history-reference-time-panel").is_some());
    ui.simulate_click(gpui::point(px(3.), px(3.)), Modifiers::none());
    ui.run_until_parked();
    assert!(ui.debug_bounds("history-reference-time-panel").is_none());
}

/// Прокручивает сам список, чтобы его рабочий обработчик мог запросить страницу.
fn scroll_history(ui: &mut VisualTestContext, delta: f32) {
    let position = ui
        .debug_bounds("conversation-history-focus")
        .expect("История видна")
        .center();
    ui.simulate_event(gpui::ScrollWheelEvent {
        position,
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(delta))),
        ..Default::default()
    });
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

/// Две строки меняют размер, а опора между ними удерживается уже в первом кадре.
#[gpui::test]
fn encrypted_history_keeps_reading_anchor_in_the_first_frame(cx: &mut TestAppContext) {
    let anchor = Uuid::from_u128(52);
    let prior = Uuid::from_u128(51);
    let later = Uuid::from_u128(54);
    let address = address(ChatEntry::Around(anchor));
    let (view, ui, requests) = scenario(cx, &address);
    let mut page = window(
        address.route.scope,
        42..90,
        HistoryEdge::End,
        HistoryEdge::End,
    );
    for row in &mut page.messages {
        if [prior, later].contains(&row.id) {
            row.body = MessageBody::Encrypted {
                key_id: Uuid::from_u128(9),
                ciphertext: "test".into(),
            };
        }
    }
    answer(
        request(&requests, Load::Open(ChatEntry::Around(anchor))),
        page,
        ui,
    );
    let geometry = |app: &App| {
        let view = view.read(app);
        (
            view.tested_message_window_y(anchor).unwrap(),
            view.tested_history_row_bounds(later).unwrap().1.size.height,
        )
    };
    let (y, height) = ui.read(geometry);
    let (new_y, new_height) = ui.update(|window, app| {
        view.update(app, |view, cx| {
            assert!(view.viewport.apply_encryption([
                (prior, super::encryption::EncryptedPresentation::Failed),
                (later, super::encryption::EncryptedPresentation::Failed)
            ]));
            cx.notify();
        });
        window.draw(app).clear();
        geometry(app)
    });
    assert_eq!(y, new_y);
    assert!(new_height > height);
}

/// У хвоста дельта двух строк не пересобирает вид неподвижного обычного бабла.
#[gpui::test]
fn encrypted_history_keeps_tail_and_plain_render_in_the_first_frame(cx: &mut TestAppContext) {
    let control = Uuid::from_u128(55);
    let moved = Uuid::from_u128(53);
    let sealed = [Uuid::from_u128(52), Uuid::from_u128(54)];
    let address = address(ChatEntry::Latest);
    let (view, ui, requests) = scenario(cx, &address);
    let mut page = window(
        address.route.scope,
        52..56,
        HistoryEdge::End,
        HistoryEdge::End,
    );
    for row in &mut page.messages {
        if sealed.contains(&row.id) {
            row.body = MessageBody::Encrypted {
                key_id: Uuid::from_u128(9),
                ciphertext: "test".into(),
            };
        }
    }
    answer(request(&requests, Load::Open(ChatEntry::Latest)), page, ui);
    let sample = |app: &App| {
        let view = view.read(app);
        let (viewport, bounds) = view.tested_history_row_bounds(control).unwrap();
        let moved_y = view.tested_message_window_y(moved).unwrap();
        (
            viewport.bottom() - bounds.bottom(),
            view.tested_bubble_renders(control, app).unwrap(),
            view.viewport.tested_body_identity(control).unwrap(),
            view.viewport.tested_body_identity(moved).unwrap(),
            view.tested_bubble_renders(moved, app).unwrap(),
            moved_y,
        )
    };
    let before = ui.update(|window, app| {
        // Сначала обычный бабл измеряется, затем GPUI использует вид с известной высотой.
        window.draw(app).clear();
        window.draw(app).clear();
        sample(app)
    });
    let after = ui.update(|window, app| {
        view.update(app, |view, cx| {
            let changes = sealed.map(|id| (id, super::encryption::EncryptedPresentation::Failed));
            assert!(view.viewport.apply_encryption(changes));
            cx.notify();
        });
        window.draw(app).clear();
        sample(app)
    });
    assert_eq!(
        (before.0, before.1, before.2, before.3),
        (after.0, after.1, after.2, after.3)
    );
    // GPUI переносит уже подготовленную строку в новые экранные координаты.
    assert_ne!(before.5, after.5);
    assert!(after.4 > before.4);
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
    assert!(ui.debug_bounds("history-older").is_none());
    assert!(ui.debug_bounds("history-newer").is_none());
    let heading = ui.debug_bounds("conversation-heading").unwrap();
    let history = ui.debug_bounds("conversation-history-focus").unwrap();
    assert!((history.top() - heading.bottom()).abs() <= px(1.));
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
    scroll_history(ui, 1.);
    let older = request(&requests, Load::Extend(41, HistoryDirection::Older));
    scroll_history(ui, 1.);
    assert!(
        requests.try_recv().is_err(),
        "Повторный жест не запускает второй запрос"
    );
    let before = ui.read(|cx| view.read(cx).viewport.y(anchor).unwrap());
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

    for _ in 0..60 {
        scroll_history(ui, -80.);
        if ui.read(|cx| view.read(cx).active.is_some()) {
            break;
        }
    }
    let (anchor, before) = ui.read(|cx| {
        let view = view.read(cx);
        let anchor = view.viewport.saved_anchor().unwrap().id;
        (anchor, view.viewport.y(anchor).unwrap())
    });
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
    let MessageBody::Markdown(text) = &mut updated.messages[49].body else {
        unreachable!("Стенд загружает обычный текст")
    };
    text.push_str("\nСтрока выше опоры выросла");
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
        message.body = MessageBody::Markdown(row["text"].as_str().unwrap().to_owned());
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

/// Показывает разделитель на местной полуночи и объединяет следующие строки того же дня.
#[gpui::test]
fn shows_date_separator_at_local_midnight(cx: &mut TestAppContext) {
    let address = address(ChatEntry::Latest);
    let (_, ui, requests) = scenario(cx, &address);
    let mut page = window(
        address.route.scope,
        1..4,
        HistoryEdge::End,
        HistoryEdge::End,
    );
    for (message, (date, time)) in page.messages.iter_mut().zip([
        ("2026-12-31", "23:59"),
        ("2027-01-01", "00:00"),
        ("2027-01-01", "00:01"),
    ]) {
        message.metadata.created_at = crate::platform::message_time::parse_local_time(date, time)
            .expect("Дата стенда должна существовать в системном часовом поясе");
    }
    answer(request(&requests, Load::Open(ChatEntry::Latest)), page, ui);

    assert!(
        ui.debug_bounds("history-message-00000000-0000-0000-0000-000000000003")
            .is_some()
    );
    for (selector, expected) in [
        ("history-date-00000000-0000-0000-0000-000000000001", true),
        ("history-date-00000000-0000-0000-0000-000000000002", true),
        ("history-date-00000000-0000-0000-0000-000000000003", false),
    ] {
        assert_eq!(ui.debug_bounds(selector).is_some(), expected, "{selector}");
    }
}

/// Проверяет, что при открытии без кэша скелетон сменяется первым настоящим сообщением.
#[gpui::test]
fn opening_without_cache_replaces_skeleton_with_messages(cx: &mut TestAppContext) {
    let address = address(ChatEntry::Latest);
    let (view, ui, requests) = scenario(cx, &address);
    let pending = request(&requests, Load::Open(ChatEntry::Latest));
    let message = "history-message-00000000-0000-0000-0000-000000000001";

    assert!(ui.debug_bounds("history-loading").is_some());
    assert!(ui.debug_bounds("conversation-history-focus").is_none());
    assert!(ui.debug_bounds(message).is_none());

    answer(
        pending,
        window(
            address.route.scope,
            1..2,
            HistoryEdge::End,
            HistoryEdge::End,
        ),
        ui,
    );

    assert!(ui.read(|app| matches!(view.read(app).state, State::Ready(_))));
    assert!(ui.debug_bounds(message).is_some());
}

/// Оставляет скелетон при пустом кэше до ответа, подтверждающего пустую историю.
#[gpui::test]
fn empty_cache_keeps_skeleton_until_server_confirms_empty_history(cx: &mut TestAppContext) {
    let address = address(ChatEntry::Latest);
    let (view, ui, requests) = scenario(cx, &address);
    let pending = request(&requests, Load::Open(ChatEntry::Latest));
    let empty = window(
        address.route.scope,
        0..0,
        HistoryEdge::End,
        HistoryEdge::End,
    );
    pending
        .cache
        .send(Snapshot {
            media_access: None,
            window: empty.clone(),
            outbox: Some(Vec::new()),
        })
        .unwrap_or_else(|_| panic!("Экран перестал ждать кэш"));
    ui.run_until_parked();

    assert!(ui.read(|app| matches!(view.read(app).state, State::Opening)));
    assert!(ui.debug_bounds("history-loading").is_some());
    assert!(ui.debug_bounds("history-empty").is_none());

    pending
        .response
        .send(ConversationReply {
            address: pending.address,
            value: Ok(Some(Snapshot {
                media_access: None,
                window: empty,
                outbox: Some(Vec::new()),
            })),
        })
        .unwrap_or_else(|_| panic!("Экран перестал ждать сервер"));
    ui.run_until_parked();

    assert!(ui.read(|app| matches!(view.read(app).state, State::Ready(_))));
    assert!(ui.debug_bounds("history-empty").is_some());
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
    ui.update(|window, app| window.focus(&view.read(app).history_focus));
    ui.simulate_keystrokes("home");
    ui.run_until_parked();
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
    use gpui::{ScrollDelta, ScrollWheelEvent, point};

    let address = address(ChatEntry::Latest);
    let (view, ui, requests) = scenario(cx, &address);
    let pending = request(&requests, Load::Open(ChatEntry::Latest));
    assert!(ui.debug_bounds("history-background-loading").is_none());
    let own = OutboxEntry {
        snapshot: PreparedSend {
            canonical_id: Uuid::from_u128(600),
            public_id: Uuid::from_u128(80),
            stream_id: address.route.scope.stream_id,
            topic_id: Uuid::from_u128(1001),
            body: workspace_core::storage::PreparedSendBody::Markdown("Тестовое исходящее".into()),
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
    let loading = ui
        .debug_bounds("history-background-loading")
        .expect("Кэш уже виден, сверка ещё идёт");
    let frame = ui.debug_bounds("conversation-history-focus").unwrap();
    let at_tail = ui.read(|app| view.read(app).viewport.at_tail());
    assert!(at_tail);
    ui.simulate_event(ScrollWheelEvent {
        position: loading.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(130.))),
        ..Default::default()
    });
    ui.run_until_parked();
    let (anchor, y) = ui.read(|app| {
        let view = view.read(app);
        assert!(!view.viewport.at_tail(), "Плашка пропускает прокрутку");
        let anchor = view.viewport.saved_anchor().unwrap().id;
        (anchor, view.tested_message_window_y(anchor).unwrap())
    });
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
    assert_eq!(ui.debug_bounds("conversation-history-focus"), Some(frame));
    ui.read(|cx| {
        let view = view.read(cx);
        assert!(!view.has_background_history_load());
        assert_eq!(
            view.viewport.list().item_count(),
            61,
            "После совпадения UUID остаётся одна строка"
        );
        assert!((view.tested_message_window_y(anchor).unwrap() - y).abs() <= px(1.));
    });
}

/// Удерживает позицию при смене ID кэшированного окна и ошибке фоновой сверки.
#[gpui::test]
fn cached_refresh_keeps_the_anchor_after_leaving_the_tail(cx: &mut TestAppContext) {
    let address = address(ChatEntry::Latest);
    let (view, ui, requests) = scenario(cx, &address);
    let scope = address.route.scope;
    answer(
        request(&requests, Load::Open(ChatEntry::Latest)),
        window(scope, 20..80, HistoryEdge::End, HistoryEdge::End),
        ui,
    );

    sync(
        &view,
        ui,
        &address,
        SyncChange::HistoryMessageCreated {
            scope,
            message_id: Uuid::from_u128(81),
        },
    );
    let read = request(&requests, Load::Read(41));
    assert!(ui.debug_bounds("history-background-loading").is_none());
    answer(
        read,
        window(scope, 20..80, HistoryEdge::End, HistoryEdge::Unchecked),
        ui,
    );
    let pending = request(&requests, Load::Open(ChatEntry::Latest));
    assert!(ui.debug_bounds("history-background-loading").is_some());
    scroll_history(ui, 260.);
    let (anchor, before) = ui.read(|app| {
        let view = view.read(app);
        assert!(!view.viewport.at_tail());
        let id = view.viewport.saved_anchor().unwrap().id;
        (id, view.tested_message_window_y(id).unwrap())
    });

    let mut fresh_cache = window(scope, 21..82, HistoryEdge::End, HistoryEdge::End);
    fresh_cache.id = 42;
    pending
        .cache
        .send(Snapshot {
            window: fresh_cache,
            media_access: None,
            outbox: Some(Vec::new()),
        })
        .unwrap_or_else(|_| panic!("Экран перестал ждать кэш"));
    ui.run_until_parked();
    ui.read(|app| {
        let view = view.read(app);
        assert!(matches!(&view.state, State::Ready(history) if history.id == 42));
        assert!(view.has_background_history_load());
        assert!((view.tested_message_window_y(anchor).unwrap() - before).abs() <= px(1.));
    });

    pending
        .response
        .send(ConversationReply {
            address: pending.address,
            value: Err(Failure::Unavailable),
        })
        .unwrap_or_else(|_| panic!("Экран перестал ждать сервер"));
    ui.run_until_parked();
    assert!(ui.debug_bounds("history-retry").is_some());
    ui.read(|app| {
        let view = view.read(app);
        assert!(matches!(&view.state, State::Ready(history) if history.id == 42));
        assert!(!view.has_background_history_load());
        assert!((view.tested_message_window_y(anchor).unwrap() - before).abs() <= px(1.));
    });
}

/// Заполняет короткий хвост без жеста и удерживает последнее сообщение у нижнего края.
#[gpui::test]
fn short_tail_prefetch_fills_the_conversation_without_moving_the_last_row(cx: &mut TestAppContext) {
    let address = address(ChatEntry::Latest);
    let (view, ui, requests) = scenario(cx, &address);
    let scope = address.route.scope;
    answer(
        request(&requests, Load::Open(ChatEntry::Latest)),
        window(
            scope,
            80..83,
            HistoryEdge::More("older".into()),
            HistoryEdge::End,
        ),
        ui,
    );
    let last = Uuid::from_u128(82);
    let before = ui.read(|app| view.read(app).tested_message_window_y(last).unwrap());
    answer(
        request(&requests, Load::Extend(41, HistoryDirection::Older)),
        window(
            scope,
            30..83,
            HistoryEdge::More("older-again".into()),
            HistoryEdge::End,
        ),
        ui,
    );
    let after = ui.read(|app| view.read(app).tested_message_window_y(last).unwrap());
    assert!(
        (after - before).abs() <= px(1.),
        "Хвост: {before:?} → {after:?}"
    );
    assert!(requests.try_recv().is_err(), "Окно уже заполнено");
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

/// Измеряет первую отрисовку опоры, когда синхронизация приносит короткую версию её текста.
#[gpui::test]
fn edit_keeps_a_surviving_text_line_on_the_first_frame(cx: &mut TestAppContext) {
    use std::{cell::Cell, rc::Rc};

    let anchor = Uuid::from_u128(52);
    let address = address(ChatEntry::Around(anchor));
    let intro = "Короткое вступление";
    let setup: String = (0..15).map(|i| format!("Предисловие {i}\n")).collect();
    let previous = "Предыдущая строка";
    let visible = "Читаемая строка";
    let next = "Соседняя строка";
    let body = format!("{intro}\n{setup}{previous}\n{visible}\n{next}\nЗаключение");
    let paragraph = format!(
        "{}Середина абзаца {}.",
        "Начало длинного абзаца. ".repeat(12),
        "Продолжение длинного абзаца. ".repeat(4)
    );
    let wrapped = format!("{intro}\n{setup}{paragraph}");
    let cases = [
        (
            "удалили строки выше",
            body.clone(),
            visible,
            format!("{intro}\n{visible}\n{next}"),
            visible,
        ),
        (
            "сосед ниже",
            body.clone(),
            visible,
            format!("{intro}\n{next}\nЗаключение"),
            next,
        ),
        (
            "при равном расстоянии сосед ниже",
            body.clone(),
            visible,
            format!("{intro}\n{previous}\n{next}"),
            next,
        ),
        (
            "сосед выше",
            body,
            visible,
            format!("{intro}\n{previous}\nЗаключение"),
            previous,
        ),
        (
            "перенос по ширине",
            wrapped,
            "Середина абзаца",
            format!("{intro}\n{paragraph}"),
            "Середина абзаца",
        ),
    ];
    for (step, body, marker, shortened, expected) in cases {
        let (view, ui, requests) = scenario(cx, &address);
        let mut original = window(
            address.route.scope,
            52..80,
            HistoryEdge::End,
            HistoryEdge::End,
        );
        let before_byte = body.find(marker).unwrap() + marker.chars().next().unwrap().len_utf8();
        let after_byte =
            shortened.find(expected).unwrap() + expected.chars().next().unwrap().len_utf8();
        original.messages[0].body = MessageBody::Markdown(body);
        answer(
            request(&requests, Load::Open(ChatEntry::Around(anchor))),
            original.clone(),
            ui,
        );

        let (line_y, top) = ui.read(|app| {
            let viewport = &view.read(app).viewport;
            (
                viewport.tested_text_y(anchor, before_byte).unwrap(),
                viewport.list().viewport_bounds().top(),
            )
        });
        // Начинаем читать чуть ниже верхней границы строки, которую потом может удалить правка.
        let (before, old_offset) = ui.update(|window, app| {
            view.update(app, |view, cx| {
                view.viewport.list().scroll_by(line_y - top + px(3.));
                cx.notify();
            });
            window.refresh();
            window.draw(app).clear();
            let viewport = &view.read(app).viewport;
            (
                viewport.tested_text_y(anchor, before_byte).unwrap(),
                viewport.list().logical_scroll_top().offset_in_item,
            )
        });
        assert!((before - top + px(3.)).abs() <= px(1.));
        assert_eq!(
            ui.read(|app| view.read(app).viewport.saved_anchor().unwrap().id),
            anchor
        );

        original.messages[0].body = MessageBody::Markdown(shortened.clone());
        sync(
            &view,
            ui,
            &address,
            SyncChange::HistoryMessageChanged { message_id: anchor },
        );
        let response = request(&requests, Load::Read(original.id));

        let captured = Rc::new(Cell::new(None));
        let capture = captured.clone();
        let subscription = ui.update(|window, app| {
            window.observe(&view, app, move |view, window, app| {
                if capture.get().is_none()
                    && matches!(&view.read(app).state, State::Ready(history)
                        if history.messages[0].body.markdown() == Some(shortened.as_str()))
                {
                    window.refresh();
                    window.draw(app).clear();
                    let viewport = &view.read(app).viewport;
                    capture.set(Some((
                        viewport.tested_text_y(anchor, after_byte),
                        viewport.tested_bounds(anchor),
                    )));
                }
            })
        });
        answer(response, original, ui);
        drop(subscription);

        let (first, bounds) = captured.get().expect("Рабочий экран принял правку");
        let delta = first.map(|y| f32::from(y - before));
        eprintln!(
            "history_edit anchor_uuid={anchor} step={step} before_y_px={} first_frame_y_px={:?} delta_px={delta:?}",
            f32::from(before),
            first.map(f32::from)
        );
        let first = first.expect("Строка текста должна быть в первом кадре");
        assert!(
            (first - before).abs() <= px(1.),
            "{expected}: {before:?} → {first:?}"
        );
        let bounds = bounds.unwrap();
        assert!(bounds.bottom() > top);
        assert!(bounds.size.height < old_offset);
        let settled = ui.read(|app| {
            view.read(app)
                .viewport
                .tested_text_y(anchor, after_byte)
                .unwrap()
        });
        assert!((settled - first).abs() <= px(1.));
    }
}

/// Проверяет разворот, когда порог нового края достигнут до прихода старого ответа.
#[gpui::test]
fn reversed_scroll_prefetches_after_the_pending_page(cx: &mut TestAppContext) {
    check_reversed_scroll(cx, true);
}

/// Проверяет разворот, когда до порога ещё далеко в момент вставки старой страницы.
#[gpui::test]
fn reversed_scroll_waits_until_the_new_edge_is_near(cx: &mut TestAppContext) {
    check_reversed_scroll(cx, false);
}

/// Удерживает старый ответ и сверяет опору в первом кадре после разворота.
fn check_reversed_scroll(cx: &mut TestAppContext, near_newer: bool) {
    use std::{cell::Cell, rc::Rc};

    use gpui::{ScrollDelta, ScrollWheelEvent, point};

    let address = address(ChatEntry::Around(Uuid::from_u128(80)));
    let (view, ui, requests) = scenario(cx, &address);
    answer(
        request(&requests, Load::Open(address.route.entry)),
        reading(&address, 51..111),
        ui,
    );
    assert!(
        requests.try_recv().is_err(),
        "Длинное окно заполняет список"
    );
    let distance = |ui: &VisualTestContext, direction| {
        ui.read(|app| {
            let viewport = &view.read(app).viewport;
            (
                viewport.distance_to_edge(direction),
                viewport.list().viewport_bounds().size.height,
            )
        })
    };
    let scroll = |ui: &mut VisualTestContext, delta| {
        let center = ui
            .debug_bounds("conversation-history-focus")
            .unwrap()
            .center();
        ui.simulate_event(ScrollWheelEvent {
            position: center,
            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
            ..Default::default()
        });
    };
    let approach = |ui: &mut VisualTestContext, direction, delta| {
        (0..100)
            .find_map(|_| {
                let (ahead, height) = distance(ui, direction);
                if ahead <= height {
                    Some(ahead)
                } else {
                    scroll(ui, delta);
                    None
                }
            })
            .unwrap_or_else(|| panic!("Не дошли до порога {direction:?}"))
    };
    let older_ahead = approach(ui, HistoryDirection::Older, 80.);
    assert!(older_ahead > px(0.), "Старые сообщения запрошены до края");
    let old_page = request(&requests, Load::Extend(41, HistoryDirection::Older));
    assert!(ui.debug_bounds("history-background-loading").is_some());

    let old_anchor = ui.read(|app| view.read(app).viewport.saved_anchor().unwrap().id);
    assert!(distance(ui, HistoryDirection::Newer).0 > distance(ui, HistoryDirection::Newer).1);
    if near_newer {
        assert!(
            approach(ui, HistoryDirection::Newer, -80.) > px(0.),
            "Нижний край ещё не достигнут"
        );
    } else {
        scroll(ui, -240.);
        let (ahead, height) = distance(ui, HistoryDirection::Newer);
        assert!(ahead > height, "Развернулись вдали от нижнего края");
    }
    assert!(
        requests.try_recv().is_err(),
        "Пока старый запрос в пути, второй запрос не запускается"
    );
    let (anchor, y_before, op_id) = ui.read(|app| {
        let chat = view.read(app);
        let id = chat.viewport.saved_anchor().unwrap().id;
        (
            id,
            chat.tested_message_window_y(id).unwrap(),
            chat.active.unwrap().id,
        )
    });
    assert_ne!(old_anchor, anchor, "Разворот действительно сменил опору");

    let captured = Rc::new(Cell::new(None));
    let frame = captured.clone();
    let subscription = ui.update(|window, app| {
        window.observe(&view, app, move |chat, window, app| {
            if frame.get().is_none()
                && matches!(&chat.read(app).state, State::Ready(history)
                    if history.messages.first().is_some_and(|row| row.id == Uuid::from_u128(1)))
            {
                window.refresh();
                window.draw(app).clear();
                let chat = chat.read(app);
                frame.set(Some((
                    chat.viewport.saved_anchor().map(|anchor| anchor.id),
                    chat.tested_message_window_y(anchor),
                )));
            }
        })
    });
    let mut extended = reading(&address, 1..111);
    extended.older = HistoryEdge::More("older-next".into());
    answer(old_page, extended, ui);
    drop(subscription);

    let (first_anchor, first_y) = captured.get().expect("Снимок старой страницы отрисован");
    assert_eq!(first_anchor, Some(anchor));
    let first_y = first_y.expect("Текущая опора видна в первом кадре");
    assert!(
        (first_y - y_before).abs() <= px(1.),
        "Опора {anchor}: {y_before:?} → {first_y:?}"
    );
    let settled_y = ui.read(|app| view.read(app).tested_message_window_y(anchor).unwrap());
    assert!((settled_y - y_before).abs() <= px(1.));
    if !near_newer {
        assert!(!ui.read(|app| view.read(app).has_background_history_load()));
        let (ahead, height) = distance(ui, HistoryDirection::Newer);
        assert!(ahead > height, "После вставки порог ещё не достигнут");
        assert!(
            requests.try_recv().is_err(),
            "Смена направления вдали от края не запускает запрос"
        );
        approach(ui, HistoryDirection::Newer, -80.);
    }
    let (ahead, height) = distance(ui, HistoryDirection::Newer);
    assert!(
        ahead > px(0.) && ahead <= height,
        "Новый запрос ушёл заранее"
    );
    let new_page = request(&requests, Load::Extend(41, HistoryDirection::Newer));
    assert!(ui.read(|app| view.read(app).has_background_history_load()));
    assert!(
        requests.try_recv().is_err(),
        "Запрос вниз отправлен один раз"
    );
    eprintln!(
        "history_reverse op_id={op_id} anchor_uuid={anchor} near_newer={near_newer} older_ahead_px={} newer_ahead_px={} y_before_px={} y_first_frame_px={} delta_px={}",
        f32::from(older_ahead),
        f32::from(ahead),
        f32::from(y_before),
        f32::from(first_y),
        f32::from(first_y - y_before),
    );

    let (next_anchor, before_new_page) = ui.read(|app| {
        let chat = view.read(app);
        let id = chat.viewport.saved_anchor().unwrap().id;
        (id, chat.tested_message_window_y(id).unwrap())
    });
    let mut final_page = reading(&address, 1..161);
    final_page.older = HistoryEdge::More("older-next".into());
    final_page.newer = HistoryEdge::End;
    answer(new_page, final_page, ui);
    assert!(!ui.read(|app| view.read(app).has_background_history_load()));
    assert!(requests.try_recv().is_err());
    let after = ui.read(|app| view.read(app).tested_message_window_y(next_anchor).unwrap());
    assert!((after - before_new_page).abs() <= px(1.));
}
