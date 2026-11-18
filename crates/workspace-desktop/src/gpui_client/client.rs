//! Направляет действия между оболочкой, рабочим потоком, экраном разговора и AppKit.
//! Здесь также собирается основная разметка окна из готовых компонентов.

use std::collections::HashMap;

use gpui::{
    Context, Entity, FocusHandle, SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use workspace_core::{auth::SessionStatus, context::AccountId};

use super::{
    Options,
    contracts::{RuntimeEvent, RuntimeExit, ShellAction, SidebarAction},
    conversation::Conversation,
    next_control,
    platform_window::WindowPlatform,
    previous_control,
    runtime::{RuntimeClient, StartupError},
    shell::{Shell, ShellEffect},
    sidebar::{FolderIntent, Sidebar},
    sign_in::SignIn,
    ui::{Palette, TextRole, ThemeMode, TopBar, TopBarAccount, TopBarSelection, label},
};

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
    shell: Shell,             // Выбор, адрес разговора и фильтр событий по входам.
    runtime: RuntimeClient,   // Очередь команд рабочего потока этого профиля.
    sidebar: Entity<Sidebar>, // Единственный каталог; хранит поиск каждого входа.
    conversation: Option<Entity<Conversation>>, // Экран актуального адреса, если он открыт.
    sign_in: Option<Entity<SignIn>>, // Форма нужна, пока в профиле нет доступного входа.
    labels: HashMap<AccountId, String>, // Подписи для переключателя и баннеров.
    top_bar: Entity<TopBar>,  // Шапка хранит локальный фокус и показывает снимок оболочки.
    _top_bar_subscription: Subscription, // Передаёт выбор в оболочку.
    initial_focus: FocusHandle, // Корневая точка обхода до выбора первого элемента.
    restore_focus: bool,      // После закрытия формы вернуть фокус в рабочую область.
    ready: bool,              // Рабочий поток открыл профиль и подготовил подписки.
    received_accounts: bool,  // Пришёл первый снимок аккаунтов, может быть пустым.
    startup_error: Option<StartupError>, // Безопасная причина сбоя открытия профиля.
    stopped: bool,            // Фоновая работа завершилась с ошибкой.
    _sidebar_subscription: Subscription, // Направляет команды каталога в оболочку.
    runtime_events: Option<Task<()>>, // Подписка живёт столько же, сколько окно.
}

impl Client {
    /// Создаёт оболочку и связывает события каталога с тем же маршрутом, что у баннеров.
    pub(super) fn new(options: Options, runtime: RuntimeClient, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(Sidebar::new);
        cx.observe(&sidebar, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&sidebar, |client, _, intent: &FolderIntent, cx| {
            client.handle_folder_intent(intent.clone(), cx);
        })
        .detach();
        let top_bar = cx.new(TopBar::new);
        let selection = cx.subscribe(&top_bar, |client, _, selection: &TopBarSelection, cx| {
            client.handle_action(ShellAction::SelectAccount(selection.0.clone()), cx);
        });
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
            shell: Shell::new(options.active),
            runtime,
            sidebar,
            conversation: None,
            sign_in: None,
            labels: options.labels,
            top_bar,
            _top_bar_subscription: selection,
            initial_focus: cx.focus_handle(),
            restore_focus: false,
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
                if self.shell.selected() != Some(&session) {
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
        match effect {
            Some(ShellEffect::OpenConversation(address)) => {
                let runtime = self.runtime.clone();
                self.conversation = Some(cx.new(|cx| Conversation::open(address, runtime, cx)));
            }
            Some(ShellEffect::CloseConversation) => self.conversation = None,
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

    /// Показывает вход после чтения профиля и передаёт окно первой готовой сессии после входа.
    fn reconcile_sign_in(&mut self, cx: &mut Context<Self>) -> Option<ShellEffect> {
        let accounts = self.shell.accounts();
        let show = self.status().is_none()
            && accounts
                .iter()
                .all(|account| account.status == SessionStatus::ReauthenticationRequired);
        if show {
            if self.sign_in.is_none() {
                self.sidebar.update(cx, |view, cx| view.dismiss_folders(cx));
                let server = match accounts {
                    [account] => Some(account.session.owner.server.as_str()),
                    _ => None,
                };
                self.sign_in = Some(cx.new(|cx| SignIn::new(self.runtime.clone(), server, cx)));
            }
            return None;
        }

        self.sign_in.take()?;
        self.restore_focus = true;
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
        if std::mem::take(&mut self.restore_focus) || window.focused(cx).is_none() {
            window.focus(&self.initial_focus);
        }
    }
}

impl Render for Client {
    /// Показывает форму входа или рабочую область с выбранным аккаунтом.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.focus_workspace(window, cx);
        let palette = Palette::for_mode(ThemeMode::from(window.appearance()));
        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.background)
            .text_color(palette.text)
            .track_focus(&self.initial_focus)
            .on_action(next_control)
            .on_action(previous_control);
        if let Some(sign_in) = &self.sign_in {
            return root.child(div().flex().flex_1().min_h_0().child(sign_in.clone()));
        }

        let issue = self.shell.route_error().or_else(|| {
            WindowPlatform::notification_error(cx).map(|_| "Системные уведомления недоступны")
        });
        let content = if let Some(status) = self.status() {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .child(label(palette, TextRole::Label, status))
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
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .overflow_hidden()
                        .map(|view| match &self.conversation {
                            Some(conversation) => view.child(conversation.clone()),
                            None => view.child(empty),
                        }),
                )
        };

        root.child(self.top_bar.clone())
            .children(issue.map(|message| {
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
        let chats = ui.debug_bounds("workspace-top-bar-chats").unwrap();
        assert_eq!(bar.center().x, chats.center().x);
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
        ui.simulate_keystrokes("tab shift-tab end");
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
