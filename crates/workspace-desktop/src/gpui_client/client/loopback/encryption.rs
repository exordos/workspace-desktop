//! Открывает защищённую личку через штатный каталог и меняет только локальный ключ поколения.

use gpui::{ScrollDelta, ScrollWheelEvent, point};
use workspace_core::messenger::encryption::GroupKey;

use super::*;

pub(super) const KEY: &str = "10000000-0000-4000-8000-000000000080";
const OTHER_KEY: &str = "10000000-0000-4000-8000-000000000081";
const SEALED_A: Uuid = Uuid::from_u128(186);
const SEALED_B: Uuid = Uuid::from_u128(188);
const LOCKED_A: &str = "history-encrypted-00000000-0000-0000-0000-0000000000ba-locked";
const LOCKED_B: &str = "history-encrypted-00000000-0000-0000-0000-0000000000bc-locked";
const READABLE_A: &str = "history-encrypted-00000000-0000-0000-0000-0000000000ba-readable";
// Вектор получен через Node/OpenSSL, с тестовым ключом [7;32].
const CIPHERTEXT: &str = "AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM1BE3iAOkrgJ3KRf/RmAQZdeFqucTcLZYqkvZLUm+5hT2YqTPupmbNHVCmw77C/ZSMcN1WTelv3nhrIi1blpcecnNKAV8=";
// Синтетические конверты Node/OpenSSL с разными поколениями: цитата + изображение и точный оригинал.
const QUOTING_BODY: &str = "EhISEhISEhISEhISEhISEhISEhISEhISEhISEhISEhKZsZzRzZRZCPoKYtSe5bgmJGHoQaAWm/C5lgcAPZ288KTAmlAt9mhYiVKbWtAMyDHJw7BJ14k/2zhIxPz2edpuBBLO4/qH/zytbfH9yflnOOs80YiSRW/GZGuNpyzvLYkbdEkf+sY/D9fGzVBsxP1R5uqFXWMg6DPQRmWGSP6tnfpOlANP9UW3TDhhTbiVQqnv1Us0VFw/EOpT+FUT+mtiT38MbVu5C9cshw==";
const ORIGINAL_BODY: &str = "ExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExNlh3aiweSCej6Ps/7RbSdhRCcOJyjPT2W2YbGjU7hFYT7J0mu3gwZaUPLKdH/+nKhNIrmU8pw1t2v+VyV7R7Mbx1nOsU89soIHhH/8dVKYmNHR";
// Синтетический оригинал с файлом, который доступен и в цитате целого сообщения.
const ORIGINAL_WITH_FILE: &str = "FxcXFxcXFxcXFxcXFxcXFxcXFxcXFxcXFxcXFxcXFxf0gUhxKDRz8wX3kGgk5tmrV+t+TKy4WrAZtcRb332F8/89ulbDZRCBm+1KxUkKbZizxGdPAfQSJPQRpp8z/NyixUSeVVovXrJ7da8XhD77Nnawdxs3B31O+T3Ff31aUtcZ5YZb26klAt3J64/z0I0eIji33w4hPHsLacI1vDkAEBmI+2AIJGIB6yJSxbK9NnGBD24PNoQ5glcr48bo4tEZuC7ngMsVavgyDntVHxSLug==";
// Синтетическая версия цитирующего тела с выдержкой из оригинала другого поколения.
const QUOTING_WITH_EXCERPT: &str = "FhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYNFtNx5vgGYZ/jQNWwlgbOq1c3MAeFTp0aUWyyqgroNc8Q2qPS7CmTSpprqOaEeSK7WzwH8chMmnIZgZly3mK5q+u2eDfzIjhHrM+KLAGns4jPFfuLG4rA4YQi1oBchq76pe9yBjv7Dclba81TioXKGyfmKU0DTnfTMZ/LodKNZBKzlaWVJwG0/vfGU54FVgn8XN+yp7eutvacTJJsKtVN5bXPyk+OH0M/JaGEkmwZ7G7g9VXEDwXEaIR/qybow5Z5VzZMFyymaxLCC9tfg6GygY4tam8OhGwwwx1R";
const PROTECTED_FILE: Uuid = Uuid::from_u128(90);
const QUOTED_FILE: Uuid = Uuid::from_u128(91);

/// Возвращает две разнесённые зашифрованные строки среди контрольных обычных сообщений.
pub(super) fn history(user: &str, peer: &str, changed: &HashMap<Uuid, &str>) -> Value {
    json!((186..190).rev().map(|number| {
        let id = Uuid::from_u128(number);
        let payload = if [SEALED_A, SEALED_B].contains(&id) {
            json!({"kind": "e2ee", "key_uuid": KEY, "content": changed.get(&id).copied().unwrap_or(CIPHERTEXT)})
        } else {
            json!({"kind":"markdown", "content":"Контрольная строка истории"})
        };
        json!({
            "uuid": id, "stream_uuid": STREAM, "topic_uuid": TOPIC, "user_uuid": user,
            "project_id": PROJECT, "author_uuid": peer,
            "created_at": format!("2026-11-17T10:00:{:02}Z", number - 186), "payload": payload,
        })
    }).collect::<Vec<_>>())
}

/// Проверяет ключ, ошибку и отзыв для двух баблов в рабочем окне.
#[gpui::test]
fn encrypted_history_reacts_to_key_without_opening_plain_composer(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        encrypted_history: true,
        direct_encrypted: AtomicBool::new(true),
        ..Received::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1000.), px(900.)));
    until(
        ui,
        "защищённая личка появилась в обычном каталоге",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "обе строки закрыты без точного ключа",
        |ui| ui.debug_bounds(LOCKED_A).is_some() && ui.debug_bounds(LOCKED_B).is_some(),
    );
    until(
        ui,
        "обычная отправка закрыта в защищённом чате",
        |ui| ui.debug_bounds("conversation-send-retry").is_some(),
    );
    assert!(ui.debug_bounds("workspace-composer-editor").is_none());

    let account = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let (engine, session) = runtime.message_encryption(&account).unwrap();
    let stream = STREAM.parse().unwrap();
    let key_id = KEY.parse().unwrap();
    let states = |ui: &VisualTestContext| {
        ui.read(|app| {
            let view = client.read(app).conversation.as_ref().unwrap().read(app);
            [SEALED_A, SEALED_B].map(|id| view.tested_history_encryption_state(id))
        })
    };

    engine
        .save_key(&session, stream, key_id, &GroupKey::from_bytes([6; 32]))
        .unwrap();
    until(
        ui,
        "неподходящий ключ показывает ошибку вместо текста",
        |ui| states(ui) == [Some("failed"); 2],
    );
    engine
        .save_key(&session, stream, key_id, &GroupKey::from_bytes([7; 32]))
        .unwrap();
    until(
        ui,
        "обе строки раскрылись под теми же UUID",
        |ui| states(ui) == [Some("readable"); 2],
    );
    assert!(ui.debug_bounds("workspace-composer-editor").is_none());

    engine.remove_key(&session, stream, key_id).unwrap();
    until(ui, "обе строки снова закрыты", |ui| {
        states(ui) == [Some("locked"); 2]
    });
    assert!(ui.debug_bounds("workspace-composer-editor").is_none());
    assert!(server.state.sent.lock().unwrap().is_empty());
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Дельта ключа переиспользует соседние представления, а прокрутка каталога сохраняет ширину поля.
#[gpui::test]
fn mixed_chat_key_event_reuses_active_composer_and_sidebar(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let socket = broadcast::channel(16).0;
    let server = Server::serve(Received {
        encrypted_history: true,
        encryption_live: Some(socket.clone()),
        ..Received::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1000.), px(900.)));
    until(
        ui,
        "готов каталог обычного чата",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "открытый композер и закрытые баблы в одном окне",
        |ui| {
            ui.debug_bounds(LOCKED_A).is_some()
                && ui.debug_bounds(LOCKED_B).is_some()
                && ui.debug_bounds("workspace-composer-editor").is_some()
        },
    );
    let account = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let (engine, activation) = runtime.message_encryption(&account).unwrap();
    let sample = |app: &gpui::App| {
        let client = client.read(app);
        let conversation = client.conversation.as_ref().unwrap().read(app);
        let composer = conversation.tested_composer().unwrap().read(app);
        (
            composer.tested_render_count(),
            client.sidebar.read(app).tested_render_count(),
        )
    };
    let state = |ui: &VisualTestContext| {
        ui.read(|app| {
            let conversation = client.read(app).conversation.as_ref().unwrap().read(app);
            [SEALED_A, SEALED_B].map(|id| conversation.tested_history_encryption_state(id))
        })
    };
    let before = ui.update(|window, app| {
        window.draw(app).clear();
        window.draw(app).clear();
        sample(app)
    });
    let stream = STREAM.parse().unwrap();
    let key_id = KEY.parse().unwrap();
    engine
        .save_key(&activation, stream, key_id, &GroupKey::from_bytes([7; 32]))
        .unwrap();
    until(
        ui,
        "ключ раскрыл только защищённые баблы",
        |ui| state(ui) == [Some("readable"); 2],
    );
    assert_eq!(
        ui.update(|window, app| {
            window.draw(app).clear();
            sample(app)
        }),
        before
    );
    engine.remove_key(&activation, stream, key_id).unwrap();
    until(
        ui,
        "отзыв закрыл только защищённые баблы",
        |ui| state(ui) == [Some("locked"); 2],
    );
    assert_eq!(
        ui.update(|window, app| {
            window.draw(app).clear();
            sample(app)
        }),
        before
    );
    engine
        .save_key(&activation, stream, key_id, &GroupKey::from_bytes([7; 32]))
        .unwrap();
    until(
        ui,
        "ключ снова доступен перед правкой",
        |ui| state(ui) == [Some("readable"); 2],
    );
    until(
        ui,
        "подключён настоящий WSS сценария",
        |_| server.state.ready.load(Ordering::SeqCst) > 0,
    );
    let ordinary_bodies = |ui: &VisualTestContext| {
        ui.read(|app| {
            let conversation = client.read(app).conversation.as_ref().unwrap().read(app);
            [187, 189].map(|id| {
                conversation
                    .tested_history_body_identity(Uuid::from_u128(id))
                    .unwrap()
            })
        })
    };
    let unchanged = ordinary_bodies(ui);
    let update = |version, id, data| {
        json!({
            "schema_version": 1, "uuid": Uuid::new_v4(), "epoch_generation": "local",
            "epoch_version": version, "project_id": PROJECT, "user_uuid": USER,
            "object_type": "message", "action": "updated", "payload": {
                "kind": "message.updated", "uuid": id, "payload": {"kind": "e2ee", "content": data, "key_uuid": KEY}
            }
        })
    };
    server
        .state
        .encrypted_edits
        .lock()
        .unwrap()
        .insert(SEALED_A, "invalid");
    let invalid = update(2, SEALED_A, "invalid");
    assert!(socket.send(RecoverySignal::Event(invalid.clone())).is_ok());
    until(
        ui,
        "правка с повреждённым телом касается одной строки",
        |ui| state(ui) == [Some("failed"), Some("readable")],
    );
    server
        .state
        .encrypted_edits
        .lock()
        .unwrap()
        .remove(&SEALED_A);
    assert!(
        socket
            .send(RecoverySignal::Event(update(3, SEALED_A, CIPHERTEXT)))
            .is_ok()
    );
    until(
        ui,
        "новая версия после правки расшифровалась",
        |ui| state(ui) == [Some("readable"); 2],
    );
    assert!(socket.send(RecoverySignal::Event(invalid)).is_ok());
    server
        .state
        .encrypted_edits
        .lock()
        .unwrap()
        .insert(SEALED_B, "invalid");
    assert!(
        socket
            .send(RecoverySignal::Event(update(4, SEALED_B, "invalid")))
            .is_ok()
    );
    until(
        ui,
        "после повтора обработана только более свежая правка",
        |ui| state(ui)[1] == Some("failed"),
    );
    assert_eq!(state(ui), [Some("readable"), Some("failed")]);
    assert_eq!(
        ordinary_bodies(ui),
        unchanged,
        "WSS и перечитывание REST сохраняют подготовку соседних UUID"
    );

    // Прокрутка под открытой подсказкой заставляет GPUI заново разместить кэшированный композер.
    let position = ui.debug_bounds(CHAT_ROW).unwrap().center();
    ui.simulate_mouse_move(position, None, Modifiers::none());
    ui.background_executor
        .advance_clock(Duration::from_millis(500));
    ui.run_until_parked();
    let renders = ui.read(|app| sample(app).0);
    ui.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
        ..Default::default()
    });
    ui.run_until_parked();
    assert!(ui.read(|app| sample(app).0) > renders);
    let panel = ui.debug_bounds("workspace-conversation-panel").unwrap();
    let composer = ui.debug_bounds("conversation-composer").unwrap();
    let editor = ui.debug_bounds("workspace-composer-editor").unwrap();
    assert_eq!(composer.size.width, panel.size.width);
    assert!(editor.size.width > px(0.));

    assert!(server.state.sent.lock().unwrap().is_empty());
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Хранит синтетические записи ровно как пришли в HTTPS, не принимая открытый текст.
#[derive(Default)]
pub(super) struct ProtectedWire {
    posts: Mutex<Vec<(Uuid, Value)>>, // Канонический UUID рядом с проекцией размещения.
    edits: AtomicUsize,               // Число принятых защищённых изменений.
    recipient_grant_reads: AtomicUsize, // Число попыток чтения выдачи устройством-получателем.
    held_invitation: Mutex<Option<HeldInvitation>>, // Задерживает подтверждение одного E2EE POST.
    presentation: bool, // Обычный каталог использует синтетическое вложение и другую версию оригинала.
    file_reads: AtomicUsize, // Число запросов файла для проверенного защищённого источника.
    quoted_file: bool, // Оригинал цитаты содержит свой отдельный файл.
    bulk: bool,         // Дополнительная тема известна SQLite, но отсутствует на открытой странице.
    unavailable_reply: Mutex<Option<Uuid>>, // Один точный запрос подтверждения вернёт временный отказ.
}

/// Управляет моментом ответа в проверке открытия звонка.
pub(super) struct HeldInvitation {
    pub(super) seen: Arc<AtomicBool>, // Сервер уже получил конверт приглашения.
    pub(super) release: oneshot::Receiver<()>, // Разрешает использовать обычный ответ POST.
}

impl ProtectedWire {
    /// Предлагает каталогу загрузить последнее защищённое сообщение через обычный batch GET.
    pub(super) fn preview(&self) -> Option<Uuid> {
        self.presentation.then_some(SEALED_A)
    }

    /// Останавливает следующий POST защищённого сообщения перед подтверждением.
    pub(super) fn hold_invitation(&self, invitation: HeldInvitation) {
        *self.held_invitation.lock().unwrap() = Some(invitation);
    }

    /// Возвращает список/точную строку истории и права двух тестовых участников.
    pub(super) fn read(
        &self,
        request: &Request<Incoming>,
        user: &str,
        peer: &str,
    ) -> Option<Response<Full<Bytes>>> {
        let path = request.uri().path();
        if self.bulk && path == format!("/api/workspace/v1/users/{peer}") {
            return Some(json_response(
                StatusCode::OK,
                json!({
                    "uuid":peer, "username":"peer", "first_name":"Собеседник", "status":"idle",
                }),
            ));
        }
        let originals = format!("/api/workspace/v1/messenger/files/{PROTECTED_FILE}/actions/download");
        let quoted = format!("/api/workspace/v1/messenger/files/{QUOTED_FILE}/actions/download");
        if self.presentation && (path == originals || (self.quoted_file && path == quoted)) {
            assert_eq!(request.method(), Method::GET);
            if path == originals {
                self.file_reads.fetch_add(1, Ordering::SeqCst);
            }
            return Some(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Full::new(Bytes::from_static(AVATAR_PNG)))
                    .unwrap(),
            );
        }
        if path == BINDINGS && query_value(request, "stream_uuid") == Some(STREAM) {
            return Some(json_response(
                StatusCode::OK,
                json!([
                    {"uuid":CHANNEL_BINDING,"stream_uuid":STREAM,"project_id":PROJECT,"user_uuid":USER,"role":"member"},
                    {"uuid":CHANNEL_OWNER_BINDING,"stream_uuid":STREAM,"project_id":PROJECT,"user_uuid":PEER,"role":"owner"},
                ]),
            ));
        }
        if request.method() != Method::GET {
            return None;
        }
        let posts = self.posts.lock().unwrap();
        let as_owner = |row: &Value| {
            let mut row = row.clone();
            row["user_uuid"] = json!(user);
            row
        };
        let mut sample = history(user, peer, &HashMap::new());
        sample[1]["payload"]["key_uuid"] = json!(OTHER_KEY);
        if self.presentation {
            sample[1]["payload"]["content"] = json!(if self.quoted_file { ORIGINAL_WITH_FILE } else { ORIGINAL_BODY });
            sample[3]["payload"]["content"] = json!(QUOTING_BODY);
            for row in sample.as_array_mut().unwrap() {
                row["source_name"] = json!("native");
            }
        }
        let sample = sample.as_array().unwrap();
        if path == MESSAGES {
            let mut rows: Vec<_> = posts
                .iter()
                .rev()
                .filter(|(_, row)| !self.bulk || row["topic_uuid"] == TOPIC)
                .map(|(_, row)| as_owner(row))
                .collect();
            // До помещения запроса в текущую тему показываем исходные закрытые строки.
            rows.extend(
                sample
                    .iter()
                    .filter(|row| {
                        !posts
                            .iter()
                            .any(|(_, posted)| posted["uuid"] == row["uuid"])
                    })
                    .cloned(),
            );
            let ascending = query_value(request, "sort_dir") == Some("asc");
            if let Some(marker) = query_value(request, "page_marker") {
                let offset = rows
                    .iter()
                    .position(|row| row["uuid"].as_str() == Some(marker))?;
                rows = if ascending {
                    rows[..offset].to_vec()
                } else {
                    rows[offset + 1..].to_vec()
                };
            }
            if ascending {
                rows.reverse();
            }
            return Some(json_response(StatusCode::OK, json!(rows)));
        }
        if let Some(id) = path.strip_prefix(MESSAGES) {
            let mut unavailable = self.unavailable_reply.lock().unwrap();
            if unavailable.is_some_and(|target| id.parse::<Uuid>() == Ok(target)) {
                unavailable.take();
                return Some(json_response(StatusCode::SERVICE_UNAVAILABLE, Value::Null));
            }
            drop(unavailable);
            let row = posts
                .iter()
                .map(|(_, row)| row)
                .chain(sample)
                .find(|row| row["uuid"].as_str() == Some(id));
            return Some(match row {
                Some(row)
                    if user == USER
                        && row["payload"]["kind"] == "key_grant"
                        && self.recipient_grant_reads.fetch_add(1, Ordering::SeqCst) == 0 =>
                {
                    json_response(StatusCode::SERVICE_UNAVAILABLE, Value::Null)
                }
                Some(row) => json_response(StatusCode::OK, as_owner(row)),
                None => json_response(StatusCode::NOT_FOUND, Value::Null),
            });
        }
        None
    }

    /// Проверяет формат закрытого тела, вычисляет размещение и сохраняет HTTP-ответ.
    pub(super) async fn accept(
        &self,
        request: Request<Incoming>,
        user: &str,
    ) -> Response<Full<Bytes>> {
        let method = request.method().clone();
        let id = request
            .uri()
            .path()
            .strip_prefix(MESSAGES)
            .unwrap()
            .to_owned();
        let bytes = request.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let payload = body["payload"].clone();
        let kind = payload["kind"].as_str().unwrap();
        assert!(matches!(
            kind,
            "key_request" | "key_grant" | "key_reject" | "e2ee"
        ));
        assert!(!payload.to_string().contains(PROTECTED_MARKER));
        let held = if kind == "e2ee" {
            self.held_invitation.lock().unwrap().take()
        } else {
            None
        };
        if let Some(held) = held {
            held.seen.store(true, Ordering::SeqCst);
            let _ = held.release.await;
        }
        let mut posts = self.posts.lock().unwrap();
        if method == Method::PUT {
            let (_, row) = posts
                .iter_mut()
                .find(|(_, row)| row["uuid"].as_str() == Some(id.as_str()))
                .unwrap();
            assert_eq!(row["author_uuid"], user);
            assert_eq!(row["payload"]["kind"], "e2ee");
            assert_eq!(payload["key_uuid"], row["payload"]["key_uuid"]);
            row["payload"] = payload;
            row["updated_at"] = json!("2026-11-17T10:01:00Z");
            self.edits.fetch_add(1, Ordering::SeqCst);
            return json_response(StatusCode::OK, row.clone());
        }
        assert_eq!(body["stream_uuid"], STREAM);
        let topic = body["topic_uuid"].as_str().unwrap();
        assert!(topic == TOPIC || (self.bulk && topic == NEXT_DIRECT_TOPIC));
        let canonical = body["uuid"].as_str().unwrap();
        let public = Uuid::new_v5(&Uuid::parse_str(topic).unwrap(), canonical.as_bytes());
        let canonical = canonical.parse().unwrap();
        let row = json!({
            "uuid":public, "canonical_message_uuid":canonical, "source_name":"native",
            "stream_uuid":STREAM, "topic_uuid":topic, "project_id":PROJECT, "user_uuid":user,
            "author_uuid":user, "created_at":format!("2026-11-17T10:00:{:02}Z", posts.len() + 10),
            "payload":payload,
        });
        posts.push((canonical, row.clone()));
        json_response(StatusCode::CREATED, row)
    }

    /// Возвращает публичный UUID и снимок принятого сообщения заданного типа.
    pub(super) fn posted(&self, kind: &str, key: &str) -> Option<(Uuid, Value)> {
        self.posts
            .lock()
            .unwrap()
            .iter()
            .find(|(_, row)| row["payload"]["kind"] == kind && row["payload"]["key_uuid"] == key)
            .map(|(_, row)| {
                (
                    serde_json::from_value(row["uuid"].clone()).unwrap(),
                    row.clone(),
                )
            })
    }
}

/// Выдаёт из панели ключи в две известные темы, включая запрос вне открытого окна.
#[gpui::test]
fn bulk_key_grants_use_persisted_requests_from_other_topics(cx: &mut TestAppContext) {
    use workspace_core::messenger::{
        encryption::{
            grant::RequestKeyPair,
            key_exchange::{KeyExchange, KeyExchangeError, KeyRequestOutcome, KeyRequestTarget},
        },
        send::SendError,
    };

    cx.update(bind_keys);
    let socket = broadcast::channel(16).0;
    let server = Server::serve(Received {
        direct_encrypted: AtomicBool::new(true),
        encryption_live: Some(socket.clone()),
        protected_wire: Some(ProtectedWire {
            bulk: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let fixture = server.state.protected_wire.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |view, cx| view.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1250.), px(900.)));
    until(ui, "каталог и WSS готовы", |ui| {
        ui.debug_bounds(CHAT_ROW).is_some() && server.state.ready.load(Ordering::SeqCst) != 0
    });
    let (engine, activation) = ui.read(|app| {
        runtime
            .message_encryption(&client.read(app).shell.selected().unwrap().clone())
            .unwrap()
    });
    let stream = STREAM.parse().unwrap();
    engine
        .save_key(
            &activation,
            stream,
            KEY.parse().unwrap(),
            &GroupKey::from_bytes([7; 32]),
        )
        .unwrap();
    let requests = [TOPIC, NEXT_DIRECT_TOPIC, NEXT_DIRECT_TOPIC].map(|topic| {
        let canonical = Uuid::new_v4();
        let id = Uuid::new_v5(&topic.parse().unwrap(), canonical.to_string().as_bytes());
        let row = json!({
            "uuid":id, "canonical_message_uuid":canonical, "source_name":"native",
            "stream_uuid":STREAM, "topic_uuid":topic, "project_id":PROJECT, "user_uuid":USER,
            "author_uuid":PEER, "created_at":"2026-11-17T10:00:11Z",
            "payload":{"kind":"key_request","key_uuid":KEY,"device_id":Uuid::new_v4(),"device_name":format!("Device {topic}"),
                "public_key":RequestKeyPair::generate().unwrap().public_key().to_base64()},
        });
        fixture.posts.lock().unwrap().push((canonical, row.clone()));
        (id, row)
    });
    for (index, (_, row)) in requests.iter().enumerate() {
        service_event(&socket, index as u64 + 2, row.clone());
    }
    // У обычного участника есть ключ, но отказать в защищённом чате может администратор.
    let reply = |request: &Value, author: &str| {
        let canonical = Uuid::new_v4();
        let topic = request["topic_uuid"].as_str().unwrap();
        let id = Uuid::new_v5(&topic.parse().unwrap(), canonical.to_string().as_bytes());
        let row = json!({
            "uuid":id, "canonical_message_uuid":canonical, "source_name":"native",
            "stream_uuid":STREAM, "topic_uuid":topic, "project_id":PROJECT, "user_uuid":USER,
            "author_uuid":author, "created_at":"2026-11-17T10:00:12Z",
            "payload":{"kind":"key_reject","key_uuid":KEY,"sender_device_id":Uuid::new_v4(),
                "recipient_device_id":request["payload"]["device_id"],"request_message_uuid":request["uuid"]},
        });
        fixture.posts.lock().unwrap().push((canonical, row.clone()));
        row
    };
    service_event(&socket, 5, reply(&requests[0].1, USER));
    let authorized_reply = reply(&requests[2].1, PEER);
    service_event(&socket, 6, authorized_reply.clone());
    click(ui, CHAT_ROW);
    until(
        ui,
        "история и защищённая отправка открылись",
        |ui| ui.debug_bounds("workspace-composer-editor").is_some(),
    );
    let inline = format!("grant-protected-key-{}", requests[0].0).leak();
    until(
        ui,
        "неавторизованный отказ не скрывает выдачу рядом с запросом",
        |ui| ui.debug_bounds(inline).is_some(),
    );
    click(ui, "conversation-details-trigger");
    let results = requests
        .each_ref()
        .map(|(id, _)| format!("bulk-key-result-{id}").leak() as &'static str);
    until(
        ui,
        "два запроса взяты из индекса разных тем",
        |ui| {
            results[..2]
                .iter()
                .all(|result| ui.debug_bounds(result).is_some())
        },
    );
    assert!(ui.debug_bounds("conversation-encrypted-info").is_some());
    assert!(ui.debug_bounds(results[2]).is_none());
    click(ui, "bulk-grant-chat-keys");
    until(ui, "сервер принял обе выдачи", |_| {
        fixture
            .posts
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, row)| row["payload"]["kind"] == "key_grant")
            .count()
            == 2
    });
    click(ui, "conversation-encrypted-settings");
    until(
        ui,
        "можно перейти к ключам из настроек",
        |ui| {
            ui.debug_bounds("conversation-encrypted-settings-keys")
                .is_some()
        },
    );
    click(ui, "conversation-encrypted-settings-keys");
    until(
        ui,
        "выданные ключи попали в историю запросов",
        |ui| {
            ui.debug_bounds("conversation-key-requests-granted")
                .is_some()
        },
    );
    click(ui, "conversation-key-requests-granted");
    let granted = requests
        .each_ref()
        .map(|(id, _)| format!("bulk-key-granted-{id}").leak() as &'static str);
    until(
        ui,
        "панель показала оба подтверждённых результата",
        |ui| {
            granted[..2]
                .iter()
                .all(|result| ui.debug_bounds(result).is_some())
        },
    );
    {
        let posts = fixture.posts.lock().unwrap();
        for (id, source) in &requests[..2] {
            assert!(
                posts
                    .iter()
                    .any(|(_, row)| row["payload"]["kind"] == "key_grant"
                        && row["payload"]["request_message_uuid"] == id.to_string()
                        && row["topic_uuid"] == source["topic_uuid"])
            );
        }
    }
    let expected = KeyRequestTarget {
        stream_id: stream,
        topic_id: NEXT_DIRECT_TOPIC.parse().unwrap(),
        key_id: KEY.parse().unwrap(),
    };
    let resolved = requests[2].0;
    *fixture.unavailable_reply.lock().unwrap() =
        Some(serde_json::from_value(authorized_reply["uuid"].clone()).unwrap());
    let captured_engine = engine.clone();
    let captured_activation = activation.clone();
    let uncertain = futures::executor::block_on(runtime.request(move |services| async move {
        KeyExchange::new(services.messenger, services.sync, captured_engine)
            .respond_to_key_request(
                &captured_activation,
                expected,
                resolved,
                KeyRequestOutcome::Granted,
            )
            .await
    }))
    .unwrap();
    assert!(matches!(
        uncertain,
        Err(KeyExchangeError::Send(SendError::Unknown))
    ));
    let dispatch = futures::executor::block_on(runtime.request(move |services| async move {
        KeyExchange::new(services.messenger, services.sync, engine)
            .respond_to_key_request(&activation, expected, resolved, KeyRequestOutcome::Granted)
            .await
    }))
    .unwrap();
    assert!(matches!(dispatch, Err(KeyExchangeError::Forbidden)));
    assert_eq!(
        fixture
            .posts
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, row)| row["payload"]["kind"] == "key_grant")
            .count(),
        2
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Проверяет зависимости одного защищённого бабла, каталога и штатного просмотрщика.
#[gpui::test]
fn protected_presentation_uses_exact_quote_file_and_sidebar_keys(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        direct_encrypted: AtomicBool::new(true),
        protected_wire: Some(ProtectedWire {
            presentation: true,
            quoted_file: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1150.), px(900.)));
    until(
        ui,
        "каталог получил защищённое превью",
        |ui| {
            ui.debug_bounds("catalog-encrypted-preview-locked")
                .is_some()
        },
    );
    click(ui, CHAT_ROW);
    until(ui, "оба конверта закрыты", |ui| {
        ui.debug_bounds(LOCKED_A).is_some()
    });
    let (engine, activation) = ui.read(|app| {
        runtime
            .message_encryption(&client.read(app).shell.selected().unwrap().clone())
            .unwrap()
    });
    let stream = STREAM.parse().unwrap();
    let parent = KEY.parse().unwrap();
    let original = OTHER_KEY.parse().unwrap();
    let key = GroupKey::from_bytes([7; 32]);
    engine.save_key(&activation, stream, parent, &key).unwrap();
    const FILE_CARD: &str = "history-attachment-00000000-0000-0000-0000-0000000000ba-0";
    const QUOTE: &str = "history-protected-quote-00000000-0000-0000-0000-0000000000ba-0";
    const REPLY: &str = "reply-protected-message-00000000-0000-0000-0000-0000000000ba";
    const QUICK_BUBBLE: &str = "history-quick-bubble-00000000-0000-0000-0000-0000000000ba";
    until(
        ui,
        "в каталоге раскрыто превью цитирующей строки",
        |ui| ui.debug_bounds("catalog-encrypted-preview-ready").is_some(),
    );
    until(ui, "в сообщении появилось защищённое вложение", |ui| {
        ui.debug_bounds(FILE_CARD).is_some()
    });
    until(
        ui,
        "у цитирующей строки появилась панель быстрых действий",
        |ui| ui.debug_bounds(QUICK_BUBBLE).is_some(),
    );
    let card = ui.debug_bounds(FILE_CARD).unwrap();
    ui.simulate_mouse_move(card.center(), None, gpui::Modifiers::none());
    ui.run_until_parked();
    assert!(ui.debug_bounds(REPLY).is_some());
    assert!(ui.debug_bounds(QUOTE).is_none());
    until(ui, "защищённое фото получило миниатюру в бабле", |ui| {
        ui.debug_bounds("history-protected-thumbnail-00000000-0000-0000-0000-0000000000ba-0").is_some()
    });
    let photo = |ui: &VisualTestContext| ui.read(|app| {
        client.read(app).conversation.as_ref().unwrap().read(app).tested_photo(app)
    });
    let wire = server.state.protected_wire.as_ref().unwrap();
    click_protected(ui, FILE_CARD);
    assert!(photo(ui).0, "карточка сообщения открывает просмотр");
    until(
        ui,
        "Core получил защищённый файл, GPUI показал фото",
        |ui| photo(ui).1,
    );
    assert_eq!(wire.file_reads.load(Ordering::SeqCst), 2);
    click(ui, "media-viewer-save-protected");
    let destination = profile.path().join("protected-photo.png");
    ui.simulate_new_path_selection(|_| Some(destination.clone()));
    until(ui, "явно выбранный путь содержит защищённый оригинал", |_| {
        std::fs::read(&destination).ok().as_deref() == Some(AVATAR_PNG)
    });
    click(ui, "media-viewer-close");
    engine
        .save_key(&activation, stream, original, &key)
        .unwrap();
    until(
        ui,
        "оригинал раскрылся под собственным поколением",
        |ui| ui.debug_bounds(QUOTE).is_some(),
    );
    let quote_text = |ui: &VisualTestContext| {
        ui.read(|app| {
            client
                .read(app)
                .conversation
                .as_ref()?
                .read(app)
                .tested_protected_quote_text(SEALED_A)
        })
    };
    let sidebar_text = |ui: &VisualTestContext| {
        ui.read(|app| {
            client
                .read(app)
                .sidebar
                .read(app)
                .tested_protected_summary(SEALED_A, app)
        })
    };
    assert_eq!(
        quote_text(ui).as_deref(),
        Some("Точный оригинал другой версии ключа")
    );
    const QUOTED_CARD: &str = "history-protected-quote-attachment-00000000-0000-0000-0000-0000000000ba-0-0";
    until(ui, "карточка цитаты получила своё фото из оригинала", |ui| {
        ui.debug_bounds("history-protected-quote-thumbnail-00000000-0000-0000-0000-0000000000ba-0-0").is_some()
    });
    click_protected(ui, QUOTED_CARD);
    until(ui, "фото из оригинала открылось в просмотре", |ui| photo(ui).1);
    click(ui, "media-viewer-save-protected");
    assert!(ui.did_prompt_for_new_path());
    let revoked = profile.path().join("revoked-quoted-photo.png");
    engine.remove_key(&activation, stream, original).unwrap();
    ui.simulate_new_path_selection(|_| Some(revoked.clone()));
    until(
        ui,
        "отзыв оригинала закрывает цитату",
        |ui| quote_text(ui).is_none() && !photo(ui).0,
    );
    assert!(!revoked.exists());
    assert!(
        ui.debug_bounds(FILE_CARD).is_some(),
        "осталась карточка цитирующего сообщения"
    );
    assert!(
        sidebar_text(ui).is_some(),
        "осталось превью цитирующего сообщения"
    );
    engine.remove_key(&activation, stream, parent).unwrap();
    until(
        ui,
        "отзыв закрывает кадр, сообщение ответа и превью",
        |ui| {
            sidebar_text(ui).is_none()
                && ui.read(|app| {
                    let view = client.read(app).conversation.as_ref().unwrap().read(app);
                    !view.tested_photo(app).0
                        && view.tested_history_encryption_state(SEALED_A) == Some("locked")
                })
        },
    );
    assert_eq!(std::fs::read(&destination).unwrap(), AVATAR_PNG);
    engine.save_key(&activation, stream, parent, &key).unwrap();
    until(ui, "после восстановления ключа доступен прежний бабл и композер", |ui| {
        ui.read(|app| {
            let view = client.read(app).conversation.as_ref().unwrap().read(app);
            view.tested_history_encryption_state(SEALED_A) == Some("readable")
                && view.tested_composer().is_some_and(|composer| composer.read(app).tested_send_state(app).0.is_some())
        })
    });
    open_history_context_menu(ui, READABLE_A);
    assert!(
        ui.debug_bounds("copy-protected-message-00000000-0000-0000-0000-0000000000ba")
            .is_none()
    );
    click(ui, "history-copy-message");
    let copied = ui.update(|_, app| app.read_from_clipboard().and_then(|item| item.text()));
    assert!(copied.is_some_and(|body| body.contains("Подпись")));
    let bubble = ui.debug_bounds(READABLE_A).unwrap().center();
    ui.simulate_mouse_move(bubble, None, Modifiers::none());
    ui.run_until_parked();
    click_protected(ui, REPLY);
    ui.simulate_input("Ответ из защищённой темы");
    click(ui, "conversation-send");
    until(
        ui,
        "штатный редактор запечатал ответ",
        |_| wire.posted("e2ee", KEY).is_some(),
    );
    let (_, reply) = wire.posted("e2ee", KEY).unwrap();
    let result = engine
        .prepare(&activation, stream, parent)
        .unwrap()
        .decrypt(reply["payload"]["content"].as_str().unwrap());
    let access = engine.accept(result).unwrap();
    assert_eq!(
        access.with_body(|body| {
            body.contains(&format!("urn:quote:{SEALED_A}"))
                && body.contains("Ответ из защищённой темы")
        }),
        Some(true)
    );
    assert!(server.state.sent.lock().unwrap().is_empty());
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Прекращает рисование и сохранение цитаты сразу после подтверждённой правки её сообщения.
#[gpui::test]
fn protected_quote_photo_stops_painting_before_history_update_reaches_window(
    cx: &mut TestAppContext,
) {
    protected_preview_after_commit(cx, PreviewInvalidation::CitingEdit);
}

/// Правка тем же ключом отзывает уже декодированные прямое фото и миниатюру.
#[gpui::test]
fn protected_direct_photo_stops_painting_before_history_update_reaches_window(
    cx: &mut TestAppContext,
) {
    protected_preview_after_commit(cx, PreviewInvalidation::DirectEdit);
}

/// Изменение только файла отзывает старые пиксели прямого вложения.
#[gpui::test]
fn protected_direct_photo_stops_painting_before_file_update_reaches_window(
    cx: &mut TestAppContext,
) {
    protected_preview_after_commit(cx, PreviewInvalidation::DirectFile);
}

/// Изменение только файла отзывает пиксели цитаты, сохраняя разрешение на её текст.
#[gpui::test]
fn protected_quote_photo_stops_painting_before_file_update_reaches_window(cx: &mut TestAppContext) {
    protected_preview_after_commit(cx, PreviewInvalidation::QuotedFile);
}

/// Сочетания проверяемого источника и записи, произведённой Core при задержанном окне.
#[derive(Clone, Copy)]
enum PreviewInvalidation {
    CitingEdit, // Та же версия ключа, но цитирующая строка уже не содержит ссылки.
    DirectEdit, // Та же версия ключа, но прямое вложение убрали из сообщения.
    DirectFile, // Версия прямого файла изменилась без правки сообщения.
    QuotedFile, // Версия файла оригинала изменилась без правки обоих сообщений.
}

/// Проверяет реальные попытки Canvas до передачи события Core в GPUI.
fn protected_preview_after_commit(cx: &mut TestAppContext, change: PreviewInvalidation) {
    use workspace_core::storage::{Cursor, HistoryChange, MessagePlacement};
    use workspace_core::sync::{SyncChange, SyncUpdate};

    cx.update(bind_keys);
    let server = Server::serve(Received {
        direct_encrypted: AtomicBool::new(true),
        protected_wire: Some(ProtectedWire {
            presentation: true,
            quoted_file: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    let hold_window_update = Arc::new(AtomicBool::new(false));
    let skipped = Arc::new(AtomicBool::new(false));
    ui.update(|_, app| {
        let hold = hold_window_update.clone();
        let skipped = skipped.clone();
        let subscription = events.attach(&client, app, move |client, event, cx| {
            if hold.load(Ordering::SeqCst) && matches!(event, RuntimeEvent::Sync(_)) {
                skipped.store(true, Ordering::SeqCst);
                return;
            }
            client.handle_runtime(event, cx);
        });
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1150.), px(1300.)));
    until(
        ui,
        "защищённый чат виден в каталоге",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "история загрузила зашифрованные строки",
        |ui| ui.debug_bounds(LOCKED_A).is_some(),
    );
    let (engine, activation) = ui.read(|app| {
        runtime
            .message_encryption(&client.read(app).shell.selected().unwrap().clone())
            .unwrap()
    });
    let stream = STREAM.parse().unwrap();
    let parent = KEY.parse().unwrap();
    let original = OTHER_KEY.parse().unwrap();
    for generation in [parent, original] {
        engine
            .save_key(
                &activation,
                stream,
                generation,
                &GroupKey::from_bytes([7; 32]),
            )
            .unwrap();
    }
    const QUOTED_CARD: &str =
        "history-protected-quote-attachment-00000000-0000-0000-0000-0000000000ba-0-0";
    const DIRECT_CARD: &str = "history-attachment-00000000-0000-0000-0000-0000000000ba-0";
    let (card, source, file) = if matches!(
        change,
        PreviewInvalidation::CitingEdit | PreviewInvalidation::QuotedFile
    ) {
        (QUOTED_CARD, SEALED_B, QUOTED_FILE)
    } else {
        (DIRECT_CARD, SEALED_A, PROTECTED_FILE)
    };
    until(
        ui,
        "история показала карточку защищённого файла",
        |ui| ui.debug_bounds(card).is_some(),
    );
    if card == DIRECT_CARD {
        let album = ui
            .debug_bounds("history-media-album-00000000-0000-0000-0000-0000000000ba")
            .expect("у прямого защищённого вложения есть полноширинный альбом");
        let tile = ui.debug_bounds(card).unwrap();
        assert_eq!((tile.left(), tile.right()), (album.left(), album.right()));
    }
    ui.update(|window, app| window.draw(app).clear());
    let tile = ui.debug_bounds(card).unwrap();
    let history = ui.debug_bounds("conversation-history-focus").unwrap();
    assert!(tile.intersects(&history), "видна часть кадра для клика");
    let point = tile.intersect(&history).center();
    ui.simulate_mouse_move(point, None, Modifiers::none());
    ui.simulate_click(point, Modifiers::none());
    let view_state = |ui: &VisualTestContext| {
        ui.read(|app| {
            let view = client.read(app).conversation.as_ref().unwrap().read(app);
            (
                view.tested_photo(app),
                view.tested_protected_photo_paints(),
                view.tested_protected_quote_text(SEALED_A),
                view.tested_history_encryption_state(SEALED_A),
                view.tested_protected_thumbnail_paints(SEALED_A, source, file, app),
            )
        })
    };
    until(
        ui,
        "просмотр и миниатюра вывели защищённые пиксели",
        |ui| {
            let state = view_state(ui);
            [state.1, state.4]
                .into_iter()
                .all(|paints| paints.is_some_and(|(_, admitted)| admitted > 0))
                && state.2.is_some()
        },
    );
    let saving = matches!(change, PreviewInvalidation::CitingEdit);
    if saving {
        click(ui, "media-viewer-save-protected");
        assert!(ui.did_prompt_for_new_path());
    }
    let before = view_state(ui);
    assert!(before.2.is_some());
    hold_window_update.store(true, Ordering::SeqCst);
    let session = activation.session().clone();
    let update_session = session.clone();
    let cursor = Cursor {
        epoch_generation: "local".into(),
        epoch_version: 101,
    };
    let (event, update) = match change {
        PreviewInvalidation::CitingEdit | PreviewInvalidation::DirectEdit => (
            Event::HistoryChanged {
                cursor,
                change: HistoryChange::Edit {
                    message: StoredMessage {
                        id: SEALED_A.to_string(),
                        body: MessageBody::Encrypted {
                            key_id: parent,
                            ciphertext: CIPHERTEXT.into(),
                        },
                    },
                    placement: Some(MessagePlacement {
                        stream_id: STREAM.into(),
                        topic_id: TOPIC.into(),
                    }),
                    reactions: None,
                },
            },
            if saving {
                SyncChange::QuoteOriginChanged {
                    message_id: SEALED_A,
                }
            } else {
                SyncChange::HistoryMessageChanged {
                    message_id: SEALED_A,
                }
            },
        ),
        PreviewInvalidation::DirectFile | PreviewInvalidation::QuotedFile => (
            Event::FileUpdated {
                cursor,
                file_id: file,
            },
            SyncChange::MediaFileInvalidated { file_id: file },
        ),
    };
    futures::executor::block_on(runtime.request(move |services| async move {
        services.sync.apply(
            &update_session.owner,
            update_session.generation,
            SOURCE_ID,
            event,
        )
    }))
    .unwrap()
    .unwrap();
    until(
        ui,
        "Core записал изменение, окно осталось на старом снимке",
        |_| skipped.load(Ordering::SeqCst),
    );
    ui.simulate_resize(size(px(1151.), px(901.)));
    let (attempts, admitted) = before.1.unwrap();
    let (thumb_attempts, thumb_admitted) = before.4.unwrap();
    until(
        ui,
        "Canvas попытался вывести просмотр и миниатюру до обработки события окном",
        |ui| {
            let state = view_state(ui);
            state.1.is_some_and(|(current, _)| current > attempts)
                // У цитаты с отозванной ссылкой следующий render уберёт сам Canvas карточки.
                && (saving || state.4.is_some_and(|(current, _)| current > thumb_attempts))
        },
    );
    let after = view_state(ui);
    assert!(
        after.0.0 && after.0.1,
        "старый оверлей и готовый кадр ещё в памяти окна"
    );
    assert_eq!(after.3, Some("readable"));
    assert_eq!(
        after.1.unwrap().1,
        admitted,
        "следующий кадр просмотра не выводит прежние пиксели"
    );
    if let Some((_, admitted)) = after.4 {
        assert_eq!(
            admitted, thumb_admitted,
            "следующий кадр миниатюры не выводит прежние пиксели"
        );
    } else {
        assert!(saving, "только отозванная цитата убирает саму миниатюру");
    }
    assert_eq!(
        after.2.is_none(),
        matches!(
            change,
            PreviewInvalidation::CitingEdit | PreviewInvalidation::DirectEdit
        ),
        "только изменение строки отзывает текст цитаты",
    );
    for generation in [parent, original] {
        assert!(engine.prepare(&activation, stream, generation).is_ok());
    }
    let revoked = profile.path().join("revoked-edited-quote-photo.png");
    if saving {
        ui.simulate_new_path_selection(|_| Some(revoked.clone()));
    }
    ui.update(|_, app| {
        client.update(app, |view, cx| {
            view.handle_runtime(
                RuntimeEvent::Sync(SyncUpdate {
                    context: session.owner,
                    generation: session.generation,
                    change: update,
                }),
                cx,
            )
        });
    });
    until(
        ui,
        "точечное событие закрыло просмотр",
        |ui| !view_state(ui).0.0,
    );
    if saving {
        assert_eq!(view_state(ui).3, Some("readable"));
    }
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
    assert!(!revoked.exists());
}

/// Оставляет ссылку на отзывный оригинал при копировании, вырезании и сетевой правке ответа.
#[gpui::test]
fn protected_quote_excerpt_stays_out_of_clipboard_and_editor_after_original_key_removal(
    cx: &mut TestAppContext,
) {
    cx.update(bind_keys);
    let mut sample = history(USER, PEER, &HashMap::new());
    let mut quoting = sample[3].take();
    quoting["source_name"] = json!("native");
    quoting["author_uuid"] = json!(USER);
    quoting["payload"]["content"] = json!(QUOTING_WITH_EXCERPT);
    let wire = ProtectedWire {
        presentation: true,
        posts: Mutex::new(vec![(SEALED_A, quoting)]),
        ..Default::default()
    };
    let server = Server::serve(Received {
        direct_encrypted: AtomicBool::new(true),
        protected_wire: Some(wire),
        ..Default::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |view, cx| view.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1150.), px(900.)));
    until(ui, "каталог доступен", |ui| {
        ui.debug_bounds(CHAT_ROW).is_some()
    });
    click(ui, CHAT_ROW);
    let (engine, activation) = ui.read(|app| {
        runtime
            .message_encryption(&client.read(app).shell.selected().unwrap().clone())
            .unwrap()
    });
    let stream = STREAM.parse().unwrap();
    let current = KEY.parse().unwrap();
    let original = OTHER_KEY.parse().unwrap();
    for generation in [current, original] {
        engine
            .save_key(
                &activation,
                stream,
                generation,
                &GroupKey::from_bytes([7; 32]),
            )
            .unwrap();
    }
    let quote_text = |ui: &VisualTestContext| {
        ui.read(|app| {
            client
                .read(app)
                .conversation
                .as_ref()?
                .read(app)
                .tested_protected_quote_text(SEALED_A)
        })
    };
    const EXCERPT: &str = "другой версии";
    until(
        ui,
        "выдержка показана только с двумя ключами",
        |ui| quote_text(ui).as_deref() == Some(EXCERPT),
    );
    engine.remove_key(&activation, stream, original).unwrap();
    until(
        ui,
        "выдержка скрыта, ключ ответа остаётся",
        |ui| {
            quote_text(ui).is_none()
                && ui.read(|app| {
                    client
                        .read(app)
                        .conversation
                        .as_ref()
                        .unwrap()
                        .read(app)
                        .tested_history_encryption_state(SEALED_A)
                        == Some("readable")
                })
        },
    );
    let safe = format!("[Метка](urn:quote:{SEALED_B})\n\nПодпись\n\n**Неизменённая разметка**");
    let clipboard = |ui: &mut VisualTestContext| {
        ui.update(|_, app| app.read_from_clipboard().and_then(|item| item.text()))
    };
    open_history_context_menu(ui, READABLE_A);
    click(ui, "history-copy-message");
    assert_eq!(clipboard(ui).as_deref(), Some(safe.as_str()));
    until(
        ui,
        "композер проверил оставшийся ключ",
        |ui| {
            ui.read(|app| {
                client
                    .read(app)
                    .conversation
                    .as_ref()
                    .and_then(|view| {
                        view.read(app)
                            .tested_composer()
                            .map(|editor| editor.read(app).tested_send_state(app).0)
                    })
                    .flatten()
                    .is_some()
            })
        },
    );
    ui.update(|window, app| window.draw(app).clear());
    assert!(
        ui.debug_bounds("edit-protected-message-00000000-0000-0000-0000-0000000000ba")
            .is_none()
    );
    open_history_context_menu(ui, READABLE_A);
    click(ui, "history-edit-message");
    assert!(ui.read(|app| {
        let composer = client
            .read(app)
            .conversation
            .as_ref()
            .unwrap()
            .read(app)
            .tested_composer()
            .unwrap()
            .read(app);
        composer.tested_edit_active() && composer.draft(app) == safe
    }));
    ui.simulate_keystrokes("cmd-a cmd-c");
    assert_eq!(clipboard(ui).as_deref(), Some(safe.as_str()));
    ui.simulate_keystrokes("cmd-a cmd-x");
    assert_eq!(clipboard(ui).as_deref(), Some(safe.as_str()));
    ui.simulate_keystrokes("cmd-v");
    ui.simulate_input("\n\nПравка");
    click(ui, "conversation-send");
    let wire = server.state.protected_wire.as_ref().unwrap();
    until(
        ui,
        "сервер принял зашифрованную правку точного UUID",
        |_| wire.edits.load(Ordering::SeqCst) == 1,
    );
    let (id, reply) = wire.posted("e2ee", KEY).unwrap();
    assert_eq!(id, SEALED_A);
    let result = engine
        .prepare(&activation, stream, current)
        .unwrap()
        .decrypt(reply["payload"]["content"].as_str().unwrap());
    assert_eq!(
        engine.accept(result).unwrap().with_body(str::to_owned),
        Some(format!("{safe}\n\nПравка"))
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

const PROTECTED_MARKER: &str = "marker from the encrypted composer";
const REQUEST_A: &str = "request-protected-key-00000000-0000-0000-0000-0000000000ba";
const REQUEST_B: &str = "request-protected-key-00000000-0000-0000-0000-0000000000bc";

/// Проверяет две штатные сессии GPUI, точное размещение обмена и шифрование нового сообщения.
#[gpui::test]
fn encrypted_composer_and_key_exchange_use_authenticated_wire(cx: &mut TestAppContext) {
    use gpui::VisualContext;
    cx.update(bind_keys);
    let events_for_requester = broadcast::channel(16).0;
    let server = Server::serve(Received {
        direct_encrypted: AtomicBool::new(true),
        encryption_live: Some(events_for_requester.clone()),
        protected_wire: Some(ProtectedWire::default()),
        ..Received::default()
    });
    let wire = server.state.protected_wire.as_ref().unwrap();
    let requester_profile = tempfile::tempdir().unwrap();
    let issuer_profile = tempfile::tempdir().unwrap();
    let (requester, events) = start_runtime(
        requester_profile.path(),
        saved_account(&server, USER, Uuid::nil()),
    );
    let (issuer, issuer_events) = start_runtime(
        issuer_profile.path(),
        saved_account(&server, PEER, PEER.parse().unwrap()),
    );
    let delivered_without_ui = Arc::new(AtomicBool::new(false));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), requester.clone(), cx));
    ui.update(|_, app| {
        let marker = delivered_without_ui.clone();
        let subscription = events.attach(&client, app, move |client, event, cx| {
            if matches!(&event, RuntimeEvent::Sync(update) if matches!(
                &update.change, workspace_core::sync::SyncChange::KeyServiceMessage { key_id, .. }
                    if key_id.to_string() == KEY
            )) {
                // Проверяем обработку уже записанного ответа после пропуска его доставки в UI.
                marker.store(true, Ordering::SeqCst);
                return;
            }
            client.handle_runtime(event, cx);
        });
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1000.), px(900.)));
    until(
        ui,
        "открыт обычный каталог устройства запроса",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "две кнопки запроса после чтения истории",
        |ui| ui.debug_bounds(REQUEST_A).is_some() && ui.debug_bounds(REQUEST_B).is_some(),
    );
    assert!(ui.debug_bounds("workspace-composer-editor").is_none());
    click(ui, REQUEST_A);
    until(
        ui,
        "сервер принял нативный запрос первого ключа",
        |_| wire.posted("key_request", KEY).is_some(),
    );
    click(ui, REQUEST_B);
    until(
        ui,
        "сервер принял нативный запрос другого ключа",
        |_| wire.posted("key_request", OTHER_KEY).is_some(),
    );
    let first = wire.posted("key_request", KEY).unwrap().0;
    let second = wire.posted("key_request", OTHER_KEY).unwrap().0;
    let requester_window = ui.window_handle();
    let (issuer_client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), issuer.clone(), cx));
    ui.update(|_, app| {
        let subscription = issuer_events.attach(&issuer_client, app, Client::handle_runtime);
        issuer_client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1000.), px(900.)));
    until(
        ui,
        "у устройства выдачи появился каталог",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    let account = ui.read(|app| issuer_client.read(app).shell.selected().unwrap().clone());
    let (engine, activation) = issuer.message_encryption(&account).unwrap();
    engine
        .save_key(
            &activation,
            STREAM.parse().unwrap(),
            KEY.parse().unwrap(),
            &GroupKey::from_bytes([7; 32]),
        )
        .unwrap();
    click(ui, CHAT_ROW);
    let grant = format!("grant-protected-key-{first}").leak();
    let reject = format!("reject-protected-key-{second}").leak();
    until(
        ui,
        "обе служебные строки доступны владельцу чата",
        |ui| ui.debug_bounds(grant).is_some() && ui.debug_bounds(reject).is_some(),
    );
    click_protected(ui, grant);
    until(
        ui,
        "выдача принята сервером с HPKE-конвертом",
        |_| wire.posted("key_grant", KEY).is_some(),
    );
    let issuer_window = ui.window_handle();
    let ui = &mut VisualTestContext::from_window(requester_window, cx);
    let account = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let (recipient_engine, recipient_activation) = requester.message_encryption(&account).unwrap();
    ui.update(|_, app| {
        client.update(app, |client, cx| {
            client.apply(Some(ShellEffect::CloseConversation), cx)
        })
    });
    service_event(
        &events_for_requester,
        2,
        wire.posted("key_grant", KEY).unwrap().1,
    );
    until(
        ui,
        "выдача сохранена после пропущенного уведомления",
        |_| delivered_without_ui.load(Ordering::SeqCst),
    );
    assert!(ui.read(|app| client.read(app).conversation.is_none()));
    assert_eq!(wire.recipient_grant_reads.load(Ordering::SeqCst), 0);
    ui.update(|_, app| {
        client.update(app, |client, cx| {
            client.handle_runtime(RuntimeEvent::SyncLagged, cx)
        })
    });
    until(
        ui,
        "первая проверка выдачи получила временный отказ",
        |_| wire.recipient_grant_reads.load(Ordering::SeqCst) == 1,
    );
    until(
        ui,
        "повтор восстановил ключ вне открытого маршрута",
        |ui| {
            ui.background_executor
                .advance_clock(Duration::from_millis(100));
            recipient_engine
                .prepare(
                    &recipient_activation,
                    STREAM.parse().unwrap(),
                    KEY.parse().unwrap(),
                )
                .is_ok()
        },
    );
    assert!(wire.recipient_grant_reads.load(Ordering::SeqCst) >= 2);
    assert!(ui.read(|app| client.read(app).conversation.is_none()));

    click_protected(ui, CHAT_ROW);
    let states = |ui: &VisualTestContext| {
        ui.read(|app| {
            let view = client.read(app).conversation.as_ref().unwrap().read(app);
            [SEALED_A, SEALED_B].map(|id| view.tested_history_encryption_state(id))
        })
    };
    until(
        ui,
        "выдача открыла только нужный бабл и композер",
        |ui| {
            ui.debug_bounds(REQUEST_B).is_some()
                && ui.debug_bounds("workspace-composer-editor").is_some()
        },
    );
    assert_eq!(states(ui), [Some("readable"), Some("locked")]);
    click_protected(ui, REQUEST_B);
    until(
        ui,
        "исходный запрос второго поколения остаётся ожидающим",
        |ui| {
            ui.read(|app| {
                client
                    .read(app)
                    .conversation
                    .as_ref()
                    .unwrap()
                    .read(app)
                    .tested_key_request_status(SEALED_B)
                    == Some("waiting")
            })
        },
    );
    let ui = &mut VisualTestContext::from_window(issuer_window, cx);
    click_protected(ui, reject);
    until(ui, "отказ принят сервером", |_| {
        wire.posted("key_reject", OTHER_KEY).is_some()
    });
    let ui = &mut VisualTestContext::from_window(requester_window, cx);
    service_event(
        &events_for_requester,
        3,
        wire.posted("key_reject", OTHER_KEY).unwrap().1,
    );
    until(
        ui,
        "проверенный отказ даёт запросить поколение снова",
        |ui| {
            ui.read(|app| {
                client
                    .read(app)
                    .conversation
                    .as_ref()
                    .unwrap()
                    .read(app)
                    .tested_key_request_status(SEALED_B)
                    == Some("rejected")
            })
        },
    );
    // После получения ещё одной служебной строки старый запрос может уйти за край хвоста.
    ui.simulate_resize(size(px(1000.), px(1400.)));
    click_protected(ui, REQUEST_B);
    until(
        ui,
        "новый запрос получает другое размещение",
        |_| {
            wire.posts.lock().unwrap().iter().any(|(_, row)| {
                row["payload"]["kind"] == "key_request"
                    && row["payload"]["key_uuid"] == OTHER_KEY
                    && row["uuid"] != second.to_string()
            })
        },
    );

    click(ui, "workspace-composer-editor");
    ui.simulate_input(PROTECTED_MARKER);
    click(ui, "conversation-send");
    until(
        ui,
        "сервер получил новый E2EE-конверт",
        |_| wire.posted("e2ee", KEY).is_some(),
    );
    let (id, row) = wire.posted("e2ee", KEY).unwrap();
    let account = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let (engine, activation) = requester.message_encryption(&account).unwrap();
    let text = engine
        .prepare(&activation, STREAM.parse().unwrap(), KEY.parse().unwrap())
        .unwrap()
        .decrypt(row["payload"]["content"].as_str().unwrap());
    assert!(
        engine
            .accept(text)
            .unwrap()
            .with_body(|text| text == PROTECTED_MARKER)
            .unwrap()
    );

    let bubble = format!("history-encrypted-{id}-readable").leak();
    until(
        ui,
        "сообщение видно, а редактор готов открыть его по стрелке вверх",
        |ui| {
            ui.debug_bounds(bubble).is_some()
                && ui.read(|app| {
                    let composer = client
                        .read(app)
                        .conversation
                        .as_ref()
                        .unwrap()
                        .read(app)
                        .tested_composer()
                        .unwrap()
                        .read(app);
                    composer.can_edit_protected() && composer.draft(app).is_empty()
                })
        },
    );
    ui.update(|window, app| {
        let view = client.read(app).conversation.as_ref().unwrap().read(app);
        view.tested_composer()
            .unwrap()
            .read(app)
            .editor_focus(app)
            .focus(window);
    });
    ui.simulate_keystrokes("up");
    until(
        ui,
        "редактор открыл точный исходный текст",
        |ui| {
            ui.read(|app| {
                let view = client.read(app).conversation.as_ref().unwrap().read(app);
                let composer = view.tested_composer().unwrap().read(app);
                composer.tested_edit_active() && composer.draft(app) == PROTECTED_MARKER
            })
        },
    );
    ui.simulate_keystrokes("cmd-a");
    ui.simulate_input("marker after editing");
    click(ui, "conversation-send");
    until(
        ui,
        "правка вернула зашифрованный PUT",
        |_| wire.edits.load(Ordering::SeqCst) == 1,
    );
    let updated = wire.posted("e2ee", KEY).unwrap().1;
    let mut event = updated.clone();
    event["kind"] = json!("message.updated");
    assert!(
        events_for_requester
            .send(RecoverySignal::Event(json!({
                "schema_version":1, "uuid":Uuid::new_v4(), "epoch_generation":"local",
                "epoch_version":4, "project_id":PROJECT, "user_uuid":USER,
                "object_type":"message", "action":"updated", "payload": event,
            })))
            .is_ok()
    );
    let text = engine
        .prepare(&activation, STREAM.parse().unwrap(), KEY.parse().unwrap())
        .unwrap()
        .decrypt(updated["payload"]["content"].as_str().unwrap());
    assert!(
        engine
            .accept(text)
            .unwrap()
            .with_body(|text| text == "marker after editing")
            .unwrap()
    );
    until(
        ui,
        "правка подтверждена в том же окне",
        |ui| {
            ui.read(|app| {
                let view = client.read(app).conversation.as_ref().unwrap().read(app);
                view.tested_history_envelope(id, updated["payload"]["content"].as_str().unwrap())
                    && !view
                        .tested_composer()
                        .unwrap()
                        .read(app)
                        .tested_edit_active()
            })
        },
    );

    for file in ["messenger.sqlite3", "messenger.sqlite3-wal"] {
        if let Ok(bytes) = std::fs::read(requester_profile.path().join(file)) {
            for forbidden in [PROTECTED_MARKER, "marker after editing"] {
                assert!(
                    !bytes
                        .windows(forbidden.len())
                        .any(|window| window == forbidden.as_bytes())
                );
            }
        }
    }
    engine
        .remove_key(&activation, STREAM.parse().unwrap(), KEY.parse().unwrap())
        .unwrap();
    until(
        ui,
        "отзыв закрывает историю и запрещает отправку",
        |ui| {
            states(ui)[0] == Some("locked")
                && ui.read(|app| {
                    let view = client.read(app).conversation.as_ref().unwrap().read(app);
                    !view.tested_composer().unwrap().read(app).is_available()
                })
        },
    );
    for runtime in [requester, issuer] {
        assert_eq!(
            futures::executor::block_on(runtime.shutdown()),
            RuntimeExit::Clean
        );
    }
}

/// Доставляет размещение только на WebSocket устройства-получателя.
fn service_event(socket: &broadcast::Sender<RecoverySignal>, version: u64, mut row: Value) {
    row["kind"] = json!("message.created");
    row["user_uuid"] = json!(USER);
    assert!(
        socket
            .send(RecoverySignal::Event(json!({
                "schema_version":1, "uuid":Uuid::new_v4(), "epoch_generation":"local",
                "epoch_version":version, "project_id":PROJECT, "user_uuid":USER,
                "object_type":"message", "action":"created", "payload":row,
            })))
            .is_ok()
    );
}

/// Переводит указатель в виртуализированную строку перед нажатием на новом окне.
fn click_protected(ui: &mut VisualTestContext, selector: &'static str) {
    ui.update(|window, app| window.draw(app).clear());
    let point = ui
        .debug_bounds(selector)
        .expect("защищённое действие")
        .center();
    ui.simulate_mouse_move(point, None, Modifiers::none());
    click(ui, selector);
}
