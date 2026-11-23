//! Показывает вложения подтверждённого сообщения и открывает одно фото через кэш Core.
//! Здесь находятся карточка, состояние просмотра, загрузка и ограниченное декодирование.

use std::{
    cell::RefCell,
    collections::HashMap,
    io::{Cursor, Read},
    rc::Rc,
    sync::Arc,
};

use gpui::{
    AnyElement, App, Context, Div, FocusHandle, ImageSource, IntoElement, MouseButton, ObjectFit,
    RenderImage, SharedString, Stateful, Task, WeakEntity, Window, actions, anchored, deferred,
    div, img, point, prelude::*, px, rgba,
};
use image::{DynamicImage, Frame, ImageFormat, ImageReader, Limits, RgbaImage, imageops};
use uuid::Uuid;
use workspace_core::{
    messenger::{files::MediaFile, history::MediaAccess, references::MediaReference},
    storage::media::{MediaHistoryItem, MediaKind},
};

use super::Conversation;
use crate::gpui_client::{
    runtime::RuntimeClient,
    ui::{Button, ButtonAppearance, Icon, Palette, SURFACE_RADIUS, TextRole, activate, label},
};

const VIEWER_CONTEXT: &str = "WorkspacePhotoViewer";
const MAX_ENCODED_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SOURCE_SIDE: u32 = 4096;
const MAX_VIEW_SIDE: u32 = 2048;

actions!(attachment_viewer, [ClosePhoto, KeepPhotoFocus]);

/// Выбирает одну карточку, включая повторные ссылки внутри того же сообщения.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct AttachmentKey {
    pub message: Uuid,   // Публичный UUID сообщения.
    pub file: Uuid,      // UUID вложения.
    pub position: usize, // Порядок ссылки в исходном тексте.
}

/// Источник открытия определяет способ проверки ссылки и возврат фокуса.
enum PhotoTarget {
    History, // Карточка находится в текущем окне истории.
    Gallery {
        item: MediaHistoryItem, // Точное сообщение и вложение из локального списка.
        whole_stream: bool,     // Источник может быть в другой теме стрима.
    },
}

/// Фокус закрытого просмотра и условия его возврата.
pub(super) enum PhotoReturn {
    History(AttachmentKey, FocusHandle), // Карточка должна оставаться видимой в истории.
    Gallery(FocusHandle),                // Плитка из открытого раздела сведений.
    Conversation,                        // Источник исчез. Фокус вернётся в разговор.
}

/// Готовые пиксели для просмотра и маленькой карточки.
struct Photo {
    full: Arc<RenderImage>,  // Кадр для открытого просмотра.
    thumb: Arc<RenderImage>, // Уменьшенная копия для истории.
}

/// Как показать отказ чтения без подробностей запроса или имени на диске.
#[derive(Clone, Copy, Debug)]
enum PictureError {
    Unavailable, // Файл недоступен либо рабочий запрос прерван.
    Unsupported, // Формат неизвестен или данные повреждены.
    TooLarge,    // Исходник превышает текущий лимит просмотра.
}

impl PictureError {
    /// Даёт короткую причину рядом с кнопкой закрытия.
    fn message(self) -> &'static str {
        match self {
            Self::Unavailable => "Не удалось открыть фото",
            Self::Unsupported => "Не удалось прочитать изображение",
            Self::TooLarge => "Изображение слишком большое для просмотра",
        }
    }
}

/// Видимое состояние оверлея для одной попытки.
enum PictureState {
    Loading,                 // Читаем копию или ждём файлы с сервера.
    Ready(Arc<RenderImage>), // Декодированный кадр в пропорциях исходника.
    Failed(PictureError),    // Сообщение об ошибке и кнопка закрытия.
}

/// Открытое намерение пользователя со своим источником возврата фокуса.
struct Viewer {
    key: AttachmentKey,   // Карточка, открывшая просмотр.
    attempt: Uuid,        // Применим только ответ этой попытки.
    title: SharedString,  // Подпись из сообщения.
    source: FocusHandle,  // Фокус карточки при закрытии.
    state: PictureState,  // Загрузка, кадр или отказ.
    focus_on_mount: bool, // Фокус после появления панели в дереве окна.
    from_gallery: bool,   // Открыто из правой панели, независимо от окна истории.
}

/// Память одного разговора. Оригинал хранит Core, экран держит последнее превью.
pub(super) struct Attachments {
    pub access: Option<MediaAccess>, // Допуск, полученный при успешном открытии истории.
    pub runtime: Option<RuntimeClient>, // Переносит загрузку на рабочий поток.
    close_focus: FocusHandle,        // Крестик получает фокус при показе оверлея.
    region: FocusHandle,             // Область клавиш и блокировки фона.
    viewer: Option<Viewer>,          // Открытое фото, по одному на разговор.
    task: Option<Task<()>>,          // Закрытие отменяет ожидание результата.
    preview: Option<(AttachmentKey, Arc<RenderImage>)>, // Последнее маленькое превью.
    gallery_preview: Option<(AttachmentKey, Arc<RenderImage>)>, // Превью только что открытой плитки.
    focus: Rc<RefCell<HashMap<AttachmentKey, FocusHandle>>>,    // Устойчивый фокус карточек.
    restore: Option<PhotoReturn>,                               // Возврат после снятия оверлея.
}

impl Attachments {
    /// Создаёт закрытый просмотр и постоянные клавиатурные точки.
    pub(super) fn new(cx: &mut Context<Conversation>) -> Self {
        Self {
            access: None,
            runtime: None,
            close_focus: cx.focus_handle(),
            region: cx.focus_handle(),
            viewer: None,
            task: None,
            preview: None,
            gallery_preview: None,
            focus: Rc::default(),
            restore: None,
        }
    }

    /// Регистрирует Escape и Tab только внутри просмотра фото.
    pub(in crate::gpui_client) fn bind_keys(app: &mut App) {
        let context = Some(VIEWER_CONTEXT);
        app.bind_keys([
            gpui::KeyBinding::new("escape", ClosePhoto, context),
            gpui::KeyBinding::new("tab", KeepPhotoFocus, context),
            gpui::KeyBinding::new("shift-tab", KeepPhotoFocus, context),
        ]);
    }

    /// Проверяет, перехватил ли просмотр клавиатуру разговора.
    pub(super) fn is_open(&self) -> bool {
        self.viewer.is_some()
    }

    /// Готовит возможности и превью для виртуальных строк списка.
    pub(super) fn cards(&self, cx: &Context<Conversation>) -> Cards {
        Cards {
            target: cx.entity().downgrade(),
            enabled: self.runtime.is_some() && self.access.is_some(),
            preview: self.preview.clone(),
            focus: self.focus.clone(),
        }
    }

    /// Скрывает кадр и отменяет ожидание; маленький снимок остаётся на карточке.
    pub(super) fn close(&mut self, cx: &mut Context<Conversation>) {
        self.task = None;
        if let Some(viewer) = self.viewer.take() {
            log::debug!("gpui_media op_id={} event=viewer_closed", viewer.attempt);
            self.restore = Some(if viewer.from_gallery {
                PhotoReturn::Gallery(viewer.source)
            } else {
                PhotoReturn::History(viewer.key, viewer.source)
            });
            if let PictureState::Ready(image) = viewer.state {
                cx.defer(move |app| app.drop_image(image, None));
            }
            cx.notify();
        }
    }

    /// Завершает показ и удаляет пиксели, которым больше нельзя доверять.
    pub(super) fn invalidate(
        &mut self,
        affected: impl Fn(AttachmentKey) -> bool,
        cx: &mut Context<Conversation>,
    ) {
        if self
            .viewer
            .as_ref()
            .is_some_and(|viewer| affected(viewer.key))
        {
            self.close(cx);
            self.restore = Some(PhotoReturn::Conversation);
        }
        if self.preview.as_ref().is_some_and(|(key, _)| affected(*key))
            && let Some((_, image)) = self.preview.take()
        {
            cx.defer(move |app| app.drop_image(image, None));
            cx.notify();
        }
        if self
            .gallery_preview
            .as_ref()
            .is_some_and(|(key, _)| affected(*key))
            && let Some((_, image)) = self.gallery_preview.take()
        {
            cx.defer(move |app| app.drop_image(image, None));
            cx.notify();
        }
    }

    /// Забывает допуск и изображения старого открытия истории.
    pub(super) fn reset(&mut self, cx: &mut Context<Conversation>) {
        self.invalidate(|_| true, cx);
        self.access = None;
        self.focus.borrow_mut().clear();
    }

    /// Возвращает точку фокуса после закрытия просмотра в том же разговоре.
    pub(super) fn take_restore(&mut self) -> Option<PhotoReturn> {
        self.restore.take()
    }

    /// Убирает карточки, вышедшие из окна истории.
    pub(super) fn retain(
        &mut self,
        contains: impl Fn(AttachmentKey) -> bool,
        cx: &mut Context<Conversation>,
    ) {
        if self
            .viewer
            .as_ref()
            .is_some_and(|viewer| !viewer.from_gallery && !contains(viewer.key))
        {
            self.close(cx);
        }
        if self
            .preview
            .as_ref()
            .is_some_and(|(key, _)| !contains(*key))
            && let Some((_, image)) = self.preview.take()
        {
            cx.defer(move |app| app.drop_image(image, None));
            cx.notify();
        }
        self.focus.borrow_mut().retain(|key, _| contains(*key));
    }

    /// Рисует самостоятельный слой фото поверх рабочего окна.
    pub(super) fn overlay(
        &mut self,
        window: &mut Window,
        palette: Palette,
        cx: &Context<Conversation>,
    ) -> Option<AnyElement> {
        let viewer = self.viewer.as_mut()?;
        if std::mem::take(&mut viewer.focus_on_mount) {
            window.focus(&self.close_focus);
        }
        let viewport = window.viewport_size();
        let backdrop = div()
            .id("media-viewer-backdrop")
            .absolute()
            .inset_0()
            .bg(rgba(0x00000099))
            .occlude()
            .on_click(cx.listener(|view, _, _, cx| view.attachments.close(cx)));
        let mut close = Button::new("media-viewer-close", "Закрыть");
        close.appearance = ButtonAppearance::Ghost;
        close.icon = Some(Icon::Close);
        let contents = div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .justify_center()
            .items_center();
        let contents = match &viewer.state {
            PictureState::Loading => contents
                .child(label(palette, TextRole::Body, "Открываем фото…"))
                .debug_selector(|| "media-viewer-loading".into()),
            PictureState::Failed(error) => contents
                .child(label(palette, TextRole::Body, error.message()))
                .debug_selector(|| "media-viewer-error".into()),
            PictureState::Ready(image) => contents.child(
                img(ImageSource::Render(image.clone()))
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .debug_selector(|| "media-viewer-image".into()),
            ),
        };
        let close_target = cx.entity().downgrade();
        let panel = div()
            .id("media-viewer")
            .debug_selector(|| "media-viewer".into())
            .relative()
            .w(viewport.width * 0.9)
            .h(viewport.height * 0.85)
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .rounded(SURFACE_RADIUS)
            .bg(palette.surface)
            .occlude()
            .track_focus(&self.region)
            .key_context(VIEWER_CONTEXT)
            .on_action(cx.listener(|view, _: &ClosePhoto, _, cx| {
                cx.stop_propagation();
                view.attachments.close(cx);
            }))
            .on_action(cx.listener(|view, _: &KeepPhotoFocus, window, cx| {
                cx.stop_propagation();
                window.focus(&view.attachments.close_focus);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        label(palette, TextRole::Label, viewer.title.clone())
                            .min_w_0()
                            .whitespace_nowrap()
                            .overflow_hidden(),
                    )
                    .child(
                        close
                            .render(palette, Some(&self.close_focus), move |_, app| {
                                let _ =
                                    close_target.update(app, |view, cx| view.attachments.close(cx));
                            })
                            .debug_selector(|| "media-viewer-close".into()),
                    ),
            )
            .child(contents);
        let layer = div()
            .relative()
            .w(viewport.width)
            .h(viewport.height)
            .flex()
            .items_center()
            .justify_center()
            .child(backdrop)
            .child(panel);
        Some(
            div()
                .absolute()
                .size(px(0.))
                .child(
                    deferred(anchored().position(point(px(0.), px(0.))).child(layer))
                        .with_priority(4),
                )
                .into_any_element(),
        )
    }
}

/// Общие действия, которыми список оснащает карточки только при отрисовке.
#[derive(Clone)]
pub(super) struct Cards {
    target: WeakEntity<Conversation>, // Действие исчезает вместе с разговором.
    enabled: bool,                    // Только действующая область может открыть фото.
    preview: Option<(AttachmentKey, Arc<RenderImage>)>, // Изображение последнего открытия.
    focus: Rc<RefCell<HashMap<AttachmentKey, FocusHandle>>>, // Фокус переживает перерисовку строки.
}

impl Cards {
    /// Показывает стабильную карточку. Лишь фото открывает просмотр этого среза.
    pub(super) fn render(
        &self,
        key: AttachmentKey,
        reference: &MediaReference,
        palette: Palette,
        app: &mut App,
    ) -> Stateful<Div> {
        let is_photo = reference.kind == MediaKind::Image;
        let name: SharedString = reference
            .name
            .as_deref()
            .or_else(|| (!reference.label.trim().is_empty()).then_some(reference.label.as_str()))
            .unwrap_or(if is_photo {
                "Фото"
            } else {
                "Вложение"
            })
            .to_owned()
            .into();
        let card = div()
            .id(SharedString::from(format!(
                "attachment-{}-{}",
                key.message, key.position
            )))
            .debug_selector(move || format!("history-attachment-{}-{}", key.message, key.position))
            .w(px(242.))
            .max_w_full()
            .h(px(60.))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded(px(8.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.card)
            .child(
                div()
                    .w(px(48.))
                    .h(px(42.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(
                        |slot| match self.preview.as_ref().filter(|(shown, _)| *shown == key) {
                            Some((_, image)) => slot.child(
                                img(ImageSource::Render(image.clone()))
                                    .size_full()
                                    .object_fit(ObjectFit::Contain)
                                    .debug_selector(|| "history-attachment-preview".into()),
                            ),
                            None => {
                                slot.child(Icon::Attach.element(px(20.), palette.text_secondary))
                            }
                        },
                    ),
            )
            .child(
                label(palette, TextRole::Label, name.clone())
                    .min_w_0()
                    .whitespace_nowrap()
                    .overflow_hidden(),
            );
        if !self.enabled || !is_photo {
            return card;
        }
        let focus = self
            .focus
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| app.focus_handle().tab_stop(true))
            .clone();
        let mouse_focus = focus.clone();
        let trigger = focus.clone();
        let target = self.target.clone();
        activate(
            card.hover(|style| style.bg(palette.card_hover)),
            Some(&focus),
            palette,
            move |_, app| {
                let _ = target.update(app, |view, cx| {
                    view.open_attachment(key, name.clone(), trigger.clone(), cx);
                });
            },
        )
        .on_mouse_down(MouseButton::Left, move |_, window, app| {
            window.focus(&mouse_focus);
            app.stop_propagation();
        })
    }
}

impl Conversation {
    /// Использует уже подтверждённый доступ для новой страницы галереи.
    pub(super) fn gallery_access(&self) -> Option<MediaAccess> {
        self.has_access()
            .then(|| self.attachments.access.clone())
            .flatten()
    }

    /// Показывает миниатюру последней открытой плитки даже до перечитывания страницы.
    pub(super) fn gallery_photo_thumbnail(
        &self,
        item: &MediaHistoryItem,
    ) -> Option<&Arc<RenderImage>> {
        self.attachments
            .gallery_preview
            .as_ref()
            .filter(|(key, _)| key.message == item.message_id && key.file == item.file_id)
            .map(|(_, image)| image)
    }

    /// Открывает фото из правой панели, в том числе из другой темы выбранного стрима.
    pub(super) fn open_gallery_photo(
        &mut self,
        item: MediaHistoryItem,
        whole_stream: bool,
        source: FocusHandle,
        cx: &mut Context<Self>,
    ) {
        if !self.has_access() || item.kind != MediaKind::Image {
            return;
        }
        let key = AttachmentKey {
            message: item.message_id,
            file: item.file_id,
            position: 0,
        };
        let title = item.name.as_deref().unwrap_or("Фото").to_owned().into();
        self.show_photo(
            key,
            title,
            source,
            PhotoTarget::Gallery { item, whole_stream },
            cx,
        );
    }

    /// Позволяет приёмке дождаться актуального кадра или закрытия просмотра.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_photo(&self) -> (bool, bool, bool) {
        let viewer = self.attachments.viewer.as_ref();
        (
            viewer.is_some(),
            viewer.is_some_and(|viewer| matches!(viewer.state, PictureState::Ready(_))),
            self.attachments.preview.is_some() || self.attachments.gallery_preview.is_some(),
        )
    }

    /// Открывает одну попытку и читает фото через допуск от уже показанного окна.
    fn open_attachment(
        &mut self,
        key: AttachmentKey,
        title: SharedString,
        source: FocusHandle,
        cx: &mut Context<Self>,
    ) {
        if !self.has_access() || !self.viewport.has_attachment(key) {
            return;
        }
        self.show_photo(key, title, source, PhotoTarget::History, cx);
    }

    /// Запускает общий просмотр с выбранной проверкой источника.
    fn show_photo(
        &mut self,
        key: AttachmentKey,
        title: SharedString,
        source: FocusHandle,
        origin: PhotoTarget,
        cx: &mut Context<Self>,
    ) {
        let (Some(runtime), Some(access)) = (
            self.attachments.runtime.clone(),
            self.attachments.access.clone(),
        ) else {
            return;
        };
        let attempt = Uuid::new_v4();
        let from_gallery = matches!(origin, PhotoTarget::Gallery { .. });
        log::debug!("gpui_media op_id={attempt} event=viewer_requested");
        let address = self.address.clone();
        self.attachments.close(cx);
        self.attachments.restore = None;
        self.attachments.viewer = Some(Viewer {
            key,
            attempt,
            title,
            source,
            state: PictureState::Loading,
            focus_on_mount: true,
            from_gallery,
        });
        self.attachments.task = Some(cx.spawn(async move |target, cx| {
            let reply = runtime
                .conversation(address, move |services, _| async move {
                    let file = match origin {
                        PhotoTarget::History => {
                            services
                                .messenger
                                .open_media(&services.sync, &access, key.message, key.file)
                                .await
                        }
                        PhotoTarget::Gallery { item, whole_stream } => {
                            services
                                .messenger
                                .open_gallery_media(&services.sync, &access, whole_stream, &item)
                                .await
                        }
                    }
                    .map_err(|_| PictureError::Unavailable)?;
                    log::debug!("gpui_media op_id={attempt} event=source_opened");
                    tokio::task::spawn_blocking(move || decode(file))
                        .await
                        .map_err(|_| PictureError::Unavailable)?
                })
                .await;
            let _ = target.update(cx, |view, cx| {
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                if !view.has_access() || (!from_gallery && !view.viewport.has_attachment(key)) {
                    return;
                }
                let Some(viewer) = view
                    .attachments
                    .viewer
                    .as_mut()
                    .filter(|viewer| viewer.attempt == attempt)
                else {
                    return;
                };
                let preview = if from_gallery {
                    &mut view.attachments.gallery_preview
                } else {
                    &mut view.attachments.preview
                };
                viewer.state = match result.unwrap_or(Err(PictureError::Unavailable)) {
                    Ok(photo) => {
                        log::debug!("gpui_media op_id={attempt} event=viewer_ready");
                        if let Some((_, image)) = preview.replace((key, photo.thumb)) {
                            cx.defer(move |app| app.drop_image(image, None));
                        }
                        PictureState::Ready(photo.full)
                    }
                    Err(error) => {
                        log::debug!(
                            "gpui_media op_id={attempt} event=viewer_failed reason={error:?}"
                        );
                        if preview.as_ref().is_some_and(|(shown, _)| *shown == key)
                            && let Some((_, image)) = preview.take()
                        {
                            cx.defer(move |app| app.drop_image(image, None));
                        }
                        PictureState::Failed(error)
                    }
                };
                cx.notify();
            });
        }));
        cx.notify();
    }
}

/// Читает ограниченный оригинал и принимает только поддерживаемый растровый формат.
fn decode_original(file: MediaFile) -> Result<DynamicImage, PictureError> {
    if file.size() > MAX_ENCODED_BYTES {
        return Err(PictureError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(MAX_ENCODED_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| PictureError::Unavailable)?;
    if bytes.len() as u64 > MAX_ENCODED_BYTES {
        return Err(PictureError::TooLarge);
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| PictureError::Unsupported)?;
    if !matches!(
        reader.format(),
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP)
    ) {
        return Err(PictureError::Unsupported);
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().map_err(|_| PictureError::Unsupported)
}

/// Возвращает маленький кадр для плитки, если локальное фото удалось прочитать.
pub(super) fn decode_thumbnail(file: MediaFile) -> Option<Arc<RenderImage>> {
    let decoded = decode_original(file).ok()?;
    Some(render_image(decoded.thumbnail(96, 91).into_rgba8()))
}

/// Готовит кадры для полного просмотра и карточки, декодируя оригинал один раз.
fn decode(file: MediaFile) -> Result<Photo, PictureError> {
    let decoded = decode_original(file)?;
    let thumb = decoded.thumbnail(96, 84).into_rgba8();
    let full = decoded.into_rgba8();
    let full = if full.width().max(full.height()) > MAX_VIEW_SIDE {
        let factor = f64::from(MAX_VIEW_SIDE) / f64::from(full.width().max(full.height()));
        imageops::thumbnail(
            &full,
            (f64::from(full.width()) * factor).max(1.) as u32,
            (f64::from(full.height()) * factor).max(1.) as u32,
        )
    } else {
        full
    };
    Ok(Photo {
        full: render_image(full),
        thumb: render_image(thumb),
    })
}

/// Передаёт GPUI неподвижный кадр с его порядком каналов.
fn render_image(mut rgba: RgbaImage) -> Arc<RenderImage> {
    for color in rgba.pixels_mut() {
        color.0.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![Frame::new(rgba)]))
}
