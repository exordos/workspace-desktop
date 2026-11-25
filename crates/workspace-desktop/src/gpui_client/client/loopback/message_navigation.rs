//! Проверяет переходы из ссылок ленты через рабочее окно GPUI и управляемые ответы HTTPS.

use workspace_core::messenger::{forward::ForwardReference, references::message_permalink};

use super::*;

const FIRST_ROW: u128 = 101;
const FAST: Uuid = Uuid::from_u128(110);
const FAST_LINK_ROW: Uuid = Uuid::from_u128(138);
const MARK_FAST: &str = "history-message-mark-00000000-0000-0000-0000-00000000006e";
const LINK_FAST: &str = "history-inline-00000000-0000-0000-0000-00000000008a-0";
const LINK_DENIED: &str = "history-inline-00000000-0000-0000-0000-00000000008b-0";
const FORWARD_DELAYED: &str =
    "forward-original-00000000-0000-0000-0000-00000000008c-00000000-0000-0000-0000-0000000001c2";
const LINK_REMOTE: &str = "history-inline-00000000-0000-0000-0000-00000000008d-0";
const LATEST_ROW: u128 = 141;
const DENIED: Uuid = Uuid::from_u128(400);
const DELAYED: Uuid = Uuid::from_u128(450);
const REMOTE: Uuid = Uuid::from_u128(500);
const MARK_REMOTE: &str = "history-message-mark-00000000-0000-0000-0000-0000000001f4";
const ERROR: &str = "message-navigation-error";
const RETRY: &str = "message-navigation-retry";

/// Даёт текущей личке ссылки на загруженное, недоступное и два удалённых сообщения.
#[derive(Default)]
pub(super) struct Fixture {
    permalink: Mutex<String>, // Точная ссылка, которую клиент скопировал бы в этом входе.
    anchor_reads: Mutex<Vec<Uuid>>, // Подробные запросы по UUID для проверки короткого пути.
    pages: Mutex<Vec<(Uuid, bool)>>, // Страницы вокруг UUID. True означает более новые сообщения.
    remote_ready: AtomicBool, // Соседние страницы временно недоступны до повтора.
    release_older: Mutex<Option<oneshot::Receiver<()>>>, // Удерживает запрос до нового клика.
    delayed_page_sent: AtomicBool, // Сервер закончил обработку задержанной страницы.
    forward_reactions: bool, // Показывает серверные счётчики на карточке для проверки нижней строки.
}

impl Fixture {
    /// Отвечает только на GET истории и фиксирует обращения к целям по отдельности.
    pub(super) async fn response(
        &self,
        request: &Request<Incoming>,
        user: &str,
        peer: &str,
    ) -> Option<Response<Full<Bytes>>> {
        let path = request.uri().path();
        if request.method() != Method::GET || !path.starts_with(MESSAGES) {
            return None;
        }
        if path != MESSAGES {
            let id: Uuid = path[MESSAGES.len()..].parse().unwrap();
            self.anchor_reads.lock().unwrap().push(id);
            let response = match id {
                DENIED => json_response(StatusCode::FORBIDDEN, Value::Null),
                REMOTE | DELAYED => json_response(StatusCode::OK, topic_row(id, user, peer)),
                _ => json_response(StatusCode::OK, self.direct_row(id.as_u128(), user, peer)),
            };
            return Some(response);
        }
        if query_value(request, "stream_uuid") != Some(CHANNEL) {
            let rows = (FIRST_ROW..=LATEST_ROW)
                .rev()
                .map(|number| self.direct_row(number, user, peer))
                .collect();
            return Some(json_response(StatusCode::OK, Value::Array(rows)));
        }

        assert_eq!(query_value(request, "topic_uuid"), Some(CHANNEL_TOPIC));
        let marker: Uuid = query_value(request, "page_marker")
            .expect("История канала загружается вокруг конкретного сообщения")
            .parse()
            .unwrap();
        let newer = query_value(request, "sort_dir") == Some("asc");
        self.pages.lock().unwrap().push((marker, newer));
        if marker == REMOTE && !self.remote_ready.load(Ordering::SeqCst) {
            return Some(json_response(StatusCode::SERVICE_UNAVAILABLE, Value::Null));
        }
        let delayed = marker == DELAYED && !newer;
        if delayed {
            let release = self.release_older.lock().unwrap().take().unwrap();
            let _ = release.await;
            self.delayed_page_sent.store(true, Ordering::SeqCst);
        }
        let anchor = marker.as_u128();
        let numbers: Vec<_> = if newer {
            ((anchor + 1)..=(anchor + 14)).collect()
        } else {
            ((anchor - 14)..anchor).rev().collect()
        };
        let rows = numbers
            .into_iter()
            .map(|number| topic_row(Uuid::from_u128(number), user, peer))
            .collect();
        Some(json_response(StatusCode::OK, Value::Array(rows)))
    }

    /// Собирает хвост лички, включая ссылки в последних сообщениях и цель выше экрана.
    fn direct_row(&self, number: u128, user: &str, peer: &str) -> Value {
        let content = match number {
            138 => format!(
                "[Загруженное сообщение]({})",
                self.permalink.lock().unwrap()
            ),
            139 => format!("[Закрытое сообщение](urn:message:{DENIED})"),
            140 => ForwardReference {
                id: DELAYED,
                author: "Автор".into(),
                snapshot: "Пересланный текст".into(),
                plain: true,
                source: None,
            }
            .to_markdown(),
            141 => format!("[Сообщение из канала](urn:message:{REMOTE})"),
            _ => format!("Строка лички {number}"),
        };
        let mut row = json!({
            "uuid": Uuid::from_u128(number), "stream_uuid": STREAM, "topic_uuid": TOPIC,
            "project_id": PROJECT, "user_uuid": user, "author_uuid": peer,
            "created_at": format!("2026-11-17T10:00:00.{number:06}Z"),
            "payload": {"kind": "markdown", "content": content},
        });
        if self.forward_reactions && number == 140 {
            row["reactions"] = json!({"heart": 1, "joy": 1, "thumbsup": 1});
        }
        row
    }
}

/// Даёт соседям и цели точные сведения одной темы канала.
fn topic_row(id: Uuid, user: &str, peer: &str) -> Value {
    topic_message(
        user,
        peer,
        id,
        &format!("2026-11-17T10:00:00.{:06}Z", id.as_u128()),
        &format!("Строка темы {}", id.as_u128()),
    )
}

/// Открывает хвост лички по штатному сайдбару и ждёт ссылок в её видимой области.
fn open_source<'a>(
    cx: &'a mut TestAppContext,
    server: &Server,
    profile: &std::path::Path,
) -> (Entity<Client>, &'a mut VisualTestContext, RuntimeClient) {
    cx.update(bind_keys);
    let account = saved_account(server, USER, Uuid::nil());
    let fixture = server.state.message_navigation.as_ref().unwrap();
    *fixture.permalink.lock().unwrap() = message_permalink(&account.context, FAST).unwrap();
    let (runtime, events) = start_runtime(profile, account);
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
        Client::install_settings_input(&client, app);
    });
    ui.simulate_resize(size(px(1080.), px(680.)));
    until(ui, "личка доступна в каталоге", |ui| {
        server.state.ready.load(Ordering::SeqCst) > 0 && ui.debug_bounds(CHAT_ROW).is_some()
    });
    click(ui, CHAT_ROW);
    until(
        ui,
        "видны ссылки из хвоста лички",
        |ui| {
            ui.debug_bounds(LINK_FAST).is_some()
                && ui.debug_bounds(FORWARD_DELAYED).is_some()
                && ui.debug_bounds(LINK_REMOTE).is_some()
                && ui.read(|app| {
                    client.read(app).conversation.as_ref().is_some_and(|view| {
                        view.read(app)
                            .tested_composer()
                            .is_some_and(|editor| editor.read(app).is_available())
                    })
                })
        },
    );
    (client, ui, runtime)
}

/// Нажимает на глифы ссылки. GPUI назначает обработчик отпускания после кадра с нажатием.
fn click_link(ui: &mut VisualTestContext, selector: &'static str) {
    let bounds = ui.debug_bounds(selector).expect("Ссылка должна быть видна");
    let position = gpui::point(bounds.center().x, bounds.top() + px(4.));
    ui.simulate_mouse_move(position, None, Modifiers::none());
    ui.update(|window, app| window.draw(app).clear());
    ui.simulate_mouse_down(position, gpui::MouseButton::Left, Modifiers::none());
    ui.update(|window, app| window.draw(app).clear());
    ui.simulate_mouse_up(position, gpui::MouseButton::Left, Modifiers::none());
}

/// Размещает настоящее серверное время рядом с реакциями и переносит его в узкой ленте.
#[gpui::test]
fn forward_footer_shares_row_with_history_reactions(cx: &mut TestAppContext) {
    const CARD: &str =
        "history-forward-00000000-0000-0000-0000-00000000008c-00000000-0000-0000-0000-0000000001c2";
    const FOOTER: &str = "message-block-footer";
    const TIME: &str = "message-block-footer-time";
    const NEXT: &str = "history-message-00000000-0000-0000-0000-00000000008d";
    const CHIPS: [&str; 3] = [
        "history-reaction-chip-00000000-0000-0000-0000-00000000008c-heart",
        "history-reaction-chip-00000000-0000-0000-0000-00000000008c-joy",
        "history-reaction-chip-00000000-0000-0000-0000-00000000008c-thumbsup",
    ];
    let server = Server::serve(Received {
        message_navigation: Some(Fixture {
            forward_reactions: true,
            ..Fixture::default()
        }),
        ..Received::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (_, ui, runtime) = open_source(cx, &server, profile.path());
    let geometry = |ui: &mut VisualTestContext| {
        let card = ui.debug_bounds(CARD).expect("Видна пересылка");
        let footer = ui.debug_bounds(FOOTER).expect("Видна нижняя строка");
        let time = ui.debug_bounds(TIME).expect("Время бабла в нижней строке");
        let chips = CHIPS.map(|chip| ui.debug_bounds(chip).expect("Видна серверная реакция"));
        let next = ui.debug_bounds(NEXT).expect("Видно следующее сообщение");
        assert_eq!(card.right(), time.right());
        assert!(card.bottom() <= footer.top() && footer.bottom() <= next.top());
        for bounds in [time].iter().chain(&chips) {
            assert!(
                bounds.left() >= footer.left()
                    && bounds.right() <= footer.right()
                    && bounds.top() >= footer.top()
                    && bounds.bottom() <= footer.bottom(),
                "Реакция или время вышли за нижнюю строку: {bounds:?} за {footer:?}"
            );
        }
        for pair in chips.windows(2) {
            assert!(pair[0].right() <= pair[1].left() || pair[0].bottom() <= pair[1].top());
        }
        (time, chips)
    };
    let (time, chips) = geometry(ui);
    assert!(
        chips
            .iter()
            .all(|chip| chip.right() < time.left() && chip.bottom() == time.bottom())
    );

    ui.simulate_resize(size(px(520.), px(680.)));
    ui.run_until_parked();
    let (time, chips) = geometry(ui);
    assert!(chips[2].top() > chips[0].top() || time.top() >= chips[0].bottom());
    assert!(
        chips
            .iter()
            .all(|chip| chip.bottom() <= time.top() || chip.right() <= time.left())
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Новая ссылка в текущей ленте выигрывает у незавершённого перехода из пересылки.
#[gpui::test]
fn message_navigation_loaded_permalink_supersedes_delayed_window(cx: &mut TestAppContext) {
    let (release, waiting) = oneshot::channel();
    let server = Server::serve(Received {
        message_navigation: Some(Fixture {
            release_older: Mutex::new(Some(waiting)),
            ..Fixture::default()
        }),
        topic: Some(TopicAcceptance::default()),
        ..Received::default()
    });
    let fixture = server.state.message_navigation.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (client, ui, runtime) = open_source(cx, &server, profile.path());
    let initial = ui.read(|app| client.read(app).conversation.as_ref().unwrap().entity_id());
    let active_composer = |ui: &VisualTestContext| {
        ui.read(|app| {
            let view = client.read(app).conversation.as_ref().unwrap().read(app);
            view.tested_composer().cloned()
        })
        .expect("У открытой лички есть редактор")
    };
    let composer = active_composer(ui);
    assert!(ui.debug_bounds(MARK_FAST).is_none());
    click(ui, FORWARD_DELAYED);
    until(
        ui,
        "проверенная цель ждёт старую страницу",
        |_| fixture.pages.lock().unwrap().contains(&(DELAYED, false)),
    );
    assert!(ui.debug_bounds(LINK_FAST).is_some());
    until(
        ui,
        "редактор доступен во время запроса другой темы",
        |ui| ui.read(|app| composer.read(app).is_available()),
    );
    let draft = "Черновик набран во время перехода";
    click(ui, "workspace-composer-editor");
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string(draft.into())));
    ui.simulate_keystrokes("cmd-v");
    ui.run_until_parked();
    let source_y = |ui: &VisualTestContext| {
        ui.read(|app| {
            let view = client.read(app).conversation.as_ref()?.read(app);
            let (viewport, row) = view.tested_history_row_bounds(FAST_LINK_ROW)?;
            (row.top() >= viewport.top() && row.bottom() <= viewport.bottom()).then_some(row.top())
        })
    };
    let before = source_y(ui).expect("До перехода исходная ссылка видна");

    click_link(ui, LINK_FAST);
    until(
        ui,
        "готовая цель показана в том же разговоре",
        |ui| ui.debug_bounds(MARK_FAST).is_some(),
    );
    assert_eq!(
        ui.read(|app| client.read(app).conversation.as_ref().unwrap().entity_id()),
        initial
    );
    assert!(!fixture.anchor_reads.lock().unwrap().contains(&FAST));
    assert!(source_y(ui).is_none());
    let assert_draft = |ui: &VisualTestContext| {
        let current = active_composer(ui);
        assert_eq!(current.entity_id(), composer.entity_id());
        ui.read(|app| assert_eq!(current.read(app).draft(app), draft));
    };
    assert_draft(ui);
    release.send(()).unwrap();
    until(
        ui,
        "поздняя страница отдана сервером",
        |_| fixture.delayed_page_sent.load(Ordering::SeqCst),
    );
    ui.simulate_keystrokes("alt-left");
    until(
        ui,
        "Назад возвращает на видимые ссылки хвоста",
        |ui| source_y(ui).is_some_and(|after| (after - before).abs() <= px(1.)),
    );
    ui.read(|app| {
        let route = &client.read(app).shell.conversation().unwrap().route;
        assert_eq!(route.scope.stream_id.to_string(), STREAM);
    });
    assert_draft(ui);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Отказ оставляет ленту открытой, а повтор после временного сбоя находит точную тему.
#[gpui::test]
fn message_navigation_reports_denial_and_retries_before_showing_target(cx: &mut TestAppContext) {
    let server = Server::serve(Received {
        message_navigation: Some(Fixture::default()),
        topic: Some(TopicAcceptance::default()),
        ..Received::default()
    });
    let fixture = server.state.message_navigation.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (client, ui, runtime) = open_source(cx, &server, profile.path());
    let initial = ui.read(|app| client.read(app).conversation.as_ref().unwrap().entity_id());
    // Воспроизводим переход из уже используемого окна после установки фокуса штатным полем.
    click(ui, "workspace-composer-editor");
    ui.update(|window, app| window.draw(app).clear());
    click_link(ui, LINK_DENIED);
    until(
        ui,
        "ошибка доступа показана над исходной лентой",
        |ui| ui.debug_bounds(ERROR).is_some(),
    );
    assert!(ui.debug_bounds(RETRY).is_none());
    assert!(ui.debug_bounds(LINK_REMOTE).is_some());
    assert_eq!(
        ui.read(|app| client.read(app).conversation.as_ref().unwrap().entity_id()),
        initial
    );

    click_link(ui, LINK_REMOTE);
    until(
        ui,
        "временная ошибка предлагает повтор",
        |ui| ui.debug_bounds(ERROR).is_some() && ui.debug_bounds(RETRY).is_some(),
    );
    assert!(ui.debug_bounds(LINK_REMOTE).is_some());
    fixture.remote_ready.store(true, Ordering::SeqCst);
    click(ui, RETRY);
    until(
        ui,
        "новая лента сразу показывает отмеченный оригинал",
        |ui| {
            let opened = ui.read(|app| {
                let route = &client.read(app).shell.conversation().unwrap().route;
                if route.entry != crate::gpui_client::contracts::ChatEntry::Around(REMOTE) {
                    return false;
                }
                assert_eq!(route.scope.stream_id.to_string(), CHANNEL);
                assert_eq!(route.scope.topic_id.unwrap().to_string(), CHANNEL_TOPIC);
                true
            });
            if opened {
                assert!(
                    ui.debug_bounds(MARK_REMOTE).is_some(),
                    "Первый кадр новой ленты уже содержит отметку"
                );
            }
            opened
        },
    );
    assert!(fixture.pages.lock().unwrap().contains(&(REMOTE, false)));
    assert!(fixture.pages.lock().unwrap().contains(&(REMOTE, true)));
    assert!(ui.read(|app| client.read(app).message_navigation.is_none()));
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
