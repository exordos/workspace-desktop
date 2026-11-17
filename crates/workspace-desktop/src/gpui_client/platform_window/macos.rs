//! Настраивает восстановление окна при запуске и связывает его режим с AppKit.

use objc2::{
    class, msg_send, msg_send_id,
    rc::{Retained, autoreleasepool},
    runtime::{Bool, NSObject},
};
use objc2_foundation::{MainThreadMarker, NSArray, NSString};

/// Отключает системное восстановление окон до запуска GPUI.
pub(in crate::gpui_client) fn disable_state_restoration() {
    autoreleasepool(|_| {
        // При восстановлении AppKit повторно вызывает обработчик GPUI 0.2.2,
        // пока тот держит блокировку окна (zed-industries/zed#42821).
        // Домен аргументов живёт в памяти процесса и сохраняет остальные ключи запуска.
        unsafe {
            let defaults: Retained<NSObject> =
                msg_send_id![class!(NSUserDefaults), standardUserDefaults];
            let domain = NSString::from_str("NSArgumentDomain");
            let current: Retained<NSObject> =
                msg_send_id![&defaults, volatileDomainForName: &*domain];
            let arguments: Retained<NSObject> = msg_send_id![&current, mutableCopy];
            for (key, value) in [
                ("NSQuitAlwaysKeepsWindows", Bool::NO),
                ("ApplePersistenceIgnoreState", Bool::YES),
            ] {
                let key = NSString::from_str(key);
                let flag: Retained<NSObject> =
                    msg_send_id![class!(NSNumber), numberWithBool: value];
                let _: () = msg_send![&arguments, setObject: &*flag, forKey: &*key];
            }
            let _: () = msg_send![&defaults, removeVolatileDomainForName: &*domain];
            let _: () = msg_send![&defaults, setVolatileDomain: &*arguments, forName: &*domain];
        }
    });
}

/// Удостоверяет главный поток перед вызовами AppKit.
pub(super) fn main_thread() -> Option<MainThreadMarker> {
    // В сборке GPUI без Iced типизированный API NSThread не включён.
    let is_main: Bool = unsafe { msg_send![class!(NSThread), isMainThread] };
    if !is_main.as_bool() {
        return None;
    }
    // NSThread подтвердил, что этот код выполняется на главном потоке.
    Some(unsafe { MainThreadMarker::new_unchecked() })
}

/// Окно текущего процесса, которое устанавливает адаптер Workspace.
#[derive(Clone)]
pub(super) struct NativeWindow {
    window: Retained<NSObject>, // NSWindow, найденное после открытия главного окна.
    application: Retained<NSObject>, // NSApplication, которому принадлежит окно.
    _main_thread: MainThreadMarker, // Удерживает вызовы и освобождение ссылок на главном потоке.
}

impl NativeWindow {
    /// Находит основное окно в списке окон текущего приложения.
    pub(super) fn find(title: &str, main_thread: MainThreadMarker) -> Option<Self> {
        if title.is_empty() {
            return None;
        }
        // У окна Workspace свой заголовок. Берём только однозначное совпадение.
        unsafe {
            let application: Retained<NSObject> =
                msg_send_id![class!(NSApplication), sharedApplication];
            let windows: Retained<NSArray<NSObject>> = msg_send_id![&application, windows];
            let title = NSString::from_str(title);
            let mut matches = (0..windows.len())
                .filter_map(|index| windows.get_retained(index))
                .filter(|window| {
                    let caption: Retained<NSString> = msg_send_id![&**window, title];
                    *caption == *title
                });
            let window = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            Some(Self {
                window,
                application,
                _main_thread: main_thread,
            })
        }
    }

    /// Проверяет, что окно показано и находится в обычном режиме.
    pub(super) fn is_ordinary(&self) -> bool {
        unsafe {
            let app_hidden: Bool = msg_send![&self.application, isHidden];
            let visible: Bool = msg_send![&self.window, isVisible];
            let minimized: Bool = msg_send![&self.window, isMiniaturized];
            let zoomed: Bool = msg_send![&self.window, isZoomed];
            !app_hidden.as_bool() && visible.as_bool() && !minimized.as_bool() && !zoomed.as_bool()
        }
    }

    /// Выводит свёрнутое окно из Dock. Активацией фокуса владеет GPUI.
    pub(super) fn restore_from_dock(&self) {
        unsafe {
            let minimized: Bool = msg_send![&self.window, isMiniaturized];
            if minimized.as_bool() {
                let _: () = msg_send![&self.window, deminiaturize: std::ptr::null::<NSObject>()];
            }
        }
    }
}
