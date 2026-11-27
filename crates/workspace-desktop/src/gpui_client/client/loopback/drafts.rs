//! Проверяет восстановление вкладок в рабочем окне, пока отправка ждёт проверки
//! на локальном HTTPS, и после запуска с тем же SQLite-профилем.

use gpui::MouseButton;

use super::*;

pub(super) const FIRST: Uuid = Uuid::from_u128(161);
pub(super) const SECOND: Uuid = Uuid::from_u128(162);
const FIRST_REPLY: (&str, &str) = (
    "history-quick-bubble-00000000-0000-0000-0000-0000000000a1",
    "history-reply-00000000-0000-0000-0000-0000000000a1",
);
const TOPIC_REPLY: (&str, &str) = (
    "history-quick-bubble-00000000-0000-0000-0000-000000000054",
    "history-reply-00000000-0000-0000-0000-000000000054",
);
const SECOND_ROW: &str = "history-message-00000000-0000-0000-0000-0000000000a2";

/// Фиксирует источник и при необходимости откладывает первую проверку перед очередью.
#[derive(Default)]
pub(super) struct Fixture {
    pub(super) hide_local_topic: bool, // В каталоге временно нет личной темы для ранней гонки.
    release: Mutex<Option<oneshot::Receiver<()>>>, // Удерживает проверку источника.
    checking: AtomicBool,              // Проверка источника дошла до сервера.
    destination: Mutex<Option<oneshot::Receiver<()>>>, // Одна ручная перепроверка цели.
    destination_waiting: AtomicBool,   // Запрос ручной перепроверки уже удерживается.
    sent: Mutex<Vec<String>>,          // Тексты из подтверждённых POST только для теста.
}

impl Fixture {
    /// Задерживает следующий запрос проверки цитаты, пока тест переключает чат.
    fn hold(&self) -> oneshot::Sender<()> {
        let (release, waiting) = oneshot::channel();
        self.checking.store(false, Ordering::SeqCst);
        assert!(self.release.lock().unwrap().replace(waiting).is_none());
        release
    }

    /// Задерживает один запрос ручной перепроверки.
    fn hold_destination(&self) -> oneshot::Sender<()> {
        let (release, waiting) = oneshot::channel();
        self.destination_waiting.store(false, Ordering::SeqCst);
        assert!(self.destination.lock().unwrap().replace(waiting).is_none());
        release
    }

    /// Удерживает ответ перепроверки, пока старое поле принимает новый ввод.
    pub(super) async fn wait_destination(&self) {
        let waiting = self.destination.lock().unwrap().take();
        if let Some(waiting) = waiting {
            self.destination_waiting.store(true, Ordering::SeqCst);
            assert!(waiting.await.is_ok());
        }
    }

    /// Возвращает подтверждённую строку цитаты в обычной личке.
    pub(super) async fn anchor(&self, path: &str, user: &str, peer: &str) -> Response<Full<Bytes>> {
        let message = path[MESSAGES.len()..].parse::<Uuid>().unwrap();
        let (text, second) = match message {
            FIRST => ("Первый источник 🦊", "00"),
            SECOND => ("Второй источник", "01"),
            _ => return json_response(StatusCode::NOT_FOUND, Value::Null),
        };
        let waiting = self.release.lock().unwrap().take();
        if let Some(waiting) = waiting {
            self.checking.store(true, Ordering::SeqCst);
            assert!(waiting.await.is_ok());
        }
        json_response(StatusCode::OK, row(user, peer, message, text, second))
    }

    /// Подтверждает ровно то исходящее сообщение, которое получил сервер.
    pub(super) fn accept(&self, user: &str, post: Value) -> Response<Full<Bytes>> {
        assert_eq!(post["stream_uuid"], STREAM);
        assert_eq!(post["topic_uuid"], TOPIC);
        let text = post["payload"]["content"].as_str().unwrap();
        let canonical = post["uuid"].as_str().unwrap();
        let id = Uuid::new_v5(&TOPIC.parse().unwrap(), canonical.as_bytes());
        self.sent.lock().unwrap().push(text.to_owned());
        json_response(StatusCode::CREATED, row(user, user, id, text, "05"))
    }
}

/// Даёт простой серверный объект с теми же полями, что у обычной лички.
pub(super) fn row(user: &str, author: &str, id: Uuid, text: &str, seconds: &str) -> Value {
    json!({
        "uuid": id, "author_uuid": author, "user_uuid": user, "project_id": PROJECT,
        "stream_uuid": STREAM, "topic_uuid": TOPIC,
        "created_at": format!("2026-11-17T10:00:{seconds}Z"),
        "payload": {"kind": "markdown", "content": text},
    })
}

/// Ждёт доступный редактор ровно того разговора, который только что открыли.
fn ready_text(
    client: &Entity<Client>,
    ui: &mut VisualTestContext,
    topic: Uuid,
    text: &str,
    step: &str,
) {
    until(ui, step, |ui| {
        ui.read(|app| {
            client.read(app).conversation.as_ref().is_some_and(|view| {
                view.read(app).tested_composer().is_some_and(|composer| {
                    let composer = composer.read(app);
                    composer.tested_send_state(app).0 == Some(topic) && composer.draft(app) == text
                })
            })
        })
    });
}

/// Печатает ответ через системный ввод рабочего поля, включая Unicode.
fn enter(ui: &mut VisualTestContext, text: &str) {
    click(ui, "workspace-composer-editor");
    ui.simulate_input(text);
    ui.run_until_parked();
}

/// Начинает первый ответ через кнопку, которая появляется у сообщения при наведении.
fn reply(ui: &mut VisualTestContext, (bubble, action): (&'static str, &'static str)) {
    let bounds = ui.debug_bounds(bubble).unwrap();
    ui.simulate_mouse_move(bounds.center(), None, Modifiers::none());
    until(
        ui,
        "появилась кнопка быстрого ответа",
        |ui| ui.debug_bounds(action).is_some(),
    );
    click(ui, action);
    ui.run_until_parked();
}

/// Выбирает «Добавить ответ» из меню, пока в редакторе уже есть вкладка.
fn add(ui: &mut VisualTestContext, row: &'static str) {
    let bounds = ui.debug_bounds(row).unwrap();
    let point = gpui::point(bounds.center().x, bounds.bottom() - px(5.));
    ui.simulate_mouse_down(point, MouseButton::Right, Modifiers::none());
    ui.simulate_mouse_up(point, MouseButton::Right, Modifiers::none());
    ui.run_until_parked();
    click(ui, "history-reply-add");
    ui.run_until_parked();
}

/// Раскрывает канал штатным действием текущего сайдбара.
fn expand_channel(ui: &mut VisualTestContext) {
    const EXPAND: &str = "catalog-stream-expand-10000000-0000-4000-8000-000000000007";
    ui.run_until_parked();
    let control = if ui.debug_bounds(EXPAND).is_some() {
        EXPAND
    } else {
        CHANNEL_ROW
    };
    click(ui, control);
    ui.run_until_parked();
}

/// Сменяет точную тему через тот же каталог, которым пользуется рабочее окно.
fn switch_topic(ui: &mut VisualTestContext) {
    click(ui, CHANNEL_TOPIC_ROW);
    ui.run_until_parked();
}

/// Прикрепляет к окну штатные события рабочего профиля.
fn attach(client: &Entity<Client>, events: RuntimeEvents, ui: &mut VisualTestContext) {
    ui.update(|_, app| {
        let subscription = events.attach(client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(ui, "каталог рабочего профиля", |ui| {
        ui.debug_bounds(CHANNEL_ROW).is_some() && ui.debug_bounds(CHAT_ROW).is_some()
    });
    expand_channel(ui);
    until(
        ui,
        "строка точной темы развернулась",
        |ui| ui.debug_bounds(CHANNEL_TOPIC_ROW).is_some(),
    );
}

/// Ставит ответы в очередь после ухода из чата и проверяет точную версию после повторного открытия.
#[gpui::test]
fn reply_draft_survives_navigation_and_restart_without_reviving_queued_input(
    cx: &mut TestAppContext,
) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        topic: Some(TopicAcceptance::default()),
        drafting: Some(Fixture::default()),
        ..Received::default()
    });
    let fixture = server.state.drafting.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let account = saved_account(&server, USER, Uuid::nil());
    let (runtime, events) = start_runtime(profile.path(), account.clone());
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    attach(&client, events, ui);
    click(ui, CHAT_ROW);
    let topic = TOPIC.parse().unwrap();
    ready_text(&client, ui, topic, "", "личка открылась с чистым полем");

    for (newer, expected) in [(None, ""), (Some(" новый"), "Отвечаю 🦊 новый")] {
        reply(ui, FIRST_REPLY);
        enter(ui, "Отвечаю 🦊");
        let release = fixture.hold();
        press_enter(ui);
        until(
            ui,
            "сервер принял проверку цитаты до очереди",
            |_| fixture.checking.load(Ordering::SeqCst),
        );
        if let Some(newer) = newer {
            enter(ui, newer);
        }
        switch_topic(ui);
        ready_text(
            &client,
            ui,
            CHANNEL_TOPIC.parse().unwrap(),
            "",
            "чужой черновик не попал в соседнюю тему",
        );
        click(ui, CHAT_ROW);
        ui.run_until_parked();
        assert!(release.send(()).is_ok());
        ready_text(
            &client,
            ui,
            topic,
            expected,
            "личка показала актуальную версию после очереди",
        );
    }
    until(
        ui,
        "обе очереди дошли до локального HTTPS",
        |_| fixture.sent.lock().unwrap().len() == 2,
    );
    assert!(
        fixture
            .sent
            .lock()
            .unwrap()
            .iter()
            .all(|text| !text.contains(" новый") && text.contains("Отвечаю 🦊"))
    );

    add(ui, SECOND_ROW);
    enter(ui, "Второй ответ\nすし");
    click(ui, "composer-reply-tab-0");
    ready_text(
        &client,
        ui,
        topic,
        "Отвечаю 🦊 новый",
        "выбран первый ответ перед переходом",
    );
    switch_topic(ui);
    click(ui, CHAT_ROW);
    ready_text(
        &client,
        ui,
        topic,
        "Отвечаю 🦊 новый",
        "первый ответ восстановился при возврате",
    );
    let from = ui.debug_bounds("composer-reply-tab-0").unwrap().center();
    let after_second = ui.debug_bounds("composer-reply-remove-1").unwrap().center();
    ui.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    ui.run_until_parked();
    for _ in 0..2 {
        ui.simulate_mouse_move(after_second, MouseButton::Left, Modifiers::none());
        ui.run_until_parked();
    }
    assert!(ui.debug_bounds("composer-reply-drop-indicator").is_some());
    ui.simulate_mouse_up(after_second, MouseButton::Left, Modifiers::none());
    switch_topic(ui);
    click(ui, CHAT_ROW);
    ready_text(
        &client,
        ui,
        topic,
        "Отвечаю 🦊 новый",
        "перестановка и переход оставили первый ответ активным",
    );
    click(ui, "composer-reply-tab-0");
    ready_text(
        &client,
        ui,
        topic,
        "Второй ответ\nすし",
        "соседний ответ восстановился в начале ряда после перетаскивания",
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );

    let (runtime, events) = start_runtime(profile.path(), account);
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    attach(&client, events, ui);
    click(ui, CHAT_ROW);
    ready_text(
        &client,
        ui,
        topic,
        "Второй ответ\nすし",
        "после перезапуска выбрана та же вкладка",
    );
    click(ui, "composer-reply-remove-0");
    ready_text(
        &client,
        ui,
        topic,
        "Отвечаю 🦊 новый",
        "удалена только вторая вкладка",
    );
    switch_topic(ui);
    click(ui, CHAT_ROW);
    ready_text(
        &client,
        ui,
        topic,
        "Отвечаю 🦊 новый",
        "после возврата удалённый ответ не восстановился",
    );
    click(ui, "composer-reply-clear");
    switch_topic(ui);
    click(ui, CHAT_ROW);
    ready_text(
        &client,
        ui,
        topic,
        "Отвечаю 🦊 новый",
        "общая отмена сохранила ранее набранный ответ",
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// При повторной проверке ответ остаётся видимым. После отзыва он не возвращается.
#[gpui::test]
fn rechecking_keeps_answer_until_explicit_topic_revocation(cx: &mut TestAppContext) {
    use workspace_core::storage::{Cursor, HistoryChange, MessagePlacement};

    cx.update(bind_keys);
    let server = Server::serve(Received {
        topic: Some(TopicAcceptance::default()),
        drafting: Some(Fixture {
            hide_local_topic: true,
            ..Fixture::default()
        }),
        ..Received::default()
    });
    let fixture = server.state.drafting.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    attach(&client, events, ui);
    click(ui, CHAT_ROW);
    let topic = TOPIC.parse().unwrap();
    ready_text(&client, ui, topic, "", "готова исходная личка");
    reply(ui, FIRST_REPLY);
    enter(ui, "Ответ до перепроверки");

    // Отказ сохраняет ответ и предлагает ручную перепроверку под историей.
    server
        .state
        .direct_unavailable
        .store(true, Ordering::SeqCst);
    press_enter(ui);
    until(
        ui,
        "под историей предлагается повторная проверка",
        |ui| ui.debug_bounds("conversation-send-retry").is_some(),
    );
    server
        .state
        .direct_unavailable
        .store(false, Ordering::SeqCst);
    let release = fixture.hold_destination();
    click(ui, "conversation-send-retry");
    until(
        ui,
        "локальный сервер удерживает проверку",
        |_| fixture.destination_waiting.load(Ordering::SeqCst),
    );
    assert!(ui.debug_bounds("workspace-composer-editor").is_some());
    assert!(ui.read(|app| {
        let view = client.read(app).conversation.as_ref().unwrap();
        view.read(app).history_and_editor_focus(app).1.is_some()
    }));
    switch_topic(ui);
    ready_text(
        &client,
        ui,
        CHANNEL_TOPIC.parse().unwrap(),
        "",
        "открыта другая тема",
    );
    release.send(()).unwrap();
    click(ui, CHAT_ROW);
    ready_text(
        &client,
        ui,
        topic,
        "Ответ до перепроверки",
        "ответ сохранился во время проверки",
    );

    // Подтверждённый отзыв проходит через настоящее хранилище и подписку живого окна.
    let address = ui.read(|app| client.read(app).shell.conversation().unwrap().clone());
    server
        .state
        .direct_topic_changed
        .store(true, Ordering::SeqCst);
    let revoked = futures::executor::block_on(runtime.request(move |services| async move {
        services.sync.apply(
            &address.route.owner,
            address.generation,
            SOURCE_ID,
            Event::HistoryChanged {
                cursor: Cursor {
                    epoch_generation: "local".into(),
                    epoch_version: 2,
                },
                change: HistoryChange::RemoveTopic(MessagePlacement {
                    stream_id: STREAM.into(),
                    topic_id: TOPIC.into(),
                }),
            },
        )
    }))
    .unwrap()
    .unwrap();
    assert_eq!(revoked, workspace_core::sync::ApplyOutcome::Applied);
    let next = NEXT_DIRECT_TOPIC.parse().unwrap();
    ready_text(
        &client,
        ui,
        next,
        "",
        "отозванный ответ не скопировался в новый default",
    );
    switch_topic(ui);
    click(ui, CHAT_ROW);
    ready_text(
        &client,
        ui,
        next,
        "",
        "после возврата новая тема тоже чистая",
    );
    assert!(fixture.sent.lock().unwrap().is_empty());
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Черновик независим для двух владельцев и читается в закрытой обычной теме.
#[gpui::test]
fn closed_topic_keeps_reply_draft_scoped_to_its_owner(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::start_with_topic();
    let gate = &server.state.topic.as_ref().unwrap().gate;
    let profile = tempfile::tempdir().unwrap();
    let account = saved_account(&server, USER, Uuid::nil());
    let other = saved_account(&server, PEER, PEER.parse().unwrap());
    JsonAccountStore::open(profile.path())
        .unwrap()
        .save(&other)
        .unwrap();
    let owner = account.context.account_id.clone();
    let peer = other.context.account_id;
    let (runtime, events) = start_runtime(profile.path(), account);
    let (client, ui) = cx.add_window_view(|_, cx| {
        Client::new(
            Options {
                active: Some(owner.clone()),
                ..Options::default()
            },
            runtime.clone(),
            cx,
        )
    });
    attach(&client, events, ui);
    switch_topic(ui);
    topic_ready(&client, ui, "открыта тема автора черновика");
    reply(ui, TOPIC_REPLY);
    enter(ui, "Ответ автора в теме");
    click_account(&client, ui, &peer);
    expand_channel(ui);
    switch_topic(ui);
    let topic = CHANNEL_TOPIC.parse().unwrap();
    ready_text(
        &client,
        ui,
        topic,
        "",
        "тот же UUID темы не раскрыл чужой ответ",
    );
    reply(ui, TOPIC_REPLY);
    enter(ui, "Ответ второго владельца");

    click_account(&client, ui, &owner);
    *gate.lock().unwrap() = TopicGate::Closed;
    switch_topic(ui);
    until(
        ui,
        "в закрытой теме плашка скрывает сохранённый ответ первого владельца",
        |ui| {
            ui.read(|app| {
                client.read(app).conversation.as_ref().is_some_and(|view| {
                    view.read(app).has_access()
                        && view.read(app).tested_composer().is_some_and(|composer| {
                            let editor = composer.read(app);
                            editor.tested_send_state(app).0.is_none()
                                && editor.draft(app) == "Ответ автора в теме"
                        })
                })
            }) && ui.debug_bounds("conversation-send-notice").is_some()
        },
    );
    assert!(ui.read(|app| {
        let view = client.read(app).conversation.as_ref().unwrap();
        view.read(app).history_and_editor_focus(app).1.is_none()
    }));
    press_enter(ui);
    assert_eq!(
        server
            .state
            .topic
            .as_ref()
            .unwrap()
            .attempts
            .load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Локальный черновик доступен до сети и сохраняет правку после перехода в другую тему.
#[gpui::test]
fn local_draft_survives_navigation_before_send_confirmation(cx: &mut TestAppContext) {
    use workspace_core::storage::{DraftScope, DraftWriteOutcome, PlaintextDraft};

    cx.update(bind_keys);
    let server = Server::serve(Received {
        topic: Some(TopicAcceptance::default()),
        drafting: Some(Fixture::default()),
        ..Received::default()
    });
    let fixture = server.state.drafting.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    attach(&client, events, ui);
    let session = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let owner = session.owner.clone();
    let generation = session.generation;
    let scope = DraftScope {
        stream_id: STREAM.parse().unwrap(),
        topic_id: TOPIC.parse().unwrap(),
    };
    let saved_id = Uuid::from_u128(0xd4);
    let outcome = futures::executor::block_on(runtime.request(move |services| async move {
        let opening = services
            .sync
            .prepare_draft_opening(&owner, generation, scope.stream_id)?;
        let (access, _) = services
            .sync
            .open_draft(&owner, generation, opening, scope)?;
        services.sync.save_draft(
            &owner,
            generation,
            &access,
            &PlaintextDraft {
                id: saved_id,
                version: 1,
                markdown: "Старый черновик".into(),
                replies: None,
            },
        )
    }))
    .unwrap()
    .unwrap();
    assert_eq!(outcome, DraftWriteOutcome::Saved);
    let owner = session.owner.clone();
    let generation = session.generation;
    let local = futures::executor::block_on(runtime.request(move |services| async move {
        services
            .sync
            .read_local_plaintext_draft(&owner, generation, scope, None)
    }))
    .unwrap()
    .unwrap();
    assert!(
        matches!(local, Some(Some(value)) if value.id == saved_id),
        "Каталог и локальная запись должны быть доступны до перехода"
    );

    let release = fixture.hold_destination();
    click(ui, CHAT_ROW);
    until(
        ui,
        "сервер удерживает первую проверку лички",
        |_| fixture.destination_waiting.load(Ordering::SeqCst),
    );
    until(
        ui,
        "локальный черновик виден до ответа проверки",
        |ui| {
            ui.read(|app| {
                client.read(app).conversation.as_ref().is_some_and(|view| {
                    view.read(app)
                        .tested_composer()
                        .is_some_and(|composer| composer.read(app).draft(app) == "Старый черновик")
                })
            })
        },
    );
    enter(ui, " с правкой");
    switch_topic(ui);
    ready_text(
        &client,
        ui,
        CHANNEL_TOPIC.parse().unwrap(),
        "",
        "открыта другая тема",
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "ранний ввод вернулся до позднего ответа",
        |ui| {
            ui.read(|app| {
                client.read(app).conversation.as_ref().is_some_and(|view| {
                    view.read(app).tested_composer().is_some_and(|composer| {
                        composer.read(app).draft(app) == "Старый черновик с правкой"
                    })
                })
            })
        },
    );
    assert!(release.send(()).is_ok());
    ready_text(
        &client,
        ui,
        TOPIC.parse().unwrap(),
        "Старый черновик с правкой",
        "личка подтвердилась без потери ввода",
    );
    until(
        ui,
        "правка сохранена в исходный черновик",
        |_| {
            futures::executor::block_on(runtime.list_local_drafts(session.clone())).is_ok_and(
                |rows| {
                    rows.iter().any(|row| {
                        row.version.id == saved_id
                            && row.preview == "Старый черновик с правкой".into()
                    })
                },
            )
        },
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Ранний ввод остаётся выбранным после перезапуска, а прежний черновик не теряется.
#[gpui::test]
fn early_input_is_restored_after_restart_with_older_draft(cx: &mut TestAppContext) {
    use workspace_core::storage::{DraftScope, DraftWriteOutcome, PlaintextDraft};

    cx.update(bind_keys);
    let server = Server::serve(Received {
        topic: Some(TopicAcceptance::default()),
        drafting: Some(Fixture {
            hide_local_topic: true,
            ..Fixture::default()
        }),
        ..Received::default()
    });
    let fixture = server.state.drafting.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let account = saved_account(&server, USER, Uuid::nil());
    let (runtime, events) = start_runtime(profile.path(), account.clone());
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    attach(&client, events, ui);
    let session = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let owner = session.owner.clone();
    let generation = session.generation;
    let scope = DraftScope {
        stream_id: STREAM.parse().unwrap(),
        topic_id: TOPIC.parse().unwrap(),
    };
    let result = futures::executor::block_on(runtime.request(move |services| async move {
        let opening = services
            .sync
            .prepare_draft_opening(&owner, generation, scope.stream_id)?;
        let (access, _) = services
            .sync
            .open_draft(&owner, generation, opening, scope)?;
        services.sync.save_draft(
            &owner,
            generation,
            &access,
            &PlaintextDraft {
                id: Uuid::from_u128(0xd5),
                version: 1,
                markdown: "Старый вариант".into(),
                replies: None,
            },
        )
    }))
    .unwrap()
    .unwrap();
    assert_eq!(result, DraftWriteOutcome::Saved);
    let release = fixture.hold_destination();
    click(ui, CHAT_ROW);
    until(
        ui,
        "сервер удерживает проверку",
        |_| fixture.destination_waiting.load(Ordering::SeqCst),
    );
    until(
        ui,
        "поле доступно до сетевой проверки",
        |ui| ui.debug_bounds("workspace-composer-editor").is_some(),
    );
    click(ui, "workspace-composer-editor");
    ui.simulate_keystrokes("secondary-a");
    ui.simulate_input("Новый вариант");
    switch_topic(ui);
    ready_text(
        &client,
        ui,
        CHANNEL_TOPIC.parse().unwrap(),
        "",
        "открылась соседняя тема",
    );
    click(ui, CHAT_ROW);
    until(ui, "ранний ввод вернулся", |ui| {
        ui.read(|app| {
            client.read(app).conversation.as_ref().is_some_and(|view| {
                view.read(app)
                    .tested_composer()
                    .is_some_and(|composer| composer.read(app).draft(app) == "Новый вариант")
            })
        })
    });
    assert!(release.send(()).is_ok());
    ready_text(
        &client,
        ui,
        TOPIC.parse().unwrap(),
        "Новый вариант",
        "новый вариант после проверки",
    );
    until(ui, "оба варианта сохранены", |_| {
        futures::executor::block_on(runtime.list_local_drafts(session.clone())).is_ok_and(|rows| {
            rows.iter()
                .any(|row| row.preview == "Старый вариант".into())
                && rows.iter().any(|row| row.preview == "Новый вариант".into())
        })
    });
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );

    let (runtime, events) = start_runtime(profile.path(), account);
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    attach(&client, events, ui);
    click(ui, CHAT_ROW);
    ready_text(
        &client,
        ui,
        TOPIC.parse().unwrap(),
        "Новый вариант",
        "после перезапуска восстановлен последний ввод",
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
