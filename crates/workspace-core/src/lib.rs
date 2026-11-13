//! Объявляет модули ядра Workspace Desktop.
//! Здесь подключены контекст, авторизация, клиент IAM, получение событий, уведомления и локальное хранение.

pub mod auth;
pub mod context;
pub mod iam_http;
pub mod messenger;
pub mod notification_target;
pub mod runtime;
pub mod storage;
pub mod sync;
