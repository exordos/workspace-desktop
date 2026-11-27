//! Проверяет новый личный чат и создание канала из сайдбара через Core и локальный HTTPS.
//! После команды добавления сервер один раз теряет подтверждение, чтобы проверить повтор.

mod archive;

use super::*;

const CHANNEL_NAME: &str = "Технические заметки";
const CHANNEL_DESCRIPTION: &str = "Описание нового канала";
const CHOOSE_PEER: &str = "new-chats-person-10000000-0000-4000-8000-000000000002";
const CHOOSE_INVITEE: &str = "new-chats-person-10000000-0000-4000-8000-000000000011";
const USERS: &str = "/api/workspace/v1/users/";

/// Даёт серверу память о новых стримах и командах, наблюдаемую из теста.
#[derive(Default)]
pub(super) struct Fixture {
    direct_posts: AtomicUsize, // Сколько POST создания лички увидел сервер.
    channel_posts: Mutex<Vec<Uuid>>, // Сколько UUID получили в командах создания канала.
    invites: Mutex<Vec<Uuid>>, // UUID канала для каждой команды приглашения.
    hold_direct: Mutex<Option<oneshot::Receiver<()>>>, // Задерживает создание до смены входа.
    direct_released: AtomicBool, // Сервер покинул ожидание и может вернуть ответ.
    archive_direct: bool,      // В сценарии архива личка существует до первого запроса.
    restore_posts: AtomicUsize, // Число подтверждённых восстановлений из архива.
}

impl Fixture {
    /// Выбирает локальные маршруты, для которых тест задаёт серверное состояние.
    pub(super) fn handles(&self, request: &Request<Incoming>) -> bool {
        let path = request.uri().path();
        let channel = self.channel();
        path.starts_with(STREAMS)
            || path.starts_with(TOPICS)
            || path.starts_with(USERS)
            || path == BINDINGS && channel.is_some()
            || path == MESSAGES
                && request.method() == Method::GET
                && channel.is_some_and(|id| {
                    query_value(request, "stream_uuid") == Some(id.to_string().as_str())
                })
    }

    /// Возвращает подтверждённый UUID канала, если сервер принял команду.
    fn channel(&self) -> Option<Uuid> {
        self.channel_posts.lock().unwrap().last().copied()
    }

    /// Готовит представление существующей лички с актуальным признаком архива.
    fn existing_direct(&self, user: &str, peer: &str) -> Value {
        let mut row = direct_row(user, peer);
        row["is_archived"] =
            json!(self.archive_direct && self.restore_posts.load(Ordering::SeqCst) == 0);
        row
    }

    /// Отвечает на страницы справочника, создание и чтение подтверждённых стримов.
    pub(super) async fn response(
        &self,
        request: Request<Incoming>,
        user: &str,
        peer: &str,
    ) -> Response<Full<Bytes>> {
        let path = request.uri().path();
        if request.method() == Method::POST && path == STREAMS {
            let body = request_body(request).await;
            assert_eq!(body["source_name"], "native");
            if let Some(target) = body.get("direct_user_uuid") {
                assert_eq!((user, target.as_str()), (USER, Some(PEER)));
                assert!(body.get("uuid").is_none());
                self.direct_posts.fetch_add(1, Ordering::SeqCst);
                let waiting = self.hold_direct.lock().unwrap().take();
                if let Some(waiting) = waiting {
                    let _ = waiting.await;
                }
                self.direct_released.store(true, Ordering::SeqCst);
                return json_response(StatusCode::CREATED, direct_row(user, peer));
            }
            let channel: Uuid = serde_json::from_value(body["uuid"].clone()).unwrap();
            assert_eq!(body["name"], CHANNEL_NAME);
            assert_eq!(body["description"], CHANNEL_DESCRIPTION);
            let mut posts = self.channel_posts.lock().unwrap();
            posts.push(channel);
            if posts.len() == 1 {
                return json_response(StatusCode::SERVICE_UNAVAILABLE, json!({}));
            }
            assert_eq!(posts[0], channel, "повтор должен сохранить UUID создания");
            return json_response(StatusCode::CREATED, channel_row(channel, user));
        }
        if let Some(channel) = self.channel()
            && path == format!("{STREAMS}{channel}/actions/add_users/invoke")
        {
            assert_eq!((request.method(), user), (&Method::POST, USER));
            assert_eq!(request_body(request).await["member"], json!([INVITEE]));
            let mut invites = self.invites.lock().unwrap();
            invites.push(channel);
            return if invites.len() == 1 {
                json_response(StatusCode::SERVICE_UNAVAILABLE, json!({}))
            } else {
                json_response(StatusCode::OK, json!([]))
            };
        }
        if self.archive_direct && path == format!("{STREAMS}{STREAM}/actions/unarchive/invoke") {
            assert_eq!(request.method(), Method::POST);
            self.restore_posts.fetch_add(1, Ordering::SeqCst);
            return json_response(StatusCode::OK, self.existing_direct(user, peer));
        }
        let direct_exists = self.archive_direct || self.direct_posts.load(Ordering::SeqCst) != 0;
        let channel = self.channel();
        let result = if path == STREAMS {
            let mut rows = Vec::new();
            let direct = query_value(&request, "direct_user_uuid");
            if direct_exists && direct.is_none_or(|id| id == peer) {
                rows.push(self.existing_direct(user, peer));
            }
            if direct.is_none()
                && let Some(channel) = channel
            {
                rows.push(channel_row(channel, user));
            }
            json!(rows)
        } else if path == format!("{STREAMS}{STREAM}") && direct_exists {
            self.existing_direct(user, peer)
        } else if let Some(channel) = channel
            && path == format!("{STREAMS}{channel}")
        {
            channel_row(channel, user)
        } else if path == TOPICS {
            json!(
                channel
                    .map(|id| topic_row(id, CHANNEL_DEFAULT, user))
                    .into_iter()
                    .collect::<Vec<_>>()
            )
        } else if path == format!("{TOPICS}{TOPIC}") && direct_exists {
            topic_row(STREAM.parse().unwrap(), TOPIC, user)
        } else if let Some(channel) = channel
            && path == format!("{TOPICS}{CHANNEL_DEFAULT}")
        {
            topic_row(channel, CHANNEL_DEFAULT, user)
        } else if path == USERS {
            json!([
                ChannelMembership::profile(USER),
                ChannelMembership::profile(PEER),
                ChannelMembership::profile(INVITEE),
            ])
        } else if let Some(id) = path.strip_prefix(USERS)
            && [USER, PEER, INVITEE].contains(&id)
        {
            ChannelMembership::profile(id)
        } else if let Some(channel) = channel
            && path == BINDINGS
        {
            json!([{
                "uuid": CHANNEL_OWNER_BINDING, "stream_uuid": channel, "project_id": PROJECT,
                "user_uuid": USER, "role": "owner",
            }])
        } else if path == MESSAGES {
            json!([])
        } else {
            return json_response(StatusCode::NOT_FOUND, Value::Null);
        };
        json_response(StatusCode::OK, result)
    }
}

/// Разбирает JSON POST тем же синтетическим HTTP-сервером.
async fn request_body(request: Request<Incoming>) -> Value {
    let bytes = request.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Возвращает личку с представлением участников для текущего токена.
fn direct_row(user: &str, peer: &str) -> Value {
    json!({
        "uuid": STREAM, "project_id": PROJECT, "user_uuid": user, "owner": user,
        "name": "Тестовая личка", "source_name": "native",
        "private": true, "direct_user_uuid": peer, "is_archived": false,
        "default_topic_uuid": TOPIC, "encryption": false, "notification_mode": "all_messages",
    })
}

/// Готовит канал по UUID, который прислал настоящий рабочий клиент.
fn channel_row(channel: Uuid, user: &str) -> Value {
    let mut row = topic_channel(user, false);
    row["uuid"] = json!(channel);
    row["owner"] = json!(USER);
    row["source_name"] = json!("native");
    row["name"] = json!(CHANNEL_NAME);
    row
}

/// Подтверждает принадлежность начальной темы новому стриму.
fn topic_row(stream: Uuid, id: &str, user: &str) -> Value {
    let mut row = channel_topic(user, TopicGate::Open);
    row["stream_uuid"] = json!(stream);
    row["uuid"] = json!(id);
    row["is_default"] = json!(true);
    row
}

/// Нажимает действие панели после отрисовки его состояния.
fn wait_click(ui: &mut VisualTestContext, selector: &'static str) {
    until(ui, selector, |ui| ui.debug_bounds(selector).is_some());
    click(ui, selector);
}

/// Выбирает собеседника после загрузки текущего списка людей, включая повторное открытие.
fn choose_peer(client: &Entity<Client>, ui: &mut VisualTestContext) {
    wait_click(ui, "new-chats-open");
    wait_click(ui, "new-chats-start-chat");
    // GPUI может хранить координаты строки из прошлого кадра, когда новый список ещё загружается.
    until(
        ui,
        "текущий список коллег загружен",
        |ui| {
            ui.read(|app| {
                client
                    .read(app)
                    .sidebar
                    .read(app)
                    .new_chats()
                    .read(app)
                    .people_ready_for_test()
            })
        },
    );
    wait_click(ui, CHOOSE_PEER);
}

/// Вставляет текст в настоящее активное поле формы или редактора.
fn paste(ui: &mut VisualTestContext, value: &str) {
    ui.update(|_, app| app.write_to_clipboard(ClipboardItem::new_string(value.to_string())));
    ui.simulate_keystrokes("secondary-v");
    ui.run_until_parked();
}

/// Создание лички приводит к штатному редактору и второй выбор открывает тот же UUID.
#[gpui::test]
fn new_chats_creates_direct_and_sends_first_message(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        new_chats: Some(Fixture::default()),
        ..Received::default()
    });
    let fixture = server.state.new_chats.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(ui, "действующий вход", |_| {
        server.state.ready.load(Ordering::SeqCst) > 0
    });
    choose_peer(&client, ui);
    wait_click(ui, "workspace-composer-editor");
    paste(ui, MARKER);
    wait_click(ui, "conversation-send");
    until(
        ui,
        "первое сообщение дошло до сервера",
        |_| !server.state.sent.lock().unwrap().is_empty(),
    );
    assert_eq!(server.state.sent.lock().unwrap().len(), 1);
    assert_eq!(fixture.direct_posts.load(Ordering::SeqCst), 1);
    let original = ui.read(|app| client.read(app).shell.conversation().unwrap().clone());
    choose_peer(&client, ui);
    until(
        ui,
        "та же личка открылась из списка людей",
        |ui| {
            ui.read(|app| {
                client
                    .read(app)
                    .shell
                    .conversation()
                    .is_some_and(|address| {
                        address.revision != original.revision && address.route == original.route
                    })
            })
        },
    );
    assert_eq!(fixture.direct_posts.load(Ordering::SeqCst), 1);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Повтор приглашения после ошибки использует уже созданный канал и открывает его начальную тему.
#[gpui::test]
fn new_chats_retries_adding_members_to_the_created_channel(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        new_chats: Some(Fixture::default()),
        ..Received::default()
    });
    let fixture = server.state.new_chats.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(ui, "действующий вход", |_| {
        server.state.ready.load(Ordering::SeqCst) > 0
    });
    wait_click(ui, "new-chats-open");
    wait_click(ui, "new-chats-create-channel");
    wait_click(ui, "new-channel-name");
    paste(ui, CHANNEL_NAME);
    wait_click(ui, "new-channel-description");
    paste(ui, CHANNEL_DESCRIPTION);
    wait_click(ui, "new-channel-create");
    until(
        ui,
        "сервер не подтвердил создание",
        |ui| ui.debug_bounds("new-channel-error").is_some(),
    );
    wait_click(ui, "new-channel-create");
    wait_click(ui, CHOOSE_INVITEE);
    wait_click(ui, "new-channel-add-members");
    until(
        ui,
        "ошибка добавления участников в исходной форме",
        |ui| ui.debug_bounds("new-channel-members-error").is_some(),
    );
    wait_click(ui, "new-channel-add-members");
    let channel = fixture.channel().unwrap();
    until(
        ui,
        "открыта начальная тема созданного канала",
        |ui| {
            ui.read(|app| {
                client.read(app).conversation.as_ref().is_some_and(|view| {
                    let view = view.read(app);
                    view.has_access()
                        && view.address().route.scope.stream_id == channel
                        && view.address().route.scope.topic_id
                            == Some(CHANNEL_DEFAULT.parse().unwrap())
                })
            })
        },
    );
    assert_eq!(*fixture.channel_posts.lock().unwrap(), [channel, channel]);
    assert_eq!(*fixture.invites.lock().unwrap(), [channel, channel]);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// После смены входа ответ создания не открывает в чужом контексте уже записанный стрим.
#[gpui::test]
fn new_chats_switch_account_ignores_late_creation(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let (release, waiting) = oneshot::channel();
    let server = Server::serve(Received {
        new_chats: Some(Fixture {
            hold_direct: Mutex::new(Some(waiting)),
            ..Fixture::default()
        }),
        ..Received::default()
    });
    let fixture = server.state.new_chats.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let a = saved_account(&server, USER, Uuid::nil());
    let b = saved_account(&server, PEER, PEER.parse().unwrap());
    let next_account = b.context.account_id.clone();
    JsonAccountStore::open(profile.path())
        .unwrap()
        .save(&b)
        .unwrap();
    let options = Options {
        active: Some(a.context.account_id.clone()),
        ..Options::default()
    };
    let (runtime, events) = start_runtime(profile.path(), a);
    let (client, ui) = cx.add_window_view(|_, cx| Client::new(options, runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(ui, "два действующих входа", |_| {
        server.state.ready.load(Ordering::SeqCst) >= 2
    });
    choose_peer(&client, ui);
    until(
        ui,
        "сервер принял создание лички исходного входа",
        |_| fixture.direct_posts.load(Ordering::SeqCst) == 1,
    );
    click_account_button(&client, ui, &next_account);
    until(ui, "выбран другой аккаунт", |ui| {
        ui.read(|app| {
            client
                .read(app)
                .shell
                .selected()
                .is_some_and(|session| session.owner.account_id == next_account)
        })
    });
    let _ = release.send(());
    until(
        ui,
        "сервер обработал позднее подтверждение",
        |_| fixture.direct_released.load(Ordering::SeqCst),
    );
    ui.run_until_parked();
    ui.read(|app| {
        let shell = &client.read(app).shell;
        assert_eq!(shell.selected().unwrap().owner.account_id, next_account);
        assert!(shell.conversation().is_none());
    });
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
