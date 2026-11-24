//! Проверяет фото и видео в истории через настоящий Core и авторизованный локальный сервер.

use gpui::{KeyUpEvent, Keystroke};

use super::*;

const FILE: Uuid = Uuid::from_u128(90);
const CARD: &str = "history-attachment-00000000-0000-0000-0000-000000000048-0";
const ROW: &str = "history-message-00000000-0000-0000-0000-000000000048";
const CAPTION: &str = "history-reply-body-00000000-0000-0000-0000-000000000048";
const VIDEO: &[u8] = include_bytes!("../../../../../../tests/fixtures/media/synthetic-video.mov");

/// Возвращает тестовое вложение и считает запросы всего HTTPS-сервера.
pub(super) struct Fixture {
    pub(super) requests: AtomicUsize, // Включает запросы членства и истории.
    pub(super) downloads: AtomicUsize, // Запросы оригиналов вложений.
    pub(super) live: broadcast::Sender<RecoverySignal>, // События файлов на локальном WSS.
    gallery: bool,                    // Включает референсы обеих тем канала и четыре типа записей.
    video: bool,                      // Подменяет снимок коротким синтетическим роликом.
    album: bool,                      // Добавляет четыре кадра с портретом первым.
}

impl Fixture {
    /// Настраивает счётчики и канал событий вложения.
    fn new() -> Self {
        Self {
            requests: AtomicUsize::new(0),
            downloads: AtomicUsize::new(0),
            live: broadcast::channel(16).0,
            gallery: false,
            video: false,
            album: false,
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
        let (mime, name, bytes) = if self.video {
            ("video/quicktime", "clip.mov", VIDEO)
        } else {
            ("image/png", "photo.png", AVATAR_PNG)
        };
        if path == format!("/api/workspace/v1/messenger/files/{FILE}/actions/download") {
            assert_eq!((request.method(), user), (&Method::GET, USER));
            self.downloads.fetch_add(1, Ordering::SeqCst);
            return Some(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, mime)
                    .body(Full::new(Bytes::from_static(bytes)))
                    .unwrap(),
            );
        }
        (path == MESSAGES && request.method() == Method::GET
            && query_value(request, "stream_uuid") == Some(STREAM))
            .then(|| json_response(StatusCode::OK, json!([{
                "uuid": FRESH_ROW, "stream_uuid": STREAM, "topic_uuid": TOPIC,
                "project_id": PROJECT, "user_uuid": user, "author_uuid": peer,
                "created_at": "2026-11-17T10:00:00Z",
                "payload": {"kind": "markdown", "content": self.message(name, mime, bytes.len())},
            }])))
    }

    /// Готовит одиночное фото с подписью, видео без подписи или подборку из четырёх плиток.
    fn message(&self, name: &str, mime: &str, bytes: usize) -> String {
        let link = |width, height| {
            format!(
                "[{name}](urn:file:{FILE}?name={name}&content_type={mime}&w={width}&h={height}&size={bytes})"
            )
        };
        if self.album {
            [
                link(600, 1000),
                link(600, 300),
                link(800, 600),
                link(600, 300),
                "Подпись к альбому".into(),
            ]
            .join("\n\n")
        } else if self.video {
            link(600, 300)
        } else {
            format!("Подпись к медиа\n\n{}", link(600, 1200))
        }
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

/// Фото появляется до клика, Escape возвращает фокус, после удаления копии снова нужен GET.
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
    let before = ui.debug_bounds(ROW).unwrap();
    let media = ui.debug_bounds(CARD).unwrap();
    let dimensions = media.size;
    let bubble = ui.debug_bounds("history-media-bubble").unwrap();
    assert_eq!(
        (media.left(), media.right()),
        (bubble.left(), bubble.right())
    );
    assert!((dimensions.height - px(430.)).abs() <= px(1.));
    let photo = |ui: &mut VisualTestContext| {
        ui.read(|app| {
            client
                .read(app)
                .conversation
                .as_ref()
                .map(|conversation| conversation.read(app).tested_photo(app))
        })
    };
    let caption = ui
        .debug_bounds(CAPTION)
        .expect("подпись сохранила выделяемый текст");
    assert!(caption.top() >= media.bottom() && caption.left() > media.left());
    assert!(ui.debug_bounds("history-media-time").is_none());
    until(
        ui,
        "фото появилось в ленте до клика",
        |ui| ui.debug_bounds("history-attachment-preview").is_some(),
    );
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 1);
    assert_eq!(ui.debug_bounds(CARD).unwrap().size, dimensions);
    let baseline = counts.requests.load(Ordering::SeqCst);

    click(ui, CARD);
    until(
        ui,
        "оригинал виден в просмотре",
        |ui| {
            photo(ui) == Some((true, true, true)) && ui.debug_bounds("media-viewer-image").is_some()
        },
    );
    assert_eq!(counts.requests.load(Ordering::SeqCst), baseline);
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
    assert_eq!(counts.requests.load(Ordering::SeqCst), baseline);
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
    assert_eq!(counts.requests.load(Ordering::SeqCst), baseline + 1);
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

/// Показывает первый кадр видео до клика и открывает проигрыватель из того же кэша.
#[gpui::test]
fn video_attachment_shows_poster_and_plays_from_core_cache(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        media: Some(Fixture {
            video: true,
            ..Fixture::new()
        }),
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
        "доступен синтетический чат",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "первый кадр виден до клика",
        |ui| ui.debug_bounds("history-video-preview").is_some(),
    );
    let media = ui.debug_bounds(CARD).unwrap();
    let bubble = ui.debug_bounds("history-media-bubble").unwrap();
    let time = ui.debug_bounds("history-media-time").unwrap();
    assert_eq!(
        (media.left(), media.right(), media.bottom()),
        (bubble.left(), bubble.right(), bubble.bottom())
    );
    assert!(time.bottom() < media.bottom() && time.right() < media.right());
    assert!((media.size.width / 2. - media.size.height).abs() <= px(1.));
    assert!(ui.debug_bounds(CAPTION).is_none());
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 1);
    let playing = |ui: &VisualTestContext| {
        ui.read(|app| {
            client
                .read(app)
                .conversation
                .as_ref()
                .is_some_and(|view| view.read(app).tested_video_open())
        })
    };
    assert!(!playing(ui));
    click(ui, CARD);
    until(
        ui,
        "файл передан проигрывателю после клика",
        |ui| playing(ui),
    );
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 1);
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
    until(ui, "отзыв закрывает проигрыватель", |ui| !playing(ui));
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Проверяет, что портрет и три других кадра образуют цельный пузырь при двух ширинах окна.
#[gpui::test]
fn media_album_fills_bubble_and_keeps_caption_below(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        media: Some(Fixture {
            album: true,
            ..Fixture::new()
        }),
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
    ui.simulate_resize(size(px(1440.), px(1200.)));
    until(
        ui,
        "доступен синтетический чат с альбомом",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "альбом и подпись отрисованы",
        |ui| ui.debug_bounds(CAPTION).is_some(),
    );

    let check_album = |ui: &mut VisualTestContext| {
        let [left, top, middle, bottom] = [
            CARD,
            "history-attachment-00000000-0000-0000-0000-000000000048-1",
            "history-attachment-00000000-0000-0000-0000-000000000048-2",
            "history-attachment-00000000-0000-0000-0000-000000000048-3",
        ]
        .map(|selector| ui.debug_bounds(selector).unwrap());
        let bubble = ui.debug_bounds("history-media-bubble").unwrap();
        let caption = ui.debug_bounds(CAPTION).unwrap();
        assert_eq!((left.left(), top.right()), (bubble.left(), bubble.right()));
        assert_eq!(left.top(), top.top());
        assert_eq!(left.bottom(), bottom.bottom());
        assert!(left.size.height <= px(521.));
        assert_eq!(top.left() - left.right(), px(2.));
        assert_eq!(middle.top() - top.bottom(), px(2.));
        assert_eq!(bottom.top() - middle.bottom(), px(2.));
        assert!(caption.top() >= left.bottom() && caption.left() > left.left());
        bubble.size.width
    };
    let wide = check_album(ui);
    ui.simulate_resize(size(px(920.), px(1000.)));
    ui.run_until_parked();
    assert!(check_album(ui) < wide);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
