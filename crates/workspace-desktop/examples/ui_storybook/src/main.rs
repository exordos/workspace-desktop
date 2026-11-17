//! Открывает историю ChatRow в отдельном GPUI Storybook.

#[cfg(target_os = "macos")]
#[path = "../../parity/chat_row.rs"]
mod chat_row;
#[cfg(target_os = "macos")]
mod story;

/// Проверяет стабильный ID и открывает соответствующую историю.
#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use chat_row::{DESCRIPTION, ID};
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => story::run(),
        [flag] if flag == ID => story::run(),
        [flag] if flag == "--list" => println!("{ID} — {DESCRIPTION}"),
        _ => {
            eprintln!("Доступна история {ID}. Укажите её ID или --list.");
            return std::process::ExitCode::FAILURE;
        }
    }
    std::process::ExitCode::SUCCESS
}

/// Сообщает о платформе примера.
#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("GPUI Storybook для Workspace запускается на macOS");
    std::process::ExitCode::FAILURE
}
