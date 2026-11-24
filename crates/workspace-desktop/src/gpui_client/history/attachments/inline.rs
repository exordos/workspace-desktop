//! Загружает превью видимых фото и видео и держит ограниченный набор кадров ленты.

use std::{cell::RefCell, collections::VecDeque, io, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, App, Context, Div, Entity, ImageSource, IntoElement, ObjectFit, Render,
    RenderImage, SharedString, Task, WeakEntity, Window, canvas, div, img, prelude::*, px, rgba,
};
use tokio::sync::Semaphore;
use workspace_core::{
    messenger::{files::MediaFile, history::MediaAccess},
    storage::media::MediaKind,
};

use super::super::media_album::Tile;
use super::{AttachmentKey, Conversation, PictureError, decode_original, render_image};
use crate::{
    gpui_client::{
        contracts::ConversationAddress,
        preferences::Language,
        runtime::RuntimeClient,
        ui::{Icon, Palette, TextRole, label},
    },
    platform::media,
};

const MAX_THUMBNAILS: usize = 32;
const MAX_FRAME_SIDE: u32 = 640;

/// Допуск обычной карточки и маршрут для ответа из рабочей очереди.
#[derive(Clone)]
pub(super) struct Load {
    pub(super) runtime: RuntimeClient, // Выполняет запрос и декодирование вне UI.
    pub(super) access: MediaAccess,    // Допуск открытой истории.
    pub(super) address: ConversationAddress, // Полный маршрут открытия.
}

/// Вид и подпись одного кадра, независимые от состояния загрузки.
#[derive(Clone)]
pub(super) struct Frame {
    pub(super) kind: MediaKind,    // Источник пикселей и вид заглушки.
    pub(super) name: SharedString, // Имя файла для заглушки ошибки.
    pub(super) language: Language, // Язык аккаунта для текста ошибки.
    pub(super) tile: Tile,         // Пропорции кадрирования и скругления плитки в пузыре.
}

/// Кадр и состояние текущей попытки для одного места в ленте.
struct Thumbnail {
    frame: Frame,                                           // Вид и подпись файла.
    result: Option<Result<Arc<RenderImage>, PictureError>>, // None, пока идёт запрос.
    task: Option<Task<()>>, // При удалении карточки отменяет ожидание и сеть.
}

impl Thumbnail {
    /// Отменяет запрос и освобождает текстуру при удалении карточки.
    fn retire(&mut self, cx: &mut Context<Self>) {
        self.task.take();
        if let Some(Ok(image)) = self.result.take() {
            cx.defer(move |app| app.drop_image(image, None));
        }
    }
}

impl Render for Thumbnail {
    /// Сохраняет размеры родительской карточки для любого состояния загрузки.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frame
            .render(self.result.as_ref(), Palette::for_window(window, cx))
    }
}

impl Frame {
    /// Показывает ожидающий, готовый или недоступный кадр в одних границах.
    fn render(
        &self,
        result: Option<&Result<Arc<RenderImage>, PictureError>>,
        palette: Palette,
    ) -> Div {
        let video = self.kind == MediaKind::Video;
        let frame = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center();
        let frame = match result {
            Some(Ok(image)) => {
                let preview = img(ImageSource::Render(image.clone()))
                    .size_full()
                    .object_fit(ObjectFit::Cover)
                    .debug_selector(move || {
                        if video {
                            "history-video-preview".into()
                        } else {
                            "history-attachment-preview".into()
                        }
                    });
                frame.child(self.tile.round(preview))
            }
            Some(Err(_)) => frame
                .child(
                    label(palette, TextRole::Label, self.name.clone())
                        .max_w_full()
                        .truncate(),
                )
                .child(label(
                    palette,
                    TextRole::Caption,
                    self.language
                        .tr("desktop.messenger.history.mediaPreviewUnavailable"),
                )),
            None if !video => frame.child(Icon::Image.element(px(24.), palette.text_secondary)),
            None => frame,
        };
        if video && !matches!(result, Some(Err(_))) {
            frame.child(play_indicator())
        } else {
            frame
        }
    }
}

/// Показывает действие воспроизведения поверх кадра или до его загрузки.
pub(super) fn play_indicator() -> Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .size(px(44.))
                .rounded_full()
                .bg(rgba(0x000000a6))
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgba(0xffffffff))
                .child("▶"),
        )
}

/// Очередь от давно показанных карточек к недавно показанным.
type Entries = VecDeque<(AttachmentKey, Entity<Thumbnail>)>;

/// Ограничивает число декодированных кадров и одновременных запросов одного разговора.
#[derive(Clone)]
pub(super) struct Thumbnails {
    entries: Rc<RefCell<Entries>>, // Одни и те же сущности между отрисовками виртуального списка.
    slots: Arc<Semaphore>,         // Не даёт превью занять общую очередь рабочего потока.
}

impl Default for Thumbnails {
    /// Создаёт кэш пустого разговора с тремя параллельными загрузками.
    fn default() -> Self {
        Self {
            entries: Rc::default(),
            slots: Arc::new(Semaphore::new(3)),
        }
    }
}

impl Thumbnails {
    /// Проверяет, виден ли декодированный кадр в тесте истории.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn has_ready(&self, app: &App) -> bool {
        self.entries
            .borrow()
            .iter()
            .any(|(_, entry)| matches!(entry.read(app).result, Some(Ok(_))))
    }

    /// Восстанавливает карточку после успешной ручной попытки открыть фото.
    pub(super) fn fill_failed(&self, key: AttachmentKey, image: Arc<RenderImage>, app: &mut App) {
        if let Some((_, entry)) = self
            .entries
            .borrow()
            .iter()
            .find(|(stored, _)| *stored == key)
        {
            entry.update(app, |thumbnail, cx| {
                if matches!(thumbnail.result, Some(Err(_))) {
                    thumbnail.result = Some(Ok(image));
                    cx.notify();
                }
            });
        }
    }

    /// Отменяет запросы и удаляет готовые кадры изменившихся или отозванных вложений.
    pub(super) fn retain(&self, keep: impl Fn(AttachmentKey) -> bool, app: &mut App) {
        self.entries.borrow_mut().retain(|(key, entry)| {
            if keep(*key) {
                return true;
            }
            entry.update(app, Thumbnail::retire);
            false
        });
    }

    /// Показывает готовый кадр или ждёт, пока пустая карточка появится в области прокрутки.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &self,
        key: AttachmentKey,
        frame: Frame,
        load: Load,
        target: WeakEntity<Conversation>,
        palette: Palette,
        app: &mut App,
    ) -> AnyElement {
        let mut entries = self.entries.borrow_mut();
        if let Some(index) = entries.iter().position(|(stored, _)| *stored == key)
            && let Some((_, entry)) = entries.remove(index)
        {
            if entry.read(app).frame.tile.corners != frame.tile.corners {
                entry.update(app, |thumbnail, cx| {
                    thumbnail.frame.tile.corners = frame.tile.corners;
                    cx.notify();
                });
            }
            entries.push_back((key, entry.clone()));
            return entry.into_any_element();
        }
        let placeholder = frame.render(None, palette);
        placeholder
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, app| {
                        if bounds.intersects(&window.content_mask().bounds) {
                            app.defer(move |app| {
                                let _ = target.update(app, |view, cx| {
                                    if view.has_access()
                                        && view.address == load.address
                                        && view.viewport.has_attachment(key)
                                    {
                                        view.attachments.inline_thumbs.start(key, frame, load, cx);
                                    }
                                });
                            });
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }

    /// Запускает одну попытку для карточки, попавшей в область прокрутки.
    fn start(&self, key: AttachmentKey, frame: Frame, load: Load, cx: &mut Context<Conversation>) {
        let mut entries = self.entries.borrow_mut();
        if entries.iter().any(|(stored, _)| *stored == key) {
            return;
        }
        if entries.len() == MAX_THUMBNAILS
            && let Some((_, oldest)) = entries.pop_front()
        {
            oldest.update(cx, Thumbnail::retire);
        }
        let slots = self.slots.clone();
        let kind = frame.kind;
        let ratio = frame.tile.ratio;
        let entry = cx.new(|cx: &mut Context<Thumbnail>| {
            let task = cx.spawn(async move |entry, cx| {
                let Ok(_slot) = slots.acquire().await else {
                    return;
                };
                let address = load.address.clone();
                let result = load
                    .runtime
                    .conversation(load.address, move |services, _| async move {
                        let file = services
                            .messenger
                            .open_media(&services.sync, &load.access, key.message, key.file)
                            .await
                            .map_err(|_| PictureError::Unavailable)?;
                        tokio::task::spawn_blocking(move || decode_frame(file, kind, ratio))
                            .await
                            .map_err(|_| PictureError::Unavailable)?
                    })
                    .await;
                let _ = entry.update(cx, |thumbnail, cx| {
                    if thumbnail.task.is_some() {
                        thumbnail.result = result
                            .for_current(Some(&address))
                            .and_then(Result::ok)
                            .or(Some(Err(PictureError::Unavailable)));
                        cx.notify();
                    }
                });
            });
            Thumbnail {
                frame,
                result: None,
                task: Some(task),
            }
        });
        entries.push_back((key, entry));
        cx.notify();
    }
}

/// Готовит ограниченный кадр и ещё раз проверяет доступ перед передачей пикселей в UI.
fn decode_frame(
    mut file: MediaFile,
    kind: MediaKind,
    ratio: f32,
) -> Result<Arc<RenderImage>, PictureError> {
    let image = crop_to_tile(decode_pixels(&mut file, kind)?, ratio);
    file.with_valid_access(|| render_image(image))
        .map_err(|_| PictureError::Unavailable)
}

/// Убирает лишние края кадра, сохраняя центр и пропорции без растяжения.
pub(super) fn crop_to_tile(mut image: image::RgbaImage, ratio: f32) -> image::RgbaImage {
    let (width, height) = image.dimensions();
    let crop_width = (height as f32 * ratio).round().clamp(1., width as f32) as u32;
    let crop_height = (width as f32 / ratio).round().clamp(1., height as f32) as u32;
    if (crop_width, crop_height) == (width, height) {
        return image;
    }
    image::imageops::crop(
        &mut image,
        (width - crop_width) / 2,
        (height - crop_height) / 2,
        crop_width,
        crop_height,
    )
    .to_image()
}

/// Готовит уменьшенные пиксели из того же авторизованного оригинала для обоих видов карточек.
pub(super) fn decode_pixels(
    file: &mut MediaFile,
    kind: MediaKind,
) -> Result<image::RgbaImage, PictureError> {
    if kind == MediaKind::Video {
        let clip = copy_video(file)?;
        let png = media::video_poster(clip.path(), f64::from(MAX_FRAME_SIDE))
            .ok_or(PictureError::Unsupported)?;
        image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .map(|image| image.into_rgba8())
            .map_err(|_| PictureError::Unsupported)
    } else {
        Ok(decode_original(file)?
            .thumbnail(MAX_FRAME_SIDE, MAX_FRAME_SIDE)
            .into_rgba8())
    }
}

/// Копирует авторизованный контейнер в закрытый файл только на время работы AVFoundation.
pub(super) fn copy_video(file: &mut MediaFile) -> Result<tempfile::NamedTempFile, PictureError> {
    media::stage_video(file).map_err(|error| match error.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => PictureError::Unsupported,
        _ => PictureError::Unavailable,
    })
}
