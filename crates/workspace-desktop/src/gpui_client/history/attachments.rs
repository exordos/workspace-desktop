//! Показывает вложения в ленте и открывает фото и видео через кэш Core.
//! Здесь находятся карточка, состояние просмотра, загрузка и ограниченное декодирование.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::{Cursor, Read},
    rc::Rc,
    sync::Arc,
};

use gpui::{
    AnyElement, App, Context, Corners, Div, FocusHandle, ImageSource, IntoElement, MouseButton,
    ObjectFit, RenderImage, SharedString, Stateful, Task, WeakEntity, Window, actions, anchored,
    canvas, deferred, div, img, point, prelude::*, px, rgba, size,
};
use image::{
    DynamicImage, Frame, ImageDecoder, ImageFormat, ImageReader, Limits, RgbaImage, imageops,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        encryption::EncryptionActivation,
        files::{MediaFile, ProtectedPreviewPermit},
        history::MediaAccess,
        references::MediaReference,
    },
    storage::media::{MediaHistoryItem, MediaKind},
};

use super::super::contracts::ConversationAddress;
use super::Conversation;
use super::media_album::Tile;
#[path = "attachments/clipboard.rs"]
mod clipboard;
#[path = "attachments/inline.rs"]
mod inline;
#[path = "attachments/protected.rs"]
mod protected;
#[path = "attachments/video.rs"]
mod video;
use crate::gpui_client::{
    preferences::{Language, Preferences},
    runtime::RuntimeClient,
    ui::{Button, ButtonAppearance, Icon, Palette, SURFACE_RADIUS, TextRole, activate, label},
};
pub(super) use protected::ProtectedAttachment;
use protected::{CardAddress, PreviewLoad, ProtectedThumbnails};

const VIEWER_CONTEXT: &str = "WorkspacePhotoViewer";
const MAX_ENCODED_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SOURCE_SIDE: u32 = 4096;
const MAX_VIEW_SIDE: u32 = 2048;

/// Считает вызовы Canvas и кадры, допущенные к выводу изображений.
#[cfg(all(test, feature = "gpui-test"))]
type PaintCounts = Rc<Cell<(usize, usize)>>;

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
    Protected(ProtectedAttachment, EncryptionActivation), // Точный оригинал и допуск среды чтения.
    Gallery {
        item: MediaHistoryItem, // Точное сообщение и вложение из локального списка.
        whole_stream: bool,     // Источник может быть в другой теме стрима.
    },
}

/// Фокус закрытого просмотра и условия его возврата.
pub(super) enum PhotoReturn {
    History(AttachmentKey, FocusHandle), // Карточка должна оставаться видимой в истории.
    Gallery(FocusHandle),                // Плитка из открытого раздела сведений.
    Protected(Uuid, FocusHandle),        // Возвращает фокус карточке в том же видимом бабле.
    Conversation,                        // Источник исчез. Фокус вернётся в разговор.
}

/// Готовые пиксели для просмотра и маленькой карточки.
enum Photo {
    Plain {
        full: Arc<RenderImage>,  // Кадр для открытого просмотра.
        thumb: Arc<RenderImage>, // Уменьшенная копия для обычной карточки.
    },
    Protected(ProtectedPicture), // Пиксели, связанные с точной проверенной версией файла.
}

/// Декодированный кадр живёт отдельно от временного файла и остаётся отзываемым.
#[derive(Clone)]
struct ProtectedPicture {
    image: Arc<RenderImage>, // Пиксели для одного защищённого места в окне.
    permit: ProtectedPreviewPermit, // Core проверяет файл, сообщения, ключи и поколение входа.
}

impl ProtectedPicture {
    /// Проверяет возможность публикации готового кадра без диска и сети.
    fn is_current(&self) -> bool {
        self.permit.with_current(|| ()).is_some()
    }
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
    fn message(self, language: Language) -> &'static str {
        language.tr(match self {
            Self::Unavailable => "desktop.messenger.history.photoUnavailable",
            Self::Unsupported => "desktop.messenger.history.photoUnsupported",
            Self::TooLarge => "desktop.messenger.history.photoTooLarge",
        })
    }
}

/// Видимое состояние оверлея для одной попытки.
enum PictureState {
    Loading,                     // Читаем копию или ждём файлы с сервера.
    Ready(Arc<RenderImage>),     // Декодированный кадр в пропорциях исходника.
    Protected(ProtectedPicture), // Кадр рисуется под отзывным допуском к файлу.
    Failed(PictureError),        // Сообщение об ошибке и кнопка закрытия.
}

/// Открытое намерение пользователя со своим источником возврата фокуса.
struct Viewer {
    key: AttachmentKey,                  // Карточка, открывшая просмотр.
    attempt: Uuid,                       // Применим только ответ этой попытки.
    title: SharedString,                 // Подпись из сообщения.
    source: FocusHandle,                 // Фокус карточки при закрытии.
    state: PictureState,                 // Загрузка, кадр или отказ.
    focus_on_mount: bool,                // Фокус после появления панели в дереве окна.
    from_gallery: bool,                  // Открыто из правой панели, независимо от окна истории.
    holder: Option<ProtectedAttachment>, // Разрешение исходного защищённого бабла или цитаты.
    mounted: Rc<Cell<bool>>, // Закрытие запрещает использовать готовую раскладку старого кадра.
    #[cfg(all(test, feature = "gpui-test"))]
    protected_paints: PaintCounts, // Число попыток и допущенных выводов фото.
}

/// Память одного разговора. Оригинал хранит Core, экран держит последнее превью.
pub(super) struct Attachments {
    pub access: Option<MediaAccess>, // Допуск, полученный при успешном открытии истории.
    pub runtime: Option<RuntimeClient>, // Переносит загрузку на рабочий поток.
    close_focus: FocusHandle,        // Крестик получает фокус при показе оверлея.
    save_focus: FocusHandle,         // Сохранение защищённого оригинала с клавиатуры.
    region: FocusHandle,             // Область клавиш и блокировки фона.
    viewer: Option<Viewer>,          // Открытое фото, по одному на разговор.
    task: Option<Task<()>>,          // Закрытие отменяет ожидание результата.
    clipboard: clipboard::CopyState, // Последний запрос копирования фото из текущей истории.
    inline_thumbs: inline::Thumbnails, // Кадры обычных фото и видео в виртуальном списке.
    video: video::Playback,          // Подготовка и нативное окно текущего видео.
    protected_thumbs: ProtectedThumbnails, // Кадры и запросы только для отрисованных защищённых ссылок.
    gallery_preview: Option<(AttachmentKey, Arc<RenderImage>)>, // Превью только что открытой плитки.
    focus: Rc<RefCell<HashMap<CardAddress, FocusHandle>>>, // Устойчивый фокус исходных и цитируемых карточек.
    restore: Option<PhotoReturn>,                          // Возврат после снятия оверлея.
}

impl Attachments {
    /// Создаёт закрытый просмотр и постоянные клавиатурные точки.
    pub(super) fn new(cx: &mut Context<Conversation>) -> Self {
        Self {
            access: None,
            runtime: None,
            close_focus: cx.focus_handle(),
            save_focus: cx.focus_handle(),
            region: cx.focus_handle(),
            viewer: None,
            task: None,
            clipboard: clipboard::CopyState::default(),
            inline_thumbs: inline::Thumbnails::default(),
            video: video::Playback::default(),
            protected_thumbs: ProtectedThumbnails::default(),
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
    pub(super) fn cards(
        &self,
        address: &ConversationAddress,
        activation: Option<&EncryptionActivation>,
        cx: &Context<Conversation>,
    ) -> Cards {
        Cards {
            target: cx.entity().downgrade(),
            enabled: self.runtime.is_some() && self.access.is_some(),
            inline_thumbs: self.inline_thumbs.clone(),
            inline_load: self.runtime.as_ref().zip(self.access.as_ref()).map(
                |(runtime, access)| inline::Load {
                    runtime: runtime.clone(),
                    access: access.clone(),
                    address: address.clone(),
                },
            ),
            video_state: self.video.card_state(),
            protected_thumbs: self.protected_thumbs.clone(),
            protected_load: self
                .runtime
                .as_ref()
                .zip(self.access.as_ref())
                .zip(activation)
                .map(|((runtime, access), activation)| PreviewLoad {
                    runtime: runtime.clone(),
                    access: access.clone(),
                    activation: activation.clone(),
                    address: address.clone(),
                }),
            focus: self.focus.clone(),
        }
    }

    /// Скрывает кадр и отменяет ожидание; маленький снимок остаётся на карточке.
    pub(super) fn close(&mut self, cx: &mut Context<Conversation>) {
        self.task = None;
        if let Some(viewer) = self.viewer.take() {
            viewer.mounted.set(false);
            log::debug!("gpui_media op_id={} event=viewer_closed", viewer.attempt);
            self.restore = Some(if viewer.from_gallery {
                PhotoReturn::Gallery(viewer.source)
            } else if let Some(media) = viewer.holder {
                PhotoReturn::Protected(media.row_id(), viewer.source)
            } else {
                PhotoReturn::History(viewer.key, viewer.source)
            });
            let image = match viewer.state {
                PictureState::Ready(image)
                | PictureState::Protected(ProtectedPicture { image, .. }) => Some(image),
                _ => None,
            };
            if let Some(image) = image {
                cx.defer(move |app| app.drop_image(image, None));
            }
            cx.notify();
        }
    }

    /// Снимает просмотр, который потерял ключ, даже если его загрузка ещё не завершилась.
    pub(super) fn invalidate_revoked(&mut self, cx: &mut Context<Conversation>) {
        self.video.invalidate_revoked(cx);
        self.protected_thumbs
            .retain(|source| source.with_current(|| ()).is_some(), cx);
        if self.viewer.as_ref().is_some_and(|viewer| {
            viewer.holder.as_ref().is_some_and(|access| access.with_current(|| ()).is_none())
                || matches!(&viewer.state, PictureState::Protected(picture) if !picture.is_current())
        }) {
            self.close(cx);
            self.restore = Some(PhotoReturn::Conversation);
        }
    }

    /// Завершает показ и удаляет пиксели, которым больше нельзя доверять.
    pub(super) fn invalidate(
        &mut self,
        affected: impl Fn(AttachmentKey) -> bool,
        cx: &mut Context<Conversation>,
    ) {
        self.clipboard.invalidate(&affected);
        self.inline_thumbs.retain(|key| !affected(key), cx);
        self.video.invalidate(|key| !affected(key), cx);
        self.protected_thumbs
            .retain(|source| !affected(source.key), cx);
        if self
            .viewer
            .as_ref()
            .is_some_and(|viewer| affected(viewer.key))
        {
            self.close(cx);
            self.restore = Some(PhotoReturn::Conversation);
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
        protected: impl Fn(&ProtectedAttachment) -> bool,
        cx: &mut Context<Conversation>,
    ) {
        self.inline_thumbs.retain(&contains, cx);
        self.video.retain_sources(&contains, &protected, cx);
        if self.viewer.as_ref().is_some_and(|viewer| {
            !viewer.from_gallery
                && match &viewer.holder {
                    Some(holder) => !protected(holder),
                    None => !contains(viewer.key),
                }
        }) {
            self.close(cx);
        }
        self.focus
            .borrow_mut()
            .retain(|(key, quote), _| quote.is_some() || contains(*key));
        self.protected_thumbs.retain(protected, cx);
    }

    /// Рисует самостоятельный слой фото поверх рабочего окна.
    pub(super) fn overlay(
        &mut self,
        window: &mut Window,
        palette: Palette,
        language: Language,
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
        let mut close = Button::new("media-viewer-close", language.tr("common.close"));
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
                .child(label(
                    palette,
                    TextRole::Body,
                    language.tr("desktop.messenger.history.photoOpening"),
                ))
                .debug_selector(|| "media-viewer-loading".into()),
            PictureState::Failed(error) => contents
                .child(label(palette, TextRole::Body, error.message(language)))
                .debug_selector(|| "media-viewer-error".into()),
            PictureState::Ready(image) => contents.child(
                img(ImageSource::Render(image.clone()))
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .debug_selector(|| "media-viewer-image".into()),
            ),
            PictureState::Protected(picture) => contents.child(
                div()
                    .size_full()
                    .child(protected_photo(
                        picture.clone(),
                        viewer.mounted.clone(),
                        None,
                        #[cfg(all(test, feature = "gpui-test"))]
                        viewer.protected_paints.clone(),
                    ))
                    .debug_selector(|| "media-viewer-image".into()),
            ),
        };
        let close_target = cx.entity().downgrade();
        let save = viewer
            .holder
            .as_ref()
            .filter(|holder| holder.with_current(|| ()).is_some())
            .map(|holder| {
                let target = cx.entity().downgrade();
                let holder = holder.clone();
                let focus = self.save_focus.clone();
                let picker_focus = focus.clone();
                Button::new("media-viewer-save", language.tr("common.save"))
                    .render(palette, Some(&focus), move |window, app| {
                        let _ = target.update(app, |view, cx| {
                            view.save_protected_attachment(
                                holder.clone(),
                                picker_focus.clone(),
                                window,
                                cx,
                            );
                        });
                    })
                    .debug_selector(|| "media-viewer-save-protected".into())
            });
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
                let attachments = &view.attachments;
                let can_save = attachments
                    .viewer
                    .as_ref()
                    .and_then(|viewer| viewer.holder.as_ref())
                    .is_some_and(|access| access.with_current(|| ()).is_some());
                let focus = if can_save && attachments.close_focus.is_focused(window) {
                    &attachments.save_focus
                } else {
                    &attachments.close_focus
                };
                window.focus(focus);
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
                        div().flex().gap_1().children(save).child(
                            close
                                .render(palette, Some(&self.close_focus), move |_, app| {
                                    let _ = close_target
                                        .update(app, |view, cx| view.attachments.close(cx));
                                })
                                .debug_selector(|| "media-viewer-close".into()),
                        ),
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
    inline_thumbs: inline::Thumbnails, // Кадры обычных фото и видео.
    inline_load: Option<inline::Load>, // Допуск на запрос из видимой строки.
    video_state: Option<(CardAddress, video::CardState)>, // Подсказка у нажатого видео.
    protected_thumbs: ProtectedThumbnails, // Независимое представление защищённой миниатюры.
    protected_load: Option<PreviewLoad>, // Адрес загрузки и ключ только для открытой истории.
    focus: Rc<RefCell<HashMap<CardAddress, FocusHandle>>>, // Фокус переживает перерисовку строки.
}

impl Cards {
    /// Проверяет, что ссылка может стать кадром в пузыре.
    pub(super) fn shows_inline(&self, reference: &MediaReference) -> bool {
        self.inline_load.is_some() && matches!(reference.kind, MediaKind::Image | MediaKind::Video)
    }

    /// Проверяет допуск открытой истории на защищённые кадры.
    pub(super) fn shows_protected_inline(&self) -> bool {
        self.protected_load.is_some()
    }

    /// Готовит карточку медиа из URL или обычное вложение с именем файла.
    pub(super) fn render(
        &self,
        key: AttachmentKey,
        reference: &MediaReference,
        tile: Option<Tile>,
        palette: Palette,
        language: Language,
        app: &mut App,
    ) -> Stateful<Div> {
        let name: SharedString = reference
            .name
            .as_deref()
            .or_else(|| (!reference.label.trim().is_empty()).then_some(reference.label.as_str()))
            .unwrap_or_else(|| {
                language.tr(match reference.kind {
                    MediaKind::Image => "desktop.messenger.history.photo",
                    MediaKind::Video => "desktop.messenger.history.video",
                    _ => "desktop.messenger.history.attachment",
                })
            })
            .to_owned()
            .into();
        if let Some((tile, load)) = tile.zip(self.inline_load.clone()) {
            let thumbnail = self.inline_thumbs.render(
                key,
                inline::Frame {
                    kind: reference.kind,
                    name: name.clone(),
                    language,
                    tile,
                },
                load,
                self.target.clone(),
                palette,
                app,
            );
            let address = (key, None);
            let card = tile.round(
                card_element(address)
                    .overflow_hidden()
                    .bg(palette.card)
                    .child(thumbnail),
            );
            return self.activate_card(
                self.video_hint(card, address, palette, language),
                address,
                reference.kind,
                name,
                None,
                palette,
                app,
            );
        }
        self.render_file_card(key, name, None, palette, app)
    }

    /// Показывает статус запуска видео поверх его кадра.
    fn video_hint(
        &self,
        card: Stateful<Div>,
        address: CardAddress,
        palette: Palette,
        language: Language,
    ) -> Stateful<Div> {
        let Some((_, state)) = self.video_state.filter(|(target, _)| *target == address) else {
            return card;
        };
        let status = language.tr(match state {
            video::CardState::Loading => "desktop.messenger.history.videoOpening",
            video::CardState::Failed => "desktop.messenger.history.videoUnavailable",
        });
        card.child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .bg(palette.card)
                .px_2()
                .py_1()
                .child(label(palette, TextRole::Caption, status)),
        )
    }

    /// Показывает карточку защищённого файла без передачи имени, URL или превью общему кэшу.
    pub(super) fn render_protected(
        &self,
        source: ProtectedAttachment,
        kind: MediaKind,
        tile: Option<Tile>,
        palette: Palette,
        language: Language,
        app: &mut App,
    ) -> Stateful<Div> {
        let name = language.tr(match kind {
            MediaKind::Image => "desktop.messenger.history.photo",
            MediaKind::Video => "desktop.messenger.history.video",
            _ => "desktop.messenger.history.attachment",
        });
        if matches!(kind, MediaKind::Image | MediaKind::Video)
            && let Some(load) = self.protected_load.as_ref()
            && source.with_current(|| ()).is_some()
        {
            let address = source.card_address();
            let thumbnail = self.protected_thumbs.render(&source, kind, tile, load, app);
            let card = match tile {
                Some(tile) => tile.round(card_element(address).overflow_hidden().bg(palette.card)),
                None => media_card(address, size(px(260.), px(176.)), palette),
            }
            .child(thumbnail)
            .when(kind == MediaKind::Video, |card| {
                card.child(inline::play_indicator())
            });
            return self.activate_card(
                self.video_hint(card, address, palette, language),
                address,
                kind,
                name.into(),
                Some(source),
                palette,
                app,
            );
        }
        self.render_file_card(source.key, name.into(), Some(source), palette, app)
    }

    /// Показывает заголовок файла и подключает сохранение для защищённой ссылки.
    fn render_file_card(
        &self,
        key: AttachmentKey,
        name: SharedString,
        protected: Option<ProtectedAttachment>,
        palette: Palette,
        app: &mut App,
    ) -> Stateful<Div> {
        let address = protected
            .as_ref()
            .map_or((key, None), ProtectedAttachment::card_address);
        let card = media_card(address, size(px(242.), px(60.)), palette)
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .border_1()
            .border_color(palette.border)
            .child(
                div()
                    .w(px(48.))
                    .h(px(42.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::Attach.element(px(20.), palette.text_secondary)),
            )
            .child(
                label(palette, TextRole::Label, name.clone())
                    .min_w_0()
                    .whitespace_nowrap()
                    .overflow_hidden(),
            );
        if !self.enabled || protected.is_none() {
            return card;
        }
        self.activate_card(
            card,
            address,
            MediaKind::File,
            name,
            protected,
            palette,
            app,
        )
    }

    /// Подключает к карточке запуск просмотра или действие с защищённым файлом.
    #[allow(clippy::too_many_arguments)]
    fn activate_card(
        &self,
        card: Stateful<Div>,
        address: CardAddress,
        kind: MediaKind,
        name: SharedString,
        protected: Option<ProtectedAttachment>,
        palette: Palette,
        app: &mut App,
    ) -> Stateful<Div> {
        let key = address.0;
        let focus = self
            .focus
            .borrow_mut()
            .entry(address)
            .or_insert_with(|| app.focus_handle().tab_stop(true))
            .clone();
        let mouse_focus = focus.clone();
        let trigger = focus.clone();
        let target = self.target.clone();
        activate(
            card.hover(|style| style.bg(palette.card_hover)),
            Some(&focus),
            palette,
            move |window, app| {
                let _ = target.update(app, |view, cx| {
                    if kind == MediaKind::Video {
                        view.open_history_video(key, name.clone(), protected.clone(), cx);
                    } else if let Some(holder) = &protected {
                        if kind == MediaKind::Image {
                            view.open_protected_attachment(
                                name.clone(),
                                trigger.clone(),
                                holder.clone(),
                                cx,
                            );
                        } else {
                            view.save_protected_attachment(
                                holder.clone(),
                                trigger.clone(),
                                window,
                                cx,
                            );
                        }
                    } else {
                        view.open_attachment(key, name.clone(), trigger.clone(), cx);
                    }
                });
            },
        )
        .on_mouse_down(MouseButton::Left, move |_, window, app| {
            window.focus(&mouse_focus);
            app.stop_propagation();
        })
    }
}

/// Рисует отдельную карточку для файла или вложения в защищённой цитате.
fn media_card(
    address: CardAddress,
    frame: gpui::Size<gpui::Pixels>,
    palette: Palette,
) -> Stateful<Div> {
    card_element(address)
        .w(frame.width)
        .h(frame.height)
        .max_w_full()
        .rounded(px(8.))
        .overflow_hidden()
        .bg(palette.card)
}

/// Даёт карточке устойчивый селектор в истории или в цитате.
fn card_element(address: CardAddress) -> Stateful<Div> {
    let (key, quote) = address;
    let selector = match quote {
        Some(slot) => format!(
            "history-protected-quote-attachment-{}-{}-{}",
            slot.message, slot.position, key.position
        ),
        None => format!("history-attachment-{}-{}", key.message, key.position),
    };
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .relative()
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
        let language = Preferences::read_for(cx, &self.address.route.owner).language;
        let title = item
            .name
            .as_deref()
            .unwrap_or_else(|| language.tr("desktop.messenger.history.photo"))
            .to_owned()
            .into();
        self.show_photo(
            key,
            title,
            source,
            PhotoTarget::Gallery { item, whole_stream },
            None,
            cx,
        );
    }

    /// Позволяет приёмке дождаться актуального кадра или закрытия просмотра.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_photo(&self, app: &App) -> (bool, bool, bool) {
        let viewer = self.attachments.viewer.as_ref();
        (
            viewer.is_some(),
            viewer.is_some_and(|viewer| {
                matches!(
                    viewer.state,
                    PictureState::Ready(_) | PictureState::Protected(..)
                )
            }),
            self.attachments.inline_thumbs.has_ready(app)
                || self.attachments.gallery_preview.is_some(),
        )
    }

    /// Считает вывод защищённого изображения, пока оверлей остаётся открытым.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_protected_photo_paints(&self) -> Option<(usize, usize)> {
        self.attachments
            .viewer
            .as_ref()
            .map(|viewer| viewer.protected_paints.get())
    }

    /// Считает вывод первого защищённого вложения в бабле или в его первой цитате.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_protected_thumbnail_paints(
        &self,
        message: Uuid,
        original: Uuid,
        file: Uuid,
        app: &App,
    ) -> Option<(usize, usize)> {
        let key = AttachmentKey {
            message: original,
            file,
            position: 0,
        };
        let slot = (message != original).then_some(super::protected_quotes::QuoteSlot {
            message,
            position: 0,
        });
        self.attachments
            .protected_thumbs
            .tested_paints((key, slot), app)
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
        self.show_photo(key, title, source, PhotoTarget::History, None, cx);
    }

    /// Заново проверяет держатель точной строки, прежде чем передать загрузку в Core.
    fn open_protected_attachment(
        &mut self,
        title: SharedString,
        source: FocusHandle,
        holder: ProtectedAttachment,
        cx: &mut Context<Self>,
    ) {
        let Some(encryption) = self.encryption.as_ref().filter(|_| holder.is_mounted(self)) else {
            return;
        };
        let target = PhotoTarget::Protected(holder.clone(), encryption.activation.clone());
        self.show_photo(holder.key, title, source, target, Some(holder), cx);
    }

    /// Запускает общий просмотр с выбранной проверкой источника.
    fn show_photo(
        &mut self,
        key: AttachmentKey,
        title: SharedString,
        source: FocusHandle,
        origin: PhotoTarget,
        holder: Option<ProtectedAttachment>,
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
            holder,
            mounted: Rc::new(Cell::new(true)),
            #[cfg(all(test, feature = "gpui-test"))]
            protected_paints: Rc::default(),
        });
        self.attachments.task = Some(cx.spawn(async move |target, cx| {
            let reply = runtime
                .conversation(address, move |services, _| async move {
                    let protected = matches!(origin, PhotoTarget::Protected(..));
                    let file = match origin {
                        PhotoTarget::History => {
                            services
                                .messenger
                                .open_media(&services.sync, &access, key.message, key.file)
                                .await
                        }
                        PhotoTarget::Protected(source, activation) => {
                            source.open(&services, &access, &activation).await
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
                    tokio::task::spawn_blocking(move || decode(file, protected))
                        .await
                        .map_err(|_| PictureError::Unavailable)?
                })
                .await;
            let _ = target.update(cx, |view, cx| {
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                if !view.has_access() {
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
                let mounted = if let Some(holder) = &viewer.holder {
                    holder.is_retained(view.encryption.as_ref())
                } else {
                    from_gallery || view.viewport.has_attachment(key)
                };
                if !mounted {
                    return;
                }
                let preview = &mut view.attachments.gallery_preview;
                viewer.state = match result.unwrap_or(Err(PictureError::Unavailable)) {
                    Ok(Photo::Protected(picture)) if picture.is_current() => {
                        PictureState::Protected(picture)
                    }
                    Ok(Photo::Protected(_)) => return,
                    Ok(Photo::Plain { full, thumb }) => {
                        log::debug!("gpui_media op_id={attempt} event=viewer_ready");
                        if from_gallery {
                            if let Some((_, image)) = preview.replace((key, thumb)) {
                                cx.defer(move |app| app.drop_image(image, None));
                            }
                        } else {
                            view.attachments.inline_thumbs.fill_failed(key, thumb, cx);
                        }
                        PictureState::Ready(full)
                    }
                    Err(error) => {
                        log::debug!(
                            "gpui_media op_id={attempt} event=viewer_failed reason={error:?}"
                        );
                        if from_gallery
                            && preview.as_ref().is_some_and(|(shown, _)| *shown == key)
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
fn decode_original(file: &mut MediaFile) -> Result<DynamicImage, PictureError> {
    if file.size().map_err(|_| PictureError::Unavailable)? > MAX_ENCODED_BYTES {
        return Err(PictureError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(MAX_ENCODED_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| PictureError::Unavailable)?;
    if bytes.len() as u64 > MAX_ENCODED_BYTES {
        return Err(PictureError::TooLarge);
    }
    if crate::platform::media::heif_mime_type(&bytes).is_some() {
        let preview = crate::platform::media::heif_preview(&bytes, MAX_VIEW_SIDE)
            .ok_or(PictureError::Unsupported)?;
        return RgbaImage::from_raw(preview.width, preview.height, preview.pixels)
            .map(DynamicImage::ImageRgba8)
            .ok_or(PictureError::Unsupported);
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| PictureError::Unsupported)?;
    if !matches!(
        reader.format(),
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif)
    ) {
        return Err(PictureError::Unsupported);
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits.clone());
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| PictureError::Unsupported)?;
    limits
        .reserve(decoder.total_bytes())
        .map_err(|_| PictureError::TooLarge)?;
    decoder
        .set_limits(limits)
        .map_err(|_| PictureError::TooLarge)?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| PictureError::Unsupported)?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// Возвращает маленький кадр для плитки, если локальное фото удалось прочитать.
pub(super) fn decode_thumbnail(mut file: MediaFile) -> Option<Arc<RenderImage>> {
    let decoded = decode_original(&mut file).ok()?;
    Some(render_image(decoded.thumbnail(96, 91).into_rgba8()))
}

/// Уменьшает защищённое фото и переносит из файла только короткий отзывный допуск.
fn decode_protected_thumbnail(
    mut file: MediaFile,
    kind: MediaKind,
    tile: Option<Tile>,
) -> Option<ProtectedPicture> {
    let pixels = inline::decode_pixels(&mut file, kind).ok()?;
    let pixels = match tile {
        Some(tile) if tile.crop => inline::crop_to_tile(pixels, tile.ratio),
        _ => pixels,
    };
    let image = render_image(pixels);
    let permit = file.protected_preview_permit()?;
    Some(ProtectedPicture { image, permit })
}

/// Готовит кадры для полного просмотра и карточки, декодируя оригинал один раз.
fn decode(mut file: MediaFile, protected: bool) -> Result<Photo, PictureError> {
    let decoded = decode_original(&mut file)?;
    let thumb = (!protected).then(|| render_image(decoded.thumbnail(96, 84).into_rgba8()));
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
    let full = render_image(full);
    if let Some(thumb) = thumb {
        return Ok(Photo::Plain { full, thumb });
    }
    let permit = file
        .protected_preview_permit()
        .ok_or(PictureError::Unavailable)?;
    Ok(Photo::Protected(ProtectedPicture {
        image: full,
        permit,
    }))
}

/// Рисует пиксели только внутри авторизованной операции paint.
fn protected_photo(
    picture: ProtectedPicture,
    mounted: Rc<Cell<bool>>,
    tile: Option<Tile>,
    #[cfg(all(test, feature = "gpui-test"))] paints: PaintCounts,
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            if !mounted.get() {
                return;
            }
            let (bounds, corners) = match tile {
                Some(tile) => (bounds, tile.corners.clamp_radii_for_quad_size(bounds.size)),
                None => (
                    ObjectFit::Contain.get_bounds(bounds, picture.image.size(0)),
                    Corners::default(),
                ),
            };
            #[cfg(all(test, feature = "gpui-test"))]
            {
                let (attempts, admitted) = paints.get();
                paints.set((attempts + 1, admitted));
            }
            let _ = picture.permit.with_current(|| {
                #[cfg(all(test, feature = "gpui-test"))]
                {
                    let (attempts, admitted) = paints.get();
                    paints.set((attempts, admitted + 1));
                }
                window.paint_image(bounds, corners, picture.image.clone(), 0, false)
            });
        },
    )
    .size_full()
}

/// Передаёт GPUI неподвижный кадр с его порядком каналов.
fn render_image(mut rgba: RgbaImage) -> Arc<RenderImage> {
    for color in rgba.pixels_mut() {
        color.0.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![Frame::new(rgba)]))
}
