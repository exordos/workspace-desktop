//! Связывает маршруты «Моей активности» с выбранным входом и командами рабочего окна.

use gpui::{Context, Div, IntoElement, Task, Window, div, prelude::*};
use workspace_core::{auth::SessionStatus, storage::HistoryScope};

use super::Client;
use crate::gpui_client::{
    activity::{
        self,
        inbox::{self, InboxAction},
    },
    contracts::{
        AccountSession, ActivityAddress, ActivityPage, ChatEntry, RuntimeEvent, ShellAction,
        SidebarAction,
    },
    preferences::Preferences,
    runtime::SelfChatRequestError,
    sidebar::ActivityNavigation,
    ui::{Button, Palette, TopBarSection},
};

/// Загрузка чата с собой для одного показа страницы «Избранное».
pub(super) struct FavoritesLoad {
    address: ActivityAddress, // Полная цель первоначального запроса.
    task: Option<Task<()>>,   // None после ошибки, когда доступен повтор.
}

impl Client {
    /// Передаёт сведения о показе и каталоге открытым страницам.
    pub(super) fn sync_activity(&mut self, cx: &mut Context<Self>) {
        let address = self.visible_activity().cloned();
        let draft_address = address
            .as_ref()
            .filter(|address| address.page == ActivityPage::Drafts)
            .cloned();
        let session = self
            .activity_account()
            .map(|account| account.session.clone());
        let catalog = self.shell.selected_catalog();
        self.activity_drafts
            .update(cx, |drafts, cx| drafts.set_view(draft_address, catalog, cx));
        self.activity_messages.update(cx, |messages, cx| {
            messages.set_session(session, cx);
            messages.set_chats(catalog.and_then(|list| list.chats.clone()), cx);
            messages.set_address(address, cx);
        });
        self.sync_activity_navigation(cx);
        self.sync_favorites(cx);
    }

    /// Отдаёт блоку выбранный маршрут и только подтверждённые счётчики.
    pub(super) fn sync_activity_navigation(&self, cx: &mut Context<Self>) {
        let navigation = self.activity_account().map(|account| ActivityNavigation {
            page: self.shell.activity().map(|address| address.page),
            inbox_count: account.unread,
            mention_count: self.activity_messages.read(cx).unread_mentions(),
        });
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_activity(navigation, cx));
    }

    /// Находит счётчики строго выбранного действующего входа.
    fn activity_account(&self) -> Option<&crate::gpui_client::contracts::AccountSnapshot> {
        let session = self.shell.selected()?;
        self.shell
            .accounts()
            .iter()
            .find(|account| account.session == *session && account.status == SessionStatus::Active)
    }

    /// Пропускает уведомления рабочего потока в списки с подходящим поколением.
    pub(super) fn sync_activity_event(&self, event: &RuntimeEvent, cx: &mut Context<Self>) {
        match event {
            RuntimeEvent::Sync(update) => {
                self.activity_drafts
                    .update(cx, |drafts, cx| drafts.sync_update(update, cx));
                self.activity_messages
                    .update(cx, |messages, cx| messages.sync_update(update, cx));
            }
            RuntimeEvent::SyncLagged => {
                self.activity_drafts
                    .update(cx, |drafts, cx| drafts.sync_lagged(cx));
                self.activity_messages
                    .update(cx, |messages, cx| messages.sync_lagged(cx));
            }
            _ => {}
        }
    }

    /// Возвращает страницу, только когда она занимает основной раздел мессенджера.
    pub(super) fn visible_activity(&self) -> Option<&ActivityAddress> {
        if self.section == TopBarSection::Chats
            && self.licenses.is_none()
            && self.sign_in.is_none()
            && self.status().is_none()
        {
            self.shell.activity()
        } else {
            None
        }
    }

    /// Показывает активность только в текущем рабочем контексте мессенджера.
    pub(super) fn render_activity(
        &self,
        address: &ActivityAddress,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Div {
        let language = Preferences::read_for(cx, &address.session.owner).language;
        let search = cx.entity().downgrade();
        let action = cx.entity().downgrade();
        let origin = address.clone();
        activity::frame(
            address.page,
            palette,
            language,
            match address.page {
                ActivityPage::Inbox => {
                    let catalog = self.shell.selected_catalog();
                    let chats = self.shell.selected_activity_chats().unwrap_or_default();
                    inbox::render(
                        address,
                        chats,
                        catalog.map(|list| list.status),
                        palette,
                        language,
                        move |event, _, app| {
                            let _ = action
                                .update(app, |client, cx| client.inbox_action(&origin, event, cx));
                        },
                    )
                    .into_any_element()
                }
                ActivityPage::Favorites => self
                    .render_favorites(address, palette, language, cx)
                    .into_any_element(),
                ActivityPage::Drafts => self.activity_drafts.clone().into_any_element(),
                ActivityPage::Marked | ActivityPage::Mentions => {
                    self.activity_messages.clone().into_any_element()
                }
            },
            move |window, app| {
                let _ = search.update(app, |client, cx| client.focus_activity_search(window, cx));
            },
        )
    }

    /// Использует тот же переход в избранное из профиля и сочетания клавиш.
    pub(super) fn open_favorites(
        &mut self,
        session: AccountSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shell.selected() != Some(&session)
            || self.status().is_some()
            || self.sign_in.is_some()
        {
            return;
        }
        self.right_panel
            .update(cx, |panel, cx| panel.close_all(window, cx));
        self.open_section(TopBarSection::Chats, cx);
        self.handle_action(
            ShellAction::Sidebar(SidebarAction::OpenActivity {
                page: ActivityPage::Favorites,
                session,
            }),
            cx,
        );
    }

    /// Запускает один запрос для видимого избранного и снимает запрос прежнего показа.
    fn sync_favorites(&mut self, cx: &mut Context<Self>) {
        let address = self.visible_activity().filter(|address| {
            address.page == ActivityPage::Favorites
                && self.shell.can_finish_activity(address)
                && self.conversation.is_none()
        });
        let Some(address) = address.cloned() else {
            self.favorites_load = None;
            return;
        };
        if self
            .favorites_load
            .as_ref()
            .is_some_and(|load| load.address == address)
        {
            return;
        }
        let origin = address.clone();
        let runtime = self.runtime.clone();
        let task = cx.spawn(async move |client, cx| {
            let result = runtime.ensure_self_chat(origin.session.clone()).await;
            let _ = client.update(cx, |client, cx| {
                client.finish_favorites(&origin, result, cx)
            });
        });
        self.favorites_load = Some(FavoritesLoad {
            address,
            task: Some(task),
        });
    }

    /// Применяет результат только к видимому запросу и открывает обычный экран истории.
    fn finish_favorites(
        &mut self,
        address: &ActivityAddress,
        result: Result<HistoryScope, SelfChatRequestError>,
        cx: &mut Context<Self>,
    ) {
        if self
            .favorites_load
            .as_ref()
            .is_none_or(|load| load.address != *address)
            || !self.shell.can_finish_activity(address)
            || self.section != TopBarSection::Chats
            || self.licenses.is_some()
            || self.sign_in.is_some()
        {
            return;
        }
        match result {
            Ok(scope) => {
                if let Some(effect) = self.shell.open_favorites_target(address, scope) {
                    self.apply(Some(effect), cx);
                    return;
                }
            }
            Err(error) => log::debug!(
                "gpui_shell op_id={} event=favorites_load_failed reason={error:?}",
                address.revision.0
            ),
        }
        if let Some(load) = &mut self.favorites_load {
            load.task = None;
        }
        cx.notify();
    }

    /// Показывает загрузку или безопасную ошибку с повтором в том же входе.
    fn render_favorites(
        &self,
        address: &ActivityAddress,
        palette: Palette,
        language: crate::gpui_client::preferences::Language,
        cx: &Context<Self>,
    ) -> Div {
        let loading = self
            .favorites_load
            .as_ref()
            .is_some_and(|load| load.address == *address && load.task.is_some());
        let status = if loading {
            activity::status(
                palette,
                language.tr("desktop.activity.favorites.loading"),
                false,
            )
            .debug_selector(|| "favorites-loading".into())
        } else {
            let target = cx.entity().downgrade();
            let origin = address.clone();
            activity::status(palette, language.tr("favorites.loadError"), true)
                .debug_selector(|| "favorites-error".into())
                .child(
                    Button::new("favorites-retry", language.tr("common.retry")).render(
                        palette,
                        None,
                        move |_, app| {
                            let _ = target.update(app, |client, cx| {
                                if client.visible_activity() == Some(&origin) {
                                    client.favorites_load = None;
                                    client.handle_action(
                                        ShellAction::Sidebar(SidebarAction::OpenActivity {
                                            page: ActivityPage::Favorites,
                                            session: origin.session.clone(),
                                        }),
                                        cx,
                                    );
                                }
                            });
                        },
                    ),
                )
        };
        div().p_3().child(status)
    }

    /// Открывает адрес входящих только для актуального показа страницы.
    fn inbox_action(
        &mut self,
        origin: &ActivityAddress,
        action: InboxAction,
        cx: &mut Context<Self>,
    ) {
        if self.visible_activity() != Some(origin) {
            return;
        }
        match action {
            InboxAction::Open(scope) => {
                let effect = self
                    .shell
                    .open_activity_target(origin, scope, ChatEntry::Latest);
                self.apply(effect, cx);
            }
            InboxAction::Retry => self.handle_action(
                ShellAction::Sidebar(SidebarAction::RetryCatalog(origin.session.clone())),
                cx,
            ),
        }
    }

    /// Переводит фокус к штатному поиску каталога и при необходимости раскрывает его.
    fn focus_activity_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_return = window.focused(cx);
        if self.sidebar_visible {
            self.sidebar.read(cx).focus_search(window, cx);
        } else {
            self.sidebar_visible = true;
            let sidebar = self.sidebar.clone();
            window.defer(cx, move |window, app| {
                sidebar.read(app).focus_search(window, app)
            });
            cx.notify();
        }
    }
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use gpui::{Modifiers, TestAppContext, px, size};
    use uuid::Uuid;
    use workspace_core::{
        bootstrap::{ChatList, ChatListStatus},
        context::{AccountId, OwnerContext, SessionGeneration},
        messenger::catalog::{Chat, ChatMetadata, Topic, TopicMetadata},
    };

    use super::*;
    use crate::gpui_client::{
        Options, bind_keys,
        contracts::{AccountSnapshot, RuntimeEvent, RuntimeExit},
        runtime::RuntimeClient,
    };

    /// Готовит действующий тестовый вход с собственным счётчиком.
    fn account(name: &str, user: u128) -> AccountSnapshot {
        AccountSnapshot {
            session: AccountSession {
                owner: OwnerContext {
                    account_id: AccountId(name.into()),
                    server: "https://workspace.test".into(),
                    organization_id: "org".into(),
                    project_id: "project".into(),
                    user_id: Uuid::from_u128(user).to_string(),
                },
                generation: SessionGeneration(1),
            },
            status: SessionStatus::Active,
            unread: Some(3),
        }
    }

    /// Сохраняет раскрытие локально и открывает выбранную тему из актуальных входящих.
    #[gpui::test]
    fn activity_sidebar_persists_per_owner_and_inbox_opens_the_selected_topic(
        cx: &mut TestAppContext,
    ) {
        let profile = tempfile::tempdir().unwrap();
        cx.update(|app| {
            bind_keys(app);
            Preferences::install(profile.path(), app);
        });
        let (runtime, _events) = RuntimeClient::start_in(profile.path().into()).unwrap();
        let a = account("A", 1);
        let b = account("B", 2);
        let stream = Uuid::from_u128(10);
        let topic = Uuid::from_u128(20);
        let (client, ui) = cx.add_window_view(|_, cx| {
            Client::new(
                Options {
                    active: Some(a.session.owner.account_id.clone()),
                    ..Options::default()
                },
                runtime.clone(),
                cx,
            )
        });
        ui.simulate_resize(size(px(860.), px(600.)));
        ui.update(|_, app| {
            client.update(app, |view, cx| {
                view.ready = true;
                view.handle_runtime(RuntimeEvent::Accounts(vec![a.clone(), b.clone()]), cx);
                view.handle_runtime(
                    RuntimeEvent::Catalog(HashMap::from([
                        (
                            a.session.owner.clone(),
                            ChatList {
                                generation: a.session.generation,
                                chats: Some(Arc::new([Chat {
                                    id: stream,
                                    name: "Разработка".into(),
                                    direct: false,
                                    topics: vec![Topic {
                                        id: topic,
                                        name: "Сборка".into(),
                                        metadata: TopicMetadata {
                                            active_unread_count: Some(3),
                                            ..Default::default()
                                        },
                                    }],
                                    metadata: ChatMetadata {
                                        active_unread_count: Some(3),
                                        ..Default::default()
                                    },
                                }])),
                                status: ChatListStatus::Ready,
                            },
                        ),
                        (
                            b.session.owner.clone(),
                            ChatList {
                                generation: b.session.generation,
                                chats: Some(Arc::new([])),
                                status: ChatListStatus::Ready,
                            },
                        ),
                    ])),
                    cx,
                );
            })
        });
        ui.run_until_parked();
        let compact = ui.debug_bounds("activity-toggle").unwrap();
        assert_eq!(compact.size.width, px(32.));

        ui.simulate_click(compact.center(), Modifiers::none());
        ui.run_until_parked();
        assert!(ui.debug_bounds("activity-toggle").unwrap().size.width > compact.size.width);
        let inbox = ui.debug_bounds("activity-inbox").unwrap();
        ui.simulate_click(inbox.center(), Modifiers::none());
        ui.run_until_parked();
        assert!(ui.debug_bounds("inbox-channels").is_some());
        let row = ui
            .debug_bounds("inbox-row-00000000-0000-0000-0000-000000000014")
            .unwrap();
        ui.simulate_click(row.center(), Modifiers::none());
        ui.run_until_parked();
        assert_eq!(
            ui.read(|app| client
                .read(app)
                .shell
                .conversation()
                .map(|address| address.route.scope)),
            Some(HistoryScope {
                stream_id: stream,
                topic_id: Some(topic)
            })
        );

        let other = ui.debug_bounds("workspace-account-1").unwrap();
        ui.simulate_click(other.center(), Modifiers::none());
        ui.run_until_parked();
        assert_eq!(
            ui.debug_bounds("activity-toggle").unwrap().size.width,
            compact.size.width
        );
        let compact = ui.debug_bounds("activity-toggle").unwrap();
        let catalog_empty = ui.debug_bounds("catalog-empty").unwrap();
        assert!(compact.bottom() <= catalog_empty.top());
        let inbox = ui.debug_bounds("activity-inbox").unwrap();
        ui.simulate_click(inbox.center(), Modifiers::none());
        ui.run_until_parked();
        assert!(ui.debug_bounds("inbox-empty").is_some());
        assert!(ui.read(|app| {
            let shell = &client.read(app).shell;
            shell
                .activity()
                .is_some_and(|address| address.session == b.session)
                && shell
                    .selected_catalog()
                    .and_then(|list| list.chats.as_deref())
                    .is_some_and(<[_]>::is_empty)
        }));

        ui.update(|_, app| {
            Preferences::install(profile.path(), app);
            assert!(Preferences::read_for(app, &a.session.owner).activity_expanded);
            assert!(!Preferences::read_for(app, &b.session.owner).activity_expanded);
        });
        assert_eq!(
            futures::executor::block_on(runtime.shutdown()),
            RuntimeExit::Clean
        );
    }
}
