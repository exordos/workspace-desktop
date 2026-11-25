//! Проверяет автоматическую отметку видимых сообщений через GPUI, HTTPS и WebSocket.
//! Сервер подтверждает флаги чтения раньше, чем публикует новый счётчик.

use super::*;
use gpui::{ScrollDelta, ScrollWheelEvent, point};

const FIRST_ROW: u128 = 101;
const FIRST_UNREAD: u128 = 105;
const LAST_ROW: u128 = 170;
const RECEIPT: Uuid = Uuid::from_u128(700);
const RECEIPT_PATH: &str =
    "/api/workspace/v1/messenger/message_mutations/00000000-0000-0000-0000-0000000002bc";

/// Держит независимо серверный флаг чтения и опубликованный счётчик.
pub(super) struct Fixture {
    pub(super) live: broadcast::Sender<RecoverySignal>, // События чтения и счётчиков текущего входа.
    written: Mutex<u128>,       // Последняя строка, которую сервер уже отметил.
    remote_single: AtomicUsize, // Отдельная прочитанная строка или ноль до события.
    counter: AtomicUsize,       // Последнее опубликованное число непрочитанных.
    requests: Mutex<Vec<u128>>, // Запрошенные границы в порядке поступления.
    first_lookups: AtomicUsize, // Поиск первой непрочитанной строки при открытии или сверке.
    status_polls: AtomicUsize,  // Обращения за результатом принятой команды.
    applied: AtomicBool,        // Журнал уже зафиксировал команду.
}

impl Fixture {
    /// Готовит историю, в которой первое непрочитанное старше страницы хвоста.
    fn new() -> Self {
        Self {
            live: broadcast::channel(16).0,
            written: Mutex::new(FIRST_UNREAD - 1),
            remote_single: AtomicUsize::new(0),
            counter: AtomicUsize::new(unread_after(FIRST_UNREAD - 1) as usize),
            requests: Mutex::default(),
            first_lookups: AtomicUsize::new(0),
            status_polls: AtomicUsize::new(0),
            applied: AtomicBool::new(false),
        }
    }

    /// Отвечает на чтение истории, приём отметки и проверку квитанции.
    pub(super) fn response(
        &self,
        request: &Request<Incoming>,
        user: &str,
        peer: &str,
    ) -> Option<Response<Full<Bytes>>> {
        let path = request.uri().path();
        let result = if path == MESSAGES && request.method() == Method::GET {
            if query_value(request, "stream_uuid") != Some(STREAM) {
                return None;
            }
            self.page(request, user, peer)
        } else if path == RECEIPT_PATH && request.method() == Method::GET {
            self.status_polls.fetch_add(1, Ordering::SeqCst);
            let status = if self.applied.load(Ordering::SeqCst) {
                "applied"
            } else {
                "pending"
            };
            json_response(
                StatusCode::OK,
                json!({"uuid": RECEIPT, "project_id": PROJECT, "user_uuid": user, "status": status}),
            )
        } else if let Some(message) = path.strip_prefix(MESSAGES) {
            self.message(request.method(), message, user, peer)?
        } else if request.method() == Method::GET
            && (path == FOLDERS
                || path.ends_with("00000000-0000-0000-0000-000000000000")
                    && path.starts_with(FOLDERS))
        {
            let count = self.counter.load(Ordering::SeqCst) as u32;
            let folder = folder_row(Uuid::nil(), "Все чаты", "all", count);
            let data = if path == FOLDERS {
                json!([folder])
            } else {
                folder
            };
            json_response(StatusCode::OK, data)
        } else {
            return None;
        };
        Some(result)
    }

    /// Возвращает страницу в точном серверном порядке и учитывает фильтр непрочитанного.
    fn page(&self, request: &Request<Incoming>, user: &str, peer: &str) -> Response<Full<Bytes>> {
        assert!(query_value(request, "topic_uuid").is_none_or(|topic| topic == TOPIC));
        assert_eq!(query_value(request, "sort_key"), Some("created_at"));
        let limit: usize = query_value(request, "page_limit").unwrap().parse().unwrap();
        let marker =
            query_value(request, "page_marker").map(|uuid| uuid.parse::<Uuid>().unwrap().as_u128());
        let asc = query_value(request, "sort_dir") == Some("asc");
        let unread_only = query_value(request, "read") == Some("false");
        let bound = |name| {
            query_value(request, name)
                .and_then(|value| value.split_once('.'))
                .and_then(|(_, value)| value.strip_suffix('Z'))
                .map(|value| value.parse::<u128>().unwrap())
        };
        let first = bound("created_at%3D%3E").unwrap_or(FIRST_ROW);
        let last = bound("created_at%3D%3C").unwrap_or(LAST_ROW);
        if unread_only && limit == 1 {
            assert!(asc);
            self.first_lookups.fetch_add(1, Ordering::SeqCst);
        }
        let written = *self.written.lock().unwrap();
        let remote = self.remote_single.load(Ordering::SeqCst) as u128;
        let mut ids: Vec<_> = (first..=last)
            .filter(|&id| !unread_only || (id > written && id != remote))
            .filter(|&id| marker.is_none_or(|marker| if asc { id > marker } else { id < marker }))
            .collect();
        if !asc {
            ids.reverse();
        }
        let has_next = ids.len() > limit;
        ids.truncate(limit);
        let page: Vec<_> = ids
            .iter()
            .map(|&id| row(id, id <= written || id == remote, user, peer))
            .collect();
        let mut response = json_response(StatusCode::OK, json!(page));
        if has_next {
            let last = Uuid::from_u128(*ids.last().unwrap());
            response
                .headers_mut()
                .insert("X-Pagination-Marker", last.to_string().parse().unwrap());
        }
        response
    }

    /// Показывает серверный флаг строки или принимает один запрос в журнал.
    fn message(
        &self,
        method: &Method,
        path: &str,
        user: &str,
        peer: &str,
    ) -> Option<Response<Full<Bytes>>> {
        if *method == Method::GET {
            let id = path.parse::<Uuid>().ok()?.as_u128();
            if !(FIRST_ROW..=LAST_ROW).contains(&id) {
                return None;
            }
            let read = id <= *self.written.lock().unwrap()
                || id == self.remote_single.load(Ordering::SeqCst) as u128;
            return Some(json_response(StatusCode::OK, row(id, read, user, peer)));
        }
        let id = path
            .strip_suffix("/actions/read_up_to/invoke")?
            .parse::<Uuid>()
            .ok()?
            .as_u128();
        assert_eq!(*method, Method::POST);
        assert_eq!(user, USER);
        self.requests.lock().unwrap().push(id);
        if self.applied.load(Ordering::SeqCst) {
            let mut written = self.written.lock().unwrap();
            *written = (*written).max(id);
            return Some(json_response(StatusCode::OK, row(id, true, user, peer)));
        }
        let read = id <= *self.written.lock().unwrap();
        let mut response = json_response(StatusCode::OK, row(id, read, user, peer));
        response.headers_mut().insert(
            "X-Workspace-Mutation-Id",
            RECEIPT.to_string().parse().unwrap(),
        );
        Some(response)
    }

    /// Фиксирует флаги без публикации нового счётчика.
    fn apply(&self, boundary: u128) {
        *self.written.lock().unwrap() = boundary;
        self.applied.store(true, Ordering::SeqCst);
    }

    /// Публикует один снимок независимо от способа чтения на другом устройстве.
    fn publish_read_event(&self, id: u128, epoch_version: u64) {
        let mut payload = row(id, true, USER, PEER);
        payload["kind"] = json!("message.read");
        let frame = json!({
            "schema_version": 1, "uuid": Uuid::new_v4(),
            "epoch_generation": "local", "epoch_version": epoch_version,
            "project_id": PROJECT, "user_uuid": USER,
            "object_type": "message", "action": "read", "payload": payload,
        });
        assert!(self.live.send(RecoverySignal::Event(frame)).is_ok());
    }

    /// Отправляет абсолютное число через тот же WebSocket, что использует рабочий клиент.
    fn publish_count(&self, boundary: u128) {
        let count = unread_after(boundary);
        self.counter.store(count as usize, Ordering::SeqCst);
        let frame = json!({
            "schema_version": 1, "uuid": Uuid::new_v4(),
            "epoch_generation": "local", "epoch_version": 2,
            "project_id": PROJECT, "user_uuid": USER,
            "object_type": "folder", "action": "updated",
            "payload": {"kind": "folder.updated", "uuid": Uuid::nil(), "unread_count": count},
        });
        assert!(self.live.send(RecoverySignal::Event(frame)).is_ok());
    }
}

/// Даёт текущие флаги чтения отдельно от текста и адреса сообщения.
fn row(id: u128, read: bool, user: &str, peer: &str) -> Value {
    json!({
        "uuid": Uuid::from_u128(id), "stream_uuid": STREAM, "topic_uuid": TOPIC,
        "user_uuid": user, "project_id": PROJECT, "author_uuid": peer, "source_name": "native",
        "read": read, "created_at": format!("2026-11-17T10:00:00.{id:06}Z"),
        "payload": {"kind": "markdown", "content": format!("Строка для проверки чтения {id}")},
    })
}

/// Считает строки после подтверждённой границы тестового разговора.
fn unread_after(boundary: u128) -> u32 {
    (LAST_ROW - boundary) as u32
}

/// Читает подтверждённый счётчик из оболочки рабочего окна.
fn unread(ui: &VisualTestContext, client: &Entity<Client>) -> Option<u32> {
    ui.read(|app| client.read(app).shell.accounts().first()?.unread)
}

/// Проверяет флаг в сохранённой истории через штатный поток клиента.
fn stored_read(runtime: &RuntimeClient, address: &ConversationAddress, id: u128) -> Option<bool> {
    let address = address.clone();
    futures::executor::block_on(runtime.request(move |services| async move {
        services
            .sync
            .history_windows(
                &address.route.owner,
                address.generation,
                address.route.scope,
            )
            .ok()?
            .into_iter()
            .flat_map(|window| window.messages)
            .find(|row| row.id == Uuid::from_u128(id))?
            .read
    }))
    .ok()
    .flatten()
}

/// Открывает длинную историю в неактивном тестовом окне через список чатов.
fn open_inactive<'a>(
    cx: &'a mut TestAppContext,
    server: &Server,
    runtime: &RuntimeClient,
    events: RuntimeEvents,
) -> (
    Entity<Client>,
    &'a mut VisualTestContext,
    ConversationAddress,
) {
    cx.update(bind_keys);
    let fixture = server.state.read_boundary.as_ref().unwrap();
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let subscription = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(subscription, cx));
    });
    ui.simulate_resize(size(px(960.), px(570.)));
    assert!(!ui.update(|window, _| window.is_window_active()));
    let before = unread_after(FIRST_UNREAD - 1);
    until(
        ui,
        "каталог, WSS и исходный счётчик",
        |ui| {
            ui.debug_bounds(CHAT_ROW).is_some()
                && server.state.ready.load(Ordering::SeqCst) == 1
                && unread(ui, &client) == Some(before)
        },
    );
    click(ui, CHAT_ROW);
    until(
        ui,
        "первое непрочитанное видно в длинной истории",
        |ui| {
            fixture.first_lookups.load(Ordering::SeqCst) > 0
                && ui.read(|app| {
                    client.read(app).conversation.as_ref().is_some_and(|view| {
                        view.read(app)
                            .message_visible_for_test(Uuid::from_u128(FIRST_UNREAD))
                    })
                })
        },
    );
    assert!(fixture.requests.lock().unwrap().is_empty());
    let address = ui.read(|app| client.read(app).shell.conversation().unwrap().clone());
    (client, ui, address)
}

/// Проверяет независимые подтверждения флагов и счётчика в активированном окне.
#[gpui::test]
fn visible_reads_wait_for_journal_and_separate_counter_event(cx: &mut TestAppContext) {
    let executor = cx.executor();
    let server = Server::serve(Received {
        read_boundary: Some(Fixture::new()),
        ..Received::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let account = saved_account(&server, USER, Uuid::nil());
    let (runtime, events) = start_runtime(profile.path(), account);
    let (client, ui, address) = open_inactive(cx, &server, &runtime, events);
    let fixture = server.state.read_boundary.as_ref().unwrap();
    let before = unread_after(FIRST_UNREAD - 1);
    assert_eq!(stored_read(&runtime, &address, FIRST_UNREAD), Some(false));

    ui.update(|window, _| window.activate_window());
    until(
        ui,
        "видимая граница принята, результат журнала ещё ожидается",
        |_| fixture.status_polls.load(Ordering::SeqCst) > 0,
    );
    let boundary = {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        requests[0]
    };
    assert!((FIRST_UNREAD..LAST_ROW).contains(&boundary));
    assert_eq!(stored_read(&runtime, &address, FIRST_UNREAD), Some(false));
    assert_eq!(unread(ui, &client), Some(before));
    let pending =
        Box::leak(format!("history-read-pending-{}", Uuid::from_u128(boundary)).into_boxed_str());
    until(
        ui,
        "долгое подтверждение подсвечивает границу",
        |ui| {
            executor.advance_clock(Duration::from_millis(20));
            ui.debug_bounds(pending).is_some()
        },
    );

    fixture.apply(boundary);
    until(
        ui,
        "команда подтверждена без нового счётчика",
        |ui| {
            stored_read(&runtime, &address, FIRST_UNREAD) == Some(true)
                && unread(ui, &client) == Some(before)
        },
    );
    fixture.publish_count(boundary);
    until(ui, "счётчик получен по WebSocket", |ui| {
        unread(ui, &client) == Some(unread_after(boundary))
    });
    assert_eq!(fixture.requests.lock().unwrap().as_slice(), [boundary]);

    let position = ui
        .debug_bounds("conversation-history-focus")
        .unwrap()
        .center();
    ui.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Pixels(point(px(0.), px(-180.))),
        ..Default::default()
    });
    until(
        ui,
        "скролл продвинул отметку чтения",
        |_| {
            let latest = *fixture.requests.lock().unwrap().last().unwrap();
            latest > boundary && stored_read(&runtime, &address, latest) == Some(true)
        },
    );
    assert_eq!(unread(ui, &client), Some(unread_after(boundary)));
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Проверяет, что событие одиночного чтения не очищает более ранние строки темы.
#[gpui::test]
fn remote_single_read_keeps_earlier_unread(cx: &mut TestAppContext) {
    let server = Server::serve(Received {
        read_boundary: Some(Fixture::new()),
        ..Received::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let account = saved_account(&server, USER, Uuid::nil());
    let (runtime, events) = start_runtime(profile.path(), account);
    let (_, ui, address) = open_inactive(cx, &server, &runtime, events);
    let fixture = server.state.read_boundary.as_ref().unwrap();
    let remote = FIRST_UNREAD + 4;
    assert_eq!(stored_read(&runtime, &address, remote), Some(false));
    fixture
        .remote_single
        .store(remote as usize, Ordering::SeqCst);
    fixture.publish_read_event(remote, 2);
    until(
        ui,
        "прочитанная на другом устройстве строка обновилась",
        |_| stored_read(&runtime, &address, remote) == Some(true),
    );
    assert_eq!(stored_read(&runtime, &address, FIRST_UNREAD), Some(false));

    // У чтения всей области точно такой же конверт. Лишь сверка подтверждает остальные строки.
    let next = remote + 2;
    fixture.apply(next);
    fixture.publish_read_event(next, 3);
    until(
        ui,
        "после чтения до границы обновился серверный префикс",
        |_| stored_read(&runtime, &address, FIRST_UNREAD) == Some(true),
    );
    assert_eq!(stored_read(&runtime, &address, next + 1), Some(false));
    assert!(fixture.requests.lock().unwrap().is_empty());
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
