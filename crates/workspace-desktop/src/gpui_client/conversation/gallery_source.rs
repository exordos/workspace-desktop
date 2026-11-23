//! Читает страницы галереи через Core и сохраняет выбранный оригинал через системный диалог.

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use gpui::{Context, FocusHandle, RenderImage, Task, Window};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        files::{MediaFile, OpenMediaError},
        history::MediaAccess,
    },
    storage::media::{
        LinkHistoryCursor, LinkHistoryItem, MediaCacheError, MediaHistoryCursor, MediaHistoryItem,
        MediaKind,
    },
};

use super::{Conversation, attachments::decode_thumbnail};
use crate::gpui_client::{
    contracts::{ConversationAddress, ConversationReply},
    runtime::{RuntimeClient, WorkerServices},
};

const PAGE_SIZE: usize = 24;
pub(super) const MAX_THUMBNAILS: usize = 12;

/// Раздел вложений, открытый в сведениях о разговоре.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GalleryCategory {
    Photos, // Ссылки на фото и изображения.
    Videos, // Ссылки на видео.
    Files,  // Обычные вложения.
    Links,  // Внешние ссылки из загруженных сообщений.
}

/// Следующая граница соответствующего локального списка.
#[derive(Clone, Debug)]
pub(super) enum GalleryCursor {
    Media(MediaHistoryCursor), // Положение в индексе вложений.
    Links(LinkHistoryCursor),  // Положение в индексе ссылок.
}

/// Данные одной строки из подтверждённой истории.
pub(super) enum GalleryItem {
    Media {
        item: MediaHistoryItem,            // Файл и исходное сообщение.
        preview: Option<Arc<RenderImage>>, // Маленькое фото из готовой копии.
    },
    Link(LinkHistoryItem), // Внешняя ссылка и исходное сообщение.
}

/// Одна порция истории и возможность продолжить чтение.
pub(super) struct GalleryPage {
    pub items: Vec<GalleryItem>,     // Порядок выбирает Core.
    pub next: Option<GalleryCursor>, // None после последней известной строки.
}

/// Ошибка чтения страницы с подходящим способом восстановления.
#[derive(Clone, Copy, Debug)]
pub(super) enum GalleryProblem {
    StaleCursor, // Следующая страница требует перечитать обновлённый список сначала.
    Unavailable, // Чтение не состоялось, можно повторить последнее действие.
}

impl From<OpenMediaError> for GalleryProblem {
    /// Распознаёт устаревшую границу страницы отдельно от временного отказа.
    fn from(error: OpenMediaError) -> Self {
        match error {
            OpenMediaError::Cache(MediaCacheError::Stale) => Self::StaleCursor,
            _ => Self::Unavailable,
        }
    }
}

/// Передаёт чтение индекса SQLite на рабочий поток.
pub(super) async fn load(
    runtime: RuntimeClient,
    address: ConversationAddress,
    access: MediaAccess,
    whole_stream: bool,
    category: GalleryCategory,
    after: Option<GalleryCursor>,
) -> ConversationReply<Result<GalleryPage, GalleryProblem>> {
    let id = Uuid::new_v4();
    log::debug!("gpui_gallery op_id={id} event=page_requested category={category:?}");
    let reply = runtime
        .conversation(address, move |services, _| async move {
            tokio::task::spawn_blocking(move || {
                read_page(&services, &access, whole_stream, category, after)
            })
            .await
            .unwrap_or(Err(GalleryProblem::Unavailable))
        })
        .await;
    let result = reply.value.unwrap_or(Err(GalleryProblem::Unavailable));
    log::debug!(
        "gpui_gallery op_id={id} event=page_completed success={}",
        result.is_ok()
    );
    ConversationReply {
        address: reply.address,
        value: result,
    }
}

/// Читает каталог известных вложений и ссылок без открытия оригиналов.
fn read_page(
    services: &WorkerServices,
    access: &MediaAccess,
    whole_stream: bool,
    category: GalleryCategory,
    after: Option<GalleryCursor>,
) -> Result<GalleryPage, GalleryProblem> {
    let core = &services.messenger;
    let sync = &services.sync;
    if category == GalleryCategory::Links {
        let after = match after {
            Some(GalleryCursor::Links(cursor)) => Some(cursor),
            None => None,
            _ => return Err(GalleryProblem::StaleCursor),
        };
        let page = core.list_links(sync, access, whole_stream, after, PAGE_SIZE)?;
        return Ok(GalleryPage {
            items: page.links.into_iter().map(GalleryItem::Link).collect(),
            next: page.next.map(GalleryCursor::Links),
        });
    }

    let after = match after {
        Some(GalleryCursor::Media(cursor)) => Some(cursor),
        None => None,
        _ => return Err(GalleryProblem::StaleCursor),
    };
    let kind = match category {
        GalleryCategory::Photos => MediaKind::Image,
        GalleryCategory::Videos => MediaKind::Video,
        GalleryCategory::Files => MediaKind::File,
        GalleryCategory::Links => return Err(GalleryProblem::Unavailable),
    };
    let page = core.list_media(sync, access, whole_stream, Some(kind), after, PAGE_SIZE)?;
    let items = page
        .attachments
        .into_iter()
        .map(|item| GalleryItem::Media {
            item,
            preview: None,
        })
        .collect();
    Ok(GalleryPage {
        items,
        next: page.next.map(GalleryCursor::Media),
    })
}

/// Декодирует небольшой набор готовых фото после показа страницы, без сети.
pub(super) async fn load_thumbnails(
    runtime: RuntimeClient,
    address: ConversationAddress,
    access: MediaAccess,
    whole_stream: bool,
    entries: Vec<(Uuid, MediaHistoryItem)>,
) -> ConversationReply<Vec<(Uuid, Arc<RenderImage>)>> {
    let reply = runtime
        .conversation(address, move |services, _| async move {
            tokio::task::spawn_blocking(move || {
                entries
                    .into_iter()
                    .filter(|(_, item)| item.kind == MediaKind::Image && item.cached)
                    .take(MAX_THUMBNAILS)
                    .filter_map(|(id, item)| {
                        let file = services
                            .messenger
                            .open_gallery_cached_media(&services.sync, &access, whole_stream, &item)
                            .ok()??;
                        Some((id, decode_thumbnail(file)?))
                    })
                    .collect()
            })
            .await
            .unwrap_or_default()
        })
        .await;
    ConversationReply {
        address: reply.address,
        value: reply.value.unwrap_or_default(),
    }
}

/// Стадия сохранения одного файла, показанная прямо в его строке.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GalleryExportStatus {
    Choosing, // Открыт системный выбор места.
    Saving,   // Читается оригинал или записывается файл.
    Done,     // Готовый файл появился по выбранному пути.
    TooLarge, // Оригинал превышает лимит получения через Core.
    Occupied, // Путь уже занят; пользователь может выбрать другое имя.
    Failed,   // Не удалось получить оригинал, открыть диалог или записать файл.
}

impl GalleryExportStatus {
    /// Показывает короткий статус без раскрытия пути и ошибок сервера.
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Choosing => "Выбираем место…",
            Self::Saving => "Сохраняем…",
            Self::Done => "Сохранено",
            Self::TooLarge => "Файл слишком большой для скачивания",
            Self::Occupied => "Имя занято. Выберите другое",
            Self::Failed => "Не удалось сохранить файл",
        }
    }

    /// Отключает повторное сохранение до завершения текущего действия.
    pub(super) fn is_pending(self) -> bool {
        matches!(self, Self::Choosing | Self::Saving)
    }
}

/// Операция принадлежит открытому экрану. Снятие состояния отменяет публикацию файла.
pub(super) struct GalleryExport {
    id: Uuid,                    // Номер попытки для проверки ответа.
    message: Uuid,               // Исходное сообщение для отзыва ссылки.
    file: Uuid,                  // UUID файла для строки и событий Core.
    status: GalleryExportStatus, // Видимый результат или стадия работы.
    cancelled: Arc<Mutex<bool>>, // Синхронизирует отмену и публикацию готовой записи.
    _task: Task<()>,             // Владеет ожиданием системного диалога и результата.
}

impl Drop for GalleryExport {
    /// Запрещает публикацию до того, как рабочий поток увидит закрытие экрана.
    fn drop(&mut self) {
        *self
            .cancelled
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
    }
}

impl Conversation {
    /// Возвращает статус только для строки, начавшей сохранение.
    pub(super) fn gallery_export_status(
        &self,
        item: &MediaHistoryItem,
    ) -> Option<GalleryExportStatus> {
        self.gallery_export
            .as_ref()
            .filter(|save| save.message == item.message_id && save.file == item.file_id)
            .map(|save| save.status)
    }

    /// Останавливает незавершённое сохранение при выходе из раздела.
    pub(super) fn cancel_gallery_export(&mut self) {
        self.gallery_export = None;
    }

    /// Отменяет сохранение, если изменились файл, сообщение или права источника.
    pub(super) fn invalidate_gallery_export(&mut self, affected: impl Fn(Uuid, Uuid) -> bool) {
        if self
            .gallery_export
            .as_ref()
            .is_some_and(|save| affected(save.message, save.file))
        {
            self.cancel_gallery_export();
        }
    }

    /// Спрашивает путь у пользователя и только затем запрашивает оригинал у Core.
    pub(super) fn activate_gallery_file(
        &mut self,
        item: MediaHistoryItem,
        whole_stream: bool,
        source: FocusHandle,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(access), Some(runtime)) =
            (self.gallery_access(), self.attachments.runtime.clone())
        else {
            return;
        };
        if self
            .gallery_export
            .as_ref()
            .is_some_and(|save| save.status.is_pending())
        {
            return;
        }
        let id = Uuid::new_v4();
        let address = self.address.clone();
        let cancelled = Arc::new(Mutex::new(false));
        let cancel_write = Arc::clone(&cancelled);
        let message = item.message_id;
        let file = item.file_id;
        let start = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let downloads = start.join("Downloads");
        let start = if downloads.is_dir() {
            &downloads
        } else {
            &start
        };
        let name = suggested_name(&item);
        let picker = cx.prompt_for_new_path(start, Some(&name));
        let task = cx.spawn_in(window, async move |target, cx| {
            let path = match picker.await {
                Ok(Ok(path)) => path,
                _ => {
                    let _ = target.update(cx, |view, cx| {
                        view.finish_gallery_export(id, Some(GalleryExportStatus::Failed), cx)
                    });
                    return;
                }
            };
            let picking_finished = cx
                .update(|window, app| {
                    target
                        .update(app, |view, cx| {
                            if view.address != address
                                || !view.has_access()
                                || !view
                                    .gallery_export
                                    .as_ref()
                                    .is_some_and(|save| save.id == id)
                            {
                                return false;
                            }
                            window.focus(&source);
                            if path.is_none() {
                                view.finish_gallery_export(id, None, cx);
                                return false;
                            }
                            if let Some(save) = &mut view.gallery_export {
                                save.status = GalleryExportStatus::Saving;
                            }
                            cx.notify();
                            true
                        })
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let Some(path) = path.filter(|_| picking_finished) else {
                return;
            };
            log::debug!("gpui_gallery op_id={id} event=save_requested");
            let reply = runtime
                .conversation(address, move |services, _| async move {
                    let file = match services
                        .messenger
                        .open_gallery_media(&services.sync, &access, whole_stream, &item)
                        .await
                    {
                        Ok(file) => file,
                        Err(OpenMediaError::Cache(MediaCacheError::TooLarge)) => {
                            return GalleryExportStatus::TooLarge;
                        }
                        Err(_) => return GalleryExportStatus::Failed,
                    };
                    tokio::task::spawn_blocking(move || write_original(file, &path, &cancel_write))
                        .await
                        .unwrap_or(GalleryExportStatus::Failed)
                })
                .await;
            let _ = target.update(cx, |view, cx| {
                if let Some(result) = reply.for_current(Some(&view.address)) {
                    view.finish_gallery_export(
                        id,
                        Some(result.unwrap_or(GalleryExportStatus::Failed)),
                        cx,
                    );
                }
            });
        });
        self.gallery_export = Some(GalleryExport {
            id,
            message,
            file,
            status: GalleryExportStatus::Choosing,
            cancelled,
            _task: task,
        });
        cx.notify();
    }

    /// Показывает результат только в той попытке, которая ещё принадлежит экрану.
    fn finish_gallery_export(
        &mut self,
        id: Uuid,
        result: Option<GalleryExportStatus>,
        cx: &mut Context<Self>,
    ) {
        let Some(save) = self.gallery_export.as_mut().filter(|save| save.id == id) else {
            return;
        };
        log::debug!("gpui_gallery op_id={id} event=save_completed status={result:?}");
        if let Some(result) = result {
            save.status = result;
        } else {
            self.cancel_gallery_export();
        }
        cx.notify();
    }
}

/// Имя из сообщения остаётся подсказкой в диалоге и не задаёт путь записи.
fn suggested_name(item: &MediaHistoryItem) -> String {
    let cleaned: String = item
        .name
        .as_deref()
        .unwrap_or_default()
        .chars()
        .filter(|letter| !letter.is_control())
        .map(|letter| {
            if matches!(letter, '/' | '\\' | ':') {
                '_'
            } else {
                letter
            }
        })
        .take(63)
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.']);
    if cleaned.is_empty() {
        if item.kind == MediaKind::Video {
            "Видео".into()
        } else {
            "Вложение".into()
        }
    } else {
        cleaned.into()
    }
}

/// Пишет проверенный файл рядом с целью, затем публикует его без перезаписи.
fn write_original(
    mut file: MediaFile,
    path: &Path,
    cancelled: &Mutex<bool>,
) -> GalleryExportStatus {
    let Some(directory) = path.parent().filter(|_| path.is_absolute()) else {
        return GalleryExportStatus::Failed;
    };
    if *cancelled.lock().unwrap_or_else(|error| error.into_inner()) {
        return GalleryExportStatus::Failed;
    }
    let Ok(mut pending) = tempfile::NamedTempFile::new_in(directory) else {
        return GalleryExportStatus::Failed;
    };
    let size = file.size();
    if !io::copy(&mut file, &mut pending).is_ok_and(|written| written == size)
        || pending.flush().is_err()
    {
        return GalleryExportStatus::Failed;
    }
    // Проверяем отмену и действующие права непосредственно перед появлением файла.
    let stopped = cancelled.lock().unwrap_or_else(|error| error.into_inner());
    if *stopped || !file.read(&mut [0]).is_ok_and(|read| read == 0) {
        return GalleryExportStatus::Failed;
    }
    match pending.persist_noclobber(path) {
        Ok(_) => GalleryExportStatus::Done,
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            GalleryExportStatus::Occupied
        }
        Err(_) => GalleryExportStatus::Failed,
    }
}
