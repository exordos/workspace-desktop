//! Проверяет пересылку, отдельный спойлер и отзыв ключа в настоящем окне на локальном HTTPS.

use workspace_core::messenger::{
    encryption::{GroupKey, fragments::fragment_links},
    forward::forward_spans,
};

use super::*;

const ORIGINAL: &str = "10000000-0000-4000-8000-000000000042";
const SECRET: &str = "PRIVATE-FORWARD-FIXTURE";
// Синтетический конверт для ключа [7;32], вычислен отдельно через Node/OpenSSL.
const SNAPSHOT: &str = "до [секрет](urn:secret:v1:10000000-0000-4000-8000-000000000005:10000000-0000-4000-8000-000000000080:FRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzS7qvkRcF5D89oetxPZ5cK3tfeCp8PQ-abZkA4ZwxWKxd0A0sAgR-g) после";

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
    assert_eq!(
        fragment_links(&parsed.snapshot)[0]
            .reference
            .stream_id()
            .to_string(),
        STREAM
    );
    assert!(!body.contains(SECRET));

    click(ui, CHANNEL_CHEVRON);
    ui.run_until_parked();
    click(ui, CHANNEL_TOPIC_ROW);
    let id = sent["uuid"].as_str().unwrap();
    // GPUI принимает только статический селектор. UUID здесь выдала реальная очередь.
    let spoiler: &'static str = Box::leak(format!("encrypted-fragment-{id}-0").into_boxed_str());
    let revealed: &'static str = Box::leak(format!("{spoiler}-revealed").into_boxed_str());
    until(
        ui,
        "вложенный спойлер в другой теме",
        |ui| ui.debug_bounds(spoiler).is_some(),
    );
    assert!(ui.debug_bounds(revealed).is_none());
    let account = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let (engine, activation) = runtime.message_encryption(&account).unwrap();
    let key_id = Uuid::parse_str(encryption::KEY).unwrap();
    let origin = Uuid::parse_str(STREAM).unwrap();
    engine
        .save_key(&activation, origin, key_id, &GroupKey::from_bytes([7; 32]))
        .unwrap();
    // Готовая строка остаётся свёрнутой, поэтому первый успешный клик должен раскрыть её.
    let ready: &'static str = Box::leak(format!("{spoiler}-ready").into_boxed_str());
    until(
        ui,
        "фрагмент готов под исходным ключом",
        |ui| ui.debug_bounds(ready).is_some(),
    );
    let point = ui.debug_bounds(ready).unwrap().center();
    ui.simulate_mouse_move(point, None, Modifiers::none());
    click(ui, ready);
    until(
        ui,
        "кнопка раскрывает фрагмент под исходным ключом",
        |ui| ui.debug_bounds(revealed).is_some(),
    );
    engine.remove_key(&activation, origin, key_id).unwrap();
    until(
        ui,
        "удаление ключа закрывает вложенный спойлер",
        |ui| {
            ui.update(|window, app| window.draw(app).clear());
            ui.debug_bounds(spoiler).is_some() && ui.debug_bounds(revealed).is_none()
        },
    );
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
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
