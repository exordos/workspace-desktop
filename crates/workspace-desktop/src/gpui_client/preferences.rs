//! Локальные настройки приложения. Читает общий файл профиля и атомарно сохраняет выбор.

use std::{
    collections::{BTreeMap, HashMap},
    fs, io,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};

use gpui::{App, BorrowAppContext, Global};
use serde::{Deserialize, Serialize};
use workspace_core::context::OwnerContext;

pub(super) use super::i18n::Language;

pub(super) mod appearance;
pub(super) mod shortcuts;

const FILE: &str = "preferences.json";
static DEFAULTS: LazyLock<PreferenceValues> = LazyLock::new(PreferenceValues::default);

/// Действие основной области строки канала в каталоге.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum ChatClick {
    #[default]
    General, // Открыть общую тему.
    LatestTopic, // Открыть тему последнего сообщения, если она доступна.
}

/// Варианты звука локальных уведомлений.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum NotificationSound {
    #[default]
    Default, // Стандартный тон.
    Subtle,  // Более тихий и низкий тон.
    Digital, // Два коротких тона.
    Glass,   // Короткий высокий тон.
    Pulse,   // Два импульса с паузой.
    None,    // Звук выключен.
}

/// Через какое время без действий нужно завершить локальные сессии.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum IdleTimeout {
    SixHours,    // 6 часов.
    TwelveHours, // 12 часов.
    Day,         // 24 часа.
    #[default]
    ThreeDays, // 72 часа.
    Week,        // 7 дней.
    Never,       // Автовыход выключен.
}

impl IdleTimeout {
    /// Возвращает интервал в часах, превращённый в Duration, или None при выключенном выходе.
    pub(super) fn duration(self) -> Option<Duration> {
        let hours = match self {
            Self::SixHours => 6,
            Self::TwelveHours => 12,
            Self::Day => 24,
            Self::ThreeDays => 72,
            Self::Week => 168,
            Self::Never => return None,
        };
        Some(Duration::from_secs(hours * 3600))
    }
}

/// Настройки одного аккаунта и проекта в локальном профиле.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct PreferenceValues {
    pub(super) language: Language,                        // Язык подписей.
    pub(super) chat_click: ChatClick,                     // Маршрут строки канала.
    pub(super) notification_sound: NotificationSound,     // Звук нового сообщения.
    pub(super) dock_bounce: bool,                         // Нужно привлечь внимание значком в Dock.
    pub(super) separate_call_window: bool, // Звонок открывается в дополнительном окне.
    pub(super) idle_timeout: IdleTimeout,  // Время до автовыхода.
    pub(super) hotkeys_enabled: bool,      // Включены изменяемые сочетания.
    pub(super) hotkeys: BTreeMap<String, Option<String>>, // Нет записи — стандартное сочетание; None — выключено.
    pub(super) appearance: appearance::AppearancePreferences, // Тема и вид мессенджера.
    pub(super) manual_away: Option<bool>, // Последний явный выбор отсутствия. None — пользователь ещё не выбирал.
}

impl Default for PreferenceValues {
    /// Возвращает исходный выбор Electron для нового профиля.
    fn default() -> Self {
        Self {
            language: Language::default(),
            chat_click: ChatClick::default(),
            notification_sound: NotificationSound::default(),
            dock_bounce: true,
            separate_call_window: true,
            idle_timeout: IdleTimeout::default(),
            hotkeys_enabled: true,
            hotkeys: BTreeMap::new(),
            appearance: appearance::AppearancePreferences::default(),
            manual_away: None,
        }
    }
}

/// Общий снимок приложения, который можно читать из любого GPUI-экрана.
pub(super) struct Preferences {
    path: Option<PathBuf>, // None хранит значения только в памяти тестового App.
    owner: Option<OwnerContext>, // Владелец активного окна или None до входа.
    values: HashMap<Option<OwnerContext>, PreferenceValues>, // Последние сохранённые снимки по владельцам.
}

impl Global for Preferences {}

impl Preferences {
    /// Читает настройки один раз до открытия окна.
    pub(super) fn install(profile: &Path, app: &mut App) {
        let path = profile.join(FILE);
        app.set_global(Self {
            values: read_file(&path),
            path: Some(path),
            owner: None,
        });
    }

    /// Берёт текущие значения или исходный выбор в галерее и тестовом окне.
    pub(super) fn read(app: &App) -> &PreferenceValues {
        app.try_global::<Self>()
            .and_then(|store| store.values.get(&store.owner))
            .unwrap_or(&DEFAULTS)
    }

    /// Читает выбор фонового аккаунта, например перед показом его уведомления.
    pub(super) fn read_for<'a>(app: &'a App, owner: &OwnerContext) -> &'a PreferenceValues {
        app.try_global::<Self>()
            .and_then(|store| store.values.get(&Some(owner.clone())))
            .unwrap_or(&DEFAULTS)
    }

    /// Проверяет, что действие старой панели всё ещё относится к владельцу настроек.
    pub(super) fn is_selected(app: &App, owner: &OwnerContext) -> bool {
        app.try_global::<Self>()
            .is_some_and(|store| store.owner.as_ref() == Some(owner))
    }

    /// Переключает настройки вместе с выбранным входом в рабочем окне.
    pub(super) fn select(app: &mut App, owner: Option<&OwnerContext>) {
        if app
            .try_global::<Self>()
            .is_some_and(|store| store.owner.as_ref() == owner)
        {
            return;
        }
        app.update_default_global::<Self, _>(|store, _| store.owner = owner.cloned());
    }

    /// Публикует новый снимок после успешной записи. Старое значение доступно при сбое.
    pub(super) fn update(
        app: &mut App,
        change: impl FnOnce(&mut PreferenceValues),
    ) -> io::Result<()> {
        let (owner, mut values) = app
            .try_global::<Self>()
            .map(|store| (store.owner.clone(), store.values.clone()))
            .unwrap_or_default();
        change(values.entry(owner).or_default());
        Self::save(app, values)
    }

    /// Возвращает исходный выбор для всех локальных аккаунтов.
    pub(super) fn reset(app: &mut App) -> io::Result<()> {
        Self::save(app, HashMap::new())
    }

    /// Заменяет записи профиля и посылает подписчикам новый снимок.
    fn save(
        app: &mut App,
        values: HashMap<Option<OwnerContext>, PreferenceValues>,
    ) -> io::Result<()> {
        if let Some(path) = app
            .try_global::<Self>()
            .and_then(|store| store.path.as_deref())
        {
            persist(path, &values)?;
        }
        app.update_default_global::<Self, _>(|store, _| store.values = values);
        Ok(())
    }
}

impl Default for Preferences {
    /// Создаёт настройки в памяти для запуска без отдельного профиля.
    fn default() -> Self {
        Self {
            path: None,
            owner: None,
            values: HashMap::new(),
        }
    }
}

/// Читает сохранённое значение или возвращает исходный выбор для нового профиля.
fn read_file(path: &Path) -> HashMap<Option<OwnerContext>, PreferenceValues> {
    fs::read(path)
        .ok()
        .and_then(|bytes| {
            serde_json::from_slice::<Vec<(Option<OwnerContext>, PreferenceValues)>>(&bytes).ok()
        })
        .map(|rows| rows.into_iter().collect())
        .unwrap_or_default()
}

/// Заменяет прежний файл только после успешной записи рядом с ним.
fn persist(
    path: &Path,
    values: &HashMap<Option<OwnerContext>, PreferenceValues>,
) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("Нет каталога профиля"))?;
    fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(file.as_file_mut(), &values.iter().collect::<Vec<_>>())?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет восстановление выбора и исходные значения полей, которых нет в старом файле.
    #[test]
    fn restores_local_values_after_restart() {
        let profile = tempfile::tempdir().unwrap();
        let path = profile.path().join(FILE);
        assert!(read_file(&path).is_empty());
        fs::write(&path, br#"[[null,{"language":"en","dockBounce":false}]]"#).unwrap();
        let mut records = read_file(&path);
        let values = records.get_mut(&None).unwrap();
        assert_eq!(values.language, Language::En);
        assert!(!values.dock_bounce);
        assert!(values.hotkeys_enabled);
        values.chat_click = ChatClick::LatestTopic;
        values.hotkeys.insert("navigation.chatSearch".into(), None);
        persist(&path, &records).unwrap();
        let restored = read_file(&path).remove(&None).unwrap();
        assert_eq!(restored.language, Language::En);
        assert_eq!(restored.chat_click, ChatClick::LatestTopic);
        assert_eq!(restored.hotkeys, records[&None].hotkeys);
    }

    /// Недоступный язык не мешает загрузить остальные настройки со старого профиля.
    #[test]
    fn restores_other_preferences_when_catalog_is_unavailable() {
        let profile = tempfile::tempdir().unwrap();
        let path = profile.path().join(FILE);
        fs::write(
            &path,
            br#"[[null,{"language":"xx-x-missing","dockBounce":false}]]"#,
        )
        .unwrap();

        let restored = read_file(&path).remove(&None).unwrap();
        assert_eq!(restored.language, Language::Ru);
        assert!(!restored.dock_bounce);
    }
}
