//! Готовит автоматический вход на тестовый стенд и хранит его токены в памяти.

use std::{collections::HashMap, env, io, sync::Mutex};

use workspace_core::auth::{Auth, SecretStore, StoreError};

/// Настройки входа на стенд, переданные через окружение запуска.
pub(super) struct TestLogin {
    server: String,          // HTTPS-адрес тестового Workspace.
    login: String,           // Логин тестового аккаунта A.
    password: String,        // Пароль для первого запроса IAM.
    project: Option<String>, // ID проекта; None подходит, если проект единственный.
}

impl TestLogin {
    /// Проверяет, что заданы переменные входа, и сообщает имя отсутствующей переменной.
    pub(super) fn from_environment() -> io::Result<Self> {
        Ok(Self {
            server: required("WORKSPACE_TEST_URL")?,
            login: required("WORKSPACE_TEST_LOGIN_A")?,
            password: required("WORKSPACE_TEST_PASSWORD_A")?,
            project: env::var("WORKSPACE_TEST_PROJECT_ID")
                .ok()
                .filter(|value| !value.is_empty()),
        })
    }

    /// Готовит отдельный вход для двух аккаунтов сквозного теста на стенде.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn from_test_account(letter: char) -> io::Result<Self> {
        let (login, password) = match letter {
            'A' => ("WORKSPACE_TEST_LOGIN_A", "WORKSPACE_TEST_PASSWORD_A"),
            'B' => ("WORKSPACE_TEST_LOGIN_B", "WORKSPACE_TEST_PASSWORD_B"),
            _ => return Err(io::Error::other("Тестовый аккаунт должен быть A или B")),
        };
        Ok(Self {
            server: required("WORKSPACE_TEST_URL")?,
            login: required(login)?,
            password: required(password)?,
            project: env::var("WORKSPACE_TEST_PROJECT_ID")
                .ok()
                .filter(|value| !value.is_empty()),
        })
    }

    /// Проходит штатные шаги IAM и сохраняет тестовый вход через переданный Auth.
    pub(super) fn activate(self, auth: &Auth) -> io::Result<()> {
        let attempt = auth
            .begin_login(&self.server, &self.login, &self.password, None)
            .map_err(io::Error::other)?;
        let project_id = match (self.project.as_deref(), attempt.projects.as_slice()) {
            (_, []) => {
                auth.cancel_login(attempt.id);
                return Err(io::Error::other("IAM не вернул доступных проектов"));
            }
            (Some(project), _) => project,
            (None, [project]) => &project.id,
            (None, _) => {
                auth.cancel_login(attempt.id);
                return Err(io::Error::other(
                    "IAM вернул несколько проектов. Задайте WORKSPACE_TEST_PROJECT_ID",
                ));
            }
        };
        auth.complete_login(attempt.id, project_id)
            .map_err(io::Error::other)?;
        Ok(())
    }
}

/// Читает обязательное значение. Значение переменной в ошибку не попадает.
fn required(name: &str) -> io::Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::other(format!("Для тестового запуска задайте {name}")))
}

/// Токены тестовых сессий, доступные только внутри текущего процесса.
#[derive(Default)]
pub(super) struct MemorySecrets {
    values: Mutex<HashMap<String, String>>, // Токены по ключам записей текущего профиля.
}

impl SecretStore for MemorySecrets {
    /// Читает токен текущего запуска, если он сохранён.
    fn load(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .values
            .lock()
            .map_err(|_| StoreError)?
            .get(key)
            .cloned())
    }

    /// Сохраняет токен в памяти или заменяет его при обновлении сессии.
    fn save(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.values
            .lock()
            .map_err(|_| StoreError)?
            .insert(key.into(), value.into());
        Ok(())
    }

    /// Убирает токен при выходе. Отсутствующая запись уже считается удалённой.
    fn remove(&self, key: &str) -> Result<(), StoreError> {
        self.values.lock().map_err(|_| StoreError)?.remove(key);
        Ok(())
    }
}
