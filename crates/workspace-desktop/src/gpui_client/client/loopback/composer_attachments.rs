//! Проверяет вставку фото из буфера, прогресс, снятие карточки и отправку через рабочее окно.

use super::*;

const FILES: &str = "/api/workspace/v1/messenger/files/";
const REMOVED: Uuid = Uuid::from_u128(201);
const RETAINED: Uuid = Uuid::from_u128(202);
const AVATAR_HASH: &str = "c5c2ac5ab393ee13a3af546d0eaa810419fbe28a9c7b90ad8fa9bbb2314c66c7";

/// Даёт управлять подтверждением upload и проверять, какой UUID дошёл до сообщения.
#[derive(Default)]
pub(super) struct Fixture {
    uploaded: AtomicUsize,        // Число полностью принятых тел файлов.
    release: tokio::sync::Notify, // Разрешает первый ответ после проверки блокировки отправки.
    deletes: Mutex<Vec<Uuid>>, // UUID, для которых клиент действительно попытался вызвать DELETE.
    posted: Mutex<Vec<Value>>, // Принятые снимки обычных сообщений.
}

impl Fixture {
    /// Перехватывает файловые операции и возвращает принятый снимок при чтении этой лички.
    pub(super) fn handles(&self, request: &Request<Incoming>) -> bool {
        let path = request.uri().path();
        match *request.method() {
            Method::POST => matches!(path, FILES | MESSAGES),
            Method::DELETE => path.starts_with(FILES),
            Method::GET => {
                path == MESSAGES
                    && query_value(request, "stream_uuid") == Some(STREAM)
                    && !self.posted.lock().unwrap().is_empty()
            }
            _ => false,
        }
    }

    /// Проверяет токен, multipart и ожидаемый DELETE, затем отдаёт квитанцию.
    pub(super) async fn response(
        &self,
        request: Request<Incoming>,
        user: &str,
    ) -> Response<Full<Bytes>> {
        assert_eq!(user, USER);
        let path = request.uri().path();
        if request.method() == Method::GET {
            let rows: Vec<_> = self
                .posted
                .lock()
                .unwrap()
                .iter()
                .map(message_row)
                .collect();
            return json_response(StatusCode::OK, json!(rows));
        }
        if request.method() == Method::DELETE {
            let id = Uuid::parse_str(path.strip_prefix(FILES).unwrap()).unwrap();
            self.deletes.lock().unwrap().push(id);
            return json_response(StatusCode::FORBIDDEN, Value::Null);
        }
        if path == MESSAGES {
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            let post: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(post["stream_uuid"], STREAM);
            assert_eq!(post["topic_uuid"], TOPIC);
            let row = message_row(&post);
            self.posted.lock().unwrap().push(post);
            return json_response(StatusCode::CREATED, row);
        }
        assert!(
            request.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("multipart/form-data; boundary=")
        );
        let bytes = request.into_body().collect().await.unwrap().to_bytes();
        assert!(
            bytes
                .windows(AVATAR_PNG.len())
                .any(|part| part == AVATAR_PNG)
        );
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains(&format!("name=\"stream_uuid\"\r\n\r\n{STREAM}")));
        let name = body
            .split("name=\"name\"\r\n\r\n")
            .nth(1)
            .and_then(|part| part.lines().next())
            .expect("имя загруженного фото");
        let id = match self.uploaded.fetch_add(1, Ordering::SeqCst) {
            0 => {
                self.release.notified().await;
                REMOVED
            }
            1 => RETAINED,
            _ => panic!("лишняя загрузка фото"),
        };
        json_response(
            StatusCode::CREATED,
            json!({
                "uuid": id, "stream_uuid": STREAM, "user_uuid": USER,
                "name": name, "content_type": "image/png", "size_bytes": AVATAR_PNG.len(),
                "hash": AVATAR_HASH,
            }),
        )
    }
}

/// Вычисляет публичный UUID тем же способом, что сервер обычного сообщения.
fn message_id(post: &Value) -> Uuid {
    let topic = Uuid::parse_str(TOPIC).unwrap();
    let canonical = post["uuid"].as_str().unwrap();
    Uuid::new_v5(&topic, canonical.as_bytes())
}

/// Возвращает то же тело в подтверждении отправки и при последующем чтении истории.
fn message_row(post: &Value) -> Value {
    let mut row = direct_message(message_id(post), TOPIC, 1);
    row["payload"] = post["payload"].clone();
    row
}

/// Загружает два фото, оставляет второе и отправляет его без текста после отказа DELETE.
#[gpui::test]
fn composer_paste_unlink_and_send_use_workspace_file_api(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        composer_attachments: Some(Fixture::default()),
        ..Received::default()
    });
    let wire = server.state.composer_attachments.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
        let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, AVATAR_PNG.to_vec());
        app.write_to_clipboard(ClipboardItem::new_image(&image));
    });
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(ui, "личка в каталоге", |ui| {
        ui.debug_bounds(CHAT_ROW).is_some()
    });
    click(ui, CHAT_ROW);
    until(
        ui,
        "скрепка доступна в свёрнутом редакторе",
        |ui| ui.debug_bounds("composer-attach-file").is_some(),
    );
    click(ui, "workspace-composer-editor");
    ui.simulate_keystrokes("secondary-v");
    until(
        ui,
        "сервер принял первое тело, квитанция ожидается",
        |ui| {
            wire.uploaded.load(Ordering::SeqCst) == 1
                && ui.debug_bounds("composer-attachment-uploading-0").is_some()
        },
    );
    click(ui, "conversation-send");
    assert!(wire.posted.lock().unwrap().is_empty());
    wire.release.notify_one();
    until(ui, "первая карточка готова", |ui| {
        ui.debug_bounds("composer-attachment-ready-0").is_some()
    });
    click(ui, "workspace-composer-editor");
    ui.simulate_keystrokes("secondary-v");
    until(ui, "обе карточки готовы", |ui| {
        wire.uploaded.load(Ordering::SeqCst) == 2
            && ui.debug_bounds("composer-attachment-ready-1").is_some()
    });
    let card = ui
        .debug_bounds("composer-attachment-drag-0")
        .unwrap()
        .center();
    let editor = ui
        .debug_bounds("workspace-composer-editor")
        .unwrap()
        .center();
    let left = gpui::MouseButton::Left;
    let modifiers = Modifiers::none();
    ui.simulate_mouse_down(card, left, modifiers);
    ui.simulate_mouse_move(card + gpui::point(px(8.), px(0.)), left, modifiers);
    ui.simulate_mouse_move(editor, left, modifiers);
    ui.simulate_mouse_up(editor, left, modifiers);
    until(
        ui,
        "готовое фото вставлено в текст",
        |ui| ui.debug_bounds("composer-attachment-in-text-0").is_some(),
    );
    ui.simulate_keystrokes("secondary-a secondary-c");
    ui.update(|_, app| {
        let copied = app.read_from_clipboard().unwrap().text().unwrap();
        assert!(copied.ends_with(".png"));
        assert!(!copied.contains(&REMOVED.to_string()));
    });
    let remove = ui
        .debug_bounds("composer-attachment-remove-0")
        .unwrap()
        .center();
    ui.simulate_click(remove, modifiers);
    until(
        ui,
        "сервер получил попытку удаления",
        |_| !wire.deletes.lock().unwrap().is_empty(),
    );
    assert_eq!(*wire.deletes.lock().unwrap(), [REMOVED]);
    ui.read(|app| {
        let client = client.read(app);
        let view = client.conversation.as_ref().unwrap().read(app);
        assert!(
            view.tested_composer()
                .unwrap()
                .read(app)
                .draft(app)
                .is_empty()
        );
    });
    click(ui, "conversation-send");
    until(
        ui,
        "сервер принял сообщение с оставшимся файлом",
        |_| !wire.posted.lock().unwrap().is_empty(),
    );
    let accepted = message_id(&wire.posted.lock().unwrap()[0]);
    let address = ui.read(|app| client.read(app).shell.conversation().unwrap().clone());
    wait_topic_confirmation(&runtime, ui, &address, accepted);
    until(
        ui,
        "подтверждённое фото появилось в истории",
        |ui| ui.debug_bounds("history-media-bubble").is_some(),
    );
    let posted = wire.posted.lock().unwrap();
    assert_eq!(posted.len(), 1);
    let markdown = posted[0]["payload"]["content"].as_str().unwrap();
    let files = workspace_core::messenger::references::media_references(markdown);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file_id, RETAINED);
    assert_eq!(*wire.deletes.lock().unwrap(), [REMOVED]);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
