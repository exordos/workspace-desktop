//! Проверяет карточку и повторное открытие фото через настоящий Core в рабочем окне.

use gpui::{KeyUpEvent, Keystroke};

use super::*;

const FILE: Uuid = Uuid::from_u128(90);
const CARD: &str = "history-attachment-00000000-0000-0000-0000-000000000048-0";
const ROW: &str = "history-message-00000000-0000-0000-0000-000000000048";
const CAPTION: &str = "history-reply-body-00000000-0000-0000-0000-000000000048";

/// Возвращает тестовое вложение и считает запросы всего HTTPS-сервера.
pub(super) struct Fixture {
    pub(super) requests: AtomicUsize, // Включает запросы членства и истории.
    pub(super) downloads: AtomicUsize, // Запросы оригиналов вложений.
    pub(super) live: broadcast::Sender<RecoverySignal>, // События файлов на локальном WSS.
    gallery: bool,                    // Включает референсы обеих тем канала и четыре типа записей.
}

impl Fixture {
    /// Настраивает счётчики и канал событий вложения.
    fn new() -> Self {
        Self {
            requests: AtomicUsize::new(0),
            downloads: AtomicUsize::new(0),
            live: broadcast::channel(16).0,
            gallery: false,
        }
    }

    /// Использует тот же сервер вложений для каталога канала.
    pub(super) fn for_gallery() -> Self {
        Self {
            gallery: true,
            ..Self::new()
        }
    }

    /// Считает каждый входящий запрос, в том числе начальную смену протокола WSS.
    pub(super) fn observe(&self) {
        self.requests.fetch_add(1, Ordering::SeqCst);
    }

    /// Отдаёт обычную историю со ссылкой и разрешённый PNG.
    pub(super) fn response(
        &self,
        request: &Request<Incoming>,
        user: &str,
        peer: &str,
    ) -> Option<Response<Full<Bytes>>> {
        if self.gallery
            && let Some((response, is_download)) = super::gallery::response(request, user, peer)
        {
            if is_download {
                self.downloads.fetch_add(1, Ordering::SeqCst);
            }
            return Some(response);
        }
        let path = request.uri().path();
        if path == format!("/api/workspace/v1/messenger/files/{FILE}/actions/download") {
            assert_eq!((request.method(), user), (&Method::GET, USER));
            self.downloads.fetch_add(1, Ordering::SeqCst);
            return Some(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Full::new(Bytes::from_static(AVATAR_PNG)))
                    .unwrap(),
            );
        }
        (path == MESSAGES && request.method() == Method::GET
            && query_value(request, "stream_uuid") == Some(STREAM))
            .then(|| json_response(StatusCode::OK, json!([{
                "uuid": FRESH_ROW, "stream_uuid": STREAM, "topic_uuid": TOPIC,
                "project_id": PROJECT, "user_uuid": user, "author_uuid": peer,
                "created_at": "2026-11-17T10:00:00Z",
                "payload": {"kind": "markdown", "content": format!("Подпись к фото\n\n![Фото](urn:image:{FILE}?name=photo.png)")},
            }])))
    }
}

/// Находит конкретный сохранённый оригинал, проверяя его байты.
fn original(profile: &std::path::Path) -> std::path::PathBuf {
    let paths = std::fs::read_dir(profile.join("messenger-media")).unwrap();
    paths
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|extension| extension == "bin")
                && std::fs::read(path).is_ok_and(|bytes| bytes == AVATAR_PNG)
        })
        .expect("оригинал сохранён кэшем")
}

/// Фото загружается по клику, Escape возвращает фокус, после удаления копии снова нужен GET.
#[gpui::test]
fn photo_attachment_reopens_through_core_cache(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        media: Some(Fixture::new()),
        ..Received::default()
    });
    let counts = server.state.media.as_ref().unwrap();
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
    until(
        ui,
        "каталог сохранённого входа",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "история и проверка редактора завершились",
        |ui| {
            ui.debug_bounds(CARD).is_some()
                && server.state.ready.load(Ordering::SeqCst) > 0
                && ui.read(|app| {
                    client
                        .read(app)
                        .conversation
                        .as_ref()
                        .is_some_and(|conversation| {
                            conversation
                                .read(app)
                                .tested_composer()
                                .is_some_and(|view| {
                                    let (topic, pending) = view.read(app).tested_send_state(app);
                                    topic.is_some() && !pending
                                })
                        })
                })
        },
    );
    let baseline = counts.requests.load(Ordering::SeqCst);
    let before = ui.debug_bounds(ROW).unwrap();
    let photo = |ui: &mut VisualTestContext| {
        ui.read(|app| {
            client
                .read(app)
                .conversation
                .as_ref()
                .map(|conversation| conversation.read(app).tested_photo())
        })
    };
    assert!(
        ui.debug_bounds(CAPTION).is_some(),
        "подпись сохранила выделяемый текст"
    );
    assert_eq!(
        counts.downloads.load(Ordering::SeqCst),
        0,
        "до клика оригинал не запрашиваем"
    );

    click(ui, CARD);
    until(
        ui,
        "оригинал виден в просмотре",
        |ui| {
            photo(ui) == Some((true, true, true)) && ui.debug_bounds("media-viewer-image").is_some()
        },
    );
    assert_eq!(counts.requests.load(Ordering::SeqCst), baseline + 1);
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 1);
    let cached_path = original(profile.path());

    ui.simulate_keystrokes("escape");
    until(ui, "Escape закрыл фото", |ui| {
        photo(ui) == Some((false, false, true))
    });
    until(
        ui,
        "миниатюра осталась на карточке",
        |ui| ui.debug_bounds("history-attachment-preview").is_some(),
    );
    assert_eq!(ui.debug_bounds(ROW).unwrap(), before);
    click(ui, CARD);
    until(
        ui,
        "повторный клик открыл фото из кэша",
        |ui| photo(ui) == Some((true, true, true)),
    );
    assert_eq!(counts.requests.load(Ordering::SeqCst), baseline + 1);
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 1);

    ui.simulate_keystrokes("escape");
    until(
        ui,
        "повторный просмотр закрыт",
        |ui| photo(ui) == Some((false, false, true)),
    );
    std::fs::remove_file(&cached_path).unwrap();
    assert!(!cached_path.exists());
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    until(
        ui,
        "фокус вернулся к карточке, Core скачал пропавшую копию",
        |ui| photo(ui) == Some((true, true, true)),
    );
    assert_eq!(counts.requests.load(Ordering::SeqCst), baseline + 2);
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 2);
    assert!(original(profile.path()).is_file());
    assert!(
        counts
            .live
            .send(RecoverySignal::Event(json!({
                "schema_version": 1, "uuid": Uuid::from_u128(91),
                "epoch_generation": "local", "epoch_version": 2,
                "project_id": PROJECT, "user_uuid": USER,
                "object_type": "file", "action": "deleted",
                "payload": {"kind": "file.deleted", "uuid": FILE},
            })))
            .is_ok()
    );
    until(
        ui,
        "событие удаления скрыло фото и миниатюру",
        |ui| photo(ui) == Some((false, false, false)) && ui.debug_bounds(ROW).is_some(),
    );
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 2);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
