//! Открывает рабочий список GPUI с локальной историей и управляемой догрузкой.

/// Открывает отдельное окно истории GPUI.
#[cfg(target_os = "macos")]
fn main() {
    workspace_desktop::gpui_client::conversation::history_lab::run();
}

/// Сообщает, на какой платформе запускается прототип.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Прототип GPUI запускается на macOS");
}
