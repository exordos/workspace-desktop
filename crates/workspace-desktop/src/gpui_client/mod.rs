//! Собирает окно GPUI, запускает обработку его событий и подключает системные адаптеры.
//! Контракты отделяют фоновую работу от состояния окна.

mod activity;
mod avatars;
mod client;
pub mod composer;
pub mod contracts;
pub mod conversation;
mod i18n;
mod message_preview;
mod notification_audio;
pub mod platform_window;
mod preferences;
mod right_panel;
pub mod runtime;
pub mod shell;
pub mod sidebar;
mod sign_in;
mod test_profile;
pub mod ui;

use std::{cell::Cell, collections::HashMap, io, path::PathBuf, rc::Rc};

use gpui::{App, AppContext, Application, KeyBinding, Window, actions};
use workspace_core::context::AccountId;

use crate::platform::state_directory;
use client::Client;
use composer::Editor;
use contracts::ShellAction;
use platform_window::WindowPlatform;
use runtime::{RuntimeClient, RuntimeEvents, StartupError};
use sidebar::Sidebar;
use sign_in::SignIn;
use ui::WorkspaceAssets;

actions!(
    workspace_focus,
    [NextControl, PreviousControl, NextArea, PreviousArea]
);

/// Подписи аккаунтов и выбор первого входа, переданные при запуске.
#[derive(Default)]
pub struct Options {
    pub active: Option<AccountId>, // Предпочитаемый сохранённый вход; None сохраняет обычный выбор.
    pub labels: HashMap<AccountId, String>, // Временные подписи для окна и системных баннеров.
}

/// Открывает обычное окно с существующим профилем Workspace.
pub fn run(options: Options) -> io::Result<()> {
    let profile = state_directory().map_err(|_| io::Error::other(StartupError::Profile))?;
    let (runtime, events) = RuntimeClient::start_in(profile.clone())
        .map_err(|_| io::Error::other(StartupError::Worker))?;
    run_in(options, profile, runtime, events)
}

/// Входит тестовым аккаунтом в новый профиль с токенами в памяти и открывает окно.
pub fn run_test_profile() -> io::Result<()> {
    let login = test_profile::TestLogin::from_environment()?;
    let temporary = tempfile::Builder::new()
        .prefix("workspace-agent-")
        .tempdir()?;
    let profile = temporary.path().to_path_buf();
    let (runtime, events) = RuntimeClient::start_test_profile(profile.clone(), login)?;
    // Ошибки стенда видны в терминале. Окно открывается после успешного входа.
    futures::executor::block_on(runtime.ready()).map_err(io::Error::other)?;
    run_in(Options::default(), profile, runtime, events)
}

/// Подключает один и тот же клиент к уже выбранному профилю и фоновой работе.
fn run_in(
    options: Options,
    profile: PathBuf,
    runtime: RuntimeClient,
    events: RuntimeEvents,
) -> io::Result<()> {
    platform_window::macos::disable_state_restoration();
    let failed = Rc::new(Cell::new(false));
    let startup_failure = Rc::clone(&failed);
    let application = Application::new().with_assets(WorkspaceAssets);
    application.on_reopen(WindowPlatform::show_window);
    application.run(move |app| {
        bind_keys(app);
        preferences::Preferences::install(&profile, app);
        bind_native_theme(app);
        composer::install_toolbar_preferences(&profile, app);
        let window_options = WindowPlatform::window_options(&profile, app);
        let window = app.open_window(window_options, |window, app| {
            refresh_on_appearance(window);
            app.new(|cx| Client::new(options, runtime.clone(), cx))
        });
        let Ok(window) = window else {
            startup_failure.set(true);
            return quit_after_window_failure(app, runtime);
        };
        let Ok(client) = window.update(app, |_, _, cx| cx.entity()) else {
            startup_failure.set(true);
            return quit_after_window_failure(app, runtime);
        };

        let target = client.downgrade();
        let _ = window.update(app, |_, window, cx| {
            cx.observe_window_activation(window, |_, window, cx| {
                if window.is_window_active() {
                    WindowPlatform::refresh_notification_permission(cx);
                }
            })
            .detach();
            cx.observe_window_activation(window, |client, window, cx| {
                client.check_idle(window, cx);
            })
            .detach();
        });
        Client::install_settings_input(&client, app);
        app.spawn(async move |cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
                if window
                    .update(cx, |client, window, cx| client.check_idle(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        if WindowPlatform::install(
            app,
            window.into(),
            &profile,
            runtime.clone(),
            move |click, _, app| {
                let _ = target.update(app, |client, cx| {
                    client.handle_action(ShellAction::NotificationClicked(click), cx);
                });
            },
        )
        .is_err()
        {
            startup_failure.set(true);
            return quit_after_window_failure(app, runtime);
        }

        // Передаём первый снимок той же подписке, которая затем обновляет живое окно.
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
        app.activate(true);
    });
    if failed.get() {
        Err(io::Error::other("Не удалось открыть окно Workspace"))
    } else {
        Ok(())
    }
}

/// Перерисовывает всё дерево окна после смены системной темы.
fn refresh_on_appearance(window: &Window) {
    // GPUI 0.2.2 обновляет Window::appearance, но дочерние виды ждут перерисовки.
    window
        .observe_window_appearance(|window, _| window.refresh())
        .detach();
}

/// Синхронизирует нативное оформление на старте и при смене выбора или активного входа.
fn bind_native_theme(app: &mut App) {
    use preferences::Preferences;

    let mut previous = Preferences::read(app).appearance.theme;
    platform_window::macos::set_application_theme(previous);
    let mut pending = None;
    app.observe_global::<Preferences>(move |app| {
        let current = Preferences::read(app).appearance.theme;
        if current != previous {
            previous = current;
            // AppKit может синхронно вызвать обновление окна. К этому моменту
            // GPUI должен закончить запись настроек и отпустить контекст приложения.
            let apply = app.foreground_executor().spawn(async move {
                platform_window::macos::set_application_theme(current);
            });
            drop(pending.replace(apply));
        }
    })
    .detach();
}

/// Регистрирует системный ввод и общий порядок фокуса для всех частей окна.
fn bind_keys(app: &mut App) {
    conversation::Conversation::bind_attachment_keys(app);
    ui::TextInput::bind_keys(app);
    Sidebar::bind_keys(app);
    Editor::bind_keys(app);
    SignIn::bind_keys(app);
    right_panel::RightPanel::bind_keys(app);
    app.bind_keys([
        KeyBinding::new("tab", NextControl, None),
        KeyBinding::new("shift-tab", PreviousControl, None),
        KeyBinding::new("f6", NextArea, None),
        KeyBinding::new("shift-f6", PreviousArea, None),
        KeyBinding::new(
            "escape",
            preferences::shortcuts::CancelRecording,
            Some("WorkspaceHotkeyRecording"),
        ),
    ]);
    ui::TopBar::bind_keys(app);
}

/// Включает общий обход интерактивных элементов на корневом контейнере.
fn next_control(_: &NextControl, window: &mut Window, _: &mut App) {
    window.focus_next();
}

/// Возвращает фокус предыдущему элементу того же окна.
fn previous_control(_: &PreviousControl, window: &mut Window, _: &mut App) {
    window.focus_prev();
}

/// При ошибке окна отключает фоновую работу до выхода, не печатая детали ОС или профиля.
fn quit_after_window_failure(app: &mut App, runtime: RuntimeClient) {
    app.spawn(async move |cx| {
        runtime.shutdown().await;
        let _ = cx.update(|app| app.quit());
    })
    .detach();
}
