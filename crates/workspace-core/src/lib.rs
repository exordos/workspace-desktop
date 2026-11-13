//! Объявляет модули ядра Workspace Desktop.
//! Здесь подключены запуск приложения, авторизация, контекст, клиент IAM, события, уведомления и хранение.

pub mod auth;
pub mod bootstrap;
pub mod context;
pub mod iam_http;
pub mod messenger;
pub mod notification_target;
pub mod runtime;
pub mod storage;
pub mod sync;
