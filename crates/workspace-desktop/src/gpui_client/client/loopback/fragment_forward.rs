//! Проверяет пересылку нескольких спойлеров и отзыв ключа в настоящем окне на локальном HTTPS.

use workspace_core::messenger::{
    encryption::{GroupKey, fragments::fragment_links},
    forward::forward_spans,
};

use super::*;

const ORIGINAL: &str = "10000000-0000-4000-8000-000000000042";
const SECRET: &str = "PRIVATE-FORWARD-FIXTURE";
// Синтетический конверт для ключа [7;32], вычислен отдельно через Node/OpenSSL.
const SNAPSHOT: &str = "до   [первый секрет](urn:secret:v1:10000000-0000-4000-8000-000000000005:10000000-0000-4000-8000-000000000080:FRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzS7qvkRcF5D89oetxPZ5cK3tfeCp8PQ-abZkA4ZwxWKxd0A0sAgR-g) между   словами [второй секрет](urn:secret:v1:10000000-0000-4000-8000-000000000005:10000000-0000-4000-8000-000000000080:FRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzS7qvkRcF5D89oetxPZ5cK3tfeCp8PQ-abZkA4ZwxWKxd0A0sAgR-g) после";

/// Нативная смешанная строка и её подтверждённая пересылка в тему другого канала.
#[derive(Default)]
pub(super) struct Fixture {
    sent: Mutex<Option<Value>>, // Эхо принятого Markdown после штатной очереди.
    deny_original: AtomicBool,  // Сервер отказывает в переходе к оригиналу.
    denied_original_reads: AtomicUsize, // Подтверждает точный GET запрещённого оригинала.
}

impl Fixture {
    /// Ограничивает подмену HTTP ответами истории двух тестовых маршрутов.
    pub(super) fn handles(&self, request: &Request<Incoming>) -> bool {
        matches!(request.uri().path(), MESSAGES)
            || request.uri().path() == format!("{MESSAGES}{ORIGINAL}")
    }

    /// Отдаёт оригинал и принимает одну пересылку с сохранением точной полезной нагрузки.
    pub(super) async fn response(
        &self,
        request: Request<Incoming>,
        user: &str,
        peer: &str,
    ) -> Response<Full<Bytes>> {
        if request.method() == Method::POST {
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            let post: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(post["payload"]["kind"], "markdown");
            assert_eq!(post["stream_uuid"], CHANNEL);
            assert_eq!(post["topic_uuid"], CHANNEL_TOPIC);
            let canonical = post["uuid"].as_str().unwrap();
            let id = Uuid::new_v5(
                &Uuid::parse_str(CHANNEL_TOPIC).unwrap(),
                canonical.as_bytes(),
            );
            let mut row = topic_message(
                user,
                user,
                id,
                "2026-01-01T12:01:00Z",
                post["payload"]["content"].as_str().unwrap(),
            );
            row["source_name"] = json!("native");
            row["canonical_message_uuid"] = post["uuid"].clone();
            *self.sent.lock().unwrap() = Some(row.clone());
            return json_response(StatusCode::CREATED, row);
        }
        let row = json!({
            "uuid":ORIGINAL, "stream_uuid":STREAM, "topic_uuid":TOPIC, "project_id":PROJECT,
            "user_uuid":user, "author_uuid":peer, "source_name":"native",
            "created_at":"2026-01-01T12:00:00Z", "payload":{"kind":"markdown","content":SNAPSHOT},
        });
        let result = if request.uri().path() != MESSAGES {
            if self.deny_original.load(Ordering::SeqCst) {
                self.denied_original_reads.fetch_add(1, Ordering::SeqCst);
                return json_response(StatusCode::FORBIDDEN, Value::Null);
            }
            row
        } else if query_value(&request, "stream_uuid") == Some(CHANNEL) {
            json!(self.sent.lock().unwrap().iter().collect::<Vec<_>>())
        } else {
            json!([row])
        };
        json_response(StatusCode::OK, result)
    }
}

/// Сверяет всю панель редактора в подтверждённой нативной личке.
#[gpui::test]
fn native_direct_composer_orders_attachments_time_and_spoilers(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        fragment_forward: Some(Fixture::default()),
        topic: Some(TopicAcceptance::default()),
        ..Received::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1400.), px(900.)));
    until(
        ui,
        "обычная личка и канал доступны в каталоге",
        |ui| {
            server.state.ready.load(Ordering::SeqCst) != 0
                && ui.debug_bounds(CHAT_ROW).is_some()
                && ui.debug_bounds(CHANNEL_CHEVRON).is_some()
        },
    );
    click(ui, CHAT_ROW);

    // В подтверждённой нативной личке раскрытая панель показывает обе условные команды.
    until(ui, "композер подтвердил цель отправки", |ui| {
        ui.debug_bounds("composer-toolbar-toggle").is_some()
            && ui.debug_bounds("composer-attach-file").is_some()
    });
    click(ui, "composer-toolbar-toggle");
    until(
        ui,
        "доступны вложения и защищённый спойлер",
        |ui| {
            ui.debug_bounds("composer-attach-file").is_some()
                && ui
                    .debug_bounds("composer-format-encrypted-spoiler")
                    .is_some()
        },
    );
    let toolbar = ui.debug_bounds("composer-toolbar").unwrap();
    let toggle = ui.debug_bounds("composer-toolbar-toggle").unwrap();
    let attach = ui.debug_bounds("composer-attach-file").unwrap();
    let time = ui.debug_bounds("composer-insert-time").unwrap();
    let link = ui.debug_bounds("composer-format-link").unwrap();
    let spoiler = ui.debug_bounds("composer-format-spoiler").unwrap();
    let protected = ui
        .debug_bounds("composer-format-encrypted-spoiler")
        .unwrap();
    assert_eq!(attach.left() - toggle.right(), px(8.));
    assert_eq!(time.left() - attach.right(), px(17.));
    assert_eq!(link.left() - time.right(), px(17.));
    assert_eq!(protected.left() - spoiler.right(), px(8.));
    for action in [attach, time, protected] {
        assert_eq!(action.size, size(px(32.), px(32.)));
        assert_eq!(action.top(), toolbar.top());
        assert!(action.right() <= toolbar.right());
    }
}

/// Отправляет через диалог в другую тему и раскрывает фрагмент только по ключу лички.
#[gpui::test]
fn forward_dialog_posts_canonical_fragment_and_reveal_uses_origin_key(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        fragment_forward: Some(Fixture::default()),
        topic: Some(TopicAcceptance::default()),
        ..Received::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1400.), px(900.)));
    until(
        ui,
        "обычная личка и канал доступны в каталоге",
        |ui| {
            server.state.ready.load(Ordering::SeqCst) != 0
                && ui.debug_bounds(CHAT_ROW).is_some()
                && ui.debug_bounds(CHANNEL_CHEVRON).is_some()
        },
    );
    click(ui, CHAT_ROW);
    let original = "history-message-10000000-0000-4000-8000-000000000042";
    until(
        ui,
        "подтверждённый оригинал виден в истории",
        |ui| ui.debug_bounds(original).is_some(),
    );
    assert!(
        ui.debug_bounds("forward-fragments-10000000-0000-4000-8000-000000000042")
            .is_none()
    );
    open_history_context_menu(ui, original);
    click(ui, "history-forward-message");
    until(ui, "диалог пересылки открыт", |ui| {
        ui.debug_bounds("fragment-forward-dialog").is_some()
    });
    let destination = "fragment-forward-target-10000000-0000-4000-8000-000000000009";
    until(
        ui,
        "выбор доступной нативной темы",
        |ui| ui.debug_bounds(destination).is_some(),
    );
    let point = ui.debug_bounds(destination).unwrap().center();
    ui.simulate_mouse_move(point, None, Modifiers::none());
    click(ui, destination);
    until(
        ui,
        "выбрана именно тема канала",
        |ui| {
            ui.debug_bounds("fragment-forward-target-10000000-0000-4000-8000-000000000009-selected")
                .is_some()
        },
    );
    let submit = "fragment-forward-submit";
    let point = ui.debug_bounds(submit).unwrap().center();
    ui.simulate_mouse_move(point, None, Modifiers::none());
    click(ui, submit);
    let fixture = server.state.fragment_forward.as_ref().unwrap();
    until(ui, "сервер принял пересылку", |_| {
        fixture.sent.lock().unwrap().is_some()
    });
    until(
        ui,
        "диалог закрылся после записи в очередь",
        |ui| ui.read(|app| !client.read(app).right_panel.read(app).has_modal()),
    );
    ui.update(|window, app| window.draw(app).clear());
    let sent = fixture.sent.lock().unwrap().clone().unwrap();
    let body = sent["payload"]["content"].as_str().unwrap();
    let parsed = forward_spans(body).pop().unwrap().reference.unwrap();
    assert_eq!(parsed.id.to_string(), ORIGINAL);
    assert_eq!(parsed.snapshot, SNAPSHOT);
    assert!(parsed.source.unwrap().direct);
    let links = fragment_links(&parsed.snapshot);
    assert_eq!(links.len(), 2);
    assert!(links.iter().all(|link| {
        link.reference.stream_id().to_string() == STREAM
            && link.reference.key_id().to_string() == encryption::KEY
    }));
    assert!(!body.contains(SECRET));

    let chevron = ui.debug_bounds(CHANNEL_CHEVRON).unwrap().center();
    ui.simulate_mouse_move(chevron, None, Modifiers::none());
    click(ui, CHANNEL_CHEVRON);
    until(
        ui,
        "тема назначения появилась в каталоге",
        |ui| ui.debug_bounds(CHANNEL_TOPIC_ROW).is_some(),
    );
    click(ui, CHANNEL_TOPIC_ROW);
    let id = sent["uuid"].as_str().unwrap();
    // GPUI принимает только статический селектор. UUID здесь выдала реальная очередь.
    let spoiler = |position| {
        Box::leak(format!("encrypted-fragment-{id}-{position}").into_boxed_str()) as &'static str
    };
    let revealed = |position| {
        Box::leak(format!("{}-revealed", spoiler(position)).into_boxed_str()) as &'static str
    };
    let wrapper = |position| {
        Box::leak(format!("history-forward-fragment-flow-{id}-{position}").into_boxed_str())
            as &'static str
    };
    let spoilers = [spoiler(0), spoiler(1)];
    let revealed = [revealed(0), revealed(1)];
    until(
        ui,
        "оба вложенных спойлера в другой теме",
        |ui| {
            spoilers
                .iter()
                .all(|selector| ui.debug_bounds(selector).is_some())
        },
    );
    assert!(
        revealed
            .iter()
            .all(|selector| ui.debug_bounds(selector).is_none())
    );
    let first = ui.debug_bounds(wrapper(0)).unwrap();
    let second = ui.debug_bounds(wrapper(1)).unwrap();
    assert!(
        first.top() < second.top() || (first.top() == second.top() && first.left() < second.left()),
        "Два спойлера должны идти в исходном порядке: {first:?}, {second:?}"
    );
    let words = (0..20)
        .filter_map(|index| {
            let selector: &'static str = Box::leak(
                format!("history-markdown-run-history-forward-body-{id}-{ORIGINAL}-{index}")
                    .into_boxed_str(),
            );
            ui.debug_bounds(selector)
        })
        .collect::<Vec<_>>();
    let precedes = |left: gpui::Bounds<gpui::Pixels>, right: gpui::Bounds<gpui::Pixels>| {
        left.top() < right.top() || (left.top() == right.top() && left.right() <= right.left())
    };
    assert!(words.iter().any(|word| precedes(*word, first)));
    assert!(
        words
            .iter()
            .any(|word| precedes(first, *word) && precedes(*word, second))
    );
    assert!(words.iter().any(|word| precedes(second, *word)));
    let account = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let (engine, activation) = runtime.message_encryption(&account).unwrap();
    let key_id = Uuid::parse_str(encryption::KEY).unwrap();
    let origin = Uuid::parse_str(STREAM).unwrap();
    let route_before = ui.read(|app| client.read(app).shell.conversation().unwrap().clone());
    let original_header: &'static str =
        Box::leak(format!("forward-original-{id}-{ORIGINAL}").into_boxed_str());
    fixture.deny_original.store(true, Ordering::SeqCst);
    let point = ui.debug_bounds(original_header).unwrap().center();
    ui.simulate_mouse_move(point, None, Modifiers::none());
    click(ui, original_header);
    until(
        ui,
        "сервер проверил и отказал в доступе к оригиналу",
        |_| fixture.denied_original_reads.load(Ordering::SeqCst) == 1,
    );
    ui.read(|app| assert_eq!(client.read(app).shell.conversation(), Some(&route_before)));

    fixture.deny_original.store(false, Ordering::SeqCst);
    ui.update(|window, app| window.draw(app).clear());
    ui.simulate_keystrokes("enter");
    ui.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse("enter").unwrap(),
    });
    let original_id = Uuid::parse_str(ORIGINAL).unwrap();
    let original_topic = Uuid::parse_str(TOPIC).unwrap();
    until(
        ui,
        "клавиша Enter открывает точный оригинал в личке",
        |ui| {
            ui.read(|app| {
                client
                    .read(app)
                    .shell
                    .conversation()
                    .is_some_and(|address| {
                        address.route.entry
                            == crate::gpui_client::contracts::ChatEntry::Around(original_id)
                            && address.route.scope.stream_id == origin
                            && address.route.scope.topic_id == Some(original_topic)
                    })
            })
        },
    );
    click(ui, CHANNEL_TOPIC_ROW);
    engine
        .save_key(&activation, origin, key_id, &GroupKey::from_bytes([7; 32]))
        .unwrap();
    // Готовая строка остаётся свёрнутой, поэтому первый успешный клик должен раскрыть её.
    let ready: [&'static str; 2] = spoilers
        .map(|spoiler| Box::leak(format!("{spoiler}-ready").into_boxed_str()) as &'static str);
    until(
        ui,
        "оба фрагмента готовы под исходным ключом",
        |ui| {
            ready
                .iter()
                .all(|selector| ui.debug_bounds(selector).is_some())
        },
    );
    for (position, selector) in ready.into_iter().enumerate() {
        let point = ui.debug_bounds(selector).unwrap().center();
        ui.simulate_mouse_move(point, None, Modifiers::none());
        click(ui, selector);
        until(
            ui,
            "нажатие раскрывает очередной независимый фрагмент",
            |ui| {
                ui.debug_bounds(revealed[position]).is_some()
                    && revealed.iter().enumerate().all(|(index, selector)| {
                        if index <= position {
                            ui.debug_bounds(selector).is_some()
                        } else {
                            ui.debug_bounds(selector).is_none()
                        }
                    })
            },
        );
    }
    assert!(
        revealed
            .iter()
            .all(|selector| ui.debug_bounds(selector).is_some()),
        "оба фрагмента раскрыты перед отзывом ключа"
    );
    engine.remove_key(&activation, origin, key_id).unwrap();
    until(
        ui,
        "удаление ключа закрывает оба вложенных спойлера",
        |ui| {
            ui.update(|window, app| window.draw(app).clear());
            let slots_collapsed_and_unreadable = ui.read(|app| {
                client
                    .read(app)
                    .conversation
                    .as_ref()
                    .map(|conversation| {
                        conversation
                            .read(app)
                            .tested_fragment_key_state(origin, key_id, app)
                    })
                    .is_some_and(|slots| {
                        slots.len() == 2
                            && slots.iter().all(|(_, expanded, has_secret_access)| {
                                !expanded && !has_secret_access
                            })
                    })
            });
            slots_collapsed_and_unreadable
                && spoilers.iter().all(|selector| ui.debug_bounds(selector).is_some())
        },
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
