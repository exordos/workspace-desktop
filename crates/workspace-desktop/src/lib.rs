//! Открывает системные адаптеры и точку входа клиента GPUI.

#[cfg(all(
    feature = "gpui",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
pub mod gpui_client;

pub mod platform;
