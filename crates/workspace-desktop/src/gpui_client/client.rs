//! Направляет действия между оболочкой, рабочим потоком, экраном разговора и AppKit.
//! Здесь также собирается основная разметка окна из готовых компонентов.

use std::collections::{HashMap, HashSet};

use gpui::{
    Context, Div, Entity, FocusHandle, SharedString, Subscription, Task, Window, div, prelude::*,
    px,
};
use workspace_core::{auth::SessionStatus, context::AccountId};

use super::{
    Options,
    contracts::{AccountSession, RuntimeEvent, RuntimeExit, ShellAction, SidebarAction},
    conversation::Conversation,
    next_control,
    platform_window::WindowPlatform,
    previous_control,
    runtime::{RuntimeClient, StartupError},
    shell::{Shell, ShellEffect},
    sidebar::Sidebar,
    ui::{BadgeTone, Button, ButtonAppearance, Palette, TextRole, ThemeMode, label, unread_badge},
};

/// Корневое состояние одного окна. Долгие операции выполняет RuntimeClient.
pub(super) struct Client {
    shell: Shell,             // Выбор, адрес разговора и фильтр событий по входам.
    runtime: RuntimeClient,   // Очередь команд рабочего потока этого профиля.
    sidebar: Entity<Sidebar>, // Единственный каталог; хранит поиск каждого входа.
    conversation: Option<Entity<Conversation>>, // Экран актуального адреса, если он открыт.
    labels: HashMap<AccountId, String>, // Подписи для переключателя и баннеров.
    account_focus: HashMap<AccountSession, FocusHandle>, // Фокус закреплён за входом, а не индексом.
    initial_focus: FocusHandle, // Корневая точка обхода до выбора первого элемента.
    ready: bool,                // Рабочий поток открыл профиль и подготовил подписки.
    received_accounts: bool,    // Пришёл первый снимок аккаунтов, может быть пустым.
    startup_error: Option<StartupError>, // Безопасная причина сбоя открытия профиля.
    stopped: bool,              // Фоновая работа завершилась с ошибкой.
    _sidebar_subscription: Subscription, // Направляет команды каталога в оболочку.
    runtime_events: Option<Task<()>>, // Подписка живёт столько же, сколько окно.
}

impl Client {
    /// Создаёт оболочку и связывает события каталога с тем же маршрутом, что у баннеров.
    pub(super) fn new(options: Options, runtime: RuntimeClient, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(Sidebar::new);
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
            labels: options.labels,
            account_focus: HashMap::new(),
            initial_focus: cx.focus_handle(),
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
                match ready {
                    Ok(()) => {
                        client.ready = true;
                        log::debug!("event=workspace_profile_ready");
                    }
                    Err(error) => {
                        log::warn!("event=workspace_start_failed reason={error:?}");
                        client.startup_error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Применяет действие каталога, переключателя или системного окна.
    pub(super) fn handle_action(&mut self, action: ShellAction, cx: &mut Context<Self>) {
        let effect = self.shell.handle_action(action);
        self.apply(effect, cx);
    }

    /// Доставляет обновления аккаунтов и истории из подписки рабочего потока.
    pub(super) fn handle_runtime(&mut self, event: RuntimeEvent, cx: &mut Context<Self>) {
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

    /// Выполняет команду оболочки и обновляет связанные части того же окна.
    fn apply(&mut self, effect: Option<ShellEffect>, cx: &mut Context<Self>) {
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
        let catalog = self.shell.selected_catalog();
        let conversation = self.shell.conversation();
        self.sidebar.update(cx, |view, cx| {
            view.update_snapshot(self.shell.selected(), catalog, conversation, cx);
        });
        cx.notify();
    }

    /// Собирает переключатель аккаунтов с отдельными счётчиками и устойчивым фокусом.
    fn header(&mut self, palette: Palette, cx: &mut Context<Self>) -> Div {
        let accounts = self.shell.accounts();
        let present: HashSet<_> = accounts
            .iter()
            .map(|account| account.session.clone())
            .collect();
        self.account_focus
            .retain(|session, _| present.contains(session));
        let target = cx.entity().downgrade();
        let entries = accounts
            .iter()
            .enumerate()
            .map(|(index, account)| {
                let session = &account.session;
                let display_name = self
                    .labels
                    .get(&session.owner.account_id)
                    .cloned()
                    .unwrap_or_else(|| format!("Аккаунт {}", index + 1));
                let name = match account.status {
                    SessionStatus::Active => display_name,
                    SessionStatus::Restoring => format!("{display_name} · Вход…"),
                    SessionStatus::TemporarilyUnavailable => format!("{display_name} · Нет связи"),
                    _ => format!("{display_name} · Требуется вход"),
                };
                let mut button = Button::new(("account", index), name);
                if self.shell.selected() == Some(session) {
                    button.appearance = ButtonAppearance::Primary;
                }
                let handle = self
                    .account_focus
                    .entry(session.clone())
                    .or_insert_with(|| cx.focus_handle());
                let destination = session.clone();
                let target = target.clone();
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .child(
                        button
                            .render(palette, Some(handle), move |_, app| {
                                let _ = target.update(app, |client, cx| {
                                    client.handle_action(
                                        ShellAction::SelectAccount(destination.clone()),
                                        cx,
                                    );
                                });
                            })
                            .max_w(px(180.))
                            .debug_selector(move || format!("workspace-account-{index}")),
                    )
                    .children(account.unread.and_then(|count| {
                        unread_badge(palette, u64::from(count), BadgeTone::Accent)
                    }))
            })
            .collect::<Vec<_>>();
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .p_3()
            .bg(palette.surface)
            .border_b_1()
            .border_color(palette.border)
            .child(label(palette, TextRole::Heading, "Workspace").flex_none())
            .child(
                div()
                    .id("workspace-accounts")
                    .flex()
                    .items_center()
                    .min_w_0()
                    .gap_2()
                    .overflow_x_scroll()
                    .children(entries),
            )
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
        if self.shell.accounts().is_empty() {
            return Some("Нет сохранённых аккаунтов. Войдите через workspace-auth login.".into());
        }
        None
    }

    /// Устанавливает фокус внутри окна, чтобы Tab работал до первого клика.
    fn focus_first_time(&self, window: &mut Window, cx: &Context<Self>) {
        if window.focused(cx).is_none() {
            window.focus(&self.initial_focus);
        }
    }
}

impl Render for Client {
    /// Показывает аккаунты, каталог и актуальный разговор с редактором.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.focus_first_time(window, cx);
        let palette = Palette::for_mode(ThemeMode::from(window.appearance()));
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
                .child(
                    div()
                        .w(px(264.))
                        .max_w(gpui::relative(0.44))
                        .h_full()
                        .flex_none()
                        .border_r_1()
                        .border_color(palette.border)
                        .child(self.sidebar.clone()),
                )
                .child(
                    div()
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

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.background)
            .text_color(palette.text)
            .track_focus(&self.initial_focus)
            .on_action(next_control)
            .on_action(previous_control)
            .child(self.header(palette, cx))
            .children(issue.map(|message| {
                label(palette, TextRole::Body, message)
                    .p_2()
                    .text_color(palette.danger)
            }))
            .child(div().flex().flex_1().min_h_0().min_w_0().child(content))
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
    use crate::gpui_client::{bind_keys, contracts::AccountSnapshot};

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
}

#[cfg(all(test, feature = "gpui-test"))]
mod loopback;
