//! Открывает системные адаптеры, оболочку Iced и общие виджеты приложения.

pub mod platform;

#[cfg(feature = "ui-kit")]
pub mod shell;

#[cfg(feature = "ui-kit")]
pub mod ui_kit;
