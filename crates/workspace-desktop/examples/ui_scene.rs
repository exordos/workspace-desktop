//! Запускает отдельную сцену для снимков GPUI.

#[cfg(target_os = "macos")]
mod parity;

/// Запускает сцену и сообщает об ошибке командной строке.
#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    match parity::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Сообщает об ограничении платформы.
#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("Снимки GPUI создаются в графической сессии macOS");
    std::process::ExitCode::FAILURE
}
