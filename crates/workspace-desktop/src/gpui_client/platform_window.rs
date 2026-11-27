//! Связывает главное окно GPUI с треем, уведомлениями и остановкой фоновой работы.
//! Здесь находятся настройки окна и обработчики системных действий.

#[cfg(target_os = "macos")]
pub(super) mod macos;
mod window_size;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
#[cfg(target_os = "macos")]
use futures::channel::mpsc;
use gpui::{
    AnyWindowHandle, App, AppContext, Entity, Global, KeyBinding, Subscription,
    Task, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, px, size,
};
#[cfg(target_os = "macos")]
use gpui::{Menu, MenuItem};
use workspace_core::context::{AccountId, OwnerContext};

use super::{
    contracts::RuntimeExit,
    preferences::Preferences,
    runtime::RuntimeClient,
};
use crate::platform::notifications::{NotificationClick, NotificationError, NotificationPermission, Notifications};
#[cfg(target_os = "macos")]
use crate::platform::{
    tray::{Command, Tray},
    wake::WakeObserver,
};
#[cfg(target_os = "macos")]
use super::preferences::{Language, NotificationSound};
#[cfg(target_os = "macos")]
use macos::NativeWindow;
use window_size::{SizeObserver, SizeWriter};

/// Бэкенды Windows и Linux управляют окном через GPUI.
#[cfg(not(target_os = "macos"))]
type NativeWindow = std::convert::Infallible;

actions!(workspace_window, [Quit]);

static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

/// Держит адаптеры одного рабочего окна до штатного выхода приложения.
pub struct WindowPlatform {
    window: AnyWindowHandle, // Окно, которое поднимается при системном действии.
    native: Option<NativeWindow>, // Ссылка macOS для восстановления из Dock.
    runtime: RuntimeClient,  // Фоновая работа, которую нужно остановить до выхода.
    notifications: Notifications, // Центр уведомлений и связь баннера с исходной целью.
    permission: Option<NotificationPermission>, // Последний применённый ответ macOS.
    permission_read: u64,    // Номер последнего чтения, чтобы отсеять поздние ответы.
    requesting_permission: bool, // Системный запрос занимает кнопку до чтения нового статуса.
    quitting: bool,          // Идёт завершение, повторные действия отбрасываются.
    #[cfg(target_os = "macos")]
    tray: Option<Tray>,      // Меню Workspace живёт вместе с оконным циклом.
    #[cfg(target_os = "macos")]
    _wake: WakeObserver,     // Передаёт пробуждение macOS фоновой работе.
    _bounds: Entity<SizeObserver>, // Подписка на размеры главного окна.
    size_writer: Option<SizeWriter>, // Ждёт последнюю запись при выходе.
    _closed: Subscription,   // Обычное закрытие окна тоже завершает фон.
    #[cfg(target_os = "macos")]
    _menu_task: Task<()>,    // Получает действия из меню трея.
    _click_task: Option<Task<()>>, // Передаёт нажатие по баннеру в оболочку.
}

impl Global for WindowPlatform {}

impl WindowPlatform {
    /// Восстанавливает размер области содержимого из существующего профиля.
    pub fn window_options(profile: &Path, app: &App) -> WindowOptions {
        WindowOptions {
            window_min_size: Some(size(
                px(window_size::MIN_WIDTH),
                px(window_size::MIN_HEIGHT),
            )),
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
        #[cfg(target_os = "macos")]
        let main_thread = macos::main_thread()
            .ok_or_else(|| std::io::Error::other("Окну Workspace нужен главный поток"))?;
        #[cfg(target_os = "macos")]
        let native = window.update(app, |_, window, _| {
            macos::NativeWindow::find(&window.window_title(), main_thread)
        })?;
        #[cfg(not(target_os = "macos"))]
        let native: Option<NativeWindow> = None;
        log::debug!(
            "event=workspace_native_window attached={}",
            native.is_some()
        );
        #[cfg(target_os = "macos")]
        let (commands, mut menu) = mpsc::unbounded();
        #[cfg(target_os = "macos")]
        let tray = if std::env::var_os("WORKSPACE_DESKTOP_DISABLE_TRAY").is_none() {
            let (show, quit) = menu_labels(Preferences::read(app).language);
            Tray::install(main_thread, commands, show, quit)
        } else {
            None
        };
        #[cfg(target_os = "macos")]
        let has_tray = tray.is_some();

        let size_writer = SizeWriter::new(profile, app);
        let sizes = size_writer.updates();
        let bounds = window.update(app, |_, window, app| {
            let close_native = native.clone();
            let close_sizes = sizes.clone();
            window.on_window_should_close(app, move |window, app| {
                // Здесь окно ещё доступно, даже если скоро уйдёт из GPUI без трея.
                window_size::record(window, close_native.as_ref(), &close_sizes);
                #[cfg(not(target_os = "macos"))]
                {
                    // При закрытии последнего окна Linux завершает цикл GPUI. Сначала ждём Bootstrap.
                    app.defer(Self::request_quit);
                    return false;
                }
                #[cfg(target_os = "macos")]
                if has_tray {
                    let op_id = operation();
                    log::debug!("op_id={op_id} event=workspace_window_hide");
                    app.hide();
                }
                #[cfg(target_os = "macos")]
                !has_tray
            });
            app.new(|cx| SizeObserver::new(window, native.clone(), sizes, cx))
        })?;

        let closed = app.on_window_closed(move |app| {
            if !app.windows().contains(&window) {
                Self::request_quit(app);
            }
        });
        #[cfg(target_os = "macos")]
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

        #[cfg(target_os = "macos")]
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
        app.set_global(Self {
            window,
            native,
            runtime,
            notifications,
            permission: None,
            permission_read: 0,
            requesting_permission: false,
            quitting: false,
            #[cfg(target_os = "macos")]
            tray,
            #[cfg(target_os = "macos")]
            _wake: wake,
            _bounds: bounds,
            size_writer: Some(size_writer),
            _closed: closed,
            #[cfg(target_os = "macos")]
            _menu_task: menu_task,
            _click_task: click_task,
        });
        app.bind_keys([KeyBinding::new("secondary-q", Quit, None)]);
        app.on_action(|_: &Quit, app| Self::request_quit(app));
        #[cfg(target_os = "macos")]
        bind_menu_labels(app, |app, show, quit| {
            set_app_menu(app, quit);
            if let Some(tray) = app.try_global::<Self>().and_then(|platform| platform.tray.as_ref())
                && let Some(main_thread) = macos::main_thread()
            {
                tray.set_labels(main_thread, show, quit);
            }
        });

        Self::refresh_notification_permission(app);
        Ok(())
    }

    /// Возвращает статус для планки и признак незавершённого системного запроса.
    pub(super) fn notification_permission(app: &App) -> (Option<NotificationPermission>, bool) {
        app.try_global::<Self>()
            .map(|platform| (platform.permission, platform.requesting_permission))
            .unwrap_or_default()
    }

    /// При запуске и возврате из настроек пассивно перечитывает разрешение.
    pub(super) fn refresh_notification_permission(app: &mut App) {
        let Some(platform) = app.try_global::<Self>() else {
            return;
        };
        if !platform.quitting && !platform.requesting_permission {
            Self::read_notification_permission(app, operation());
        }
    }

    /// Запускает системный диалог ровно один раз до получения обновлённого статуса.
    pub(super) fn request_notification_permission(app: &mut App) {
        let Some(platform) = app.try_global::<Self>() else {
            return;
        };
        if platform.quitting
            || platform.requesting_permission
            || platform.permission != Some(NotificationPermission::NeedsPermission)
        {
            return;
        }
        let op_id = operation();
        let platform = app.global_mut::<Self>();
        platform.requesting_permission = true;
        platform.permission_read = op_id;
        log::debug!("op_id={op_id} event=workspace_notification_permission_requested");
        let request = platform.notifications.request_permission();
        app.refresh_windows();
        app.spawn(async move |cx| {
            let result = request.await;
            log::debug!(
                "op_id={op_id} event=workspace_notification_permission_answered result={}",
                Self::notification_result(&result),
            );
            let _ = cx.update(|app| Self::read_notification_permission(app, op_id));
        })
        .detach();
    }

    /// Передаёт macOS ссылку на настройки, а окно при ошибке покажет путь вручную.
    pub(super) fn open_notification_settings(app: &App) -> bool {
        let opened = app
            .try_global::<Self>()
            .is_some_and(|platform| platform.notifications.open_settings());
        log::debug!("event=workspace_notification_settings_opened result={opened}");
        opened
    }

    /// Сохраняет только актуальный ответ ОС, включая обновление после диалога.
    fn read_notification_permission(app: &mut App, op_id: u64) {
        let platform = app.global_mut::<Self>();
        if platform.quitting {
            return;
        }
        platform.permission_read = op_id;
        let read = platform.notifications.permission();
        app.spawn(async move |cx| {
            let result = read.await;
            let _ = cx.update(|app| {
                let platform = app.global_mut::<Self>();
                if platform.quitting || platform.permission_read != op_id {
                    return;
                }
                let status = match &result {
                    Ok(NotificationPermission::NeedsPermission) => "needs_permission",
                    Ok(NotificationPermission::NeedsSettings) => "needs_settings",
                    Ok(NotificationPermission::Enabled) => "enabled",
                    Err(error) => Self::notification_error_result(error),
                };
                log::debug!(
                    "op_id={op_id} event=workspace_notification_permission_read result={status}"
                );
                let permission = result.ok();
                let changed = platform.requesting_permission || platform.permission != permission;
                platform.requesting_permission = false;
                platform.permission = permission;
                if changed {
                    app.refresh_windows();
                }
            });
        })
        .detach();
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
        #[cfg(target_os = "macos")]
        {
            let settings = Preferences::read_for(app, &target.context);
            if settings.notification_sound != NotificationSound::None {
                super::notification_audio::preview(settings.notification_sound);
            }
            if settings.dock_bounce {
                macos::request_attention();
            }
        }
        let AccountId(account) = &target.context.account_id;
        let id = format!(
            "{}/{}/{}",
            account, target.context.project_id, target.message_id
        );
        let label = label.unwrap_or_else(|| account.get(..8).unwrap_or(account));
        let (title, body) = notification_text(app, &target.context, label);
        let delivery = platform
            .notifications
            .show(&id, title, &body, target, click.generation);
        let op_id = operation();
        log::debug!("op_id={op_id} event=workspace_notification_requested");
        app.spawn(async move |_| {
            let result = delivery.await;
            Self::notification_finished(op_id, result);
        })
        .detach();
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

    /// Записывает итог системной операции без текста уведомления и данных аккаунта.
    fn notification_finished(op_id: u64, result: Result<(), NotificationError>) {
        let status = Self::notification_result(&result);
        log::debug!("op_id={op_id} event=workspace_notification_finished result={status}");
    }

    /// Преобразует системный результат в безопасное поле журнала.
    fn notification_result<T>(result: &Result<T, NotificationError>) -> &'static str {
        match result {
            Ok(_) => "ok",
            Err(error) => Self::notification_error_result(error),
        }
    }

    /// Указывает тип сбоя, исключая из журнала свободный текст macOS.
    fn notification_error_result(error: &NotificationError) -> &'static str {
        match error {
            NotificationError::Unavailable => "unavailable",
            NotificationError::PermissionDenied => "permission_denied",
            NotificationError::System(_) => "system_error",
        }
    }
}

/// Получает тексты системных команд по языку выбранного входа.
#[cfg(target_os = "macos")]
fn menu_labels(language: Language) -> (&'static str, &'static str) {
    (language.tr("desktop.menu.showWindow"), language.tr("desktop.menu.quit"))
}

/// Передаёт подписи при первом запуске и при смене языка выбранного входа.
#[cfg(target_os = "macos")]
fn bind_menu_labels(app: &mut App, mut update: impl FnMut(&mut App, &'static str, &'static str) + 'static) {
    let mut previous = Preferences::read(app).language;
    let (show, quit) = menu_labels(previous);
    update(app, show, quit);
    app.observe_global::<Preferences>(move |app| {
        let current = Preferences::read(app).language;
        if current == previous {
            return;
        }
        previous = current;
        let (show, quit) = menu_labels(current);
        update(app, show, quit);
    }).detach();
}

/// Заменяет главное меню GPUI, сохраняя системное действие закрытия приложения.
#[cfg(target_os = "macos")]
fn set_app_menu(app: &mut App, quit: &'static str) {
    app.set_menus(vec![Menu {
        name: "Workspace Desktop".into(),
        items: vec![MenuItem::action(quit, Quit)],
    }]);
}

/// Готовит подписи по языку владельца уведомления, даже если в окне открыт другой аккаунт.
fn notification_text(app: &App, owner: &OwnerContext, label: &str) -> (&'static str, String) {
    let language = Preferences::read_for(app, owner).language;
    (
        language.tr("notifications.newMessage"),
        language.tr_with("desktop.notifications.accountLabel", &[("account", label)]),
    )
}

/// Возвращает окно из Dock и активирует его текущий экран.
fn raise_window(
    app: &mut App,
    window: AnyWindowHandle,
    _native: Option<NativeWindow>,
    op_id: u64,
) {
    #[cfg(target_os = "macos")]
    if let Some(native) = _native {
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
    native: Option<NativeWindow>,
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
