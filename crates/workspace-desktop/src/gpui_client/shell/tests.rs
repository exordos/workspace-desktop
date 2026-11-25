//! Проверяет действия оболочки, видимый контекст и привязку фоновых ответов к сессии.

use std::sync::Arc;

use workspace_core::{
    bootstrap::ChatListStatus,
    context::OwnerContext,
    messenger::{
        activity::ActivityMessagePreview,
        catalog::{Chat, Topic},
    },
    storage::HistoryTime,
    sync::HistoryRevocation,
};

use super::*;
use crate::gpui_client::contracts::ConversationReply;

const STREAM: Uuid = Uuid::from_u128(1);
const MESSAGE: Uuid = Uuid::from_u128(2);
const DIRECT_TOPIC: Uuid = Uuid::from_u128(9);

/// Открывает только доступную точную тему и игнорирует ссылку со старого экрана.
#[test]
fn reference_topic_checks_parent_and_conversation_revision() {
    let (mut shell, a, _) = setup();
    let Some(ShellEffect::OpenConversation(first)) =
        shell.handle_action(ShellAction::Sidebar(SidebarAction::OpenChat {
            route: route(&a.session, ChatEntry::Latest),
            generation: a.session.generation,
        }))
    else {
        panic!("первый разговор открыт")
    };
    let wrong = ConversationReference::Topic {
        stream_id: Some(Uuid::from_u128(11)),
        topic_id: DIRECT_TOPIC,
    };
    assert!(shell.open_conversation_reference(&first, wrong).is_none());
    let exact = ConversationReference::Topic {
        stream_id: None,
        topic_id: DIRECT_TOPIC,
    };
    let Some(ShellEffect::OpenConversation(second)) =
        shell.open_conversation_reference(&first, exact)
    else {
        panic!("ссылка ведёт к доступной теме")
    };
    assert_eq!(second.route.scope.topic_id, Some(DIRECT_TOPIC));
    assert!(
        shell
            .open_conversation_reference(&first, ConversationReference::Stream(STREAM))
            .is_none()
    );
}

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
    shell.restore_navigation(None, None, None).unwrap();
    assert!(matches!(
        shell.restore_navigation(Some(&selected), None, None),
        Ok(Some(ShellEffect::OpenConversation(address))) if address.route == selected
    ));
}

/// Сохраняет оформление «Избранного» и точный адрес после возврата в тему чата с собой.
#[test]
fn self_chat_topic_keeps_favorites_presentation_and_route() {
    let (mut shell, mut a, _) = setup();
    let user = Uuid::from_u128(25);
    a.session.owner.user_id = user.to_string();
    let catalog = direct_catalog(&a.session, Some(user));
    shell.handle_runtime(RuntimeEvent::Accounts(vec![a.clone()]));
    shell.handle_runtime(RuntimeEvent::Catalog(HashMap::from([(
        a.session.owner.clone(),
        catalog,
    )])));
    let mut selected = route(&a.session, ChatEntry::Latest);
    selected.scope.topic_id = Some(DIRECT_TOPIC);
    shell.handle_action(ShellAction::Sidebar(SidebarAction::OpenChat {
        route: selected.clone(),
        generation: a.session.generation,
    }));
    assert!(matches!(shell.activity(), Some(address) if address.page == ActivityPage::Favorites));
    assert!(matches!(
        shell.restore_navigation(Some(&selected), None, None),
        Ok(None)
    ));
    shell.restore_navigation(None, None, None).unwrap();
    assert!(matches!(
        shell.restore_navigation(Some(&selected), None, None),
        Ok(Some(ShellEffect::OpenConversation(address))) if address.route == selected
    ));
    assert!(matches!(shell.activity(), Some(address) if address.page == ActivityPage::Favorites));
}

/// Открывает отмеченное сообщение в той теме личного чата, где оно находится.
#[test]
fn activity_message_opens_its_exact_direct_topic() {
    let (mut shell, a, _) = setup();
    shell.handle_action(ShellAction::Sidebar(SidebarAction::OpenActivity {
        page: ActivityPage::Marked,
        session: a.session.clone(),
    }));
    let page = shell.activity().unwrap().clone();
    let message = ActivityMessage {
        id: MESSAGE,
        stream_id: STREAM,
        topic_id: DIRECT_TOPIC,
        author_id: Uuid::from_u128(24),
        author_name: None,
        created_at: HistoryTime::parse("2026-07-16T12:30:00Z").unwrap(),
        preview: ActivityMessagePreview::Text("Сообщение".into()),
        read: true,
    };
    let mut expected = route(&a.session, ChatEntry::Around(MESSAGE));
    expected.scope.topic_id = Some(DIRECT_TOPIC);
    assert!(matches!(
        shell.open_activity_message(&page, &message),
        Some(ShellEffect::OpenConversation(address)) if address.route == expected
    ));
}

/// Закрывает историю при переходе во входящие и отклоняет строку из старого показа.
#[test]
fn activity_targets_follow_the_visible_account_and_navigation() {
    let (mut shell, a, b) = setup();
    let open = |page, session: &AccountSession| {
        ShellAction::Sidebar(SidebarAction::OpenActivity {
            page,
            session: session.clone(),
        })
    };
    let first = open_sidebar(&mut shell, &a.session);
    assert!(matches!(
        shell.handle_action(open(ActivityPage::Inbox, &a.session)),
        Some(ShellEffect::CloseConversation)
    ));
    let inbox = shell.activity().unwrap().clone();
    assert!(shell.conversation().is_none());
    assert!(
        shell
            .handle_action(open(ActivityPage::Drafts, &b.session))
            .is_none()
    );
    assert_eq!(shell.activity(), Some(&inbox));

    shell.handle_action(ShellAction::SelectAccount(b.session.clone()));
    shell.handle_action(open(ActivityPage::Inbox, &b.session));
    assert!(
        shell
            .open_activity_target(&inbox, first.route.scope, ChatEntry::Latest)
            .is_none()
    );
    let active = shell.activity().unwrap().clone();
    let Some(ShellEffect::OpenConversation(new)) =
        shell.open_activity_target(&active, first.route.scope, ChatEntry::Latest)
    else {
        panic!("Входящие текущего входа не открыли разговор");
    };
    assert_eq!(new.route.owner, b.session.owner);
    assert_eq!(new.generation, b.session.generation);
    assert!(shell.activity().is_none());
    assert!(matches!(
        shell.restore_navigation(None, Some(ActivityPage::Inbox), None),
        Ok(Some(ShellEffect::CloseConversation))
    ));
    assert!(
        shell
            .activity()
            .is_some_and(|route| route.page == ActivityPage::Inbox)
    );
}

/// Позднее избранное не перехватывает новое намерение открыть уведомление.
#[test]
fn favorites_reply_cannot_override_new_navigation() {
    let (mut shell, a, _) = setup();
    shell.handle_action(ShellAction::Sidebar(SidebarAction::OpenActivity {
        page: ActivityPage::Favorites,
        session: a.session.clone(),
    }));
    let old = shell.activity().unwrap().clone();
    shell.handle_action(ShellAction::NotificationClicked(click(&a.session)));
    assert!(
        shell
            .open_favorites_target(&old, route(&a.session, ChatEntry::Latest).scope)
            .is_none()
    );

    shell.handle_action(ShellAction::Sidebar(SidebarAction::OpenActivity {
        page: ActivityPage::Favorites,
        session: a.session.clone(),
    }));
    let new = shell.activity().unwrap().clone();
    let scope = HistoryScope {
        stream_id: Uuid::from_u128(22),
        topic_id: None,
    };
    assert!(
        matches!(shell.open_favorites_target(&new, scope), Some(ShellEffect::OpenConversation(address)) if address.route.scope == scope)
    );
    assert!(
        shell
            .activity()
            .is_some_and(|address| address.page == ActivityPage::Favorites)
    );
}

/// Возвращает именно выбранный черновик, когда пользователь идёт назад и затем вперёд.
#[test]
fn draft_route_preserves_exact_uuid_in_navigation_history() {
    let (mut shell, a, _) = setup();
    shell.handle_action(ShellAction::Sidebar(SidebarAction::OpenActivity {
        page: ActivityPage::Drafts,
        session: a.session.clone(),
    }));
    let page = shell.activity().unwrap().clone();
    let draft = Uuid::from_u128(77);
    let mut selected = route(&a.session, ChatEntry::Latest);
    selected.scope.topic_id = Some(DIRECT_TOPIC);
    assert!(
        matches!(shell.open_activity_draft(&page, selected.scope, draft), Some(ShellEffect::OpenDraft(_, id)) if id == draft)
    );
    assert_eq!(shell.conversation_draft(), Some(draft));
    shell
        .restore_navigation(None, Some(ActivityPage::Drafts), None)
        .unwrap();
    assert_eq!(shell.conversation_draft(), None);
    assert!(
        matches!(shell.restore_navigation(Some(&selected), None, Some(draft)), Ok(Some(ShellEffect::OpenDraft(address, id))) if address.route == selected && id == draft)
    );
}

/// Подтверждённый новый канал открывается до снимка каталога. Поздний ответ входа отбрасывается.
#[test]
fn opens_confirmed_new_chat_only_in_its_active_session() {
    let (mut shell, a, b) = setup();
    let scope = HistoryScope {
        stream_id: Uuid::from_u128(150),
        topic_id: Some(Uuid::from_u128(151)),
    };
    assert!(matches!(
        shell.open_confirmed_chat(&a.session, scope, ChatEntry::Latest),
        Some(ShellEffect::OpenConversation(address)) if address.route.scope == scope
    ));
    shell.handle_action(ShellAction::SelectAccount(b.session.clone()));
    assert!(
        shell
            .open_confirmed_chat(&a.session, scope, ChatEntry::Latest)
            .is_none()
    );
    assert!(shell.conversation().is_none());
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

/// Общий номер перехода оставляет только последнее действие, сохраняя исходный экран до успеха.
#[test]
fn message_navigation_uses_the_same_revision_as_notifications_and_account_selection() {
    let (mut shell, a, b) = setup();
    let source = open_sidebar(&mut shell, &a.session);
    let old_click = request(&mut shell, &b.session);
    let first = shell.begin_message_navigation(&source).unwrap();
    let second = shell.begin_message_navigation(&source).unwrap();
    let destination = |revision| ConversationAddress {
        route: route(&a.session, ChatEntry::Around(MESSAGE)),
        generation: a.session.generation,
        revision,
    };
    assert!(!shell.is_message_navigation_current(&source, first));
    assert!(
        shell
            .finish_message_navigation(&source, destination(first))
            .is_none()
    );
    assert!(
        shell
            .handle_notification_reply(NotificationReply {
                request: old_click,
                result: Ok(route(&b.session, ChatEntry::Around(MESSAGE))),
            })
            .is_none()
    );
    assert_eq!(shell.conversation(), Some(&source));

    let newer_click = request(&mut shell, &b.session);
    assert!(
        shell
            .finish_message_navigation(&source, destination(second))
            .is_none()
    );
    let third = shell.begin_message_navigation(&source).unwrap();
    assert!(
        shell
            .handle_notification_reply(NotificationReply {
                request: newer_click,
                result: Ok(route(&b.session, ChatEntry::Around(MESSAGE))),
            })
            .is_none()
    );
    shell.handle_action(ShellAction::SelectAccount(b.session.clone()));
    assert!(
        shell
            .finish_message_navigation(&source, destination(third))
            .is_none()
    );
    assert_eq!(shell.selected(), Some(&b.session));
}

/// Ранее подтверждённая точка чтения открывается для проверки Core даже до обновления каталога.
#[test]
fn reading_navigation_reopens_a_resolved_scope_absent_from_catalog() {
    let (mut shell, a, _) = setup();
    let source = open_sidebar(&mut shell, &a.session);
    let revision = shell.begin_message_navigation(&source).unwrap();
    let mut resolved = route(&a.session, ChatEntry::Around(MESSAGE));
    resolved.scope.topic_id = Some(Uuid::from_u128(22));
    let destination = ConversationAddress {
        route: resolved.clone(),
        generation: a.session.generation,
        revision,
    };
    assert!(matches!(
        shell.finish_message_navigation(&source, destination),
        Some(ShellEffect::OpenConversation(_))
    ));
    open_sidebar(&mut shell, &a.session);
    assert!(
        shell
            .restore_navigation(Some(&resolved), None, None)
            .is_err()
    );
    assert!(matches!(shell.restore_reading_navigation(&resolved, None),
        Ok(Some(ShellEffect::OpenConversation(address))) if address.route == resolved));
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
