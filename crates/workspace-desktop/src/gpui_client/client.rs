//! Направляет действия между оболочкой, рабочим потоком, экраном разговора и AppKit.
//! Здесь также собирается основная разметка окна из готовых компонентов.

mod history_positions;
mod notification_banner;

use std::collections::HashMap;

use gpui::{
    Context, Entity, FocusHandle, SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use workspace_core::{
    auth::SessionStatus,
    context::{AccountId, OwnerContext, SessionGeneration},
};

use history_positions::HistoryPositions;
use notification_banner::{NotificationBanner, NotificationBannerActions};

use super::{
    Options,
    avatars::AvatarImages,
    contracts::{
        AccountSession, AccountSnapshot, RuntimeEvent, RuntimeExit, ShellAction, SidebarAction,
    },
    conversation::{Conversation, OpenDetailsTopic},
    next_control,
    platform_window::WindowPlatform,
    previous_control,
    right_panel::{Back, CloseAll, RightPanel},
    runtime::{RuntimeClient, StartupError},
    shell::{Shell, ShellEffect},
    sidebar::{FolderIntent, Sidebar},
    sign_in::{SignIn, SignInAction},
    ui::{
        Palette, TextRole, TopBar, TopBarAccount, TopBarAction, TopBarSection, TopBarSelection,
        label,
    },
};

mod profile;
use profile::Profile;

/// Дополнительный вход в уже открытом окне.
struct AddAccount {
    previous: Vec<AccountSession>, // Ранее видимые поколения, чтобы дождаться нового входа.
    saved: Option<OwnerContext>,   // Подтверждённый Auth владелец, None до сохранения.
    select_on_success: bool, // Можно открыть этот вход, пока пользователь не запросил другой переход.
}

/// Проверяет, что позиции и черновики ещё принадлежат действующему поколению входа.
fn keeps_session(
    accounts: &[AccountSnapshot],
    owner: &OwnerContext,
    generation: SessionGeneration,
) -> bool {
    accounts.iter().any(|account| {
        account.session.owner == *owner
            && account.session.generation == generation
            && matches!(
                account.status,
                SessionStatus::Active | SessionStatus::TemporarilyUnavailable
            )
    })
}

/// Куда вернуть клавиатурный фокус после закрытия текущего экрана.
enum ReturnFocus {
    Workspace,   // Общая точка входа в рабочую область.
    AccountMenu, // Кнопка выбора аккаунта в шапке.
}

const SIDEBAR_PREFERRED_WIDTH: f32 = 299.;
const SIDEBAR_MIN_WIDTH: f32 = 240.;
const SIDEBAR_MAX_WIDTH: f32 = 720.;
const PANEL_GAP: f32 = 4.;

/// Ограничивает ширину каталога долей от окна, оставляя место открытому разговору.
fn sidebar_width(width: gpui::Pixels) -> gpui::Pixels {
    let maximum = (f32::from(width) * 0.4)
        .round()
        .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
    px(SIDEBAR_PREFERRED_WIDTH.min(maximum))
}

/// Корневое состояние одного окна. Долгие операции выполняет RuntimeClient.
pub(super) struct Client {
    section: TopBarSection, // Видимый раздел окна, сохраняющий маршрут чатов при переходе.
    shell: Shell,           // Выбор, адрес разговора и фильтр событий по входам.
    runtime: RuntimeClient, // Очередь команд рабочего потока этого профиля.
    sidebar: Entity<Sidebar>, // Единственный каталог; хранит поиск каждого входа.
    conversation: Option<Entity<Conversation>>, // Экран актуального адреса, если он открыт.
    history_positions: HistoryPositions, // Опоры чтения недавних разговоров этого окна.
    details_navigation: Option<Subscription>, // Подписка на переход из правой панели текущего экрана.
    right_panel: Entity<RightPanel>,          // Общая история правых панелей рабочего окна.
    sign_in: Option<Entity<SignIn>>,          // Открытая форма первого или дополнительного входа.
    add_account: Option<AddAccount>,          // Ход дополнительного входа, пока видна форма.
    profile_requested: Option<AccountSession>, // Нажатие профиля ждёт окно для передачи фокуса.
    profile_avatar: AvatarImages,             // Фото активного входа для верхней панели.
    labels: HashMap<AccountId, String>,       // Подписи для переключателя и баннеров.
    top_bar: Entity<TopBar>, // Шапка хранит локальный фокус и показывает снимок оболочки.
    notification_banner: Entity<NotificationBanner>, // Планка разрешения поверх рабочей области.
    _top_bar_subscriptions: [Subscription; 2], // Выбор аккаунта и открытие экранов из шапки.
    initial_focus: FocusHandle, // Корневая точка обхода до выбора первого элемента.
    restore_focus: Option<ReturnFocus>, // После закрытия формы вернуть фокус в рабочую область.
    ready: bool,             // Рабочий поток открыл профиль и подготовил подписки.
    received_accounts: bool, // Пришёл первый снимок аккаунтов, может быть пустым.
    startup_error: Option<StartupError>, // Безопасная причина сбоя открытия профиля.
    stopped: bool,           // Фоновая работа завершилась с ошибкой.
    _sidebar_subscription: Subscription, // Направляет команды каталога в оболочку.
    runtime_events: Option<Task<()>>, // Подписка живёт столько же, сколько окно.
}

impl Client {
    /// Создаёт оболочку и связывает события каталога с тем же маршрутом, что у баннеров.
    pub(super) fn new(options: Options, runtime: RuntimeClient, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| Sidebar::with_runtime(runtime.clone(), cx));
        cx.observe(&sidebar, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&sidebar, |client, _, intent: &FolderIntent, cx| {
            client.handle_folder_intent(intent.clone(), cx);
        })
        .detach();
        let top_bar = cx.new(TopBar::new);
        let initial_focus = cx.focus_handle();
        let notification_banner = cx.new(|cx| {
            NotificationBanner::new(
                NotificationBannerActions {
                    status: WindowPlatform::notification_permission,
                    request: WindowPlatform::request_notification_permission,
                    open_settings: WindowPlatform::open_notification_settings,
                },
                initial_focus.clone(),
                cx,
            )
        });
        let right_panel = cx.new(|cx| RightPanel::new(initial_focus.clone(), cx));
        cx.observe(&right_panel, |_, _, cx| cx.notify()).detach();
        let selection = cx.subscribe(&top_bar, |client, _, selection: &TopBarSelection, cx| {
            client.open_section(TopBarSection::Chats, cx);
            client.handle_action(ShellAction::SelectAccount(selection.0.clone()), cx);
        });
        let top_bar_action =
            cx.subscribe(
                &top_bar,
                |client, _, action: &TopBarAction, cx| match action {
                    TopBarAction::AddAccount => client.open_add_account(cx),
                    TopBarAction::OpenProfile => {
                        client.profile_requested = client.shell.selected().cloned();
                        cx.notify();
                    }
                    TopBarAction::OpenSection(section) => client.open_section(*section, cx),
                },
            );
        let subscription = cx.subscribe(&sidebar, |client, _, action: &SidebarAction, cx| {
            let action = match action {
                SidebarAction::OpenChat { route, generation } => SidebarAction::OpenChat {
                    route: route.clone(),
                    generation: *generation,
                },
                SidebarAction::RetryCatalog(session) => {
                    SidebarAction::RetryCatalog(session.clone())
                }
            };
            client.handle_action(ShellAction::Sidebar(action), cx);
        });

        Self {
            section: TopBarSection::default(),
            shell: Shell::new(options.active),
            runtime,
            sidebar,
            conversation: None,
            history_positions: HistoryPositions::default(),
            right_panel,
            details_navigation: None,
            sign_in: None,
            add_account: None,
            profile_requested: None,
            profile_avatar: AvatarImages::default(),
            labels: options.labels,
            top_bar,
            notification_banner,
            _top_bar_subscriptions: [selection, top_bar_action],
            initial_focus,
            restore_focus: None,
            ready: false,
            received_accounts: false,
            startup_error: None,
            stopped: false,
            _sidebar_subscription: subscription,
            runtime_events: None,
        }
    }

    /// Оставляет подписку живой и асинхронно показывает результат открытия профиля.
    pub(super) fn attach(&mut self, events: Task<()>, cx: &mut Context<Self>) {
        self.runtime_events = Some(events);
        let runtime = self.runtime.clone();
        cx.spawn(async move |client, cx| {
            let ready = runtime.ready().await;
            let _ = client.update(cx, |client, cx| {
                client.handle_ready(ready, cx);
            });
        })
        .detach();
    }

    /// Сохраняет результат чтения профиля и обновляет маршрут окна.
    fn handle_ready(&mut self, ready: Result<(), StartupError>, cx: &mut Context<Self>) {
        match ready {
            Ok(()) => {
                self.ready = true;
                log::debug!("event=workspace_profile_ready");
            }
            Err(error) => {
                log::warn!("event=workspace_start_failed reason={error:?}");
                self.startup_error = Some(error);
            }
        }
        self.apply(None, cx);
    }

    /// Применяет действие каталога, переключателя или системного окна.
    pub(super) fn handle_action(&mut self, action: ShellAction, cx: &mut Context<Self>) {
        if let Some(add_account) = &mut self.add_account
            && matches!(
                &action,
                ShellAction::SelectAccount(_) | ShellAction::NotificationClicked(_)
            )
        {
            add_account.select_on_success = false;
        }
        let effect = self.shell.handle_action(action);
        self.apply(effect, cx);
    }

    /// Доставляет обновления аккаунтов и истории из подписки рабочего потока.
    pub(super) fn handle_runtime(&mut self, event: RuntimeEvent, cx: &mut Context<Self>) {
        let event = match event {
            RuntimeEvent::Folders(snapshots) => {
                self.shell
                    .handle_runtime(RuntimeEvent::Folders(snapshots.clone()));
                self.sidebar
                    .update(cx, |view, cx| view.set_folder_snapshots(snapshots, cx));
                return;
            }
            RuntimeEvent::FolderPreferences(preferences) => {
                self.sidebar
                    .update(cx, |view, cx| view.set_folder_preferences(preferences, cx));
                return;
            }
            event => event,
        };
        match &event {
            RuntimeEvent::Accounts(accounts) => {
                self.history_positions
                    .keep_sessions(|owner, generation| keeps_session(accounts, owner, generation));
                self.received_accounts = true;
                let active = accounts
                    .iter()
                    .filter(|account| account.status == SessionStatus::Active)
                    .count();
                log::debug!(
                    "event=workspace_accounts_updated count={} active={active}",
                    accounts.len()
                );
            }
            RuntimeEvent::Catalog(catalogs) => {
                let ready = catalogs
                    .values()
                    .filter(|catalog| {
                        catalog.status == workspace_core::bootstrap::ChatListStatus::Ready
                    })
                    .count();
                log::debug!(
                    "event=workspace_catalog_updated count={} ready={ready}",
                    catalogs.len()
                );
            }
            RuntimeEvent::Sync(update) => {
                use workspace_core::sync::{HistoryRevocation, SyncChange};
                match update.change {
                    SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(stream)) => {
                        self.history_positions.revoke(
                            &update.context,
                            update.generation,
                            stream,
                            None,
                        );
                    }
                    SyncChange::HistoryAccessRevoked(HistoryRevocation::Topic {
                        stream_id,
                        topic_id,
                    }) => {
                        self.history_positions.revoke(
                            &update.context,
                            update.generation,
                            stream_id,
                            Some(topic_id),
                        );
                    }
                    _ => {}
                }
            }
            RuntimeEvent::Stopped(RuntimeExit::Failed) => self.stopped = true,
            _ => {}
        }
        let effect = self.shell.handle_runtime(event);
        self.apply(effect, cx);
    }

    /// Передаёт запрос папок рабочему потоку и возвращает результат исходному показу.
    fn handle_folder_intent(&mut self, intent: FolderIntent, cx: &mut Context<Self>) {
        let runtime = self.runtime.clone();
        let sidebar = self.sidebar.downgrade();
        match intent {
            FolderIntent::Command {
                session,
                operation,
                command,
            } => {
                if self.shell.selected() != Some(&session) || self.section != TopBarSection::Chats {
                    return;
                }
                cx.spawn(async move |_, cx| {
                    log::debug!("op_id={operation} event=folder_ui_requested");
                    let result = runtime.folder_command(session.clone(), command).await;
                    log::debug!("op_id={operation} event=folder_ui_finished result={result:?}");
                    let _ = sidebar.update(cx, |view, cx| {
                        view.finish_folder_command(&session, operation, result, cx);
                    });
                })
                .detach();
            }
            FolderIntent::Preferences { session, value } => {
                if self.shell.selected() != Some(&session) {
                    return;
                }
                cx.spawn(async move |_, cx| {
                    if runtime
                        .save_folder_preferences(session.clone(), value)
                        .await
                        .is_err()
                    {
                        let _ = sidebar.update(cx, |view, cx| {
                            view.folder_preferences_failed(&session, value, cx);
                        });
                    }
                })
                .detach();
            }
        }
    }

    /// Выполняет команду оболочки и обновляет связанные части того же окна.
    fn apply(&mut self, effect: Option<ShellEffect>, cx: &mut Context<Self>) {
        let effect = self.reconcile_sign_in(cx).or(effect);
        self.sync_right_panel_context(cx);
        match effect {
            Some(ShellEffect::OpenConversation(address)) => {
                self.save_history_position(cx);
                self.release_old_conversation_avatars(cx);
                self.open_section(TopBarSection::Chats, cx);
                let runtime = self.runtime.clone();
                let panel = self.right_panel.clone();
                let restore = self.history_positions.take(&address);
                let conversation =
                    cx.new(|cx| Conversation::open(address, runtime, panel, restore, cx));
                self.details_navigation = Some(cx.subscribe(
                    &conversation,
                    |client, _, target: &OpenDetailsTopic, cx| {
                        client.open_details_topic(target, cx)
                    },
                ));
                self.conversation = Some(conversation);
            }
            Some(ShellEffect::CloseConversation) => {
                self.save_history_position(cx);
                self.release_old_conversation_avatars(cx);
                self.details_navigation = None;
                self.conversation = None;
            }
            Some(ShellEffect::RetryCatalog(session)) => {
                let runtime = self.runtime.clone();
                cx.spawn(async move |_, _| {
                    let _ = runtime.retry_catalog(session).await;
                })
                .detach();
            }
            Some(ShellEffect::ResolveNotification(request)) => {
                let runtime = self.runtime.clone();
                cx.spawn(async move |client, cx| {
                    let reply = runtime.resolve_notification(request).await;
                    let _ = client.update(cx, |client, cx| {
                        let effect = client.shell.handle_notification_reply(reply);
                        client.apply(effect, cx);
                    });
                })
                .detach();
            }
            Some(ShellEffect::ShowNotification(click)) => {
                let label = self.labels.get(&click.target.context.account_id);
                WindowPlatform::show_notification(cx, click, label.map(String::as_str));
            }
            Some(ShellEffect::ReloadConversation(address)) => {
                if let Some(conversation) = &self.conversation {
                    conversation.update(cx, |view, cx| view.reload(&address, cx));
                }
            }
            Some(ShellEffect::ConversationUpdated(address, update)) => {
                if let Some(conversation) = &self.conversation {
                    conversation.update(cx, |view, cx| view.apply_sync(&address, update, cx));
                }
            }
            None => {}
        }
        let accounts = self.top_bar_accounts();
        self.top_bar
            .update(cx, |view, cx| view.set_accounts(accounts, cx));
        let catalog = self.shell.selected_catalog();
        let conversation = self.shell.conversation();
        if let Some(view) = &self.conversation {
            view.update(cx, |view, cx| {
                view.set_catalog(self.shell.selected(), catalog, cx)
            });
        }
        self.sync_conversation_avatar_visibility(cx);
        self.sidebar.update(cx, |view, cx| {
            view.update_snapshot(self.shell.selected(), catalog, conversation, cx);
            view.set_folder_online(
                self.shell.selected().is_some_and(|session| {
                    self.shell.accounts().iter().any(|account| {
                        &account.session == session && account.status == SessionStatus::Active
                    })
                }),
                cx,
            );
        });
        cx.notify();
    }

    /// Начинает дополнительный вход через тот же RuntimeClient, сохраняя маршрут мессенджера.
    fn open_add_account(&mut self, cx: &mut Context<Self>) {
        if self.sign_in.is_some() || self.status().is_some() {
            return;
        }
        self.sidebar.update(cx, |view, cx| view.on_hidden(cx));
        let form = cx.new(|cx| SignIn::add_account(self.runtime.clone(), cx));
        cx.subscribe(&form, |client, _, action: &SignInAction, cx| {
            match action {
                SignInAction::Cancel => {
                    client.add_account = None;
                    client.sign_in = None;
                    client.restore_focus = Some(ReturnFocus::AccountMenu);
                    log::debug!("event=workspace_add_account_cancelled");
                }
                SignInAction::Saved(owner) => {
                    if let Some(add_account) = &mut client.add_account {
                        add_account.saved = Some(owner.clone());
                    }
                }
            }
            client.apply(None, cx);
        })
        .detach();
        self.add_account = Some(AddAccount {
            previous: self
                .shell
                .accounts()
                .iter()
                .map(|account| account.session.clone())
                .collect(),
            saved: None,
            select_on_success: true,
        });
        self.sign_in = Some(form);
        self.sync_conversation_avatar_visibility(cx);
        log::debug!("event=workspace_add_account_opened");
        cx.notify();
    }

    /// Меняет показанный раздел, сохраняя открытый разговор до возврата в мессенджер.
    fn open_section(&mut self, section: TopBarSection, cx: &mut Context<Self>) {
        if self.section != section && self.status().is_none() {
            self.section = section;
            if section != TopBarSection::Chats {
                self.sidebar.update(cx, |view, cx| view.on_hidden(cx));
            }
            self.sync_conversation_avatar_visibility(cx);
            self.sync_right_panel_context(cx);
            self.top_bar
                .update(cx, |bar, cx| bar.set_section(section, cx));
            log::debug!("event=workspace_section_opened section={}", section.id());
            cx.notify();
        }
    }

    /// Выключает подписки на фото при уходе из рабочей области, сохраняя состояние разговора.
    fn sync_conversation_avatar_visibility(&self, cx: &mut Context<Self>) {
        if let Some(conversation) = &self.conversation {
            let visible = self.section == TopBarSection::Chats
                && self.sign_in.is_none()
                && self.status().is_none();
            let address = self.shell.conversation();
            conversation.update(cx, |view, cx| {
                view.set_avatars_visible(visible && address == Some(view.address()), cx);
            });
        }
    }

    /// Запоминает опору старого экрана перед тем, как заменить его новым адресом.
    fn save_history_position(&mut self, cx: &Context<Self>) {
        let Some(conversation) = &self.conversation else {
            return;
        };
        let view = conversation.read(cx);
        let address = view.address();
        if keeps_session(
            self.shell.accounts(),
            &address.route.owner,
            address.generation,
        ) && let Some(anchor) = view.reading_position()
        {
            self.history_positions.save(address, anchor);
        }
    }

    /// Снимает подписки со старого разговора до замены его маршрута или входа.
    fn release_old_conversation_avatars(&self, cx: &mut Context<Self>) {
        if let Some(conversation) = &self.conversation {
            conversation.update(cx, |view, cx| view.set_avatars_visible(false, cx));
        }
    }

    /// Привязывает стек к входу и открытому разговору в видимом разделе.
    fn sync_right_panel_context(&self, cx: &mut Context<Self>) {
        let account = self
            .status()
            .is_none()
            .then(|| self.shell.selected())
            .flatten();
        let conversation = (self.section == TopBarSection::Chats)
            .then(|| self.shell.conversation())
            .flatten();
        self.right_panel.update(cx, |panel, cx| {
            panel.set_context(account, conversation, cx);
        });
    }

    /// Передаёт запрос профиля общему стеку с фокусом кнопки в шапке.
    fn open_requested_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.profile_requested.take()
            && self.shell.selected() == Some(&session)
            && self.sign_in.is_none()
            && self.status().is_none()
        {
            let opener = self.top_bar.read(cx).profile_focus();
            self.right_panel.update(cx, |panel, cx| {
                Profile::toggle(panel, session, opener, &self.runtime, window, cx);
            });
        }
    }

    /// Передаёт шапке изображение активного входа из общей подписки аватара.
    fn sync_profile_avatar(&mut self, cx: &mut Context<Self>) {
        let active = self
            .shell
            .selected()
            .filter(|_| self.sign_in.is_none() && self.status().is_none());
        let avatar = if let Some((session, id)) =
            active.and_then(|session| Some((session, session.user_uuid()?)))
        {
            self.profile_avatar
                .refresh([id], &self.runtime, session, cx);
            match self.profile_avatar.image(id, cx) {
                Some(gpui::ImageSource::Render(image)) => Some((session.clone(), image)),
                _ => None,
            }
        } else {
            self.profile_avatar.clear();
            None
        };
        self.top_bar
            .update(cx, |bar, cx| bar.set_profile_avatar(avatar, cx));
    }

    /// Переключает тему из открытой панели и оставляет панель на новом адресе.
    fn open_details_topic(&mut self, target: &OpenDetailsTopic, cx: &mut Context<Self>) {
        if let Some(effect) = self
            .shell
            .open_details_topic(&target.address, target.topic_id)
        {
            self.apply(Some(effect), cx);
            if let Some(conversation) = &self.conversation {
                conversation.update(cx, Conversation::restore_open_details);
            }
        }
    }

    /// Показывает вход после чтения профиля и передаёт окно первой готовой сессии после входа.
    fn reconcile_sign_in(&mut self, cx: &mut Context<Self>) -> Option<ShellEffect> {
        let accounts = self.shell.accounts();
        if let Some(add_account) = &self.add_account {
            if self.status().is_none() {
                let owner = add_account.saved.as_ref()?;
                let session = accounts
                    .iter()
                    .find(|account| {
                        account.status == SessionStatus::Active
                            && account.session.owner == *owner
                            && !add_account.previous.contains(&account.session)
                    })?
                    .session
                    .clone();
                let select = add_account.select_on_success;
                self.add_account = None;
                self.sign_in = None;
                self.restore_focus = Some(ReturnFocus::AccountMenu);
                log::debug!("event=workspace_add_account_finished select={select}");
                return if select {
                    self.open_section(TopBarSection::Chats, cx);
                    self.shell
                        .handle_action(ShellAction::SelectAccount(session))
                } else {
                    None
                };
            }
            self.add_account = None;
            self.sign_in = None;
            self.restore_focus = Some(ReturnFocus::Workspace);
        }
        let show = self.status().is_none()
            && accounts
                .iter()
                .all(|account| account.status == SessionStatus::ReauthenticationRequired);
        if show {
            if self.sign_in.is_none() {
                let server = match accounts {
                    [account] => Some(account.session.owner.server.as_str()),
                    _ => None,
                };
                self.sign_in = Some(cx.new(|cx| SignIn::new(self.runtime.clone(), server, cx)));
            }
            return None;
        }

        self.sign_in.take()?;
        self.restore_focus = Some(ReturnFocus::Workspace);
        let session = accounts
            .iter()
            .find(|account| account.status == SessionStatus::Active)?
            .session
            .clone();
        self.shell
            .handle_action(ShellAction::SelectAccount(session))
    }

    /// Подготавливает подписи и счётчики с учётом выбранного оболочкой входа.
    fn top_bar_accounts(&self) -> Vec<TopBarAccount> {
        self.shell
            .accounts()
            .iter()
            .enumerate()
            .map(|(index, account)| {
                let session = &account.session;
                let display_name = self
                    .labels
                    .get(&session.owner.account_id)
                    .cloned()
                    .unwrap_or_else(|| format!("Аккаунт {}", index + 1));
                let label = match account.status {
                    SessionStatus::Active => display_name,
                    SessionStatus::Restoring => format!("{display_name} · Вход…"),
                    SessionStatus::TemporarilyUnavailable => format!("{display_name} · Нет связи"),
                    _ => format!("{display_name} · Требуется вход"),
                };
                let server = session.owner.server.trim_start_matches("https://");
                TopBarAccount {
                    session: session.clone(),
                    label: label.into(),
                    detail: format!("{server} · {}", session.owner.project_id).into(),
                    selected: self.shell.selected() == Some(session),
                    unread: account.unread,
                }
            })
            .collect()
    }

    /// Сообщает о начальном состоянии или ошибке только безопасным текстом.
    fn status(&self) -> Option<SharedString> {
        if let Some(error) = self.startup_error {
            return Some(error.to_string().into());
        }
        if self.stopped {
            return Some("Фоновая работа остановлена".into());
        }
        if !self.ready || !self.received_accounts {
            return Some("Загружаем аккаунты…".into());
        }
        None
    }

    /// Устанавливает фокус внутри окна, чтобы Tab работал до первого клика.
    fn focus_workspace(&mut self, window: &mut Window, cx: &Context<Self>) {
        match self.restore_focus.take() {
            Some(ReturnFocus::AccountMenu) => self.top_bar.read(cx).focus_account_menu(window),
            Some(_) => window.focus(&self.initial_focus),
            None if window.focused(cx).is_none() => window.focus(&self.initial_focus),
            None => {}
        }
    }
}

impl Render for Client {
    /// Показывает форму входа или рабочую область с выбранным аккаунтом.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.focus_workspace(window, cx);
        self.sync_profile_avatar(cx);
        RightPanel::restore_focus(&self.right_panel, window, cx);
        self.open_requested_profile(window, cx);
        let palette = Palette::for_window(window, cx);
        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .child(super::ui::track_focus_input(cx))
            .bg(palette.background)
            .text_color(palette.text)
            .track_focus(&self.initial_focus)
            .on_action(next_control)
            .on_action(previous_control)
            .on_action(cx.listener(|client, _: &Back, window, cx| {
                client
                    .right_panel
                    .update(cx, |panel, cx| panel.back(window, cx));
            }))
            .on_action(cx.listener(|client, _: &CloseAll, window, cx| {
                client
                    .right_panel
                    .update(cx, |panel, cx| panel.close_all(window, cx));
            }));
        if let Some(sign_in) = &self.sign_in {
            return root.child(div().flex().flex_1().min_h_0().child(sign_in.clone()));
        }

        let route_error = self.shell.route_error();
        let status = self.status();
        let show_notifications = status.is_none() && self.shell.selected().is_some();
        let content = if let Some(status) = status {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .child(label(palette, TextRole::Label, status))
                .into_any_element()
        } else if self.section != TopBarSection::Chats {
            let section = self.section;
            div()
                .relative()
                .flex()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(
                    label(palette, TextRole::Heading, section.title())
                        .id("workspace-section")
                        .debug_selector(move || format!("workspace-section-{}", section.id()))
                        .flex_1()
                        .min_w_0()
                        .p_4(),
                )
                .children(RightPanel::frame(&self.right_panel, palette, window, cx))
                .into_any_element()
        } else {
            let empty = div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .child(label(palette, TextRole::Body, "Выберите разговор"));
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .gap(px(PANEL_GAP))
                .child(
                    div()
                        .id("workspace-sidebar-and-folders")
                        .w(sidebar_width(window.viewport_size().width)
                            + px(self.sidebar.read(cx).folder_rail_width()))
                        .h_full()
                        .flex_none()
                        .overflow_hidden()
                        .child(self.sidebar.clone()),
                )
                .child(
                    div()
                        .id("workspace-conversation-panel")
                        .debug_selector(|| "workspace-conversation-panel".into())
                        .relative()
                        .flex()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .overflow_hidden()
                        .map(|view| match &self.conversation {
                            Some(conversation) => view.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .child(conversation.clone()),
                            ),
                            None => view.child(empty.flex_1().min_w_0()),
                        })
                        .children(RightPanel::frame(&self.right_panel, palette, window, cx)),
                )
                .into_any_element()
        };

        root.child(self.top_bar.clone())
            .children(route_error.map(|message| {
                label(palette, TextRole::Body, message)
                    .p_2()
                    .text_color(palette.danger)
            }))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .pt(px(PANEL_GAP))
                    .child(content),
            )
            .when(show_notifications, |root| {
                root.child(gpui::deferred(self.notification_banner.clone()).with_priority(3))
            })
            .child(self.sidebar.read(cx).folder_dialog())
    }
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests {
    use std::sync::Arc;

    use gpui::{Modifiers, TestAppContext, px, size};
    use uuid::Uuid;
    use workspace_core::{
        bootstrap::{ChatList, ChatListStatus},
        context::{OwnerContext, SessionGeneration},
        messenger::catalog::Chat,
    };

    use super::*;
    use crate::gpui_client::{
        bind_keys,
        contracts::{AccountSession, AccountSnapshot},
    };

    /// Готовит один временный вход, который показывается в новом окне.
    fn account(name: &str) -> AccountSnapshot {
        AccountSnapshot {
            session: AccountSession {
                owner: OwnerContext {
                    account_id: AccountId(name.to_owned()),
                    server: "https://workspace.test".to_owned(),
                    organization_id: "org".to_owned(),
                    project_id: "project".to_owned(),
                    user_id: Uuid::from_u128(1).to_string(),
                },
                generation: SessionGeneration(1),
            },
            status: SessionStatus::Active,
            unread: None,
        }
    }

    /// Подменяет ответы системы для проверки планки в рабочей оболочке.
    struct MockNotificationPermission {
        status: crate::platform::notifications::NotificationPermission, // Текущий ответ macOS.
        requesting: bool,               // Ждём ответа в системном диалоге.
        requests: u32,                  // Сколько намерений дошло от кнопки до адаптера.
        settings: std::cell::Cell<u32>, // Сколько раз пользователь открывал настройки.
    }

    impl gpui::Global for MockNotificationPermission {}

    /// Проверяет сворачивание, предпросмотр, клавиатуру и действия для разных статусов.
    #[gpui::test]
    fn notification_permission_banner_overlays_the_chat_and_waits_for_explicit_action(
        cx: &mut TestAppContext,
    ) {
        use crate::platform::notifications::NotificationPermission;
        use std::time::Duration;

        cx.update(|app| {
            bind_keys(app);
            app.set_global(MockNotificationPermission {
                status: NotificationPermission::NeedsPermission,
                requesting: false,
                requests: 0,
                settings: Default::default(),
            });
        });
        let clock = cx.executor();
        let profile = tempfile::tempdir().unwrap();
        let (runtime, _events) = RuntimeClient::start_in(profile.path().into()).unwrap();
        let saved = account("A");
        let (client, ui) = cx.add_window_view(|_, cx| {
            let mut client = Client::new(Options::default(), runtime.clone(), cx);
            let focus = client.initial_focus.clone();
            client.notification_banner = cx.new(|cx| {
                NotificationBanner::new(
                    NotificationBannerActions {
                        status: |app| {
                            let state = app.global::<MockNotificationPermission>();
                            (Some(state.status), state.requesting)
                        },
                        request: |app| {
                            let state = app.global_mut::<MockNotificationPermission>();
                            state.requests += 1;
                            state.requesting = true;
                            app.refresh_windows();
                        },
                        open_settings: |app| {
                            let counter = &app.global::<MockNotificationPermission>().settings;
                            counter.set(counter.get() + 1);
                            false
                        },
                    },
                    focus,
                    cx,
                )
            });
            client.ready = true;
            client.handle_runtime(RuntimeEvent::Accounts(vec![saved]), cx);
            client
        });
        ui.simulate_resize(size(px(700.), px(600.)));
        let finish_slide = |ui: &mut gpui::VisualTestContext| {
            ui.run_until_parked();
            clock.advance_clock(Duration::from_millis(175));
            ui.update(|window, _| window.refresh());
            ui.run_until_parked();
        };
        let banner = "notification-permission-banner";
        let strip = "notification-permission-strip";
        let primary = "notification-permission-primary";
        finish_slide(ui);
        let sidebar = ui.debug_bounds("workspace-sidebar-panel").unwrap();
        let expanded = ui.debug_bounds(banner).unwrap();
        let top_bar = ui.debug_bounds("workspace-top-bar").unwrap();
        assert!(expanded.top() <= top_bar.top());
        let later = ui.debug_bounds("notification-permission-later").unwrap();
        assert_eq!(
            ui.read(|app| app.global::<MockNotificationPermission>().requests),
            0
        );

        ui.simulate_mouse_move(later.center(), None, Modifiers::none());
        ui.simulate_click(later.center(), Modifiers::none());
        finish_slide(ui);
        let collapsed = ui.debug_bounds(banner).unwrap();
        assert!(collapsed.size.height < expanded.size.height);
        assert_eq!(ui.debug_bounds("workspace-sidebar-panel"), Some(sidebar));
        let outside = gpui::point(sidebar.center().x, sidebar.bottom() - px(24.));
        ui.simulate_mouse_move(outside, None, Modifiers::none());
        let strip_bounds = ui.debug_bounds(strip).unwrap();
        ui.simulate_mouse_move(strip_bounds.center(), None, Modifiers::none());
        finish_slide(ui);
        ui.simulate_mouse_move(later.center(), None, Modifiers::none());
        assert_eq!(ui.debug_bounds(banner), Some(expanded));
        ui.simulate_mouse_move(outside, None, Modifiers::none());
        finish_slide(ui);
        assert_eq!(ui.debug_bounds(banner), Some(collapsed));
        assert_eq!(
            ui.read(|app| app.global::<MockNotificationPermission>().requests),
            0
        );

        press_key(ui, "enter");
        finish_slide(ui);
        assert_eq!(ui.debug_bounds(banner), Some(expanded));
        let button = ui.debug_bounds(primary).unwrap();
        ui.simulate_click(button.center(), Modifiers::none());
        ui.run_until_parked();
        ui.simulate_click(button.center(), Modifiers::none());
        assert_eq!(
            ui.read(|app| app.global::<MockNotificationPermission>().requests),
            1
        );

        ui.update(|_, app| {
            let state = app.global_mut::<MockNotificationPermission>();
            state.status = NotificationPermission::NeedsSettings;
            state.requesting = false;
            app.refresh_windows();
        });
        finish_slide(ui);
        let button = ui.debug_bounds(primary).unwrap();
        ui.simulate_click(button.center(), Modifiers::none());
        assert_eq!(
            ui.read(|app| app.global::<MockNotificationPermission>().settings.get()),
            1
        );
        ui.update(|_, app| {
            app.global_mut::<MockNotificationPermission>().status = NotificationPermission::Enabled;
            app.refresh_windows();
        });
        finish_slide(ui);
        assert_eq!(ui.debug_bounds("workspace-sidebar-panel"), Some(sidebar));
        assert!(ui.update(|window, app| client.read(app).initial_focus.is_focused(window)));
        // GPUI сохраняет последние debug_bounds даже после удаления элемента. Проверяем ввод.
        ui.simulate_click(button.center(), Modifiers::none());
        assert_eq!(
            ui.read(|app| app.global::<MockNotificationPermission>().settings.get()),
            1
        );
        assert_eq!(
            futures::executor::block_on(runtime.shutdown()),
            RuntimeExit::Clean,
        );
    }

    /// Добавляет второй аккаунт из меню и проверяет отмену и сохранение обеих сессий.
    #[gpui::test]
    fn account_menu_adds_another_login_and_keeps_the_first(cx: &mut TestAppContext) {
        use crate::platform::JsonAccountStore;
        use workspace_core::auth::AccountStore;

        let wait_for = |ui: &mut gpui::VisualTestContext, selector: &'static str| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                ui.run_until_parked();
                if ui.debug_bounds(selector).is_some() {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "Пункт не виден: {selector}"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };

        let directory = tempfile::tempdir().unwrap();
        let (runtime, events) = RuntimeClient::start_sign_in_for_test(directory.path().into());
        let first = futures::executor::block_on(async {
            runtime.ready().await.unwrap();
            let attempt = runtime
                .begin_login(
                    "https://127.0.0.1:1".into(),
                    "login-test-user".into(),
                    "test-password".into(),
                    Some("123456".into()),
                )
                .await
                .unwrap();
            runtime
                .complete_login(attempt.id(), "test-project".into())
                .await
                .unwrap()
        });

        cx.update(bind_keys);
        let (client, ui) =
            cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
        ui.simulate_resize(size(px(700.), px(600.)));
        ui.update(|_, app| {
            let subscription = events.attach(&client, app, Client::handle_runtime);
            client.update(app, |client, cx| client.attach(subscription, cx));
        });
        wait_for(ui, "workspace-account-menu-trigger");
        let trigger = ui.debug_bounds("workspace-account-menu-trigger").unwrap();
        ui.simulate_click(trigger.center(), Modifiers::none());
        wait_for(ui, "workspace-add-account");
        let row = ui.debug_bounds("workspace-add-account").unwrap();
        ui.simulate_click(row.center(), Modifiers::none());
        wait_for(ui, "sign-in-back");
        ui.simulate_keystrokes("escape");
        ui.run_until_parked();

        // После отмены клавиатура снова открывает меню, а End выбирает действие под аккаунтами.
        assert!(ui.read(|app| client.read(app).sign_in.is_none()));
        assert_eq!(
            ui.read(|app| client.read(app).shell.selected().unwrap().owner.clone()),
            first
        );
        press_key(ui, "enter");
        ui.simulate_keystrokes("end");
        press_key(ui, "enter");
        assert!(ui.read(|app| client.read(app).sign_in.is_some()));
        ui.simulate_input("https://127.0.0.1:1");
        ui.simulate_keystrokes("enter");
        wait_for(ui, "sign-in-login");
        ui.simulate_input("second-login-test-user");
        ui.simulate_keystrokes("tab");
        ui.simulate_input("test-password");
        ui.simulate_keystrokes("enter");
        wait_for(ui, "sign-in-otp");
        ui.simulate_input("123456");
        ui.simulate_keystrokes("enter");
        wait_for(ui, "sign-in-project-1");
        let submit = ui.debug_bounds("sign-in-submit").unwrap();
        ui.simulate_click(submit.center(), Modifiers::none());
        wait_for(ui, "workspace-account-1");
        assert!(ui.read(|app| client.read(app).sign_in.is_none()));
        assert_eq!(
            ui.read(|app| client
                .read(app)
                .shell
                .selected()
                .unwrap()
                .owner
                .user_id
                .clone()),
            "second-login-test-user",
        );

        assert_eq!(
            futures::executor::block_on(runtime.shutdown()),
            RuntimeExit::Clean,
        );
        let saved = JsonAccountStore::open(directory.path())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(saved.len(), 2);
        assert!(saved.iter().any(|account| account.context == first));
        assert!(
            saved
                .iter()
                .any(|account| account.context.user_id == "second-login-test-user")
        );
    }

    /// Открывает отдельные страницы из профиля и возвращает фокус в шапку.
    #[gpui::test]
    fn profile_opens_personal_info_and_settings_pages(cx: &mut TestAppContext) {
        cx.update(bind_keys);
        let directory = tempfile::tempdir().unwrap();
        let (runtime, _events) = RuntimeClient::start_in(directory.path().into()).unwrap();
        let (client, ui) =
            cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
        ui.simulate_resize(size(px(700.), px(640.)));
        ui.update(|_, app| {
            client.update(app, |view, cx| {
                view.handle_ready(Ok(()), cx);
                view.handle_runtime(RuntimeEvent::Accounts(vec![account("A")]), cx);
            });
        });
        ui.run_until_parked();
        let click = |ui: &mut gpui::VisualTestContext, id: &'static str| {
            let item = ui
                .debug_bounds(id)
                .unwrap_or_else(|| panic!("Пункт не виден: {id}"));
            ui.simulate_click(item.center(), Modifiers::none());
            ui.run_until_parked();
        };
        let root_profile = ui.read(|app| {
            let session = client.read(app).shell.selected().unwrap().clone();
            crate::gpui_client::right_panel::PanelKey {
                scope: crate::gpui_client::right_panel::Scope::Account(session),
                id: "profile-root".into(),
            }
        });
        click(ui, "workspace-profile-trigger");
        assert!(ui.debug_bounds("profile-root-page").is_some());
        click(ui, "profile-personal-info");
        assert!(ui.debug_bounds("profile-personal-info-page").is_some());
        click(ui, "profile-back");
        click(ui, "profile-settings");
        assert!(ui.debug_bounds("profile-settings-page").is_some());
        click(ui, "profile-language");
        assert!(ui.debug_bounds("profile-language-page").is_some());
        press_key(ui, "escape");
        click(ui, "profile-hotkeys");
        assert!(ui.debug_bounds("profile-hotkeys-page").is_some());
        click(ui, "right-panel-close-all");
        let active_profile = |ui: &mut gpui::VisualTestContext| {
            ui.read(|app| {
                let panel = client.read(app).right_panel.read(app);
                panel.is_active(&root_profile)
            })
        };
        assert!(!active_profile(ui));
        press_key(ui, "enter");
        assert!(active_profile(ui));

        ui.update(|_, app| {
            client.update(app, |view, cx| {
                view.handle_runtime(RuntimeEvent::Accounts(vec![account("B")]), cx);
            });
        });
        ui.run_until_parked();
        assert!(!active_profile(ui));
        assert!(ui.update(|window, app| client.read(app).initial_focus.is_focused(window)));
        assert_eq!(
            futures::executor::block_on(runtime.shutdown()),
            RuntimeExit::Clean
        );
    }

    /// Проверяет, что форма ждёт готовности профиля и отдаёт окно активному входу.
    #[gpui::test]
    fn sign_in_routes_only_after_profile_ready_and_session_changes(cx: &mut TestAppContext) {
        cx.update(bind_keys);
        let profile = tempfile::tempdir().unwrap();
        let (runtime, _events) = RuntimeClient::start_in(profile.path().into()).unwrap();
        let mut saved = account("Сохранённый аккаунт");
        let signed_in = account("Новый аккаунт");
        let (client, ui) =
            cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
        ui.simulate_resize(size(px(700.), px(600.)));
        let send_accounts = |accounts, ui: &mut gpui::VisualTestContext| {
            ui.update(|_, app| {
                client.update(app, |view, cx| {
                    view.handle_runtime(RuntimeEvent::Accounts(accounts), cx);
                });
            });
            ui.run_until_parked();
        };

        send_accounts(vec![], ui);
        assert!(ui.debug_bounds("workspace-sign-in").is_none());
        ui.update(|_, app| {
            client.update(app, |view, cx| view.handle_ready(Ok(()), cx));
        });
        ui.run_until_parked();
        assert!(ui.debug_bounds("workspace-sign-in").is_some());
        let has_form =
            |ui: &mut gpui::VisualTestContext| ui.read(|app| client.read(app).sign_in.is_some());

        saved.status = SessionStatus::Restoring;
        send_accounts(vec![saved.clone()], ui);
        assert!(!has_form(ui));
        saved.status = SessionStatus::TemporarilyUnavailable;
        send_accounts(vec![saved.clone()], ui);
        assert!(!has_form(ui));

        saved.status = SessionStatus::ReauthenticationRequired;
        send_accounts(vec![saved.clone()], ui);
        assert!(has_form(ui));

        send_accounts(vec![saved.clone(), signed_in.clone()], ui);
        assert!(!has_form(ui));
        assert!(ui.debug_bounds("workspace-top-bar").is_some());
        assert!(ui.debug_bounds("workspace-sidebar-panel").is_some());
        assert!(ui.update(|window, app| client.read(app).initial_focus.is_focused(window)));
        assert_eq!(
            ui.read(|app| client.read(app).shell.selected().cloned()),
            Some(signed_in.session),
        );

        send_accounts(vec![], ui);
        assert!(has_form(ui));
        assert!(ui.debug_bounds("sign-in-server").is_some());
        // Ввод сразу попадает в адрес, без клика или перезапуска окна.
        ui.simulate_input("https://127.0.0.1:1");
        ui.simulate_keystrokes("enter");
        ui.run_until_parked();
        assert!(ui.debug_bounds("sign-in-login").is_some());
        assert_eq!(
            futures::executor::block_on(runtime.shutdown()),
            RuntimeExit::Clean,
        );
    }

    /// Вход меняется от кнопки корня, а каталог того же окна следует за новым владельцем.
    #[gpui::test]
    fn account_control_replaces_sidebar_content(cx: &mut TestAppContext) {
        cx.update(bind_keys);
        let profile = tempfile::tempdir().unwrap();
        let (runtime, _events) = RuntimeClient::start_in(profile.path().into()).unwrap();
        let a = account("A");
        let b = account("B");
        let (client, ui) = cx.add_window_view(|_, cx| {
            Client::new(
                Options {
                    active: Some(a.session.owner.account_id.clone()),
                    labels: HashMap::new(),
                },
                runtime.clone(),
                cx,
            )
        });
        ui.simulate_resize(size(px(640.), px(500.)));
        ui.update(|_, app| {
            client.update(app, |view, cx| {
                view.ready = true;
                view.handle_runtime(RuntimeEvent::Accounts(vec![a.clone(), b.clone()]), cx);
                let catalogs = [(&a, 11), (&b, 12)]
                    .into_iter()
                    .map(|(account, row)| {
                        let chats = Arc::from([Chat {
                            id: Uuid::from_u128(row),
                            name: "Тестовый чат".to_owned(),
                            direct: true,
                            topics: vec![],
                            metadata: Default::default(),
                        }]);
                        (
                            account.session.owner.clone(),
                            ChatList {
                                generation: account.session.generation,
                                chats: Some(chats),
                                status: ChatListStatus::Ready,
                            },
                        )
                    })
                    .collect();
                view.handle_runtime(RuntimeEvent::Catalog(catalogs), cx);
            });
        });
        ui.run_until_parked();
        let a_row = "catalog-chat-00000000-0000-0000-0000-00000000000b";
        let b_row = "catalog-chat-00000000-0000-0000-0000-00000000000c";
        assert!(ui.debug_bounds(a_row).is_some());
        assert!(ui.debug_bounds(b_row).is_none());

        let b_button = ui.debug_bounds("workspace-account-1").unwrap();
        ui.simulate_click(b_button.center(), Modifiers::none());
        ui.run_until_parked();
        assert!(
            ui.read(|app| client.read(app).shell.selected() == Some(&b.session)),
            "Кнопка B должна переключить вход (bounds {b_button:?})"
        );
        assert!(ui.debug_bounds(b_row).is_some());
        assert_eq!(
            ui.read(|app| client
                .read(app)
                .shell
                .selected_catalog()
                .and_then(|catalog| catalog.chats.as_deref())
                .map(|rows| rows[0].id)),
            Some(Uuid::from_u128(12))
        );
        let chat = ui.debug_bounds(b_row).unwrap();
        ui.simulate_click(chat.center(), Modifiers::none());
        ui.run_until_parked();
        let conversation = ui
            .read(|app| client.read(app).conversation.clone())
            .unwrap();
        for (section, button_id, page_id) in [
            (
                TopBarSection::Calendar,
                "workspace-top-bar-calendar",
                "workspace-section-calendar",
            ),
            (
                TopBarSection::Mail,
                "workspace-top-bar-mail",
                "workspace-section-mail",
            ),
            (
                TopBarSection::Contacts,
                "workspace-top-bar-contacts",
                "workspace-section-contacts",
            ),
            (
                TopBarSection::Calls,
                "workspace-top-bar-calls",
                "workspace-section-calls",
            ),
        ] {
            let button = ui.debug_bounds(button_id).unwrap();
            ui.simulate_click(button.center(), Modifiers::none());
            ui.run_until_parked();
            assert!(ui.debug_bounds(page_id).is_some());
            assert_eq!(ui.read(|app| client.read(app).section), section);
        }
        let chats = ui.debug_bounds("workspace-top-bar-chats").unwrap();
        ui.simulate_click(chats.center(), Modifiers::none());
        ui.run_until_parked();
        ui.read(|app| {
            assert_eq!(client.read(app).section, TopBarSection::Chats);
            assert!(client.read(app).conversation.as_ref() == Some(&conversation));
        });
        assert_eq!(
            futures::executor::block_on(runtime.shutdown()),
            RuntimeExit::Clean
        );
    }

    /// Открывает полное меню, показывает непрочитанное и проверяет модальное поведение.
    #[gpui::test]
    fn top_bar_exposes_overflow_accounts_without_passing_through_outside_clicks(
        cx: &mut TestAppContext,
    ) {
        cx.update(bind_keys);
        let profile = tempfile::tempdir().unwrap();
        let (runtime, _events) = RuntimeClient::start_in(profile.path().into()).unwrap();
        let mut accounts = (0..20)
            .map(|index| account(&format!("Аккаунт {index}")))
            .collect::<Vec<_>>();
        accounts[1].unread = Some(7);
        accounts[3].unread = Some(1001);
        let (client, ui) = cx.add_window_view(|_, cx| {
            Client::new(
                Options {
                    active: Some(accounts[0].session.owner.account_id.clone()),
                    labels: HashMap::new(),
                },
                runtime.clone(),
                cx,
            )
        });
        ui.simulate_resize(size(px(620.), px(360.)));
        ui.update(|_, app| {
            client.update(app, |view, cx| {
                view.ready = true;
                view.handle_runtime(RuntimeEvent::Accounts(accounts.clone()), cx);
            });
        });
        ui.run_until_parked();

        let bar = ui.debug_bounds("workspace-top-bar").unwrap();
        let sidebar = ui.debug_bounds("workspace-sidebar-panel").unwrap();
        assert_eq!(sidebar.top() - bar.bottom(), px(PANEL_GAP));
        let sections = ui.debug_bounds("workspace-top-bar-sections").unwrap();
        assert_eq!(bar.center().x, sections.center().x);
        assert!(ui.debug_bounds("workspace-account-dot-1").is_some());
        assert!(ui.debug_bounds("workspace-account-3").is_none());
        assert!(ui.debug_bounds("workspace-account-overflow").is_some());
        let trigger = ui.debug_bounds("workspace-account-menu-trigger").unwrap();
        let b = ui.debug_bounds("workspace-account-1").unwrap();

        ui.simulate_click(trigger.center(), Modifiers::none());
        ui.run_until_parked();
        let menu = ui.debug_bounds("workspace-account-menu").unwrap();
        assert!(menu.right() <= px(616.) && menu.bottom() <= px(356.));
        assert!(ui.debug_bounds("workspace-account-count-3").is_some());

        ui.simulate_click(b.center(), Modifiers::none());
        ui.run_until_parked();
        assert!(!ui.read(|app| client.read(app).top_bar.read(app).menu_is_open()));
        assert_eq!(
            ui.read(|app| client.read(app).shell.selected().cloned()),
            Some(accounts[0].session.clone())
        );

        press_key(ui, "enter");
        assert!(ui.read(|app| client.read(app).top_bar.read(app).menu_is_open()));
        ui.simulate_keystrokes("tab shift-tab end up");
        ui.run_until_parked();
        let last = ui.debug_bounds("workspace-account-menu-row-19").unwrap();
        assert!(
            last.top() >= menu.top() && last.bottom() <= menu.bottom(),
            "{last:?} / {menu:?}"
        );
        assert!(ui.update(|window, app| client.read(app).top_bar.read(app).menu_has_focus(window)));
        press_key(ui, "space");
        assert!(!ui.read(|app| client.read(app).top_bar.read(app).menu_is_open()));
        assert_eq!(
            ui.read(|app| client.read(app).shell.selected().cloned()),
            Some(accounts[19].session.clone())
        );

        press_key(ui, "space");
        assert!(ui.read(|app| client.read(app).top_bar.read(app).menu_is_open()));
        ui.simulate_keystrokes("escape");
        ui.run_until_parked();
        assert!(!ui.read(|app| client.read(app).top_bar.read(app).menu_is_open()));
        assert_eq!(
            ui.read(|app| client.read(app).shell.selected().cloned()),
            Some(accounts[19].session.clone())
        );
        assert_eq!(
            futures::executor::block_on(runtime.shutdown()),
            RuntimeExit::Clean
        );
    }

    /// Передаёт обе фазы нативной клавиши активации GPUI.
    fn press_key(ui: &mut gpui::VisualTestContext, key: &str) {
        ui.simulate_keystrokes(key);
        ui.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse(key).unwrap(),
        });
        ui.run_until_parked();
    }

    /// Отпускание указателя после замены сессий не выбирает аккаунт на старой позиции.
    #[gpui::test]
    fn top_bar_discards_a_press_when_the_account_list_changes(cx: &mut TestAppContext) {
        cx.update(bind_keys);
        let profile = tempfile::tempdir().unwrap();
        let (runtime, _events) = RuntimeClient::start_in(profile.path().into()).unwrap();
        let a = account("A");
        let b = account("B");
        let c = account("C");
        let (client, ui) = cx.add_window_view(|_, cx| {
            Client::new(
                Options {
                    active: Some(a.session.owner.account_id.clone()),
                    labels: HashMap::new(),
                },
                runtime.clone(),
                cx,
            )
        });
        ui.simulate_resize(size(px(620.), px(360.)));
        let show_accounts = |accounts: Vec<AccountSnapshot>, ui: &mut gpui::VisualTestContext| {
            ui.update(|_, app| {
                client.update(app, |view, cx| {
                    view.ready = true;
                    view.handle_runtime(RuntimeEvent::Accounts(accounts), cx);
                });
            });
            ui.run_until_parked();
        };

        for use_menu in [false, true] {
            show_accounts(vec![a.clone(), b.clone(), c.clone()], ui);
            let selector = if use_menu {
                let trigger = ui.debug_bounds("workspace-account-menu-trigger").unwrap();
                ui.simulate_click(trigger.center(), Modifiers::none());
                ui.run_until_parked();
                "workspace-account-menu-row-1"
            } else {
                "workspace-account-1"
            };
            let position = ui.debug_bounds(selector).unwrap().center();
            ui.simulate_mouse_down(position, gpui::MouseButton::Left, Modifiers::none());
            show_accounts(vec![a.clone(), c.clone(), b.clone()], ui);
            ui.simulate_mouse_up(position, gpui::MouseButton::Left, Modifiers::none());
            ui.run_until_parked();
            assert_eq!(
                ui.read(|app| client.read(app).shell.selected().cloned()),
                Some(a.session.clone())
            );
        }
        assert_eq!(
            futures::executor::block_on(runtime.shutdown()),
            RuntimeExit::Clean
        );
    }
}

#[cfg(all(test, feature = "gpui-test"))]
mod loopback;
