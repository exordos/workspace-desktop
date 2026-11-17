//! Проверяет обычный режим окна и восстанавливает его из Dock через AppKit.

use objc2::{
    class, msg_send, msg_send_id,
    rc::Retained,
    runtime::{Bool, NSObject},
};
use objc2_foundation::{MainThreadMarker, NSArray, NSString};

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
