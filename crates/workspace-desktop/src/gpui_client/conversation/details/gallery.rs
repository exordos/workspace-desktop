//! Управляет открытым разделом вложений, областями поиска и постраничными ответами Core.

use std::collections::HashMap;

use gpui::{App, Context, FocusHandle, ScrollHandle, Task, Window, point, px};
use uuid::Uuid;
use workspace_core::{
    messenger::history::MediaAccess,
    storage::HistoryScope,
    sync::{HistoryRevocation, SyncChange},
};

use super::super::{
    Conversation,
    gallery_source::{self, GalleryCategory, GalleryCursor, GalleryItem, GalleryProblem},
};

/// Разделы в порядке показа в сведениях разговора.
pub(super) const CATEGORIES: [GalleryCategory; 4] = [
    GalleryCategory::Photos,
    GalleryCategory::Videos,
    GalleryCategory::Files,
    GalleryCategory::Links,
];

/// Действие пользователя внутри сведений или каталога вложений.
#[derive(Clone, Copy)]
pub(super) enum Action {
    Open(GalleryCategory), // Показать выбранный раздел.
    Scope(bool),           // Показать весь канал или текущую тему.
    More,                  // Получить следующую страницу.
    Retry,                 // Повторить неудачную загрузку.
    Refresh,               // Показать новый снимок после изменений истории.
    Activate(Uuid),        // Открыть конкретную строку текущего снимка.
}

/// Загрузка первой или очередной страницы в одном открытии галереи.
pub(super) enum State {
    Ready, // Запроса нет, загруженные позиции доступны.
    Loading {
        id: Uuid,                         // Принимаем только ответ этого запроса.
        _task: Task<()>,                  // Выход из раздела отменяет ожидание на стороне окна.
        invalidations: Vec<Invalidation>, // События после начала чтения страницы.
    },
    Failed {
        append: bool, // Нужно повторить первую страницу или дополнить текущую.
    },
}

/// Изменение, способное сделать запись уже отправленного запроса устаревшей.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Invalidation {
    Message(Uuid),              // Исходное сообщение отредактировали или удалили.
    File(Uuid),                 // Метаданные или оригинал файла изменились.
    Revoked(HistoryRevocation), // История стрима или темы перестала быть доступна.
}

/// Видимая позиция и её устойчивый фокус в пределах текущей выдачи.
pub(super) struct Entry {
    pub id: Uuid,           // Связывает нажатие с актуальной позицией в панели.
    pub item: GalleryItem,  // Подтверждённая запись из локальной истории.
    pub focus: FocusHandle, // Возврат после просмотра фото или выбора места сохранения.
}

/// Открытая категория, область, загруженные страницы и их запрос.
pub(super) struct Gallery {
    pub category: GalleryCategory,       // Раздел, выбранный в сведениях.
    pub whole_stream: bool,              // Истина при охвате всех известных тем канала.
    pub has_topic: bool,                 // В исходном разговоре открыта конкретная тема.
    pub entries: Vec<Entry>,             // Записи от новых к старым.
    pub state: State,                    // Состояние очередной страницы.
    pub scroll: ScrollHandle,            // Сохраняет положение при добавлении страниц.
    pub needs_refresh: bool,             // Известная история изменилась после показа страницы.
    next: Option<GalleryCursor>,         // Точная следующая позиция, выданная Core.
    thumbnails: HashMap<Uuid, Task<()>>, // Пакеты миниатюр с диска в текущем разделе.
}

impl Gallery {
    /// Готовит раздел текущей темы либо единственной области разговора.
    fn new(category: GalleryCategory, has_topic: bool) -> Self {
        Self {
            category,
            whole_stream: false,
            has_topic,
            entries: Vec::new(),
            state: State::Ready,
            scroll: ScrollHandle::new(),
            needs_refresh: false,
            next: None,
            thumbnails: HashMap::new(),
        }
    }

    /// Возвращает фокус пункта, с которого открыли этот раздел.
    pub(super) fn trigger<'a>(&self, handles: &'a [FocusHandle; 4]) -> &'a FocusHandle {
        &handles[match self.category {
            GalleryCategory::Photos => 0,
            GalleryCategory::Videos => 1,
            GalleryCategory::Files => 2,
            GalleryCategory::Links => 3,
        }]
    }

    /// Проверяет, осталась ли известная следующая страница в текущем снимке.
    pub(super) fn has_next(&self) -> bool {
        self.next.is_some()
    }

    /// Убирает локальные миниатюры при смене области, закрытии или уничтожении экрана.
    pub(super) fn release_previews(&mut self, app: &mut App) {
        self.thumbnails.clear();
        for mut entry in self.entries.drain(..) {
            entry.release_preview(app);
        }
    }
}

impl Entry {
    /// Освобождает текстуру миниатюры после снятия строки с экрана.
    fn release_preview(&mut self, app: &mut App) {
        if let GalleryItem::Media { preview, .. } = &mut self.item
            && let Some(image) = preview.take()
        {
            app.defer(move |app| app.drop_image(image, None));
        }
    }
}

impl Invalidation {
    /// Проверяет, относится ли изменение к источнику или файлу позиции.
    fn affects(self, item: &GalleryItem) -> bool {
        let (scope, message, file) = match item {
            GalleryItem::Media { item, .. } => (item.scope, item.message_id, Some(item.file_id)),
            GalleryItem::Link(link) => (link.scope, link.message_id, None),
        };
        match self {
            Self::Message(id) => message == id,
            Self::File(id) => file == Some(id),
            Self::Revoked(HistoryRevocation::Stream(id)) => scope.stream_id == id,
            Self::Revoked(HistoryRevocation::Topic {
                stream_id,
                topic_id,
            }) => scope.stream_id == stream_id && scope.topic_id == Some(topic_id),
        }
    }
}

impl Conversation {
    /// Перечитывает счётчики только для открытой панели и текущего допуска.
    pub(in crate::gpui_client::conversation) fn refresh_gallery_counts(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let Some(access) = self.gallery_access() else {
            return;
        };
        let Some(panel) = self.details.as_mut().filter(|panel| panel.is_present(cx)) else {
            return;
        };
        let id = Uuid::new_v4();
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        log::debug!("gpui_gallery op_id={id} event=counts_requested");
        let task = cx.spawn(async move |view, cx| {
            let reply = gallery_source::load_counts(runtime, address, access).await;
            let _ = view.update(cx, |view, cx| {
                let Some(counts) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                let Some(panel) = view.details.as_mut().filter(|panel| panel.is_present(cx)) else {
                    return;
                };
                if !panel
                    .gallery_counts_request
                    .as_ref()
                    .is_some_and(|(current, _)| *current == id)
                {
                    return;
                }
                log::debug!(
                    "gpui_gallery op_id={id} event=counts_completed success={}",
                    counts.is_some()
                );
                panel.gallery_counts_request = None;
                if panel.gallery_counts != counts {
                    panel.gallery_counts = counts;
                    cx.notify();
                }
            });
        });
        panel.gallery_counts_request = Some((id, task));
    }

    /// Обрабатывает действие только для живого раздела и текущего доступа.
    pub(super) fn handle_gallery_action(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.gallery_access().is_none() {
            return;
        }
        if let Action::Open(category) = action {
            if let Some(panel) = &mut self.details
                && panel.snapshot.is_some()
                && panel.picker.is_none()
                && panel.gallery.is_none()
                && panel.encrypted_screen().is_none()
            {
                let has_topic = self.address.route.scope.topic_id.is_some();
                panel.refocus_details = None;
                panel.gallery = Some(Gallery::new(category, has_topic));
                panel.host.read(cx).focus_close(&panel.key, window);
                panel.refresh_frame(cx);
                self.request_gallery(false, cx);
            }
            return;
        }

        let Some(gallery) = self
            .details
            .as_mut()
            .and_then(|panel| panel.gallery.as_mut())
        else {
            return;
        };
        match action {
            Action::Scope(whole_stream)
                if gallery.has_topic && gallery.whole_stream != whole_stream =>
            {
                gallery.whole_stream = whole_stream;
                self.cancel_gallery_export();
                self.request_gallery(false, cx);
            }
            Action::More if matches!(gallery.state, State::Ready) && gallery.has_next() => {
                self.request_gallery(true, cx);
            }
            Action::Refresh if gallery.needs_refresh => self.request_gallery(false, cx),
            Action::Retry => {
                if let State::Failed { append } = gallery.state {
                    self.request_gallery(append, cx);
                }
            }
            Action::Activate(id) => {
                let Some(entry) = gallery.entries.iter().find(|entry| entry.id == id) else {
                    return;
                };
                window.focus(&entry.focus);
                let focus = entry.focus.clone();
                let whole_stream = gallery.whole_stream;
                let photos = gallery.category == GalleryCategory::Photos;
                match &entry.item {
                    GalleryItem::Media { item, .. } => {
                        let item = item.clone();
                        if photos {
                            self.open_gallery_photo(item, whole_stream, focus, cx);
                        } else {
                            self.activate_gallery_file(item, whole_stream, focus, window, cx);
                        }
                    }
                    GalleryItem::Link(link) => cx.open_url(&link.url),
                }
            }
            _ => {}
        }
    }

    /// Показывает обновление истории, сохраняя загруженные страницы и положение прокрутки.
    pub(in crate::gpui_client) fn mark_open_gallery_needs_refresh(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if let Some(gallery) = self
            .details
            .as_mut()
            .and_then(|panel| panel.gallery.as_mut())
            && !gallery.needs_refresh
        {
            gallery.needs_refresh = true;
            cx.notify();
        }
    }

    /// Сразу убирает изменённые или отозванные записи и сохраняет остальные страницы.
    pub(in crate::gpui_client) fn refresh_open_gallery(
        &mut self,
        change: &SyncChange,
        cx: &mut Context<Self>,
    ) {
        let current = self.address.route.scope;
        let includes = |scope: HistoryScope, whole_stream: bool| {
            scope.stream_id == current.stream_id
                && (whole_stream
                    || current.topic_id.is_none()
                    || scope.topic_id == current.topic_id)
        };
        let counts_changed = match change {
            SyncChange::HistoryMessageCreated { scope, .. } => includes(*scope, false),
            SyncChange::HistoryMessageChanged { .. } | SyncChange::MediaFileInvalidated { .. } => {
                true
            }
            SyncChange::HistoryAccessRevoked(HistoryRevocation::Topic {
                stream_id,
                topic_id,
            }) => includes(
                HistoryScope {
                    stream_id: *stream_id,
                    topic_id: Some(*topic_id),
                },
                false,
            ),
            _ => false,
        };
        if counts_changed {
            self.refresh_gallery_counts(cx);
        }
        let Some(gallery) = self
            .details
            .as_mut()
            .and_then(|panel| panel.gallery.as_mut())
        else {
            return;
        };
        let includes = |scope| includes(scope, gallery.whole_stream);
        let invalidation = match change {
            SyncChange::HistoryMessageCreated { scope, .. } if includes(*scope) => {
                self.mark_open_gallery_needs_refresh(cx);
                return;
            }
            SyncChange::HistoryMessageChanged { message_id } => Invalidation::Message(*message_id),
            SyncChange::MediaFileInvalidated { file_id }
                if gallery.category != GalleryCategory::Links =>
            {
                Invalidation::File(*file_id)
            }
            SyncChange::HistoryAccessRevoked(revocation @ HistoryRevocation::Stream(id))
                if current.stream_id == *id =>
            {
                Invalidation::Revoked(*revocation)
            }
            SyncChange::HistoryAccessRevoked(
                revocation @ HistoryRevocation::Topic {
                    stream_id,
                    topic_id,
                },
            ) if includes(HistoryScope {
                stream_id: *stream_id,
                topic_id: Some(*topic_id),
            }) =>
            {
                Invalidation::Revoked(*revocation)
            }
            _ => return,
        };
        let before = gallery.entries.len();
        gallery.entries.retain_mut(|entry| {
            if invalidation.affects(&entry.item) {
                entry.release_preview(cx);
                false
            } else {
                true
            }
        });
        if let State::Loading { invalidations, .. } = &mut gallery.state
            && !invalidations.contains(&invalidation)
        {
            invalidations.push(invalidation);
        }
        let revoked = matches!(invalidation, Invalidation::Revoked(_));
        if revoked {
            gallery.next = None;
        }
        if gallery.entries.len() != before || revoked {
            gallery.needs_refresh = true;
            cx.notify();
        }
    }

    /// Читает страницу через штатный рабочий поток с допуском открытой истории.
    fn request_gallery(&mut self, append: bool, cx: &mut Context<Self>) {
        let access = self.gallery_access();
        let Some(panel) = &mut self.details else {
            return;
        };
        let Some(gallery) = &mut panel.gallery else {
            return;
        };
        if !append {
            gallery.release_previews(cx);
            gallery.next = None;
            gallery.needs_refresh = false;
            gallery.scroll.set_offset(point(px(0.), px(0.)));
        }
        let Some(access) = access else {
            gallery.state = State::Failed { append };
            cx.notify();
            return;
        };
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        let category = gallery.category;
        let whole_stream = gallery.whole_stream;
        let cursor = gallery.next.clone();
        let id = Uuid::new_v4();
        log::debug!("gpui_gallery op_id={id} event=page_started append={append}");
        let task = cx.spawn(async move |view, cx| {
            let reply = gallery_source::load(
                runtime,
                address,
                access.clone(),
                whole_stream,
                category,
                cursor,
            )
            .await;
            let _ = view.update(cx, |view, cx| {
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                let Some(panel) = view.details.as_mut().filter(|panel| panel.is_present(cx)) else {
                    return;
                };
                let Some(gallery) = panel.gallery.as_mut() else {
                    return;
                };
                let State::Loading {
                    id: current,
                    invalidations,
                    ..
                } = &gallery.state
                else {
                    return;
                };
                if *current != id {
                    return;
                }
                let revoked = invalidations
                    .iter()
                    .any(|change| matches!(change, Invalidation::Revoked(_)));
                log::debug!(
                    "gpui_gallery op_id={id} event=page_completed success={}",
                    result.is_ok()
                );
                match result {
                    Ok(page) => {
                        let first = gallery.entries.len();
                        for item in page.items {
                            // Ответ, прочитанный до события, может ещё содержать устаревшую позицию.
                            if invalidations.iter().any(|change| change.affects(&item)) {
                                gallery.needs_refresh = true;
                                continue;
                            }
                            gallery.entries.push(Entry {
                                id: Uuid::new_v4(),
                                item,
                                focus: cx.focus_handle(),
                            });
                        }
                        gallery.next = if revoked { None } else { page.next };
                        gallery.state = State::Ready;
                        if category == GalleryCategory::Photos {
                            view.request_gallery_thumbnails(first, access, cx);
                        }
                    }
                    Err(GalleryProblem::StaleCursor) => {
                        gallery.next = None;
                        gallery.needs_refresh = true;
                        gallery.state = State::Ready;
                    }
                    Err(GalleryProblem::Unavailable) if revoked => gallery.state = State::Ready,
                    Err(GalleryProblem::Unavailable) => gallery.state = State::Failed { append },
                }
                cx.notify();
            });
        });
        gallery.state = State::Loading {
            id,
            _task: task,
            invalidations: Vec::new(),
        };
        cx.notify();
    }

    /// После показа списка загружает миниатюры только из готовых оригиналов небольшими пакетами.
    fn request_gallery_thumbnails(
        &mut self,
        first: usize,
        access: MediaAccess,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.details else {
            return;
        };
        let Some(gallery) = &mut panel.gallery else {
            return;
        };
        let cached: Vec<_> = gallery.entries[first..]
            .iter()
            .filter_map(|entry| match &entry.item {
                GalleryItem::Media { item, .. } if item.cached => Some((entry.id, item.clone())),
                _ => None,
            })
            .collect();
        let whole_stream = gallery.whole_stream;
        for batch in cached.chunks(gallery_source::MAX_THUMBNAILS) {
            let entries = batch.to_vec();
            let runtime = panel.runtime.clone();
            let address = self.address.clone();
            let access = access.clone();
            let id = Uuid::new_v4();
            let task = cx.spawn(async move |view, cx| {
                let reply = gallery_source::load_thumbnails(
                    runtime,
                    address,
                    access,
                    whole_stream,
                    entries,
                )
                .await;
                let _ = view.update(cx, |view, cx| {
                    let Some(images) = reply.for_current(Some(&view.address)) else {
                        return;
                    };
                    let Some(gallery) = view
                        .details
                        .as_mut()
                        .and_then(|panel| panel.gallery.as_mut())
                    else {
                        return;
                    };
                    if gallery.thumbnails.remove(&id).is_none() {
                        return;
                    }
                    for (id, image) in images {
                        if let Some(entry) = gallery.entries.iter_mut().find(|entry| entry.id == id)
                            && let GalleryItem::Media { preview, .. } = &mut entry.item
                        {
                            *preview = Some(image);
                        }
                    }
                    cx.notify();
                });
            });
            gallery.thumbnails.insert(id, task);
        }
    }
}

#[cfg(all(test, feature = "gpui-test"))]
impl Conversation {
    /// Возвращает числа, которые панель передала строкам вложений.
    pub(in crate::gpui_client) fn tested_gallery_counts(
        &self,
    ) -> Option<workspace_core::storage::media::GalleryCounts> {
        self.details.as_ref()?.gallery_counts
    }

    /// Даёт фактическое состояние галереи проверке рабочего окна.
    pub(in crate::gpui_client) fn tested_gallery(&self) -> Option<(bool, usize, bool)> {
        let gallery = self.details.as_ref()?.gallery.as_ref()?;
        Some((
            gallery.whole_stream,
            gallery.entries.len(),
            gallery.has_next(),
        ))
    }
}
