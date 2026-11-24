//! Приёмка создания канала, его панели сведений, ротации и локального удаления ключей в GPUI.

use super::*;
use workspace_core::messenger::encryption::DecryptionError;

/// Сервер фиксирует канал и объявления под точными идентификаторами из запроса.
#[derive(Default)]
pub(super) struct Fixture {
    stream: Mutex<Option<Value>>,     // Нативный канал после первого POST.
    announcements: Mutex<Vec<Value>>, // Подтверждённые сервером объявления поколения.
    creates: AtomicUsize,             // Число POST создания, включая потерю подтверждения.
    invites: AtomicUsize,             // Число приглашений из возобновлённого намерения.
    reject_first: bool, // Первый ответ возвращает окончательный отказ без создания канала.
    requested_ids: Mutex<Vec<Uuid>>, // Сравнивает новый UUID и идентичный повтор операции.
}

impl Fixture {
    /// Возвращает нативные проекции по точному ID и имитирует сбой первой доставки статуса.
    pub(super) async fn response(
        &self,
        request: Request<Incoming>,
        user: &str,
    ) -> Response<Full<Bytes>> {
        let path = request.uri().path().to_owned();
        let method = request.method().clone();
        if path == STREAMS && method == Method::POST {
            let post = parse(request).await;
            assert_eq!(post["encryption"], true);
            assert_eq!(post["source_name"], "native");
            assert_eq!(post["name"], "GPUI encrypted channel");
            let id: Uuid = serde_json::from_value(post["uuid"].clone()).unwrap();
            let key: Uuid =
                serde_json::from_value(post["current_encryption_key_uuid"].clone()).unwrap();
            assert!(!id.is_nil() && !key.is_nil());
            self.requested_ids.lock().unwrap().push(id);
            let attempt = self.creates.fetch_add(1, Ordering::SeqCst);
            if self.reject_first && attempt == 0 {
                return json_response(StatusCode::BAD_REQUEST, Value::Null);
            }
            *self.stream.lock().unwrap() = Some(json!({
                "uuid":id,"name":post["name"],"description":post["description"],"project_id":PROJECT,
                "user_uuid":user,"owner":user,"source_name":"native","private":false,
                "invite_only":false,"is_archived":false,"encryption":true,
                "notification_mode":"all_messages","default_topic_uuid":TOPIC,
                "current_encryption_key_uuid":key,"allow_key_sharing_by_holders":true,
            }));
            return json_response(StatusCode::SERVICE_UNAVAILABLE, Value::Null);
        }

        let stream = self.stream.lock().unwrap().clone();
        let id = stream.as_ref().and_then(|stream| stream["uuid"].as_str());
        if path == TOPICS {
            return json_response(
                StatusCode::OK,
                json!(id.into_iter().map(|id| topic(id, user)).collect::<Vec<_>>()),
            );
        }
        if path == format!("{TOPICS}{TOPIC}") {
            return json_response(
                if id.is_some() {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                },
                id.map_or(Value::Null, |id| topic(id, user)),
            );
        }
        if path == STREAMS {
            return json_response(
                StatusCode::OK,
                json!(stream.into_iter().collect::<Vec<_>>()),
            );
        }
        if let Some(id) = id {
            if path == format!("{STREAMS}{id}/actions/add_users/invoke") && method == Method::POST {
                let post = parse(request).await;
                assert_eq!(post["member"], json!([PEER]));
                self.invites.fetch_add(1, Ordering::SeqCst);
                return json_response(StatusCode::OK, json!({}));
            }
            if path == format!("{STREAMS}{id}") && method == Method::PUT {
                let patch = parse(request).await;
                let mut stream = self.stream.lock().unwrap();
                let stream = stream.as_mut().unwrap();
                let next = !stream["allow_key_sharing_by_holders"].as_bool().unwrap();
                assert_eq!(patch, json!({ "allow_key_sharing_by_holders": next }));
                stream["allow_key_sharing_by_holders"] = next.into();
                return json_response(StatusCode::OK, stream.clone());
            }
            if path == format!("{STREAMS}{id}") {
                return json_response(StatusCode::OK, stream.unwrap());
            }
            if path == BINDINGS {
                let mut bindings = vec![
                    json!({"uuid":CHANNEL_OWNER_BINDING,"user_uuid":USER,"stream_uuid":id,"project_id":PROJECT,"role":"owner"}),
                ];
                if self.invites.load(Ordering::SeqCst) != 0 {
                    bindings.push(json!({"uuid":CHANNEL_BINDING,"user_uuid":PEER,"stream_uuid":id,"project_id":PROJECT,"role":"member"}));
                }
                return json_response(StatusCode::OK, json!(bindings));
            }
            if path == MESSAGES && method == Method::POST {
                let post = parse(request).await;
                assert_eq!(post["payload"]["kind"], "key_announced");
                assert_eq!(post["stream_uuid"], id);
                assert_eq!(post["topic_uuid"], TOPIC);
                let canonical = post["uuid"].as_str().unwrap();
                let public = Uuid::new_v5(&Uuid::parse_str(TOPIC).unwrap(), canonical.as_bytes());
                let message = json!({
                    "uuid":public,"canonical_message_uuid": canonical,"source_name":"native",
                    "stream_uuid":id,"topic_uuid":TOPIC,"author_uuid":user,"user_uuid":user,
                    "project_id":PROJECT,"created_at":"2026-11-17T10:00:00Z","payload":post["payload"],
                });
                self.stream.lock().unwrap().as_mut().unwrap()["current_encryption_key_uuid"] =
                    post["payload"]["key_uuid"].clone();
                self.announcements.lock().unwrap().push(message.clone());
                return json_response(StatusCode::CREATED, message);
            }
            if path == MESSAGES {
                return json_response(
                    StatusCode::OK,
                    json!(self.announcements.lock().unwrap().clone()),
                );
            }
            if let Some(message) = path.strip_prefix(MESSAGES) {
                let list = self.announcements.lock().unwrap();
                return match list
                    .iter()
                    .find(|row| row["uuid"].as_str() == Some(message))
                {
                    Some(row) => json_response(StatusCode::OK, row.clone()),
                    None => json_response(StatusCode::NOT_FOUND, Value::Null),
                };
            }
        }
        let response = match path.as_str() {
            FOLDERS => json!([folder_row(Uuid::nil(), "Все чаты", "all", 0)]),
            "/api/workspace/v1/events/" | MESSAGES => json!([]),
            "/api/workspace/v1/events/notifications/" => json!([]),
            "/api/workspace/v1/epoch/" => json!({"epoch_generation":"local","epoch_version":1}),
            "/api/workspace/v1/users/" => {
                json!([profile(USER, "Creator"), profile(PEER, "Invitee")])
            }
            path if path.ends_with(USER) => profile(USER, "Creator"),
            path if path.ends_with(PEER) => profile(PEER, "Invitee"),
            _ => return json_response(StatusCode::NOT_FOUND, Value::Null),
        };
        json_response(StatusCode::OK, response)
    }
}

/// Входит в защищённый сценарий из общего действия создания канала.
fn open_channel_forms(ui: &mut VisualTestContext) {
    until(ui, "доступен плюс в каталоге", |ui| {
        ui.debug_bounds("new-chats-open").is_some()
    });
    click(ui, "new-chats-open");
    until(ui, "доступно создание канала", |ui| {
        ui.debug_bounds("new-chats-create-channel").is_some()
    });
    click(ui, "new-chats-create-channel");
    until(ui, "форма предлагает защищённый канал", |ui| {
        ui.debug_bounds("e2ee-create-channel").is_some()
    });
}

/// Терминальный отказ возвращает форму к новой операции с другим серверным UUID.
#[gpui::test]
fn rejected_channel_creation_allows_a_new_attempt(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        lifecycle_wire: Some(Fixture { reject_first: true, ..Default::default() }),
        ..Default::default()
    });
    let fixture = server.state.lifecycle_wire.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) = start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) = cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |view, cx| view.attach(subscription, cx));
    });
    open_channel_forms(ui);
    click(ui, "new-channel-name");
    ui.simulate_input("GPUI encrypted channel");
    click(ui, "e2ee-create-channel");
    let member = format!("e2ee-invite-member-{PEER}").leak();
    until(ui, "форма готова", |ui| ui.debug_bounds(member).is_some());
    click(ui, "e2ee-channel-submit");
    until(ui, "форма показывает известный отказ без продолжения", |ui| {
        fixture.creates.load(Ordering::SeqCst) == 1
            && ui.debug_bounds("e2ee-channel-failure").is_some()
            && ui.debug_bounds("e2ee-channel-resume").is_none()
    });
    assert!(fixture.stream.lock().unwrap().is_none());
    click(ui, "e2ee-channel-submit");
    until(ui, "сервер получил новый запрос", |_| fixture.creates.load(Ordering::SeqCst) == 2);
    let requested = fixture.requested_ids.lock().unwrap().clone();
    assert_ne!(requested[0], requested[1]);
    assert_eq!(futures::executor::block_on(runtime.shutdown()), RuntimeExit::Clean);
}

/// Декодирует только синтетический HTTP body в памяти.
async fn parse(request: Request<Incoming>) -> Value {
    serde_json::from_slice(&request.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

/// Подтверждённая открытая тема только что созданного канала.
fn topic(id: &str, user: &str) -> Value {
    json!({"uuid":TOPIC,"stream_uuid":id,"project_id":PROJECT,"user_uuid":user,"name":"Общая",
        "is_default":true,"is_done":false,"notification_mode":"default"})
}

/// Публичный справочник синтетического проекта.
fn profile(id: &str, name: &str) -> Value {
    json!({"uuid":id,"username":name.to_lowercase(),"first_name":name,"status":"idle"})
}

/// Созданный канал открывает защищённые сведения и позволяет удалить только выбранное поколение.
#[gpui::test]
fn encrypted_channel_creation_rotation_and_removal_use_normal_routes(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        lifecycle_wire: Some(Fixture::default()),
        ..Received::default()
    });
    let wire = server.state.lifecycle_wire.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1250.), px(900.)));
    open_channel_forms(ui);
    click(ui, "e2ee-create-channel");
    until(ui, "форма готова", |ui| {
        ui.debug_bounds("e2ee-channel-name").is_some()
    });
    let invite = format!("e2ee-invite-member-{PEER}").leak();
    until(
        ui,
        "справочник позволяет выбрать участника",
        |ui| ui.debug_bounds(invite).is_some(),
    );
    click(ui, "e2ee-channel-name");
    ui.simulate_input("GPUI encrypted channel");
    click(ui, invite);
    click(ui, "e2ee-channel-submit");
    until(ui, "сервер принял создание", |_| {
        wire.creates.load(Ordering::SeqCst) == 1
    });
    until(
        ui,
        "кнопка продолжения отображается после потери статуса",
        |ui| ui.debug_bounds("e2ee-channel-resume").is_some(),
    );
    click(ui, "e2ee-channel-resume");
    until(
        ui,
        "открыта точная тема, показан защищённый редактор и возвращён каталог",
        |ui| {
            wire.invites.load(Ordering::SeqCst) == 1
                && ui.debug_bounds("workspace-composer-editor").is_some()
                && ui.read(|app| {
                    !client
                        .read(app)
                        .sidebar
                        .read(app)
                        .new_chats()
                        .read(app)
                        .is_open()
                })
        },
    );
    assert_eq!(wire.creates.load(Ordering::SeqCst), 1);
    let current = wire.stream.lock().unwrap().clone().unwrap();
    let stream: Uuid = serde_json::from_value(current["uuid"].clone()).unwrap();
    let key_id: Uuid =
        serde_json::from_value(current["current_encryption_key_uuid"].clone()).unwrap();
    let topic_id = TOPIC.parse().unwrap();
    assert!(ui.read(|app| {
        let scope = client.read(app).shell.conversation().unwrap().route.scope;
        scope.stream_id == stream && scope.topic_id == Some(topic_id)
    }));
    let shown = |ui: &mut VisualTestContext, id| ui.debug_bounds(id).is_some();
    let screen = |ui: &VisualTestContext| {
        ui.read(|app| {
            client
                .read(app)
                .conversation
                .as_ref()
                .and_then(|view| view.read(app).tested_details_screen(app))
        })
    };
    let modal_open =
        |ui: &VisualTestContext| ui.read(|app| client.read(app).right_panel.read(app).has_modal());
    const INFO: &str = "conversation-encrypted-info";
    const SETTINGS: &str = "conversation-encrypted-settings";
    const SETTINGS_SCREEN: &str = "conversation-encrypted-settings-screen";
    const SHARING: &str = "conversation-encrypted-settings-key-sharing";
    const KEYS_ROW: &str = "conversation-encrypted-settings-keys";
    const LOCAL_KEYS: &str = "conversation-local-keys";
    const FILES: &str = "conversation-gallery-open-files";
    const ROTATE: &str = "conversation-rotate-key";
    const CONFIRM: &str = "conversation-key-confirm";
    const CANCEL: &str = "conversation-key-cancel";
    click(ui, DETAILS_TRIGGER);
    until(
        ui,
        "в защищённых сведениях видны настройки, текущий ключ, участники и вложения",
        |ui| {
            screen(ui) == Some("info")
                && [
                    INFO,
                    SETTINGS,
                    ROTATE,
                    "conversation-details-members",
                    FILES,
                ]
                .into_iter()
                .all(|id| shown(ui, id))
        },
    );
    assert!(!shown(ui, "conversation-details-channel"));
    assert!(!shown(ui, "details-topics-toggle"));
    assert!(!shown(ui, LOCAL_KEYS));
    click(ui, FILES);
    until(
        ui,
        "категория файлов открылась в той же панели",
        |ui| screen(ui) == Some("gallery") && shown(ui, "conversation-gallery-panel"),
    );
    click(ui, "conversation-gallery-back");
    until(
        ui,
        "из файлов вернулись в сведения",
        |ui| screen(ui) == Some("info") && shown(ui, FILES),
    );
    click(ui, ROTATE);
    until(
        ui,
        "открыто подтверждение ротации",
        |ui| modal_open(ui) && shown(ui, CONFIRM),
    );
    ui.simulate_keystrokes("escape");
    until(ui, "Escape отменил ротацию", |ui| {
        !modal_open(ui)
    });
    assert!(wire.announcements.lock().unwrap().is_empty());
    click(ui, ROTATE);
    until(
        ui,
        "ротацию можно подтвердить повторно",
        |ui| modal_open(ui) && shown(ui, CONFIRM),
    );
    click(ui, CONFIRM);
    until(
        ui,
        "сервер подтвердил объявление и окно ротации закрылось",
        |ui| !wire.announcements.lock().unwrap().is_empty() && !modal_open(ui),
    );
    let new_id: Uuid = serde_json::from_value(
        wire.announcements.lock().unwrap()[0]["payload"]["key_uuid"].clone(),
    )
    .unwrap();
    assert_ne!(new_id, key_id);

    click(ui, SETTINGS);
    until(
        ui,
        "настройки открылись отдельным экраном",
        |ui| screen(ui) == Some("settings") && shown(ui, SETTINGS_SCREEN) && shown(ui, SHARING),
    );
    click(ui, SHARING);
    until(
        ui,
        "в сведениях подтверждён сервером запрет выдачи ключей",
        |ui| {
            ui.read(|app| {
                let conversation = client.read(app).conversation.as_ref().unwrap().read(app);
                conversation.tested_details(app, |snapshot| {
                    matches!(snapshot, Some(SidebarDetails::Channel(channel))
                    if channel.allow_key_sharing_by_holders == Some(false))
                })
            })
        },
    );
    assert!(!shown(
        ui,
        "conversation-encrypted-settings-key-sharing-error"
    ));
    click(ui, KEYS_ROW);
    until(
        ui,
        "из настроек открыли экран ключей",
        |ui| screen(ui) == Some("keys") && shown(ui, LOCAL_KEYS),
    );
    ui.simulate_keystrokes("escape");
    until(
        ui,
        "Escape из ключей вернул настройки",
        |ui| screen(ui) == Some("settings"),
    );
    press_enter(ui);
    until(
        ui,
        "экран ключей снова доступен",
        |ui| screen(ui) == Some("keys"),
    );
    click(ui, LOCAL_KEYS);
    let old_row = format!("conversation-local-key-{key_id}").leak();
    let new_row = format!("conversation-local-key-{new_id}").leak();
    until(
        ui,
        "старое и новое поколения доступны локально",
        |ui| shown(ui, old_row) && shown(ui, new_row),
    );
    click(ui, old_row);
    let remove_old = format!("conversation-remove-key-{key_id}").leak();
    until(
        ui,
        "открыта карточка конкретного локального ключа",
        |ui| {
            screen(ui) == Some("local-key")
                && shown(ui, "conversation-local-key-details")
                && shown(ui, remove_old)
        },
    );
    let account = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let (engine, activation) = runtime.message_encryption(&account).unwrap();
    click(ui, remove_old);
    until(ui, "удаление можно отменить", |ui| {
        modal_open(ui) && shown(ui, CANCEL)
    });
    click(ui, CANCEL);
    until(
        ui,
        "после отмены открыта прежняя карточка",
        |ui| screen(ui) == Some("local-key") && !modal_open(ui),
    );
    assert!(engine.prepare(&activation, stream, key_id).is_ok());
    click(ui, remove_old);
    until(
        ui,
        "удаление можно подтвердить",
        |ui| modal_open(ui) && shown(ui, CONFIRM),
    );
    click(ui, CONFIRM);
    until(
        ui,
        "удалено только старое поколение и снова виден список ключей",
        |ui| {
            screen(ui) == Some("keys")
                && !modal_open(ui)
                && matches!(
                    engine.prepare(&activation, stream, key_id),
                    Err(DecryptionError::MissingKey)
                )
                && engine.prepare(&activation, stream, new_id).is_ok()
        },
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
