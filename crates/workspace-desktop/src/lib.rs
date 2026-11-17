//! Открывает системные адаптеры и точки входа клиентов Iced и GPUI.

#[cfg(all(feature = "gpui", target_os = "macos"))]
pub mod gpui_client;

pub mod platform;

#[cfg(feature = "ui-kit")]
pub mod shell;

#[cfg(feature = "ui-kit")]
pub mod ui_kit;
