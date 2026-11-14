//! Проверяет скрытие и возврат окна Iced через строку меню macOS.

#[cfg(all(target_os = "macos", feature = "ui-kit"))]
mod probe {
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering::Relaxed};
    use std::time::Duration;

    use iced::{Element, Task, widget, window};
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::{NSObject, NSObjectProtocol};
    use objc2::{
        ClassType, DeclaredClass, class, declare_class, msg_send, msg_send_id, mutability, sel,
    };
    use objc2_foundation::NSString;

    static COMMAND: AtomicU8 = AtomicU8::new(0);
    static STOP: AtomicBool = AtomicBool::new(false);
    static COUNT: AtomicUsize = AtomicUsize::new(0);

    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    declare_class!(
        /// Получает команды меню на главном потоке macOS.
        struct MenuTarget;

        unsafe impl ClassType for MenuTarget {
            type Super = NSObject;
            type Mutability = mutability::Immutable;
            const NAME: &'static str = "WorkspaceDesktopTrayProbeTarget";
        }

        impl DeclaredClass for MenuTarget {
            type Ivars = ();
        }

        unsafe impl NSObjectProtocol for MenuTarget {}

        unsafe impl MenuTarget {
            /// Просит Iced вернуть существующее окно.
            #[method(probeShow:)]
            fn show(&self, _sender: &NSObject) {
                COMMAND.store(1, Relaxed);
            }

            /// Просит Iced завершить приложение.
            #[method(probeQuit:)]
            fn quit(&self, _sender: &NSObject) {
                COMMAND.store(2, Relaxed);
            }
        }
    );

    /// Держит значок и получателя команд, пока работает окно.
    struct Tray {
        bar: Retained<NSObject>,       // Системная строка меню.
        item: Retained<NSObject>,      // Значок, который нужно убрать при выходе.
        _target: Retained<MenuTarget>, // AppKit хранит слабую ссылку на получателя.
    }

    impl Tray {
        /// Создаёт меню из главного потока. При сбое приложение оставляет обычное закрытие.
        fn install() -> Option<Self> {
            unsafe {
                let bar: Retained<NSObject> = msg_send_id![class!(NSStatusBar), systemStatusBar];
                let item: Retained<NSObject> = msg_send_id![&bar, statusItemWithLength: -1.0f64];
                let button: Option<Retained<NSObject>> = msg_send_id![&item, button];
                let Some(button) = button else {
                    let _: () = msg_send![&bar, removeStatusItem: &*item];
                    return None;
                };
                let _: () = msg_send![&button, setTitle: &*NSString::from_str("WD Probe")];
                let target: Retained<MenuTarget> =
                    msg_send_id![super(MenuTarget::alloc().set_ivars(())), init];
                let menu: Retained<NSObject> = msg_send_id![class!(NSMenu), new];
                for (title, action) in [
                    ("Show window", sel!(probeShow:)),
                    ("Quit probe", sel!(probeQuit:)),
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

    /// Хранит состояние единственного окна и наличие пути возврата.
    #[derive(Default)]
    struct State {
        tray: Option<Tray>,     // Доступное пользователю меню.
        id: Option<window::Id>, // Идентификатор окна, который сохраняется при скрытии.
        input: String,          // Введённый текст для проверки сохранения состояния.
    }

    /// Передаёт события окна, формы и фоновой проверки.
    #[derive(Debug, Clone)]
    enum Message {
        Opened(window::Id), // Главное окно создано.
        Close(window::Id),  // Пользователь нажал кнопку закрытия.
        Input(String),      // Текущее содержимое поля.
        Tick,               // Пора обновить счётчик и прочитать команду меню.
    }

    impl State {
        /// Меняет видимость того же окна или завершает цикл Iced.
        fn update(&mut self, message: Message) -> Task<Message> {
            match message {
                Message::Opened(id) => {
                    self.id = Some(id);
                    if std::env::var_os("WORKSPACE_TRAY_PROBE_DISABLE_MENU").is_none() {
                        self.tray = Tray::install();
                    }
                }
                Message::Close(id) if self.tray.is_some() => {
                    eprintln!(
                        "hide {id:?} count={} input={:?}",
                        COUNT.load(Relaxed),
                        self.input
                    );
                    return window::set_mode(id, window::Mode::Hidden);
                }
                Message::Close(_) => return iced::exit(),
                Message::Input(value) => self.input = value,
                Message::Tick => match COMMAND.swap(0, Relaxed) {
                    1 => {
                        if let Some(id) = self.id {
                            eprintln!(
                                "show {id:?} count={} input={:?}",
                                COUNT.load(Relaxed),
                                self.input
                            );
                            return window::set_mode(id, window::Mode::Windowed)
                                .chain(window::gain_focus(id));
                        }
                    }
                    2 => return iced::exit(),
                    _ => {}
                },
            }
            Task::none()
        }

        /// Показывает счётчик задачи и поле для проверки состояния окна.
        fn view(&self) -> Element<'_, Message> {
            widget::column![
                widget::text(format!("Background count: {}", COUNT.load(Relaxed))),
                widget::text_input("Type a marker", &self.input).on_input(Message::Input),
            ]
            .spacing(12)
            .padding(20)
            .into()
        }
    }

    /// Запускает окно и дожидается остановки фоновой задачи после выхода.
    pub fn run() -> iced::Result {
        let worker = std::thread::spawn(|| {
            while !STOP.load(Relaxed) {
                COUNT.fetch_add(1, Relaxed);
                std::thread::sleep(Duration::from_millis(100));
            }
            eprintln!("worker stopped count={}", COUNT.load(Relaxed));
        });
        let result = iced::application(State::default, State::update, State::view)
            .title("Workspace Tray Probe")
            .window(window::Settings {
                size: iced::Size::new(400.0, 140.0),
                exit_on_close_request: false,
                ..window::Settings::default()
            })
            .subscription(|_| {
                iced::Subscription::batch([
                    window::open_events().map(Message::Opened),
                    window::close_requests().map(Message::Close),
                    iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick),
                ])
            })
            .run();
        STOP.store(true, Relaxed);
        worker
            .join()
            .expect("фоновая проверка завершилась с ошибкой");
        result
    }
}

/// Запускает прототип на macOS с включённым Iced.
#[cfg(all(target_os = "macos", feature = "ui-kit"))]
fn main() -> iced::Result {
    probe::run()
}

/// Сообщает об условиях запуска на других конфигурациях.
#[cfg(not(all(target_os = "macos", feature = "ui-kit")))]
fn main() {
    eprintln!("Для запуска нужны macOS и feature ui-kit.");
}
