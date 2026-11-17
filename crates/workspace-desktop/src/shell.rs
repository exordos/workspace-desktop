//! Собирает окно Iced с переключателем аккаунтов и счётчиками.
//! Сначала описаны состояние и вид окна, затем подписка на авторизацию и синхронизацию.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use futures::stream::{self, Stream, StreamExt};
use futures::{SinkExt, channel::mpsc, future::BoxFuture};
use iced::keyboard::{self, Key, key::Named};
use iced::widget::{column, container, keyed, operation, responsive, row, rule, text, text_editor};
use iced::{Element, Event, Fill, Subscription, Task, Theme, event, window};
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;
use workspace_core::auth::{Auth, SessionSnapshot, SessionStatus};
use workspace_core::bootstrap::{ChatCatalog, ChatList, ChatLists};
use workspace_core::context::{AccountId, OwnerContext, SessionGeneration};
use workspace_core::messenger::send::DirectSendError;
use workspace_core::messenger::{MessengerSource, history::OpenHistoryError};
use workspace_core::notification_target::{NotificationTarget, NotificationTargetError};
use workspace_core::storage::{HistoryEdge, HistoryScope, HistoryWindow};
use workspace_core::sync::{HistoryRevocation, Sync, SyncChange, SyncUpdate};

use crate::platform::notifications::{NotificationClick, NotificationError, Notifications};
use crate::ui_kit::{ThemeMode, TopBarAccount, top_bar, workspace_theme};

mod composer;
mod history;
mod history_anchor;
mod history_input;
mod history_layout;
mod sidebar;
use history::{HistoryScreen, HistoryState};
pub use sidebar::SidebarAction;

/// Состояние окна, которое Iced меняет в ответ на действия пользователя и события `core`.
pub struct Shell {
    accounts: Accounts, // Список аккаунтов и текущий выбор.
    chats: ChatLists,   // Каталог каждого владельца и состояние загрузки.
    sidebars: HashMap<(OwnerContext, SessionGeneration), sidebar::Sidebar>, // Поиск и раскрытие каждого входа.
    labels: HashMap<AccountId, String>, // Читаемые имена для известных идентификаторов.
    sources: Sources,                   // Входные потоки авторизации и мессенджера.
    notifications: Box<dyn NotificationDelivery>, // Получатель запросов оболочки на баннер.
    notification_error: Option<NotificationError>, // Причина, по которой ОС не показала баннер.
    notification_route_error: Option<&'static str>, // Причина, по которой цель баннера недоступна.
    notification_revision: u64,         // Номер проверки баннера, независимый от открытой истории.
    history: Option<HistoryScreen>,     // История, которую открыл последний переход.
    navigation_revision: u64,           // Номер последнего перехода, повтора или выбора аккаунта.
}

/// Снимок сессии и сохранённое значение счётчика «Все чаты».
#[derive(Clone, Debug)]
pub struct AccountSummary {
    session: SessionSnapshot, // Владелец, состояние и поколение сессии.
    unread: Option<u32>,      // Число с сервера. `None` до загрузки или после завершения сессии.
}

/// Адрес и начальная позиция открытого разговора.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatRoute {
    pub owner: OwnerContext, // Аккаунт и проект, которым принадлежит история.
    pub scope: HistoryScope, // UUID разговора и, для канала, конкретной темы.
    pub entry: ChatEntry,    // Место, которое нужно показать после загрузки.
}

/// Место открытия истории, которое задаёт действие пользователя.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatEntry {
    Latest,       // Конец текущей истории.
    Around(Uuid), // Строка сообщения из уведомления.
}

/// События, которые могут изменить окно или вызвать системное действие.
#[derive(Clone, Debug)]
pub enum Message {
    Select(AccountId, SessionGeneration), // Аккаунт и поколение входа на момент клика.
    Snapshot(Vec<AccountSummary>),        // Свежий список сессий и счётчиков.
    Chats(ChatLists),                     // Свежие состояния загрузки каталогов.
    Sidebar {
        owner: OwnerContext,           // Владелец открытого каталога в момент действия.
        generation: SessionGeneration, // Поколение этого входа.
        action: SidebarAction,         // Поиск, раскрытие или повтор загрузки.
    },
    OpenChat {
        route: ChatRoute,              // Разговор из показанного каталога.
        generation: SessionGeneration, // Вход, к которому относится каталог.
    },
    RetryHistory(u64),  // Повтор загрузки для указанного номера перехода.
    SyncLagged,         // Часть событий Sync потерялась, историю нужно прочитать заново.
    Synced(SyncUpdate), // Синхронизация завершила запись события.
    NotificationFinished(Result<(), NotificationError>), // Результат обращения к ОС.
    NotificationClicked(NotificationClick), // macOS передала цель нажатого баннера.
    NotificationResolved {
        revision: u64,                 // Номер действия, которое запустило проверку.
        generation: SessionGeneration, // Вход, к которому относился баннер.
        result: Result<NotificationTarget, Arc<NotificationTargetError>>, // Проверенная цель или причина отказа.
    },
    HistoryLoaded {
        revision: u64, // Номер клика, запустившего чтение.
        result: Result<Arc<HistoryWindow>, Arc<OpenHistoryError>>, // Окно или причина отказа.
    },
    HistoryCached {
        revision: u64,              // Номер перехода, для которого доступ подтверждён.
        window: Arc<HistoryWindow>, // Пригодный хвост, пока свежий запрос выполняется.
    },
    SendAddressResolved {
        revision: u64,                              // Переход, для которого проверили личку.
        result: Result<Uuid, Arc<DirectSendError>>, // Точная тема или объяснение отказа.
    },
    EditConversationText {
        revision: u64,               // Переход, которому принадлежит поле.
        action: text_editor::Action, // Действие в редакторе Iced.
    },
    SendConversationText(u64), // Клик по отправке в текущем переходе.
    ConversationTextSaved {
        revision: u64, // Редактор, чей текст сохраняли.
        version: u64,  // Версия отправленного текста.
        saved: bool,   // Снимок записан на диск.
    },
    RetrySendAddress(u64), // Повторить проверку в текущем разговоре.
    SeeLatest(u64),        // Перейти к свежему хвосту той же истории.
    ScrollToHistory {
        revision: u64,       // Номер клика, для которого измерены координаты.
        offset: Option<f32>, // Положение цели от начала списка в пикселях.
    },
    HistoryUserScrolled {
        revision: u64, // Номер открытого маршрута.
    }, // Пользователь прокрутил открытый список.
    HistoryManualScroll(u64), // Жест, который отменяет коррекцию, не запуская догрузку вверх.
    HistoryEdgeMeasured {
        revision: u64,                                   // Номер открытого маршрута.
        position: Option<history_anchor::HistoryLayout>, // Разметка списка после жеста.
    }, // Позиция после жеста.
    HistoryOlderLoaded {
        revision: u64,  // Номер маршрута в начале запроса.
        window_id: i64, // ID окна, чей край продолжали.
        result: Result<Arc<HistoryWindow>, Arc<OpenHistoryError>>, // Полученное окно или ошибка.
    }, // Ответ старой страницы.
    HistoryBeforeInsert {
        revision: u64,                                   // Номер открытого маршрута.
        layout_revision: u64,                            // Номер ожидаемой перестройки строк.
        scroll_revision: u64, // Ревизия ручной прокрутки на момент начала измерения.
        position: Option<history_anchor::HistoryLayout>, // Координаты прежней разметки.
    }, // Разметка до вставки, правки или удаления.
    HistoryAfterInsert {
        revision: u64,                                // Номер открытого маршрута.
        layout_revision: u64,                         // Номер завершившейся перестройки строк.
        scroll_revision: u64, // Ручной ввод после этой ревизии имеет приоритет.
        before: history_anchor::HistoryLayout, // Координаты до перестройки.
        after: Option<history_anchor::HistoryLayout>, // Координаты после перестройки.
        follow_tail: bool,    // Читатель был у подтверждённого конца истории.
    }, // Разметка после обновления.
    MoveFocus(bool),          // Перейти назад по Shift+Tab или вперёд по Tab.
}

/// Хранит текущий экран независимо от входящих обновлений счётчиков.
#[derive(Default)]
struct Accounts {
    summaries: Vec<AccountSummary>, // Сессии в порядке, который вернула авторизация.
    selected: Option<AccountId>,    // Аккаунт, открытый в окне.
    preferred: Option<AccountId>,   // Аккаунт, который нужно открыть при восстановлении.
}

/// Принимает запросы оболочки на системный баннер и сообщает результат.
trait NotificationDelivery {
    /// Запрашивает разрешение на показ баннеров.
    fn prepare(&self) -> BoxFuture<'static, Result<(), NotificationError>>;

    /// Передаёт баннер с заданным идентификатором получателю.
    fn show(
        &self,
        id: &str,
        title: &str,
        body: &str,
        click: &NotificationClick,
    ) -> BoxFuture<'static, Result<(), NotificationError>>;

    /// Передаёт подписке оболочки очередь нажатий из системы.
    fn take_clicks(&self) -> Option<mpsc::Receiver<NotificationClick>> {
        None
    }
}

impl NotificationDelivery for Notifications {
    /// Просит разрешение через системный адаптер.
    fn prepare(&self) -> BoxFuture<'static, Result<(), NotificationError>> {
        Box::pin(self.prepare())
    }

    /// Отправляет запрос через системный адаптер.
    fn show(
        &self,
        id: &str,
        title: &str,
        body: &str,
        click: &NotificationClick,
    ) -> BoxFuture<'static, Result<(), NotificationError>> {
        Box::pin(self.show(id, title, body, &click.target, click.generation))
    }

    /// Забирает очередь нажатий из системного адаптера.
    fn take_clicks(&self) -> Option<mpsc::Receiver<NotificationClick>> {
        self.take_clicks()
    }
}

impl Shell {
    /// Создаёт окно на главном потоке. Подписка прочитает сохранённые сессии и счётчики.
    pub fn new(
        auth: Auth,
        labels: HashMap<AccountId, String>,
        sync: Arc<Sync>,
        messenger: (Arc<ChatCatalog>, Arc<MessengerSource>),
        preferred_account: Option<AccountId>,
        subscribed: oneshot::Sender<()>,
    ) -> Self {
        Self::with_notifications(
            auth,
            labels,
            sync,
            messenger,
            preferred_account,
            subscribed,
            Box::new(Notifications::new()),
        )
    }

    /// Собирает оболочку с переданным получателем уведомлений.
    fn with_notifications(
        auth: Auth,
        labels: HashMap<AccountId, String>,
        sync: Arc<Sync>,
        messenger: (Arc<ChatCatalog>, Arc<MessengerSource>),
        preferred_account: Option<AccountId>,
        subscribed: oneshot::Sender<()>,
        notifications: Box<dyn NotificationDelivery>,
    ) -> Self {
        let (catalog, messenger) = messenger;
        let clicks = notifications.take_clicks();
        Self {
            accounts: Accounts {
                preferred: preferred_account,
                ..Accounts::default()
            },
            chats: ChatLists::new(),
            sidebars: HashMap::new(),
            labels,
            sources: Sources(Arc::new(SourceData {
                auth,
                sync,
                catalog,
                messenger,
                subscribed: Mutex::new(Some(subscribed)),
                clicks: Mutex::new(clicks),
            })),
            notifications,
            notification_error: None,
            notification_route_error: None,
            notification_revision: 0,
            navigation_revision: 0,
            history: None,
        }
    }

    /// Запрашивает разрешение на баннеры сразу после открытия окна.
    pub fn prepare_notifications(&self) -> Task<Message> {
        Task::perform(self.notifications.prepare(), Message::NotificationFinished)
    }

    /// Обрабатывает выбор аккаунта, новые счётчики и запросы на баннеры.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Select(account, generation) => {
                if !self.accounts.summaries.iter().any(|summary| {
                    summary.session.context.account_id == account
                        && summary.session.generation == generation
                }) {
                    return Task::none();
                }
                self.cancel_notification_open();
                if self.accounts.selected.as_ref() == Some(&account)
                    && self.visible_history().is_some()
                {
                    return Task::none();
                }
                self.navigation_revision += 1;
                self.history = None;
                self.accounts.select(account);
            }
            Message::Snapshot(accounts) => {
                self.sidebars.retain(|(owner, generation), _| {
                    accounts.iter().any(|summary| {
                        summary.session.context == *owner
                            && summary.session.generation == *generation
                    })
                });
                self.accounts.replace(accounts);
            }
            Message::Chats(chats) => self.chats = chats,
            Message::Sidebar {
                owner,
                generation,
                action,
            } => {
                return self.update_sidebar(owner, generation, action);
            }
            Message::OpenChat { route, generation } => {
                self.accounts.replace(self.sources.0.snapshot());
                if self.is_selected_session(&route.owner, generation)
                    && route.entry == ChatEntry::Latest
                    && sidebar::catalog_title(self.catalog(&route.owner, generation), route.scope)
                        .is_some()
                {
                    self.cancel_notification_open();
                    if route.scope.topic_id.is_some() {
                        self.sidebars
                            .entry((route.owner.clone(), generation))
                            .or_default()
                            .expanded
                            .insert(route.scope.stream_id);
                    }
                    return self.open_history(route, generation);
                }
            }
            Message::RetryHistory(revision) => {
                self.accounts.replace(self.sources.0.snapshot());
                if revision == self.navigation_revision
                    && let Some(screen) = self.visible_history()
                    && screen.can_retry()
                {
                    let (route, generation) = (screen.route.clone(), screen.generation);
                    self.cancel_notification_open();
                    return self.reopen_editor_history(route, generation);
                }
            }
            Message::SyncLagged => {
                self.accounts.replace(self.sources.0.snapshot());
                if let Some(screen) = self.visible_history() {
                    return self.open_history(screen.route.clone(), screen.generation);
                }
                self.history = None;
            }
            Message::Synced(update) => match update.change {
                SyncChange::Unread(count) => self.accounts.set_unread(&update, count),
                SyncChange::MessengerReset => return self.reset_open_history(&update),
                SyncChange::HistoryMessageCreated { scope, message_id } => {
                    return self.refresh_open_history(&update, scope, Some(message_id));
                }
                SyncChange::HistoryAccessRevoked(revocation) => {
                    return self.revoke_open_history(&update, revocation);
                }
                SyncChange::IncomingDirectMessage {
                    ref conversation_id,
                    ref message_id,
                } => {
                    // Авторизация может активировать сессию до того, как её снимок дошёл до окна.
                    self.accounts.replace(self.sources.0.snapshot());
                    return self.notify(&update, conversation_id, message_id);
                }
                SyncChange::HistoryMessageChanged { message_id } => {
                    if let Some(screen) = self.visible_history()
                        && let HistoryState::Ready(window) = &screen.state
                        && (window.messages.iter().any(|row| row.id == message_id)
                            || screen.layout.pending.as_ref().is_some_and(|patch| {
                                patch.window.messages.iter().any(|row| row.id == message_id)
                            }))
                    {
                        return self.refresh_open_history(&update, screen.route.scope, None);
                    }
                }
                SyncChange::OutboxChanged { scope } => {
                    if let Some(screen) = self.visible_history()
                        && history_update_matches(screen, &update, scope)
                    {
                        return self.refresh_open_history(&update, scope, None);
                    }
                }
            },
            Message::NotificationFinished(result) => {
                self.notification_error = result.err();
                if let Some(error) = &self.notification_error {
                    log::warn!("Не удалось отправить уведомление macOS: {error}");
                }
            }
            Message::NotificationClicked(click) => return self.open_notification(click),
            Message::NotificationResolved {
                revision,
                generation,
                result,
            } => return self.finish_open_notification(revision, generation, result),
            Message::HistoryLoaded { revision, result } => {
                let task = self.finish_history(revision, result, false);
                if revision == self.navigation_revision
                    && self
                        .history
                        .as_ref()
                        .is_some_and(|screen| !screen.layout.measuring)
                {
                    self.refresh_outbox();
                }
                return task;
            }
            Message::HistoryCached { revision, window } => {
                let task = self.finish_history(revision, Ok(window), true);
                if revision == self.navigation_revision {
                    self.refresh_outbox();
                }
                return task;
            }
            Message::SendAddressResolved { revision, result } => {
                if let Some(screen) = self.ready_send_screen(revision)
                    && matches!(screen.send.address, composer::Address::Unchecked)
                {
                    screen.send.address = match result {
                        Ok(topic) => composer::Address::Ready(topic),
                        Err(error) => {
                            composer::Address::Unavailable(composer::unavailable_reason(&error))
                        }
                    };
                }
            }
            Message::RetrySendAddress(revision) => {
                if let Some(screen) = self.ready_send_screen(revision)
                    && matches!(screen.send.address, composer::Address::Unavailable(_))
                {
                    screen.send.address = composer::Address::Unchecked;
                }
            }
            Message::EditConversationText { revision, action } => {
                if let Some(screen) = self.ready_send_screen(revision)
                    && matches!(screen.send.address, composer::Address::Ready(_))
                {
                    screen.send.composer.edit(action);
                }
            }
            Message::SendConversationText(revision) => {
                return self.send_conversation_text(revision);
            }
            Message::ConversationTextSaved {
                revision,
                version,
                saved,
            } => return self.finish_conversation_text(revision, version, saved),
            Message::SeeLatest(revision) => {
                self.accounts.replace(self.sources.0.snapshot());
                if revision == self.navigation_revision
                    && let Some(screen) = self.visible_history()
                {
                    let mut route = screen.route.clone();
                    route.entry = ChatEntry::Latest;
                    let generation = screen.generation;
                    self.cancel_notification_open();
                    return self.reopen_editor_history(route, generation);
                }
            }
            Message::ScrollToHistory { revision, offset } => {
                if revision == self.navigation_revision
                    && self
                        .visible_history()
                        .is_some_and(|screen| !screen.user_scrolled)
                    && let Some(y) = offset
                {
                    return operation::scroll_to(
                        history::list_id(revision),
                        operation::AbsoluteOffset {
                            x: None,
                            y: Some(y),
                        },
                    );
                }
            }
            Message::HistoryUserScrolled { revision } => {
                if revision != self.navigation_revision {
                    return Task::none();
                }
                if self.visible_history().is_none() {
                    return Task::none();
                }
                let screen = self.history.as_mut().unwrap();
                screen.user_scrolled = true;
                screen.scroll_revision = screen.scroll_revision.wrapping_add(1);
                let HistoryState::Ready(window) = &screen.state else {
                    return Task::none();
                };
                if screen.refreshing
                    || screen.older_pending
                    || !matches!(window.older, HistoryEdge::More(_))
                {
                    return Task::none();
                }
                screen.older_pending = true;
                return history_anchor::measure(history::list_id(revision), Vec::new())
                    .map(move |position| Message::HistoryEdgeMeasured { revision, position });
            }
            Message::HistoryManualScroll(revision) => {
                if revision == self.navigation_revision && self.visible_history().is_some() {
                    let screen = self.history.as_mut().unwrap();
                    screen.user_scrolled = true;
                    screen.scroll_revision = screen.scroll_revision.wrapping_add(1);
                }
            }
            Message::HistoryEdgeMeasured { revision, position } => {
                self.accounts.replace(self.sources.0.snapshot());
                if revision != self.navigation_revision || self.visible_history().is_none() {
                    return Task::none();
                }
                let screen = self.history.as_mut().unwrap();
                if !screen.older_pending {
                    return Task::none();
                }
                if !position.is_some_and(|position| position.scroll() <= 1.0) {
                    screen.older_pending = false;
                    return Task::none();
                }
                let HistoryState::Ready(window) = &screen.state else {
                    screen.older_pending = false;
                    return Task::none();
                };
                if !matches!(window.older, HistoryEdge::More(_)) {
                    screen.older_pending = false;
                    return Task::none();
                }
                let owner = screen.route.owner.clone();
                let generation = screen.generation;
                let scope = window.scope;
                let window_id = window.id;
                let source = self.sources.0.clone();
                return Task::perform(
                    async move {
                        source
                            .messenger
                            .load_older_history(&source.sync, &owner, generation, scope, window_id)
                            .await
                            .map(Arc::new)
                            .map_err(Arc::new)
                    },
                    move |result| Message::HistoryOlderLoaded {
                        revision,
                        window_id,
                        result,
                    },
                );
            }
            Message::HistoryOlderLoaded {
                revision,
                window_id,
                result,
            } => {
                self.accounts.replace(self.sources.0.snapshot());
                if revision != self.navigation_revision || self.visible_history().is_none() {
                    return Task::none();
                }
                let screen = self.history.as_mut().unwrap();
                let HistoryState::Ready(current) = &screen.state else {
                    return Task::none();
                };
                if !screen.older_pending || current.id != window_id {
                    return Task::none();
                }
                match result {
                    Err(error) if matches!(*error, OpenHistoryError::AccessLost) => {
                        screen.fail(error);
                    }
                    Err(_) => {
                        screen.older_pending = false;
                        screen.older_failed = true;
                    }
                    Ok(window) if window.id == window_id => {
                        return self.queue_history_patch(true);
                    }
                    Ok(_) => {
                        screen.older_pending = false;
                    }
                }
            }
            Message::HistoryBeforeInsert {
                revision,
                layout_revision,
                scroll_revision,
                position,
            } => {
                return self.apply_history_patch(
                    revision,
                    layout_revision,
                    scroll_revision,
                    position,
                );
            }
            Message::HistoryAfterInsert {
                revision,
                layout_revision,
                scroll_revision,
                before,
                after,
                follow_tail,
            } => {
                return self.restore_history_position(
                    revision,
                    layout_revision,
                    scroll_revision,
                    before,
                    after,
                    follow_tail,
                );
            }
            Message::MoveFocus(true) => return operation::focus_previous(),
            Message::MoveFocus(false) => return operation::focus_next(),
        }
        if self
            .history
            .as_ref()
            .is_some_and(|screen| !self.is_current_history_owner(screen))
        {
            self.history = None;
        }
        Task::none()
    }

    /// Подписывается на все сессии независимо от выбранного аккаунта и обрабатывает Tab.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run_with(self.sources.clone(), Sources::events),
            self.composer_subscription(),
            event::listen().filter_map(|event| match event {
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: Key::Named(Named::Tab),
                    modifiers,
                    ..
                }) if !modifiers.alt() && !modifiers.control() && !modifiers.logo() => {
                    Some(Message::MoveFocus(modifiers.shift()))
                }
                _ => None,
            }),
        ])
    }

    /// Возвращает общую тему Workspace для окна.
    pub fn theme(&self) -> Theme {
        workspace_theme(ThemeMode::Light)
    }

    /// Показывает шапку над каталогом и разговором с независимой прокруткой.
    pub fn view(&self) -> Element<'_, Message> {
        let screen: Element<'_, Message> = match self.accounts.current() {
            Some(current) if current.session.status == SessionStatus::Active => {
                responsive(move |size| self.messenger_view(&current.session, size.width)).into()
            }
            Some(current) => container(text(session_status(current.session.status)))
                .padding(24)
                .height(Fill)
                .into(),
            None => container(text("Ожидание сохранённых сессий"))
                .padding(24)
                .height(Fill)
                .into(),
        };
        let mut content = column![top_bar(self.top_bar_accounts())].height(Fill);
        if let Some(reason) = self.notification_route_error {
            content = content.push(container(text(reason).size(13)).padding([8, 12]));
        }
        if self.notification_error.is_some() {
            content = content.push(
                container(text("Системные уведомления недоступны").size(13)).padding([8, 12]),
            );
        }
        content.push(screen).into()
    }

    /// Собирает две панели с независимой прокруткой каталога и истории.
    fn messenger_view<'a>(
        &'a self,
        session: &'a SessionSnapshot,
        width: f32,
    ) -> Element<'a, Message> {
        let catalog = self.catalog(&session.context, session.generation);
        let visible = self.visible_history();
        let selected = visible.map(|history| history.route.scope);
        let sidebar_width = (width * 0.42).clamp(160.0, 320.0).min(width * 0.55);
        let key = (session.context.clone(), session.generation);
        let sidebar = keyed::Column::new()
            .push(
                session.generation,
                sidebar::view(session, catalog, self.sidebars.get(&key), selected),
            )
            .width(sidebar_width)
            .height(Fill);
        let history = visible.map_or_else(
            || text("Выберите чат или тему").into(),
            |history| {
                let title = sidebar::catalog_title(catalog, history.route.scope).unwrap_or(
                    if history.route.scope.topic_id.is_some() {
                        "Тема канала"
                    } else {
                        "Личный чат"
                    },
                );
                history.view(self.navigation_revision, title)
            },
        );
        row![
            sidebar,
            rule::vertical(1),
            container(history).padding(16).width(Fill).height(Fill)
        ]
        .height(Fill)
        .into()
    }

    /// Возвращает каталог только для поколения, которому он принадлежит.
    fn catalog(&self, owner: &OwnerContext, generation: SessionGeneration) -> Option<&ChatList> {
        self.chats
            .get(owner)
            .filter(|list| list.generation == generation)
    }

    /// Применяет поиск, раскрытие или повтор только к каталогу, который пользователь ещё видит.
    fn update_sidebar(
        &mut self,
        owner: OwnerContext,
        generation: SessionGeneration,
        action: SidebarAction,
    ) -> Task<Message> {
        self.accounts.replace(self.sources.0.snapshot());
        if !self.is_selected_session(&owner, generation) {
            return Task::none();
        }
        match action {
            SidebarAction::Search(query) => {
                self.sidebars.entry((owner, generation)).or_default().query = query;
                operation::snap_to(
                    sidebar::scroll_id(generation),
                    operation::RelativeOffset::START,
                )
            }
            SidebarAction::ClearSearch => {
                self.sidebars
                    .entry((owner, generation))
                    .or_default()
                    .query
                    .clear();
                operation::snap_to(
                    sidebar::scroll_id(generation),
                    operation::RelativeOffset::START,
                )
                .chain(operation::focus(sidebar::search_id(generation)))
            }
            SidebarAction::ToggleChannel(stream) => {
                if sidebar::can_toggle(self.catalog(&owner, generation), stream) {
                    let expanded = &mut self
                        .sidebars
                        .entry((owner, generation))
                        .or_default()
                        .expanded;
                    if !expanded.remove(&stream) {
                        expanded.insert(stream);
                    }
                }
                Task::none()
            }
            SidebarAction::Retry => {
                self.sources.0.catalog.retry(owner, generation);
                Task::none()
            }
        }
    }

    /// Передаёт шапке снимки аккаунтов и действия, привязанные к этому входу.
    fn top_bar_accounts(&self) -> Vec<TopBarAccount<'_, Message>> {
        self.accounts
            .summaries
            .iter()
            .map(|summary| {
                let context = &summary.session.context;
                let account = &context.account_id;
                let server = context.server.trim_start_matches("https://");
                TopBarAccount {
                    id: &account.0,
                    generation: summary.session.generation.0,
                    label: self.label(account),
                    detail: format!("{server} · {}", context.project_id),
                    selected: self.accounts.selected.as_ref() == Some(account),
                    unread: summary.unread,
                    on_select: Message::Select(account.clone(), summary.session.generation),
                }
            })
            .collect()
    }

    /// Выбирает короткую подпись, если настройки не содержат имени аккаунта.
    fn label<'a>(&'a self, account: &'a AccountId) -> &'a str {
        self.labels
            .get(account)
            .map(String::as_str)
            .unwrap_or_else(|| account.0.get(..8).unwrap_or(&account.0))
    }

    /// Передаёт ОС событие фоновой сессии, которая всё ещё активна.
    fn notify(
        &self,
        update: &SyncUpdate,
        conversation_id: &str,
        message_id: &str,
    ) -> Task<Message> {
        if !self.accounts.is_background(update)
            || !self
                .sources
                .0
                .auth
                .snapshot()
                .iter()
                .any(|session| matches_session(session, update))
        {
            return Task::none();
        }
        let account = &update.context.account_id;
        let id = format!("{}/{}/{}", account.0, update.context.project_id, message_id);
        let body = format!("Аккаунт {}", self.label(account));
        let click = NotificationClick {
            target: NotificationTarget {
                context: update.context.clone(),
                conversation_id: conversation_id.to_owned(),
                message_id: message_id.to_owned(),
            },
            generation: update.generation,
        };
        let request = self
            .notifications
            .show(&id, "Новое сообщение", &body, &click);
        Task::perform(
            async move {
                let result = request.await;
                if result.is_ok() {
                    log::info!("macOS приняла запрос на уведомление для фонового аккаунта");
                }
                result
            },
            Message::NotificationFinished,
        )
    }

    /// Поднимает окно и передаёт цель в core на исполнителе задач Iced.
    fn open_notification(&mut self, click: NotificationClick) -> Task<Message> {
        self.cancel_notification_open();
        let revision = self.notification_revision;
        let sync = self.sources.0.sync.clone();
        let NotificationClick { target, generation } = click;
        let raise = window::oldest().then(|window| {
            window.map_or_else(Task::none, |id| {
                window::minimize(id, false).chain(window::gain_focus(id))
            })
        });
        let resolve = Task::perform(
            async move {
                sync.resolve_notification_target(target, generation)
                    .map_err(Arc::new)
            },
            move |result| Message::NotificationResolved {
                revision,
                generation,
                result,
            },
        );
        Task::batch([raise, resolve])
    }

    /// Отменяет ещё не подтверждённый переход из уведомления при новом действии пользователя.
    fn cancel_notification_open(&mut self) {
        self.notification_revision += 1;
        self.notification_route_error = None;
    }

    /// Применяет ответ core, если пользователь всё ещё ждёт этот переход и сессия активна.
    fn finish_open_notification(
        &mut self,
        revision: u64,
        generation: SessionGeneration,
        result: Result<NotificationTarget, Arc<NotificationTargetError>>,
    ) -> Task<Message> {
        if revision != self.notification_revision {
            return Task::none();
        }
        self.cancel_notification_open();
        let target = match result {
            Ok(target) => target,
            Err(error) => {
                log::warn!("Цель уведомления Workspace недоступна: {error}");
                self.notification_route_error = Some(match error.as_ref() {
                    NotificationTargetError::Auth(_) => "Аккаунт из уведомления недоступен",
                    NotificationTargetError::UnknownTarget => "Сообщение из уведомления недоступно",
                    NotificationTargetError::Storage(_) => "Не удалось открыть уведомление",
                });
                return Task::none();
            }
        };
        let (Ok(stream_id), Ok(message_id)) =
            (target.conversation_id.parse(), target.message_id.parse())
        else {
            self.notification_route_error = Some("Сообщение из уведомления недоступно");
            return Task::none();
        };
        self.accounts.replace(self.sources.0.snapshot());
        if !self.accounts.summaries.iter().any(|summary| {
            summary.session.context == target.context
                && summary.session.generation == generation
                && summary.session.status == SessionStatus::Active
        }) {
            self.notification_route_error = Some("Аккаунт из уведомления недоступен");
            return Task::none();
        }
        self.accounts.select(target.context.account_id.clone());
        self.open_history(
            ChatRoute {
                owner: target.context,
                scope: HistoryScope {
                    stream_id,
                    topic_id: None,
                },
                entry: ChatEntry::Around(message_id),
            },
            generation,
        )
    }

    /// Меняет окно того же разговора, сохраняя черновик и текущую запись на диск.
    fn reopen_editor_history(
        &mut self,
        route: ChatRoute,
        generation: SessionGeneration,
    ) -> Task<Message> {
        let send = std::mem::take(&mut self.history.as_mut().unwrap().send);
        let task = self.open_history(route, generation);
        self.history.as_mut().unwrap().send = send;
        task
    }

    /// Запускает переход. Для хвоста сначала показывает кэш после проверки доступа.
    fn open_history(&mut self, route: ChatRoute, generation: SessionGeneration) -> Task<Message> {
        self.navigation_revision += 1;
        let revision = self.navigation_revision;
        self.history = Some(HistoryScreen::loading(route.clone(), generation));
        self.history.as_mut().unwrap().send.revision = revision;
        let source = self.sources.0.clone();
        Task::stream(iced::stream::channel(2, async move |mut output| {
            let result = match route.entry {
                ChatEntry::Latest => match source
                    .messenger
                    .open_latest_history(&source.sync, &route.owner, generation, route.scope)
                    .await
                {
                    Ok(mut opening) => {
                        if let Some(window) = opening.cached.take()
                            && output
                                .send(Message::HistoryCached {
                                    revision,
                                    window: Arc::new(window),
                                })
                                .await
                                .is_err()
                        {
                            return;
                        }
                        opening.refresh().await
                    }
                    Err(error) => Err(error),
                },
                ChatEntry::Around(message_id) => match route.scope.topic_id {
                    Some(topic) => {
                        source
                            .messenger
                            .open_topic_history(
                                &source.sync,
                                &route.owner,
                                generation,
                                route.scope.stream_id,
                                topic,
                                message_id,
                            )
                            .await
                    }
                    None => {
                        let target = NotificationTarget {
                            context: route.owner,
                            conversation_id: route.scope.stream_id.to_string(),
                            message_id: message_id.to_string(),
                        };
                        source
                            .messenger
                            .open_direct_history(&source.sync, &target, generation)
                            .await
                    }
                },
            };
            let _ = output
                .send(Message::HistoryLoaded {
                    revision,
                    result: result.map(Arc::new).map_err(Arc::new),
                })
                .await;
        }))
    }

    /// Проверяет, что выбран тот же вход, из которого пришло действие.
    fn is_selected_session(&self, owner: &OwnerContext, generation: SessionGeneration) -> bool {
        self.accounts.current().is_some_and(|current| {
            current.session.context == *owner
                && current.session.generation == generation
                && current.session.status == SessionStatus::Active
        })
    }

    /// Показывает историю только в том входе, которому принадлежит запрос.
    fn visible_history(&self) -> Option<&HistoryScreen> {
        let history = self.history.as_ref()?;
        self.is_selected_session(&history.route.owner, history.generation)
            .then_some(history)
    }

    /// Проверяет владельца и поколение, даже когда авторизация временно недоступна.
    fn is_current_history_owner(&self, screen: &HistoryScreen) -> bool {
        self.accounts.current().is_some_and(|current| {
            current.session.context == screen.route.owner
                && current.session.generation == screen.generation
        })
    }

    /// Скрывает очищенную историю и заново проверяет доступ к прежней цели.
    fn reset_open_history(&mut self, update: &SyncUpdate) -> Task<Message> {
        self.accounts.replace(self.sources.0.snapshot());
        let Some(history) = self.history.as_ref() else {
            return Task::none();
        };
        if history.route.owner != update.context || history.generation != update.generation {
            return Task::none();
        }
        if self.is_selected_session(&history.route.owner, history.generation) {
            self.open_history(history.route.clone(), history.generation)
        } else {
            self.history = None;
            Task::none()
        }
    }

    /// Скрывает отозванную область и делает уже запущенные запросы устаревшими.
    fn revoke_open_history(
        &mut self,
        update: &SyncUpdate,
        revocation: HistoryRevocation,
    ) -> Task<Message> {
        let Some(screen) = self.history.as_ref() else {
            return Task::none();
        };
        if screen.route.owner != update.context || screen.generation != update.generation {
            return Task::none();
        }
        let scope = screen.route.scope;
        let matches = match revocation {
            HistoryRevocation::Stream(stream) => scope.stream_id == stream,
            HistoryRevocation::Topic {
                stream_id,
                topic_id,
            } => {
                if scope.stream_id == stream_id && scope.topic_id.is_none() {
                    // Личный чат открыт для всего стрима, а его окно содержало и строки темы.
                    return if self.is_selected_session(&screen.route.owner, screen.generation) {
                        self.open_history(screen.route.clone(), screen.generation)
                    } else {
                        self.history = None;
                        Task::none()
                    };
                }
                scope.stream_id == stream_id && scope.topic_id == Some(topic_id)
            }
        };
        if matches {
            self.navigation_revision += 1;
            self.history
                .as_mut()
                .expect("видимый экран истории существует")
                .fail(Arc::new(OpenHistoryError::AccessLost));
        }
        Task::none()
    }

    /// Запрашивает обновление разметки только для событий открытого разговора.
    fn refresh_open_history(
        &mut self,
        update: &SyncUpdate,
        scope: HistoryScope,
        created: Option<Uuid>,
    ) -> Task<Message> {
        let Some(screen) = self.visible_history() else {
            return Task::none();
        };
        if !history_update_matches(screen, update, scope) {
            return Task::none();
        }
        self.queue_history_change(false, created)
    }

    /// Сохраняет ответ, только если его маршрут и вход всё ещё открыты.
    fn finish_history(
        &mut self,
        revision: u64,
        result: Result<Arc<HistoryWindow>, Arc<OpenHistoryError>>,
        cached: bool,
    ) -> Task<Message> {
        if revision != self.navigation_revision {
            return Task::none();
        }
        self.accounts.replace(self.sources.0.snapshot());
        if self.visible_history().is_none() {
            self.history = None;
            return Task::none();
        }
        let history = self
            .history
            .as_mut()
            .expect("видимый экран истории существует");
        if cached && !history.refreshing {
            return Task::none();
        }
        if !cached {
            history.refreshing = false;
        }
        match result {
            Ok(window) if window.scope == history.route.scope => {
                // Создание могло сохраниться после снимка, пока экран ещё загружался.
                match self.sources.0.sync.refresh_history_window(
                    &history.route.owner,
                    history.generation,
                    window.scope,
                    window.id,
                ) {
                    Ok(Some(fresh)) => {
                        if matches!(&history.state, HistoryState::Ready(current) if **current != fresh)
                        {
                            return self.queue_refreshed_window(fresh);
                        }
                        history.state = HistoryState::Ready(Arc::new(fresh));
                        match history.route.entry {
                            ChatEntry::Around(id) => history::measure_anchor(revision, id),
                            ChatEntry::Latest if !history.user_scrolled => operation::snap_to(
                                history::list_id(revision),
                                operation::RelativeOffset::END,
                            ),
                            ChatEntry::Latest => Task::none(),
                        }
                    }
                    Ok(None) => {
                        if !cached {
                            history.fail(Arc::new(OpenHistoryError::StaleWindow));
                        }
                        Task::none()
                    }
                    Err(error) => {
                        if !cached {
                            history.fail(Arc::new(error.into()));
                        }
                        Task::none()
                    }
                }
            }
            Ok(_) => Task::none(),
            Err(error) => {
                if history.route.entry == ChatEntry::Latest
                    && !matches!(error.as_ref(), OpenHistoryError::AccessLost)
                    && let HistoryState::Ready(window) = &history.state
                    && let Ok(Some(fresh)) = self.sources.0.sync.refresh_history_window(
                        &history.route.owner,
                        history.generation,
                        history.route.scope,
                        window.id,
                    )
                {
                    history.state = HistoryState::Ready(Arc::new(fresh));
                    history.refresh_failed = true;
                } else {
                    history.fail(error);
                }
                Task::none()
            }
        }
    }
}

/// Ограничивает изменения истории и очереди открытым входом, стримом и темой.
fn history_update_matches(
    screen: &HistoryScreen,
    update: &SyncUpdate,
    scope: HistoryScope,
) -> bool {
    screen.route.owner == update.context
        && screen.generation == update.generation
        && screen.route.scope.stream_id == scope.stream_id
        && (screen.route.scope.topic_id.is_none() || screen.route.scope.topic_id == scope.topic_id)
}

impl Accounts {
    /// Обновляет список и сохраняет выбор, пока соответствующая сессия существует.
    fn replace(&mut self, summaries: Vec<AccountSummary>) {
        self.summaries = summaries;
        if let Some(preferred) = &self.preferred
            && self.contains(preferred)
        {
            self.selected = self.preferred.take();
            return;
        }
        if self.current().is_none() {
            self.selected = self
                .summaries
                .first()
                .map(|summary| summary.session.context.account_id.clone());
        }
    }

    /// Открывает известный аккаунт и запоминает явный выбор пользователя.
    fn select(&mut self, account: AccountId) {
        if self.contains(&account) {
            self.selected = Some(account);
            self.preferred = None;
        }
    }

    /// Проверяет, присутствует ли аккаунт в текущем списке сессий.
    fn contains(&self, account: &AccountId) -> bool {
        self.summaries
            .iter()
            .any(|summary| &summary.session.context.account_id == account)
    }

    /// Возвращает снимок аккаунта, открытого в окне.
    fn current(&self) -> Option<&AccountSummary> {
        let selected = self.selected.as_ref()?;
        self.summaries
            .iter()
            .find(|summary| &summary.session.context.account_id == selected)
    }

    /// Меняет счётчик только у сессии, которая получила событие.
    fn set_unread(&mut self, update: &SyncUpdate, count: u32) {
        if let Some(summary) = self
            .summaries
            .iter_mut()
            .find(|summary| matches_session(&summary.session, update))
        {
            summary.unread = Some(count);
        }
    }

    /// Проверяет, что окно открыто на другом аккаунте, а сессия получателя актуальна.
    fn is_background(&self, update: &SyncUpdate) -> bool {
        self.selected
            .as_ref()
            .is_some_and(|selected| *selected != update.context.account_id)
            && self
                .summaries
                .iter()
                .any(|summary| matches_session(&summary.session, update))
    }
}

/// Хранит ссылки на источники и задаёт постоянный идентификатор подписки Iced.
#[derive(Clone)]
struct Sources(Arc<SourceData>); // Одна и та же ссылка сохраняется при перерисовке окна.

/// Источники изменений сессий и сохранённых счётчиков.
struct SourceData {
    auth: Auth,                      // Снимки действующих и восстанавливаемых сессий.
    sync: Arc<Sync>,                 // Сводки SQLite и события после записи.
    messenger: Arc<MessengerSource>, // Запрос истории через штатный API core.
    catalog: Arc<ChatCatalog>,       // Состояния списков и команда повтора.
    subscribed: Mutex<Option<oneshot::Sender<()>>>, // Разрешает фоновому потоку запустить восстановление.
    clicks: Mutex<Option<mpsc::Receiver<NotificationClick>>>, // Очередь системных нажатий для подписки Iced.
}

impl Hash for Sources {
    /// Использует адрес общего объекта, чтобы сохранять подписку между обновлениями окна.
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

impl Sources {
    /// Объединяет события синхронизации и снимки авторизации в один поток Iced.
    fn events(&self) -> impl Stream<Item = Message> + use<> {
        let source = self.0.clone();
        let catalog = stream::unfold(
            (source.catalog.subscribe(), true),
            |(mut catalog, initial)| async move {
                if !initial {
                    catalog.changed().await.ok()?;
                }
                let chats = catalog.borrow_and_update().clone();
                Some((Message::Chats(chats), (catalog, false)))
            },
        );
        let clicks = source
            .clicks
            .lock()
            .unwrap()
            .take()
            .map(|receiver| receiver.map(Message::NotificationClicked));
        // Сначала подключаемся к потоку событий, чтобы видеть запись, которая завершится во время чтения снимка.
        let updates = source.sync.subscribe();
        let sessions = source.auth.subscribe().map({
            let source = source.clone();
            move |_| Message::Snapshot(source.snapshot())
        });
        if let Some(subscribed) = source.subscribed.lock().unwrap().take() {
            let _ = subscribed.send(());
        }
        let synced = stream::unfold((source, updates), |(source, mut updates)| async move {
            loop {
                let message = match updates.recv().await {
                    Ok(mut update) => {
                        // После снимка в очереди могут остаться старые значения. Берём текущую запись.
                        if matches!(update.change, SyncChange::Unread(_)) {
                            let Some(count) = source.unread(&update.context) else {
                                continue;
                            };
                            update.change = SyncChange::Unread(count);
                        }
                        Message::Synced(update)
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        updates = source.sync.subscribe();
                        Message::SyncLagged
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                };
                return Some((message, (source, updates)));
            }
        });
        stream::select(
            stream::select(sessions, synced),
            stream::select(catalog, stream::iter(clicks).flatten()),
        )
    }
}

impl SourceData {
    /// Читает актуальные счётчики для сессий, которые сейчас могут синхронизироваться.
    fn snapshot(&self) -> Vec<AccountSummary> {
        self.auth
            .snapshot()
            .into_iter()
            .map(|session| AccountSummary {
                unread: if session.status == SessionStatus::Active {
                    self.unread(&session.context)
                } else {
                    None
                },
                session,
            })
            .collect()
    }

    /// Читает подтверждённое число. При сбое оставляет его неизвестным и пишет ошибку в лог.
    fn unread(&self, context: &OwnerContext) -> Option<u32> {
        match self.sync.unread(context) {
            Ok(count) => count,
            Err(error) => {
                log::warn!("Не удалось прочитать счётчик мессенджера: {error}");
                None
            }
        }
    }
}

/// Привязывает событие к конкретному входу и проекту, пока эта сессия активна.
fn matches_session(session: &SessionSnapshot, update: &SyncUpdate) -> bool {
    session.status == SessionStatus::Active
        && session.context == update.context
        && session.generation == update.generation
}

/// Возвращает подпись текущего состояния авторизации.
fn session_status(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Restoring => "Восстановление сессии",
        SessionStatus::Active => "Подключён",
        SessionStatus::TemporarilyUnavailable => "Временно недоступен",
        SessionStatus::ReauthenticationRequired => "Требуется вход",
        SessionStatus::SigningOut => "Выход из аккаунта",
    }
}

#[cfg(test)]
mod background_message;

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_core::context::SessionGeneration;

    /// Создаёт активную сессию в одном тестовом проекте.
    fn account(name: &str, generation: u64, unread: u32) -> AccountSummary {
        AccountSummary {
            session: SessionSnapshot {
                context: OwnerContext {
                    account_id: AccountId(name.to_owned()),
                    server: "https://workspace.example.test".to_owned(),
                    organization_id: "organization".to_owned(),
                    project_id: "project".to_owned(),
                    user_id: name.to_owned(),
                },
                generation: SessionGeneration(generation),
                status: SessionStatus::Active,
                token_version: 1,
            },
            unread: Some(unread),
        }
    }

    /// Проверяет обновление фонового аккаунта и запоздалые события от старого входа.
    #[test]
    fn updates_only_the_current_generation_of_the_recipient() {
        let first = account("first", 1, 5);
        let second = account("second", 2, 2);
        let first_id = first.session.context.account_id.clone();
        let second_id = second.session.context.account_id.clone();
        let background = SyncUpdate {
            context: second.session.context.clone(),
            generation: second.session.generation,
            change: SyncChange::Unread(3),
        };
        let mut accounts = Accounts::default();
        accounts.replace(vec![first.clone(), second]);
        accounts.select(first_id.clone());

        assert!(accounts.is_background(&background));
        accounts.set_unread(&background, 3);
        assert_eq!(accounts.selected, Some(first_id));
        assert_eq!(accounts.current().unwrap().unread, Some(5));
        assert_eq!(accounts.summaries[1].unread, Some(3));

        accounts.replace(vec![first, account("second", 3, 8)]);
        assert!(!accounts.is_background(&background));
        accounts.set_unread(&background, 3);
        accounts.select(second_id);
        assert_eq!(accounts.current().unwrap().unread, Some(8));
    }
}
