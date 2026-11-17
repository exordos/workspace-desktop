//! Запускает отдельное окно Workspace Desktop на GPUI.

/// Передаёт управление точке сборки клиента на главном потоке.
#[cfg(target_os = "macos")]
fn main() {
    workspace_desktop::gpui_client::run();
}

/// Сообщает, где доступен экспериментальный клиент.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Клиент Workspace Desktop на GPUI запускается на macOS");
}
