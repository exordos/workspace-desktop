//! Определяет общий каталог профиля для окна и команд авторизации.

use std::io::{self, ErrorKind};
use std::path::PathBuf;

/// Возвращает указанный каталог профиля или стандартный каталог данных приложения.
pub fn state_directory() -> io::Result<PathBuf> {
    if let Some(directory) = std::env::var_os("WORKSPACE_AUTH_STATE_DIR") {
        if directory.is_empty() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "WORKSPACE_AUTH_STATE_DIR must name a directory",
            ));
        }
        return Ok(PathBuf::from(directory));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "Home directory unavailable"))?;
    Ok(PathBuf::from(home).join("Library/Application Support/Workspace Desktop/auth"))
}
