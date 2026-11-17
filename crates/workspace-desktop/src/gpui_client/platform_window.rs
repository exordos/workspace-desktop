//! Связывает главное окно GPUI с треем, уведомлениями и остановкой фоновой работы.
//! Здесь находятся настройки окна и обработчики системных действий.

pub(super) mod macos;
mod window_size;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::{StreamExt, channel::mpsc};
use gpui::{
    AnyWindowHandle, App, AppContext, Entity, Global, KeyBinding, Menu, MenuItem, Subscription,
    Task, TitlebarOptions, Window, WindowBounds, WindowOptions, actions,
};
use workspace_core::context::AccountId;

use super::{contracts::RuntimeExit, runtime::RuntimeClient};
use crate::platform::{
    notifications::{NotificationClick, NotificationError, Notifications},
    tray::{Command, Tray},
    wake::WakeObserver,
};
use window_size::{SizeObserver, SizeWriter};

actions!(workspace_window, [Quit]);

static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

/// Держит адаптеры одного рабочего окна до штатного выхода приложения.
pub struct WindowPlatform {
    window: AnyWindowHandle, // Окно, которое поднимается при системном действии.
    native: Option<macos::NativeWindow>, // Ссылка macOS для восстановления из Dock.
    runtime: RuntimeClient,  // Фоновая работа, которую нужно остановить до выхода.
    notifications: Notifications, // Центр уведомлений и связь баннера с исходной целью.
    notification_error: Option<NotificationError>, // Текущая причина недоступности баннеров.
    quitting: bool,          // Идёт завершение, повторные действия отбрасываются.
    _tray: Option<Tray>,     // Меню Workspace живёт вместе с оконным циклом.
    _wake: WakeObserver,     // Передаёт пробуждение macOS фоновой работе.
    _bounds: Entity<SizeObserver>, // Подписка на размеры главного окна.
    size_writer: Option<SizeWriter>, // Ждёт последнюю запись при выходе.
    _closed: Subscription,   // Обычное закрытие окна тоже завершает фон.
    _menu_task: Task<()>,    // Получает действия из меню трея.
    _click_task: Option<Task<()>>, // Передаёт нажатие по баннеру в оболочку.
}

impl Global for WindowPlatform {}

impl WindowPlatform {
    /// Восстанавливает размер области содержимого из существующего профиля.
    pub fn window_options(profile: &Path, app: &App) -> WindowOptions {
        WindowOptions {
            window_bounds: Some(WindowBounds::centered(window_size::load(profile), app)),
            titlebar: Some(TitlebarOptions {
                title: Some("Workspace Desktop".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Подключает адаптеры на главном потоке после открытия окна.
    /// Обрабатывает клик, запрашивая подъём окна перед передачей цели оболочке.
    pub fn install(
        app: &mut App,
        window: AnyWindowHandle,
        profile: &Path,
        runtime: RuntimeClient,
        mut on_click: impl FnMut(NotificationClick, &mut Window, &mut App) + 'static,
    ) -> gpui::Result<()> {
        if app.try_global::<Self>().is_some() {
            return Err(std::io::Error::other("Адаптеры окна уже подключены").into());
        }
        let main_thread = macos::main_thread()
            .ok_or_else(|| std::io::Error::other("Окну Workspace нужен главный поток"))?;
        let native = window.update(app, |_, window, _| {
            macos::NativeWindow::find(&window.window_title(), main_thread)
        })?;
        log::debug!(
            "event=workspace_native_window attached={}",
            native.is_some()
        );
        let (commands, mut menu) = mpsc::unbounded();
        let tray = if std::env::var_os("WORKSPACE_DESKTOP_DISABLE_TRAY").is_none() {
            Tray::install(main_thread, commands)
        } else {
            None
        };
        let has_tray = tray.is_some();

        let size_writer = SizeWriter::new(profile, app);
        let sizes = size_writer.updates();
        let bounds = window.update(app, |_, window, app| {
            let close_native = native.clone();
            let close_sizes = sizes.clone();
            window.on_window_should_close(app, move |window, app| {
                // Здесь окно ещё доступно, даже если скоро уйдёт из GPUI без трея.
                window_size::record(window, close_native.as_ref(), &close_sizes);
                if has_tray {
                    let op_id = operation();
                    log::debug!("op_id={op_id} event=workspace_window_hide");
                    app.hide();
                }
                !has_tray
            });
            app.new(|cx| SizeObserver::new(window, native.clone(), sizes, cx))
        })?;

        let closed = app.on_window_closed(move |app| {
            if !app.windows().contains(&window) {
                Self::request_quit(app);
            }
        });
        let menu_task = app.spawn(async move |app| {
            while let Some(command) = menu.next().await {
                if app
                    .update(|app| match command {
                        Command::Show => Self::show_window(app),
                        Command::Quit => Self::request_quit(app),
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let wake = WakeObserver::subscribe(main_thread, runtime.wake_sender());
        let notifications = Notifications::new();
        let click_task = notifications.take_clicks().map(|mut clicks| {
            app.spawn(async move |app| {
                while let Some(click) = clicks.next().await {
                    if app
                        .update(|app| {
                            deliver_click(app, click, &mut on_click);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });
        let prepare = notifications.prepare();

        app.set_global(Self {
            window,
            native,
            runtime,
            notifications,
            notification_error: None,
            quitting: false,
            _tray: tray,
            _wake: wake,
            _bounds: bounds,
            size_writer: Some(size_writer),
            _closed: closed,
            _menu_task: menu_task,
            _click_task: click_task,
        });
        app.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        app.on_action(|_: &Quit, app| Self::request_quit(app));
        app.set_menus(vec![Menu {
            name: "Workspace Desktop".into(),
            items: vec![MenuItem::action("Выход", Quit)],
        }]);

        let op_id = operation();
        app.spawn(async move |app| {
            let result = prepare.await;
            let _ = app.update(|app| Self::notification_finished(app, op_id, result));
        })
        .detach();
        Ok(())
    }

    /// Поднимает существующее окно и оставляет фокус его текущему экрану.
    pub fn show_window(app: &mut App) {
        let Some(platform) = app
            .try_global::<Self>()
            .filter(|platform| !platform.quitting)
        else {
            return;
        };
        raise_window(app, platform.window, platform.native.clone(), operation());
    }

    /// Просит macOS показать баннер с подписью аккаунта и исходной целью перехода.
    pub fn show_notification(app: &mut App, click: NotificationClick, label: Option<&str>) {
        let Some(platform) = app
            .try_global::<Self>()
            .filter(|platform| !platform.quitting)
        else {
            return;
        };
        let target = &click.target;
        let AccountId(account) = &target.context.account_id;
        let id = format!(
            "{}/{}/{}",
            account, target.context.project_id, target.message_id
        );
        let label = label.unwrap_or_else(|| account.get(..8).unwrap_or(account));
        let body = format!("Аккаунт {label}");
        let delivery =
            platform
                .notifications
                .show(&id, "Новое сообщение", &body, target, click.generation);
        let op_id = operation();
        log::debug!("op_id={op_id} event=workspace_notification_requested");
        app.spawn(async move |app| {
            let result = delivery.await;
            let _ = app.update(|app| Self::notification_finished(app, op_id, result));
        })
        .detach();
    }

    /// Возвращает причину недоступности баннеров для сообщения в оболочке.
    pub fn notification_error(app: &App) -> Option<&NotificationError> {
        app.try_global::<Self>()?.notification_error.as_ref()
    }

    /// Останавливает Bootstrap и дожидается записи размера до завершения GPUI.
    pub fn request_quit(app: &mut App) {
        if !app
            .try_global::<Self>()
            .is_some_and(|platform| !platform.quitting)
        {
            return;
        }
        let platform = app.global_mut::<Self>();
        platform.quitting = true;
        let runtime = platform.runtime.clone();
        let window = platform.window;
        let native = platform.native.clone();
        let size_writer = platform.size_writer.take();
        if let Some(writer) = &size_writer {
            let _ = window.update(app, |_, window, _| writer.record(window, native.as_ref()));
        }
        let op_id = operation();
        log::debug!("op_id={op_id} event=workspace_shutdown_requested");

        // on_app_quit в GPUI ждёт только 100 мс. Пока Bootstrap работает, цикл окна жив.
        app.spawn(async move |app| {
            let result = runtime.shutdown().await;
            if let Some(writer) = size_writer {
                writer.finish().await;
            }
            if result == RuntimeExit::Clean {
                log::info!("op_id={op_id} event=workspace_shutdown_finished result=clean");
            } else {
                log::error!("op_id={op_id} event=workspace_shutdown_finished result=failed");
            }
            let _ = app.update(|app| app.quit());
        })
        .detach();
    }

    /// Запоминает результат запроса к macOS для следующей отрисовки оболочки.
    fn notification_finished(app: &mut App, op_id: u64, result: Result<(), NotificationError>) {
        log::debug!(
            "op_id={op_id} event=workspace_notification_finished result={}",
            if result.is_ok() { "ok" } else { "unavailable" },
        );
        let platform = app.global_mut::<Self>();
        let changed = platform.notification_error.as_ref() != result.as_ref().err();
        platform.notification_error = result.err();
        if changed {
            app.refresh_windows();
        }
    }
}

/// Возвращает окно из Dock и активирует его текущий экран.
fn raise_window(
    app: &mut App,
    window: AnyWindowHandle,
    native: Option<macos::NativeWindow>,
    op_id: u64,
) {
    if let Some(native) = native {
        // Восстановление вызывает оконные события. Делаем это после выхода из App::update.
        app.foreground_executor()
            .spawn(async move { native.restore_from_dock() })
            .detach();
    }
    app.activate(true);
    let result = window.update(app, |_, window, _| window.activate_window());
    log::debug!(
        "op_id={op_id} event=workspace_window_show result={}",
        result.is_ok()
    );
}

/// Поднимает окно перед передачей цели в оболочку. Закрывающийся клиент пропускает клики.
fn deliver_click(
    app: &mut App,
    click: NotificationClick,
    on_click: &mut impl FnMut(NotificationClick, &mut Window, &mut App),
) {
    let Some(platform) = app
        .try_global::<WindowPlatform>()
        .filter(|platform| !platform.quitting)
    else {
        return;
    };
    apply_click(
        app,
        platform.window,
        platform.native.clone(),
        click,
        on_click,
    );
}

/// Доставляет принятую цель в тот же экземпляр окна после запроса активации.
fn apply_click(
    app: &mut App,
    window: AnyWindowHandle,
    native: Option<macos::NativeWindow>,
    click: NotificationClick,
    on_click: &mut impl FnMut(NotificationClick, &mut Window, &mut App),
) {
    let op_id = operation();
    log::debug!("op_id={op_id} event=workspace_notification_clicked");
    raise_window(app, window, native, op_id);
    let result = window.update(app, |_, window, app| on_click(click, window, app));
    log::debug!(
        "op_id={op_id} event=workspace_notification_delivered result={}",
        result.is_ok()
    );
}

/// Выдаёт номер для связанных диагностических записей без данных аккаунта.
fn operation() -> u64 {
    NEXT_OPERATION.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests;
