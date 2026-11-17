//! Создаёт меню Workspace в строке macOS и передаёт команды показа и выхода.

use futures::channel::mpsc::UnboundedSender;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{
    ClassType, DeclaredClass, class, declare_class, msg_send, msg_send_id, mutability, sel,
};
use objc2_foundation::{MainThreadMarker, NSString};

/// Команда, которую меню передаёт оконному циклу.
#[derive(Debug, Clone)]
pub enum Command {
    Show, // Вернуть существующее окно.
    Quit, // Завершить оконный цикл.
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

declare_class!(
    /// Получает команды меню на главном потоке macOS.
    struct MenuTarget;

    unsafe impl ClassType for MenuTarget {
        type Super = NSObject;
        type Mutability = mutability::Immutable;
        const NAME: &'static str = "WorkspaceDesktopTrayTarget";
    }

    impl DeclaredClass for MenuTarget {
        type Ivars = UnboundedSender<Command>;
    }

    unsafe impl NSObjectProtocol for MenuTarget {}

    unsafe impl MenuTarget {
        /// Просит оболочку вернуть существующее окно.
        #[method(workspaceShow:)]
        fn show(&self, _sender: &NSObject) {
            let _ = self.ivars().unbounded_send(Command::Show);
        }

        /// Просит оболочку завершить приложение.
        #[method(workspaceQuit:)]
        fn quit(&self, _sender: &NSObject) {
            let _ = self.ivars().unbounded_send(Command::Quit);
        }
    }
);

/// Держит значок и получателя команд, пока работает окно.
pub struct Tray {
    bar: Retained<NSObject>,       // Системная строка меню.
    item: Retained<NSObject>,      // Значок, который нужно убрать при выходе.
    _target: Retained<MenuTarget>, // AppKit хранит слабую ссылку на получателя.
}

impl Tray {
    /// Создаёт меню из главного потока. При сбое приложение оставляет обычное закрытие.
    pub fn install(
        _main_thread: MainThreadMarker,
        commands: UnboundedSender<Command>,
    ) -> Option<Self> {
        // Все обращения к AppKit выполняются на главном потоке.
        unsafe {
            let bar: Retained<NSObject> = msg_send_id![class!(NSStatusBar), systemStatusBar];
            let item: Option<Retained<NSObject>> =
                msg_send_id![&bar, statusItemWithLength: -1.0f64];
            let item = item?;
            let button: Option<Retained<NSObject>> = msg_send_id![&item, button];
            let Some(button) = button else {
                let _: () = msg_send![&bar, removeStatusItem: &*item];
                return None;
            };
            let _: () = msg_send![&button, setTitle: &*NSString::from_str("Workspace")];
            let target: Retained<MenuTarget> =
                msg_send_id![super(MenuTarget::alloc().set_ivars(commands)), init];
            let menu: Retained<NSObject> = msg_send_id![class!(NSMenu), new];
            for (title, action) in [
                ("Показать окно", sel!(workspaceShow:)),
                ("Выход", sel!(workspaceQuit:)),
            ] {
                let menu_item: Allocated<NSObject> = msg_send_id![class!(NSMenuItem), alloc];
                let menu_item: Retained<NSObject> = msg_send_id![menu_item,
                    initWithTitle: &*NSString::from_str(title),
                    action: action,
                    keyEquivalent: &*NSString::from_str("")
                ];
                let _: () = msg_send![&menu_item, setTarget: &*target];
                let _: () = msg_send![&menu, addItem: &*menu_item];
            }
            let _: () = msg_send![&item, setMenu: &*menu];
            Some(Self {
                bar,
                item,
                _target: target,
            })
        }
    }
}

impl Drop for Tray {
    /// Убирает значок из системной строки меню.
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![&self.bar, removeStatusItem: &*self.item];
        }
    }
}
