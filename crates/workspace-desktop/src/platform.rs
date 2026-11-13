use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use workspace_core::auth::{AccountRecord, AccountStore, StoreError};
use workspace_core::context::AccountId;

pub struct JsonAccountStore {
    path: PathBuf,
    _process_lock: File,
    write_lock: Mutex<()>,
}

impl JsonAccountStore {
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

    fn read(&self) -> Result<Vec<AccountRecord>, StoreError> {
        match File::open(&self.path) {
            Ok(file) => serde_json::from_reader(io::BufReader::new(file)).map_err(|_| StoreError),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(_) => Err(StoreError),
        }
    }

    fn write(&self, records: &[AccountRecord]) -> Result<(), StoreError> {
        let directory = self.path.parent().ok_or(StoreError)?;
        let mut file = tempfile::NamedTempFile::new_in(directory).map_err(|_| StoreError)?;
        serde_json::to_writer(file.as_file_mut(), records).map_err(|_| StoreError)?;
        file.flush().map_err(|_| StoreError)?;
        file.as_file().sync_all().map_err(|_| StoreError)?;
        file.persist(&self.path).map_err(|_| StoreError)?;
        // Rename committed the new record. A directory sync failure cannot undo it.
        let _ = File::open(directory).and_then(|directory| directory.sync_all());
        Ok(())
    }
}

impl AccountStore for JsonAccountStore {
    fn load(&self) -> Result<Vec<AccountRecord>, StoreError> {
        self.read()
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_core::context::OwnerContext;

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
