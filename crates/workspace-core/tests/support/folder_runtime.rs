//! Проверяет загрузку и восстановление папок через штатный Runtime.
//! Локальный сервер управляет журналом, страницами и для одной папки задерживает старый ответ.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use reqwest::Url;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use serde_json::{Value, json};
use tokio::{
    io::AsyncWriteExt,
    sync::{Notify, broadcast, oneshot, watch},
    time::timeout,
};
use tokio_tungstenite::tungstenite::Message as Frame;
use uuid::Uuid;
use workspace_core::{
    auth::Auth,
    messenger::{FolderSnapshot, MessengerSource, SOURCE_ID, folders::FolderKind},
    runtime::Runtime,
    storage::Storage,
    sync::Sync,
};

use super::{
    super::{
        MemoryStores,
        catalog_revocation::{A, PROJECT, TestIam},
    },
    CA, HistoryApi, HttpsSocket, serve, upgrade,
};

const TIMESTAMP: &str = "2026-01-01T00:00:00Z";
const ALL: Uuid = Uuid::nil();
const CUSTOM: Uuid = Uuid::from_u128(31);
const TO_REMOVE: Uuid = Uuid::from_u128(32);
const KNOWN_CHAT: Uuid = Uuid::from_u128(41);
const UNKNOWN_CHAT: Uuid = Uuid::from_u128(42);
const DEADLINE: Duration = Duration::from_secs(10);

/// Управляет одним открытым WebSocket.
#[derive(Clone, Debug)]
enum SocketCommand {
    Event(Value), // Событие, которое получит работающий клиент.
    Disconnect,   // Разорвать соединение до отправки следующего события.
}

/// Управляемые ответы сервера для прогона через настоящие REST и WebSocket.
pub(super) struct Api {
    socket: broadcast::Sender<SocketCommand>, // События и закрытие для текущего WebSocket.
    socket_opened: Notify,                    // Тест может проверить состояние до ready.
    send_ready: Notify,                       // Разрешение объявить журнал догнанным.
    connections: AtomicUsize,                 // Номер WebSocket для проверки позиции при открытии.
    pages: AtomicUsize,                       // Число первых страниц папок.
    fail_page: AtomicBool,                    // Вторая страница падает после успешной первой.
    page_failed: Notify,                      // Сбой второй страницы дошёл до сервера.
    stale_started: Notify,                    // Начался запрос снимка, который будет удалён.
    release_stale: Notify,                    // Разрешение вернуть снимок после удаления.
    replay: AtomicBool,                       // В журнале появился ответ, пропущенный WebSocket.
}

impl Api {
    /// Создаёт начальный снимок сервера без сбоев и событий переподключения.
    fn new() -> Self {
        Self {
            socket: broadcast::channel(8).0,
            socket_opened: Notify::new(),
            send_ready: Notify::new(),
            connections: AtomicUsize::new(0),
            pages: AtomicUsize::new(0),
            fail_page: AtomicBool::new(false),
            page_failed: Notify::new(),
            stale_started: Notify::new(),
            release_stale: Notify::new(),
            replay: AtomicBool::new(false),
        }
    }

    /// Раздаёт страницы папок, каталог и журнал для одного авторизованного владельца.
    pub(super) async fn reply(
        &self,
        mut socket: HttpsSocket,
        request: Url,
        method: &str,
        headers: &str,
    ) {
        assert_eq!(method, "GET");
        let path = request.path();
        let query: std::collections::HashMap<_, _> = request.query_pairs().collect();
        if path.ends_with("/events/ws") {
            assert!(headers.contains(&format!("bearer.{A}")));
            assert_eq!(query["epoch_generation"], "folders-test");
            let connection = self.connections.fetch_add(1, SeqCst);
            let expected_cursor = if connection == 0 { 8 } else { 12 };
            assert_eq!(query["last_epoch_version"], expected_cursor.to_string());
            self.realtime(upgrade(socket, headers).await, expected_cursor)
                .await;
            return;
        }
        assert!(headers.contains(&format!("authorization: Bearer {A}")));
        let mut marker = "";
        let mut status = "200 OK";
        let body = if path.ends_with("/epoch/") {
            json!({"epoch_generation":"folders-test", "epoch_version":7, "current_epoch_version":7, "minimum_epoch_version":1})
        } else if path.ends_with("/events/") {
            assert_eq!(query["epoch_generation"], "folders-test");
            match query["epoch_version>"].as_ref() {
                "7" => json!([folder_event(8, "updated", CUSTOM)]),
                "11" if self.replay.load(SeqCst) => json!([folder_event(12, "updated", CUSTOM)]),
                "8" | "12" => json!([]),
                value => panic!("неожиданная позиция REST: {value}"),
            }
        } else if path.ends_with("/folders/") {
            let failed = self.fail_page.load(SeqCst);
            if let Some(value) = query.get("page_marker") {
                assert_eq!(value, "next-folders");
                if failed {
                    status = "500 Internal Server Error";
                    self.page_failed.notify_one();
                    Value::Null
                } else {
                    json!([
                        folder(
                            CUSTOM,
                            "Рабочая",
                            vec![
                                item(CUSTOM, KNOWN_CHAT, 51, false),
                                item(CUSTOM, UNKNOWN_CHAT, 52, true),
                            ]
                        ),
                        folder(TO_REMOVE, "Будет удалена", vec![])
                    ])
                }
            } else {
                self.pages.fetch_add(1, SeqCst);
                marker = "X-Pagination-Marker: next-folders\r\n";
                let label = if failed {
                    "Неполный снимок"
                } else {
                    "Все чаты"
                };
                json!([folder(ALL, label, vec![])])
            }
        } else if path.ends_with(&format!("/folders/{ALL}")) {
            let mut baseline = folder(ALL, "Все чаты", vec![]);
            baseline["unread_count"] = json!(13);
            baseline
        } else if path.ends_with(&format!("/folders/{TO_REMOVE}")) {
            self.stale_started.notify_one();
            self.release_stale.notified().await;
            folder(TO_REMOVE, "Старый ответ", vec![])
        } else if path.ends_with(&format!("/folders/{CUSTOM}")) {
            let title = if self.replay.load(SeqCst) {
                "После догонки"
            } else {
                "После WebSocket"
            };
            folder(CUSTOM, title, vec![item(CUSTOM, KNOWN_CHAT, 51, true)])
        } else if path.ends_with("/streams/") {
            json!([{
                "uuid":KNOWN_CHAT, "name":"Канал", "project_id":PROJECT, "user_uuid":A,
                "private":false, "is_archived":false,
            }])
        } else if path.ends_with("/stream_topics/") {
            json!([])
        } else {
            panic!("неожиданный маршрут {path}");
        };
        let body = body.to_string();
        socket.get_mut().write_all(format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{marker}\r\n{body}",
            body.len()
        ).as_bytes()).await.unwrap();
        socket.get_mut().shutdown().await.unwrap();
    }

    /// Держит открытое соединение до команды отключения или остановки Runtime.
    async fn realtime(
        &self,
        mut socket: tokio_tungstenite::WebSocketStream<HttpsSocket>,
        cursor: u64,
    ) {
        let mut commands = self.socket.subscribe();
        self.socket_opened.notify_one();
        self.send_ready.notified().await;
        let ready =
            json!({"type":"ready", "epoch_generation":"folders-test", "epoch_version":cursor});
        socket.send(Frame::text(ready.to_string())).await.unwrap();
        loop {
            tokio::select! {
                command = commands.recv() => match command.unwrap() {
                    SocketCommand::Event(event) => socket.send(Frame::text(event.to_string())).await.unwrap(),
                    SocketCommand::Disconnect => {
                        socket.close(None).await.unwrap();
                        return;
                    }
                },
                frame = socket.next() => {
                    if !matches!(frame, Some(Ok(Frame::Ping(_) | Frame::Pong(_)))) {
                        return;
                    }
                }
            }
        }
    }
}

/// Готовит снимок в форме ответа сервера. У верхнего объекта отсутствуют поля владельца.
fn folder(id: Uuid, title: &str, items: Vec<Value>) -> Value {
    json!({
        "uuid":id, "title":title, "system_type":if id.is_nil() {"all"} else {"created"},
        "unread_count":0, "folder_items":items, "created_at":TIMESTAMP, "updated_at":TIMESTAMP,
    })
}

/// Готовит связь канала с отдельными активным и пассивным счётчиками.
fn item(folder_id: Uuid, stream: Uuid, id: u128, pinned: bool) -> Value {
    json!({
        "uuid":Uuid::from_u128(id), "folder_uuid":folder_id, "stream_uuid":stream,
        "project_id":PROJECT, "user_uuid":A, "chat_type":"stream", "pinned_at":pinned.then_some(TIMESTAMP),
        "unread_count":3, "active_unread_count":1, "passive_unread_count":2,
        "created_at":TIMESTAMP, "updated_at":TIMESTAMP,
    })
}

/// Собирает событие папки с полным снимком при обновлении и UUID при удалении.
fn folder_event(version: u64, action: &str, id: Uuid) -> Value {
    let mut payload = if action == "deleted" {
        json!({"uuid":id})
    } else {
        folder(id, "Снимок события", vec![])
    };
    payload["kind"] = json!(format!("folder.{action}"));
    json!({
        "schema_version":1, "uuid":format!("event-{version}"), "epoch_generation":"folders-test",
        "epoch_version":version, "project_id":PROJECT, "user_uuid":A, "object_type":"folder",
        "action":action, "payload":payload,
    })
}

/// Ждёт управляемый шаг сервера и указывает, какой шаг не произошёл вовремя.
async fn wait_step(step: &Notify, description: &str) {
    timeout(DEADLINE, step.notified()).await.expect(description);
}

/// Ждёт подтверждённый снимок через публичную подписку папок.
async fn wait_snapshot(
    read: &impl Fn() -> FolderSnapshot,
    changes: &mut watch::Receiver<u64>,
    description: &str,
    predicate: impl Fn(&FolderSnapshot) -> bool,
) -> FolderSnapshot {
    timeout(DEADLINE, async {
        loop {
            let snapshot = read();
            if predicate(&snapshot) {
                return snapshot;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect(description)
}

/// Проверяет цельный снимок, сбой страницы, удаление, догонку и восстановление с диска.
#[tokio::test]
async fn sync_folders_after_ready_and_recover_from_delayed_responses() {
    let api = Arc::new(Api::new());
    let (server, network) = serve(Arc::new(HistoryApi {
        folders: Some(api.clone()),
        ..HistoryApi::default()
    }))
    .await;
    let stores = Arc::new(MemoryStores::default());
    let auth = Auth::new(Arc::new(TestIam), stores.clone(), stores);
    let login = auth.begin_login(&server, A, "password", None).unwrap();
    let owner = auth.complete_login(login.id, PROJECT).unwrap();
    let generation = auth.snapshot()[0].generation;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("folders.sqlite");
    let storage = Arc::new(Storage::open(&path).unwrap());
    let source = Arc::new(
        MessengerSource::with_private_ca(CertificateDer::from_pem_slice(CA).unwrap().as_ref())
            .unwrap(),
    );
    let sync = Arc::new(Sync::new(auth.clone(), storage.clone()));
    let runtime = Runtime::new(sync.clone(), vec![source.clone()]).unwrap();
    let folders = source.folders(storage);
    let (stop, stopped) = oneshot::channel();
    let worker = tokio::spawn(runtime.run(async {
        let _ = stopped.await;
    }));
    let read = || folders.snapshot(&auth, &owner, generation).unwrap();
    let mut changes = folders.subscribe();

    wait_step(&api.socket_opened, "первый WebSocket не открылся").await;
    assert_eq!(
        sync.cursor(&owner, SOURCE_ID)
            .unwrap()
            .unwrap()
            .epoch_version,
        8
    );
    assert_eq!(api.pages.load(SeqCst), 0);
    assert!(read().folders.is_none());
    api.send_ready.notify_one();
    let initial = wait_snapshot(
        &read,
        &mut changes,
        "две страницы папок не загрузились",
        |s| !s.needs_refresh && s.rows(CUSTOM).is_some_and(|rows| rows.len() == 1),
    )
    .await;
    assert_eq!(initial.folders.as_ref().unwrap().len(), 3);
    assert_eq!(initial.folders.as_ref().unwrap()[0].kind, FolderKind::All);
    assert_eq!(initial.all_chats_unread, Some(13));
    let rows = initial.rows(CUSTOM).unwrap();
    assert_eq!(rows[0].chat.id, KNOWN_CHAT);
    assert_eq!(rows[0].item.unwrap().passive_unread_count, 2);
    assert_eq!(initial.rows(ALL).unwrap()[0].item, None);

    api.fail_page.store(true, SeqCst);
    folders.retry(&auth, &owner, generation).unwrap();
    wait_step(
        &api.page_failed,
        "не запрошена вторая страница повторной сверки",
    )
    .await;
    let after_failure = read();
    assert!(after_failure.needs_refresh);
    assert_eq!(after_failure.folders, initial.folders);
    api.fail_page.store(false, SeqCst);
    wait_snapshot(
        &read,
        &mut changes,
        "повтор после сбоя не завершился",
        |s| !s.needs_refresh,
    )
    .await;

    api.socket
        .send(SocketCommand::Event(folder_event(9, "updated", TO_REMOVE)))
        .unwrap();
    wait_step(&api.stale_started, "не началась точечная сверка").await;
    api.socket
        .send(SocketCommand::Event(folder_event(10, "deleted", TO_REMOVE)))
        .unwrap();
    wait_snapshot(
        &read,
        &mut changes,
        "удаление не применилось до ответа GET",
        |s| s.rows(TO_REMOVE).is_none(),
    )
    .await;
    api.release_stale.notify_one();
    // После позднего ответа заставляем тот же worker завершить ещё одну точечную сверку.
    api.socket
        .send(SocketCommand::Event(folder_event(11, "updated", CUSTOM)))
        .unwrap();
    let after_live = wait_snapshot(
        &read,
        &mut changes,
        "изменение из WebSocket не сверено",
        |s| !s.needs_refresh && has_title(s, CUSTOM, "После WebSocket"),
    )
    .await;
    assert!(after_live.rows(TO_REMOVE).is_none());

    api.replay.store(true, SeqCst);
    api.socket.send(SocketCommand::Disconnect).unwrap();
    wait_step(
        &api.socket_opened,
        "WebSocket не подключился после REST-догонки",
    )
    .await;
    assert_eq!(
        sync.cursor(&owner, SOURCE_ID)
            .unwrap()
            .unwrap()
            .epoch_version,
        12
    );
    api.send_ready.notify_one();
    let after_replay = wait_snapshot(
        &read,
        &mut changes,
        "изменение из REST не сверено",
        |s| !s.needs_refresh && has_title(s, CUSTOM, "После догонки"),
    )
    .await;
    assert!(after_replay.rows(TO_REMOVE).is_none());
    assert!(
        after_replay.rows(CUSTOM).unwrap()[0]
            .item
            .unwrap()
            .pinned_at
            .is_some()
    );
    stop.send(()).unwrap();
    timeout(DEADLINE, worker)
        .await
        .expect("Runtime не остановился")
        .unwrap()
        .unwrap();
    drop(folders);
    drop(sync);
    let reopened = source.folders(Arc::new(Storage::open(&path).unwrap()));
    let cached = reopened.snapshot(&auth, &owner, generation).unwrap();
    assert_eq!(cached.folders, after_replay.folders);
    assert_eq!(cached.all_chats_unread, Some(13));
    assert!(cached.rows(TO_REMOVE).is_none());
    network.abort();
}

/// Проверяет название конкретной папки в последнем опубликованном снимке.
fn has_title(snapshot: &FolderSnapshot, id: Uuid, title: &str) -> bool {
    snapshot.folders.as_ref().is_some_and(|folders| {
        folders
            .iter()
            .any(|folder| folder.id == id && folder.title == title)
    })
}
