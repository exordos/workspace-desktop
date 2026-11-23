//! Запускает общий локальный стенд через команду `cargo stand`.

use std::process::{Command, ExitCode};

/// Передаёт аргументы скрипту стенда и возвращает его код завершения.
fn main() -> ExitCode {
    let result = Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../workspace-stand/stand.sh"
        ))
        .args(std::env::args_os().skip(1))
        .status();
    match result {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("Не удалось запустить локальный стенд: {error}");
            ExitCode::FAILURE
        }
    }
}
