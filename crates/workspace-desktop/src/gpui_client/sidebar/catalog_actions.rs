//! Собирает действия чатов и тем и отправляет выбранную команду в Core.

use std::rc::Rc;

use gpui::{Context, Entity, Window};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        catalog::{Chat, Topic},
        details::{SidebarTopicAction, StreamNotificationMode, TopicNotificationMode},
        folders::FolderItem,
        sidebar_actions::{SidebarStreamAction, SidebarStreamResult},
    },
    storage::HistoryScope,
};

use super::{
    AccountSession, Active, ChatEntry, ChatRoute, RowKey, Sidebar, SidebarAction, chat_topic,
    folder_menu::{RowAction, RowActions, option},
};
use crate::gpui_client::{
    conversation::{ContactPanel, ContactRequest, ContactTarget},
    preferences::{Language, Preferences},
    right_panel::RightPanel,
};

mod dialog;
use dialog::{CatalogDialog, DialogMode, DialogRequest, Status};

/// Действие контекстного меню разговора или темы.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum CatalogAction {
    ChatNotifications(StreamNotificationMode), // Задать уведомления сразу для чата.
    TopicNotifications(TopicNotificationMode), // Задать исключение для темы.
    MarkRead,                                  // Прочитать разговор или только выбранную тему.
    Contact,                                   // Открыть профиль собеседника.
    NewTopic,                                  // Запросить название новой темы.
    Rename,                                    // Запросить новое название канала или темы.
    ToggleDone,                                // Переключить завершённость темы.
    Archive,                                   // Запросить подтверждение архивации разговора.
}

/// Команда, готовая к отправке на рабочий поток.
pub(super) enum Command {
    Stream(SidebarStreamAction),           // Команда всему разговору.
    Notifications(StreamNotificationMode), // Изменить уведомления разговора.
    Topic(Uuid, SidebarTopicAction),       // Команда одной теме по её UUID.
}

/// Возвращает общие действия разговора с учётом вида и состояния чата.
pub(super) fn actions_for_chat(
    chat: &Chat,
    item: Option<&FolderItem>,
    language: Language,
    editable: bool,
) -> RowActions {
    use StreamNotificationMode::{AllMessages, MentionsOnly, Muted};
    let active = match chat.metadata.notification_mode.as_deref() {
        Some("muted") => Muted,
        Some("mentions_only") if !chat.direct => MentionsOnly,
        _ => AllMessages,
    };
    let mut actions = vec![setting(
        language.tr("desktop.conversation.channel.allNotifications"),
        CatalogAction::ChatNotifications(AllMessages),
        active == AllMessages,
        editable,
    )];
    if !chat.direct {
        actions.push(setting(
            language.tr("desktop.conversation.channel.mentionsOnly"),
            CatalogAction::ChatNotifications(MentionsOnly),
            active == MentionsOnly,
            editable,
        ));
    }
    actions.push(setting(
        language.tr("desktop.sidebar.actions.muteChat"),
        CatalogAction::ChatNotifications(Muted),
        active == Muted,
        editable,
    ));
    let has_unread = item
        .map(|item| u64::from(item.unread_count))
        .or(chat.metadata.unread_count)
        .map_or_else(
            || {
                chat.topics.iter().any(|topic| total_unread(topic) > 0)
                    || chat.metadata.active_unread_count.unwrap_or(0) > 0
                    || chat.metadata.passive_unread_count.unwrap_or(0) > 0
            },
            |count| count > 0,
        );
    if has_unread {
        actions.push(entry(
            language,
            "desktop.sidebar.actions.markAllAsRead",
            CatalogAction::MarkRead,
            editable,
        ));
    }
    if chat.direct {
        actions.push(entry(
            language,
            "desktop.sidebar.actions.contactInfo",
            CatalogAction::Contact,
            editable,
        ));
    }
    if !chat.direct {
        actions.push(entry(
            language,
            "desktop.sidebar.actions.renameChannel",
            CatalogAction::Rename,
            editable,
        ));
    }
    actions.push(entry(
        language,
        "desktop.sidebar.actions.newTopic",
        CatalogAction::NewTopic,
        editable,
    ));
    actions.push(entry(
        language,
        "desktop.sidebar.actions.archive",
        CatalogAction::Archive,
        editable,
    ));
    actions
}

/// Использует один список действий для тем в чатах и каналах.
pub(super) fn actions_for_topic(
    chat: &Chat,
    topic: &Topic,
    language: Language,
    editable: bool,
) -> RowActions {
    use TopicNotificationMode::{Default, Follow, Mute, Unmute};
    let mode = match topic.metadata.notification_mode.as_deref() {
        Some("mute") => Mute,
        Some("follow") => Follow,
        Some("unmute") => Unmute,
        _ => Default,
    };
    let stream_muted = chat.metadata.notification_mode.as_deref() == Some("muted");
    let mut actions: RowActions = TopicNotificationMode::choices(chat.direct, stream_muted, mode)
        .iter()
        .map(|&choice| {
            let key = match choice {
                Mute => "desktop.conversation.channel.topicNotificationsMuted",
                Default if chat.direct => "desktop.sidebar.actions.inheritFromChat",
                Default => "desktop.conversation.topics.inheritFromChannel",
                Follow => "desktop.conversation.topics.follow",
                Unmute => "desktop.conversation.topics.unmuted",
            };
            setting(
                language.tr(key),
                CatalogAction::TopicNotifications(choice),
                choice == mode.displayed(chat.direct),
                editable,
            )
        })
        .collect();
    if total_unread(topic) > 0 {
        actions.push(entry(
            language,
            "desktop.conversation.topics.markAsRead",
            CatalogAction::MarkRead,
            editable,
        ));
    }
    actions.push(entry(
        language,
        "channel.renameTopic",
        CatalogAction::Rename,
        editable,
    ));
    actions.push(entry(
        language,
        if topic.metadata.is_done {
            "desktop.sidebar.actions.reopenTopic"
        } else {
            "desktop.conversation.topics.markAsCompleted"
        },
        CatalogAction::ToggleDone,
        editable,
    ));
    actions
}

/// Учитывает приглушённые сообщения, если сервер передал счётчики по частям.
fn total_unread(topic: &Topic) -> u64 {
    topic.metadata.unread_count.unwrap_or_else(|| {
        topic
            .metadata
            .active_unread_count
            .unwrap_or(0)
            .saturating_add(topic.metadata.passive_unread_count.unwrap_or(0))
    })
}

/// Создаёт строку меню с обычной подписью.
fn entry(
    language: Language,
    title: &'static str,
    action: CatalogAction,
    enabled: bool,
) -> super::folder_menu::RowOption {
    option(language.tr(title), RowAction::Catalog(action), enabled)
}

/// Отмечает уже выбранный режим и оставляет его без повторной команды.
fn setting(
    title: &str,
    action: CatalogAction,
    chosen: bool,
    enabled: bool,
) -> super::folder_menu::RowOption {
    let title = if chosen {
        format!("✓  {title}")
    } else {
        title.to_owned()
    };
    option(title, RowAction::Catalog(action), enabled && !chosen)
}

impl Active {
    /// Использует счётчик элемента именно той папки, где показан чат.
    pub(super) fn folder_item(&self, stream_id: Uuid) -> Option<&FolderItem> {
        let folders = self.folder_view.as_ref()?.folders.as_ref()?;
        let folder_id = if self.folder_contains(stream_id) {
            self.folder
        } else {
            Uuid::nil()
        };
        folders
            .iter()
            .find(|folder| folder.id == folder_id)?
            .items
            .iter()
            .find(|item| item.stream_id == stream_id)
    }
}

impl Sidebar {
    /// Связывает каталог с общим слоем профилей и диалогов окна.
    pub(in crate::gpui_client) fn set_action_panel(&mut self, host: &Entity<RightPanel>) {
        self.action_panel = Some(host.downgrade());
    }

    /// Открывает форму или сразу посылает выбранное из актуального меню действие.
    pub(super) fn run_catalog_action(
        &mut self,
        session: &AccountSession,
        key: RowKey,
        action: CatalogAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active.as_ref().filter(|active| {
            &active.session == session && active.folder_online && active.catalog_pending.is_none()
        }) else {
            return;
        };
        let stream_id = key.chat_id();
        let topic = match key {
            RowKey::Chat(_) => None,
            RowKey::Topic(_, id) => Some(id),
            _ => return,
        };
        let command = match action {
            CatalogAction::ChatNotifications(mode) => Command::Notifications(mode),
            CatalogAction::TopicNotifications(mode) => {
                let Some(topic_id) = topic else { return };
                Command::Topic(topic_id, SidebarTopicAction::SetNotifications(mode))
            }
            CatalogAction::MarkRead => topic
                .map_or(Command::Stream(SidebarStreamAction::MarkRead), |id| {
                    Command::Topic(id, SidebarTopicAction::MarkRead)
                }),
            CatalogAction::ToggleDone => {
                let Some(id) = topic else { return };
                Command::Topic(id, SidebarTopicAction::ToggleDone)
            }
            CatalogAction::NewTopic
            | CatalogAction::Rename
            | CatalogAction::Archive
            | CatalogAction::Contact => {
                let Some(chat) = active.chat(stream_id) else {
                    return;
                };
                let Some(host) = self.action_panel.as_ref().and_then(|host| host.upgrade()) else {
                    return;
                };
                let opener = active
                    .focus
                    .get(&key)
                    .or_else(|| active.focus.get(&RowKey::Chevron(stream_id)))
                    .cloned()
                    .unwrap_or_else(|| active.search.read(cx).keyboard_focus());
                if action == CatalogAction::Contact {
                    if let Some(runtime) = &self.runtime {
                        let source = session.clone();
                        let sidebar = cx.weak_entity();
                        let request = ContactRequest {
                            session: session.clone(),
                            target: ContactTarget::Direct(stream_id),
                            opener,
                            message: Some(Rc::new(move |app| {
                                let _ = sidebar.update(app, |sidebar, cx| {
                                    sidebar.open_contact_chat(&source, stream_id, cx);
                                });
                            })),
                            origin: None,
                        };
                        ContactPanel::open(&host, runtime.clone(), request, window, cx);
                    }
                    return;
                }
                let mode = match action {
                    CatalogAction::NewTopic => DialogMode::CreateTopic(Uuid::new_v4()),
                    CatalogAction::Rename => match topic {
                        Some(id) => {
                            let Some(topic) = chat.topics.iter().find(|topic| topic.id == id)
                            else {
                                return;
                            };
                            DialogMode::RenameTopic(id, topic.name.clone())
                        }
                        None => DialogMode::RenameStream,
                    },
                    CatalogAction::Archive => DialogMode::Archive(
                        chat.direct && chat.metadata.direct_peer_id != session.user_uuid(),
                    ),
                    _ => unreachable!(),
                };
                let request = DialogRequest {
                    sidebar: cx.weak_entity(),
                    session: session.clone(),
                    key,
                    mode,
                    title: chat.name.clone(),
                };
                CatalogDialog::open(&host, request, opener, window, cx);
                return;
            }
        };
        self.send_catalog_command(session, key, command, None, window, cx);
    }

    /// Открывает тот же чат из карточки, даже если пользователь уже сменил папку.
    fn open_contact_chat(
        &mut self,
        source: &AccountSession,
        stream_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self
            .active
            .as_ref()
            .filter(|active| active.session == *source)
            .and_then(|active| active.chat(stream_id))
            .filter(|chat| chat.direct)
        else {
            return;
        };
        let click = Preferences::read_for(cx, &source.owner).chat_click;
        cx.emit(SidebarAction::OpenChat {
            route: ChatRoute {
                owner: source.owner.clone(),
                scope: HistoryScope {
                    stream_id,
                    topic_id: chat_topic(chat, click),
                },
                entry: ChatEntry::Latest,
            },
            generation: source.generation,
        });
    }

    /// Сохраняет результат только для входа и операции, которые всё ещё показаны.
    fn send_catalog_command(
        &mut self,
        session: &AccountSession,
        key: RowKey,
        command: Command,
        dialog: Option<Entity<Status>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(runtime) = self.runtime.clone() else {
            return false;
        };
        let stream_id = key.chat_id();
        let Some(active) = self.active.as_mut().filter(|active| {
            &active.session == session && active.folder_online && active.catalog_pending.is_none()
        }) else {
            return false;
        };
        let Some(chat) = active.chat(stream_id) else {
            return false;
        };
        if let RowKey::Topic(_, id) = key
            && !chat.topics.iter().any(|topic| topic.id == id)
        {
            return false;
        }
        let operation = Uuid::new_v4();
        let selected = active.selected;
        active.catalog_pending = Some(operation);
        active.catalog_action_failed = false;
        cx.notify();
        let session = session.clone();
        let original = session.clone();
        cx.spawn_in(window, async move |view, cx| {
            log::debug!("gpui_sidebar op_id={operation} event=catalog_command_started");
            let result = runtime
                .request(move |services| async move {
                    let sync = &services.sync;
                    let messenger = &services.messenger;
                    let owner = &original.owner;
                    let generation = original.generation;
                    match command {
                        Command::Stream(action) => {
                            messenger
                                .update_sidebar_stream(sync, owner, generation, stream_id, action)
                                .await
                        }
                        Command::Notifications(mode) => messenger
                            .update_sidebar_notifications(sync, owner, generation, stream_id, mode)
                            .await
                            .map(|()| SidebarStreamResult::Updated),
                        Command::Topic(id, action) => messenger
                            .update_sidebar_topic(sync, owner, generation, stream_id, id, action)
                            .await
                            .map(|()| SidebarStreamResult::Updated),
                    }
                })
                .await;
            let success = matches!(&result, Ok(Ok(_)));
            log::debug!(
                "gpui_sidebar op_id={operation} event=catalog_command_finished success={success}"
            );
            let _ = cx.update(|window, app| {
                view.update(app, |view, cx| {
                    let Some(active) = view.active.as_mut().filter(|active| {
                        active.session == session && active.catalog_pending == Some(operation)
                    }) else {
                        return;
                    };
                    active.catalog_pending = None;
                    active.catalog_action_failed = dialog.is_none() && !success;
                    if let Some(dialog) = dialog
                        && let Some(host) =
                            view.action_panel.as_ref().and_then(|host| host.upgrade())
                    {
                        CatalogDialog::finished(&host, &dialog, success, window, cx);
                    }
                    if let Ok(Ok(SidebarStreamResult::TopicCreated(topic))) = result
                        && active.selected == selected
                    {
                        cx.emit(SidebarAction::OpenCreatedTopic {
                            session,
                            stream_id,
                            topic,
                        });
                    }
                    cx.notify();
                })
            });
        })
        .detach();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Использует одинаковый набор действий и точные отличия личного чата.
    #[test]
    fn personal_menu_shows_contact_and_personal_notification_choices() {
        let chat = Chat {
            id: Uuid::from_u128(1),
            name: "Пользователь".into(),
            direct: true,
            topics: Vec::new(),
            metadata: workspace_core::messenger::catalog::ChatMetadata {
                direct_peer_id: Some(Uuid::from_u128(2)),
                notification_mode: Some("mentions_only".into()),
                unread_count: Some(1),
                ..Default::default()
            },
        };
        let options = actions_for_chat(&chat, None, Language::En, true);
        let has = |action| {
            options
                .iter()
                .any(|entry| entry.action == RowAction::Catalog(action))
        };
        assert!(has(CatalogAction::Contact));
        assert!(has(CatalogAction::Archive));
        assert!(has(CatalogAction::NewTopic));
        assert!(has(CatalogAction::MarkRead));
        assert!(!has(CatalogAction::Rename));
        assert!(!has(CatalogAction::ChatNotifications(
            StreamNotificationMode::MentionsOnly
        )));
        assert!(options.iter().any(|entry| {
            entry.action
                == RowAction::Catalog(CatalogAction::ChatNotifications(
                    StreamNotificationMode::AllMessages,
                ))
                && !entry.enabled
                && entry.title.starts_with("✓")
        }));
    }

    /// В личной теме сохраняет действия над завершённой темой и показывает старое включение как подписку.
    #[test]
    fn personal_topic_menu_keeps_reopen_and_legacy_follow_selection() {
        let mut chat = Chat {
            id: Uuid::from_u128(1),
            name: "Чат".into(),
            direct: true,
            topics: Vec::new(),
            metadata: workspace_core::messenger::catalog::ChatMetadata {
                notification_mode: Some("muted".into()),
                ..Default::default()
            },
        };
        let topic = Topic {
            id: Uuid::from_u128(2),
            name: "Тема".into(),
            metadata: workspace_core::messenger::catalog::TopicMetadata {
                notification_mode: Some("unmute".into()),
                is_done: true,
                passive_unread_count: Some(1),
                ..Default::default()
            },
        };
        let options = actions_for_topic(&chat, &topic, Language::En, true);
        assert!(options.iter().any(|entry| {
            entry.action
                == RowAction::Catalog(CatalogAction::TopicNotifications(
                    TopicNotificationMode::Follow,
                ))
                && !entry.enabled
                && entry.title.starts_with("✓")
        }));
        assert!(!options.iter().any(|entry| entry.action
            == RowAction::Catalog(CatalogAction::TopicNotifications(
                TopicNotificationMode::Unmute
            ))));
        assert!(
            options
                .iter()
                .any(|entry| entry.action == RowAction::Catalog(CatalogAction::Rename))
        );
        assert!(
            options
                .iter()
                .any(|entry| entry.action == RowAction::Catalog(CatalogAction::MarkRead))
        );
        assert!(options.iter().any(|entry| entry.action
            == RowAction::Catalog(CatalogAction::ToggleDone)
            && entry.title == "Reopen topic"));

        chat.direct = false;
        assert!(
            actions_for_topic(&chat, &topic, Language::En, true)
                .iter()
                .any(|entry| {
                    entry.action
                        == RowAction::Catalog(CatalogAction::TopicNotifications(
                            TopicNotificationMode::Unmute,
                        ))
                        && !entry.enabled
                })
        );
    }
}
