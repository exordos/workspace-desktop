//! Проверяет отправку из редактора, подтверждение строки и смену аккаунта во время записи.

use iced::widget::text_editor::{Action as EditAction, Edit};
use workspace_core::storage::{
    Cursor, DeliveryStatus, HistoryMessage, HistoryMetadata, HistoryPageEdges, HistoryPageOutcome,
    HistoryPageTarget, HistoryTime, Message as StoredMessage, MessagePlacement, PreparedSend,
};
use workspace_core::sync::Event as SyncEvent;

use super::*;

const STREAM: Uuid = Uuid::from_u128(90);
const TOPIC: Uuid = Uuid::from_u128(91);
const SCOPE: HistoryScope = HistoryScope {
    stream_id: STREAM,
    topic_id: None,
};

/// Нажимает кнопку Iced и проверяет запись, одну строку после HTTP и событий, поздний ответ.
#[tokio::test]
async fn composer_saves_once_and_keeps_another_accounts_draft() {
    let workspace = mock::MockWorkspace::start().await;
    let profile = tempfile::tempdir().unwrap();
    let (auth, a, b) = workspace.accounts(profile.path()).await;
    auth.restore().await.unwrap();
    let storage = Arc::new(Storage::open(profile.path().join("messenger.sqlite3")).unwrap());
    let sync = Arc::new(Sync::new(auth.clone(), storage.clone()));
    let messenger = Arc::new(workspace.source());
    let (catalog, _) = bootstrap::chat_catalog(messenger.catalog(storage.clone()));
    let (subscribed, _) = oneshot::channel();
    let mut shell = Shell::with_notifications(
        auth,
        HashMap::from([
            (a.account_id.clone(), "A".to_owned()),
            (b.account_id.clone(), "B".to_owned()),
        ]),
        sync.clone(),
        (Arc::new(catalog), messenger),
        Some(b.account_id.clone()),
        subscribed,
        Box::<RecordedNotifications>::default(),
    );
    let mut renderer = iced::Renderer::new(Font::DEFAULT, Pixels(16.0));
    let mut cache = Cache::new();
    let _ = shell.update(Message::Snapshot(shell.sources.0.snapshot()));
    let b_generation = open_editor(
        &mut shell,
        &mut renderer,
        &mut cache,
        &sync,
        &b,
        ChatEntry::Around(Uuid::from_u128(1)),
        HistoryEdge::Unchecked,
    )
    .await;
    assert!(click_send(&shell, &mut renderer).is_empty());

    let first_text = "Сначала подтвердил HTTP";
    paste(&mut shell, first_text);
    let latest = click_latest(&shell, &mut renderer);
    // Последнюю страницу готовит фикстура SQLite. Кнопка меняет только видимое окно той же лички.
    assert!(into_stream(shell.update(latest)).is_some());
    let current = shell.visible_history().unwrap();
    assert_eq!(current.route.owner, b);
    assert_eq!(current.route.entry, ChatEntry::Latest);
    let window = seed_window(&sync, &b, b_generation, HistoryEdge::End);
    show_window(&mut shell, &mut renderer, &mut cache, window).await;

    for (number, text) in [(1, first_text), (2, "Сначала пришло событие")] {
        if number == 2 {
            paste(&mut shell, text);
        }
        let click = send_click(&shell, &mut renderer);
        let saving = shell.update(click.clone());
        assert!(click_send(&shell, &mut renderer).is_empty());
        // Событие второго клика могло уже стоять в очереди до перерисовки.
        assert!(into_stream(shell.update(click)).is_none());

        let saved = saved_message(saving).await;
        let entries = sync.outbox(&b, b_generation, SCOPE).unwrap();
        let [entry] = entries.as_slice() else {
            panic!("Ожидалась одна запись очереди B");
        };
        assert_eq!(entry.status, DeliveryStatus::Waiting);
        assert_eq!(entry.snapshot.stream_id, STREAM);
        assert_eq!(entry.snapshot.topic_id, TOPIC);
        assert_eq!(entry.snapshot.markdown, text);
        assert!(storage.pending_sends(&a).unwrap().is_empty());
        update_history_ui(&mut shell, &mut renderer, &mut cache, saved).await;
        assert!(click_send(&shell, &mut renderer).is_empty());
        let pending = RenderedText::from_shell(&shell, &mut renderer);
        assert!(pending.contains(text));
        assert!(pending.contains("В очереди"));

        let snapshot = &entry.snapshot;
        let server_message = delivered(&b, snapshot, number);
        assert_eq!(
            storage.claim_first_send(&b, snapshot.canonical_id).unwrap(),
            Some(snapshot.clone())
        );
        let mut updates = sync.subscribe();
        if number == 1 {
            assert!(
                sync.confirm_http_send(&b, b_generation, &server_message)
                    .unwrap()
            );
            apply_created(&sync, &b, b_generation, &server_message, number);
        } else {
            apply_created(&sync, &b, b_generation, &server_message, number);
            assert!(
                !sync
                    .confirm_http_send(&b, b_generation, &server_message)
                    .unwrap()
            );
        }
        while let Ok(update) = updates.try_recv() {
            update_history_ui(
                &mut shell,
                &mut renderer,
                &mut cache,
                Message::Synced(update),
            )
            .await;
        }
        assert!(sync.outbox(&b, b_generation, SCOPE).unwrap().is_empty());
        let confirmed = RenderedText::from_shell(&shell, &mut renderer);
        assert!(!confirmed.contains("В очереди"));
        assert_eq!(
            confirmed
                .0
                .iter()
                .filter(|(_, label)| label == text)
                .count(),
            1
        );
    }

    paste(&mut shell, "Ответ B перед сменой аккаунта");
    let click = send_click(&shell, &mut renderer);
    let late = saved_message(shell.update(click)).await;
    select_account(&mut shell, &mut renderer, &a.account_id);
    let a_generation = open_editor(
        &mut shell,
        &mut renderer,
        &mut cache,
        &sync,
        &a,
        ChatEntry::Latest,
        HistoryEdge::End,
    )
    .await;
    paste(&mut shell, "Новый текст в A");
    let _ = shell.update(late);
    let click = send_click(&shell, &mut renderer);
    let _ = saved_message(shell.update(click)).await;

    let a_entries = sync.outbox(&a, a_generation, SCOPE).unwrap();
    assert_eq!(a_entries.len(), 1);
    assert_eq!(a_entries[0].snapshot.markdown, "Новый текст в A");
    let b_entries = sync.outbox(&b, b_generation, SCOPE).unwrap();
    assert_eq!(b_entries.len(), 1);
    assert_eq!(
        b_entries[0].snapshot.markdown,
        "Ответ B перед сменой аккаунта"
    );
}

/// Открывает выбранную личку с ответами истории и адреса из фикстуры.
async fn open_editor(
    shell: &mut Shell,
    renderer: &mut iced::Renderer,
    cache: &mut Cache,
    sync: &Sync,
    owner: &OwnerContext,
    entry: ChatEntry,
    newer: HistoryEdge,
) -> SessionGeneration {
    let generation = shell.accounts.current().unwrap().session.generation;
    let window = seed_window(sync, owner, generation, newer);
    let _ = shell.open_history(
        ChatRoute {
            owner: owner.clone(),
            scope: SCOPE,
            entry,
        },
        generation,
    );
    show_window(shell, renderer, cache, window).await;
    generation
}

/// Записывает подтверждённое окно, в границы которого войдет серверная строка теста.
fn seed_window(
    sync: &Sync,
    owner: &OwnerContext,
    generation: SessionGeneration,
    newer: HistoryEdge,
) -> HistoryWindow {
    let request = sync
        .prepare_history_page(owner, generation, SCOPE, HistoryPageTarget::New)
        .unwrap()
        .unwrap();
    let rows = [("2025-01-01T12:00:00Z", 1), ("2025-01-31T12:00:00Z", 2)].map(|(date, id)| {
        HistoryMessage {
            id: Uuid::from_u128(id),
            stream_id: STREAM,
            topic_id: TOPIC,
            text: "Ранее подтверждённое сообщение".to_owned(),
            metadata: HistoryMetadata {
                author_id: owner.user_id.parse().ok(),
                created_at: HistoryTime::parse(date).unwrap(),
                updated_at: None,
            },
        }
    });
    let HistoryPageOutcome::Saved(id) = sync
        .save_history_page(
            request,
            &rows,
            HistoryPageEdges::New {
                older: HistoryEdge::End,
                newer,
            },
        )
        .unwrap()
    else {
        panic!("Не удалось сохранить окно истории");
    };
    sync.refresh_history_window(owner, generation, SCOPE, id)
        .unwrap()
        .unwrap()
}

/// Передаёт редактору готовую историю и адрес после штатного перехода.
async fn show_window(
    shell: &mut Shell,
    renderer: &mut iced::Renderer,
    cache: &mut Cache,
    window: HistoryWindow,
) {
    // Загрузку истории и сетевую проверку адреса покрывают отдельные сценарии.
    let revision = shell.navigation_revision;
    update_history_ui(
        shell,
        renderer,
        cache,
        Message::HistoryLoaded {
            revision,
            result: Ok(Arc::new(window)),
        },
    )
    .await;
    let _ = shell.update(Message::SendAddressResolved {
        revision,
        result: Ok(TOPIC),
    });
}

/// Выполняет измерение и восстановление позиции списка до следующего действия.
async fn update_history_ui(
    shell: &mut Shell,
    renderer: &mut iced::Renderer,
    cache: &mut Cache,
    message: Message,
) {
    let mut next = Some(message);
    while let Some(message) = next {
        let task = shell.update(message);
        let layout = std::mem::take(cache);
        let (layout, output) = run_history_task(shell, renderer, layout, task).await;
        *cache = layout;
        next = output;
    }
}

/// Передаёт вставку так же, как многострочный редактор передаёт действие оболочке.
fn paste(shell: &mut Shell, text: &str) {
    let _ = shell.update(Message::EditConversationText {
        revision: shell.navigation_revision,
        action: EditAction::Edit(Edit::Paste(Arc::new(text.to_owned()))),
    });
}

/// Нажимает настоящую кнопку отправки по положению в разметке Iced.
fn click_send(shell: &Shell, renderer: &mut iced::Renderer) -> Vec<Message> {
    let mut ui = build_ui(shell, renderer);
    let button = RenderedText::from_ui(&mut ui, renderer)
        .bounds(Id::new("conversation-send"))
        .expect("в открытом разговоре нет кнопки отправки");
    click_ui(&mut ui, renderer, button.center())
}

/// Нажимает кнопку перехода от сообщения из уведомления к последнему сообщению.
fn click_latest(shell: &Shell, renderer: &mut iced::Renderer) -> Message {
    let mut ui = build_ui(shell, renderer);
    let labels = RenderedText::from_ui(&mut ui, renderer);
    let (bounds, _) = labels
        .0
        .iter()
        .find(|(_, label)| label == "К последним сообщениям")
        .expect("в истории нет кнопки перехода к последним сообщениям");
    let messages = click_ui(&mut ui, renderer, bounds.center());
    let [message @ Message::SeeLatest(_)] = messages.as_slice() else {
        panic!("Кнопка перехода вернула {messages:?}");
    };
    message.clone()
}

/// Проверяет, что активная кнопка вернула ровно одно действие рабочего экрана.
fn send_click(shell: &Shell, renderer: &mut iced::Renderer) -> Message {
    let messages = click_send(shell, renderer);
    let [message @ Message::SendConversationText(_)] = messages.as_slice() else {
        panic!("Кнопка отправки вернула {messages:?}");
    };
    message.clone()
}

/// Дожидается записи настоящей задачи редактора и удерживает её ответ для сценария.
async fn saved_message(task: Task<Message>) -> Message {
    let mut actions = into_stream(task).expect("кнопка не запустила запись");
    match timeout(DEADLINE, actions.next()).await.unwrap() {
        Some(Action::Output(message @ Message::ConversationTextSaved { saved: true, .. })) => {
            message
        }
        _ => panic!("не пришло подтверждение записи сообщения в SQLite"),
    }
}

/// Возвращает совпадающее подтверждение для исходного снимка и автора.
fn delivered(owner: &OwnerContext, snapshot: &PreparedSend, number: u64) -> HistoryMessage {
    HistoryMessage {
        id: snapshot.public_id,
        stream_id: snapshot.stream_id,
        topic_id: snapshot.topic_id,
        text: snapshot.markdown.clone(),
        metadata: HistoryMetadata {
            author_id: owner.user_id.parse().ok(),
            created_at: HistoryTime::parse(&format!("2025-01-{number:02}T13:00:00Z")).unwrap(),
            updated_at: None,
        },
    }
}

/// Пропускает подтверждение сервера через ту же запись, что использует WebSocket.
fn apply_created(
    sync: &Sync,
    owner: &OwnerContext,
    generation: SessionGeneration,
    message: &HistoryMessage,
    epoch_version: u64,
) {
    sync.apply(
        owner,
        generation,
        "messenger",
        SyncEvent::MessageCreated {
            cursor: Cursor {
                epoch_generation: "test".to_owned(),
                epoch_version,
            },
            message: StoredMessage {
                id: message.id.to_string(),
                text: message.text.clone(),
            },
            placement: Some(MessagePlacement {
                stream_id: message.stream_id.to_string(),
                topic_id: message.topic_id.to_string(),
            }),
            metadata: Some(message.metadata.clone()),
        },
    )
    .unwrap();
}
