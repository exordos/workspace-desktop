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
    default_directory()
}

/// Возвращает каталог данных macOS, сохраняя путь существующих установок.
#[cfg(target_os = "macos")]
fn default_directory() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "Home directory unavailable"))?;
    Ok(PathBuf::from(home).join("Library/Application Support/Workspace Desktop/auth"))
}

/// Возвращает локальный каталог данных текущего пользователя Windows.
#[cfg(target_os = "windows")]
fn default_directory() -> io::Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("Workspace Desktop/auth"))
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "Local app data directory unavailable"))
}

/// Использует XDG_DATA_HOME или стандартный пользовательский каталог Linux.
#[cfg(target_os = "linux")]
fn default_directory() -> io::Result<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            let home = PathBuf::from(std::env::var_os("HOME")?);
            home.is_absolute().then(|| home.join(".local/share"))
        })
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "User data directory unavailable"))?;
    Ok(data.join("workspace-desktop/auth"))
}
