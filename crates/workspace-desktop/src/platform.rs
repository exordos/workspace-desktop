//! Связывает хранилища авторизации с операционной системой.
//! Здесь хранится список аккаунтов в JSON. Работа с секретами macOS вынесена в keychain.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use workspace_core::auth::{AccountRecord, AccountStore, StoreError};
use workspace_core::context::AccountId;

#[cfg(target_os = "macos")]
mod profile;

#[cfg(target_os = "macos")]
pub use profile::state_directory;

/// Хранилище записей аккаунтов в одном JSON-файле.
pub struct JsonAccountStore {
    path: PathBuf,         // Путь к файлу с записями аккаунтов.
    _process_lock: File, // Открытый файл, который не даёт второму процессу менять записи одновременно.
    write_lock: Mutex<()>, // Блокировка записи между потоками одного процесса.
}

impl JsonAccountStore {
    /// Создаёт каталог и пытается получить исключительный доступ к хранилищу.
    pub fn open(directory: &Path) -> io::Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(directory)?;

        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(directory.join("accounts.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "another Workspace auth process is using the account store",
                ));
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }

        Ok(Self {
            path: directory.join("accounts.json"),
            _process_lock: lock,
            write_lock: Mutex::new(()),
        })
    }

    /// Читает JSON. Если файла ещё нет, возвращает пустой список.
    fn read(&self) -> Result<Vec<AccountRecord>, StoreError> {
        match File::open(&self.path) {
            Ok(file) => serde_json::from_reader(io::BufReader::new(file)).map_err(|_| StoreError),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(_) => Err(StoreError),
        }
    }

    /// Пишет данные во временный файл и заменяет им основной после успешной записи.
    fn write(&self, records: &[AccountRecord]) -> Result<(), StoreError> {
        let directory = self.path.parent().ok_or(StoreError)?;
        let mut file = tempfile::NamedTempFile::new_in(directory).map_err(|_| StoreError)?;
        serde_json::to_writer(file.as_file_mut(), records).map_err(|_| StoreError)?;
        file.flush().map_err(|_| StoreError)?;
        file.as_file().sync_all().map_err(|_| StoreError)?;
        file.persist(&self.path).map_err(|_| StoreError)?;
        // Переименование уже заменило запись. Сбой синхронизации каталога её не отменит.
        let _ = File::open(directory).and_then(|directory| directory.sync_all());
        Ok(())
    }
}

impl AccountStore for JsonAccountStore {
    /// Загружает сохранённые записи аккаунтов.
    fn load(&self) -> Result<Vec<AccountRecord>, StoreError> {
        self.read()
    }

    /// Под блокировкой добавляет или заменяет одну запись аккаунта.
    fn save(&self, record: &AccountRecord) -> Result<(), StoreError> {
        let _guard = self.write_lock.lock().map_err(|_| StoreError)?;
        let mut records = self.read()?;
        if let Some(existing) = records
            .iter_mut()
            .find(|existing| existing.context.account_id == record.context.account_id)
        {
            *existing = record.clone();
        } else {
            records.push(record.clone());
        }
        self.write(&records)
    }

    /// Под блокировкой удаляет аккаунт. Если его уже нет, запись файла не нужна.
    fn remove(&self, account_id: &AccountId) -> Result<(), StoreError> {
        let _guard = self.write_lock.lock().map_err(|_| StoreError)?;
        let mut records = self.read()?;
        let previous_count = records.len();
        records.retain(|record| &record.context.account_id != account_id);
        if previous_count == records.len() {
            return Ok(());
        }
        self.write(&records)
    }
}

#[cfg(target_os = "macos")]
mod keychain;

#[cfg(target_os = "macos")]
pub use keychain::MacKeychain;

#[cfg(feature = "ui-kit")]
pub mod notifications;

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_core::context::OwnerContext;

    /// Готовит запись аккаунта с заданным идентификатором для тестов.
    fn account(id: &str) -> AccountRecord {
        AccountRecord {
            context: OwnerContext {
                account_id: AccountId(id.to_owned()),
                server: "https://workspace.example.test".to_owned(),
                organization_id: "test-organization".to_owned(),
                project_id: "test-project".to_owned(),
                user_id: id.to_owned(),
            },
            secret_key: format!("key-{id}"),
            signed_out: false,
            retired_secret_keys: Vec::new(),
        }
    }

    /// Проверяет сохранение изменений и удаления между открытиями хранилища.
    #[test]
    fn persists_account_updates_and_removal_across_opens() {
        let directory = tempfile::tempdir().unwrap();
        let first = account("first");
        let mut second = account("second");

        {
            let store = JsonAccountStore::open(directory.path()).unwrap();
            store.save(&first).unwrap();
            store.save(&second).unwrap();
            second.retired_secret_keys.push(second.secret_key.clone());
            second.secret_key = "key-second-new".to_owned();
            second.signed_out = true;
            store.save(&second).unwrap();
        }

        let store = JsonAccountStore::open(directory.path()).unwrap();
        assert_eq!(store.load().unwrap(), vec![first.clone(), second.clone()]);
        store.remove(&first.context.account_id).unwrap();
        store.remove(&first.context.account_id).unwrap();
        drop(store);

        let store = JsonAccountStore::open(directory.path()).unwrap();
        assert_eq!(store.load().unwrap(), vec![second]);
    }

    /// Проверяет, что второй экземпляр не получит доступ до закрытия первого.
    #[test]
    fn refuses_second_process_lock_until_first_store_closes() {
        let directory = tempfile::tempdir().unwrap();
        let first = JsonAccountStore::open(directory.path()).unwrap();
        let Err(error) = JsonAccountStore::open(directory.path()) else {
            panic!("a second writer acquired the account lock");
        };
        assert_eq!(error.kind(), ErrorKind::WouldBlock);
        drop(first);
        assert!(JsonAccountStore::open(directory.path()).is_ok());
    }

    /// Проверяет, что повреждённый JSON не затирается при неудачном обновлении.
    #[test]
    fn invalid_metadata_is_preserved_if_update_cannot_read_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("accounts.json");
        fs::write(&path, b"invalid metadata").unwrap();
        let store = JsonAccountStore::open(directory.path()).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(&account("first")).is_err());
        assert_eq!(fs::read(path).unwrap(), b"invalid metadata");
    }
}
