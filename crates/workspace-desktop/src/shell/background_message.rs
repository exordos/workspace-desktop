//! Проверяет путь входящего сообщения через сеть, SQLite и оболочку Iced.
//! Сценарий получает тестовые аккаунты и отправляет сообщение через адаптер среды.

mod live;
mod mock;

use std::{path::Path, time::Duration};

use futures::StreamExt;
use iced::advanced::subscription::into_recipes;
use iced::advanced::widget::{Id, Operation};
use iced::{Font, Pixels, Rectangle, Size, mouse};
use iced_runtime::Action;
use iced_runtime::core::clipboard;
use iced_runtime::task::into_stream;
use iced_runtime::user_interface::{Cache, UserInterface};
use tokio::time::timeout;
use uuid::Uuid;
use workspace_core::auth::{SecretStore, StoreError};
use workspace_core::bootstrap;
use workspace_core::messenger::MessengerSource;
use workspace_core::runtime::{Runtime, RuntimeError};
use workspace_core::storage::Storage;

use super::*;

const DEADLINE: Duration = Duration::from_secs(10);

/// Даёт сценарию сессии, сетевой источник и способ отправить сообщение.
trait TestWorkspace {
    /// Возвращает авторизацию для двух аккаунтов, начальный выбор и получателя.
    async fn accounts(&self, profile: &Path) -> (Auth, OwnerContext, OwnerContext);

    /// Создаёт штатный сетевой источник с настройками доверия для этой среды.
    fn source(&self) -> MessengerSource;

    /// Отправляет сообщение от первого аккаунта второму и возвращает идентификаторы чата и сообщения.
    async fn send_direct_message(&self, body: &str) -> (String, String);
}

/// Изолирует тестовые токены от системной связки ключей.
#[derive(Default)]
struct MemorySecrets(Mutex<HashMap<String, String>>); // Токены по ключам временного профиля.

impl SecretStore for MemorySecrets {
    /// Читает токен из памяти текущего прогона.
    fn load(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }

    /// Сохраняет токен на время прогона.
    fn save(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.0
            .lock()
            .unwrap()
            .insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    /// Удаляет токен из временного профиля.
    fn remove(&self, key: &str) -> Result<(), StoreError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// Запоминает запросы Iced на баннер для проверки без системного центра уведомлений.
#[derive(Clone, Default)]
struct RecordedNotifications(Arc<Mutex<Vec<(String, NotificationClick)>>>); // Запросы с целями в порядке получения.

impl NotificationDelivery for RecordedNotifications {
    /// Разрешает уведомления в тестовом приёмнике.
    fn prepare(&self) -> BoxFuture<'static, Result<(), NotificationError>> {
        Box::pin(async { Ok(()) })
    }

    /// Записывает идентификатор каждого запроса, включая возможные дубли.
    fn show(
        &self,
        id: &str,
        _: &str,
        _: &str,
        click: &NotificationClick,
    ) -> BoxFuture<'static, Result<(), NotificationError>> {
        self.0.lock().unwrap().push((id.to_owned(), click.clone()));
        Box::pin(async { Ok(()) })
    }
}

/// Считывает текст и расположение виджетов из разметки настоящего окна Iced.
#[derive(Default)]
struct RenderedText(Vec<(Rectangle, String)>); // Границы и текст видимых элементов оболочки.

impl Operation for RenderedText {
    /// Обходит дочерние виджеты, включая содержимое строк аккаунтов.
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    /// Запоминает подпись и её положение в окне.
    fn text(&mut self, _: Option<&Id>, bounds: Rectangle, value: &str) {
        self.0.push((bounds, value.to_owned()));
    }
}

impl RenderedText {
    /// Проверяет, что окно показывает указанную подпись.
    fn contains(&self, label: &str) -> bool {
        self.0.iter().any(|(_, text)| text == label)
    }

    /// Строит разметку оболочки программным рендерером без открытия окна.
    fn from_shell(shell: &Shell, renderer: &mut iced::Renderer) -> Self {
        let mut ui = UserInterface::build(
            shell.view(),
            Size::new(620.0, 360.0),
            Cache::new(),
            renderer,
        );
        let mut text = Self::default();
        ui.operate(renderer, &mut text);
        text
    }

    /// Возвращает число справа от подписи аккаунта. Пустой бейдж означает ноль.
    fn badge_count(&self, account: &str) -> u32 {
        let (label, _) = self.0.iter().find(|(_, text)| text == account).unwrap();
        self.0
            .iter()
            .find_map(|(bounds, text)| {
                let center = bounds.y + bounds.height / 2.0;
                (bounds.x > label.x && center >= label.y && center <= label.y + label.height)
                    .then(|| text.parse().ok())
                    .flatten()
            })
            .unwrap_or(0)
    }

    /// Проверяет подпись открытого аккаунта в видимом содержимом.
    fn shows_selected(&self, account: &str) -> bool {
        let prefix = format!("Открыт аккаунт {account} ·");
        self.0.iter().any(|(_, text)| text.starts_with(&prefix))
    }
}

/// Применяет полученные Iced сообщения и выполняет задачи, созданные оболочкой.
async fn update_shell(shell: &mut Shell, message: Message) {
    if let Some(mut actions) = into_stream(shell.update(message)) {
        while let Some(action) = actions.next().await {
            let Action::Output(message) = action else {
                panic!("ожидался результат запроса на уведомление");
            };
            assert!(matches!(message, Message::NotificationFinished(Ok(()))));
            assert!(into_stream(shell.update(message)).is_none());
        }
    }
}

/// Доставляет одно сообщение из настоящей подписки и сообщает, на каком шаге истекло ожидание.
async fn next_update(
    shell: &mut Shell,
    events: &mut (impl futures::Stream<Item = Message> + Unpin),
    step: &str,
) {
    let message = timeout(DEADLINE, events.next())
        .await
        .unwrap_or_else(|_| panic!("истекло ожидание: {step}"))
        .expect("подписка Iced закрылась");
    update_shell(shell, message).await;
}

/// Проверяет, что после отказа по WSS история и счётчик восстанавливаются только у B.
#[tokio::test]
async fn local_wss_recovers_pruned_cursor_and_changed_epoch() {
    use workspace_core::messenger::SOURCE_ID;
    use workspace_core::storage::Message as StoredMessage;
    use workspace_core::sync::Event;

    for change_epoch in [false, true] {
        let workspace = mock::MockWorkspace::start().await;
        let profile = tempfile::tempdir().unwrap();
        let (auth, foreground, background) = workspace.accounts(profile.path()).await;
        auth.restore().await.unwrap();
        let storage = Arc::new(Storage::open(profile.path().join("messenger.sqlite3")).unwrap());
        let sync = Arc::new(Sync::new(auth.clone(), storage.clone()));
        let runtime = Runtime::new(sync.clone(), vec![Arc::new(workspace.source())]).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let worker = tokio::spawn(async move {
            runtime
                .run(async {
                    let _ = stopped.await;
                })
                .await
        });

        timeout(DEADLINE, async {
            while storage.unread(&foreground).unwrap().is_none()
                || storage.unread(&background).unwrap().is_none()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("оба начальных сокета должны быть готовы");
        let old_a = storage.cursor(&foreground, SOURCE_ID).unwrap().unwrap();
        let old_b = storage.cursor(&background, SOURCE_ID).unwrap().unwrap();
        let generation = auth
            .snapshot()
            .into_iter()
            .find(|session| session.context == foreground)
            .unwrap()
            .generation;
        let mut cursor_a = old_a.clone();
        cursor_a.epoch_version += 1;
        sync.apply(
            &foreground,
            generation,
            SOURCE_ID,
            Event::MessageCreated {
                cursor: cursor_a.clone(),
                message: StoredMessage {
                    id: "foreground".into(),
                    text: "данные A".into(),
                },
                placement: None,
                metadata: None,
            },
        )
        .unwrap();

        let (_, old_id) = workspace
            .send_direct_message("сообщение из старого кэша")
            .await;
        timeout(DEADLINE, async {
            while storage.message(&background, &old_id).unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("старое сообщение должно попасть в SQLite");

        workspace.reject_cursor(change_epoch);
        timeout(DEADLINE, async {
            loop {
                let cursor = storage.cursor(&background, SOURCE_ID).unwrap();
                if storage.unread(&background).unwrap() == Some(7)
                    && cursor.is_some_and(|cursor| {
                        if change_epoch {
                            cursor.epoch_generation != old_b.epoch_generation
                                && cursor.epoch_version == 1
                        } else {
                            cursor.epoch_generation == old_b.epoch_generation
                                && cursor.epoch_version == 50
                        }
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("B должен подключиться с новой позиции и получить счётчик 7");
        assert!(storage.message(&background, &old_id).unwrap().is_none());
        assert_eq!(
            storage.cursor(&foreground, SOURCE_ID).unwrap(),
            Some(cursor_a)
        );
        assert_eq!(storage.unread(&foreground).unwrap(), Some(4));
        assert!(
            storage
                .message(&foreground, "foreground")
                .unwrap()
                .is_some()
        );

        let (stream_id, new_id) = workspace
            .send_direct_message("сообщение после восстановления")
            .await;
        timeout(DEADLINE, async {
            while storage.message(&background, &new_id).unwrap().is_none()
                || storage.unread(&background).unwrap() != Some(8)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("восстановленный WSS должен принять сообщение и обновить счётчик");
        if change_epoch {
            let edit = workspace.send_history_change(
                "message",
                "updated",
                serde_json::json!({
                    "kind": "message.updated", "uuid": new_id,
                    "payload": {"kind": "markdown", "content": "исправленный текст"}
                }),
            );
            wait_for_position(&storage, &background, edit).await;
            assert_eq!(
                storage.message(&background, &new_id).unwrap().unwrap().text,
                "исправленный текст"
            );

            let (_, remaining) = workspace
                .send_direct_message("другое сообщение в том же стриме")
                .await;
            let deletion = workspace.send_history_change(
                "message",
                "deleted",
                serde_json::json!({
                    "kind": "message.deleted", "uuid": new_id
                }),
            );
            wait_for_position(&storage, &background, deletion).await;
            assert!(storage.message(&background, &new_id).unwrap().is_none());
            assert!(storage.message(&background, &remaining).unwrap().is_some());

            let revoke = workspace.send_history_change(
                "stream",
                "deleted",
                serde_json::json!({
                    "kind": "stream.deleted", "uuid": stream_id
                }),
            );
            wait_for_position(&storage, &background, revoke).await;
            assert!(storage.message(&background, &remaining).unwrap().is_none());
            assert!(
                storage
                    .message(&foreground, "foreground")
                    .unwrap()
                    .is_some()
            );
        }
        stop.send(()).unwrap();
        timeout(DEADLINE, worker).await.unwrap().unwrap().unwrap();
    }
}

/// Ждёт фиксацию нужной позиции журнала в SQLite.
async fn wait_for_position(storage: &Storage, owner: &OwnerContext, version: u64) {
    timeout(DEADLINE, async {
        while storage
            .cursor(owner, workspace_core::messenger::SOURCE_ID)
            .unwrap()
            .is_none_or(|cursor| cursor.epoch_version < version)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("позиция {version} должна попасть в SQLite"));
}

/// Прогоняет тот же сценарий для любой среды, предоставляющей два аккаунта и личный чат.
async fn background_message_scenario(workspace: &impl TestWorkspace, check_catalog_retry: bool) {
    let profile = tempfile::tempdir().unwrap();
    let (auth, foreground, background) = timeout(DEADLINE, workspace.accounts(profile.path()))
        .await
        .expect("адаптер не подготовил тестовые аккаунты");
    let storage = Arc::new(Storage::open(profile.path().join("messenger.sqlite3")).unwrap());
    let sync = Arc::new(Sync::new(auth.clone(), storage.clone()));
    let messenger = Arc::new(workspace.source());
    let (catalog, catalog_startup) = bootstrap::chat_catalog(messenger.catalog(storage.clone()));
    let runtime = Runtime::new(sync.clone(), vec![messenger]).unwrap();
    let (subscribed, ready) = oneshot::channel();
    let (stop, stopped) = oneshot::channel::<()>();
    let notifications = RecordedNotifications::default();
    let mut shell = Shell::with_notifications(
        auth.clone(),
        HashMap::from([
            (foreground.account_id.clone(), "Аккаунт A".to_owned()),
            (background.account_id.clone(), "Аккаунт B".to_owned()),
        ]),
        sync.clone(),
        Arc::new(catalog),
        Some(foreground.account_id.clone()),
        subscribed,
        Box::new(notifications.clone()),
    );
    let mut events = futures::stream::select_all(
        into_recipes(shell.subscription())
            .into_iter()
            .map(|recipe| recipe.stream(Box::pin(futures::stream::pending()))),
    );
    timeout(DEADLINE, ready).await.unwrap().unwrap();
    let worker = tokio::spawn(bootstrap::run(
        runtime,
        Some(catalog_startup),
        async {
            let _ = stopped.await;
        },
        |restored| {
            restored.expect("Bootstrap не восстановил сохранённые аккаунты");
            Ok::<(), RuntimeError>(())
        },
    ));

    timeout(DEADLINE, async {
        while ![&foreground, &background].iter().all(|owner| {
            shell
                .accounts
                .summaries
                .iter()
                .any(|summary| summary.session.context == **owner && summary.unread.is_some())
        }) {
            next_update(
                &mut shell,
                &mut events,
                "начальные счётчики обоих аккаунтов",
            )
            .await;
        }
    })
    .await
    .expect("оболочка не получила оба начальных счётчика");

    let mut renderer = iced::Renderer::new(Font::DEFAULT, Pixels(16.0));
    if check_catalog_retry {
        timeout(DEADLINE, async {
            while !shell
                .chats
                .get(&foreground)
                .is_some_and(|list| list.status == ChatListStatus::Failed)
                || !shell
                    .chats
                    .get(&background)
                    .is_some_and(|list| list.status == ChatListStatus::Ready)
            {
                next_update(
                    &mut shell,
                    &mut events,
                    "ошибка каталога A и готовность каталога B",
                )
                .await;
            }
        })
        .await
        .expect("Bootstrap не завершил попытку для обоих аккаунтов");
        let failed = RenderedText::from_shell(&shell, &mut renderer);
        assert!(failed.contains("Не удалось загрузить чаты"));
        assert!(failed.contains("Повторить"));
        let _ = shell.update(Message::Select(background.account_id.clone()));
        let other = RenderedText::from_shell(&shell, &mut renderer);
        assert!(other.contains("Канал B"));
        assert!(other.contains("Алиса"));
        assert!(!other.contains("Повторить"));
        let _ = shell.update(Message::Select(foreground.account_id.clone()));
        let mut ui = UserInterface::build(
            shell.view(),
            Size::new(620.0, 360.0),
            Cache::new(),
            &mut renderer,
        );
        let (bounds, _) = failed
            .0
            .iter()
            .find(|(_, label)| label == "Повторить")
            .unwrap();
        let mut messages = Vec::new();
        let _ = ui.update(
            &[
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ],
            mouse::Cursor::Available(bounds.center()),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        drop(ui);
        assert!(matches!(messages.as_slice(), [Message::RetryChats]));
        let _ = shell.update(messages.remove(0));
        timeout(DEADLINE, async {
            while !shell
                .chats
                .get(&foreground)
                .is_some_and(|list| list.status == ChatListStatus::Ready)
            {
                next_update(&mut shell, &mut events, "загрузка каталога A после повтора").await;
            }
        })
        .await
        .expect("список чатов A не загрузился после повтора");
        let loaded = RenderedText::from_shell(&shell, &mut renderer);
        assert!(loaded.contains("Канал A"));
        assert!(loaded.contains("Борис"));
        assert!(loaded.contains("# Планы"));
        assert!(!loaded.contains("Канал B"));
        assert!(!loaded.contains("Повторить"));
    }
    let before = RenderedText::from_shell(&shell, &mut renderer);
    assert!(before.shows_selected("Аккаунт A"));
    let foreground_count = before.badge_count("Аккаунт A");
    let previous_count = before.badge_count("Аккаунт B");
    assert_eq!(sync.unread(&foreground).unwrap(), Some(foreground_count));
    assert_eq!(sync.unread(&background).unwrap(), Some(previous_count));
    let body = format!("Workspace Desktop integration test {}", Uuid::new_v4());
    let (conversation_id, message_id) = timeout(DEADLINE, workspace.send_direct_message(&body))
        .await
        .expect("сервер не принял тестовое сообщение");
    let expected_notification = format!(
        "{}/{}/{}",
        background.account_id.0, background.project_id, message_id
    );

    timeout(DEADLINE, async {
        loop {
            next_update(&mut shell, &mut events, "новый бейдж и уведомление").await;
            let rendered = RenderedText::from_shell(&shell, &mut renderer);
            if rendered.badge_count("Аккаунт B") == previous_count + 1
                && !notifications.0.lock().unwrap().is_empty()
            {
                break;
            }
        }
    })
    .await
    .expect("сообщение не обновило бейдж и уведомление фонового аккаунта");

    // После остановки фоновых задач обработаем всё, что они успели передать оболочке.
    stop.send(()).unwrap();
    timeout(DEADLINE, worker).await.unwrap().unwrap().unwrap();
    while let Some(Some(message)) = futures::FutureExt::now_or_never(events.next()) {
        update_shell(&mut shell, message).await;
    }

    assert_eq!(
        storage
            .message(&background, &message_id)
            .unwrap()
            .unwrap()
            .text,
        body
    );
    let after = RenderedText::from_shell(&shell, &mut renderer);
    assert!(after.shows_selected("Аккаунт A"));
    assert_eq!(after.badge_count("Аккаунт A"), foreground_count);
    assert_eq!(after.badge_count("Аккаунт B"), previous_count + 1);
    let click = {
        let requests = notifications.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, expected_notification);
        requests[0].1.clone()
    };
    assert_eq!(
        click.target,
        NotificationTarget {
            context: background.clone(),
            conversation_id,
            message_id,
        }
    );
    notification_click_scenario(&mut shell, &mut renderer, &click).await;
}

/// Подтверждает точную цель из баннера, подъём окна и защиту от запоздалого ответа.
async fn notification_click_scenario(
    shell: &mut Shell,
    renderer: &mut iced::Renderer,
    click: &NotificationClick,
) {
    let run_click = async |shell: &mut Shell, click: NotificationClick| {
        let window_id = window::Id::unique();
        let mut actions = into_stream(shell.update(Message::NotificationClicked(click))).unwrap();
        let mut restored = false;
        let mut focused = false;
        let mut result = None;
        while let Some(action) = timeout(DEADLINE, actions.next()).await.unwrap() {
            match action {
                Action::Window(window::Action::GetOldest(reply)) => {
                    reply.send(Some(window_id)).unwrap();
                }
                Action::Window(window::Action::Minimize(id, false)) => {
                    assert_eq!(id, window_id);
                    restored = true;
                }
                Action::Window(window::Action::GainFocus(id)) => {
                    assert_eq!(id, window_id);
                    assert!(restored);
                    focused = true;
                }
                Action::Output(message @ Message::NotificationResolved { .. }) => {
                    result = Some(message);
                }
                action => panic!("неожиданное действие при клике: {action:?}"),
            }
        }
        assert!(focused);
        result.unwrap()
    };

    let mut wrong_chat = click.clone();
    wrong_chat.target.conversation_id = Uuid::new_v4().to_string();
    let result = run_click(shell, wrong_chat).await;
    assert!(matches!(
        result,
        Message::NotificationResolved { result: Err(_), .. }
    ));
    let _ = shell.update(result);
    assert!(RenderedText::from_shell(shell, renderer).shows_selected("Аккаунт A"));

    let result = run_click(shell, click.clone()).await;
    assert!(
        matches!(&result, Message::NotificationResolved { result: Ok(target), .. } if target == &click.target)
    );
    // Выбор пользователя, сделанный пока core готовил ответ, сохраняет текущий маршрут.
    let selected = shell.accounts.selected.clone().unwrap();
    let _ = shell.update(Message::Select(selected));
    let _ = shell.update(result);
    assert!(RenderedText::from_shell(shell, renderer).shows_selected("Аккаунт A"));

    let result = run_click(shell, click.clone()).await;
    let _ = shell.update(result);
    assert!(RenderedText::from_shell(shell, renderer).shows_selected("Аккаунт B"));
}

/// Проверяет повтор загрузки каталога и фоновое уведомление через локальный HTTPS и WebSocket.
#[tokio::test]
async fn background_message_updates_iced_badge_and_notifies_once() {
    let workspace = mock::MockWorkspace::start().await;
    workspace.fail_next_catalog();
    background_message_scenario(&workspace, true).await;
}

/// Запускает тот же сценарий на явно указанном тестовом Workspace.
#[tokio::test]
#[ignore = "отправляет сообщение на выделенный тестовый Workspace"]
async fn live_workspace_background_message_updates_iced_badge_and_notifies_once() {
    let workspace = live::LiveWorkspace::from_environment();
    background_message_scenario(&workspace, false).await;
}
