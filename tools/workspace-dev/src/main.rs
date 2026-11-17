//! Связывает команду `cargo dev` со скриптом автопересборки Workspace Desktop.

use std::process::ExitCode;

/// Передаёт процесс скрипту, чтобы Ctrl+C останавливал наблюдатель как при прямом запуске.
#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let error = Command::new("/bin/sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/dev-workspace-desktop.sh"
        ))
        .exec();
    eprintln!("Не удалось запустить dev-режим: {error}");
    ExitCode::FAILURE
}

/// Сообщает, на какой платформе доступен dev-режим приложения.
#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!("Dev-режим Workspace Desktop доступен на macOS.");
    ExitCode::FAILURE
}
