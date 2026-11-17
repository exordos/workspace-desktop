//! Проверяет ввод, геометрию редактора и отправку через рабочий экран разговора.

use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

use gpui::{
    ClipboardItem, EntityInputHandler, Focusable, KeyDownEvent, Keystroke, ListOffset, Modifiers,
    TestAppContext, VisualTestContext, px, size,
};
use workspace_core::{
    context::{AccountId, OwnerContext, SessionGeneration},
    storage::{DeliveryStatus, HistoryMessage, HistoryMetadata, HistoryTime, PreparedSend},
};

use crate::gpui_client::{
    composer::{DestinationError, Editor},
    contracts::{ChatRoute, ConversationReply, NavigationRevision},
};

use super::*;

#[path = "../history/stability.rs"]
mod stability;

const MESSAGE: Uuid = Uuid::from_u128(2001);
const TOPIC: Uuid = Uuid::from_u128(2002);

impl Conversation {
    /// Передаёт сквозному тесту редактор разговора после проверки истории.
    pub(in crate::gpui_client) fn tested_composer(&self) -> Option<&Entity<Composer>> {
        self.sending.as_ref().map(|sending| &sending.view)
    }

    /// Проверяет, какой участок разговора получил фокус от клавиши Tab.
    pub(in crate::gpui_client) fn keyboard_focus(
        &self,
        window: &Window,
        app: &App,
    ) -> Option<&'static str> {
        if self.history_focus.is_focused(window) {
            Some("история")
        } else if self.sending.as_ref().is_some_and(|sending| {
            sending
                .view
                .read(app)
                .editor()
                .read(app)
                .focus_handle(app)
                .is_focused(window)
        }) {
            Some("редактор")
        } else {
            None
        }
    }

    /// Подставляет отказ записи и возвращает действие для восстановления рабочего источника.
    pub(in crate::gpui_client) fn fail_queue_write(
        &mut self,
    ) -> (Entity<Composer>, impl FnOnce(&mut Self) + use<>) {
        let sending = self.sending.as_mut().unwrap();
        let previous = std::mem::replace(
            &mut sending.source.save,
            Arc::new(|submission| {
                Box::pin(async move {
                    ConversationReply {
                        address: submission.address,
                        value: None,
                    }
                })
            }),
        );
        let restore = move |conversation: &mut Self| {
            conversation.sending.as_mut().unwrap().source.save = previous;
        };
        (sending.view.clone(), restore)
    }
}

/// Запрос разговора, который тест может завершить после следующих действий пользователя.
struct Request<T, U> {
    input: T,                  // Данные начала операции.
    reply: oneshot::Sender<U>, // Канал ответа, удерживаемый до нужного шага.
}

/// Три канала рабочего сценария: загрузка истории, проверка темы и запись очереди.
struct Requests {
    history: mpsc::Receiver<Request<(ConversationAddress, Load), ConversationReply<LoadResult>>>, // Открытие и обновление.
    resolve: mpsc::Receiver<
        Request<ConversationAddress, ConversationReply<Result<Uuid, DestinationError>>>,
    >, // Проверка темы.
    save: mpsc::Receiver<Request<Submission, ConversationReply<Option<Uuid>>>>, // Сохранение снимка.
}

/// Создаёт поток запросов с задержанным ответом.
fn send_request<T: Send + 'static, U: Send + 'static>(
    requests: &mpsc::Sender<Request<T, U>>,
    input: T,
) -> futures::future::BoxFuture<'static, U> {
    let (reply, output) = oneshot::channel();
    assert!(requests.send(Request { input, reply }).is_ok());
    Box::pin(async move { output.await.expect("Тест должен отправить ответ") })
}

/// Ожидает нужный вызов через штатный обработчик GPUI.
fn next<T, U>(requests: &mpsc::Receiver<Request<T, U>>, step: &str) -> Request<T, U> {
    requests
        .recv_timeout(Duration::from_millis(100))
        .unwrap_or_else(|_| panic!("Экран не запросил: {step}"))
}

/// Собирает разговор с полным адресом и источниками, чьи ответы выдаёт тест.
fn conversation(
    name: &'static str,
    revision: u64,
    cx: &mut Context<Conversation>,
) -> (Conversation, Requests) {
    let address = ConversationAddress {
        route: ChatRoute {
            owner: OwnerContext {
                account_id: AccountId(name.into()),
                server: "https://workspace.test".into(),
                organization_id: "test-org".into(),
                project_id: "test-project".into(),
                user_id: Uuid::from_u128(900).to_string(),
            },
            scope: HistoryScope {
                stream_id: Uuid::from_u128(1000),
                topic_id: None,
            },
            entry: ChatEntry::Latest,
        },
        generation: SessionGeneration(1),
        revision: NavigationRevision(revision),
    };
    let (history, history_rx) = mpsc::channel();
    let (resolve, resolve_rx) = mpsc::channel();
    let (save, save_rx) = mpsc::channel();
    let source: Source = Arc::new(move |address, load, _, cache| {
        drop(cache);
        send_request(&history, (address, load))
    });
    let mut view = Conversation::with_source(address, source, cx);
    view.attach_composer(
        SendSource {
            resolve: Arc::new(move |address, _| send_request(&resolve, address)),
            save: Arc::new(move |submission| send_request(&save, submission)),
        },
        cx,
    );
    (
        view,
        Requests {
            history: history_rx,
            resolve: resolve_rx,
            save: save_rx,
        },
    )
}

/// Открывает тестовое окно на том же представлении разговора, что и приложение.
fn start(cx: &mut TestAppContext) -> (Entity<Conversation>, &mut VisualTestContext, Requests) {
    cx.update(Editor::bind_keys);
    let mut requests = None;
    let (view, ui) = cx.add_window_view(|_, cx| {
        let (view, channels) = conversation("test-a", 5, cx);
        requests = Some(channels);
        view
    });
    ui.simulate_resize(size(px(650.), px(530.)));
    ui.run_until_parked();
    (view, ui, requests.unwrap())
}

/// Публикует в экран подтверждённый хвост и записи очереди из одного снимка.
fn publish(
    requests: &Requests,
    expected: Load,
    messages: Vec<HistoryMessage>,
    outbox: Vec<OutboxEntry>,
    ui: &mut VisualTestContext,
) {
    publish_with_older(requests, expected, messages, outbox, HistoryEdge::End, ui);
}

/// Публикует окно истории, сохраняя возможность открыть страницу сверху.
fn publish_with_older(
    requests: &Requests,
    expected: Load,
    messages: Vec<HistoryMessage>,
    outbox: Vec<OutboxEntry>,
    older: HistoryEdge,
    ui: &mut VisualTestContext,
) {
    let Request {
        input: (address, load),
        reply,
    } = next(&requests.history, "снимок истории");
    assert_eq!(load, expected);
    let scope = address.route.scope;
    assert!(
        reply
            .send(ConversationReply {
                address,
                value: Ok(Some(Snapshot {
                    window: HistoryWindow {
                        id: 7,
                        scope,
                        older,
                        newer: HistoryEdge::End,
                        needs_refresh: false,
                        messages
                    },
                    outbox: Some(outbox),
                }))
            })
            .is_ok()
    );
    ui.run_until_parked();
}

/// Возвращает проверенную тему или отказ для исходного разговора.
fn allow(requests: &Requests, value: Result<Uuid, DestinationError>, ui: &mut VisualTestContext) {
    let Request {
        input: address,
        reply,
    } = next(&requests.resolve, "проверка темы");
    assert!(reply.send(ConversationReply { address, value }).is_ok());
    ui.run_until_parked();
}

/// Нажимает видимую кнопку с реальным событием GPUI.
fn click(ui: &mut VisualTestContext, selector: &'static str) {
    let center = ui
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("На экране нет {selector}"))
        .center();
    ui.simulate_click(center, Modifiers::none());
    ui.run_until_parked();
}

/// Вставляет текст тем же сочетанием клавиш, что используется в окне.
fn paste(ui: &mut VisualTestContext, text: &str) {
    click(ui, "workspace-composer-editor");
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string(text.into())));
    ui.simulate_keystrokes("cmd-a cmd-v");
    ui.run_until_parked();
}

/// Проверяет показанный ввод, который ещё принадлежит этому разговору.
fn draft(view: &Entity<Conversation>, ui: &VisualTestContext) -> String {
    ui.read(|app| {
        view.read(app)
            .sending
            .as_ref()
            .unwrap()
            .view
            .read(app)
            .draft(app)
            .to_owned()
    })
}

/// Завершает задержанную запись с тем адресом, который был передан в исходный запрос.
fn saved(
    request: Request<Submission, ConversationReply<Option<Uuid>>>,
    value: Option<Uuid>,
    ui: &mut VisualTestContext,
) {
    assert!(
        request
            .reply
            .send(ConversationReply {
                address: request.input.address,
                value
            })
            .is_ok()
    );
    ui.run_until_parked();
}

/// Отказ и пробелы не сохраняют сообщение. Ошибка SQLite оставляет текст; повтор даёт одну запись.
#[gpui::test]
fn accepts_only_valid_text_and_one_click_while_saving(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Err(DestinationError::EncryptionUnavailable), ui);
    paste(ui, "Обычный текст");
    ui.simulate_keystrokes("enter");
    assert!(
        requests.save.try_recv().is_err(),
        "Запрещённая тема не допускает запись"
    );

    click(ui, "conversation-send-retry");
    allow(&requests, Ok(TOPIC), ui);
    paste(ui, " \n ");
    click(ui, "conversation-send");
    assert!(
        requests.save.try_recv().is_err(),
        "Пробелы не считаются сообщением"
    );

    paste(ui, &"Ж".repeat(40_001));
    ui.simulate_keystrokes("enter");
    assert!(requests.save.try_recv().is_err());
    assert!(ui.debug_bounds("conversation-send-explanation").is_some());

    paste(ui, "Обычный текст");
    click(ui, "conversation-send");
    let first = next(&requests.save, "первая запись очереди");
    assert_eq!(first.input.topic, TOPIC);
    assert_eq!(first.input.markdown, "Обычный текст");
    click(ui, "conversation-send");
    assert!(
        requests.save.try_recv().is_err(),
        "Второй клик не создаёт запись"
    );
    saved(first, None, ui);
    assert_eq!(draft(&view, ui), "Обычный текст");
    assert!(ui.debug_bounds("conversation-send-explanation").is_some());

    click(ui, "conversation-send");
    saved(
        next(&requests.save, "успешный повтор записи"),
        Some(MESSAGE),
        ui,
    );
    assert!(
        draft(&view, ui).is_empty(),
        "Успешно записанная версия должна очиститься"
    );
    publish(&requests, Load::Read(7), vec![], vec![], ui);
}

/// Рабочий редактор передаёт Enter один раз, а AppKit подтверждает композицию отдельно.
#[gpui::test]
fn editor_keyboard_and_ime_share_the_same_send_path(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(TOPIC), ui);
    paste(ui, "Строка 🧪");
    ui.simulate_keystrokes("right shift-enter");
    assert_eq!(draft(&view, ui), "Строка 🧪\n");
    assert!(requests.save.try_recv().is_err());

    let editor = ui.read(|app| {
        view.read(app)
            .sending
            .as_ref()
            .unwrap()
            .view
            .read(app)
            .editor()
    });
    ui.update(|window, app| {
        editor.update(app, |field, cx| {
            field.replace_and_mark_text_in_range(None, "仮", Some(0..1), window, cx);
        });
    });
    ui.run_until_parked();
    ui.simulate_keystrokes("enter");
    assert!(requests.save.try_recv().is_err());
    assert_eq!(draft(&view, ui), "Строка 🧪\n仮");

    ui.update(|window, app| {
        editor.update(app, |field, cx| {
            field.replace_text_in_range(None, "確定", window, cx);
        });
    });
    ui.run_until_parked();
    ui.simulate_keystrokes("enter");
    let submission = next(&requests.save, "Enter после подтверждения IME");
    assert_eq!(submission.input.markdown, "Строка 🧪\n確定");
    saved(submission, None, ui);
    assert_eq!(draft(&view, ui), "Строка 🧪\n確定");

    ui.simulate_event(KeyDownEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
        is_held: true,
    });
    ui.run_until_parked();
    assert!(
        requests.save.try_recv().is_err(),
        "Удержание Enter не повторяет сбой"
    );
    assert!(ui.update(|window, app| editor.focus_handle(app).is_focused(window)));

    click(ui, "conversation-send");
    saved(next(&requests.save, "Повтор кнопкой"), Some(MESSAGE), ui);
    assert!(draft(&view, ui).is_empty());
    assert!(ui.update(|window, app| editor.focus_handle(app).is_focused(window)));
    publish(&requests, Load::Read(7), vec![], vec![], ui);
}

/// Перенос и рост редактора оставляют опору читателя на месте и сохраняют нижнюю привязку.
#[gpui::test]
fn editor_growth_preserves_reading_anchor_and_tail(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    let stream_id = ui.read(|app| view.read(app).address().route.scope.stream_id);
    let messages = (1..=60)
        .map(|index| HistoryMessage {
            id: Uuid::from_u128(index),
            stream_id,
            topic_id: TOPIC,
            text: "Синтетическая строка".into(),
            metadata: HistoryMetadata {
                author_id: Some(Uuid::from_u128(900)),
                created_at: HistoryTime::parse("2026-11-17T07:10:30Z").unwrap(),
                updated_at: None,
            },
        })
        .collect();
    publish(
        &requests,
        Load::Open(ChatEntry::Latest),
        messages,
        vec![],
        ui,
    );
    allow(&requests, Ok(TOPIC), ui);

    let empty = ui.debug_bounds("workspace-composer-editor").unwrap();
    assert_eq!(empty.size.height, px(38.));
    let send = ui.debug_bounds("conversation-send").unwrap();
    assert_eq!(send.size, size(px(40.), px(40.)));
    assert_eq!(send.bottom(), empty.bottom() + px(1.));

    // Измеряем верх последней строки от нижнего края списка. Её высота и текст не меняются.
    let tail_inset = |ui: &VisualTestContext| {
        ui.read(|app| {
            let viewport = &view.read(app).viewport;
            viewport.list().viewport_bounds().size.height
                - px(viewport.y(Uuid::from_u128(60)).unwrap())
        })
    };
    let inset_before = tail_inset(ui);
    paste(
        ui,
        "Длинная строка, которая переносится в узком окне. "
            .repeat(3)
            .as_str(),
    );
    let wide = ui
        .debug_bounds("workspace-composer-editor")
        .unwrap()
        .size
        .height;
    assert!(wide > empty.size.height);
    assert_eq!(tail_inset(ui), inset_before);

    ui.simulate_resize(size(px(340.), px(530.)));
    ui.run_until_parked();
    let narrow = ui
        .debug_bounds("workspace-composer-editor")
        .unwrap()
        .size
        .height;
    assert!(
        narrow > wide,
        "Перенос должен увеличить высоту поля: {wide:?} → {narrow:?}"
    );
    assert_eq!(tail_inset(ui), inset_before);

    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.viewport.list().scroll_to(ListOffset {
                item_ix: 8,
                offset_in_item: px(11.),
            });
            cx.notify();
        })
    });
    ui.run_until_parked();
    let anchor = Uuid::from_u128(9);
    let anchor_y =
        |ui: &VisualTestContext| ui.read(|app| view.read(app).viewport.y(anchor).unwrap());
    let before = anchor_y(ui);
    let long_text = "Длинный черновик 🧪\r\n".repeat(50);
    paste(ui, &long_text);
    assert_eq!(draft(&view, ui), long_text.replace("\r\n", "\n"));
    assert_eq!(
        ui.debug_bounds("workspace-composer-editor")
            .unwrap()
            .size
            .height,
        px(126.)
    );
    assert_eq!(anchor_y(ui), before);
    paste(ui, "");
    assert_eq!(
        ui.debug_bounds("workspace-composer-editor")
            .unwrap()
            .size
            .height,
        empty.size.height
    );
    assert_eq!(anchor_y(ui), before);
}

/// Новая версия того же ввода и другой открытый разговор переживают поздний итог сохранения.
#[gpui::test]
fn late_results_keep_changed_text_and_the_next_account_draft(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(TOPIC), ui);
    paste(ui, "Первый черновик");
    click(ui, "conversation-send");
    let first = next(&requests.save, "старый черновик");
    paste(ui, "Изменено");
    paste(ui, "Первый черновик");
    saved(first, Some(MESSAGE), ui);
    assert_eq!(
        draft(&view, ui),
        "Первый черновик",
        "Совпадения строки мало: версия уже другая"
    );
    publish(&requests, Load::Read(7), vec![], vec![], ui);

    click(ui, "conversation-send");
    let late = next(&requests.save, "отправка из A");
    assert_eq!(late.input.address.route.owner.account_id.0, "test-a");
    let mut second_requests = None;
    let second = ui.update(|window, app| {
        window.replace_root(app, |_, cx| {
            let (view, channels) = conversation("test-b", 6, cx);
            second_requests = Some(channels);
            view
        })
    });
    drop(view);
    ui.run_until_parked();
    let second_requests = second_requests.unwrap();
    publish(
        &second_requests,
        Load::Open(ChatEntry::Latest),
        vec![],
        vec![],
        ui,
    );
    allow(&second_requests, Ok(TOPIC), ui);
    paste(ui, "Черновик B");
    saved(late, Some(MESSAGE), ui);
    assert_eq!(draft(&second, ui), "Черновик B");
    assert!(
        second_requests.save.try_recv().is_err(),
        "Ответ A не записывает ничего в B"
    );
}

/// Передаёт событие от core маршруту, который сейчас открыт в окне.
fn changed(view: &Entity<Conversation>, ui: &mut VisualTestContext, change: SyncChange) {
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            let address = view.address().clone();
            let update = SyncUpdate {
                context: address.route.owner.clone(),
                generation: address.generation,
                change,
            };
            view.apply_sync(&address, update, cx);
        })
    });
    ui.run_until_parked();
}

/// Запись с неизвестным исходом остаётся видимой. Подтверждения и повторы дают одну строку UUID.
#[gpui::test]
fn keeps_unknown_delivery_and_deduplicates_after_confirmation(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(TOPIC), ui);
    paste(ui, "Проверка отправки");
    click(ui, "conversation-send");
    let request = next(&requests.save, "запись текста");
    let scope = request.input.address.route.scope;
    let prepared = PreparedSend {
        canonical_id: Uuid::from_u128(2000),
        public_id: MESSAGE,
        stream_id: scope.stream_id,
        topic_id: TOPIC,
        markdown: request.input.markdown.clone(),
    };
    saved(request, Some(MESSAGE), ui);
    let waiting = OutboxEntry {
        snapshot: prepared.clone(),
        status: DeliveryStatus::Waiting,
    };
    publish(&requests, Load::Read(7), vec![], vec![waiting], ui);
    ui.read(|app| assert!(view.read(app).viewport.y(MESSAGE).is_some()));

    changed(&view, ui, SyncChange::OutboxChanged { scope });
    let unknown = OutboxEntry {
        snapshot: prepared,
        status: DeliveryStatus::Unknown,
    };
    publish(&requests, Load::Read(7), vec![], vec![unknown.clone()], ui);
    ui.read(|app| {
        let conversation = view.read(app);
        assert_eq!(conversation.outbox[0].status, DeliveryStatus::Unknown);
        assert_eq!(conversation.viewport.list().item_count(), 1);
        assert!(conversation.viewport.y(MESSAGE).is_some());
    });

    assert!(ui.debug_bounds("history-delivery-unknown").is_some());

    let confirmed = HistoryMessage {
        id: MESSAGE,
        stream_id: scope.stream_id,
        topic_id: TOPIC,
        text: "Проверка отправки".into(),
        metadata: HistoryMetadata {
            author_id: Some(Uuid::from_u128(900)),
            created_at: HistoryTime::parse("2026-11-17T07:10:30Z").unwrap(),
            updated_at: None,
        },
    };
    changed(
        &view,
        ui,
        SyncChange::HistoryMessageCreated {
            scope,
            message_id: MESSAGE,
        },
    );
    publish(
        &requests,
        Load::Read(7),
        vec![confirmed.clone()],
        vec![unknown],
        ui,
    );
    ui.read(|app| assert_eq!(view.read(app).viewport.list().item_count(), 1));
    changed(&view, ui, SyncChange::OutboxChanged { scope });
    publish(&requests, Load::Read(7), vec![confirmed], vec![], ui);
    ui.read(|app| {
        let conversation = view.read(app);
        assert!(conversation.outbox.is_empty());
        assert_eq!(conversation.viewport.list().item_count(), 1);
        assert!(conversation.viewport.y(MESSAGE).is_some());
    });
}
