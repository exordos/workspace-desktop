use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use workspace_core::auth::{SecretStore, StoreError};

const SERVICE: &str = "com.workspace.desktop.auth";
const ITEM_NOT_FOUND: i32 = -25300;

pub struct MacKeychain;

impl SecretStore for MacKeychain {
    fn load(&self, key: &str) -> Result<Option<String>, StoreError> {
        match get_generic_password(SERVICE, key) {
            Ok(value) => String::from_utf8(value).map(Some).map_err(|_| StoreError),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
            Err(_) => Err(StoreError),
        }
    }

    fn save(&self, key: &str, value: &str) -> Result<(), StoreError> {
        set_generic_password(SERVICE, key, value.as_bytes()).map_err(|_| StoreError)
    }

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

    #[test]
    #[ignore = "requires an unlocked macOS login Keychain"]
    fn keychain_round_trip() {
        struct Cleanup(String);
        impl Drop for Cleanup {
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
