//! Проверяет доставку цели баннера в то же окно и штатный переход оболочки.

#[cfg(feature = "gpui-test")]
mod gpui_tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{AppContext, Context, Render, TestAppContext, Window, div};
    use uuid::Uuid;
    use workspace_core::{
        auth::SessionStatus,
        context::{AccountId, OwnerContext, SessionGeneration},
        notification_target::NotificationTarget,
        storage::HistoryScope,
    };

    use super::super::{NotificationClick, apply_click, notification_text};
    #[cfg(target_os = "macos")]
    use super::super::bind_menu_labels;
    use crate::gpui_client::{
        contracts::{
            AccountSession, AccountSnapshot, ChatEntry, ChatRoute, NotificationReply, RuntimeEvent,
            ShellAction,
        },
        preferences::{Language, Preferences},
        shell::{Shell, ShellEffect},
    };

    /// Состояние рабочего окна, к которому подключается обработчик клика.
    struct TestWindow {
        shell: Shell, // Оболочка использует те же действия, что и рабочее окно.
    }

    impl Render for TestWindow {
        /// Создаёт область окна для проверки доставки события GPUI.
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            div()
        }
    }

    /// Готовит действующую локальную сессию для нужного аккаунта и поколения.
    fn account(account_id: &str, generation: u64) -> AccountSnapshot {
        AccountSnapshot {
            session: AccountSession {
                owner: OwnerContext {
                    account_id: AccountId(account_id.into()),
                    server: "https://workspace.test".into(),
                    organization_id: "organization".into(),
                    project_id: "project".into(),
                    user_id: account_id.into(),
                },
                generation: SessionGeneration(generation),
            },
            status: SessionStatus::Active,
            unread: None,
        }
    }

    /// Язык заголовка и подписи берётся у фонового аккаунта, для которого пришло сообщение.
    #[gpui::test]
    fn notification_text_uses_its_owner_language(cx: &mut TestAppContext) {
        let visible = account("visible", 1).session.owner;
        let background = account("background", 2).session.owner;
        cx.update(|app| {
            Preferences::select(app, Some(&background));
            Preferences::update(app, |settings| settings.language = Language::En).unwrap();
            Preferences::select(app, Some(&visible));

            assert_eq!(Preferences::read(app).language, Language::Ru);
            assert_eq!(
                notification_text(app, &background, "Workspace B"),
                ("New message", "Account Workspace B".to_owned())
            );
            assert_eq!(
                notification_text(app, &visible, "Workspace A"),
                ("Новое сообщение", "Аккаунт Workspace A".to_owned())
            );
        });
    }

    /// Подписи меню меняются при выборе языка и при переходе между двумя входами.
    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn menu_labels_follow_preferences_and_account_switch(cx: &mut TestAppContext) {
        let first = account("first", 1).session.owner;
        let second = account("second", 2).session.owner;
        let updates = Rc::new(RefCell::new(Vec::new()));
        cx.update(|app| {
            Preferences::select(app, Some(&first));
            let updates = updates.clone();
            bind_menu_labels(app, move |_, show, quit| {
                updates.borrow_mut().push((show, quit))
            });
        });
        cx.update(|app| Preferences::select(app, Some(&second)));
        cx.update(|app| Preferences::update(app, |value| value.language = Language::En).unwrap());
        cx.update(|app| Preferences::select(app, Some(&first)));
        cx.update(|app| Preferences::select(app, Some(&second)));
        assert_eq!(
            updates.borrow().as_slice(),
            &[
                ("Показать окно", "Выход"),
                ("Show window", "Quit"),
                ("Показать окно", "Выход"),
                ("Show window", "Quit"),
            ]
        );
    }

    /// Клик по цели из системного адаптера открывает проверенное сообщение B при выбранном A.
    #[gpui::test]
    fn notification_click_reaches_its_account_and_message_in_the_same_window(
        cx: &mut TestAppContext,
    ) {
        let a = account("a", 1);
        let b = account("b", 2);
        let stream_id = Uuid::from_u128(1);
        let message_id = Uuid::from_u128(2);
        let expected = ChatRoute {
            owner: b.session.owner.clone(),
            scope: HistoryScope {
                stream_id,
                topic_id: None,
            },
            entry: ChatEntry::Around(message_id),
        };
        let click = NotificationClick {
            generation: b.session.generation,
            target: NotificationTarget {
                context: b.session.owner.clone(),
                conversation_id: stream_id.to_string(),
                message_id: message_id.to_string(),
            },
        };

        let (view, cx) = cx.add_window_view(|_, _| {
            let mut shell = Shell::new(Some(a.session.owner.account_id.clone()));
            shell.handle_runtime(RuntimeEvent::Accounts(vec![a.clone(), b.clone()]));
            TestWindow { shell }
        });

        cx.update(|window, app| {
            let handle = window.window_handle();
            let view = view.clone();
            app.defer(move |app| {
                apply_click(app, handle, None, click, &mut |click, window, app| {
                    assert!(window.window_handle() == handle);
                    app.update_entity(&view, |view, _| {
                        assert_eq!(view.shell.selected(), Some(&a.session));
                        let Some(ShellEffect::ResolveNotification(request)) =
                            view.shell.handle_action(ShellAction::NotificationClicked(click))
                        else {
                            panic!("Клик не дошёл до проверки цели оболочкой");
                        };
                        assert_eq!(request.click.generation, b.session.generation);
                        assert_eq!(view.shell.selected(), Some(&a.session));
                        // Серверная проверка цели заменена готовым результатом для теста окна.
                        let effect = view.shell.handle_notification_reply(NotificationReply {
                            request,
                            result: Ok(expected.clone()),
                        });
                        assert!(matches!(
                            effect,
                            Some(ShellEffect::OpenConversation(address)) if address.route == expected
                        ));
                    })
                });
            });
        });
        cx.run_until_parked();
        cx.read(|app| {
            let view = view.read(app);
            assert_eq!(
                view.shell.conversation().map(|address| address.route.entry),
                Some(ChatEntry::Around(message_id))
            );
            assert_eq!(
                view.shell
                    .selected()
                    .map(|session| session.owner.account_id.0.as_str()),
                Some("b")
            );
        });
    }
}
