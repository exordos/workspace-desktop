//! Проверяет добавление быстрой реакции и снятие через счётчик в окне с локальным HTTPS.

use super::*;

const MESSAGE: &str = "10000000-0000-4000-8000-000000000075";
const REACTION: &str = "10000000-0000-4000-8000-000000000076";
const REACTIONS: &str = "/api/workspace/v1/messenger/message_reactions/";
const BUBBLE: &str = "history-quick-bubble-10000000-0000-4000-8000-000000000075";
const FIRST_EMOJI: &str = "history-quick-reaction-10000000-0000-4000-8000-000000000075-0";
const CHIP: &str = "history-reaction-chip-10000000-0000-4000-8000-000000000075-heart";

/// Серверная реакция в одном тестовом разговоре.
#[derive(Default)]
pub(super) struct Fixture {
    added: AtomicUsize,   // Сколько команд с быстрым эмодзи дошли через HTTPS.
    removed: AtomicUsize, // Сколько снятий той же реакции сервер подтвердил.
}

impl Fixture {
    /// Ограничивает ответы одной строкой истории и её реакцией.
    pub(super) fn handles(&self, request: &Request<Incoming>) -> bool {
        let path = request.uri().path();
        path == MESSAGES || path == format!("{MESSAGES}{MESSAGE}") || path.starts_with(REACTIONS)
    }

    /// Отдаёт текущую серверную проекцию и проверяет точный формат команд.
    pub(super) async fn response(
        &self,
        request: Request<Incoming>,
        user: &str,
        peer: &str,
    ) -> Response<Full<Bytes>> {
        let selected = self.added.load(Ordering::SeqCst) > self.removed.load(Ordering::SeqCst);
        let reaction = json!({
            "uuid":REACTION, "message_uuid":MESSAGE, "project_id":PROJECT,
            "user_uuid":user, "emoji_name":"heart",
        });
        if request.uri().path() == REACTIONS {
            match *request.method() {
                Method::GET => {
                    assert_eq!(query_value(&request, "message_uuid"), Some(MESSAGE));
                    assert_eq!(query_value(&request, "user_uuid"), Some(user));
                    let rows = if selected { vec![reaction] } else { Vec::new() };
                    return json_response(StatusCode::OK, json!(rows));
                }
                Method::POST => {
                    assert!(!selected);
                    let bytes = request.into_body().collect().await.unwrap().to_bytes();
                    let body: Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(body, json!({"message_uuid":MESSAGE,"emoji_name":"❤️"}));
                    self.added.fetch_add(1, Ordering::SeqCst);
                    return json_response(StatusCode::CREATED, reaction);
                }
                _ => panic!("Неожиданный метод коллекции реакций"),
            }
        }
        if request.uri().path() == format!("{REACTIONS}{REACTION}") {
            assert_eq!(request.method(), Method::DELETE);
            assert!(selected);
            self.removed.fetch_add(1, Ordering::SeqCst);
            return Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Full::default())
                .unwrap();
        }
        assert_eq!(request.method(), Method::GET);
        let row = json!({
            "uuid":MESSAGE, "stream_uuid":STREAM, "topic_uuid":TOPIC, "project_id":PROJECT,
            "user_uuid":user, "author_uuid":peer, "created_at":"2026-11-17T10:00:00Z",
            "payload":{"kind":"markdown", "content":"Сообщение для проверки быстрых реакций"},
            "reactions": if selected { json!({"heart":1}) } else { json!({}) },
            "reaction_users": if selected { json!({"heart":[user]}) } else { json!({}) },
        });
        let data = if request.uri().path() == MESSAGES {
            json!([row])
        } else {
            row
        };
        json_response(StatusCode::OK, data)
    }
}

/// Первый эмодзи появляется при наведении, проходит через HTTPS и остаётся на строке до снятия.
#[gpui::test]
fn quick_reaction_toggle_round_trips_through_history(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        reactions: Some(Fixture::default()),
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
        "личка появилась в каталоге",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "отображено подтверждённое сообщение",
        |ui| ui.debug_bounds(BUBBLE).is_some(),
    );
    assert!(ui.debug_bounds(FIRST_EMOJI).is_none());
    let before = ui.debug_bounds(BUBBLE).unwrap();
    ui.simulate_mouse_move(before.center(), None, Modifiers::none());
    until(
        ui,
        "появилась быстрая реакция",
        |ui| ui.debug_bounds(FIRST_EMOJI).is_some(),
    );
    click(ui, FIRST_EMOJI);
    let fixture = server.state.reactions.as_ref().unwrap();
    let message = MESSAGE.parse::<Uuid>().unwrap();
    let confirmed = |ui: &VisualTestContext, selected| {
        ui.read(|app| {
            client.read(app).conversation.as_ref().is_some_and(|view| {
                view.read(app)
                    .tested_reaction_confirmed(message, "heart", selected)
            })
        })
    };
    until(
        ui,
        "подтверждённая реакция с серверным именем видна в истории",
        |ui| {
            fixture.added.load(Ordering::SeqCst) == 1
                && ui.debug_bounds(CHIP).is_some()
                && ui
                    .debug_bounds(BUBBLE)
                    .is_some_and(|bounds| bounds.size.height > before.size.height)
                && confirmed(ui, true)
        },
    );
    let chip = ui.debug_bounds(CHIP).unwrap().center();
    ui.simulate_mouse_move(chip, None, Modifiers::none());
    ui.run_until_parked();
    click(ui, CHIP);
    until(
        ui,
        "сервер получил DELETE по UUID реакции",
        |_| fixture.removed.load(Ordering::SeqCst) == 1,
    );
    until(
        ui,
        "снятие подтверждено в принятой истории",
        |ui| confirmed(ui, false),
    );
    until(
        ui,
        "счётчик исчез из сообщения",
        |ui| {
            ui.debug_bounds(BUBBLE)
                .is_some_and(|bounds| bounds.size.height == before.size.height)
        },
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
