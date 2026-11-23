//! Режим сайдбара для создания разговоров, фильтрации каталога и работы с архивом.

mod catalog_view;
mod channel;
mod people;
#[cfg(debug_assertions)]
pub(super) mod preview;

use gpui::{
    App, Context, Div, Entity, EventEmitter, FocusHandle, KeyBinding, Render, Stateful, Window,
    div, prelude::*,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        catalog::{Chat, NewChatCatalog},
        creation::ConversationCreationError,
    },
    storage::HistoryScope,
    sync::SyncUpdate,
};

use crate::gpui_client::{
    contracts::{AccountSession, ChatEntry},
    preferences::{Language, Preferences},
    runtime::RuntimeClient,
    ui::{
        Button, ButtonAppearance, FilterGroup, Icon, Palette, TextInput, TextRole, label,
        popup::tooltip,
    },
};

use self::{catalog_view::Filter, people::People};
use super::search;

pub(in crate::gpui_client) use channel::ChannelDialog;

gpui::actions!(new_chats, [Back]);

/// Состояние одной загрузки каталога или пользователей.
enum Load<T> {
    Idle,     // Запрос ещё не начался.
    Pending,  // Рабочий поток читает данные.
    Ready(T), // Последний успешный снимок.
    Failed,   // Пользователь может повторить запрос.
}

impl<T> Load<T> {
    /// Сообщает, что можно предложить повтор запроса.
    fn failed(&self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// Причина, которую можем объяснить при открытии лички.
#[derive(Clone, Copy)]
enum DirectError {
    Failed,   // Пользователь может повторно выбрать собеседника.
    Archived, // Существующая личка оставлена в архиве.
}

/// Подтверждённая цель, которую окно открывает обычным маршрутом разговора.
pub(in crate::gpui_client) struct Navigation {
    pub session: AccountSession, // Вход, в котором пользователь начал действие.
    pub scope: HistoryScope,     // Стрим и тема, если это канал.
    pub entry: ChatEntry,        // Обычное открытие или чтение архива.
    pub chat: Option<Chat>,      // Готовый заголовок, если строка пришла из отдельной выборки.
}

/// Каталог новых чатов и вложенный выбор коллеги.
pub(in crate::gpui_client) struct NewChats {
    runtime: Option<RuntimeClient>, // None у изолированного сайдбара без запросов.
    session: Option<AccountSession>, // Последний выбранный вход.
    open: bool,                     // Панель заняла место обычного списка.
    choosing: bool,                 // Панель показывает поиск коллеги.
    filter_group: FilterGroup,      // Положение и фокус единого ряда фильтров.
    filter: Filter,                 // Активный фильтр нижнего блока.
    search: Entity<TextInput>,      // Фильтрация текущего набора.
    people: People,                 // Справочник и фото для личного чата.
    catalog: Load<NewChatCatalog>,  // Списки активных и архивных разговоров.
    opener: FocusHandle,            // Фокус плюсика после закрытия панели.
    start: FocusHandle,             // Фокус «Начать чат» после выбора или отмены.
    channel: FocusHandle,           // Фокус после закрытия модалки канала.
    people_revision: u64,           // Отличает каждый новый выбор коллеги.
    revision: u64,                  // Отделяет ответы закрытой панели.
    catalog_revision: u64,          // Отделяет старые снимки при повторной загрузке.
    request_channel: bool,          // Модалку нужно открыть на следующем кадре окна.
    close_channel: bool,            // Модалка завершилась и освобождает фокус.
    open_person: Option<Uuid>,      // Запрошенная личка, повтор временно недоступен.
    direct_error: Option<DirectError>, // Причина ошибки или None перед открытием.
    restoring: Option<Uuid>,        // Архивный разговор, который восстанавливается.
    restore_failed: bool,           // Сервер отклонил восстановление.
}

impl EventEmitter<Navigation> for NewChats {}

impl NewChats {
    /// Создаёт вторую панель рядом с существующим каталогом.
    pub(super) fn new(runtime: Option<RuntimeClient>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::for_search(String::new(), cx));
        cx.observe(&search, |_, _, cx| cx.notify()).detach();
        Self {
            runtime,
            session: None,
            open: false,
            choosing: false,
            filter_group: FilterGroup::new(Filter::ALL.len(), cx),
            filter: Filter::All,
            search,
            people: People::new(cx),
            catalog: Load::Idle,
            opener: cx.focus_handle(),
            start: cx.focus_handle(),
            channel: cx.focus_handle(),
            people_revision: 0,
            revision: 0,
            catalog_revision: 0,
            request_channel: false,
            close_channel: false,
            open_person: None,
            direct_error: None,
            restoring: None,
            restore_failed: false,
        }
    }

    /// Добавляет Escape для возврата из нового режима каталога.
    pub(super) fn bind_keys(app: &mut App) {
        app.bind_keys([KeyBinding::new("escape", Back, Some("NewChats"))]);
    }

    /// Убирает результаты старой сессии вместе с поиском и текущими операциями.
    pub(super) fn select(&mut self, session: Option<&AccountSession>, cx: &mut Context<Self>) {
        if self.session.as_ref() != session {
            self.reset(cx);
            self.session = session.cloned();
        }
    }

    /// Сообщает, что обычный каталог сейчас заменён новой панелью.
    pub(in crate::gpui_client) fn is_open(&self) -> bool {
        self.open
    }

    /// Проверяет, что видимый поиск уже получил список коллег для этого открытия.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn people_ready_for_test(&self) -> bool {
        self.choosing && matches!(&self.people.data, Load::Ready(_))
    }

    /// Возвращает видимое поле для системного сочетания поиска и фокуса сайдбара.
    pub(super) fn active_input(&self) -> &Entity<TextInput> {
        if self.choosing {
            &self.people.query
        } else {
            &self.search
        }
    }

    /// Ведёт модалку к текущему входу и запоминает кнопку возврата фокуса.
    pub(in crate::gpui_client) fn take_channel_request(
        &mut self,
    ) -> Option<(AccountSession, FocusHandle, u64)> {
        if std::mem::take(&mut self.request_channel) && self.open {
            Some((self.session.clone()?, self.channel.clone(), self.revision))
        } else {
            None
        }
    }

    /// Забирает сообщение закрытия однажды, когда у окна доступен фокус.
    pub(in crate::gpui_client) fn take_channel_close(&mut self) -> bool {
        std::mem::take(&mut self.close_channel)
    }

    /// Показывает плюсик рядом со штатным поиском без вмешательства в поиск папки.
    pub(super) fn opener(
        view: &Entity<Self>,
        palette: Palette,
        locale: Language,
        searching: bool,
        app: &App,
    ) -> Stateful<Div> {
        let weak = view.downgrade();
        let mut button = Button::new("new-chats-open", "");
        button.appearance = ButtonAppearance::Ghost;
        button.icon = Some(Icon::Add);
        let focus = &view.read(app).opener;
        button
            .render(palette, Some(focus), move |window, app| {
                let _ = weak.update(app, |view, cx| view.open(window, cx));
            })
            // Во время фильтрации Tab после очистки должен вести к результатам.
            .track_focus(&focus.clone().tab_stop(!searching))
            .tooltip(move |_, app| {
                tooltip(palette, locale.text("Новый чат", "New chat").into(), app)
            })
            .debug_selector(|| "new-chats-open".into())
    }

    /// Начинает новый режим, сохраняя прежний каталог у родителя.
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.search.read(cx).focus(window);
        self.load_catalog(cx);
        cx.notify();
    }

    /// Возвращается к списку каналов или в прежнюю папку с её позицией.
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.choosing {
            self.choosing = false;
            self.people_revision = self.people_revision.wrapping_add(1);
            self.open_person = None;
            self.people.reset(cx);
            window.focus(&self.start);
        } else {
            self.reset(cx);
            window.focus(&self.opener);
        }
        cx.notify();
    }

    /// Отвязывает панель и делает все предыдущие ответы устаревшими.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.choosing = false;
        self.filter = Filter::All;
        self.filter_group.reset();
        self.revision = self.revision.wrapping_add(1);
        self.search.update(cx, TextInput::clear);
        self.people.reset(cx);
        self.catalog = Load::Idle;
        self.request_channel = false;
        self.close_channel = false;
        self.open_person = None;
        self.direct_error = None;
        self.restoring = None;
        self.restore_failed = false;
    }

    /// Убирает старые строки при отзыве доступа, даже если обычный каталог не изменился.
    pub(in crate::gpui_client) fn reload_after_access_change(
        &mut self,
        update: &SyncUpdate,
        cx: &mut Context<Self>,
    ) {
        if self.open
            && self.session.as_ref().is_some_and(|session| {
                session.owner == update.context && session.generation == update.generation
            })
        {
            self.load_catalog(cx);
        }
    }

    /// Читает активные и архивные стримы одного аккаунта.
    pub(super) fn load_catalog(&mut self, cx: &mut Context<Self>) {
        let (Some(runtime), Some(session)) = (self.runtime.clone(), self.session.clone()) else {
            self.catalog = Load::Failed;
            return;
        };
        self.catalog = Load::Pending;
        self.catalog_revision = self.catalog_revision.wrapping_add(1);
        let revision = self.revision;
        let request = self.catalog_revision;
        cx.spawn(async move |entity, cx| {
            let result = runtime.new_chat_catalog(session).await;
            let _ = entity.update(cx, |view, cx| {
                if view.revision == revision && view.catalog_revision == request {
                    view.catalog = result.map_or(Load::Failed, Load::Ready);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Переключает сайдбар на выбор одного коллеги и открывает поиск.
    fn choose_person(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.choosing = true;
        self.direct_error = None;
        self.people.query.read(cx).focus(window);
        self.load_people(cx);
        cx.notify();
    }

    /// Загружает справочник для поиска коллеги, в том числе без общей истории.
    fn load_people(&mut self, cx: &mut Context<Self>) {
        let (Some(runtime), Some(session)) = (self.runtime.clone(), self.session.clone()) else {
            self.people.data = Load::Failed;
            return;
        };
        self.people.data = Load::Pending;
        self.people_revision = self.people_revision.wrapping_add(1);
        let revision = (self.revision, self.people_revision);
        cx.spawn(async move |entity, cx| {
            let result = runtime.new_chat_people(session).await;
            let _ = entity.update(cx, |view, cx| {
                if (view.revision, view.people_revision) == revision && view.choosing {
                    view.people.data = match result {
                        Ok(Ok(people)) => Load::Ready(people),
                        _ => Load::Failed,
                    };
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Загружает фотографии только людей, видимых в личном поиске.
    fn visible_people(&mut self, users: Vec<Uuid>, cx: &mut Context<Self>) {
        if self.choosing
            && let (Some(runtime), Some(session)) = (&self.runtime, &self.session)
        {
            self.people.set_visible(users, runtime, session, cx);
        }
    }

    /// Запрашивает один личный стрим и открывает его после подтверждения Core.
    fn open_direct(&mut self, user: Uuid, cx: &mut Context<Self>) {
        let (Some(runtime), Some(session)) = (self.runtime.clone(), self.session.clone()) else {
            return;
        };
        if !self.choosing || self.open_person.is_some() {
            return;
        }
        self.open_person = Some(user);
        self.direct_error = None;
        let revision = (self.revision, self.people_revision);
        cx.spawn(async move |entity, cx| {
            let result = runtime.new_chat_open_direct(session.clone(), user).await;
            let _ = entity.update(cx, |view, cx| {
                if (view.revision, view.people_revision) != revision
                    || view.open_person != Some(user)
                {
                    return;
                }
                view.open_person = None;
                match result {
                    Ok(Ok(stream_id)) => {
                        view.reset(cx);
                        cx.emit(Navigation {
                            session,
                            scope: HistoryScope {
                                stream_id,
                                topic_id: None,
                            },
                            entry: ChatEntry::Latest,
                            chat: None,
                        });
                    }
                    Ok(Err(ConversationCreationError::Archived)) => {
                        view.direct_error = Some(DirectError::Archived);
                    }
                    _ => view.direct_error = Some(DirectError::Failed),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Даёт окну запрос открыть этап создания общего канала.
    fn create_channel(&mut self, cx: &mut Context<Self>) {
        self.request_channel = true;
        cx.notify();
    }

    /// Завершает текущую модалку и открывает успешно созданный канал.
    pub(in crate::gpui_client) fn finish_channel(
        &mut self,
        session: AccountSession,
        revision: u64,
        scope: Option<HistoryScope>,
        cx: &mut Context<Self>,
    ) {
        if self.revision != revision || self.session.as_ref() != Some(&session) {
            return;
        }
        if let Some(scope) = scope {
            self.reset(cx);
            cx.emit(Navigation {
                session,
                scope,
                entry: ChatEntry::Latest,
                chat: None,
            });
        }
        self.close_channel = true;
        cx.notify();
    }

    /// Открывает строку подтверждённого каталога или её архивную историю.
    fn navigate_row(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let (Some(session), Load::Ready(catalog)) = (self.session.clone(), &self.catalog) else {
            return;
        };
        let archived = self.filter == Filter::Archive;
        let Some(chat) = self
            .filter
            .rows(catalog)
            .iter()
            .find(|row| row.id == id && self.filter.includes(row))
        else {
            return;
        };
        let Some(scope) = row_scope(chat) else { return };
        let entry = if archived {
            ChatEntry::Archived
        } else {
            ChatEntry::Latest
        };
        let heading = row_heading(chat);
        self.reset(cx);
        cx.emit(Navigation {
            session,
            scope,
            entry,
            chat: Some(heading),
        });
        cx.notify();
    }

    /// Восстанавливает архивную строку и перечитывает оба набора.
    fn restore(&mut self, stream: Uuid, cx: &mut Context<Self>) {
        let (Some(runtime), Some(session)) = (self.runtime.clone(), self.session.clone()) else {
            return;
        };
        if self.filter != Filter::Archive
            || self.restoring.is_some()
            || !matches!(&self.catalog, Load::Ready(catalog) if catalog.archived.iter().any(|chat| chat.id == stream))
        {
            return;
        }
        self.restoring = Some(stream);
        self.restore_failed = false;
        let revision = self.revision;
        cx.spawn(async move |entity, cx| {
            let result = runtime.restore_archived_chat(session, stream).await;
            let _ = entity.update(cx, |view, cx| {
                if view.revision != revision {
                    return;
                }
                view.restoring = None;
                if result.is_ok() {
                    view.load_catalog(cx);
                } else {
                    view.restore_failed = true;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Собирает поиск коллеги или каналов с общими карточками и кнопками.
    fn content(&self, palette: Palette, window: &Window, cx: &Context<Self>) -> Div {
        let language = Preferences::read(cx).language;
        if self.choosing {
            let mut group = div()
                .flex()
                .flex_col()
                .gap_2()
                .min_h_0()
                .flex_1()
                .child(self.people.search(palette, window, cx));
            if self.people.data.failed() {
                group = group.child(
                    Button::new(
                        "new-chats-retry-people",
                        language.text("Повторить", "Retry"),
                    )
                    .render(
                        palette,
                        None,
                        action(cx, |view, _, cx| view.load_people(cx)),
                    ),
                );
            }
            if let Some(error) = self.direct_error {
                let message = match error {
                    DirectError::Failed => language.text(
                        "Не удалось открыть чат. Выберите коллегу снова.",
                        "Could not open the chat. Select the person again.",
                    ),
                    DirectError::Archived => {
                        language.text("Личка находится в архиве.", "This chat is archived.")
                    }
                };
                group = group
                    .child(label(palette, TextRole::Caption, message).text_color(palette.danger));
                if matches!(error, DirectError::Archived) {
                    group = group.child(
                        Button::new(
                            "new-chats-open-archive",
                            language.text("Открыть архив", "Open archive"),
                        )
                        .render(
                            palette,
                            None,
                            action(cx, |view, window, cx| {
                                view.back(window, cx);
                                view.filter = Filter::Archive;
                                view.search.update(cx, TextInput::clear);
                                view.search.read(cx).focus(window);
                            }),
                        ),
                    );
                }
            }
            return group.child(self.people.results(
                palette,
                self.open_person.is_some(),
                cx,
                Self::open_direct,
                Self::visible_people,
            ));
        }
        let action_buttons = [
            (
                "new-chats-start-chat",
                language.text("Начать чат", "Start a chat"),
                Icon::PersonAdd,
                Self::choose_person as fn(&mut Self, &mut Window, &mut Context<Self>),
            ),
            (
                "new-chats-create-channel",
                language.text("Создать канал", "Create a channel"),
                Icon::Channels,
                |view, _, cx| view.create_channel(cx),
            ),
        ];
        div()
            .flex()
            .flex_col()
            .gap_2()
            .min_h_0()
            .flex_1()
            .child(search::render(&self.search, palette, window, cx))
            .children(
                action_buttons
                    .into_iter()
                    .map(|(id, title, icon, callback)| {
                        let mut button = Button::new(id, title);
                        button.icon = Some(icon);
                        button.appearance = ButtonAppearance::Ghost;
                        button
                            .render(
                                palette,
                                (id == "new-chats-start-chat")
                                    .then_some(&self.start)
                                    .or(Some(&self.channel)),
                                action(cx, callback),
                            )
                            .justify_start()
                            .debug_selector(move || id.into())
                    }),
            )
            .child(
                label(
                    palette,
                    TextRole::Heading,
                    language.text("Каналы", "Channels"),
                )
                .pt_2(),
            )
            .child(self.catalog_filters(palette, language, window, cx))
            .child(self.catalog_list(palette, language, cx))
    }
}

impl Render for NewChats {
    /// Показывает режим внутри той же области сайдбара и обрабатывает возврат.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read(cx).language;
        let title = if self.choosing {
            language.text("Начать чат", "Start a chat")
        } else {
            language.text("Новые чаты и каналы", "New chats and channels")
        };
        let mut back = Button::new("new-chats-back", "");
        back.icon = Some(Icon::ChevronLeft);
        back.appearance = ButtonAppearance::Ghost;
        div()
            .id("new-chats-panel")
            .key_context("NewChats")
            .on_action(cx.listener(|view, _: &Back, window, cx| {
                cx.stop_propagation();
                view.back(window, cx);
            }))
            .debug_selector(|| "new-chats-panel".into())
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        back.render(palette, None, action(cx, Self::back))
                            .debug_selector(|| "new-chats-back".into()),
                    )
                    .child(label(palette, TextRole::Heading, title)),
            )
            .child(self.content(palette, window, cx))
    }
}

/// Создаёт обработчик GPUI по слабой ссылке на панель.
fn action(
    cx: &Context<NewChats>,
    callback: impl Fn(&mut NewChats, &mut Window, &mut Context<NewChats>) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = cx.entity().downgrade();
    move |window, app| {
        let _ = target.update(app, |view, cx| callback(view, window, cx));
    }
}

/// Определяет обычный адрес или доступную тему в архивном канале.
fn row_scope(chat: &Chat) -> Option<HistoryScope> {
    let topic_id = if !chat.direct {
        Some(
            chat.metadata
                .default_topic_id
                .or_else(|| chat.topics.first().map(|topic| topic.id))?,
        )
    } else {
        None
    };
    Some(HistoryScope {
        stream_id: chat.id,
        topic_id,
    })
}

/// Сохраняет подтверждённые подписи из снимка для заголовка архива.
fn row_heading(chat: &Chat) -> Chat {
    Chat {
        id: chat.id,
        name: chat.name.clone(),
        direct: chat.direct,
        metadata: workspace_core::messenger::catalog::ChatMetadata {
            direct_peer_id: chat.metadata.direct_peer_id,
            ..Default::default()
        },
        topics: chat
            .topics
            .iter()
            .map(|topic| workspace_core::messenger::catalog::Topic {
                id: topic.id,
                name: topic.name.clone(),
                metadata: Default::default(),
            })
            .collect(),
    }
}
