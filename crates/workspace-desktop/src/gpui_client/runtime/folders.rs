//! Связывает окно с папками Core. Здесь находятся команды, снимки для окна
//! и локальные настройки панели в каталоге профиля.

use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{self, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::{mpsc, watch};
use workspace_core::{
    auth::SessionStatus,
    context::OwnerContext,
    messenger::folders::{FolderChanges, FolderError},
    runtime::SourceError,
};

use super::{AccountSession, AccountSnapshot, RuntimeClient, RuntimeEvent, WorkerServices};
use crate::gpui_client::contracts::{FolderCommand, FolderCommandError, FolderRailPreferences};

impl RuntimeClient {
    /// Передаёт команду Core для указанного входа и возвращает подтверждённый результат.
    pub async fn folder_command(
        &self,
        session: AccountSession,
        command: FolderCommand,
    ) -> Result<(), FolderCommandError> {
        let is_mutation = !matches!(&command, FolderCommand::Retry);
        let started = Arc::new(AtomicBool::new(false));
        let job_started = started.clone();
        let reply = self
            .enqueue(move |services, reply| async move {
                if reply.is_closed() {
                    return;
                }
                job_started.store(true, Ordering::Release);
                // Смена экрана может закрыть получателя. Уже начатая команда
                // всё равно завершается и ставит запрос на сверку в Core.
                let result = run_command(services, session, command).await;
                let _ = reply.send(result);
            })
            .map_err(|_| FolderCommandError::WorkerUnavailable)?;
        reply
            .await
            .map_err(|_| {
                if is_mutation && started.load(Ordering::Acquire) {
                    FolderCommandError::Unknown
                } else {
                    FolderCommandError::WorkerUnavailable
                }
            })?
            .map_err(FolderCommandError::from)
    }

    /// Сохраняет вид панели для того входа, в котором пользователь поменял настройку.
    pub async fn save_folder_preferences(
        &self,
        session: AccountSession,
        preferences: FolderRailPreferences,
    ) -> Result<(), FolderCommandError> {
        self.request(move |services| async move {
            if !services.auth.snapshot().iter().any(|current| {
                current.context == session.owner
                    && current.generation == session.generation
                    && (current.status == SessionStatus::Active
                        || (current.status == SessionStatus::TemporarilyUnavailable
                            && current.token_version != 0))
            }) {
                return Err(FolderCommandError::Unavailable);
            }
            services
                .folder_preferences
                .save(&session.owner, preferences)
                .map_err(|error| {
                    log::warn!(
                        "event=workspace_folder_preferences_save result=failed kind={:?}",
                        error.kind()
                    );
                    FolderCommandError::WorkerUnavailable
                })
        })
        .await
        .map_err(|_| FolderCommandError::WorkerUnavailable)?
    }
}

impl From<FolderError> for FolderCommandError {
    /// Выбирает безопасную причину для окна без ответов API и данных профиля.
    fn from(error: FolderError) -> Self {
        match error {
            FolderError::Unknown => Self::Unknown,
            FolderError::Refresh(_) => Self::Refresh,
            FolderError::Source(SourceError::Temporary) => Self::WorkerUnavailable,
            FolderError::Auth(_)
            | FolderError::Source(_)
            | FolderError::Storage(_)
            | FolderError::Unavailable
            | FolderError::Rejected(401 | 403 | 404) => Self::Unavailable,
            FolderError::InvalidChanges
            | FolderError::InvalidFolderId
            | FolderError::ReadOnly
            | FolderError::Rejected(_) => Self::Rejected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Временный сбой проверки данных оставляет форму доступной для повторной отправки.
    #[test]
    fn temporary_preflight_failure_can_be_retried() {
        assert_eq!(
            FolderCommandError::from(FolderError::Source(SourceError::Temporary)),
            FolderCommandError::WorkerUnavailable
        );
        assert_eq!(
            FolderCommandError::from(FolderError::Unavailable),
            FolderCommandError::Unavailable
        );
    }
}

/// Применяет команду к сессии, захваченной окном до запуска операции.
async fn run_command(
    services: WorkerServices,
    session: AccountSession,
    command: FolderCommand,
) -> Result<(), FolderError> {
    let AccountSession { owner, generation } = session;
    let auth = &services.auth;
    let folders = &services.folders;
    match command {
        FolderCommand::Retry => folders.retry(auth, &owner, generation),
        FolderCommand::Create { id, title, color } => folders
            .create_folder(auth, &owner, generation, id, &title, color)
            .await
            .map(|_| ()),
        FolderCommand::Update { id, title, color } => {
            let changes = FolderChanges {
                title,
                background_color_value: color,
            };
            folders
                .update_folder(auth, &owner, generation, id, changes)
                .await
        }
        FolderCommand::Delete(id) => folders.delete_folder(auth, &owner, generation, id).await,
        FolderCommand::Add { folder, chat } => folders
            .add_chat(auth, &owner, generation, folder, chat)
            .await
            .map(|_| ()),
        FolderCommand::Remove { folder, item } => {
            folders
                .remove_chat(auth, &owner, generation, folder, item)
                .await
        }
        FolderCommand::Pin {
            folder,
            item,
            pinned,
        } => {
            folders
                .set_pinned(auth, &owner, generation, folder, item, pinned)
                .await
        }
    }
}

/// Читает папки для поколений, которые окно уже получило в событии аккаунтов.
pub(super) fn folder_snapshot(
    accounts: &[AccountSnapshot],
    services: &WorkerServices,
) -> RuntimeEvent {
    let snapshots = accounts
        .iter()
        .filter(|account| can_read_folders(account.status))
        .filter_map(|account| {
            let AccountSession { owner, generation } = &account.session;
            match services
                .folders
                .snapshot(&services.auth, owner, *generation)
            {
                Ok(snapshot) => Some((owner.clone(), Arc::new(snapshot))),
                Err(FolderError::Auth(_)) => None,
                Err(_) => {
                    log::debug!("event=workspace_folder_snapshot result=unavailable");
                    None
                }
            }
        })
        .collect();
    RuntimeEvent::Folders(snapshots)
}

/// Передаёт вид панели и папки сразу после полного снимка аккаунтов.
pub(super) async fn send_folder_state(
    accounts: &[AccountSnapshot],
    services: &WorkerServices,
    events: &mpsc::Sender<RuntimeEvent>,
) -> bool {
    events
        .send(services.folder_preferences.snapshot(accounts))
        .await
        .is_ok()
        && events
            .send(folder_snapshot(accounts, services))
            .await
            .is_ok()
}

/// Отбирает действующие входы. Core дополнительно проверяет поколение и подтверждение.
fn can_read_folders(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Active | SessionStatus::TemporarilyUnavailable
    )
}

/// Хранит пользовательские настройки панели отдельно от синхронизируемых папок.
#[derive(Default)]
pub(super) struct FolderPreferenceStore {
    path: Option<PathBuf>, // Файл профиля. None оставляет настройки только в памяти.
    values: Mutex<HashMap<OwnerContext, FolderRailPreferences>>, // Настройки каждого аккаунта и проекта.
    changed: watch::Sender<()>, // Сигнал после сохранения настроек.
}

impl FolderPreferenceStore {
    /// Читает настройки из профиля. При первом запуске и ошибке чтения берёт значения по умолчанию.
    pub(super) fn open(directory: &Path) -> Self {
        let path = directory.join("folder-rail.json");
        let values = Self::read(&path).unwrap_or_else(|error| {
            if error.kind() != io::ErrorKind::NotFound {
                log::warn!(
                    "event=workspace_folder_preferences_load result=failed kind={:?}",
                    error.kind()
                );
            }
            HashMap::new()
        });
        Self {
            path: Some(path),
            values: Mutex::new(values),
            changed: watch::Sender::default(),
        }
    }

    /// Даёт мосту сигнал о подтверждённой записи настроек.
    pub(super) fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    /// Собирает настройки показанных входов. Первый вход получает значения по умолчанию.
    pub(super) fn snapshot(&self, accounts: &[AccountSnapshot]) -> RuntimeEvent {
        let values = self
            .values
            .lock()
            .expect("блокировка настроек панели папок");
        let preferences = accounts
            .iter()
            .filter(|account| can_read_folders(account.status))
            .map(|account| {
                let owner = &account.session.owner;
                (
                    owner.clone(),
                    values.get(owner).copied().unwrap_or_default(),
                )
            })
            .collect();
        RuntimeEvent::FolderPreferences(preferences)
    }

    /// Атомарно заменяет файл профиля и затем текущие настройки в памяти.
    fn save(&self, owner: &OwnerContext, preferences: FolderRailPreferences) -> io::Result<()> {
        let mut values = self
            .values
            .lock()
            .expect("блокировка настроек панели папок");
        if values.get(owner).copied().unwrap_or_default() == preferences {
            return Ok(());
        }
        if let Some(path) = &self.path {
            let mut next = values.clone();
            next.insert(owner.clone(), preferences);
            Self::write(path, &next)?;
            *values = next;
        } else {
            values.insert(owner.clone(), preferences);
        }
        drop(values);
        self.changed.send_replace(());
        Ok(())
    }

    /// Читает именованные флаги для каждого владельца.
    fn read(path: &Path) -> io::Result<HashMap<OwnerContext, FolderRailPreferences>> {
        let rows: Vec<(OwnerContext, BTreeMap<String, bool>)> =
            serde_json::from_reader(BufReader::new(File::open(path)?))?;
        Ok(rows
            .into_iter()
            .map(|(owner, flags)| {
                let defaults = FolderRailPreferences::default();
                (
                    owner,
                    FolderRailPreferences {
                        vertical: flags.get("vertical").copied().unwrap_or(defaults.vertical),
                        show_system: flags
                            .get("show_system")
                            .copied()
                            .unwrap_or(defaults.show_system),
                    },
                )
            })
            .collect())
    }

    /// Записывает новый снимок во временный файл и заменяет им предыдущий после записи.
    fn write(path: &Path, values: &HashMap<OwnerContext, FolderRailPreferences>) -> io::Result<()> {
        let directory = path.parent().expect("каталог профиля с настройками панели");
        let rows: Vec<_> = values
            .iter()
            .map(|(owner, value)| {
                (
                    owner,
                    BTreeMap::from([
                        ("vertical", value.vertical),
                        ("show_system", value.show_system),
                    ]),
                )
            })
            .collect();
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(file.as_file_mut(), &rows)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|error| error.error)?;
        // Файл уже заменён. Сбой синхронизации каталога не отменит сохранение.
        let _ = File::open(directory).and_then(|directory| directory.sync_all());
        Ok(())
    }
}
