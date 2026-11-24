//! Проверяет четыре раздела вложений через реальные Core, GPUI и локальные HTTPS/WSS.

use super::*;
use gpui::{KeyUpEvent, Keystroke};
use workspace_core::storage::media::GalleryCounts;

const PHOTO: Uuid = Uuid::from_u128(150);
const DOCUMENT: Uuid = Uuid::from_u128(160);
const VIDEO: Uuid = Uuid::from_u128(161);
const FIRST_PHOTO: &str = "conversation-gallery-entry-00000000-0000-0000-0000-000000000048-00000000-0000-0000-0000-000000000096";
const OTHER_TOPIC_PHOTO: &str = "conversation-gallery-entry-00000000-0000-0000-0000-000000000049-00000000-0000-0000-0000-000000000096";
const FIRST_PREVIEW: &str = "conversation-gallery-preview-00000000-0000-0000-0000-000000000048-00000000-0000-0000-0000-000000000096";
const DOCUMENT_ROW: &str = "conversation-gallery-entry-00000000-0000-0000-0000-000000000048-00000000-0000-0000-0000-0000000000a0";
const VIDEO_ROW: &str = "conversation-gallery-entry-00000000-0000-0000-0000-000000000048-00000000-0000-0000-0000-0000000000a1";
const DOCUMENT_BYTES: &[u8] = b"synthetic authorized attachment";
const VIDEO_BYTES: &[u8] = b"synthetic authorized video";

/// Отвечает на обычные страницы канала и скачивания трёх вложений.
pub(super) fn response(
    request: &Request<Incoming>,
    user: &str,
    peer: &str,
) -> Option<(Response<Full<Bytes>>, bool)> {
    let path = request.uri().path();
    for (id, mime, bytes) in [
        (PHOTO, "image/png", AVATAR_PNG),
        (DOCUMENT, "text/plain", DOCUMENT_BYTES),
        (VIDEO, "video/mp4", VIDEO_BYTES),
    ] {
        if path == format!("/api/workspace/v1/messenger/files/{id}/actions/download") {
            assert_eq!((request.method(), user), (&Method::GET, USER));
            let response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CONTENT_LENGTH, bytes.len())
                .header("x-workspace-file-stream-uuid", CHANNEL)
                .body(Full::new(Bytes::from_static(bytes)))
                .unwrap();
            return Some((response, true));
        }
    }
    if path != MESSAGES
        || request.method() != Method::GET
        || query_value(request, "stream_uuid") != Some(CHANNEL)
    {
        return None;
    }
    let topic = query_value(request, "topic_uuid")?;
    let mut row = match topic {
        CHANNEL_TOPIC => {
            let photos: String = (125..=150)
                .map(|id| {
                    format!(
                        "![Фото {id}](urn:image:{}?name=photo-{id}.png)\n\n",
                        Uuid::from_u128(id)
                    )
                })
                .collect();
            let markdown = format!(
                "{photos}[Документ](urn:file:{DOCUMENT}?name=report.txt)\n\n[Видео](urn:video:{VIDEO}?name=movie.mp4)\n\n[Справка](https://example.test/guide)"
            );
            topic_message(user, peer, FRESH_ROW, "2026-11-17T10:00:00Z", &markdown)
        }
        CHANNEL_DEFAULT => topic_message(
            user,
            peer,
            PEER_ROW,
            "2026-12-01T10:00:00Z",
            &format!("![Фото в другой теме](urn:image:{PHOTO}?name=photo-other.png)"),
        ),
        _ => return None,
    };
    row["topic_uuid"] = json!(topic);
    // Галерея читает известную историю, а медиа выше экрана не подгружаются лентой.
    let mut tail = topic_message(
        user,
        peer,
        Uuid::from_u128(if topic == CHANNEL_TOPIC { 162 } else { 163 }),
        "2026-12-02T10:00:00Z",
        &"Сообщение без вложений\n".repeat(100),
    );
    tail["topic_uuid"] = json!(topic);
    Some((json_response(StatusCode::OK, json!([tail, row])), false))
}

/// Галерея меняет область, догружает страницы, читает фото и сохраняет файл по выбору пользователя.
#[gpui::test]
fn conversation_gallery_uses_known_history_and_downloads_only_on_action(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        media: Some(media::Fixture::for_gallery()),
        topic: Some(TopicAcceptance::default()),
        details: Some(DetailsFixture::default()),
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
    ui.simulate_resize(size(px(1440.), px(1200.)));
    until(
        ui,
        "доступен синтетический канал",
        |ui| {
            server.state.ready.load(Ordering::SeqCst) != 0
                && ui.debug_bounds(CHANNEL_CHEVRON).is_some()
        },
    );
    click(ui, CHANNEL_CHEVRON);
    ui.run_until_parked();
    click(ui, CHANNEL_TOPIC_ROW);
    let current = |ui: &VisualTestContext, topic: &str| {
        ui.read(|app| {
            client.read(app).conversation.as_ref().is_some_and(|view| {
                let view = view.read(app);
                view.has_access()
                    && view
                        .address()
                        .route
                        .scope
                        .topic_id
                        .is_some_and(|id| id.to_string() == topic)
            })
        })
    };
    until(
        ui,
        "проверен доступ к выбранной теме",
        |ui| current(ui, CHANNEL_TOPIC),
    );
    click(ui, DETAILS_TRIGGER);
    until(
        ui,
        "сведения канала загружены",
        |ui| ui.debug_bounds(CHANNEL_SECOND_TOPIC_ROW).is_some(),
    );
    click(ui, CHANNEL_SECOND_TOPIC_ROW);
    until(
        ui,
        "загружена история второй темы",
        |ui| current(ui, CHANNEL_DEFAULT),
    );
    click(ui, CHANNEL_TOPIC_ROW);
    until(
        ui,
        "первая тема снова подтверждена",
        |ui| current(ui, CHANNEL_TOPIC),
    );
    click(ui, DETAILS_TRIGGER);
    until(
        ui,
        "видны четыре категории и числа из истории выбранной темы",
        |ui| {
            ui.read(|app| {
                client
                    .read(app)
                    .conversation
                    .as_ref()
                    .and_then(|view| view.read(app).tested_gallery_counts())
                    == Some(GalleryCounts {
                        photos: 26,
                        videos: 1,
                        files: 1,
                        links: 1,
                    })
            }) && ui.debug_bounds("conversation-gallery-open-links").is_some()
        },
    );
    let block = ui.debug_bounds("conversation-gallery-triggers").unwrap();
    let rows = [
        "conversation-gallery-open-photos",
        "conversation-gallery-open-videos",
        "conversation-gallery-open-files",
        "conversation-gallery-open-links",
    ];
    assert_eq!(block.size.height, px(152.));
    for (index, id) in rows.into_iter().enumerate() {
        let row = ui.debug_bounds(id).unwrap();
        assert_eq!(row.top(), block.top() + px(40. * index as f32), "{id}");
        assert_eq!(row.left(), block.left() + px(8.), "{id}");
        assert_eq!(row.right(), block.right() - px(8.), "{id}");
        assert_eq!(row.size.height, px(32.), "{id}");
    }
    click(ui, "conversation-gallery-open-photos");
    let gallery = |ui: &VisualTestContext| {
        ui.read(|app| {
            client
                .read(app)
                .conversation
                .as_ref()
                .and_then(|view| view.read(app).tested_gallery())
        })
    };
    until(
        ui,
        "24 первые записи своей темы и признак следующей страницы",
        |ui| {
            gallery(ui)
                .is_some_and(|(whole_stream, count, next)| !whole_stream && count == 24 && next)
                && ui.debug_bounds(FIRST_PHOTO).is_some()
                && ui.debug_bounds("conversation-gallery-more").is_some()
        },
    );
    assert!(ui.debug_bounds("conversation-gallery-coverage").is_some());
    assert!(ui.debug_bounds(OTHER_TOPIC_PHOTO).is_none());
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 0);
    click(ui, "conversation-gallery-more");
    until(
        ui,
        "постранично показаны все 26 локальных записей темы",
        |ui| {
            gallery(ui)
                .is_some_and(|(whole_stream, count, next)| !whole_stream && count == 26 && !next)
        },
    );
    click(ui, "conversation-gallery-scope-stream");
    until(
        ui,
        "в галерее канала видна другая уже открытая тема",
        |ui| {
            gallery(ui)
                .is_some_and(|(whole_stream, count, next)| whole_stream && count == 24 && next)
                && ui.debug_bounds(OTHER_TOPIC_PHOTO).is_some()
        },
    );
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 0);
    let photo = |ui: &VisualTestContext| {
        ui.read(|app| {
            client
                .read(app)
                .conversation
                .as_ref()
                .map(|view| view.read(app).tested_photo(app))
        })
    };
    click(ui, OTHER_TOPIC_PHOTO);
    until(
        ui,
        "фото другой темы открывается из общей галереи",
        |ui| photo(ui) == Some((true, true, true)),
    );
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 1);
    let before_reopen = counts.requests.load(Ordering::SeqCst);
    ui.simulate_keystrokes("escape");
    until(
        ui,
        "Escape вернул фокус на плитку",
        |ui| photo(ui) == Some((false, false, true)),
    );
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    until(
        ui,
        "плитка открывает то же фото с клавиатуры из кэша",
        |ui| photo(ui) == Some((true, true, true)),
    );
    assert_eq!(counts.requests.load(Ordering::SeqCst), before_reopen);
    ui.simulate_keystrokes("escape");
    click(ui, "conversation-gallery-back");

    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    until(
        ui,
        "Назад возвращает фокус к строке, Enter снова открывает фото",
        |ui| {
            gallery(ui).is_some_and(|(whole_stream, count, _)| !whole_stream && count == 24)
                && ui.debug_bounds(FIRST_PREVIEW).is_some()
        },
    );
    assert_eq!(
        counts.downloads.load(Ordering::SeqCst),
        1,
        "миниатюра не запрашивает сервер"
    );
    click(ui, "conversation-gallery-back");
    click(ui, "conversation-gallery-open-links");
    until(
        ui,
        "ссылка показана из истории",
        |ui| gallery(ui).is_some_and(|(_, count, next)| count == 1 && !next),
    );
    click(ui, "conversation-gallery-back");
    click(ui, "conversation-gallery-open-videos");
    until(
        ui,
        "оригинал видео доступен через явное сохранение",
        |ui| ui.debug_bounds(VIDEO_ROW).is_some(),
    );
    click(ui, VIDEO_ROW);
    assert!(ui.did_prompt_for_new_path());
    ui.simulate_new_path_selection(|_| None);
    ui.run_until_parked();
    assert_eq!(
        counts.downloads.load(Ordering::SeqCst),
        1,
        "отмена диалога не скачивает видео"
    );
    click(ui, VIDEO_ROW);
    assert!(ui.did_prompt_for_new_path());
    let video_destination = profile.path().join("exported-movie.mp4");
    ui.simulate_new_path_selection(|_| Some(video_destination.clone()));
    until(
        ui,
        "выбранный путь содержит завершённый оригинал видео",
        |_| std::fs::read(&video_destination).ok().as_deref() == Some(VIDEO_BYTES),
    );
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 2);

    click(ui, "conversation-gallery-back");
    click(ui, "conversation-gallery-open-files");
    until(
        ui,
        "файл индексируется до скачивания",
        |ui| ui.debug_bounds(DOCUMENT_ROW).is_some(),
    );
    click(ui, DOCUMENT_ROW);
    assert!(ui.did_prompt_for_new_path());
    let destination = profile.path().join("exported-report.txt");
    ui.simulate_new_path_selection(|_| Some(destination.clone()));
    until(
        ui,
        "выбранный путь содержит завершённый оригинал",
        |_| std::fs::read(&destination).ok().as_deref() == Some(DOCUMENT_BYTES),
    );
    assert_eq!(counts.downloads.load(Ordering::SeqCst), 3);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
