//! Хранит обычный размер окна в window-size.json и объединяет обновления при перетаскивании.

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use gpui::{App, Context, Pixels, Size, Subscription, Task, Window, WindowBounds, px, size};
use tokio::sync::watch;

use super::NativeWindow;

const FILE_NAME: &str = "window-size.json";
const DEFAULT_SIZE: [f32; 2] = [MIN_WIDTH, MIN_HEIGHT];
pub(super) const MIN_WIDTH: f32 = 700.;
pub(super) const MIN_HEIGHT: f32 = 600.;

/// Последний обычный размер и сигнал завершения его фоновой записи.
#[derive(Clone, Copy, Default)]
pub(super) struct SizeUpdate {
    size: Option<[f32; 2]>, // Последние ширина и высота области содержимого в логических пикселях.
    closing: bool,          // После последней записи фоновый исполнитель завершится.
}

/// Сохраняет изменения по порядку на одном фоновом исполнителе.
pub(super) struct SizeWriter {
    updates: watch::Sender<SizeUpdate>, // Передаёт свежий снимок и команду завершения.
    task: Task<()>,                     // Заканчивается после записи последнего принятого размера.
}

impl SizeWriter {
    /// Открывает канал записи для файла текущего профиля.
    pub(super) fn new(profile: &Path, app: &App) -> Self {
        let (updates, receiver) = watch::channel(SizeUpdate::default());
        let path = profile.join(FILE_NAME);
        let task = app
            .background_executor()
            .spawn(write_updates(path, receiver));
        Self { updates, task }
    }

    /// Даёт подписке окна канал для передачи обычного размера.
    pub(super) fn updates(&self) -> watch::Sender<SizeUpdate> {
        self.updates.clone()
    }

    /// Запоминает размер перед выходом, пока окно ещё открыто.
    pub(super) fn record(&self, window: &Window, native: Option<&NativeWindow>) {
        record(window, native, &self.updates);
    }

    /// Просит записать последний принятый размер и ждёт завершения записи.
    pub(super) async fn finish(self) {
        self.updates.send_modify(|update| update.closing = true);
        self.task.await;
    }
}

/// Владеет подпиской окна на изменения размера.
pub(super) struct SizeObserver {
    _bounds: Subscription,     // Активна до закрытия главного окна.
    pending: Option<Task<()>>, // Ждёт завершения текущего изменения геометрии.
}

impl SizeObserver {
    /// Следит за размером и проверяет режим после завершения изменения геометрии.
    pub(super) fn new(
        window: &mut Window,
        native: Option<NativeWindow>,
        updates: watch::Sender<SizeUpdate>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _bounds = cx.observe_window_bounds(window, move |observer, window, cx| {
            let native = native.clone();
            let updates = updates.clone();
            observer.pending = Some(window.spawn(cx, async move |window| {
                // AppKit сообщает конечный режим Zoom после уведомлений о самом изменении размера.
                window
                    .background_executor()
                    .timer(Duration::from_millis(180))
                    .await;
                let _ = window.update(|window, _| record(window, native.as_ref(), &updates));
            }));
        });
        Self {
            _bounds,
            pending: None,
        }
    }
}

/// Принимает размер только из видимого окна в обычном режиме.
pub(super) fn record(
    window: &Window,
    _native: Option<&NativeWindow>,
    updates: &watch::Sender<SizeUpdate>,
) {
    if !matches!(window.window_bounds(), WindowBounds::Windowed(_))
        || window.is_maximized()
    {
        return;
    }
    #[cfg(target_os = "macos")]
    if _native.is_some_and(|native| !native.is_ordinary()) {
        return;
    }
    let size = window.viewport_size();
    let value = [size.width.into(), size.height.into()];
    if valid(value) {
        updates.send_if_modified(|current| {
            if current.closing || current.size == Some(value) {
                return false;
            }
            current.size = Some(value);
            true
        });
    }
}

/// Возвращает обычный размер из профиля или размер первого запуска.
pub(super) fn load(profile: &Path) -> Size<Pixels> {
    let [width, height] = read(&profile.join(FILE_NAME)).unwrap_or(DEFAULT_SIZE);
    // Старые профили могут хранить размер меньше нового минимума окна.
    size(px(width.max(MIN_WIDTH)), px(height.max(MIN_HEIGHT)))
}

/// Читает и проверяет обе координаты в формате существующего профиля.
fn read(path: &Path) -> Option<[f32; 2]> {
    let bytes = std::fs::read(path).ok()?;
    let value = serde_json::from_slice(&bytes).ok()?;
    valid(value).then_some(value)
}

/// Отбрасывает размеры, с которыми нельзя открыть рабочее окно.
fn valid([width, height]: [f32; 2]) -> bool {
    (100. ..=10000.).contains(&width) && (100. ..=10000.).contains(&height)
}

/// Сохраняет обновления по порядку и завершает работу после записи последнего снимка.
async fn write_updates(path: PathBuf, mut updates: watch::Receiver<SizeUpdate>) {
    let mut saved = None;
    while updates.changed().await.is_ok() {
        let update = *updates.borrow_and_update();
        if let Some(value) = update.size
            && saved != Some(value)
        {
            match save(&path, value) {
                Ok(()) => saved = Some(value),
                Err(error) => {
                    log::warn!(
                        "event=workspace_window_size_save result=failed kind={:?}",
                        error.kind()
                    );
                }
            }
        }
        if update.closing {
            break;
        }
    }
}

/// Атомарно заменяет файл размера в каталоге профиля.
fn save(path: &Path, value: [f32; 2]) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("Нет каталога профиля"))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(file.as_file_mut(), &value)?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Восстанавливает профиль Iced в прежнем формате и использует стандартный размер при ошибке.
    #[test]
    fn restores_existing_profile_size_and_rejects_invalid_data() {
        let profile = tempfile::tempdir().unwrap();
        let path = profile.path().join(FILE_NAME);
        std::fs::write(&path, b"[880.5,490]").unwrap();
        assert_eq!(load(profile.path()), size(px(880.5), px(600.0)));
        std::fs::write(&path, b"[800,0]").unwrap();
        assert_eq!(load(profile.path()), size(px(700.0), px(600.0)));
    }

    /// Сохраняет новый размер даже при закрытии до первой фоновой записи.
    #[tokio::test]
    async fn flushes_latest_accepted_size_when_the_window_closes() {
        let profile = tempfile::tempdir().unwrap();
        let path = profile.path().join(FILE_NAME);
        let (changes, receiver) = watch::channel(SizeUpdate::default());
        changes.send_modify(|update| update.size = Some([750., 610.]));
        changes.send_modify(|update| update.size = Some([880.5, 640.]));
        changes.send_modify(|update| update.closing = true);
        write_updates(path, receiver).await;
        assert_eq!(load(profile.path()), size(px(880.5), px(640.)));
    }
}
