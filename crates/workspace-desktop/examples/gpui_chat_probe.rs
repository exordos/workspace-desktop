//! Запускает общий прототип истории и редактора GPUI на фиксированных данных.

#[cfg(target_os = "macos")]
#[path = "gpui_probe/chat.rs"]
mod chat;
#[cfg(target_os = "macos")]
#[path = "gpui_probe/editor.rs"]
mod editor;
#[cfg(target_os = "macos")]
#[path = "gpui_probe/history.rs"]
mod history;

/// Открывает окно. Флаг запускает воспроизведение после появления первого кадра.
#[cfg(target_os = "macos")]
fn main() {
    let reproduce = std::env::args().any(|arg| arg == "--reproduce-scroll-jump");
    chat::open(reproduce);
}

/// Объясняет ограничение прототипа на других платформах.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Прототип GPUI запускается на macOS");
}
