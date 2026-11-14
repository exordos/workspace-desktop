//! Собирает окно Iced с переключателем аккаунтов и счётчиками.
//! Сначала описаны состояние и вид окна, затем подписка на авторизацию и синхронизацию.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use futures::stream::{self, Stream, StreamExt};
use futures::{channel::mpsc, future::BoxFuture};
use iced::keyboard::{self, Key, key::Named};
use iced::widget::{Column, column, container, operation, row, scrollable, text};
use iced::{Alignment, Element, Event, Fill, Subscription, Task, Theme, event, window};
use tokio::sync::{broadcast, oneshot};
use workspace_core::auth::{Auth, SessionSnapshot, SessionStatus};
use workspace_core::bootstrap::{ChatCatalog, ChatListStatus, ChatLists};
use workspace_core::context::{AccountId, OwnerContext, SessionGeneration};
use workspace_core::messenger::{MessengerSource, history::OpenHistoryError};
use workspace_core::notification_target::{NotificationTarget, NotificationTargetError};
use workspace_core::storage::HistoryWindow;
use workspace_core::sync::{Sync, SyncChange, SyncUpdate};

use crate::platform::notifications::{NotificationClick, NotificationError, Notifications};
use crate::ui_kit::{
    BadgeSize, BadgeTone, ButtonVariant, IconName, IconTone, ThemeMode, button, icon,
    unread_count_badge, workspace_theme,
};

mod history;
use history::{HistoryScreen, HistoryState};

/// Состояние окна, которое Iced меняет в ответ на действия пользователя и события `core`.
pub struct Shell {
    accounts: Accounts,                            // Список аккаунтов и текущий выбор.
    chats: ChatLists, // Каталог каждого владельца и состояние загрузки.
    labels: HashMap<AccountId, String>, // Читаемые имена для известных идентификаторов.
    sources: Sources, // Входные потоки авторизации и мессенджера.
    notifications: Box<dyn NotificationDelivery>, // Получатель запросов оболочки на баннер.
    notification_error: Option<NotificationError>, // Причина, по которой ОС не показала баннер.
    history: Option<HistoryScreen>, // История, которую открыл последний клик.
    navigation_revision: u64, // Номер последнего клика по баннеру или выбора аккаунта.
}

/// Снимок сессии и сохранённое значение счётчика «Все чаты».
#[derive(Clone, Debug)]
pub struct AccountSummary {
    session: SessionSnapshot, // Владелец, состояние и поколение сессии.
    unread: Option<u32>,      // Число с сервера. `None` до загрузки или после завершения сессии.
}

/// События, которые могут изменить окно или вызвать системное действие.
#[derive(Clone, Debug)]
pub enum Message {
    Select(AccountId),                                   // Пользователь выбрал аккаунт.
    Snapshot(Vec<AccountSummary>),                       // Свежий список сессий и счётчиков.
    Chats(ChatLists),                                    // Свежие состояния загрузки каталогов.
    RetryChats,         // Пользователь просит повторить загрузку списка.
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
    ScrollToHistory {
        revision: u64,       // Номер клика, для которого измерены координаты.
        offset: Option<f32>, // Положение цели от начала списка в пикселях.
    },
    MoveFocus(bool), // Перейти назад по Shift+Tab или вперёд по Tab.
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
            Message::Select(account) => {
                self.navigation_revision += 1;
                self.history = None;
                self.accounts.select(account);
            }
            Message::Snapshot(accounts) => self.accounts.replace(accounts),
            Message::Chats(chats) => self.chats = chats,
            Message::RetryChats => {
                if let Some(current) = self.accounts.current()
                    && current.session.status == SessionStatus::Active
                {
                    let session = &current.session;
                    self.sources
                        .0
                        .catalog
                        .retry(session.context.clone(), session.generation);
                }
            }
            Message::Synced(update) => match update.change {
                SyncChange::Unread(count) => self.accounts.set_unread(&update, count),
                SyncChange::MessengerReset => self.accounts.replace(self.sources.0.snapshot()),
                SyncChange::IncomingDirectMessage {
                    ref conversation_id,
                    ref message_id,
                } => {
                    // Авторизация может активировать сессию до того, как её снимок дошёл до окна.
                    self.accounts.replace(self.sources.0.snapshot());
                    return self.notify(&update, conversation_id, message_id);
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
                return self.finish_history(revision, result);
            }
            Message::ScrollToHistory { revision, offset } => {
                if revision == self.navigation_revision
                    && self.visible_history().is_some()
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
            Message::MoveFocus(true) => return operation::focus_previous(),
            Message::MoveFocus(false) => return operation::focus_next(),
        }
        if self.visible_history().is_none() {
            self.history = None;
        }
        Task::none()
    }

    /// Подписывается на все сессии независимо от выбранного аккаунта и обрабатывает Tab.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run_with(self.sources.clone(), Sources::events),
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

    /// Показывает аккаунты, число непрочитанных и текущий экран.
    pub fn view(&self) -> Element<'_, Message> {
        let mut rows = Column::new().spacing(8);
        for summary in &self.accounts.summaries {
            rows = rows.push(self.account_row(summary));
        }
        let current = self.accounts.current().map_or_else(
            || String::from("Ожидание сохранённых сессий"),
            |active| {
                format!(
                    "Открыт аккаунт {} · {}",
                    self.label(&active.session.context.account_id),
                    session_status(active.session.status),
                )
            },
        );
        let mut content = column![text("Аккаунты").size(22), rows, text(current)].spacing(16);
        if let Some(history) = self.visible_history() {
            return container(content.push(history.view(self.navigation_revision)))
                .padding(24)
                .width(Fill)
                .height(Fill)
                .into();
        }
        if let Some(current) = self.accounts.current()
            && current.session.status == SessionStatus::Active
        {
            content = content.push(self.chat_list(&current.session));
        }
        if self.notification_error.is_some() {
            content = content.push(text("Системные уведомления недоступны").size(13));
        }
        scrollable(container(content).padding(24).width(Fill)).into()
    }

    /// Показывает список выбранного входа, его темы и действие после ошибки.
    fn chat_list(&self, session: &SessionSnapshot) -> Element<'_, Message> {
        let mut list = column![text("Чаты").size(18)].spacing(8);
        let state = self
            .chats
            .get(&session.context)
            .filter(|state| state.generation == session.generation);
        match state.map(|state| state.status) {
            Some(ChatListStatus::Failed) => {
                list = list.push(
                    row![
                        text("Не удалось загрузить чаты"),
                        button(
                            "Повторить",
                            ButtonVariant::Neutral,
                            Some(Message::RetryChats)
                        ),
                    ]
                    .spacing(12)
                    .align_y(Alignment::Center),
                );
            }
            Some(ChatListStatus::Ready) => {}
            _ => list = list.push(text("Загрузка чатов…")),
        }
        if let Some(chats) = state.and_then(|state| state.chats.as_deref()) {
            if chats.is_empty() && state.is_some_and(|state| state.status == ChatListStatus::Ready)
            {
                list = list.push(text("У вас пока нет чатов"));
            }
            for chat in chats {
                let symbol = if chat.direct {
                    IconName::Profile
                } else {
                    IconName::ChatBubbleOutline
                };
                list = list.push(
                    row![
                        icon(symbol, 20, IconTone::Inherit, None),
                        text(&chat.name).width(Fill),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                );
                for topic in &chat.topics {
                    list = list.push(
                        container(text(format!("# {}", topic.name)).width(Fill)).padding([0, 30]),
                    );
                }
            }
        }
        list.into()
    }

    /// Рисует кнопку аккаунта и число из последнего подтверждённого снимка.
    fn account_row<'a>(&'a self, summary: &'a AccountSummary) -> Element<'a, Message> {
        let account = &summary.session.context.account_id;
        let variant = if Some(account) == self.accounts.selected.as_ref() {
            ButtonVariant::Primary
        } else {
            ButtonVariant::Neutral
        };
        let mut account_row = row![button(
            self.label(account),
            variant,
            Some(Message::Select(account.clone()))
        )]
        .spacing(12)
        .align_y(Alignment::Center);
        if let Some(count) = summary.unread.filter(|count| *count > 0) {
            account_row = account_row.push(unread_count_badge(
                u64::from(count),
                BadgeTone::Unread,
                BadgeSize::Regular,
            ));
        }
        account_row.into()
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
        self.navigation_revision += 1;
        self.history = None;
        let revision = self.navigation_revision;
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

    /// Применяет ответ core, если пользователь всё ещё ждёт этот переход и сессия активна.
    fn finish_open_notification(
        &mut self,
        revision: u64,
        generation: SessionGeneration,
        result: Result<NotificationTarget, Arc<NotificationTargetError>>,
    ) -> Task<Message> {
        if revision != self.navigation_revision {
            return Task::none();
        }
        let target = match result {
            Ok(target) => target,
            Err(error) => {
                log::warn!("Цель уведомления Workspace недоступна: {error}");
                return Task::none();
            }
        };
        self.accounts.replace(self.sources.0.snapshot());
        if self.accounts.summaries.iter().any(|summary| {
            summary.session.context == target.context
                && summary.session.generation == generation
                && summary.session.status == SessionStatus::Active
        }) {
            self.accounts.select(target.context.account_id.clone());
            self.history = Some(HistoryScreen {
                target: target.clone(),
                generation,
                state: HistoryState::Loading,
            });
            let source = self.sources.0.clone();
            return Task::perform(
                async move {
                    source
                        .messenger
                        .open_direct_history(&source.sync, &target, generation)
                        .await
                        .map(Arc::new)
                        .map_err(Arc::new)
                },
                move |result| Message::HistoryLoaded { revision, result },
            );
        }
        Task::none()
    }

    /// Показывает историю только в том входе, которому принадлежит запрос.
    fn visible_history(&self) -> Option<&HistoryScreen> {
        let history = self.history.as_ref()?;
        let current = &self.accounts.current()?.session;
        (current.context == history.target.context
            && current.generation == history.generation
            && current.status == SessionStatus::Active)
            .then_some(history)
    }

    /// Сохраняет ответ, только если его маршрут и вход всё ещё открыты.
    fn finish_history(
        &mut self,
        revision: u64,
        result: Result<Arc<HistoryWindow>, Arc<OpenHistoryError>>,
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
        match result {
            Ok(window) => {
                history.state = HistoryState::Ready(window);
                history::measure_anchor(revision)
            }
            Err(error) => {
                history.state = HistoryState::Failed(error);
                Task::none()
            }
        }
    }
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
                        Message::Snapshot(source.snapshot())
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
