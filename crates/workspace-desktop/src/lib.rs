//! Открывает системные адаптеры и, при включённой возможности ui-kit, общие виджеты приложения.

pub mod platform;

#[cfg(feature = "ui-kit")]
pub mod ui_kit;
