//! Запоминает раскрытие инструментов редактора в локальном профиле.

use std::{
    io,
    path::{Path, PathBuf},
};

use gpui::{App, Global};

const FILE_NAME: &str = "composer-toolbar.json";

/// Параметр текущего профиля, доступный всем разговорам в одном приложении.
pub(super) struct ToolbarPreferences {
    path: Option<PathBuf>, // None у галереи и тестов, где состояние хранится в памяти.
    expanded: bool,        // Текущий выбор пользователя в открытом профиле.
}

impl Global for ToolbarPreferences {}

impl ToolbarPreferences {
    /// Считывает небольшой файл профиля до открытия рабочего окна.
    pub(super) fn install(profile: &Path, app: &mut App) {
        let path = profile.join(FILE_NAME);
        app.set_global(Self {
            expanded: read(&path),
            path: Some(path),
        });
    }

    /// Берёт сохранённый выбор или компактный вид для первого запуска.
    pub(super) fn expanded(app: &App) -> bool {
        app.try_global::<Self>()
            .is_some_and(|settings| settings.expanded)
    }

    /// Сохраняет новый выбор для следующего разговора и запуска.
    pub(super) fn set_expanded(expanded: bool, app: &mut App) {
        let path = app
            .try_global::<Self>()
            .and_then(|state| state.path.clone());
        if let Some(path) = path.as_deref()
            && let Err(error) = persist(path, expanded)
        {
            log::warn!(
                "event=workspace_composer_toolbar_save result=failed kind={:?}",
                error.kind()
            );
        }
        app.set_global(Self { path, expanded });
    }
}

/// Возвращает компактный вид, если файл отсутствует или повреждён.
fn read(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Записывает файл рядом с прежним и заменяет его после успешного сохранения.
fn persist(path: &Path, expanded: bool) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("Нет каталога профиля"))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(file.as_file_mut(), &expanded)?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет восстановление старого и повреждённого профиля и запись рабочего выбора.
    #[test]
    fn restores_expanded_panel_from_profile() {
        let profile = tempfile::tempdir().unwrap();
        let path = profile.path().join(FILE_NAME);
        assert!(!read(&path));
        persist(&path, true).unwrap();
        assert!(read(&path));
        persist(&path, false).unwrap();
        assert!(!read(&path));
        std::fs::write(&path, b"broken").unwrap();
        assert!(!read(&path));
    }
}
