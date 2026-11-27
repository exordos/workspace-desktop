//! Открывает защищённые вложения и держит маленькие кадры только пока доступны исходные сообщения.

use std::{cell::RefCell, collections::VecDeque, rc::Rc, sync::Arc};

use gpui::{App, Context, Entity, IntoElement, Render, Task, Window, div, prelude::*};
use workspace_core::{
    messenger::{
        encryption::{EncryptionActivation, ProtectedMessage},
        files::{MediaFile, OpenMediaError},
        history::MediaAccess,
    },
    storage::media::MediaKind,
};

use super::super::media_album::Tile;
use super::{
    AttachmentKey, Conversation, ProtectedPicture, decode_protected_thumbnail, protected_photo,
};
use crate::gpui_client::{
    contracts::ConversationAddress,
    conversation::protected_quotes::{QuoteSlot, QuoteText},
    runtime::{RuntimeClient, WorkerServices},
    ui::{Icon, Palette},
};

/// Адрес в интерфейсе, различающий исходную карточку и разные цитаты того же файла.
pub(super) type CardAddress = (AttachmentKey, Option<QuoteSlot>);

const MAX_THUMBNAILS: usize = 48;

/// Положение карточки и действующий источник файла.
#[derive(Clone)]
pub(in crate::gpui_client::conversation) struct ProtectedAttachment {
    pub(in crate::gpui_client::conversation) key: AttachmentKey, // Точное вложение из тела оригинала.
    source: Source, // Доступ к оригиналу и, для цитаты, к цитирующему сообщению.
}

/// Способ проверки опубликованной ссылки перед получением файла.
#[derive(Clone)]
enum Source {
    Message(Arc<ProtectedMessage>), // Отзывный материал раскрытого исходного бабла.
    Quote(QuoteSlot, QuoteText),    // Место, ключ цитаты и ключ отдельно найденного оригинала.
}

impl ProtectedAttachment {
    /// Привязывает файл к тому держателю строки, который разрешил его показать.
    pub(in crate::gpui_client::conversation) fn direct(
        key: AttachmentKey,
        access: Arc<ProtectedMessage>,
    ) -> Self {
        Self {
            key,
            source: Source::Message(access),
        }
    }

    /// Привязывает карточку к исходному файлу и живому месту цитаты.
    pub(in crate::gpui_client::conversation) fn quoted(
        key: AttachmentKey,
        slot: QuoteSlot,
        access: QuoteText,
    ) -> Self {
        Self {
            key,
            source: Source::Quote(slot, access),
        }
    }

    /// Возвращает текущий родительский бабл для проверки экрана и возврата фокуса.
    pub(in crate::gpui_client::conversation) fn row_id(&self) -> uuid::Uuid {
        match &self.source {
            Source::Message(_) => self.key.message,
            Source::Quote(slot, _) => slot.message,
        }
    }

    /// Использует номер цитаты при разграничении фокуса и пикселей одного оригинала.
    pub(super) fn card_address(&self) -> CardAddress {
        let quote = match &self.source {
            Source::Quote(slot, _) => Some(*slot),
            _ => None,
        };
        (self.key, quote)
    }

    /// Не переиспользует миниатюру, если пользователь видит другое тело или цитату.
    fn same_source(&self, other: &Self) -> bool {
        match (&self.source, &other.source) {
            (Source::Message(left), Source::Message(right)) => Arc::ptr_eq(left, right),
            (Source::Quote(_, left), Source::Quote(_, right)) => left.same_location(right),
            _ => false,
        }
    }

    /// Удерживает все необходимые допуски до конца короткого рисования.
    pub(super) fn with_current<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        match &self.source {
            Source::Message(access) => access.with_current(action),
            Source::Quote(_, access) => access.with_current(action),
        }
    }

    /// Подтверждает принадлежность карточки действующему экрану непосредственно перед действием.
    pub(in crate::gpui_client::conversation) fn is_mounted(&self, view: &Conversation) -> bool {
        view.has_access() && self.is_retained(view.encryption.as_ref())
    }

    /// Держит кадр, пока сохраняются точный исходный конверт и место его показа.
    pub(in crate::gpui_client::conversation) fn is_retained(
        &self,
        encryption: Option<&super::super::encryption::HistoryEncryption>,
    ) -> bool {
        match &self.source {
            Source::Message(access) => {
                encryption.is_some_and(|encryption| encryption.holds_file(self.key, access))
            }
            Source::Quote(_, access) => access.with_current(|| ()).is_some(),
        }
    }

    /// Открывает исходный файл в Core, добавляя допуск цитаты только там, где он нужен.
    pub(in crate::gpui_client::conversation) async fn open(
        &self,
        services: &WorkerServices,
        access: &MediaAccess,
        activation: &EncryptionActivation,
    ) -> Result<MediaFile, OpenMediaError> {
        self.with_current(|| ())
            .ok_or(OpenMediaError::Unavailable)?;
        match &self.source {
            Source::Message(_) => {
                services
                    .messenger
                    .open_protected_media(
                        &services.sync,
                        access,
                        activation,
                        self.key.message,
                        self.key.file,
                    )
                    .await
            }
            Source::Quote(_, quote) => {
                services
                    .messenger
                    .open_linked_protected_quote_media(
                        &services.sync,
                        access,
                        activation,
                        quote.media_quote().ok_or(OpenMediaError::Unavailable)?,
                        self.key.file,
                    )
                    .await
            }
        }
    }
}

/// Миниатюра на месте одной видимой карточки. Изменение уведомляет этот лист GPUI.
struct Thumbnail {
    source: ProtectedAttachment,       // Проверка источника при каждом paint.
    kind: MediaKind,                   // Изображение или первый кадр видео.
    tile: Option<Tile>,                // Геометрия кадра в альбоме прямого сообщения.
    picture: Option<ProtectedPicture>, // Кадр в памяти с версией исходного файла.
    active: Rc<std::cell::Cell<bool>>, // Закрытие окна блокирует готовый элемент и будущий ответ.
    load: Option<Task<()>>,            // Снимает HTTPS-запрос сразу при отзыве карточки.
    #[cfg(all(test, feature = "gpui-test"))]
    paints: super::PaintCounts, // Считает попытки и разрешённые выводы миниатюры.
}

impl Thumbnail {
    /// Прерывает рабочий запрос и убирает подготовленные пиксели после отзыва.
    fn retire(&mut self, cx: &mut Context<Self>) {
        self.active.set(false);
        self.load.take();
        if let Some(picture) = self.picture.take() {
            cx.defer(move |app| app.drop_image(picture.image, None));
        }
    }
}

impl Render for Thumbnail {
    /// Рисует только полученный кадр, не передавая его в общий кэш истории.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let frame = div()
            .relative()
            .size_full()
            .flex()
            .items_center()
            .justify_center();
        if let Some(picture) = &self.picture {
            frame
                .child(protected_photo(
                    picture.clone(),
                    self.active.clone(),
                    self.tile,
                    #[cfg(all(test, feature = "gpui-test"))]
                    self.paints.clone(),
                ))
                .debug_selector({
                    let address = self.source.card_address();
                    move || match address.1 {
                        Some(slot) => format!(
                            "history-protected-quote-thumbnail-{}-{}-{}",
                            slot.message, slot.position, address.0.position
                        ),
                        None => format!(
                            "history-protected-thumbnail-{}-{}",
                            address.0.message, address.0.position
                        ),
                    }
                })
        } else if self.kind == MediaKind::Video {
            frame
        } else {
            frame.child(Icon::Image.element(gpui::px(24.), palette.text_secondary))
        }
    }
}

/// Адрес ограниченного запроса превью; передаётся только карточкам открытого списка.
#[derive(Clone)]
pub(super) struct PreviewLoad {
    pub(super) runtime: RuntimeClient, // Выполняет HTTPS и декодирование за пределами GPUI.
    pub(super) access: MediaAccess,    // Допуск открытой истории.
    pub(super) activation: EncryptionActivation, // Точный вход для чтения ключей.
    pub(super) address: ConversationAddress, // Полный маршрут для ответа из рабочей очереди.
}

/// Подготовленные карточки от старых к недавно показанным.
type ThumbnailQueue = VecDeque<(CardAddress, Entity<Thumbnail>)>;

/// Кадры и попытки текущих карточек с независимым временем жизни по месту в списке.
#[derive(Clone, Default)]
pub(super) struct ProtectedThumbnails(Rc<RefCell<ThumbnailQueue>>); // Общая очередь видимых карточек.

impl ProtectedThumbnails {
    /// Возвращает число попыток и разрешённых кадров миниатюры.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_paints(&self, key: CardAddress, app: &App) -> Option<(usize, usize)> {
        self.0
            .borrow()
            .iter()
            .find_map(|(address, entry)| (*address == key).then(|| entry.read(app).paints.get()))
    }

    /// Убирает результаты старой версии, снимая сетевую задачу и возможность paint в этом кадре.
    pub(super) fn retain(&self, keep: impl Fn(&ProtectedAttachment) -> bool, app: &mut App) {
        self.0.borrow_mut().retain(|(_, entry)| {
            let thumbnail = entry.read(app);
            if keep(&thumbnail.source)
                && thumbnail
                    .picture
                    .as_ref()
                    .is_none_or(ProtectedPicture::is_current)
            {
                return true;
            }
            entry.update(app, Thumbnail::retire);
            false
        });
    }

    /// Запускает GET после попадания карточки в отрисовываемую область виртуального списка.
    pub(super) fn render(
        &self,
        source: &ProtectedAttachment,
        kind: MediaKind,
        tile: Option<Tile>,
        load: &PreviewLoad,
        app: &mut App,
    ) -> impl IntoElement + use<> {
        let key = source.card_address();
        let mut entries = self.0.borrow_mut();
        if let Some(index) = entries.iter().position(|(address, _)| *address == key)
            && let Some((_, entry)) = entries.remove(index)
        {
            let previous_tile = entry.read(app).tile;
            let same_layout = match (previous_tile, tile) {
                (Some(previous), Some(current)) => {
                    previous.ratio == current.ratio && previous.crop == current.crop
                }
                (None, None) => true,
                _ => false,
            };
            if entry.read(app).source.same_source(source) && same_layout {
                if previous_tile.map(|tile| tile.corners) != tile.map(|tile| tile.corners) {
                    entry.update(app, |thumbnail, cx| {
                        thumbnail.tile = tile;
                        cx.notify();
                    });
                }
                entries.push_back((key, entry.clone()));
                return entry;
            }
            entry.update(app, Thumbnail::retire);
        }
        if entries.len() == MAX_THUMBNAILS
            && let Some((_, oldest)) = entries.pop_front()
        {
            oldest.update(app, Thumbnail::retire);
        }
        let source = source.clone();
        let load = load.clone();
        let entry = app.new(|cx: &mut Context<Thumbnail>| {
            let active = Rc::new(std::cell::Cell::new(true));
            let request = source.clone();
            let mounted = active.clone();
            let address = load.address.clone();
            let task = cx.spawn(async move |entry, cx| {
                let result = load
                    .runtime
                    .conversation(load.address, move |services, _| async move {
                        request.with_current(|| ())?;
                        let file = request
                            .open(&services, &load.access, &load.activation)
                            .await
                            .ok()?;
                        tokio::task::spawn_blocking(move || {
                            decode_protected_thumbnail(file, kind, tile)
                        })
                        .await
                        .ok()
                        .flatten()
                    })
                    .await;
                let _ = entry.update(cx, |thumbnail, cx| {
                    if mounted.get() && thumbnail.source.with_current(|| ()).is_some() {
                        thumbnail.picture = result
                            .for_current(Some(&address))
                            .and_then(Result::ok)
                            .flatten()
                            .filter(ProtectedPicture::is_current);
                        cx.notify();
                    }
                });
            });
            Thumbnail {
                source,
                kind,
                tile,
                picture: None,
                active,
                load: Some(task),
                #[cfg(all(test, feature = "gpui-test"))]
                paints: Rc::default(),
            }
        });
        entries.push_back((key, entry.clone()));
        entry
    }
}
