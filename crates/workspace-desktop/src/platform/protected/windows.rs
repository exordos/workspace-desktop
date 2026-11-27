//! Шифрует секреты для текущего пользователя Windows и атомарно заменяет файлы профиля.

use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};

use sha2::{Digest, Sha256};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};
use workspace_core::auth::StoreError;
use zeroize::{Zeroize, Zeroizing};

/// Зашифрованные записи под каталогом одного профиля и одного вида секретов.
pub(super) struct Records {
    directory: PathBuf, // Каталог, где лежат только зашифрованные DPAPI файлы.
}

impl Records {
    /// Запоминает каталог записей. Каталог создаётся при первой записи.
    pub(super) fn new(profile: &Path, service: &'static str) -> Self {
        Self {
            directory: profile.join("protected").join(service),
        }
    }

    /// Назначает стабильное имя файла независимо от символов в ключе.
    fn path(&self, key: &str) -> PathBuf {
        self.directory
            .join(format!("{:x}", Sha256::digest(key.as_bytes())))
    }

    /// Читает запись и расшифровывает её в памяти текущего пользователя.
    pub(super) fn read(&self, key: &str) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        match fs::read(self.path(key)) {
            Ok(data) => dpapi(&data, false).map(Some),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(_) => Err(StoreError),
        }
    }

    /// Шифрует секрет до записи на диск и заменяет файл после успешной записи.
    pub(super) fn write(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        let encrypted = dpapi(value, true)?;
        fs::create_dir_all(&self.directory).map_err(|_| StoreError)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.directory).map_err(|_| StoreError)?;
        file.write_all(&encrypted).map_err(|_| StoreError)?;
        file.as_file().sync_all().map_err(|_| StoreError)?;
        file.persist(self.path(key)).map_err(|_| StoreError)?;
        Ok(())
    }

    /// Удаляет зашифрованный файл, считая отсутствие успешной очисткой.
    pub(super) fn remove(&self, key: &str) -> Result<(), StoreError> {
        match fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(_) => Err(StoreError),
        }
    }
}

/// Выполняет DPAPI-операцию без диалога и освобождает выделенный Windows буфер.
fn dpapi(source: &[u8], encrypt: bool) -> Result<Zeroizing<Vec<u8>>, StoreError> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: source.len().try_into().map_err(|_| StoreError)?,
        pbData: source.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };
    // DPAPI читает вход до возврата. При успехе система передаёт вызывающему выходной буфер.
    let success = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if success == 0 {
        return Err(StoreError);
    }
    if output.pbData.is_null() {
        return Ok(Zeroizing::new(Vec::new()));
    }
    // Успешный DPAPI-вызов вернул буфер длины cbData. Очищаем его перед LocalFree.
    let data = unsafe {
        let bytes = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let data = Zeroizing::new(bytes.to_vec());
        bytes.zeroize();
        LocalFree(output.pbData.cast());
        data
    };
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет чтение после открытия профиля, замену и повторное удаление секрета.
    #[test]
    fn persists_encrypted_records_for_current_user() {
        let profile = tempfile::tempdir().unwrap();
        let records = Records::new(profile.path(), "test");
        let secret = b"sample refresh token";
        assert!(records.read("account").unwrap().is_none());
        records.write("account", secret).unwrap();
        let stored = fs::read(records.path("account")).unwrap();
        assert!(!stored.windows(secret.len()).any(|bytes| bytes == secret));

        let records = Records::new(profile.path(), "test");
        assert_eq!(records.read("account").unwrap().unwrap().as_slice(), secret);
        records.write("account", b"rotated").unwrap();
        assert_eq!(
            records.read("account").unwrap().unwrap().as_slice(),
            b"rotated"
        );
        records.remove("account").unwrap();
        records.remove("account").unwrap();
        assert!(records.read("account").unwrap().is_none());
    }
}
