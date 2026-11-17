//! Проверяет открытие последних сообщений по клику, восстановление и смену аккаунта.

use super::*;
use tokio::sync::oneshot;

const ORIGINAL: &str = "Старое сообщение 20";
const AFTER_RESET: &str = "Сообщение после сброса 20";

/// Проверяет путь от клика по готовому каталогу до истории, сохранённой в SQLite.
#[tokio::test]
async fn chat_row_opens_latest_and_keeps_route_during_delayed_responses() {
    let mock = mock::MockWorkspace::start().await;
    let profile = tempfile::tempdir().unwrap();
    let (auth, foreground, background) = mock.accounts(profile.path()).await;
    let storage = Arc::new(Storage::open(profile.path().join("messenger.sqlite3")).unwrap());
    let sync = Arc::new(Sync::new(auth.clone(), storage.clone()));
    let messenger = Arc::new(mock.source());
    let (catalog, startup) = bootstrap::chat_catalog(messenger.catalog(storage));
    let runtime = Runtime::new(sync.clone(), vec![messenger.clone()]).unwrap();
    let (subscribed, ready) = oneshot::channel();
    let (stop, stopped) = oneshot::channel();
    let mut shell = Shell::with_notifications(
        auth,
        HashMap::from([
            (foreground.account_id.clone(), "Аккаунт A".into()),
            (background.account_id.clone(), "Аккаунт B".into()),
        ]),
        sync,
        (Arc::new(catalog), messenger),
        Some(background.account_id.clone()),
        subscribed,
        Box::<RecordedNotifications>::default(),
    );
    let mut events = futures::stream::select_all(
        into_recipes(shell.subscription())
            .into_iter()
            .map(|recipe| recipe.stream(Box::pin(futures::stream::pending()))),
    );
    timeout(DEADLINE, ready).await.unwrap().unwrap();
    let worker = tokio::spawn(bootstrap::run(
        runtime,
        Some(startup),
        async {
            let _ = stopped.await;
        },
        |restored| {
            restored.expect("Bootstrap не восстановил сохранённые аккаунты");
            Ok::<(), RuntimeError>(())
        },
    ));
    timeout(DEADLINE, async {
        while ![&foreground, &background].into_iter().all(|owner| {
            shell
                .chats
                .get(owner)
                .is_some_and(|list| list.status == ChatListStatus::Ready)
                && shell
                    .accounts
                    .summaries
                    .iter()
                    .any(|summary| summary.session.context == *owner && summary.unread.is_some())
        }) {
            next_update(
                &mut shell,
                &mut events,
                "каталоги и соединения обоих аккаунтов",
            )
            .await;
        }
    })
    .await
    .unwrap();

    let mut renderer = iced::Renderer::new(Font::DEFAULT, Pixels(16.0));
    sidebar_scenario(&mut shell, &mut renderer, &mock, &foreground, &background).await;
    let click = click_label(&shell, &mut renderer, "Алиса");
    let Message::OpenChat { route, generation } = &click else {
        panic!("Строка чата не открыла маршрут");
    };
    assert_eq!(route.owner, background);
    assert_eq!(route.entry, ChatEntry::Latest);

    let mut loading = into_stream(shell.update(click.clone())).unwrap();
    assert!(RenderedText::from_shell(&shell, &mut renderer).contains("Загрузка истории…"));
    let result = history_result(&mut loading).await;
    assert!(matches!(
        result,
        Message::HistoryLoaded { result: Ok(_), .. }
    ));
    let scroll = shell.update(result);
    assert_latest_position(&shell, &mut renderer, scroll).await;
    assert_stale_notification_keeps_history(&mut shell, &mut renderer, &background, *generation)
        .await;

    // Сервер снова подтвердил доступ, поэтому кэш можно показать до ошибки свежего запроса.
    let (arrived, release) = mock.pause_next_latest();
    mock.set_latest_failure(1);
    let mut reload = into_stream(shell.update(click.clone())).unwrap();
    let cached = history_result(&mut reload).await;
    assert!(matches!(cached, Message::HistoryCached { .. }));
    let scroll = shell.update(cached);
    assert_latest_position(&shell, &mut renderer, scroll).await;
    wait_for_rest_request(&mut reload, arrived).await;
    release.send(()).unwrap();
    let _ = shell.update(history_result(&mut reload).await);
    let rendered = RenderedText::from_shell(&shell, &mut renderer);
    assert!(rendered.contains(ORIGINAL));
    assert!(rendered.contains("Не удалось обновить историю"));

    // Отказ свежего запроса скрывает и уже показанный кэш.
    mock.set_latest_failure(2);
    let retry = click_label(&shell, &mut renderer, "Повторить");
    assert!(matches!(retry, Message::RetryHistory(_)));
    let mut denied = into_stream(shell.update(retry)).unwrap();
    let _ = shell.update(history_result(&mut denied).await);
    let _ = shell.update(history_result(&mut denied).await);
    assert_access_lost(&shell, &mut renderer, ORIGINAL);

    // Смена аккаунта во время запроса не даёт ответу из B изменить экран A.
    let (arrived, release) = mock.pause_next_latest();
    let mut delayed = into_stream(shell.update(click.clone())).unwrap();
    let _ = shell.update(history_result(&mut delayed).await);
    wait_for_rest_request(&mut delayed, arrived).await;
    let foreground_generation = shell
        .accounts
        .summaries
        .iter()
        .find(|summary| summary.session.context == foreground)
        .unwrap()
        .session
        .generation;
    let _ = shell.update(Message::Select(
        foreground.account_id.clone(),
        foreground_generation,
    ));
    release.send(()).unwrap();
    assert!(into_stream(shell.update(history_result(&mut delayed).await)).is_none());
    assert!(into_stream(shell.update(click.clone())).is_none());
    let rendered = RenderedText::from_shell(&shell, &mut renderer);
    assert!(rendered.contains("Канал A"));
    assert!(!rendered.contains(ORIGINAL));

    // Сброс B заново открывает Latest и удаляет строки старого окна.
    let _ = shell.update(Message::Select(background.account_id.clone(), *generation));
    let mut reopen = into_stream(shell.update(click.clone())).unwrap();
    let _ = shell.update(history_result(&mut reopen).await);
    let _ = shell.update(history_result(&mut reopen).await);
    mock.reject_cursor(false);
    let reset = owner_update(
        &mut events,
        &background,
        "сброс журнала B",
        |change| change == &SyncChange::MessengerReset,
    )
    .await;
    let mut after_reset = into_stream(shell.update(reset)).unwrap();
    assert!(!RenderedText::from_shell(&shell, &mut renderer).contains(ORIGINAL));
    let late_page = history_result(&mut after_reset).await;
    let _ = shell.update(late_page.clone());
    assert!(RenderedText::from_shell(&shell, &mut renderer).contains(AFTER_RESET));
    // После сброса Runtime читает новую начальную позицию. Ждём готовый сокет, чтобы отзыв
    // пришёл как новое событие, а не вошёл в начальную позицию следующего подключения.
    let baseline = owner_update(
        &mut events,
        &background,
        "готовность после сброса",
        |change| change == &SyncChange::Unread(7),
    )
    .await;
    let _ = shell.update(baseline);

    let mut rechecked = into_stream(shell.update(Message::SyncLagged)).unwrap();
    assert!(!RenderedText::from_shell(&shell, &mut renderer).contains(AFTER_RESET));
    let _ = shell.update(history_result(&mut rechecked).await);
    let late_page = history_result(&mut rechecked).await;
    let _ = shell.update(late_page.clone());
    assert!(RenderedText::from_shell(&shell, &mut renderer).contains(AFTER_RESET));

    // Событие об отзыве закрывает уже открытое окно и отбрасывает позднюю страницу.
    mock.send_history_change(
        "stream",
        "deleted",
        serde_json::json!({
            "kind": "stream.deleted", "uuid": route.scope.stream_id,
        }),
    );
    let revoked = owner_update(
        &mut events,
        &background,
        "отзыв доступа к чату",
        |change| matches!(change, SyncChange::HistoryAccessRevoked(_)),
    )
    .await;
    let _ = shell.update(revoked);
    assert!(into_stream(shell.update(late_page)).is_none());
    assert_access_lost(&shell, &mut renderer, AFTER_RESET);
    stop.send(()).unwrap();
    timeout(DEADLINE, worker).await.unwrap().unwrap().unwrap();
}

/// Проверяет локальный поиск, отдельное раскрытие каналов и доступ к теме через HTTPS.
async fn sidebar_scenario(
    shell: &mut Shell,
    renderer: &mut iced::Renderer,
    mock: &mock::MockWorkspace,
    foreground: &OwnerContext,
    background: &OwnerContext,
) {
    let render =
        |shell: &Shell, renderer: &mut iced::Renderer| RenderedText::from_shell(shell, renderer);
    let initial = render(shell, renderer);
    assert!(initial.contains("Канал B") && initial.contains("Алиса"));
    assert!(!initial.contains("Планы") && !initial.contains("Архив"));
    for channel in ["Канал B", "Новости"] {
        let action = click_label(shell, renderer, channel);
        assert!(matches!(
            action,
            Message::Sidebar {
                action: SidebarAction::ToggleChannel(_),
                ..
            }
        ));
        assert!(into_stream(shell.update(action)).is_none());
    }
    let expanded = render(shell, renderer);
    assert!(expanded.contains("Планы") && expanded.contains("Архив"));

    let a = enter_search(shell, renderer, "АЛИСА");
    let task = shell.update(a);
    let _ = run_history_task(shell, renderer, Cache::new(), task).await;
    let result = render(shell, renderer);
    assert!(result.contains("Алиса") && !result.contains("Канал B"));
    clear_search(shell, renderer).await;
    let miss = enter_search(shell, renderer, "нет совпадений");
    let _ = shell.update(miss);
    assert!(render(shell, renderer).contains("Ничего не найдено"));
    clear_search(shell, renderer).await;

    let plans = enter_search(shell, renderer, "ПЛАНЫ");
    let _ = shell.update(plans.clone());
    let result = render(shell, renderer);
    assert!(result.contains("Канал B") && result.contains("Планы"));
    assert!(!result.contains("Новости") && !result.contains("Алиса"));

    let topic = open_topic_with_keyboard(shell, renderer);
    let Message::OpenChat { route, .. } = &topic else {
        panic!("Тема не открыла маршрут")
    };
    assert!(route.scope.topic_id.is_some());
    let mut opened = into_stream(shell.update(topic.clone())).unwrap();
    let _ = shell.update(history_result(&mut opened).await);
    let result = render(shell, renderer);
    assert!(result.contains("Чаты и каналы") && result.contains("Сообщение в теме канала"));
    assert!(result.contains("Канал B"));

    clear_search(shell, renderer).await;
    let cleared = render(shell, renderer);
    assert!(cleared.contains("Планы") && cleared.contains("Архив"));
    mock.deny_channel_topic();
    let click = click_label(shell, renderer, "Планы");
    let mut denied = into_stream(shell.update(click)).unwrap();
    let _ = shell.update(history_result(&mut denied).await);
    let result = render(shell, renderer);
    assert!(result.contains("Тема недоступна") && !result.contains("Сообщение в теме канала"));

    select_account(shell, renderer, &foreground.account_id);
    assert!(into_stream(shell.update(plans)).is_none());
    assert!(into_stream(shell.update(topic.clone())).is_none());
    let result = render(shell, renderer);
    assert!(result.contains("Канал A") && result.contains("Борис"));
    assert!(!result.contains("Канал B") && !result.contains("Алиса"));
    select_account(shell, renderer, &background.account_id);
    let restored = render(shell, renderer);
    assert!(restored.contains("Планы") && restored.contains("Архив"));

    let original = shell.chats.clone();
    let mut empty = original.clone();
    empty.get_mut(background).unwrap().chats = Some(Arc::from([]));
    let _ = shell.update(Message::Chats(empty));
    assert!(render(shell, renderer).contains("У вас пока нет чатов"));
    assert!(into_stream(shell.update(topic)).is_none());
    let _ = shell.update(Message::Chats(original));
}

/// Переходит клавиатурой от поиска через кнопку очистки к единственной найденной теме.
fn open_topic_with_keyboard(shell: &Shell, renderer: &mut iced::Renderer) -> Message {
    use iced::advanced::widget::operation::{Outcome, focusable};
    use iced::keyboard::{
        Location, Modifiers,
        key::{NativeCode, Physical},
    };

    let generation = shell.accounts.current().unwrap().session.generation;
    let mut ui = build_ui(shell, renderer);
    ui.operate(
        renderer,
        &mut focusable::focus::<()>(sidebar::search_id(generation)),
    );
    for _ in 0..2 {
        let mut focus = focusable::focus_next::<()>();
        ui.operate(renderer, &mut focus);
        let Outcome::Chain(mut next) = focus.finish() else {
            panic!("Iced не передал фокус следующей кнопке");
        };
        ui.operate(renderer, next.as_mut());
    }
    let mut messages = Vec::new();
    let _ = ui.update(
        &[Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::Enter),
            modified_key: Key::Named(Named::Enter),
            physical_key: Physical::Unidentified(NativeCode::Unidentified),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            text: None,
            repeat: false,
        })],
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let [message @ Message::OpenChat { .. }] = messages.as_slice() else {
        panic!("Enter не открыл найденную тему: {messages:?}");
    };
    message.clone()
}

/// Отправляет текст в обычный SearchInput через событие клавиатуры Iced.
fn enter_search(shell: &Shell, renderer: &mut iced::Renderer, value: &str) -> Message {
    use iced::keyboard::{
        Location, Modifiers,
        key::{NativeCode, Physical},
    };
    let generation = shell.accounts.current().unwrap().session.generation;
    let mut ui = build_ui(shell, renderer);
    let mut focus =
        iced::advanced::widget::operation::focusable::focus::<()>(sidebar::search_id(generation));
    ui.operate(renderer, &mut focus);
    let mut messages = Vec::new();
    let keys: Vec<_> = value
        .chars()
        .map(|symbol| {
            let value = symbol.to_string();
            let key = Key::Character(value.as_str().into());
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key,
                physical_key: Physical::Unidentified(NativeCode::Unidentified),
                location: Location::Standard,
                modifiers: Modifiers::empty(),
                text: Some(value.as_str().into()),
                repeat: false,
            })
        })
        .collect();
    let _ = ui.update(
        &keys,
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let Some(
        message @ Message::Sidebar {
            action: SidebarAction::Search(query),
            ..
        },
    ) = messages.last()
    else {
        panic!("Поле поиска не передало введённый текст: {messages:?}");
    };
    assert_eq!(query, value);
    message.clone()
}

/// Нажимает кнопку очистки справа от поиска и выполняет возврат фокуса.
async fn clear_search(shell: &mut Shell, renderer: &mut iced::Renderer) {
    let generation = shell.accounts.current().unwrap().session.generation;
    let mut ui = build_ui(shell, renderer);
    let field = RenderedText::from_ui(&mut ui, renderer)
        .bounds(sidebar::search_id(generation))
        .unwrap();
    let messages = click_ui(
        &mut ui,
        renderer,
        Point::new(field.x + field.width - 14.0, field.center().y),
    );
    drop(ui);
    let [
        message @ Message::Sidebar {
            action: SidebarAction::ClearSearch,
            ..
        },
    ] = messages.as_slice()
    else {
        panic!("Кнопка очистки не закрыла поиск: {messages:?}");
    };
    let task = shell.update(message.clone());
    let _ = run_history_task(shell, renderer, Cache::new(), task).await;
}

/// Проверяет, что старый баннер сообщает об ошибке и сохраняет текущую историю.
async fn assert_stale_notification_keeps_history(
    shell: &mut Shell,
    renderer: &mut iced::Renderer,
    owner: &OwnerContext,
    generation: SessionGeneration,
) {
    let screen = shell.visible_history().unwrap();
    let revision = shell.navigation_revision;
    let mut actions = into_stream(
        shell.update(Message::NotificationClicked(NotificationClick {
            target: NotificationTarget {
                context: owner.clone(),
                conversation_id: screen.route.scope.stream_id.to_string(),
                message_id: Uuid::new_v4().to_string(),
            },
            generation,
        })),
    )
    .unwrap();
    assert!(RenderedText::from_shell(shell, renderer).contains(ORIGINAL));
    while let Some(action) = timeout(DEADLINE, actions.next()).await.unwrap() {
        match action {
            Action::Window(window::Action::GetOldest(reply)) => reply.send(None).unwrap(),
            Action::Output(message @ Message::NotificationResolved { .. }) => {
                let _ = shell.update(message);
            }
            action => panic!("Неожиданное действие для устаревшего баннера: {action:?}"),
        }
    }
    let rendered = RenderedText::from_shell(shell, renderer);
    assert!(rendered.contains(ORIGINAL));
    assert!(rendered.contains("Сообщение из уведомления недоступно"));
    assert_eq!(shell.navigation_revision, revision);
}

/// Нажимает настоящую кнопку по видимой текстовой подписи и возвращает её сообщение.
fn click_label(shell: &Shell, renderer: &mut iced::Renderer, label: &str) -> Message {
    let mut ui = build_ui(shell, renderer);
    let text = RenderedText::from_ui(&mut ui, renderer);
    let (bounds, _) = text
        .0
        .iter()
        .find(|(_, value)| value == label)
        .unwrap_or_else(|| panic!("Нет подписи {label}"));
    let messages = click_ui(&mut ui, renderer, bounds.center());
    let [message] = messages.as_slice() else {
        panic!("Клик по {label} вернул {messages:?}");
    };
    message.clone()
}

/// Читает следующее сообщение сетевой задачи истории с ограниченным ожиданием.
async fn history_result(
    actions: &mut (impl futures::Stream<Item = Action<Message>> + Unpin),
) -> Message {
    let Some(Action::Output(result)) = timeout(DEADLINE, actions.next()).await.unwrap() else {
        panic!("Загрузка истории не вернула сообщение");
    };
    result
}

/// Ждёт начала HTTP-запроса, пока поток команды Iced продолжает выполняться.
async fn wait_for_rest_request(
    actions: &mut (impl futures::Stream<Item = Action<Message>> + Unpin),
    arrived: oneshot::Receiver<()>,
) {
    timeout(DEADLINE, async {
        tokio::select! {
            message = actions.next() => panic!("HTTP ответил до снятия задержки: {message:?}"),
            result = arrived => result.expect("Запрос не дошёл до сервера"),
        }
    })
    .await
    .unwrap();
}

/// Находит нужное событие реальной подписки для конкретного аккаунта.
async fn owner_update(
    events: &mut (impl futures::Stream<Item = Message> + Unpin),
    owner: &OwnerContext,
    step: &str,
    matches_change: impl Fn(&SyncChange) -> bool,
) -> Message {
    timeout(DEADLINE, async {
        loop {
            let message = events.next().await.expect("Подписка завершилась");
            if matches!(&message, Message::Synced(update) if &update.context == owner && matches_change(&update.change)) {
                return message;
            }
        }
    }).await.unwrap_or_else(|_| panic!("Обновление истории не пришло: {step}"))
}

/// Проверяет, что Iced сдвинул список к последней строке.
async fn assert_latest_position(
    shell: &Shell,
    renderer: &mut iced::Renderer,
    scroll: Task<Message>,
) {
    let (cache, result) = run_history_task(shell, renderer, Cache::new(), scroll).await;
    assert!(result.is_none());
    let mut ui = UserInterface::build(shell.view(), Size::new(620.0, 360.0), cache, renderer);
    let mut viewport = HistoryViewport::new(shell.navigation_revision);
    ui.operate(renderer, &mut viewport);
    let (viewport, shift) = viewport.0.unwrap();
    let text = RenderedText::from_ui(&mut ui, renderer);
    let (bounds, _) = text.0.iter().find(|(_, value)| value == ORIGINAL).unwrap();
    assert!(shift > 0.0);
    assert!(viewport.contains(Point::new(bounds.center().x, bounds.center().y - shift)));
}

/// Проверяет, что при потере доступа старые сообщения исчезли.
fn assert_access_lost(shell: &Shell, renderer: &mut iced::Renderer, old_message: &str) {
    let denied = RenderedText::from_shell(shell, renderer);
    assert!(denied.contains("Чат недоступен"));
    assert!(!denied.contains(old_message));
}
