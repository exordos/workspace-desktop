//! Хранит токены обновления в связке ключей macOS.
//! В конце есть тест чтения, замены и удаления секрета в настоящей связке ключей.

use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use workspace_core::auth::{SecretStore, StoreError};

const SERVICE: &str = "com.workspace.desktop.auth";
const ITEM_NOT_FOUND: i32 = -25300;

/// Адаптер защищённого хранилища macOS для авторизации.
pub struct MacKeychain;

impl SecretStore for MacKeychain {
    /// Читает токен по ключу. Возвращает `None`, если записи нет.
    fn load(&self, key: &str) -> Result<Option<String>, StoreError> {
        match get_generic_password(SERVICE, key) {
            Ok(value) => String::from_utf8(value).map(Some).map_err(|_| StoreError),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
            Err(_) => Err(StoreError),
        }
    }

    /// Записывает новый токен или заменяет старый по тому же ключу.
    fn save(&self, key: &str, value: &str) -> Result<(), StoreError> {
        set_generic_password(SERVICE, key, value.as_bytes()).map_err(|_| StoreError)
    }

    /// Удаляет секрет. Отсутствие записи считается успешной очисткой.
    fn remove(&self, key: &str) -> Result<(), StoreError> {
        match delete_generic_password(SERVICE, key) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(()),
            Err(_) => Err(StoreError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет запись, чтение, замену и удаление тестового секрета в связке ключей.
    #[test]
    #[ignore = "requires an unlocked macOS login Keychain"]
    fn keychain_round_trip() {
        /// Удаляет тестовый секрет, когда тест выходит из области видимости.
        struct Cleanup(String); // Ключ тестовой записи в связке ключей.
        impl Drop for Cleanup {
            /// Пытается удалить тестовый секрет даже после сбоя проверки.
            fn drop(&mut self) {
                let _ = MacKeychain.remove(&self.0);
            }
        }

        let directory = tempfile::Builder::new()
            .prefix("auth-keychain-test-")
            .tempdir()
            .unwrap();
        let key = Cleanup(
            directory
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
        );
        let store = MacKeychain;
        assert_eq!(store.load(&key.0).unwrap(), None);
        store.save(&key.0, "sample refresh token").unwrap();
        assert_eq!(
            store.load(&key.0).unwrap().as_deref(),
            Some("sample refresh token")
        );
        store.save(&key.0, "sample rotated token").unwrap();
        assert_eq!(
            store.load(&key.0).unwrap().as_deref(),
            Some("sample rotated token")
        );
        store.remove(&key.0).unwrap();
        store.remove(&key.0).unwrap();
        assert_eq!(store.load(&key.0).unwrap(), None);
    }
}
