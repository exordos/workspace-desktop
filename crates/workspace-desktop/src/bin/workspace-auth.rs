//! Запускает команды авторизации и синхронизации без окна.
//! Сначала обрабатывает справку и ошибки запуска, затем подключает хранилища macOS и выполняет команду.

use std::error::Error;
use std::process::ExitCode;

const USAGE: &str = "Usage: workspace-auth <command>\n\
  login <https://workspace-server>   Sign in and select a project\n\
  status                            Restore and list saved accounts\n\
  verify <account-id>               Verify access to Workspace Messenger\n\
  sync                               Sync Messenger until Ctrl-C\n\
  logout <account-id>               Sign out locally and delete the saved secret";

/// Читает аргументы и возвращает код завершения процесса.
fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    match run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Показывает справку или передаёт аргументы реализации для текущей системы.
fn run(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    match arguments {
        [] => println!("{USAGE}"),
        [help] if help == "--help" || help == "-h" || help == "help" => println!("{USAGE}"),
        _ => platform::run(arguments)?,
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
mod platform {
    /// Сообщает, что для команд нужна связка ключей macOS.
    pub(super) fn run(_: &[String]) -> Result<(), Box<dyn std::error::Error>> {
        Err("workspace-auth requires macOS Keychain".into())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::error::Error;
    use std::io::{self, IsTerminal, Write};
    use std::sync::Arc;

    use futures::executor::block_on;
    use workspace_core::auth::{Auth, AuthError, IamError, Service, SessionStatus};
    use workspace_core::context::AccountId;
    use workspace_core::iam_http::WorkspaceIamHttp;
    use workspace_core::messenger::MessengerSource;
    use workspace_core::runtime::Runtime;
    use workspace_core::storage::Storage;
    use workspace_core::sync::Sync;
    use workspace_desktop::platform::{JsonAccountStore, MacKeychain, state_directory};

    /// Команды, которые утилита умеет выполнять.
    enum Command<'a> {
        Login(&'a str),    // Адрес сервера для входа.
        Status,            // Восстанавливает сессии и выводит список аккаунтов.
        Verify(AccountId), // Аккаунт, для которого проверяем доступ к мессенджеру.
        Sync,              // Запускает фоновую синхронизацию мессенджера.
        Logout(AccountId), // Аккаунт, из которого нужно выйти.
    }

    /// Разбирает аргументы, подключает хранилища и выполняет команду macOS.
    pub(super) fn run(arguments: &[String]) -> Result<(), Box<dyn Error>> {
        let command = match arguments {
            [name, server] if name == "login" => Command::Login(server),
            [name] if name == "status" => Command::Status,
            [name, id] if name == "verify" => Command::Verify(AccountId(id.clone())),
            [name] if name == "sync" => Command::Sync,
            [name, id] if name == "logout" => Command::Logout(AccountId(id.clone())),
            _ => return Err(super::USAGE.into()),
        };
        let iam = Arc::new(WorkspaceIamHttp::new());
        let directory = state_directory()?;
        let accounts = Arc::new(JsonAccountStore::open(&directory)?);
        let auth = Auth::new(iam.clone(), accounts, Arc::new(MacKeychain));

        match command {
            Command::Login(server) => login(&auth, server)?,
            Command::Status => {
                block_on(auth.restore())?;
                let sessions = auth.snapshot();
                if sessions.is_empty() {
                    println!("No saved Workspace accounts.");
                }
                for session in sessions {
                    let context = session.context;
                    println!(
                        "{}  {}  project={}  {}",
                        context.account_id.0.escape_debug(),
                        context.server.escape_debug(),
                        context.project_id.escape_debug(),
                        status_label(session.status),
                    );
                }
            }
            Command::Verify(id) => {
                block_on(auth.restore())?;
                let session = auth
                    .snapshot()
                    .into_iter()
                    .find(|session| session.context.account_id == id)
                    .ok_or("Workspace account not found")?;
                if session.status != SessionStatus::Active {
                    return Err(
                        format!("Workspace session is {}", status_label(session.status)).into(),
                    );
                }
                let context = session.context;
                let token = block_on(auth.access_token(&context, Service::Messenger))?;
                iam.verify_messenger(&context, &token)?;
                println!("Workspace Messenger accepted the project's credentials.");
            }
            Command::Sync => {
                log::set_logger(&SyncLog).expect("logger initialized once");
                log::set_max_level(log::LevelFilter::Info);
                let storage = Arc::new(Storage::open(directory.join("messenger.sqlite3"))?);
                let sync = Arc::new(Sync::new(auth.clone(), storage));
                let runtime = Runtime::new(sync, vec![Arc::new(MessengerSource::new()?)])?;
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(async {
                        let mut interrupt = tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::interrupt(),
                        )?;
                        let (stop, stopped) = tokio::sync::oneshot::channel();
                        let restore = async {
                            tokio::select! {
                                _ = interrupt.recv() => Ok::<(), Box<dyn Error>>(()),
                                restored = auth.restore() => {
                                    restored?;
                                    if auth.snapshot().is_empty() {
                                        return Err("No saved Workspace accounts.".into());
                                    }
                                    interrupt.recv().await;
                                    Ok(())
                                }
                            }
                        };
                        let run = runtime.run(async {
                            let _ = stopped.await;
                        });
                        tokio::pin!(run);
                        tokio::select! {
                            result = &mut run => result?,
                            restored = restore => {
                                let _ = stop.send(());
                                run.await?;
                                restored?;
                            }
                        }
                        Ok::<(), Box<dyn Error>>(())
                    })?;
            }
            Command::Logout(id) => {
                auth.logout(&id)?;
                println!("Workspace account signed out locally.");
            }
        }
        Ok(())
    }

    /// Выводит сообщения фоновой синхронизации в терминал.
    struct SyncLog;

    impl log::Log for SyncLog {
        /// Проверяет, нужно ли выводить запись журнала этого уровня.
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.target().starts_with("workspace_core::")
        }

        /// Печатает подходящую запись журнала в stderr.
        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!("{}", record.args());
            }
        }

        /// Завершает вывод журнала. Собственного буфера у него нет.
        fn flush(&self) {}
    }

    /// Спрашивает данные для входа и даёт выбрать доступный проект.
    fn login(auth: &Auth, server: &str) -> Result<(), Box<dyn Error>> {
        if !io::stdin().is_terminal() {
            return Err("Workspace login requires an interactive terminal".into());
        }
        let login = prompt("Login: ")?;
        let password = rpassword::prompt_password("Password: ")?;
        let attempt = match auth.begin_login(server, &login, &password, None) {
            Err(AuthError::Iam(IamError::OtpRequired)) => {
                let otp = rpassword::prompt_password("One-time password: ")?;
                auth.begin_login(server, &login, &password, Some(&otp))?
            }
            result => result?,
        };
        drop(password);

        if attempt.projects.is_empty() {
            auth.cancel_login(attempt.id);
            return Err("IAM returned no available projects".into());
        }

        for (index, project) in attempt.projects.iter().enumerate() {
            println!(
                "{}. {} ({})",
                index + 1,
                project.name.escape_debug(),
                project.id.escape_debug(),
            );
        }
        let selected = prompt("Project number (empty to cancel): ").and_then(|number| {
            if number.is_empty() {
                return Ok(None);
            }
            let index = number
                .parse::<usize>()
                .ok()
                .and_then(|number| number.checked_sub(1));
            index
                .and_then(|index| attempt.projects.get(index))
                .map(Some)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Invalid project number")
                })
        });
        let context = match selected {
            Ok(Some(project)) => auth.complete_login(attempt.id, &project.id),
            selection => {
                auth.cancel_login(attempt.id);
                selection?;
                println!("Workspace login cancelled.");
                return Ok(());
            }
        }?;
        println!(
            "Signed in to project {} as account {}.",
            context.project_id.escape_debug(),
            context.account_id.0.escape_debug(),
        );
        Ok(())
    }

    /// Выводит приглашение и читает одну строку из терминала.
    fn prompt(message: &str) -> io::Result<String> {
        eprint!("{message}");
        io::stderr().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        Ok(input.trim().to_owned())
    }

    /// Возвращает короткую подпись состояния сессии для терминала.
    fn status_label(status: SessionStatus) -> &'static str {
        match status {
            SessionStatus::Restoring => "restoring",
            SessionStatus::Active => "active",
            SessionStatus::TemporarilyUnavailable => "temporarily unavailable",
            SessionStatus::ReauthenticationRequired => "sign in required",
            SessionStatus::SigningOut => "sign-out pending",
        }
    }
}
