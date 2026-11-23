//! Проверяет ввод, геометрию редактора и отправку через рабочий экран разговора.

use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

use gpui::{
    ClipboardItem, EntityInputHandler, Focusable, KeyDownEvent, KeyUpEvent, Keystroke, ListOffset,
    Modifiers, MouseButton, TestAppContext, VisualTestContext, px, size,
};
use workspace_core::{
    context::{AccountId, OwnerContext, SessionGeneration},
    storage::{
        DeliveryStatus, HistoryMessage, HistoryMetadata, HistoryTime, MessageBody, PreparedSend,
    },
};

use crate::gpui_client::{
    composer::{DestinationError, Editor},
    contracts::{ChatRoute, ConversationReply, NavigationRevision},
};

use super::*;

#[path = "quote_tests.rs"]
mod quote_tests;

#[path = "../history/stability.rs"]
mod stability;

#[path = "tests/toolbar.rs"]
mod toolbar;

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
                        value: QueueWriteOutcome::WriteFailed,
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
    save: mpsc::Receiver<Request<Submission, ConversationReply<QueueWriteOutcome>>>, // Сохранение снимка.
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
    let mut view = Conversation::with_source(address, source, None, None, cx);
    view.attach_composer(
        SendSource {
            resolve: Arc::new(move |address, _| {
                let future = send_request(&resolve, address);
                Box::pin(async move {
                    let reply: ConversationReply<Result<Uuid, DestinationError>> = future.await;
                    ConversationReply {
                        address: reply.address,
                        value: reply.value.map(|topic| {
                            crate::gpui_client::composer::ResolvedDestination {
                                encrypted: false,
                                topic,
                                closed: false,
                                plain_target: None,
                                restored_protected: false,
                                draft: None,
                            }
                        }),
                    }
                })
            }),
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
    cx.update(|app| {
        Editor::bind_keys(app);
        crate::gpui_client::ui::TopBar::bind_keys(app);
    });
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
                    media_access: None,
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
    request: Request<Submission, ConversationReply<QueueWriteOutcome>>,
    value: Option<Uuid>,
    ui: &mut VisualTestContext,
) {
    assert!(
        request
            .reply
            .send(ConversationReply {
                address: request.input.address,
                value: value.map_or(QueueWriteOutcome::WriteFailed, QueueWriteOutcome::Saved),
            })
            .is_ok()
    );
    ui.run_until_parked();
}

/// При блокировке отправки пояснение и кнопка занимают компактную плашку под историей.
#[gpui::test]
fn blocked_composer_keeps_footer_compact(cx: &mut TestAppContext) {
    let (_, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Err(DestinationError::EncryptionUnavailable), ui);
    assert!(ui.debug_bounds("workspace-composer-editor").is_none());

    for (width, max_height) in [(650., 48.), (340., 72.), (220., 72.)] {
        ui.simulate_resize(size(px(width), px(530.)));
        ui.run_until_parked();
        let notice = ui.debug_bounds("conversation-send-notice").unwrap();
        let text = ui.debug_bounds("conversation-send-notice-text").unwrap();
        let button = ui.debug_bounds("conversation-send-retry").unwrap();
        assert!(notice.size.height <= px(max_height), "{notice:?}");
        assert!(text.size.width > px(140.), "{text:?}");
        assert!(notice.contains(&button.origin), "{notice:?}, {button:?}");
        assert!(button.bottom() <= notice.bottom() && button.right() <= notice.right());
    }

    click(ui, "conversation-send-retry");
    allow(&requests, Ok(TOPIC), ui);
    assert!(ui.debug_bounds("workspace-composer-editor").is_some());
}

/// Отказ и пробелы не сохраняют сообщение. Ошибка SQLite оставляет текст; повтор даёт одну запись.
#[gpui::test]
fn accepts_only_valid_text_and_one_click_while_saving(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    assert!(ui.debug_bounds("conversation-send-notice").is_some());
    assert!(ui.debug_bounds("conversation-composer").is_none());
    allow(&requests, Err(DestinationError::EncryptionUnavailable), ui);
    assert!(ui.debug_bounds("conversation-send-notice").is_some());
    assert!(ui.debug_bounds("workspace-composer-editor").is_none());
    ui.simulate_keystrokes("enter");
    assert!(
        requests.save.try_recv().is_err(),
        "Запрещённая тема не допускает запись"
    );

    for width in [650., 340.] {
        ui.simulate_resize(size(px(width), px(530.)));
        ui.run_until_parked();
        let notice = ui.debug_bounds("conversation-send-notice").unwrap();
        let button = ui.debug_bounds("conversation-send-retry").unwrap();
        assert!(notice.contains(&button.origin), "{notice:?}, {button:?}");
        assert!(button.bottom() <= notice.bottom() && button.right() <= notice.right());
    }
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

/// Читает актуальное состояние меню. В debug_bounds GPUI остаётся геометрия закрытых элементов.
fn text_menu_open(view: &Entity<Conversation>, ui: &VisualTestContext) -> bool {
    ui.read(|app| {
        view.read(app)
            .tested_composer()
            .unwrap()
            .read(app)
            .editor()
            .read(app)
            .text_menu_is_open()
    })
}

/// Отпускает Enter после его нажатия, как при выборе команды меню в приложении.
fn confirm_text_menu(ui: &mut VisualTestContext) {
    ui.simulate_keystrokes("enter");
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    ui.run_until_parked();
}

/// Системное меню работает в нормальном разговоре, удерживает Enter и сохраняет выделение.
#[gpui::test]
fn composer_text_menu_edits_selection_without_sending(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(TOPIC), ui);

    click(ui, "workspace-composer-editor");
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string("Ёж 🧪".into())));
    let editor_bounds = ui.debug_bounds("workspace-composer-editor").unwrap();
    ui.simulate_keystrokes("shift-f10");
    let menu_bounds = ui
        .debug_bounds("composer-text-menu")
        .expect("Меню видно в рабочем окне");
    assert_eq!(menu_bounds.size, size(px(180.), px(138.)));
    assert_eq!(
        ui.debug_bounds("workspace-composer-editor"),
        Some(editor_bounds)
    );
    click(ui, "composer-text-cut");
    click(ui, "composer-text-copy");
    click(ui, "composer-text-select-all");
    assert!(ui.debug_bounds("composer-text-menu").is_some());
    confirm_text_menu(ui);
    assert_eq!(draft(&view, ui), "Ёж 🧪");
    assert!(!text_menu_open(&view, ui));

    ui.simulate_keystrokes("menu end");
    assert!(ui.debug_bounds("composer-text-menu").is_some());
    confirm_text_menu(ui);
    // Правый клик даже вне выделенного слова не меняет исходный диапазон.
    let position = ui
        .debug_bounds("workspace-composer-editor")
        .unwrap()
        .center();
    ui.simulate_mouse_down(position, MouseButton::Right, Modifiers::none());
    ui.simulate_mouse_up(position, MouseButton::Right, Modifiers::none());
    ui.run_until_parked();
    assert!(ui.debug_bounds("composer-text-menu").is_some());
    click(ui, "composer-text-copy");
    assert_eq!(draft(&view, ui), "Ёж 🧪");
    assert!(!text_menu_open(&view, ui));
    let copied = ui.update(|_, app| app.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some("Ёж 🧪"));

    ui.simulate_keystrokes("right shift-left shift-f10 escape");
    assert!(!text_menu_open(&view, ui));
    ui.simulate_keystrokes("shift-f10");
    confirm_text_menu(ui);
    assert_eq!(draft(&view, ui), "Ёж ");
    let selected = ui.update(|_, app| app.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(selected.as_deref(), Some("🧪"));
    assert!(
        requests.save.try_recv().is_err(),
        "Меню не отправляет черновик по Enter"
    );
}

/// Меню закрывается снаружи, а сохранение снимка временно отключает его команды.
#[gpui::test]
fn composer_text_menu_dismisses_outside_and_disables_during_save(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(TOPIC), ui);
    paste(ui, "Черновик");

    ui.simulate_keystrokes("enter cmd-a shift-f10");
    let attempt = next(&requests.save, "сохранение черновика под открытым меню");
    assert!(ui.debug_bounds("composer-text-menu").is_some());
    click(ui, "composer-text-cut");
    confirm_text_menu(ui);
    assert_eq!(draft(&view, ui), "Черновик");
    assert!(text_menu_open(&view, ui));

    let exposed = ui.debug_bounds("conversation-send").unwrap().center();
    ui.simulate_click(exposed, Modifiers::none());
    ui.run_until_parked();
    assert!(!text_menu_open(&view, ui));
    saved(attempt, None, ui);
    assert_eq!(draft(&view, ui), "Черновик");
    assert!(requests.save.try_recv().is_err());

    click(ui, "workspace-composer-editor");
    ui.simulate_keystrokes("shift-f10");
    assert!(text_menu_open(&view, ui));
    click(ui, "conversation-send");
    assert!(!text_menu_open(&view, ui));
    let retry = next(&requests.save, "кнопка под немодальным меню");
    assert_eq!(retry.input.markdown, "Черновик");
    saved(retry, None, ui);
}

/// Старый отказ записи не меняет более новую успешную проверку той же темы.
#[gpui::test]
fn stale_write_rejection_keeps_the_latest_destination_confirmation(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(TOPIC), ui);
    paste(ui, "Черновик при сбросе");
    click(ui, "conversation-send");
    let late = next(&requests.save, "задержанная проверка перед записью");

    let address = late.input.address.clone();
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.apply_sync(
                &address,
                SyncUpdate {
                    context: address.route.owner.clone(),
                    generation: address.generation,
                    change: SyncChange::MessengerReset,
                },
                cx,
            );
        })
    });
    ui.run_until_parked();
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(TOPIC), ui);

    assert!(
        late.reply
            .send(ConversationReply {
                address,
                value: QueueWriteOutcome::DestinationUnavailable(
                    DestinationError::EncryptionUnavailable,
                ),
            })
            .is_ok()
    );
    ui.run_until_parked();
    assert_eq!(draft(&view, ui), "Черновик при сбросе");
    assert!(ui.debug_bounds("conversation-send-retry").is_none());
    click(ui, "conversation-send");
    let fresh = next(&requests.save, "запись после новой проверки");
    assert_eq!(fresh.input.topic, TOPIC);
    assert_eq!(fresh.input.markdown, "Черновик при сбросе");
    saved(fresh, Some(MESSAGE), ui);
    assert!(draft(&view, ui).is_empty());
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
    ui.simulate_keystrokes("shift-f10 enter");
    assert!(!text_menu_open(&view, ui));
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
            body: MessageBody::Markdown("Синтетическая строка".into()),
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
        body: workspace_core::storage::PreparedSendBody::Markdown(request.input.markdown.clone()),
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
        body: MessageBody::Markdown("Проверка отправки".into()),
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
