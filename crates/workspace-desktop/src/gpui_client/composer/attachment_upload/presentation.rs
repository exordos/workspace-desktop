//! Рисует полосу вложений, карточки с ходом загрузки и перетаскивание картинок в текст.
//! Состояние передачи и действия со ссылками остаются у композера.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    f32::consts::TAU,
    rc::Rc,
    sync::Arc,
};

use gpui::{
    App, Corners, Div, EntityId, FocusHandle, FontWeight, PathBuilder, Pixels, Point, Render,
    RenderImage, ScrollHandle, SharedString, Stateful, WeakEntity, Window, canvas, div, img, point,
    prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::files::ProtectedPreviewPermit;

use super::Composer;
use crate::gpui_client::{
    preferences::Language,
    ui::{Icon, Palette, activate, popup},
};

/// Устойчивый адрес карточки. Порядок в полосе может измениться до клика.
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub(in crate::gpui_client::composer) enum CardId {
    Upload(Uuid), // Локальный идентификатор передачи.
    Reference {
        id: Uuid,      // Идентификатор файла на сервере.
        offset: usize, // Начало ссылки в текущем тексте, если она встречается несколько раз.
    },
}

impl CardId {
    /// Использует локальный идентификатор или пару ссылки и позиции для фокуса GPUI.
    fn element_id(self, action: &str) -> SharedString {
        match self {
            Self::Upload(id) => format!("attachment-{id}-{action}").into(),
            Self::Reference { id, offset } => format!("attachment-{id}-{offset}-{action}").into(),
        }
    }
}

/// Стадия передачи, которую видит пользователь на месте вложения.
#[derive(Clone, Copy)]
pub(super) enum CardStatus {
    Validating,    // Чтение и проверка локального файла.
    Queued,        // Файл ждёт свободного места для передачи.
    Uploading(u8), // Доля переданных данных в целых процентах.
    Ready,         // Ссылку можно отправить с сообщением.
    Error {
        key: &'static str, // Ключ перевода сообщения об ошибке.
        retryable: bool,   // Можно запустить ещё одну передачу того же файла.
    },
}

impl CardStatus {
    /// Возвращает название стадии для проверки карточки в окне.
    fn selector(&self) -> &'static str {
        match self {
            Self::Validating => "validating",
            Self::Queued => "queued",
            Self::Uploading(_) => "uploading",
            Self::Ready => "ready",
            Self::Error { .. } => "error",
        }
    }

    /// Показывает сведения о готовом файле или доступное пояснение к текущей стадии.
    fn detail(&self, card: &Card, language: Language) -> String {
        match self {
            Self::Ready => metadata(&card.name, card.content_type.as_deref(), card.size),
            Self::Validating => language
                .tr("desktop.messenger.composer.attachmentValidating")
                .to_owned(),
            Self::Queued => language
                .tr("desktop.messenger.composer.attachmentQueued")
                .to_owned(),
            Self::Uploading(progress) => language.tr_with(
                "desktop.messenger.composer.attachmentProgress",
                &[("percent", &(*progress).min(100).to_string())],
            ),
            Self::Error { key, .. } => language.tr(key).to_owned(),
        }
    }
}

/// Снимок одной карточки в порядке выбора файлов для текущего черновика.
#[derive(Clone)]
pub(super) struct Card {
    pub(super) id: CardId,                   // Адрес карточки для действий.
    pub(super) name: String,                 // Видимое имя файла.
    pub(super) content_type: Option<String>, // MIME, если известен.
    pub(super) size: Option<u64>,            // Размер в байтах, если известен.
    pub(super) preview: Option<CardPreview>, // Уменьшенная картинка и её допуск, если нужно.
    pub(super) status: CardStatus,           // Состояние загрузки для этого файла.
    pub(super) can_remove: bool,             // Доступны ли изменение черновика и отмена.
    pub(super) can_inline: bool,             // Можно ли помещать изображение внутрь текста.
    pub(super) in_text: bool,                // Изображение уже находится в тексте.
}

/// Кадр карточки из локального файла или из защищённого исходного сообщения.
#[derive(Clone)]
pub(super) enum CardPreview {
    Plain(Arc<RenderImage>), // Готовые открытые пиксели.
    Protected {
        image: Arc<RenderImage>, // Пиксели, доступные только внутри проверки допуска.
        permit: ProtectedPreviewPermit, // Core отзывает допуск к ключу, сообщению и файлу.
    },
}

impl CardPreview {
    /// Проверяет доступность картинки для действий, например перетаскивания.
    pub(super) fn available(&self) -> bool {
        match self {
            Self::Plain(_) => true,
            Self::Protected { permit, .. } => permit.with_current(|| ()).is_some(),
        }
    }

    /// Убирает текстуру, когда карточка покидает композер.
    pub(super) fn release(self, app: &mut App) {
        let image = match self {
            Self::Plain(image) | Self::Protected { image, .. } => image,
        };
        app.defer(move |app| app.drop_image(image, None));
    }
}

/// Идентификатор карточки, передаваемый нативным перетаскиванием в редактор.
pub(in crate::gpui_client::composer) struct AttachmentDrag {
    pub(in crate::gpui_client::composer) id: CardId, // Карточка-источник перетаскивания.
    pub(in crate::gpui_client::composer) composer: EntityId, // Композер, которому доступен файл.
}

/// Помнит прокрутку ряда и стабильные группы фокуса карточек.
#[derive(Default)]
pub(super) struct AttachmentStrip {
    scroll: ScrollHandle,                          // Горизонтальная позиция ряда.
    groups: RefCell<HashMap<CardId, FocusHandle>>, // Карточки с живыми кнопками.
    focused: Cell<Option<CardId>>,                 // Карточка с последним фокусом.
    width: Rc<Cell<Pixels>>,                       // Ширина ряда до изменения размера окна.
}

impl AttachmentStrip {
    /// Собирает один горизонтальный ряд и доводит его до карточки, выбранной через Tab.
    pub(super) fn render(
        &self,
        cards: Vec<Card>,
        palette: Palette,
        language: Language,
        composer: WeakEntity<Composer>,
        window: &Window,
        app: &App,
    ) -> Option<Stateful<Div>> {
        let mut groups = self.groups.borrow_mut();
        groups.retain(|id, _| cards.iter().any(|card| card.id == *id));
        if groups.is_empty() {
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
        let groups: Vec<_> = cards
            .iter()
            .map(|card| {
                groups
                    .entry(card.id)
                    .or_insert_with(|| app.focus_handle().tab_stop(false))
                    .clone()
            })
            .collect();
        let focused = groups
            .iter()
            .enumerate()
            .find(|(_, handle)| handle.contains_focused(window, app));
        let card_id = focused.map(|(index, _)| cards[index].id);
        if self.focused.replace(card_id) != card_id
            && let Some((index, _)) = focused
        {
            self.scroll.scroll_to_item(index);
        }
        if cards.is_empty() {
            return None;
        }
        let has_inline = cards.iter().any(|card| card.in_text);
        let scroll = self.scroll.clone();
        let previous_width = self.width.clone();
        let focused = focused.map(|(index, handle)| (index, handle.clone()));
        Some(
            div()
                .on_children_prepainted(move |_, window, app| {
                    let width = scroll.bounds().size.width;
                    if previous_width.replace(width) != width
                        && let Some((index, handle)) = focused.clone()
                    {
                        // Новая ширина уже известна. Прокрутка применяется на следующем кадре.
                        let scroll = scroll.clone();
                        window.defer(app, move |window, app| {
                            if handle.contains_focused(window, app) {
                                scroll.scroll_to_item(index);
                                window.refresh();
                            }
                        });
                    }
                })
                .id("composer-attachments")
                .debug_selector(|| "composer-attachments".into())
                .flex()
                .flex_none()
                .w_full()
                .min_w_0()
                .gap(px(10.))
                .when(has_inline, |strip| strip.pb(px(6.)))
                .overflow_x_scroll()
                .track_scroll(&self.scroll)
                .children(cards.into_iter().zip(groups).enumerate().map(
                    |(index, (card, group))| {
                        render_card(card, index, palette, language, &composer).track_focus(&group)
                    },
                )),
        )
    }
}

/// Рисует карточку и привязывает клики к устойчивому идентификатору файла.
fn render_card(
    card: Card,
    index: usize,
    palette: Palette,
    language: Language,
    composer: &WeakEntity<Composer>,
) -> Stateful<Div> {
    let id = card.id;
    let selector = card.status.selector();
    let draggable = matches!(card.status, CardStatus::Ready)
        && card.can_inline
        && !card.in_text
        && card.preview.as_ref().is_some_and(CardPreview::available);
    let removable = card.can_remove;
    let retryable = removable
        && matches!(
            card.status,
            CardStatus::Error {
                retryable: true,
                ..
            }
        );
    let preview_slot = preview(&card, palette).when(draggable, |slot| {
        slot.debug_selector(move || format!("composer-attachment-drag-{index}"))
    });
    let frame = card_frame(&card, palette, language, preview_slot)
        .id(id.element_id("card"))
        .debug_selector(move || format!("composer-attachment-{selector}-{index}"))
        .when(card.in_text, |frame| {
            frame.group("composer-attachment-inline")
        })
        .children((retryable || removable).then(|| {
            div()
                .flex()
                .items_center()
                .flex_none()
                .children(retryable.then(|| {
                    let composer = composer.clone();
                    card_action(
                        id.element_id("retry"),
                        palette,
                        Icon::AttachmentRetry,
                        language.tr_with(
                            "desktop.messenger.composer.attachmentRetry",
                            &[("name", &card.name)],
                        ),
                        move |_, app| {
                            let _ = composer.update(app, |composer, cx| {
                                composer.retry_attachment_card(id, cx);
                            });
                        },
                    )
                    .debug_selector(move || format!("composer-attachment-retry-{index}"))
                }))
                .children(removable.then(|| {
                    let composer = composer.clone();
                    let key = if matches!(card.status, CardStatus::Uploading(_)) {
                        "desktop.messenger.composer.attachmentCancel"
                    } else {
                        "desktop.messenger.composer.attachmentRemove"
                    };
                    card_action(
                        id.element_id("remove"),
                        palette,
                        Icon::Close,
                        language.tr_with(key, &[("name", &card.name)]),
                        move |window, app| {
                            let _ = composer.update(app, |composer, cx| {
                                composer.remove_attachment_card(id, window, cx);
                            });
                        },
                    )
                    .debug_selector(move || format!("composer-attachment-remove-{index}"))
                }))
        }))
        .children(
            card.in_text
                .then(|| inline_badge(&card, index, palette, language, composer)),
        );
    if draggable {
        let drag_label = language.tr("desktop.messenger.composer.attachmentDragIntoText");
        frame
            .cursor_grab()
            .tooltip(move |_, app| popup::tooltip(palette, drag_label.into(), app))
            .on_drag(
                AttachmentDrag {
                    id,
                    composer: composer.entity_id(),
                },
                move |_, offset, _, app| {
                    app.new(|_| AttachmentDragPreview {
                        card: card.clone(),
                        palette,
                        language,
                        pointer_x: offset.x,
                    })
                },
            )
    } else {
        frame
    }
}

/// Рисует рамку 240 × 58, подписи и слот превью. Действия занимают правый край.
fn card_frame(card: &Card, palette: Palette, language: Language, preview: Div) -> Div {
    let failed = matches!(card.status, CardStatus::Error { .. });
    div()
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .w(px(240.))
        .max_w_full()
        .h(px(58.))
        .gap(px(12.))
        .p(px(8.))
        .rounded(if failed { px(12.) } else { px(8.) })
        .border_1()
        .border_color(if failed {
            palette.danger
        } else if card.in_text {
            palette.accent
        } else {
            palette.border
        })
        .bg(palette.surface)
        .child(preview)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.))
                .child(
                    div()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .line_height(px(14.))
                        .text_color(palette.text)
                        .child(card.name.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .mt(px(2.))
                        .line_height(px(15.))
                        .text_color(if failed {
                            palette.danger
                        } else {
                            palette.text_muted
                        })
                        .child(card.status.detail(card, language)),
                ),
        )
}

/// Выбирает картинку, индикатор или заглушку слева от имени файла.
fn preview(card: &Card, palette: Palette) -> Div {
    let slot = div()
        .size(px(40.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .bg(palette.field);
    match &card.status {
        CardStatus::Uploading(percent) => slot.child(progress_ring(*percent, palette)),
        CardStatus::Ready => {
            let icon = file_icon(&card.name, card.content_type.as_deref());
            match &card.preview {
                Some(CardPreview::Plain(image)) => {
                    slot.child(img(image.clone()).size_full().rounded(px(8.)))
                }
                Some(CardPreview::Protected { image, permit }) => slot
                    .relative()
                    .children(
                        (!card.preview.as_ref().is_some_and(CardPreview::available))
                            .then(|| icon.element(px(18.), palette.text_muted)),
                    )
                    .child(protected_preview(image.clone(), permit.clone())),
                None => slot.child(icon.element(px(18.), palette.text_muted)),
            }
        }
        CardStatus::Error { .. } => {
            slot.child(Icon::AttachmentUploadError.element(px(18.), palette.danger))
        }
        CardStatus::Validating | CardStatus::Queued => {
            slot.child(Icon::GalleryFile.element(px(18.), palette.text_muted))
        }
    }
}

/// Рисует пиксели только внутри действующего допуска, в том числе в тени перетаскивания.
fn protected_preview(image: Arc<RenderImage>, permit: ProtectedPreviewPermit) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let _ = permit.with_current(|| {
                window.paint_image(bounds, Corners::all(px(8.)), image.clone(), 0, false)
            });
        },
    )
    .absolute()
    .inset_0()
}

/// Рисует 18-пиксельное кольцо с дугой фактического прогресса.
fn progress_ring(percent: u8, palette: Palette) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let center = bounds.center();
            if let Some(track) = ring_path(center, 1.) {
                window.paint_path(track, palette.text_muted.opacity(0.24));
            }
            if let Some(progress) = ring_path(center, f32::from(percent.min(100)) / 100.) {
                window.paint_path(progress, palette.accent);
            }
        },
    )
    .size(px(18.))
}

/// Собирает дугу по часовой стрелке, начиная с верхней точки кольца.
fn ring_path(center: Point<gpui::Pixels>, fraction: f32) -> Option<gpui::Path<gpui::Pixels>> {
    if fraction == 0. {
        return None;
    }
    let radius = px(7.5);
    let radii = point(radius, radius);
    let end = |part: f32| {
        let (sin, cos) = (TAU * part).sin_cos();
        point(center.x + radius * sin, center.y - radius * cos)
    };
    let mut path = PathBuilder::stroke(px(3.));
    path.move_to(end(0.));
    // У одной SVG-дуги должны отличаться начало и конец, даже при 100%.
    if fraction > 0.5 {
        path.arc_to(radii, px(0.), false, true, end(0.5));
    }
    path.arc_to(radii, px(0.), false, true, end(fraction));
    path.build().ok()
}

/// Рисует круглую кнопку с существующим управлением фокусом и подсказкой.
fn card_action(
    id: SharedString,
    palette: Palette,
    icon: Icon,
    caption: String,
    action: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    activate(
        div()
            .id(id)
            .size(px(24.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_1()
            .border_color(gpui::transparent_black())
            .hover(move |style| style.bg(palette.field))
            .child(icon.element(
                if icon == Icon::AttachmentRetry {
                    px(16.)
                } else {
                    px(12.)
                },
                palette.text_muted,
            ))
            .tooltip(move |_, app| popup::tooltip(palette, caption.clone().into(), app)),
        None,
        palette,
        action,
    )
}

/// Помечает картинку внутри текста и возвращает её к обычному вложению по клику.
fn inline_badge(
    card: &Card,
    index: usize,
    palette: Palette,
    language: Language,
    composer: &WeakEntity<Composer>,
) -> Stateful<Div> {
    let id = card.id;
    let composer = composer.clone();
    let badge = div()
        .id(id.element_id("inline"))
        .debug_selector(move || format!("composer-attachment-in-text-{index}"))
        .absolute()
        .right(px(0.))
        .bottom(px(-4.))
        .h(px(20.))
        .px(px(4.))
        .flex()
        .items_center()
        .gap(px(2.))
        .rounded_full()
        .border_1()
        .border_color(palette.surface)
        .bg(palette.accent)
        .text_size(px(9.))
        .font_weight(FontWeight::BOLD)
        .text_color(palette.on_accent)
        .child("TX");
    if card.can_remove {
        let caption = language.tr("desktop.messenger.composer.attachmentRemoveFromText");
        let close = activate(
            div()
                .id(id.element_id("move-out"))
                .debug_selector(move || format!("composer-attachment-remove-from-text-{index}"))
                .w_0()
                .h(px(16.))
                .opacity(0.)
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(gpui::transparent_black())
                .group_hover("composer-attachment-inline", |style| {
                    style.w(px(16.)).opacity(1.)
                })
                .child(Icon::Close.element(px(11.), palette.on_accent))
                .tooltip(move |_, app| popup::tooltip(palette, caption.into(), app)),
            None,
            palette,
            move |window, app| {
                let _ = composer.update(app, |composer, cx| {
                    composer.remove_inline_attachment(id, window, cx);
                });
            },
        )
        .focus(move |style| style.w(px(16.)).opacity(1.).border_color(palette.on_accent));
        badge.child(close)
    } else {
        badge
    }
}

/// Тень под указателем во время перетаскивания картинки в текст.
struct AttachmentDragPreview {
    card: Card,         // Данные исходной карточки и её уменьшенное изображение.
    palette: Palette,   // Палитра окна, которое начало перетаскивание.
    language: Language, // Язык подписей исходной карточки.
    pointer_x: Pixels,  // Положение курсора внутри исходной карточки по горизонтали.
}

impl Render for AttachmentDragPreview {
    /// Показывает источник справа от указателя, оставляя курсор над целевой строкой.
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div().pl(self.pointer_x + px(24.)).children(
            self.card
                .preview
                .as_ref()
                .filter(|preview| preview.available())
                .map(|_| {
                    card_frame(
                        &self.card,
                        self.palette,
                        self.language,
                        preview(&self.card, self.palette),
                    )
                }),
        )
    }
}

/// Убирает параметры MIME и приводит регистр к виду, общему для сравнения и подписи.
fn mime_type(content_type: Option<&str>) -> String {
    content_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Берёт последнее расширение, если оно содержит только латиницу или цифры.
fn extension(name: &str) -> Option<String> {
    let (base, extension) = name.trim().rsplit_once('.')?;
    let extension = extension.trim();
    (!base.is_empty()
        && (1..=16).contains(&extension.len())
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    .then(|| extension.to_ascii_lowercase())
}

/// Подбирает значок по MIME. Расширение нужно для файлов без точного MIME.
fn file_icon(name: &str, content_type: Option<&str>) -> Icon {
    let mime = mime_type(content_type);
    if mime.starts_with("image/") {
        return Icon::GalleryPhoto;
    }
    if mime.starts_with("video/") {
        return Icon::GalleryVideo;
    }
    if mime.is_empty() || mime == "application/octet-stream" {
        return match extension(name).as_deref() {
            Some("avif" | "bmp" | "gif" | "heic" | "jpeg" | "jpg" | "png" | "svg" | "webp") => {
                Icon::GalleryPhoto
            }
            Some("avi" | "m4v" | "mov" | "mp4" | "mpeg" | "mpg" | "webm") => Icon::GalleryVideo,
            _ => Icon::GalleryFile,
        };
    }
    Icon::GalleryFile
}

/// Формирует подпись готового вложения как в Electron, например «JPEG · 42 KB».
fn metadata(name: &str, content_type: Option<&str>, size: Option<u64>) -> String {
    let format = extension(name).map_or_else(
        || {
            match mime_type(content_type).as_str() {
                "application/pdf" => "PDF",
                "application/zip" | "application/x-zip-compressed" => "ZIP",
                "application/vnd.rar" | "application/x-rar-compressed" => "RAR",
                "application/x-7z-compressed" => "7Z",
                "application/gzip" => "GZ",
                "application/msword" => "DOC",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "DOCX",
                "application/vnd.ms-excel" => "XLS",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "XLSX",
                "application/vnd.ms-powerpoint" => "PPT",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
                    "PPTX"
                }
                "text/plain" => "TXT",
                "text/csv" => "CSV",
                _ => "FILE",
            }
            .to_owned()
        },
        |extension| {
            extension
                .chars()
                .take(4)
                .collect::<String>()
                .to_ascii_uppercase()
        },
    );
    if let Some(bytes) = size {
        format!("{format} · {}", size_label(bytes))
    } else {
        format
    }
}

/// Округляет килобайты до целых, мегабайты и гигабайты до одной значащей дробной цифры.
fn size_label(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    match bytes {
        0..KIB => format!("{bytes} B"),
        KIB..MIB => format!("{} KB", (bytes as f64 / KIB as f64).round()),
        MIB..GIB => format!("{} MB", (bytes as f64 / MIB as f64 * 10.).round() / 10.),
        _ => format!("{} GB", (bytes as f64 / GIB as f64 * 10.).round() / 10.),
    }
}

pub(super) use thumbnail::decode_thumbnail;
mod thumbnail;

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет подпись файла и приоритет известного формата над расширением.
    #[test]
    fn attachment_card_metadata_uses_reference_format() {
        assert_eq!(metadata("photo.jpeg", None, Some(1699)), "JPEG · 2 KB");
        assert_eq!(
            metadata("draft", Some("TEXT/PLAIN; charset=UTF-8"), None),
            "TXT"
        );
        assert_eq!(size_label(1_258_291), "1.2 MB");
        assert_eq!(file_icon("clip.MOV", None), Icon::GalleryVideo);
        assert_eq!(
            file_icon("renamed.png", Some("application/pdf")),
            Icon::GalleryFile
        );
    }

    /// Проверяет, что кольцо скрывает пустую часть и рисуется на граничных процентах.
    #[test]
    fn attachment_progress_has_a_complete_ring() {
        let center = point(px(9.), px(9.));
        assert!(ring_path(center, 0.).is_none());
        for fraction in [0.01, 0.5, 0.75, 1.] {
            assert!(ring_path(center, fraction).is_some());
        }
    }

    #[cfg(feature = "gpui-test")]
    mod keyboard {
        use gpui::{Context, KeyBinding, TestAppContext, size};

        use super::*;
        use crate::gpui_client::ui::ThemeMode;

        gpui::actions!(attachment_strip_test, [NextAttachment]);

        /// Табуляция начинается перед рядом из трёх уже загруженных файлов.
        struct View {
            strip: AttachmentStrip, // Та же полоса, которую рисует рабочий композер.
            initial: FocusHandle,   // Исходный фокус перед кнопками вложений.
        }

        impl Render for View {
            /// Подставляет данные без сети, сохраняя настоящее управление фокусом GPUI.
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let cards = (1..=3)
                    .map(|index| Card {
                        id: CardId::Upload(Uuid::from_u128(index)),
                        name: format!("Файл {index}.txt"),
                        content_type: Some("text/plain".into()),
                        size: Some(12),
                        preview: None,
                        status: CardStatus::Ready,
                        can_remove: true,
                        can_inline: false,
                        in_text: false,
                    })
                    .collect();
                div()
                    .size_full()
                    .track_focus(&self.initial)
                    .on_action(|_: &NextAttachment, window, _| window.focus_next())
                    .children(self.strip.render(
                        cards,
                        Palette::for_mode(ThemeMode::Light),
                        Language::Ru,
                        WeakEntity::new_invalid(),
                        window,
                        cx,
                    ))
            }
        }

        /// Tab прокручивает карточку с выбранной кнопкой в область видимости.
        #[gpui::test]
        fn attachment_strip_scrolls_to_an_offscreen_card_on_tab(cx: &mut TestAppContext) {
            cx.update(|app| app.bind_keys([KeyBinding::new("tab", NextAttachment, None)]));
            let (_, ui) = cx.add_window_view(|window, cx| {
                let initial = cx.focus_handle();
                window.focus(&initial);
                View {
                    strip: AttachmentStrip::default(),
                    initial,
                }
            });
            ui.simulate_resize(size(px(275.), px(140.)));
            ui.run_until_parked();
            let strip = ui.debug_bounds("composer-attachments").unwrap();
            let second = ui.debug_bounds("composer-attachment-ready-1").unwrap();
            assert_eq!(strip.size.height, px(58.));
            assert_eq!(second.size.width, px(240.));
            assert!(second.right() > strip.right());

            for _ in 0..2 {
                ui.simulate_keystrokes("tab");
                ui.run_until_parked();
            }
            let second = ui.debug_bounds("composer-attachment-ready-1").unwrap();
            let remove = ui.debug_bounds("composer-attachment-remove-1").unwrap();
            assert!(second.left() >= strip.left());
            assert!(remove.right() <= strip.right());
        }
    }
}
