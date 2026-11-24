//! Проверяет запуск обычного и защищённого звонков через окно и подтверждение очереди.

use workspace_core::messenger::encryption::GroupKey;

use super::{
    encryption::{HeldInvitation, KEY, ProtectedWire},
    *,
};

const MEET_URL: &str = "https://meet.workspace.test";
const ORDINARY_HEADING: &str = "conversation-call-heading";
const ORDINARY_DETAILS: &str = "details-peer-call";
const HEAD_CALL: &str = "protected-call-heading";
const DETAILS_CALL: &str = "protected-call-details";

/// Удерживает первую квитанцию и запоминает отправленные ссылки на конференции.
#[derive(Default)]
pub(super) struct PlainCallFixture {
    held: Mutex<Option<oneshot::Receiver<()>>>, // Передаёт квитанцию после разрешения теста.
    urls: Mutex<Vec<String>>,                   // Ссылки в принятых телах запроса по порядку.
    change_reply: AtomicBool,                   // Возвращает другое тело в подтверждённой строке.
}

impl PlainCallFixture {
    /// Возвращает подтверждение той же Markdown-строки для точной темы лички.
    pub(super) async fn accept(&self, post: Value) -> Response<Full<Bytes>> {
        assert_eq!(post["stream_uuid"], STREAM);
        assert_eq!(post["topic_uuid"], TOPIC);
        assert_eq!(post["payload"]["kind"], "markdown");
        let url = post["payload"]["content"].as_str().unwrap();
        assert!(url.starts_with(&format!("{MEET_URL}/workspace-")));
        assert!(url.contains(&format!("-{STREAM}-{TOPIC}-")));
        let canonical = post["uuid"].as_str().unwrap();
        let id = Uuid::new_v5(&Uuid::parse_str(TOPIC).unwrap(), canonical.as_bytes());
        let position = {
            let mut urls = self.urls.lock().unwrap();
            let position = urls.len();
            urls.push(url.to_owned());
            position
        };
        let held = self.held.lock().unwrap().take();
        if let Some(release) = held {
            let _ = release.await;
        }
        let mut row = direct_message(id, TOPIC, position);
        row["payload"]["content"] = if self.change_reply.load(Ordering::SeqCst) {
            "Другое сообщение".into()
        } else {
            url.into()
        };
        json_response(StatusCode::CREATED, row)
    }
}

/// Обычный звонок из профиля ждёт квитанцию, и шапка может начать следующий звонок.
#[gpui::test]
fn ordinary_call_from_profile_and_heading_follows_delivery(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let (release, gate) = oneshot::channel();
    let server = Server::serve(Received {
        plain_call: Some(PlainCallFixture {
            held: Mutex::new(Some(gate)),
            ..PlainCallFixture::default()
        }),
        details: Some(DetailsFixture::default()),
        meet_url: Some(MEET_URL),
        ..Received::default()
    });
    let wire = server.state.plain_call.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1150.), px(900.)));
    until(
        ui,
        "обычная личка есть в каталоге",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "у лички настроен сервер Jitsi",
        |ui| {
            ui.debug_bounds(ORDINARY_HEADING).is_some()
                && ui.debug_bounds(DETAILS_TRIGGER).is_some()
                && ui.debug_bounds("workspace-composer-editor").is_some()
        },
    );
    click(ui, DETAILS_TRIGGER);
    until(
        ui,
        "профиль показывает действие звонка",
        |ui| ui.debug_bounds(ORDINARY_DETAILS).is_some(),
    );
    click(ui, ORDINARY_DETAILS);
    until(
        ui,
        "сервер ещё не подтвердил обычное приглашение",
        |_| wire.urls.lock().unwrap().len() == 1,
    );
    assert!(ui.opened_url().is_none());
    click(ui, ORDINARY_HEADING);
    release.send(()).unwrap();
    until(
        ui,
        "квитанция открыла ту же комнату",
        |ui| ui.opened_url().as_deref() == wire.urls.lock().unwrap().first().map(String::as_str),
    );
    assert_eq!(wire.urls.lock().unwrap().len(), 1);
    click(ui, ORDINARY_HEADING);
    until(
        ui,
        "шапка отправила и открыла новое приглашение",
        |ui| ui.opened_url().as_deref() == wire.urls.lock().unwrap().get(1).map(String::as_str),
    );
    wire.change_reply.store(true, Ordering::SeqCst);
    click(ui, ORDINARY_HEADING);
    until(
        ui,
        "другое тело ответа показывает ошибку",
        |ui| ui.debug_bounds("conversation-call-error").is_some(),
    );
    assert_eq!(
        ui.opened_url().as_deref(),
        wire.urls.lock().unwrap().get(1).map(String::as_str)
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Системный адаптер получает точное приглашение только после доставки, и карточка исчезает без ключа.
#[gpui::test]
fn protected_call_waits_for_confirmation_and_replaces_join_on_key_removal(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        direct_encrypted: AtomicBool::new(true),
        protected_wire: Some(ProtectedWire::default()),
        details: Some(DetailsFixture::default()),
        meet_url: Some(MEET_URL),
        ..Received::default()
    });
    let clock = cx.background_executor.clone();
    let wire = server.state.protected_wire.as_ref().unwrap();
    let seen = Arc::new(AtomicBool::new(false));
    let (release, gate) = oneshot::channel();
    wire.hold_invitation(HeldInvitation {
        seen: seen.clone(),
        release: gate,
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
    ui.simulate_resize(size(px(1150.), px(900.)));
    until(
        ui,
        "защищённый чат есть в каталоге",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    let account = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let (engine, activation) = runtime.message_encryption(&account).unwrap();
    let (stream, key) = (STREAM.parse().unwrap(), KEY.parse().unwrap());
    engine
        .save_key(&activation, stream, key, &GroupKey::from_bytes([7; 32]))
        .unwrap();
    server
        .state
        .meet_settings_failure
        .store(true, Ordering::SeqCst);
    click(ui, CHAT_ROW);
    until(
        ui,
        "сервер вернул временный отказ чтения настроек",
        |_| server.state.meet_settings_seen.load(Ordering::SeqCst),
    );
    assert!(ui.debug_bounds(HEAD_CALL).is_none());
    until(
        ui,
        "шапка показывает звонок после повтора чтения настроек",
        |ui| {
            clock.advance_clock(Duration::from_millis(100));
            ui.debug_bounds(HEAD_CALL).is_some()
        },
    );
    click(ui, DETAILS_TRIGGER);
    until(
        ui,
        "сведения о защищённом чате показывают звонок",
        |ui| {
            ui.debug_bounds(DETAILS_CALL).is_some()
                && ui.debug_bounds("conversation-encrypted-info").is_some()
        },
    );
    assert!(ui.debug_bounds(ORDINARY_DETAILS).is_none());
    click(ui, DETAILS_CALL);
    until(
        ui,
        "сервер принял конверт и ещё не ответил",
        |_| seen.load(Ordering::SeqCst),
    );
    assert!(ui.opened_url().is_none());
    release.send(()).unwrap();
    until(
        ui,
        "ответ открыл комнату через тестовый системный адаптер",
        |ui| ui.opened_url().is_some(),
    );
    let (message_id, invitation) = wire.posted("e2ee", KEY).unwrap();
    assert!(!invitation.to_string().contains(MEET_URL));
    let encoded = invitation["payload"]["content"].as_str().unwrap();
    let grant = engine
        .accept(
            engine
                .prepare(&activation, stream, key)
                .unwrap()
                .decrypt(encoded),
        )
        .unwrap();
    let url = grant.with_body(str::to_owned).unwrap();
    assert!(url.starts_with(&format!("{MEET_URL}/workspace-")));
    assert_eq!(ui.opened_url().as_deref(), Some(url.as_str()));
    let join: &'static str = format!("protected-call-join-{message_id}").leak();
    until(
        ui,
        "подтверждённое сообщение показывает кнопку входа",
        |ui| ui.debug_bounds(join).is_some(),
    );
    engine.remove_key(&activation, stream, key).unwrap();
    until(
        ui,
        "удаление ключа закрывает действие входа из истории",
        |ui| ui.debug_bounds(join).is_none(),
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
