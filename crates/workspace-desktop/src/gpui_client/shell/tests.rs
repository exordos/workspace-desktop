//! Проверяет действия оболочки, видимый контекст и привязку фоновых ответов к сессии.

use std::sync::Arc;

use workspace_core::{
    bootstrap::ChatListStatus,
    context::OwnerContext,
    messenger::catalog::{Chat, Topic},
    sync::HistoryRevocation,
};

use super::*;
use crate::gpui_client::contracts::ConversationReply;

const STREAM: Uuid = Uuid::from_u128(1);
const MESSAGE: Uuid = Uuid::from_u128(2);
const DIRECT_TOPIC: Uuid = Uuid::from_u128(9);

/// Готовит сводку тестового входа с одним непрочитанным сообщением.
fn account(name: &str, generation: u64) -> AccountSnapshot {
    AccountSnapshot {
        session: AccountSession {
            owner: OwnerContext {
                account_id: AccountId(name.into()),
                server: "https://workspace.test".into(),
                organization_id: "organization".into(),
                project_id: "project".into(),
                user_id: name.into(),
            },
            generation: SessionGeneration(generation),
        },
        status: SessionStatus::Active,
        unread: Some(1),
    }
}

/// Создаёт адрес личного разговора для указанного тестового входа.
fn route(session: &AccountSession, entry: ChatEntry) -> ChatRoute {
    ChatRoute {
        owner: session.owner.clone(),
        scope: HistoryScope {
            stream_id: STREAM,
            topic_id: None,
        },
        entry,
    }
}

/// Готовит действие по нажатию баннера для указанного тестового входа.
fn click(session: &AccountSession) -> NotificationClick {
    NotificationClick {
        target: NotificationTarget {
            context: session.owner.clone(),
            conversation_id: STREAM.to_string(),
            message_id: MESSAGE.to_string(),
        },
        generation: session.generation,
    }
}

/// Наполняет оболочку двумя действующими входами и одним личным чатом в каждом.
fn setup() -> (Shell, AccountSnapshot, AccountSnapshot) {
    let a = account("a", 1);
    let b = account("b", 2);
    let mut shell = Shell::new(Some(a.session.owner.account_id.clone()));
    shell.handle_runtime(RuntimeEvent::Accounts(vec![b.clone(), a.clone()]));
    shell.handle_runtime(RuntimeEvent::Catalog(
        [&a, &b]
            .into_iter()
            .map(|account| {
                (
                    account.session.owner.clone(),
                    direct_catalog(&account.session, None),
                )
            })
            .collect(),
    ));
    (shell, a, b)
}

/// Даёт снимок личного чата с дополнительной темой и необязательным собеседником.
fn direct_catalog(session: &AccountSession, peer: Option<Uuid>) -> ChatList {
    ChatList {
        generation: session.generation,
        status: ChatListStatus::Ready,
        chats: Some(Arc::new([Chat {
            id: STREAM,
            name: "Тестовый чат".into(),
            direct: true,
            topics: vec![Topic {
                id: DIRECT_TOPIC,
                name: "Другая тема".into(),
                metadata: Default::default(),
            }],
            metadata: workspace_core::messenger::catalog::ChatMetadata {
                direct_peer_id: peer,
                ..Default::default()
            },
        }])),
    }
}

/// Имитирует выбор личного чата в каталоге и извлекает адрес видимой истории.
fn open_sidebar(shell: &mut Shell, session: &AccountSession) -> ConversationAddress {
    let action = ShellAction::Sidebar(SidebarAction::OpenChat {
        route: route(session, ChatEntry::Latest),
        generation: session.generation,
    });
    match shell.handle_action(action) {
        Some(ShellEffect::OpenConversation(address)) => address,
        _ => panic!("Клик по личному чату не открыл историю"),
    }
}

/// Открывает только существующую тему личного чата и восстанавливает точный адрес.
#[test]
fn opens_and_restores_a_direct_topic_from_the_catalog() {
    let (mut shell, a, _) = setup();
    let mut selected = route(&a.session, ChatEntry::Latest);
    selected.scope.topic_id = Some(DIRECT_TOPIC);
    let mut missing = selected.clone();
    missing.scope.topic_id = Some(Uuid::from_u128(100));
    let open = |route| {
        ShellAction::Sidebar(SidebarAction::OpenChat {
            route,
            generation: a.session.generation,
        })
    };
    assert!(matches!(
        shell.handle_action(open(selected.clone())),
        Some(ShellEffect::OpenConversation(address)) if address.route == selected
    ));
    assert!(shell.handle_action(open(missing)).is_none());
    shell.restore_navigation(None).unwrap();
    assert!(matches!(
        shell.restore_navigation(Some(&selected)),
        Ok(Some(ShellEffect::OpenConversation(address))) if address.route == selected
    ));
}

/// При открытии темы оболочка использует тот же актуальный снимок, что и сайдбар.
#[test]
fn sidebar_route_uses_live_catalog_before_folder_cache() {
    let (mut shell, a, _) = setup();
    let session = &a.session;
    let previous = Uuid::from_u128(3);
    let fresh = Uuid::from_u128(4);
    let channel = |id| Chat {
        id: STREAM,
        name: "Канал".into(),
        direct: false,
        topics: vec![Topic {
            id,
            name: "Тема".into(),
            metadata: Default::default(),
        }],
        metadata: Default::default(),
    };
    let action = |id| {
        ShellAction::Sidebar(SidebarAction::OpenChat {
            route: ChatRoute {
                scope: HistoryScope {
                    stream_id: STREAM,
                    topic_id: Some(id),
                },
                ..route(session, ChatEntry::Latest)
            },
            generation: session.generation,
        })
    };
    let publish = |shell: &mut Shell, chats| {
        shell.handle_runtime(RuntimeEvent::Catalog(HashMap::from([(
            session.owner.clone(),
            ChatList {
                generation: session.generation,
                chats,
                status: ChatListStatus::Ready,
            },
        )])));
    };
    let publish_folders = |shell: &mut Shell, generation| {
        shell.handle_runtime(RuntimeEvent::Folders(HashMap::from([(
            session.owner.clone(),
            Arc::new(FolderSnapshot {
                owner: session.owner.clone(),
                generation,
                folders: Some(Vec::new()),
                chats: vec![channel(previous)],
                all_chats_unread: None,
                needs_refresh: false,
            }),
        )])));
    };

    publish(&mut shell, None);
    publish_folders(&mut shell, SessionGeneration(session.generation.0 + 1));
    assert!(shell.handle_action(action(previous)).is_none());
    publish_folders(&mut shell, session.generation);
    assert!(matches!(
        shell.handle_action(action(previous)),
        Some(ShellEffect::OpenConversation(_))
    ));

    publish(&mut shell, Some(vec![channel(fresh)].into()));
    assert!(shell.handle_action(action(previous)).is_none());
    assert!(matches!(
        shell.handle_action(action(fresh)),
        Some(ShellEffect::OpenConversation(address)) if address.route.scope.topic_id == Some(fresh)
    ));

    publish(&mut shell, Some(Vec::<Chat>::new().into()));
    assert!(shell.handle_action(action(previous)).is_none());
    assert!(shell.handle_action(action(fresh)).is_none());
}

/// Передаёт событие синхронизации с владельцем и поколением соответствующего входа.
fn sync(shell: &mut Shell, session: &AccountSession, change: SyncChange) -> Option<ShellEffect> {
    shell.handle_runtime(RuntimeEvent::Sync(SyncUpdate {
        context: session.owner.clone(),
        generation: session.generation,
        change,
    }))
}

/// Нажимает баннер и извлекает запрос, который оболочка передаёт на рабочий поток.
fn request(shell: &mut Shell, session: &AccountSession) -> NotificationRequest {
    match shell.handle_action(ShellAction::NotificationClicked(click(session))) {
        Some(ShellEffect::ResolveNotification(request)) => request,
        _ => panic!("Действующая сессия должна разрешить проверку баннера"),
    }
}

/// Читает счётчик, который переключатель получает для отдельного входа.
fn unread(shell: &Shell, session: &AccountSession) -> Option<u32> {
    shell
        .accounts()
        .iter()
        .find(|a| a.session == *session)?
        .unread
}

/// Фоновый счётчик B растёт, а переход к B происходит только после проверенного клика.
#[test]
fn background_badge_and_valid_notification_preserve_context_until_confirmed() {
    let (mut shell, a, b) = setup();
    let route_a = open_sidebar(&mut shell, &a.session);
    sync(&mut shell, &b.session, SyncChange::Unread(5));
    assert_eq!(unread(&shell, &b.session), Some(5));
    assert_eq!(unread(&shell, &a.session), Some(1));
    assert_eq!(shell.selected(), Some(&a.session));
    assert_eq!(shell.conversation(), Some(&route_a));

    let banner = sync(
        &mut shell,
        &b.session,
        SyncChange::IncomingDirectMessage {
            conversation_id: STREAM.to_string(),
            message_id: MESSAGE.to_string(),
        },
    );
    assert!(
        matches!(banner, Some(ShellEffect::ShowNotification(banner)) if banner == click(&b.session))
    );
    let request = request(&mut shell, &b.session);
    assert_eq!(shell.conversation(), Some(&route_a));

    let expected = route(&b.session, ChatEntry::Around(MESSAGE));
    let effect = shell.handle_notification_reply(NotificationReply {
        request,
        result: Ok(expected.clone()),
    });
    assert!(
        matches!(effect, Some(ShellEffect::OpenConversation(address))
        if address.route == expected && address.generation == b.session.generation)
    );
    assert_eq!(shell.selected(), Some(&b.session));
    assert_eq!(
        shell.conversation().map(|address| &address.route),
        Some(&expected)
    );
}

/// Быстрый возврат в тот же чат исключает старые ответы истории и уведомления.
#[test]
fn fast_return_to_same_chat_rejects_previous_navigation_and_generation() {
    let (mut shell, a, b) = setup();
    let initial = open_sidebar(&mut shell, &a.session);
    let old_click = request(&mut shell, &b.session);

    assert!(matches!(
        shell.handle_action(ShellAction::SelectAccount(b.session.clone())),
        Some(ShellEffect::CloseConversation)
    ));
    assert!(shell.conversation().is_none());
    shell.handle_action(ShellAction::SelectAccount(a.session.clone()));
    let current = open_sidebar(&mut shell, &a.session);

    assert_eq!(initial.route, current.route);
    assert_ne!(initial.revision, current.revision);
    assert_eq!(
        ConversationReply {
            address: initial,
            value: ()
        }
        .for_current(shell.conversation()),
        None
    );
    shell.handle_notification_reply(NotificationReply {
        request: old_click,
        result: Ok(route(&b.session, ChatEntry::Around(MESSAGE))),
    });
    assert_eq!(shell.conversation(), Some(&current));

    let before_login = request(&mut shell, &b.session);
    let new_b = account("b", 3);
    shell.handle_runtime(RuntimeEvent::Accounts(vec![a, new_b.clone()]));
    sync(&mut shell, &b.session, SyncChange::Unread(99));
    assert_eq!(unread(&shell, &new_b.session), Some(1));
    assert!(
        shell
            .handle_notification_reply(NotificationReply {
                request: before_login,
                result: Err(NotificationRouteError::InactiveSession),
            })
            .is_none()
    );
    assert_eq!(shell.route_error(), None);
    assert_eq!(shell.conversation(), Some(&current));
}

/// Ошибка ожидаемого клика остаётся видимой, а подменённая цель его не завершает.
#[test]
fn invalid_notification_reports_error_without_replacing_open_conversation() {
    let (mut shell, a, b) = setup();
    let route_a = open_sidebar(&mut shell, &a.session);
    let pending = request(&mut shell, &b.session);
    let mut tampered = pending.clone();
    tampered.click.target.context.project_id = "чужой проект".into();
    shell.handle_notification_reply(NotificationReply {
        request: tampered,
        result: Err(NotificationRouteError::LookupFailed),
    });
    assert_eq!(shell.route_error(), None);

    let mut wrong_route = route(&b.session, ChatEntry::Around(MESSAGE));
    wrong_route.owner.project_id = "чужой проект".into();
    shell.handle_notification_reply(NotificationReply {
        request: pending,
        result: Ok(wrong_route),
    });
    assert_eq!(
        shell.route_error(),
        Some(NotificationRouteError::TargetUnavailable)
    );
    assert_eq!(shell.conversation(), Some(&route_a));
    assert_eq!(shell.selected(), Some(&a.session));

    let pending = request(&mut shell, &b.session);
    shell.handle_notification_reply(NotificationReply {
        request: pending,
        result: Err(NotificationRouteError::LookupFailed),
    });
    assert_eq!(
        shell.route_error(),
        Some(NotificationRouteError::LookupFailed)
    );
    assert_eq!(shell.conversation(), Some(&route_a));
}

/// Изменения истории идут только в текущий вход и адрес, а при пропуске запрашивается перечитывание.
#[test]
fn forwards_history_events_only_for_current_session() {
    let (mut shell, a, b) = setup();
    let active = open_sidebar(&mut shell, &a.session);
    let revocation = SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(STREAM));
    assert!(sync(&mut shell, &b.session, revocation.clone()).is_none());

    let mut old = a.session.clone();
    old.generation = SessionGeneration(0);
    assert!(sync(&mut shell, &old, revocation.clone()).is_none());
    assert!(matches!(sync(&mut shell, &a.session, revocation),
        Some(ShellEffect::ConversationUpdated(address, _)) if address == active));

    assert!(matches!(shell.handle_runtime(RuntimeEvent::SyncLagged),
        Some(ShellEffect::ReloadConversation(address)) if address == active));

    assert!(
        matches!(sync(&mut shell, &a.session, SyncChange::MessengerReset),
        Some(ShellEffect::ConversationUpdated(address, _)) if address == active)
    );
    assert_eq!(unread(&shell, &a.session), None);
}

/// Устаревший каталог и выбравший его клик не открывают историю после повторного входа.
#[test]
fn new_session_clears_route_and_hides_previous_catalog() {
    let (mut shell, a, b) = setup();
    open_sidebar(&mut shell, &a.session);
    let new_a = account("a", 4);
    assert!(matches!(
        shell.handle_runtime(RuntimeEvent::Accounts(vec![b, new_a.clone()])),
        Some(ShellEffect::CloseConversation)
    ));
    assert_eq!(shell.selected(), Some(&new_a.session));
    assert!(shell.selected_catalog().is_none());
    assert!(
        shell
            .handle_action(ShellAction::Sidebar(SidebarAction::OpenChat {
                route: route(&a.session, ChatEntry::Latest),
                generation: a.session.generation,
            }))
            .is_none()
    );
    assert!(shell.conversation().is_none());
}
