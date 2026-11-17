//! Открывает системные адаптеры и точку входа клиента GPUI.

#[cfg(all(feature = "gpui", target_os = "macos"))]
pub mod gpui_client;

pub mod platform;
