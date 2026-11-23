//! Проверяет чтение A → B → A и применение запоздавшей страницы через рабочие маршруты GPUI и HTTPS.

use gpui::{App, ScrollDelta, ScrollWheelEvent, point};

use super::*;

const OLDER_MARKER: &str = "older-position-fixture";
const FIRST_LATEST: u128 = 151;
const LAST: u128 = 200;

/// Хранит старый ответ до тех пор, пока тест не измерит положение читаемого сообщения.
pub(super) struct Fixture {
    release: Mutex<Option<oneshot::Receiver<()>>>, // Пропускает первый запрос догрузки по сигналу.
    requested: AtomicUsize, // Сколько запросов старой страницы получил сервер.
    anchor_reads: Mutex<Vec<Uuid>>, // UUID целей при повторном открытии лички.
}

impl Fixture {
    /// Отвечает обычной страницей хвоста или удерживает отдельный запрос старой страницы.
    pub(super) async fn page(
        &self,
        request: &Request<Incoming>,
        user: &str,
        peer: &str,
    ) -> Response<Full<Bytes>> {
        assert_eq!(query_value(request, "stream_uuid"), Some(STREAM));
        assert_eq!(query_value(request, "sort_dir"), Some("desc"));
        let marker = query_value(request, "page_marker");
        let (first, last) = if let Some(marker) = marker {
            assert_eq!(marker, OLDER_MARKER);
            let waiting = self.release.lock().unwrap().take();
            self.requested.fetch_add(1, Ordering::SeqCst);
            if let Some(waiting) = waiting {
                assert!(waiting.await.is_ok());
            }
            (101, FIRST_LATEST - 1)
        } else {
            (FIRST_LATEST, LAST)
        };
        let rows = (first..=last)
            .rev()
            .map(|number| row(number, user, peer))
            .collect();
        let mut response = json_response(StatusCode::OK, Value::Array(rows));
        if marker.is_none() {
            response
                .headers_mut()
                .insert("X-Pagination-Marker", OLDER_MARKER.parse().unwrap());
        }
        response
    }

    /// Возвращает ту же запись, которую Core сохранил в SQLite при первом открытии.
    pub(super) fn anchor(&self, path: &str, user: &str, peer: &str) -> Response<Full<Bytes>> {
        let id: Uuid = path[MESSAGES.len()..].parse().unwrap();
        assert!((FIRST_LATEST..=LAST).contains(&id.as_u128()));
        self.anchor_reads.lock().unwrap().push(id);
        json_response(StatusCode::OK, row(id.as_u128(), user, peer))
    }
}

/// Создаёт запись с постоянными UUID, датой и переменной высотой текста.
fn row(number: u128, user: &str, peer: &str) -> Value {
    json!({
        "uuid": Uuid::from_u128(number), "stream_uuid": STREAM, "topic_uuid": TOPIC,
        "user_uuid": user, "project_id": PROJECT, "author_uuid": peer,
        "created_at": format!("2026-11-17T10:{:02}:{:02}Z", number / 60, number % 60),
        "payload": {"kind": "markdown", "content": "Строка длинной истории. ".repeat(1 + number as usize % 7)},
    })
}

/// Подаёт вертикальный жест в рабочий список, пока приложение отвечает через GPUI.
fn scroll(ui: &mut VisualTestContext, delta: f32) {
    let center = ui
        .debug_bounds("conversation-history-focus")
        .expect("Рабочий список")
        .center();
    ui.simulate_event(ScrollWheelEvent {
        position: center,
        delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
        ..Default::default()
    });
    ui.run_until_parked();
}

/// Читает фактическую координату UUID в кадре актуального экрана.
fn message_y(ui: &VisualTestContext, client: &Entity<Client>, id: Uuid) -> Option<gpui::Pixels> {
    ui.read(|app| {
        client
            .read(app)
            .conversation
            .as_ref()?
            .read(app)
            .tested_message_window_y(id)
    })
}

/// Проверяет координату старого сообщения после переключения чата и последующей догрузки.
#[gpui::test]
fn restores_same_message_and_y_after_switching_chats_and_prefetch(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let (release, waiting) = oneshot::channel();
    let server = Server::serve(Received {
        topic: Some(TopicAcceptance::default()),
        history_position: Some(Fixture {
            release: Mutex::new(Some(waiting)),
            requested: AtomicUsize::new(0),
            anchor_reads: Mutex::default(),
        }),
        ..Received::default()
    });
    let fixture = server.state.history_position.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let account = saved_account(&server, USER, Uuid::nil());
    let (runtime, events) = start_runtime(profile.path(), account);
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(960.), px(620.)));
    until(ui, "каталог и вход готовы", |ui| {
        ui.debug_bounds(CHAT_ROW).is_some() && server.state.ready.load(Ordering::SeqCst) > 0
    });
    click(ui, CHAT_ROW);
    until(
        ui,
        "личка показала хвост и маркер",
        |ui| {
            ui.debug_bounds("history-older").is_some()
                && message_y(ui, &client, Uuid::from_u128(LAST)).is_some()
        },
    );
    let original = ui.read(|app| client.read(app).conversation.clone().unwrap());

    // Останавливаемся за порогом, чтобы первый запрос начался уже после возврата в A.
    let (distance, height) = (0..60)
        .find_map(|_| {
            scroll(ui, 90.);
            let (distance, height) = ui.read(|app| original.read(app).tested_older_distance());
            (distance > height && distance - height <= 95.).then_some((distance, height))
        })
        .expect("Можно подойти к старому краю, не пересекая порог");
    assert_eq!(fixture.requested.load(Ordering::SeqCst), 0);
    let anchor = ui.read(|app| {
        original
            .read(app)
            .reading_position()
            .expect("A читает историю")
    });
    let before = message_y(ui, &client, anchor.id).expect("Опора A видна");

    click(ui, CHANNEL_CHEVRON);
    click(ui, CHANNEL_TOPIC_ROW);
    until(
        ui,
        "B показал собственную историю",
        |ui| message_y(ui, &client, CHANNEL_HISTORY).is_some(),
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "A снова показал сохранённый UUID",
        |ui| message_y(ui, &client, anchor.id).is_some(),
    );
    let restored = message_y(ui, &client, anchor.id).unwrap();
    assert!(
        (restored - before).abs() <= px(1.),
        "Возврат A: {before:?} → {restored:?}"
    );
    assert_eq!(*fixture.anchor_reads.lock().unwrap(), vec![anchor.id]);

    scroll(ui, distance - height + 12.);
    until(
        ui,
        "рабочий чат заранее запросил старую страницу",
        |_| fixture.requested.load(Ordering::SeqCst) == 1,
    );
    let (ahead, height) = ui.read(|app| {
        client
            .read(app)
            .conversation
            .as_ref()
            .unwrap()
            .read(app)
            .tested_older_distance()
    });
    assert!(
        ahead > 100. && ahead <= height,
        "Запас перед ответом: {ahead} из {height} px"
    );
    let current = message_y(ui, &client, anchor.id).expect("Та же опора перед вставкой");
    release.send(()).unwrap();
    until(
        ui,
        "старая страница применилась и маркер исчез",
        |ui| {
            assert!(
                ui.debug_bounds("history-error").is_none(),
                "Core вернул ошибку страницы"
            );
            ui.read(|app| {
                client
                    .read(app)
                    .conversation
                    .as_ref()
                    .unwrap()
                    .read(app)
                    .tested_older_page()
                    == Some((100, workspace_core::storage::HistoryEdge::End))
            })
        },
    );
    let after = message_y(ui, &client, anchor.id).expect("Та же опора после вставки");
    eprintln!(
        "history_restore anchor_uuid={} initial_y={} restored_y={} before_page_y={} after_page_y={} ahead_px={ahead}",
        anchor.id,
        f32::from(before),
        f32::from(restored),
        f32::from(current),
        f32::from(after)
    );
    assert!(
        (after - current).abs() <= px(1.),
        "Догрузка: {current:?} → {after:?}"
    );
    assert_eq!(fixture.requested.load(Ordering::SeqCst), 1);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

const REFLOW_ROW: u128 = DETAILS_LAST_ROW - 16;
const REFLOW_MARKER: &str = "Место чтения внутри длинного сообщения";

/// Даёт длинное тело опорному и последнему сообщению. Предыдущий абзац меняет высоту от ширины.
pub(super) fn reflow_text(number: u128) -> String {
    if number == REFLOW_ROW {
        format!(
            "{}{REFLOW_MARKER}. {}",
            "Длинный абзац перед читаемым фрагментом. ".repeat(24),
            "Продолжение длинного сообщения. ".repeat(8)
        )
    } else if number == DETAILS_LAST_ROW - 1 {
        "Последнее сообщение переносится в несколько строк. ".repeat(6)
    } else {
        format!("Сообщение {number}")
    }
}

/// Снимает геометрию в первом кадре открытия или закрытия панели по клику.
fn first_panel_frame<T: Copy + 'static>(
    ui: &mut VisualTestContext,
    client: &Entity<Client>,
    selector: &'static str,
    open: bool,
    sample: impl Fn(&App) -> T + 'static,
) -> T {
    let capture = std::rc::Rc::new(std::cell::Cell::new(None));
    let frame = capture.clone();
    let subscription = ui.update(|window, app| {
        let panel = client.read(app).right_panel.clone();
        window.observe(&panel, app, move |panel, window, app| {
            if frame.get().is_none() && panel.read(app).is_empty() != open {
                window.refresh();
                window.draw(app).clear();
                frame.set(Some(sample(app)));
            }
        })
    });
    click(ui, selector);
    drop(subscription);
    capture
        .get()
        .expect("Панель изменилась и GPUI построил первый кадр")
}

/// Открывает синтетическую личку в рабочем клиенте со штатным правым разделом и HTTPS.
fn reflow_chat<'a>(
    cx: &'a mut TestAppContext,
    server: &Server,
    profile: &std::path::Path,
) -> (Entity<Client>, &'a mut VisualTestContext, RuntimeClient) {
    cx.update(bind_keys);
    let account = saved_account(server, USER, Uuid::nil());
    let (runtime, events) = start_runtime(profile, account);
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(1150.), px(690.)));
    until(
        ui,
        "рабочий вход и каталог готовы",
        |ui| ui.debug_bounds(CHAT_ROW).is_some() && server.state.ready.load(Ordering::SeqCst) > 0,
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "последнее сообщение показано",
        |ui| message_y(ui, &client, Uuid::from_u128(DETAILS_LAST_ROW - 1)).is_some(),
    );
    (client, ui, runtime)
}

/// Проверяет в рабочем клиенте, что фрагмент внутри длинного сообщения остаётся на прежней высоте.
#[gpui::test]
fn right_panel_reflow_keeps_the_reading_fragment_on_the_first_frame(cx: &mut TestAppContext) {
    let server = Server::serve(Received {
        details: Some(DetailsFixture {
            reflow: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (client, ui, runtime) = reflow_chat(cx, &server, profile.path());
    let view = ui.read(|app| client.read(app).conversation.clone().unwrap());
    let id = Uuid::from_u128(REFLOW_ROW);
    let text = reflow_text(REFLOW_ROW);
    let nearby = text.find(REFLOW_MARKER).unwrap() + 'М'.len_utf8();
    let (viewport, _, line_y) = (0..16)
        .find_map(|_| {
            ui.read(|app| view.read(app).tested_history_geometry(id, nearby))
                .or_else(|| {
                    scroll(ui, 90.);
                    None
                })
        })
        .expect("В прокручиваемом списке появилась разметка длинного сообщения");
    // Удерживаем первую букву реально перенесённой строки, на высоте которой остановились.
    let start = ui.read(|app| {
        view.read(app)
            .tested_history_line_start(id, nearby)
            .unwrap()
    });
    assert!(start > 0, "Фрагмент находится внутри перенесённого абзаца");
    let fragment = text[start..].trim_start();
    let byte = text.len() - fragment.len() + fragment.chars().next().unwrap().len_utf8();
    let sample = move |app: &App| view.read(app).tested_history_geometry(id, byte).unwrap();

    scroll(ui, f32::from(viewport.top() - line_y - px(3.)));
    let (wide, row, before) = ui.read(&sample);
    let wide_height = row.size.height;
    assert!(row.top() < wide.top() && row.bottom() > wide.top());
    assert!((before - wide.top() + px(3.)).abs() <= px(1.));

    let mut measurements = Vec::new();
    for (selector, open) in [
        (DETAILS_TRIGGER, true),
        ("conversation-details-close", false),
    ] {
        let (viewport, row, line_y) =
            first_panel_frame(ui, &client, selector, open, sample.clone());
        assert_eq!(viewport.size.width < wide.size.width, open);
        assert_eq!(row.size.height > wide_height, open);
        measurements.push((open, viewport.size.width, line_y));
    }
    for (open, width, line_y) in measurements {
        eprintln!(
            "history_reflow anchor_uuid={id} panel_open={open} width_px={} y_before_px={} y_after_px={} delta_px={}",
            f32::from(width),
            f32::from(before),
            f32::from(line_y),
            f32::from(line_y - before)
        );
        assert!(
            (line_y - before).abs() <= px(1.),
            "Панель открыта={open}: {before:?} → {line_y:?}"
        );
    }
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Проверяет, что после переноса последнего сообщения нижний отступ хвоста не меняется.
#[gpui::test]
fn right_panel_reflow_keeps_tail_gap_on_the_first_frame(cx: &mut TestAppContext) {
    let server = Server::serve(Received {
        details: Some(DetailsFixture {
            reflow: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (client, ui, runtime) = reflow_chat(cx, &server, profile.path());
    let view = ui.read(|app| client.read(app).conversation.clone().unwrap());
    let id = Uuid::from_u128(DETAILS_LAST_ROW - 1);
    let sample = move |app: &App| {
        let (viewport, bounds, _) = view
            .read(app)
            .tested_history_geometry(id, 0)
            .expect("Последнее сообщение нарисовано");
        (
            viewport.size.width,
            bounds.size.height,
            viewport.bottom() - bounds.bottom(),
        )
    };
    let (wide, initial_height, before) = ui.read(&sample);
    assert_eq!(before, px(12.));
    for (selector, open) in [
        (DETAILS_TRIGGER, true),
        ("conversation-details-close", false),
    ] {
        let (width, height, after) = first_panel_frame(ui, &client, selector, open, sample.clone());
        assert_eq!(width < wide, open);
        assert_eq!(height > initial_height, open);
        eprintln!(
            "history_reflow tail_uuid={id} panel_open={open} width_px={} gap_before_px={} gap_after_px={}",
            f32::from(width),
            f32::from(before),
            f32::from(after)
        );
        assert!(
            (after - before).abs() <= px(1.),
            "Хвост, панель открыта={open}: {before:?} → {after:?}"
        );
    }
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
