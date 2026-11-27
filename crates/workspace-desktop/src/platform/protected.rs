//! Хранит токены и ключи чатов через защищённое хранилище Windows или Linux.

use std::path::Path;

use workspace_core::auth::{SecretStore, StoreError};
use zeroize::Zeroizing;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
use windows::Records;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
use linux::Records;

/// Открывает отдельный набор защищённых записей для токенов входа.
pub(crate) struct ProfileSecrets(ProtectedRecords); // Записи имеют те же ключи, что метаданные профиля.

impl ProfileSecrets {
    /// Подключает хранилище токенов к профилю обычного запуска.
    pub(crate) fn new(profile: &Path) -> Self {
        Self(ProtectedRecords::new(profile, "com.workspace.desktop.auth"))
    }
}

impl SecretStore for ProfileSecrets {
    /// Читает токен, если секрет ещё существует в хранилище текущего пользователя.
    fn load(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.0
            .read(key)?
            .map(|value| {
                std::str::from_utf8(&value)
                    .map(str::to_owned)
                    .map_err(|_| StoreError)
            })
            .transpose()
    }

    /// Сохраняет токен в защищённом виде для последующего запуска.
    fn save(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.0.write(key, value.as_bytes())
    }

    /// Удаляет токен при выходе или ротации.
    fn remove(&self, key: &str) -> Result<(), StoreError> {
        self.0.remove(key)
    }
}

/// Хранит произвольные байты одного набора записей.
pub(crate) struct ProtectedRecords(Records); // Системный адаптер, выбранный при сборке.

impl ProtectedRecords {
    /// Привязывает системный адаптер к виду записей приложения.
    pub(crate) fn new(profile: &Path, service: &'static str) -> Self {
        Self(Records::new(profile, service))
    }

    /// Читает закрытую запись и очищает её байты после использования.
    pub(crate) fn read(&self, key: &str) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        self.0.read(key)
    }

    /// Заменяет целую запись в защищённом хранилище.
    pub(crate) fn write(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        self.0.write(key, value)
    }

    /// Удаляет секрет и допускает повторную очистку после сбоя.
    fn remove(&self, key: &str) -> Result<(), StoreError> {
        self.0.remove(key)
    }
}
