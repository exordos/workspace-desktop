//! Показывает отмеченные сообщения и упоминания, ведёт страницы, фильтр и счётчик.

use std::{collections::HashSet, sync::Arc, time::Duration};

use gpui::{
    Context, Div, ElementId, EventEmitter, IntoElement, Render, ScrollHandle, Window, div,
    prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        activity::{
            ActivityMessage, ActivityMessageError, ActivityMessageFilter, ActivityMessagePage,
            ActivityMessagePreview,
        },
        catalog::Chat,
    },
    sync::{HistoryRevocation, SyncChange, SyncUpdate},
};

use crate::gpui_client::{
    contracts::{AccountSession, ActivityAddress, ActivityPage},
    preferences::{Language, Preferences},
    runtime::RuntimeClient,
    ui::{
        Button, ButtonAppearance, Icon, PANEL_RADIUS, Palette, TextRole, activate,
        calendar_date_label, label,
    },
};

const REVALIDATE_DELAY: Duration = Duration::from_millis(350);

/// Действие пользователя над карточкой текущего списка.
pub(in crate::gpui_client) enum ActivityMessageEvent {
    Open {
        address: ActivityAddress, // Показ, в котором нажали на карточку.
        message: ActivityMessage, // Открыть историю вокруг указанного сообщения.
    },
}

/// Что нужно перечитать и какие строки можно сохранить на время запроса.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Load {
    Replace(usize), // Число уже просмотренных страниц, которые надо сверить с начала.
    More(Uuid),     // Маркер ещё не загруженной страницы.
}

/// Подтверждённые страницы одного фильтра и запрет повторить тот же маркер.
#[derive(Default)]
struct Collection {
    messages: Vec<ActivityMessage>, // Карточки, упорядоченные от новых к старым.
    next: Option<Uuid>,             // Следующий маркер. None означает конец.
    cursors: HashSet<Uuid>,         // Маркеры, уже встреченные в этой последовательности.
    pages: usize,                   // Число принятых страниц вместе с пустыми.
}

impl Collection {
    /// Добавляет страницу, сохраняя по одной строке на публичный UUID.
    fn append(&mut self, page: ActivityMessagePage) -> Result<(), ActivityMessageError> {
        if page.next.is_some_and(|next| !self.cursors.insert(next)) {
            return Err(ActivityMessageError::Unavailable);
        }
        self.next = page.next;
        self.pages += 1;
        self.messages.extend(page.messages);
        self.messages
            .sort_by(|left, right| (&right.created_at, right.id).cmp(&(&left.created_at, left.id)));
        let mut ids = HashSet::new();
        self.messages.retain(|message| ids.insert(message.id));
        Ok(())
    }
}

/// Два списка используют один экран. Счётчик работает и на других страницах аккаунта.
pub(in crate::gpui_client) struct ActivityMessages {
    runtime: RuntimeClient,           // Фоновый вход в Core.
    address: Option<ActivityAddress>, // Текущий показ с исходным поколением и номером перехода.
    session: Option<AccountSession>,  // Выбранный вход для фонового счётчика.
    session_revision: u64,            // Отличает повторный выбор того же входа.
    chats: Option<Arc<[Chat]>>,       // Текущий каталог для названий разговоров и тем.
    unread_only: bool,                // Активный фильтр упоминаний.
    collection: Collection,           // Принятые строки запроса на текущем экране.
    request: u64,                     // Отличает старые запросы после смены фильтра или доступа.
    loading: Option<Load>,            // Выполняющаяся загрузка, если она есть.
    failed: Option<Load>,             // Действие, которое можно повторить после ошибки.
    scroll: ScrollHandle,             // Прокрутка текущего фильтра.
    pagination_armed: bool,           // Разрешено начать догрузку при приближении к низу.
    unread_count: Option<u64>,        // Последний известный счётчик упоминаний.
    count_loading: bool,              // Счётчик уже перечитывается для выбранного входа.
    count_retries: u8,                // Повторы первоначальной загрузки после отказа.
    page_dirty: bool,                 // После запроса нужно учесть новое событие.
    count_dirty: bool,                // События могли изменить непрочитанные упоминания.
    revalidation_scheduled: bool,     // Таймер объединяет соседние события журнала.
}

impl EventEmitter<ActivityMessageEvent> for ActivityMessages {}

impl ActivityMessages {
    /// Готовит общий экран двух коллекций и фонового счётчика.
    pub(in crate::gpui_client) fn new(runtime: RuntimeClient) -> Self {
        Self {
            runtime,
            address: None,
            session: None,
            session_revision: 0,
            chats: None,
            unread_only: false,
            collection: Collection::default(),
            request: 0,
            loading: None,
            failed: None,
            scroll: ScrollHandle::new(),
            pagination_armed: true,
            unread_count: None,
            count_loading: false,
            count_retries: 0,
            page_dirty: false,
            count_dirty: false,
            revalidation_scheduled: false,
        }
    }

    /// Выбирает владельца счётчика и отбрасывает данные предыдущего показа.
    pub(in crate::gpui_client) fn set_session(
        &mut self,
        session: Option<AccountSession>,
        cx: &mut Context<Self>,
    ) {
        if self.session == session {
            return;
        }
        self.session = session;
        self.session_revision = self.session_revision.wrapping_add(1);
        self.unread_count = None;
        self.count_loading = false;
        self.count_dirty = false;
        self.count_retries = 0;
        self.load_count(cx);
        cx.notify();
    }

    /// Возвращает подтверждённое количество для текущего владельца.
    pub(in crate::gpui_client) fn unread_mentions(&self) -> Option<u64> {
        self.unread_count
    }

    /// Передаёт актуальные имена разговоров и тем из подтверждённого каталога.
    pub(in crate::gpui_client) fn set_chats(
        &mut self,
        chats: Option<Arc<[Chat]>>,
        cx: &mut Context<Self>,
    ) {
        let same = match (&self.chats, &chats) {
            (None, None) => true,
            (Some(current), Some(next)) => Arc::ptr_eq(current, next),
            _ => false,
        };
        if !same {
            self.chats = chats;
            cx.notify();
        }
    }

    /// Открывает только поддерживаемые коллекции, начиная с первого экрана.
    pub(in crate::gpui_client) fn set_address(
        &mut self,
        address: Option<ActivityAddress>,
        cx: &mut Context<Self>,
    ) {
        let address = address.filter(|address| {
            matches!(address.page, ActivityPage::Marked | ActivityPage::Mentions)
        });
        if self.address == address {
            return;
        }
        self.address = address;
        self.unread_only = false;
        self.reset_collection(cx);
    }

    /// Обрабатывает события только этого поколения. Отзыв доступа скрывает строки сразу.
    pub(in crate::gpui_client) fn sync_update(
        &mut self,
        update: &SyncUpdate,
        cx: &mut Context<Self>,
    ) {
        if !self.session.as_ref().is_some_and(|session| {
            session.owner == update.context && session.generation == update.generation
        }) {
            return;
        }
        match update.change {
            SyncChange::HistoryAccessRevoked(revocation) => {
                self.collection.messages.retain(|message| match revocation {
                    HistoryRevocation::Stream(id) => message.stream_id != id,
                    HistoryRevocation::Topic {
                        stream_id,
                        topic_id,
                    } => message.stream_id != stream_id || message.topic_id != topic_id,
                });
                self.invalidate_loading();
                self.invalidate_count();
                cx.notify();
            }
            SyncChange::MessengerReset => {
                self.collection = Collection::default();
                self.invalidate_loading();
                self.invalidate_count();
                cx.notify();
            }
            SyncChange::HistoryMessageCreated { .. }
            | SyncChange::HistoryMessageChanged { .. }
            | SyncChange::ActivityChanged
            | SyncChange::Unread(_) => {}
            _ => return,
        }
        self.sync_lagged(cx);
    }

    /// Сверяет открытый список и счётчик после событий или пропуска уведомления.
    pub(in crate::gpui_client) fn sync_lagged(&mut self, cx: &mut Context<Self>) {
        self.page_dirty |= self.address.is_some();
        self.count_dirty |= self.session.is_some();
        self.schedule_revalidation(cx);
    }

    /// Прерывает приём ответа, начатого до отзыва доступа.
    fn invalidate_loading(&mut self) {
        self.request = self.request.wrapping_add(1);
        self.loading = None;
        self.failed = None;
    }

    /// Прерывает приём счётчика, начатый до очистки или отзыва доступа.
    fn invalidate_count(&mut self) {
        self.session_revision = self.session_revision.wrapping_add(1);
        self.unread_count = None;
        self.count_loading = false;
        self.count_retries = 0;
    }

    /// Применяет фильтр, очищает прокрутку и запрашивает первую страницу.
    fn select_mentions_filter(&mut self, unread_only: bool, cx: &mut Context<Self>) {
        if self.unread_only != unread_only {
            self.unread_only = unread_only;
            self.reset_collection(cx);
        }
    }

    /// Снимает состояние предыдущего фильтра перед загрузкой новой коллекции.
    fn reset_collection(&mut self, cx: &mut Context<Self>) {
        self.invalidate_loading();
        self.collection = Collection::default();
        self.scroll = ScrollHandle::new();
        self.pagination_armed = true;
        self.page_dirty = false;
        self.load(Load::Replace(1), cx);
        cx.notify();
    }

    /// Запускает фоновое чтение и принимает ответ только для исходного запроса.
    fn load(&mut self, load: Load, cx: &mut Context<Self>) {
        let Some(address) = self.address.clone() else {
            return;
        };
        if self.loading.is_some() {
            return;
        }
        let filter = match address.page {
            ActivityPage::Marked => ActivityMessageFilter::Marked,
            ActivityPage::Mentions if self.unread_only => ActivityMessageFilter::UnreadMentions,
            ActivityPage::Mentions => ActivityMessageFilter::Mentions,
            _ => return,
        };
        self.loading = Some(load);
        self.failed = None;
        self.request = self.request.wrapping_add(1);
        let request = self.request;
        let runtime = self.runtime.clone();
        cx.spawn(async move |entity, cx| {
            let result = fetch(&runtime, &address.session, filter, load).await;
            let _ = entity.update(cx, |view, cx| {
                if view.address.as_ref() != Some(&address) || view.request != request {
                    return;
                }
                view.loading = None;
                let result = result.and_then(|fetched| match fetched {
                    Loaded::Window(collection) => {
                        view.collection = collection;
                        Ok(())
                    }
                    Loaded::Page(page) => view.collection.append(page),
                });
                if result == Err(ActivityMessageError::ExpiredMarker)
                    && matches!(load, Load::More(_))
                {
                    view.load(Load::Replace(view.collection.pages + 1), cx);
                    return;
                }
                if result == Err(ActivityMessageError::AccessLost) {
                    view.collection = Collection::default();
                    view.failed = Some(Load::Replace(1));
                    cx.notify();
                    return;
                }
                view.failed = result.is_err().then_some(load);
                view.schedule_revalidation(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Добавляет следующий запрос при приближении к концу прокручиваемого списка.
    fn near_bottom(&mut self, cx: &mut Context<Self>) {
        let remaining = self.scroll.max_offset().height + self.scroll.offset().y;
        if remaining > px(96.) {
            self.pagination_armed = true;
        } else if remaining <= px(64.)
            && self.pagination_armed
            && self.failed.is_none()
            && self.loading.is_none()
            && let Some(next) = self.collection.next
        {
            self.pagination_armed = false;
            self.load(Load::More(next), cx);
        }
    }

    /// Считывает счётчик отдельно от открытых карточек.
    fn load_count(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.count_loading = true;
        let revision = self.session_revision;
        let runtime = self.runtime.clone();
        cx.spawn(async move |entity, cx| {
            let result = runtime.unread_activity_mentions(session).await;
            let _ = entity.update(cx, |view, cx| {
                if revision != view.session_revision {
                    return;
                }
                view.count_loading = false;
                match result {
                    Ok(count) => {
                        view.count_retries = 0;
                        if view.unread_count != Some(count) {
                            view.unread_count = Some(count);
                            cx.notify();
                        }
                    }
                    Err(ActivityMessageError::AccessLost) => {
                        if view.unread_count.take().is_some() {
                            cx.notify();
                        }
                    }
                    Err(ActivityMessageError::Unavailable)
                        if view.unread_count.is_none() && view.count_retries < 2 =>
                    {
                        view.count_retries += 1;
                        view.count_dirty = true;
                    }
                    Err(_) => {}
                }
                view.schedule_revalidation(cx);
            });
        })
        .detach();
    }

    /// Объединяет уведомления во время чтения и после всплеска журнала.
    fn schedule_revalidation(&mut self, cx: &mut Context<Self>) {
        if self.revalidation_scheduled
            || !(self.page_dirty && self.loading.is_none()
                || self.count_dirty && !self.count_loading)
        {
            return;
        }
        self.revalidation_scheduled = true;
        let delay = cx.background_executor().timer(REVALIDATE_DELAY);
        cx.spawn(async move |entity, cx| {
            delay.await;
            let _ = entity.update(cx, |view, cx| {
                view.revalidation_scheduled = false;
                if view.page_dirty && view.loading.is_none() {
                    view.page_dirty = false;
                    view.load(Load::Replace(view.collection.pages.max(1)), cx);
                }
                if view.count_dirty && !view.count_loading {
                    view.count_dirty = false;
                    view.load_count(cx);
                }
            });
        })
        .detach();
    }

    /// Показывает два фильтра упоминаний с видимым активным выбором.
    fn filters(&self, palette: Palette, language: Language, cx: &Context<Self>) -> Div {
        let entity = cx.weak_entity();
        let address = self.address.clone();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(palette.border)
            .children(
                [
                    (
                        "activity-mentions-all",
                        language.tr("activity.allMentions"),
                        false,
                    ),
                    (
                        "activity-mentions-unread",
                        language.tr("activity.unreadMentions"),
                        true,
                    ),
                ]
                .into_iter()
                .map(|(id, title, unread)| {
                    let mut button = Button::new(id, title);
                    let mut colors = palette;
                    if unread == self.unread_only {
                        colors.card = palette.card_selected;
                    } else {
                        button.appearance = ButtonAppearance::Ghost;
                    }
                    let entity = entity.clone();
                    let address = address.clone();
                    button.render(colors, None, move |_, app| {
                        let _ = entity.update(app, |view, cx| {
                            if view.address == address {
                                view.select_mentions_filter(unread, cx);
                            }
                        });
                    })
                }),
            )
    }

    /// Выбирает сообщение по UUID, пока оно присутствует в текущем показе.
    fn open_message(
        &self,
        address: ActivityAddress,
        unread_only: bool,
        id: Uuid,
        cx: &mut Context<Self>,
    ) {
        if self.address.as_ref() == Some(&address)
            && self.unread_only == unread_only
            && let Some(message) = self.collection.messages.iter().find(|row| row.id == id)
        {
            cx.emit(ActivityMessageEvent::Open {
                address,
                message: message.clone(),
            });
        }
    }

    /// Рисует карточку сообщения с автором, источником, временем и коротким текстом.
    fn card(
        &self,
        message: &ActivityMessage,
        mentions: bool,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let chat = self
            .chats
            .as_deref()
            .and_then(|chats| chats.iter().find(|chat| chat.id == message.stream_id));
        let user = self
            .address
            .as_ref()
            .and_then(|address| address.session.user_uuid());
        let location = chat.map(|chat| {
            let name = language.chat_title(chat, user);
            if chat.direct {
                language.tr_with("desktop.activity.directSource", &[("name", name)])
            } else if let Some(topic) = chat
                .topics
                .iter()
                .find(|topic| topic.id == message.topic_id)
                .filter(|topic| !topic.name.trim().is_empty())
            {
                format!("{name} · {}", topic.name)
            } else {
                name.to_owned()
            }
        });
        let author = message.author_name.clone().unwrap_or_else(|| {
            if user == Some(message.author_id) {
                language.tr("common.you").into()
            } else {
                chat.filter(|chat| chat.metadata.direct_peer_id == Some(message.author_id))
                    .map(|chat| chat.name.clone())
                    .unwrap_or_else(|| language.tr("desktop.messenger.participant").into())
            }
        });
        let preview = match &message.preview {
            ActivityMessagePreview::Text(text) => language.preview_text(text),
            ActivityMessagePreview::Encrypted => language
                .tr("desktop.messenger.history.encryptedMessage")
                .into(),
            ActivityMessagePreview::Unsupported => language
                .tr("desktop.messenger.composer.draftMessage")
                .into(),
        };
        let time = crate::platform::message_time::LocalTime::system()
            .history_stamp(&message.created_at)
            .map(|local| {
                language.tr_with(
                    "desktop.activity.dateTime",
                    &[
                        ("date", &calendar_date_label(local.date, language)),
                        ("time", &local.short_time()),
                    ],
                )
            })
            .unwrap_or_default();
        let unread = mentions && !message.read;
        let id = message.id;
        let row = div()
            .id(ElementId::Uuid(id))
            .debug_selector(move || format!("activity-message-{id}"))
            .flex()
            .flex_none()
            .items_start()
            .gap_2()
            .min_w_0()
            .p_3()
            .rounded(PANEL_RADIUS)
            .border_1()
            .border_color(gpui::transparent_black())
            .when(unread, |row| row.bg(palette.card))
            .hover(|style| style.bg(palette.card_hover))
            .when(mentions, |row| {
                row.child(
                    div()
                        .flex_none()
                        .size(px(8.))
                        .mt_1()
                        .rounded_full()
                        .when(unread, |dot| dot.bg(palette.unread)),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap_2()
                            .min_w_0()
                            .child(label(palette, TextRole::Caption, time).flex_none())
                            .children(location.map(|location| {
                                label(palette, TextRole::Caption, location)
                                    .min_w_0()
                                    .truncate()
                            })),
                    )
                    .child(label(palette, TextRole::Label, author).truncate())
                    .child(label(palette, TextRole::Body, preview).line_clamp(2)),
            )
            .child(Icon::NewWindow.element(px(16.), palette.text_muted));
        let entity = cx.weak_entity();
        let address = self.address.clone();
        let unread_only = self.unread_only;
        activate(row, None, palette, move |_, app| {
            if let Some(address) = address.clone() {
                let _ = entity.update(app, |view, cx| {
                    view.open_message(address, unread_only, id, cx)
                });
            }
        })
    }

    /// Показывает индикатор, пустое состояние, повтор или ручную догрузку.
    fn footer(&self, palette: Palette, language: Language, cx: &Context<Self>) -> Option<Div> {
        let empty = self.collection.messages.is_empty();
        let text = if let Some(load) = self.failed {
            Some(if matches!(load, Load::More(_)) {
                language.tr("activity.loadMoreError")
            } else if empty {
                language.tr("activity.messagesLoadError")
            } else {
                language.tr("activity.messagesRefreshError")
            })
        } else if self.loading.is_some() {
            Some(language.tr("app.loading"))
        } else if empty && self.collection.next.is_none() {
            Some(match self.address.as_ref().map(|address| address.page) {
                Some(ActivityPage::Mentions) if self.unread_only => {
                    language.tr("activity.unreadMentionsEmpty")
                }
                Some(ActivityPage::Mentions) => language.tr("activity.mentionsEmpty"),
                _ => language.tr("desktop.activity.marked.empty"),
            })
        } else {
            None
        };
        let action = self.failed.or_else(|| {
            self.collection
                .next
                .filter(|_| self.loading.is_none())
                .map(Load::More)
        });
        if text.is_none() && action.is_none() {
            return None;
        }
        let button = action.map(|action| {
            let entity = cx.weak_entity();
            let address = self.address.clone();
            let unread_only = self.unread_only;
            let title = if self.failed.is_some() {
                language.tr("common.retry")
            } else {
                language.tr("common.loadMore")
            };
            Button::new("activity-messages-action", title).render(palette, None, move |_, app| {
                let _ = entity.update(app, |view, cx| {
                    if view.address == address
                        && view.unread_only == unread_only
                        && (view.failed == Some(action)
                            || matches!(action, Load::More(marker)
                                    if view.collection.next == Some(marker)))
                    {
                        view.load(action, cx);
                    }
                });
            })
        });
        Some(
            text.map_or_else(
                || div().flex_none(),
                |text| super::status(palette, text, self.failed.is_some()),
            )
            .children(button),
        )
    }
}

impl Render for ActivityMessages {
    /// Встраивает общий список в рамку открытой страницы.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = self.address.as_ref().map_or_else(
            || Preferences::read(cx).language,
            |address| Preferences::read_for(cx, &address.session.owner).language,
        );
        let mentions = self
            .address
            .as_ref()
            .is_some_and(|address| address.page == ActivityPage::Mentions);
        let entity = cx.weak_entity();
        let address = self.address.clone();
        let unread_only = self.unread_only;
        let content = div()
            .id("activity-messages-list")
            .debug_selector(|| "activity-messages-list".into())
            .flex()
            .flex_1()
            .flex_col()
            .min_h_0()
            .min_w_0()
            .gap_1()
            .p_2()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .on_scroll_wheel(move |_, window, _| {
                let entity = entity.clone();
                let address = address.clone();
                window.on_next_frame(move |_, app| {
                    let _ = entity.update(app, |view, cx| {
                        if view.address == address && view.unread_only == unread_only {
                            view.near_bottom(cx);
                        }
                    });
                });
            })
            .children(
                self.collection
                    .messages
                    .iter()
                    .map(|message| self.card(message, mentions, palette, language, cx)),
            )
            .children(self.footer(palette, language, cx));
        div()
            .flex()
            .flex_1()
            .flex_col()
            .min_h_0()
            .min_w_0()
            .when(mentions, |page| {
                page.child(self.filters(palette, language, cx))
            })
            .child(content)
    }
}

/// Возвращает одну дочитанную страницу или заново подтверждённое окно списка.
enum Loaded {
    Page(ActivityMessagePage), // Дописывается к списку с тем же серверным маркером.
    Window(Collection),        // Заменяет прежние данные после успешной сверки.
}

/// При сверке поддерживает прежнюю глубину просмотра, запрашивая от головы коллекции.
async fn fetch(
    runtime: &RuntimeClient,
    session: &AccountSession,
    filter: ActivityMessageFilter,
    load: Load,
) -> Result<Loaded, ActivityMessageError> {
    let pages = match load {
        Load::More(marker) => {
            return runtime
                .activity_messages(session.clone(), filter, Some(marker))
                .await
                .map(Loaded::Page);
        }
        Load::Replace(pages) => pages,
    };
    let mut collection = Collection::default();
    for _ in 0..pages {
        let page = runtime
            .activity_messages(session.clone(), filter, collection.next)
            .await?;
        collection.append(page)?;
        if collection.next.is_none() {
            break;
        }
    }
    Ok(Loaded::Window(collection))
}

#[cfg(test)]
mod tests;
