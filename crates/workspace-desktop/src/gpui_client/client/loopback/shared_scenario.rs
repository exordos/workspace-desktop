//! Проверяет общие действия отправителя и получателя на локальном и настоящем сервере.
//! Фикстура ниже хранит один обмен, а сценарий использует одинаковые действия GPUI.

use super::*;
use std::path::Path;

/// Даёт серверу время на работу и регулярно обрабатывает события тестового окна.
fn wait_for(
    ui: &mut VisualTestContext,
    step: &str,
    mut ready: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        ui.run_until_parked();
        if ready(ui) {
            return;
        }
        assert!(Instant::now() < deadline, "Нет результата шага: {step}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Хранит отправленное сообщение и подтверждённое чтение получателя.
pub(super) struct Fixture {
    message: Mutex<Option<Value>>, // Серверная строка сообщения, отправленная участником A.
    read: AtomicBool,              // Сервер подтвердил чтение участником B.
    posts: AtomicUsize,            // Количество попыток отправки через сервер.
    pub(super) live: broadcast::Sender<RecoverySignal>, // Изменения счётчика получателя.
}

impl Default for Fixture {
    /// Готовит пустой разговор с независимой подпиской событий.
    fn default() -> Self {
        Self {
            message: Mutex::new(None),
            read: AtomicBool::new(false),
            posts: AtomicUsize::new(0),
            live: broadcast::channel(16).0,
        }
    }
}

impl Fixture {
    /// Выбирает только запросы истории и отправки общего сценария.
    pub(super) fn handles(&self, request: &Request<Incoming>) -> bool {
        let path = request.uri().path();
        path.starts_with(MESSAGES) || path == FOLDERS || path == format!("{FOLDERS}{}", Uuid::nil())
    }

    /// Сохраняет отправку и отвечает историей и подтверждённым состоянием чтения.
    pub(super) async fn response(
        &self,
        request: Request<Incoming>,
        user: &str,
    ) -> Response<Full<Bytes>> {
        let path = request.uri().path().to_owned();
        if path.starts_with(FOLDERS) {
            let count = u32::from(
                user == PEER
                    && self.message.lock().unwrap().is_some()
                    && !self.read.load(Ordering::SeqCst),
            );
            let folder = folder_row(Uuid::nil(), "Все чаты", "all", count);
            return json_response(
                StatusCode::OK,
                if path == FOLDERS {
                    json!([folder])
                } else {
                    folder
                },
            );
        }
        if request.method() == Method::POST && path == MESSAGES {
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            let post: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(user, USER);
            assert_eq!(post["stream_uuid"], STREAM);
            assert_eq!(post["topic_uuid"], TOPIC);
            self.posts.fetch_add(1, Ordering::SeqCst);
            let id = Uuid::new_v5(
                &Uuid::parse_str(TOPIC).unwrap(),
                post["uuid"].as_str().unwrap().as_bytes(),
            );
            let row = json!({"uuid":id,"stream_uuid":STREAM,"topic_uuid":TOPIC,"user_uuid":USER,
                "project_id":PROJECT,"author_uuid":USER,"source_name":"native", "read":true,
                "created_at":"2026-11-25T10:00:00.000001Z","payload":post["payload"]});
            *self.message.lock().unwrap() = Some(row.clone());
            return json_response(StatusCode::CREATED, row);
        }
        if request.method() == Method::POST && path.ends_with("/actions/read_up_to/invoke") {
            assert_eq!(user, PEER);
            let message = self.message.lock().unwrap().clone().unwrap();
            let requested = path
                .strip_prefix(MESSAGES)
                .and_then(|part| part.strip_suffix("/actions/read_up_to/invoke"))
                .expect("адрес границы чтения");
            assert_eq!(Some(requested), message["uuid"].as_str());
            assert_eq!(message["topic_uuid"], TOPIC);
            self.read.store(true, Ordering::SeqCst);
            let frame = json!({"schema_version":1,"uuid":Uuid::new_v4(),"epoch_generation":"local",
                "epoch_version":2,"project_id":PROJECT,"user_uuid":PEER,"object_type":"folder",
                "action":"updated","payload":{"kind":"folder.updated","uuid":Uuid::nil(),"unread_count":0}});
            let _ = self.live.send(RecoverySignal::Event(frame));
            let mut row = message;
            row["user_uuid"] = json!(user);
            row["read"] = json!(true);
            return json_response(StatusCode::OK, row);
        }
        if request.method() == Method::GET {
            let mut row = self.message.lock().unwrap().clone();
            if let Some(row) = &mut row {
                row["user_uuid"] = json!(user);
                row["read"] = json!(user == USER || self.read.load(Ordering::SeqCst));
            }
            if path == MESSAGES {
                if query_value(&request, "read") == Some("false")
                    && (user == USER || self.read.load(Ordering::SeqCst))
                {
                    row = None;
                }
                return json_response(StatusCode::OK, json!(row.into_iter().collect::<Vec<_>>()));
            }
            if let Some(row) = row {
                if row["uuid"].as_str() == path.strip_prefix(MESSAGES) {
                    return json_response(StatusCode::OK, row);
                }
            }
        }
        json_response(StatusCode::NOT_FOUND, Value::Null)
    }
}

/// Находит сообщение в подтверждённой истории выбранного разговора.
fn history(
    runtime: &RuntimeClient,
    address: &ConversationAddress,
    text: &str,
) -> Option<(Uuid, Option<bool>, usize)> {
    let address = address.clone();
    let text = text.to_owned();
    futures::executor::block_on(runtime.request(move |services| async move {
        let rows = services
            .sync
            .history_windows(
                &address.route.owner,
                address.generation,
                address.route.scope,
            )
            .ok()?
            .into_iter()
            .flat_map(|window| window.messages)
            .filter(|row| row.body.markdown() == Some(text.as_str()))
            .collect::<Vec<_>>();
        let ids: std::collections::HashSet<_> = rows.iter().map(|row| row.id).collect();
        rows.first().map(|row| (row.id, row.read, ids.len()))
    }))
    .ok()
    .flatten()
}

/// Находит личный чат другого участника и открывает его обычным кликом.
fn open_chat(
    client: &Entity<Client>,
    ui: &mut VisualTestContext,
    peer: Uuid,
) -> ConversationAddress {
    let mut stream = None;
    wait_for(
        ui,
        "личный чат появился в каталоге",
        |ui| {
            stream = ui.read(|app| {
                client
                    .read(app)
                    .shell
                    .selected_catalog()
                    .and_then(|catalog| catalog.chats.as_deref())
                    .and_then(|chats| {
                        chats
                            .iter()
                            .find(|chat| chat.direct && chat.metadata.direct_peer_id == Some(peer))
                    })
                    .map(|chat| chat.id)
            });
            stream.is_some()
        },
    );
    let selector: &'static str =
        Box::leak(format!("catalog-chat-{}", stream.unwrap()).into_boxed_str());
    wait_for(
        ui,
        "строка личного чата доступна",
        |ui| ui.debug_bounds(selector).is_some(),
    );
    click(ui, selector);
    wait_for(
        ui,
        "история и редактор открыты",
        |ui| ui.debug_bounds("workspace-composer-editor").is_some(),
    );
    ui.read(|app| client.read(app).shell.conversation().unwrap().clone())
}

/// Запускает одинаковый сценарий с двумя отдельными профилями и выбранной средой.
pub(super) fn run(
    cx: &mut TestAppContext,
    start: impl Fn(&Path, char) -> (RuntimeClient, RuntimeEvents),
    peer_a: Uuid,
    peer_b: Uuid,
) {
    cx.update(bind_keys);
    let baseline_profile = tempfile::tempdir().unwrap();
    let a_profile = tempfile::tempdir().unwrap();
    let b_profile = tempfile::tempdir().unwrap();
    let text = format!("Проверка отправки и чтения {}", Uuid::new_v4());
    let (baseline_runtime, baseline_events) = start(baseline_profile.path(), 'B');
    let baseline = {
        let (client, ui) = cx
            .add_window_view(|_, cx| Client::new(Options::default(), baseline_runtime.clone(), cx));
        ui.update(|_, app| {
            let subscription = baseline_events.attach(&client, app, Client::handle_runtime);
            client.update(app, |client, cx| client.attach(subscription, cx));
        });
        ui.simulate_resize(size(px(960.), px(700.)));
        let mut count = None;
        wait_for(
            ui,
            "начальный каталог и счётчик B загружены",
            |ui| {
                ui.read(|app| {
                    let client = client.read(app);
                    let ready = client.shell.selected_catalog().is_some_and(|catalog| {
                        catalog.status == workspace_core::bootstrap::ChatListStatus::Ready
                    });
                    if ready {
                        count = client
                            .shell
                            .accounts()
                            .first()
                            .and_then(|account| account.unread);
                    }
                    ready && count.is_some()
                })
            },
        );
        count.unwrap()
    };
    assert_eq!(
        futures::executor::block_on(baseline_runtime.shutdown()),
        RuntimeExit::Clean
    );
    let (a_runtime, a_events) = start(a_profile.path(), 'A');
    let sent = {
        let (client, ui) =
            cx.add_window_view(|_, cx| Client::new(Options::default(), a_runtime.clone(), cx));
        ui.update(|_, app| {
            let subscription = a_events.attach(&client, app, Client::handle_runtime);
            client.update(app, |client, cx| client.attach(subscription, cx));
        });
        ui.simulate_resize(size(px(960.), px(700.)));
        let address = open_chat(&client, ui, peer_b);
        click(ui, "workspace-composer-editor");
        ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string(text.clone())));
        ui.simulate_keystrokes("secondary-v");
        ui.run_until_parked();
        click(ui, "conversation-send");
        let mut id = None;
        wait_for(
            ui,
            "отправленное сообщение подтверждено в истории A",
            |_| {
                id = history(&a_runtime, &address, &text).map(|row| row.0);
                id.is_some()
            },
        );
        let id = id.unwrap();
        let selector: &'static str = Box::leak(format!("history-message-{id}").into_boxed_str());
        wait_for(
            ui,
            "отправленное сообщение видно в истории A",
            |ui| {
                let in_viewport = ui
                    .debug_bounds(selector)
                    .zip(ui.debug_bounds("conversation-history-focus"))
                    .is_some_and(|(row, viewport)| {
                        row.bottom() > viewport.top() && row.top() < viewport.bottom()
                    });
                in_viewport
                    && ui.read(|app| {
                        client
                            .read(app)
                            .conversation
                            .as_ref()
                            .is_some_and(|view| view.read(app).message_visible_for_test(id))
                    })
            },
        );
        assert_eq!(history(&a_runtime, &address, &text).unwrap().2, 1);
        id
    };
    assert_eq!(
        futures::executor::block_on(a_runtime.shutdown()),
        RuntimeExit::Clean
    );

    let (b_runtime, b_events) = start(b_profile.path(), 'B');
    {
        let (client, ui) =
            cx.add_window_view(|_, cx| Client::new(Options::default(), b_runtime.clone(), cx));
        ui.update(|_, app| {
            let subscription = b_events.attach(&client, app, Client::handle_runtime);
            client.update(app, |client, cx| client.attach(subscription, cx));
        });
        ui.simulate_resize(size(px(960.), px(700.)));
        wait_for(
            ui,
            "непрочитанное сообщение отражено в счётчике B",
            |ui| {
                ui.read(|app| {
                    client
                        .read(app)
                        .shell
                        .accounts()
                        .first()
                        .and_then(|account| account.unread)
                }) == baseline.checked_add(1)
            },
        );
        let address = open_chat(&client, ui, peer_a);
        wait_for(
            ui,
            "сообщение A загружено в историю B",
            |_| history(&b_runtime, &address, &text).is_some_and(|row| row.0 == sent),
        );
        let selector: &'static str = Box::leak(format!("history-message-{sent}").into_boxed_str());
        wait_for(
            ui,
            "сообщение A видно пользователю B",
            |ui| {
                let in_viewport = ui
                    .debug_bounds(selector)
                    .zip(ui.debug_bounds("conversation-history-focus"))
                    .is_some_and(|(row, viewport)| {
                        row.bottom() > viewport.top() && row.top() < viewport.bottom()
                    });
                in_viewport
                    && ui.read(|app| {
                        client
                            .read(app)
                            .conversation
                            .as_ref()
                            .is_some_and(|view| view.read(app).message_visible_for_test(sent))
                    })
            },
        );
        assert!(!ui.update(|window, _| window.is_window_active()));
        assert_eq!(
            history(&b_runtime, &address, &text),
            Some((sent, Some(false), 1))
        );
        ui.update(|window, _| window.activate_window());
        wait_for(
            ui,
            "сервер подтвердил чтение B",
            |_| {
                history(&b_runtime, &address, &text).is_some_and(|row| row == (sent, Some(true), 1))
            },
        );
        wait_for(
            ui,
            "счётчик B уменьшился после подтверждения",
            |ui| {
                ui.read(|app| {
                    client
                        .read(app)
                        .shell
                        .accounts()
                        .first()
                        .and_then(|account| account.unread)
                }) == Some(baseline)
            },
        );
    }
    assert_eq!(
        futures::executor::block_on(b_runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Проверяет отправку и чтение через два GPUI-клиента и общий тестовый HTTPS-сервер.
#[gpui::test]
fn shared_messenger_mock_workspace(cx: &mut TestAppContext) {
    let server = Server::serve(Received {
        shared: Some(Fixture::default()),
        ..Received::default()
    });
    run(
        cx,
        |profile, account| {
            let (user, nonce) = if account == 'A' {
                (USER, Uuid::nil())
            } else {
                (PEER, Uuid::parse_str(PEER).unwrap())
            };
            start_runtime(profile, saved_account(&server, user, nonce))
        },
        Uuid::parse_str(USER).unwrap(),
        Uuid::parse_str(PEER).unwrap(),
    );
    assert!(
        server
            .state
            .shared
            .as_ref()
            .unwrap()
            .read
            .load(Ordering::SeqCst)
    );
    assert_eq!(
        server
            .state
            .shared
            .as_ref()
            .unwrap()
            .posts
            .load(Ordering::SeqCst),
        1
    );
}

/// Выполняет тот же пользовательский сценарий на явно подготовленном тестовом Workspace.
#[gpui::test]
#[ignore = "нужен выделенный Workspace и тестовые аккаунты A/B"]
fn shared_messenger_real_workspace(cx: &mut TestAppContext) {
    run(
        cx,
        super::stand_scenario::start_stand_account,
        Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"workspace-desktop-stand.test/user-a"),
        Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"workspace-desktop-stand.test/user-b"),
    );
}
