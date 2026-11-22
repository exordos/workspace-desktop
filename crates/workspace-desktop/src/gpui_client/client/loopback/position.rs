//! Проверяет чтение A → B → A и применение запоздавшей страницы через рабочие маршруты GPUI и HTTPS.

use gpui::{ScrollDelta, ScrollWheelEvent, point};

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
