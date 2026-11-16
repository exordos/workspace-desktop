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
    use std::path::{Path, PathBuf};
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
        Shell(ShellMessage),             // Событие экрана или фоновой подписки.
        Opened(window::Id),              // Оконный цикл готов, можно создать меню.
        Close(window::Id),               // Пользователь запросил закрытие окна.
        Resized(window::Id, iced::Size), // Изменился размер окна в логических пикселях.
        CheckedSize(u64, iced::Size, window::Mode, bool), // Проверены режим и максимизация окна.
        Menu(Command),                   // Пользователь выбрал команду в строке меню.
    }

    /// Владеет экраном и меню до выхода из Iced.
    struct App {
        shell: Shell,           // Состояние экрана остаётся в памяти при скрытии.
        id: Option<window::Id>, // ID главного окна после его открытия.
        tray: Option<Tray>,     // Если меню нет, окно должно закрыться штатно.
        commands: menu_channel::UnboundedSender<Command>, // Передаёт команды от AppKit.
        size_path: PathBuf,     // Файл размера в текущем профиле.
        size_revision: u64,     // Отсекает запоздавшие ответы о режиме окна.
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
                    // Ответ о режиме до скрытия больше не относится к видимому окну.
                    self.size_revision += 1;
                    window::set_mode(id, window::Mode::Hidden)
                }
                Message::Close(_) | Message::Menu(Command::Quit) => iced::exit(),
                Message::Resized(id, size) if self.id == Some(id) => {
                    self.size_revision += 1;
                    let revision = self.size_revision;
                    window::mode(id).then(move |mode| {
                        window::is_maximized(id).map(move |maximized| {
                            Message::CheckedSize(revision, size, mode, maximized)
                        })
                    })
                }
                Message::Resized(_, _) => Task::none(),
                Message::CheckedSize(revision, size, mode, maximized) => {
                    if should_save_size(revision, self.size_revision, size, mode, maximized)
                        && let Err(error) = save_size(&self.size_path, size)
                    {
                        log::warn!("Не удалось сохранить размер окна: {error}");
                    }
                    Task::none()
                }
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
        let size_path = directory.join("window-size.json");
        let initial_size = load_size(&size_path);
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
                        size_path: size_path.clone(),
                        size_revision: 0,
                    },
                    prepare.map(Message::Shell),
                )
            },
            App::update,
            App::view,
        )
        .title("Workspace Desktop")
        .window(window::Settings {
            size: initial_size,
            exit_on_close_request: false,
            ..window::Settings::default()
        })
        .theme(|app: &App| app.shell.theme())
        .subscription(move |app| {
            Subscription::batch([
                app.shell.subscription().map(Message::Shell),
                window::open_events().map(Message::Opened),
                window::close_requests().map(Message::Close),
                window::resize_events().map(|(id, size)| Message::Resized(id, size)),
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

    /// Проверяет границы размера в логических пикселях.
    fn valid_size(size: iced::Size) -> bool {
        (100.0..=10000.0).contains(&size.width) && (100.0..=10000.0).contains(&size.height)
    }

    /// Принимает только последний ответ для обычного видимого окна.
    fn should_save_size(
        revision: u64,
        current_revision: u64,
        size: iced::Size,
        mode: window::Mode,
        maximized: bool,
    ) -> bool {
        revision == current_revision
            && mode == window::Mode::Windowed
            && !maximized
            && valid_size(size)
    }

    /// Читает размер профиля или возвращает штатный размер при любой ошибке.
    fn load_size(path: &Path) -> iced::Size {
        let size = std::fs::read(path).ok().and_then(|data| {
            let [width, height]: [f32; 2] = serde_json::from_slice(&data).ok()?;
            Some(iced::Size::new(width, height))
        });
        size.filter(|size| valid_size(*size))
            .unwrap_or(iced::Size::new(620.0, 360.0))
    }

    /// Атомарно сохраняет обычный размер рядом с файлом профиля.
    fn save_size(path: &Path, size: iced::Size) -> std::io::Result<()> {
        let directory = path
            .parent()
            .ok_or_else(|| std::io::Error::other("missing profile directory"))?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(file.as_file_mut(), &[size.width, size.height])?;
        file.persist(path).map_err(|error| error.error)?;
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
    #[cfg(test)]
    mod size_tests {
        use super::*;

        /// Старый ответ после скрытия или следующего изменения не перезаписывает размер.
        #[test]
        fn saves_only_the_current_ordinary_window_size() {
            let size = iced::Size::new(800.0, 500.0);
            assert!(should_save_size(2, 2, size, window::Mode::Windowed, false));
            assert!(!should_save_size(1, 2, size, window::Mode::Windowed, false));
            assert!(!should_save_size(2, 2, size, window::Mode::Hidden, false));
            assert!(!should_save_size(
                2,
                2,
                size,
                window::Mode::Fullscreen,
                false
            ));
            assert!(!should_save_size(2, 2, size, window::Mode::Windowed, true));
        }

        /// Проверяет независимость профилей и запасной размер при повреждённых данных.
        #[test]
        fn size_is_restored_only_from_valid_profile_data() {
            let first = tempfile::tempdir().unwrap();
            let second = tempfile::tempdir().unwrap();
            let path = first.path().join("window-size.json");
            let fallback = iced::Size::new(620.0, 360.0);
            assert_eq!(load_size(&path), fallback);
            save_size(&path, iced::Size::new(800.0, 500.0)).unwrap();
            assert_eq!(load_size(&path), iced::Size::new(800.0, 500.0));
            assert_eq!(load_size(&second.path().join("window-size.json")), fallback);
            for invalid in ["oops", "[0, 500]", "[10001, 500]", "[null, 500]"] {
                std::fs::write(&path, invalid).unwrap();
                assert_eq!(load_size(&path), fallback);
            }
        }
    }
}
