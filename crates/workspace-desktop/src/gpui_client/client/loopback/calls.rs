//! Проверяет запуск защищённого звонка через обычное окно и локальное подтверждение очереди.

use workspace_core::messenger::encryption::GroupKey;

use super::{
    encryption::{HeldInvitation, KEY, ProtectedWire},
    *,
};

const MEET_URL: &str = "https://meet.workspace.test";
const HEAD_CALL: &str = "protected-call-heading";
const DETAILS_CALL: &str = "protected-call-details";
const BOUNDARY: &str = "protected-chat-boundary";

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
        "сведения показывают звонок и границу защиты",
        |ui| ui.debug_bounds(DETAILS_CALL).is_some() && ui.debug_bounds(BOUNDARY).is_some(),
    );
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
