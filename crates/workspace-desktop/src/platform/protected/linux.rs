//! Получает защищённые записи из Secret Service графической сессии Linux.

use std::path::Path;

use keyring::{Entry, Error::NoEntry};
use workspace_core::auth::StoreError;
use zeroize::Zeroizing;

/// Записи одной службы приложения в пользовательском хранилище Linux.
pub(super) struct Records {
    service: &'static str, // Пространство имён для токенов или ключей чатов.
}

impl Records {
    /// Запоминает пространство имён. Соединение открывается при первом обращении.
    pub(super) fn new(_: &Path, service: &'static str) -> Self {
        Self { service }
    }

    /// Находит запись по паре из имени службы и ключа Workspace.
    fn entry(&self, key: &str) -> Result<Entry, StoreError> {
        Entry::new(self.service, key).map_err(|_| StoreError)
    }

    /// Считает отсутствующую запись пустой и сообщает об ошибке заблокированного хранилища.
    pub(super) fn read(&self, key: &str) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        match self.entry(key)?.get_secret() {
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            Err(NoEntry) => Ok(None),
            Err(_) => Err(StoreError),
        }
    }

    /// Записывает секрет в пользовательскую коллекцию Secret Service.
    pub(super) fn write(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        self.entry(key)?.set_secret(value).map_err(|_| StoreError)
    }

    /// Удаляет запись и допускает повторную очистку после сбоя выхода.
    pub(super) fn remove(&self, key: &str) -> Result<(), StoreError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(NoEntry) => Ok(()),
            Err(_) => Err(StoreError),
        }
    }
}
