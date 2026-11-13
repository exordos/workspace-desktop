//! Запускает окно Iced, восстанавливает аккаунты и держит синхронизацию до закрытия окна.

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

    use tokio::sync::oneshot;
    use workspace_core::auth::Auth;
    use workspace_core::context::AccountId;
    use workspace_core::iam_http::WorkspaceIamHttp;
    use workspace_core::messenger::MessengerSource;
    use workspace_core::runtime::Runtime;
    use workspace_core::storage::Storage;
    use workspace_core::sync::Sync;
    use workspace_desktop::platform::{JsonAccountStore, MacKeychain, state_directory};
    use workspace_desktop::shell::Shell;

    const USAGE: &str = "Usage: workspace-desktop [--active ACCOUNT_ID] [--label ACCOUNT_ID NAME]…";

    /// Параметры выбора и подписей сохранённых аккаунтов.
    #[derive(Default)]
    struct Options {
        active: Option<AccountId>, // Аккаунт, который следует выбрать при запуске.
        labels: HashMap<AccountId, String>, // Подписи для переключателя аккаунтов.
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
        let sync = Arc::new(Sync::new(auth.clone(), storage));
        let runtime = Runtime::new(sync.clone(), vec![Arc::new(MessengerSource::new()?)])?;
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (stop, mut stopped) = oneshot::channel();
        let (subscribed, subscription_ready) = oneshot::channel();
        let subscribed = Mutex::new(Some(subscribed));
        let background_auth = auth.clone();
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
                    let run = runtime.run(async {
                        let _ = stopped.await;
                    });
                    tokio::pin!(run);
                    tokio::select! {
                        result = &mut run => result,
                        restored = background_auth.restore() => {
                            if let Err(error) = restored {
                                log::error!("Could not restore Workspace accounts: {error}");
                            }
                            run.await
                        }
                    }
                });
                result
                    .inspect_err(|error| log::error!("Workspace synchronization stopped: {error}"))
            })?;

        let window = iced::application(
            move || {
                let shell = Shell::new(
                    auth.clone(),
                    options.labels.clone(),
                    sync.clone(),
                    options.active.clone(),
                    subscribed
                        .lock()
                        .unwrap()
                        .take()
                        .expect("Iced opens one window"),
                );
                let prepare = shell.prepare_notifications();
                (shell, prepare)
            },
            Shell::update,
            Shell::view,
        )
        .title("Workspace Desktop")
        .window_size((620.0, 360.0))
        .theme(Shell::theme)
        .subscription(Shell::subscription)
        .run();

        // Дожидаемся отмены сетевых задач и уже начатых записей перед выходом.
        let _ = stop.send(());
        let result = worker
            .join()
            .map_err(|_| "Workspace sync thread panicked")?;
        window?;
        result?;
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
