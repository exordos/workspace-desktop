//! Подключает сквозной GPUI-сценарий к настоящему локальному Workspace.
//!
//! Для каждого аккаунта создаётся отдельный временный профиль и обычный рабочий
//! runtime. Адрес, учётные данные и доверенный CA передаются через окружение.

use std::path::Path;

use super::{RuntimeClient, RuntimeEvents};
use crate::gpui_client::test_profile::TestLogin;

/// Запускает рабочий клиент аккаунта A или B с изолированным профилем.
pub(super) fn start_stand_account(profile: &Path, letter: char) -> (RuntimeClient, RuntimeEvents) {
    let login = TestLogin::from_test_account(letter)
        .unwrap_or_else(|error| panic!("Не подготовлен тестовый аккаунт {letter}: {error}"));
    RuntimeClient::start_test_profile(profile.to_owned(), login)
        .unwrap_or_else(|error| panic!("Не запущен тестовый аккаунт {letter}: {error}"))
}
