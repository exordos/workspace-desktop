//! Запускает окно Iced, восстанавливает аккаунты и держит синхронизацию до выхода из приложения.

#[cfg(target_os = "macos")]
/// Запускает окно и сообщает о сбое при запуске или завершении.
fn main() -> std::process::ExitCode {
    match macos::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
/// Сообщает, что для восстановления аккаунтов нужна связка ключей macOS.
fn main() -> std::process::ExitCode {
    eprintln!("Workspace Desktop requires macOS Keychain to restore accounts.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;
    use std::error::Error;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::thread;

    use futures::{StreamExt, channel::mpsc as menu_channel};
    use iced::{Element, Subscription, Task, window};
    use objc2_foundation::MainThreadMarker;
    use std::hash::{Hash, Hasher};
    use tokio::sync::{mpsc, oneshot};
    use workspace_core::auth::Auth;
    use workspace_core::bootstrap;
    use workspace_core::context::AccountId;
    use workspace_core::iam_http::WorkspaceIamHttp;
    use workspace_core::messenger::MessengerSource;
    use workspace_core::runtime::{Runtime, RuntimeError};
    use workspace_core::storage::Storage;
    use workspace_core::sync::Sync;
    use workspace_desktop::platform::tray::{Command, Tray};
    use workspace_desktop::platform::wake::WakeObserver;
    use workspace_desktop::platform::{JsonAccountStore, MacKeychain, state_directory};
    use workspace_desktop::shell::{Message as ShellMessage, Shell};

    const USAGE: &str = "Usage: workspace-desktop [--active ACCOUNT_ID] [--label ACCOUNT_ID NAME]…";

    /// Параметры выбора и подписей сохранённых аккаунтов.
    #[derive(Default)]
    struct Options {
        active: Option<AccountId>, // Аккаунт, который следует выбрать при запуске.
        labels: HashMap<AccountId, String>, // Подписи для переключателя аккаунтов.
    }

    /// Связывает события единственного окна, оболочки и меню macOS.
    #[derive(Debug, Clone)]
    enum Message {
        Shell(ShellMessage), // Событие экрана или фоновой подписки.
        Opened(window::Id),  // Оконный цикл готов, можно создать меню.
        Close(window::Id),   // Пользователь запросил закрытие окна.
        Menu(Command),       // Пользователь выбрал команду в строке меню.
    }

    /// Владеет экраном и меню до выхода из Iced.
    struct App {
        shell: Shell,           // Состояние экрана остаётся в памяти при скрытии.
        id: Option<window::Id>, // ID главного окна после его открытия.
        tray: Option<Tray>,     // Если меню нет, окно должно закрыться штатно.
        commands: menu_channel::UnboundedSender<Command>, // Передаёт команды от AppKit.
    }

    impl App {
        /// Меняет видимость окна и делегирует остальные события оболочке.
        fn update(&mut self, message: Message) -> Task<Message> {
            match message {
                Message::Shell(message) => self.shell.update(message).map(Message::Shell),
                Message::Opened(id) => {
                    self.id = Some(id);
                    if std::env::var_os("WORKSPACE_DESKTOP_DISABLE_TRAY").is_none() {
                        self.tray = MainThreadMarker::new()
                            .and_then(|main| Tray::install(main, self.commands.clone()));
                    }
                    Task::none()
                }
                Message::Close(id) if self.tray.is_some() => {
                    log::info!("Скрываем окно Workspace {id:?}");
                    window::set_mode(id, window::Mode::Hidden)
                }
                Message::Close(_) | Message::Menu(Command::Quit) => iced::exit(),
                Message::Menu(Command::Show) => self.id.map_or_else(Task::none, |id| {
                    log::info!("Возвращаем окно Workspace {id:?}");
                    window::set_mode(id, window::Mode::Windowed)
                        .chain(window::minimize(id, false))
                        .chain(window::gain_focus(id))
                }),
            }
        }

        /// Показывает существующую оболочку без отдельного экрана для меню.
        fn view(&self) -> Element<'_, Message> {
            self.shell.view().map(Message::Shell)
        }
    }

    /// Хранит приёмник до запуска постоянной подписки Iced.
    #[derive(Clone)]
    struct MenuEvents(Arc<Mutex<Option<menu_channel::UnboundedReceiver<Command>>>>); // Общий приёмник для одного запуска.

    impl Hash for MenuEvents {
        /// Опознаёт подписку по её каналу.
        fn hash<H: Hasher>(&self, state: &mut H) {
            Arc::as_ptr(&self.0).hash(state);
        }
    }

    impl MenuEvents {
        /// Передаёт входящие команды в обновление окна.
        fn stream(&self) -> impl futures::Stream<Item = Message> + use<> {
            let receiver = self
                .0
                .lock()
                .unwrap()
                .take()
                .expect("one menu subscription");
            receiver.map(Message::Menu)
        }
    }

    /// Разбирает параметры, открывает профиль и запускает окно на главном потоке.
    pub(super) fn run() -> Result<(), Box<dyn Error>> {
        let arguments = std::env::args().skip(1).collect::<Vec<_>>();
        if matches!(arguments.as_slice(), [help] if help == "--help" || help == "-h") {
            println!("{USAGE}");
            return Ok(());
        }
        let options = options(&arguments).ok_or(USAGE)?;
        if log::set_logger(&DesktopLog).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
        let directory = state_directory()?;
        let accounts = Arc::new(JsonAccountStore::open(&directory)?);
        let auth = Auth::new(
            Arc::new(WorkspaceIamHttp::new()),
            accounts,
            Arc::new(MacKeychain),
        );
        let storage = Arc::new(Storage::open(directory.join("messenger.sqlite3"))?);
        let messenger = Arc::new(MessengerSource::new()?);
        let (chat_catalog, catalog_startup) =
            bootstrap::chat_catalog(messenger.catalog(storage.clone()));
        let chat_catalog = Arc::new(chat_catalog);
        let sync = Arc::new(Sync::new(auth.clone(), storage));
        let runtime = Runtime::new(sync.clone(), vec![messenger.clone()])?;
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (stop, mut stopped) = oneshot::channel();
        let (subscribed, subscription_ready) = oneshot::channel();
        let subscribed = Mutex::new(Some(subscribed));
        let main_thread = MainThreadMarker::new().ok_or("Workspace needs the main thread")?;
        let (wake, wake_signals) = mpsc::channel(1);
        let wake_observer = WakeObserver::subscribe(main_thread, wake);
        let worker = thread::Builder::new()
            .name("workspace-sync".into())
            .spawn(move || {
                let result = executor.block_on(async {
                    tokio::select! {
                        result = subscription_ready => {
                            if result.is_err() {
                                return Ok(());
                            }
                        }
                        _ = &mut stopped => return Ok(()),
                    }
                    bootstrap::run_with_wake(
                        runtime,
                        Some(catalog_startup),
                        Some(wake_signals),
                        async {
                            let _ = stopped.await;
                        },
                        |restored| {
                            if let Err(error) = restored {
                                log::error!("Could not restore Workspace accounts: {error}");
                            }
                            Ok::<(), RuntimeError>(())
                        },
                    )
                    .await
                });
                result
                    .inspect_err(|error| log::error!("Workspace synchronization stopped: {error}"))
            })?;

        let (commands, menu_events) = menu_channel::unbounded();
        let menu_events = MenuEvents(Arc::new(Mutex::new(Some(menu_events))));
        let window = iced::application(
            move || {
                let shell = Shell::new(
                    auth.clone(),
                    options.labels.clone(),
                    sync.clone(),
                    (chat_catalog.clone(), messenger.clone()),
                    options.active.clone(),
                    subscribed
                        .lock()
                        .unwrap()
                        .take()
                        .expect("Iced opens one window"),
                );
                let prepare = shell.prepare_notifications();
                (
                    App {
                        shell,
                        id: None,
                        tray: None,
                        commands: commands.clone(),
                    },
                    prepare.map(Message::Shell),
                )
            },
            App::update,
            App::view,
        )
        .title("Workspace Desktop")
        .window(window::Settings {
            size: iced::Size::new(620.0, 360.0),
            exit_on_close_request: false,
            ..window::Settings::default()
        })
        .theme(|app: &App| app.shell.theme())
        .subscription(move |app| {
            Subscription::batch([
                app.shell.subscription().map(Message::Shell),
                window::open_events().map(Message::Opened),
                window::close_requests().map(Message::Close),
                Subscription::run_with(menu_events.clone(), MenuEvents::stream),
            ])
        })
        .run();

        drop(wake_observer);
        // Дожидаемся отмены сетевых задач и уже начатых записей перед выходом.
        let _ = stop.send(());
        let result = worker
            .join()
            .map_err(|_| "Workspace sync thread panicked")?;
        window?;
        result?;
        log::info!("Фоновая работа Workspace остановлена");
        Ok(())
    }

    /// Разбирает начальный выбор и подписи без чтения данных сессии.
    fn options(mut arguments: &[String]) -> Option<Options> {
        let mut result = Options::default();
        while !arguments.is_empty() {
            match arguments {
                [option, id, rest @ ..] if option == "--active" => {
                    result.active = Some(AccountId(id.clone()));
                    arguments = rest;
                }
                [option, id, name, rest @ ..] if option == "--label" => {
                    result.labels.insert(AccountId(id.clone()), name.clone());
                    arguments = rest;
                }
                _ => return None,
            }
        }
        Some(result)
    }

    /// Направляет диагностику синхронизации и системных адаптеров в консоль.
    struct DesktopLog;

    impl log::Log for DesktopLog {
        /// Выбирает записи приложения, исключая подробные журналы зависимостей.
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.target().starts_with("workspace")
        }

        /// Выводит сообщение выбранного модуля.
        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!("{}", record.args());
            }
        }

        /// Завершает вывод. Собственного буфера у журнала нет.
        fn flush(&self) {}
    }
}
