//! Разбирает параметры аккаунтов и запускает отдельный клиент Workspace Desktop на GPUI.

/// Запускает окно с выбранными аккаунтом и подписями или возвращает понятную ошибку.
#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    use workspace_desktop::gpui_client;

    const USAGE: &str =
        "Usage: workspace-desktop-gpui [--active ACCOUNT_ID] [--label ACCOUNT_ID NAME]…";
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if matches!(arguments.as_slice(), [help] if help == "--help" || help == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let Some(options) = parse_options(&arguments) else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };

    if log::set_logger(&DesktopLog).is_ok() {
        let level = if std::env::var_os("WORKSPACE_DESKTOP_DEBUG").is_some_and(|value| value == "1")
        {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        };
        log::set_max_level(level);
    }
    match gpui_client::run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Сохраняет параметры выбора и подписей действующего окна Workspace.
#[cfg(target_os = "macos")]
fn parse_options(mut args: &[String]) -> Option<workspace_desktop::gpui_client::Options> {
    use workspace_core::context::AccountId;

    let mut options = workspace_desktop::gpui_client::Options::default();
    while !args.is_empty() {
        match args {
            [flag, id, rest @ ..] if flag == "--active" => {
                options.active = Some(AccountId(id.clone()));
                args = rest;
            }
            [flag, id, label, rest @ ..] if flag == "--label" => {
                options.labels.insert(AccountId(id.clone()), label.clone());
                args = rest;
            }
            _ => return None,
        }
    }
    Some(options)
}

/// Печатает только записи GPUI-клиента с идентификаторами операций и безопасными статусами.
#[cfg(target_os = "macos")]
struct DesktopLog;

#[cfg(target_os = "macos")]
impl log::Log for DesktopLog {
    /// Исключает сетевые библиотеки и сообщения core, которые могут включать ответ сервера.
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata
            .target()
            .starts_with("workspace_desktop::gpui_client")
    }

    /// Выводит безопасную диагностическую запись рабочего окна.
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("level={} {}", record.level(), record.args());
        }
    }

    /// Журнал пишет сразу и не хранит буфер.
    fn flush(&self) {}
}

/// Сообщает, где доступен экспериментальный клиент.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Клиент Workspace Desktop на GPUI запускается на macOS");
}
