//! Проверяет навигацию формы и полный вход из рабочего окна с тестовым IAM.

use std::time::{Duration, Instant};

use gpui::{
    ClipboardItem, KeyUpEvent, Keystroke, Modifiers, TestAppContext, VisualTestContext, px, size,
};
use workspace_core::auth::AccountStore;

use super::*;
use crate::{
    gpui_client::{Options, bind_keys, client::Client, contracts::RuntimeExit},
    platform::JsonAccountStore,
};

/// Проверяет ошибку HTTPS, фокус нового шага и очистку данных при возврате к адресу.
#[gpui::test]
fn server_and_credentials_keep_their_own_fields_and_focus(cx: &mut TestAppContext) {
    let profile = tempfile::tempdir().unwrap();
    let (runtime, _events) = RuntimeClient::start_sign_in_for_test(profile.path().into());
    cx.update(|app| {
        TextInput::bind_keys(app);
        SignIn::bind_keys(app);
    });
    let (form, ui) = cx.add_window_view(|_, cx| SignIn::new(runtime.clone(), None, cx));
    ui.simulate_resize(size(px(250.), px(370.)));
    ui.run_until_parked();

    ui.update(|window, app| {
        assert!(form.read(app).server.read(app).is_focused(window));
    });
    ui.simulate_input("http://workspace.example.com");
    ui.simulate_keystrokes("enter");
    ui.run_until_parked();
    assert!(ui.debug_bounds("sign-in-error").is_some());
    ui.read(|app| {
        let view = form.read(app);
        assert!(matches!(view.step, Step::Server));
        assert_eq!(view.issue, Some(LoginError::InvalidServer));
    });

    ui.simulate_keystrokes("cmd-a");
    ui.simulate_input("https://workspace.example.com/");
    ui.simulate_keystrokes("enter");
    ui.run_until_parked();
    ui.update(|window, app| {
        let view = form.read(app);
        assert!(matches!(view.step, Step::Credentials));
        assert!(view.issue.is_none());
        assert!(view.login.read(app).is_focused(window));
        assert_eq!(
            view.server.read(app).text(),
            "https://workspace.example.com"
        );
    });

    ui.simulate_input("demo");
    let password = ui.debug_bounds("sign-in-password").expect("виден пароль");
    assert!(password.right() <= px(250.));
    ui.simulate_click(password.center(), Modifiers::none());
    ui.simulate_input("demo-password");
    ui.simulate_keystrokes("escape");
    ui.run_until_parked();
    ui.update(|window, app| {
        let view = form.read(app);
        assert!(matches!(view.step, Step::Server));
        assert!(view.server.read(app).is_focused(window));
        assert!(view.login.read(app).text().is_empty());
        assert!(view.password.read(app).text().is_empty());
    });
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean,
    );
}

/// Ждёт отрисовки после ответа рабочего потока и называет ожидаемый шаг при сбое.
fn wait_for(ui: &mut VisualTestContext, selector: &'static str) {
    wait_until(ui, selector, |ui| ui.debug_bounds(selector).is_some());
}

/// Дожидается состояния, которое обновляет рабочий поток.
fn wait_until(
    ui: &mut VisualTestContext,
    step: &str,
    mut ready: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        ui.run_until_parked();
        if ready(ui) {
            return;
        }
        assert!(Instant::now() < deadline, "Нет результата шага: {step}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Открывает ссылку из настроек выбранного сервера и оставляет форму входа доступной.
#[gpui::test]
fn registration_link_belongs_to_the_current_server(cx: &mut TestAppContext) {
    let profile = tempfile::tempdir().unwrap();
    let (runtime, _events) = RuntimeClient::start_sign_in_for_test(profile.path().into());
    cx.update(bind_keys);
    let (form, ui) =
        cx.add_window_view(|_, cx| SignIn::new(runtime.clone(), Some("https://127.0.0.1:1"), cx));
    ui.update(|_, app| {
        form.update(app, |view, cx| {
            view.submit(cx);
            let Registration::Loading { id: old, .. } = view.registration else {
                panic!("Форма должна запросить настройки первой организации");
            };
            // Подставляем запоздавший ответ прямо между выбором нового сервера и его ответом.
            view.back(cx);
            view.server.update(cx, |input, cx| {
                input.set_text("https://other.example.test", cx)
            });
            view.submit(cx);
            view.registration_received(old, Some("https://old.example.test/register".into()), cx);
            assert!(matches!(view.registration, Registration::Loading { id, .. } if id != old));
        });
    });
    wait_until(
        ui,
        "Ответ организации без ссылки",
        |ui| ui.read(|app| matches!(form.read(app).registration, Registration::Hidden)),
    );
    assert!(ui.debug_bounds("sign-in-login").is_some());
    assert!(ui.debug_bounds("sign-in-register").is_none());

    ui.simulate_keystrokes("escape");
    ui.run_until_parked();
    ui.simulate_keystrokes("cmd-a");
    ui.simulate_input("https://127.0.0.1:1");
    ui.simulate_keystrokes("enter");
    wait_for(ui, "sign-in-register");
    ui.simulate_input("demo");
    let link = ui.debug_bounds("sign-in-register").unwrap();
    ui.simulate_click(link.center(), Modifiers::none());
    ui.read(|app| {
        let view = form.read(app);
        assert!(matches!(view.step, Step::Credentials));
        assert_eq!(view.login.read(app).text(), "demo");
    });
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.test/register")
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Входит через OTP с повтором и проверяет, что оболочка открыла выбранный проект.
#[gpui::test]
fn wrong_otp_can_be_retried_before_entering_selected_project(cx: &mut TestAppContext) {
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) = RuntimeClient::start_sign_in_for_test(profile.path().into());
    cx.update(bind_keys);
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.simulate_resize(size(px(700.), px(600.)));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });

    wait_for(ui, "sign-in-server");
    let form = ui.debug_bounds("sign-in-form").unwrap();
    assert_eq!(form.size.width, px(448.));
    assert!((form.center().y - px(300.)).abs() <= px(1.), "{form:?}");
    ui.simulate_input("https://127.0.0.1:1");
    ui.simulate_keystrokes("enter");
    wait_for(ui, "sign-in-login");
    ui.simulate_input("login-test-user");
    ui.simulate_keystrokes("tab");
    ui.simulate_input("test-password");
    ui.simulate_keystrokes("enter");

    wait_for(ui, "sign-in-otp");
    ui.simulate_input("111111");
    ui.simulate_keystrokes("enter");
    wait_for(ui, "sign-in-error");
    assert!(ui.debug_bounds("sign-in-otp").is_some());

    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string("123 456".into())));
    ui.simulate_keystrokes("cmd-v enter");
    wait_for(ui, "sign-in-project-1");
    for _ in 0..2 {
        ui.simulate_keystrokes("tab");
        ui.simulate_event(KeyUpEvent {
            keystroke: Keystroke::parse("enter").unwrap(),
        });
    }

    wait_for(ui, "workspace-sidebar-panel");
    assert!(ui.debug_bounds("workspace-account-0").is_some());
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean,
    );
    let saved = JsonAccountStore::open(profile.path())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].context.project_id, "another-project");
}
