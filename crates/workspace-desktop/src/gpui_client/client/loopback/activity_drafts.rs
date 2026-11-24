//! Проверяет в рабочем окне точный переход из «Черновиков» и удаление выбранной записи.

use workspace_core::storage::{DraftScope, DraftWriteOutcome, PlaintextDraft};

use super::*;

const SECOND_DRAFT: &str = "00000000-0000-0000-0000-0000000000d2";
const REMOTE_DRAFT: &str = "00000000-0000-0000-0000-0000000000d3";
const OPEN_REMOTE: &str = "activity-draft-open-00000000-0000-0000-0000-0000000000d3";
const DELETE_REMOTE: &str = "activity-draft-delete-00000000-0000-0000-0000-0000000000d3";
const DRAFTS: &str = "/api/workspace/v1/messenger/drafts/";
const OPEN_SECOND: &str = "activity-draft-open-00000000-0000-0000-0000-0000000000d2";
const DELETE_SECOND: &str = "activity-draft-delete-00000000-0000-0000-0000-0000000000d2";
const FIRST_TEXT: &str = "Набранный в обычном редакторе текст";
const UPDATED_FIRST_TEXT: &str = "Набранный в обычном редакторе текст с дополнением";
const SECOND_TEXT: &str = "Другой черновик этой же темы";

/// Управляет состоянием одного серверного черновика и его условным удалением.
#[derive(Default)]
pub(super) struct Fixture {
    change_on_delete: AtomicBool, // Первый DELETE приходит после серверной правки.
    changed: AtomicBool,          // Сервер уже показал версию 4. Исходная версия 3.
    removed: AtomicBool,          // Сервер принял DELETE с верным ETag.
    if_match: Mutex<Vec<String>>, // Предусловия каждого фактического DELETE.
}

impl Fixture {
    /// Даёт разные страницы и проверяет точный ETag при удалении.
    pub(super) fn response(
        &self,
        request: &Request<Incoming>,
        user: &str,
    ) -> Option<Response<Full<Bytes>>> {
        let id = request.uri().path().strip_prefix(DRAFTS)?;
        let revision = if self.changed.load(Ordering::SeqCst) {
            4
        } else {
            3
        };
        if id.is_empty() {
            assert_eq!(request.method(), Method::GET);
            assert_eq!(query_value(request, "sort_key"), Some("updated_at"));
            assert_eq!(query_value(request, "sort_dir"), Some("desc"));
            let response = match query_value(request, "page_marker") {
                None if self.removed.load(Ordering::SeqCst) => {
                    json_response(StatusCode::OK, json!([remote_row(user, SECOND_DRAFT, 3)]))
                }
                None => {
                    let mut page = json_response(
                        StatusCode::OK,
                        json!([remote_row(user, REMOTE_DRAFT, revision)]),
                    );
                    page.headers_mut()
                        .insert("X-Pagination-Marker", REMOTE_DRAFT.parse().unwrap());
                    page
                }
                Some(REMOTE_DRAFT) => {
                    json_response(StatusCode::OK, json!([remote_row(user, SECOND_DRAFT, 3)]))
                }
                _ => panic!("Клиент должен повторить маркер выданной страницы"),
            };
            return Some(response);
        }
        assert_eq!(id, REMOTE_DRAFT);
        if self.removed.load(Ordering::SeqCst) {
            return Some(json_response(StatusCode::NOT_FOUND, Value::Null));
        }
        let etag = format!("\"{revision}\"");
        if request.method() == Method::DELETE {
            let if_match = request.headers()[header::IF_MATCH].to_str().unwrap();
            assert_eq!(if_match, etag);
            self.if_match.lock().unwrap().push(if_match.into());
            if self.change_on_delete.swap(false, Ordering::SeqCst) {
                self.changed.store(true, Ordering::SeqCst);
                return Some(json_response(
                    StatusCode::PRECONDITION_FAILED,
                    remote_row(user, id, 4),
                ));
            }
            self.removed.store(true, Ordering::SeqCst);
            return Some(
                Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(Full::default())
                    .unwrap(),
            );
        }
        assert_eq!(request.method(), Method::GET);
        let mut response = json_response(StatusCode::OK, remote_row(user, id, revision));
        response
            .headers_mut()
            .insert(header::ETAG, etag.parse().unwrap());
        Some(response)
    }
}

/// Возвращает серверный снимок, доступный только владельцу тестового входа.
fn remote_row(user: &str, id: &str, revision: u64) -> Value {
    json!({
        "uuid": id, "project_id": PROJECT, "user_uuid": user,
        "stream_uuid": STREAM, "topic_uuid": TOPIC, "revision": revision,
        "updated_at": "2026-11-18T12:00:00Z",
        "payload": {"kind": "markdown", "content": if revision == 3 { MARKER } else { "Серверный черновик изменился" }},
    })
}

/// Ждёт загрузки редактора и проверяет, какой текст из одной темы в нём появился.
fn wait_for_text(client: &Entity<Client>, ui: &mut VisualTestContext, text: &str, step: &str) {
    let topic = TOPIC.parse().unwrap();
    until(ui, step, |ui| {
        ui.read(|app| {
            client.read(app).conversation.as_ref().is_some_and(|view| {
                view.read(app).tested_composer().is_some_and(|view| {
                    let composer = view.read(app);
                    composer.tested_send_state(app).0 == Some(topic) && composer.draft(app) == text
                })
            })
        })
    });
}

/// Открывает второй UUID той же темы, затем удаляет его и проверяет сохранность обычного ввода.
#[gpui::test]
fn opens_and_deletes_only_the_selected_draft_in_a_shared_topic(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::start();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let task = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(task, cx));
    });
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(
        ui,
        "в каталоге появился личный чат",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, CHAT_ROW);
    wait_for_text(&client, ui, "", "обычный редактор готов");
    let session = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    click(ui, "workspace-composer-editor");
    ui.simulate_input(FIRST_TEXT);
    let rows = futures::executor::block_on(runtime.list_local_drafts(session.clone())).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].preview, FIRST_TEXT.into());

    let owner = session.owner.clone();
    let generation = session.generation;
    let scope = DraftScope {
        stream_id: STREAM.parse().unwrap(),
        topic_id: TOPIC.parse().unwrap(),
    };
    let version = futures::executor::block_on(runtime.request(move |services| async move {
        let opening = services
            .sync
            .prepare_draft_opening(&owner, generation, scope.stream_id)?;
        let (access, _) = services
            .sync
            .open_draft(&owner, generation, opening, scope)?;
        services.sync.save_draft(
            &owner,
            generation,
            &access,
            &PlaintextDraft {
                id: SECOND_DRAFT.parse().unwrap(),
                version: 1,
                markdown: SECOND_TEXT.into(),
                replies: None,
            },
        )
    }))
    .unwrap()
    .unwrap();
    assert_eq!(version, DraftWriteOutcome::Saved);

    ui.simulate_input(" с дополнением");
    click(ui, "activity-drafts");
    until(
        ui,
        "локальный список остаётся видимым после ответа сервера с ошибкой",
        |ui| {
            ui.debug_bounds(OPEN_SECOND).is_some()
                && ui.read(|app| {
                    client
                        .read(app)
                        .activity_drafts
                        .read(app)
                        .tested_server_failed()
                })
        },
    );
    let rows = futures::executor::block_on(runtime.list_local_drafts(session.clone())).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.preview == UPDATED_FIRST_TEXT.into()));
    click(ui, OPEN_SECOND);
    until(
        ui,
        "оболочка приняла точный UUID из карточки",
        |ui| {
            ui.read(|app| {
                client.read(app).shell.conversation_draft() == Some(SECOND_DRAFT.parse().unwrap())
            })
        },
    );
    wait_for_text(
        &client,
        ui,
        SECOND_TEXT,
        "открыт выбранный UUID, а не первый ввод темы",
    );

    click(ui, "activity-drafts");
    until(
        ui,
        "доступна кнопка удаления второго черновика",
        |ui| {
            ui.debug_bounds(DELETE_SECOND).is_some()
                && ui.read(|app| {
                    let page = client.read(app).activity_drafts.read(app);
                    page.tested_server_failed()
                        && page
                            .tested_draft_version(SECOND_DRAFT.parse().unwrap())
                            .is_some()
                })
        },
    );
    click(ui, DELETE_SECOND);
    until(
        ui,
        "удалённая карточка исчезла из списка",
        |ui| {
            ui.read(|app| {
                client
                    .read(app)
                    .activity_drafts
                    .read(app)
                    .tested_draft_version(SECOND_DRAFT.parse().unwrap())
                    .is_none()
            })
        },
    );
    let rows = futures::executor::block_on(runtime.list_local_drafts(session)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].preview, UPDATED_FIRST_TEXT.into());
    click(ui, CHAT_ROW);
    wait_for_text(
        &client,
        ui,
        UPDATED_FIRST_TEXT,
        "обычный вход восстановил оставшийся черновик",
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// Применяет локальный приоритет, открывает серверный UUID и отправляет его без правки.
#[gpui::test]
fn opens_server_draft_and_sends_the_untouched_snapshot(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        server_drafts: Some(Fixture::default()),
        ..Received::default()
    });
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let task = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(task, cx));
    });
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(
        ui,
        "доступна локальная страница черновиков",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    let session = ui.read(|app| client.read(app).shell.selected().unwrap().clone());
    let owner = session.owner.clone();
    let generation = session.generation;
    let scope = DraftScope {
        stream_id: STREAM.parse().unwrap(),
        topic_id: TOPIC.parse().unwrap(),
    };
    futures::executor::block_on(runtime.request(move |services| async move {
        let opening = services
            .sync
            .prepare_draft_opening(&owner, generation, scope.stream_id)?;
        let (access, _) = services
            .sync
            .open_draft(&owner, generation, opening, scope)?;
        services.sync.save_draft(
            &owner,
            generation,
            &access,
            &PlaintextDraft {
                id: SECOND_DRAFT.parse().unwrap(),
                version: 1,
                markdown: SECOND_TEXT.into(),
                replies: None,
            },
        )
    }))
    .unwrap()
    .unwrap();
    click(ui, "activity-drafts");
    until(
        ui,
        "серверная карточка добавилась к локальной",
        |ui| ui.debug_bounds(OPEN_REMOTE).is_some(),
    );
    assert!(ui.debug_bounds(OPEN_SECOND).is_some());
    assert!(ui.debug_bounds(DELETE_REMOTE).is_some());
    assert!(ui.debug_bounds(DELETE_SECOND).is_some());
    click(ui, OPEN_REMOTE);
    wait_for_text(
        &client,
        ui,
        MARKER,
        "серверный UUID загрузил текст в обычный редактор",
    );
    click(ui, "workspace-composer-editor");
    press_enter(ui);
    until(
        ui,
        "сервер принял сообщение из неизменённого черновика",
        |_| server.state.sent.lock().unwrap().len() == 1,
    );

    click(ui, "activity-drafts");
    until(
        ui,
        "локальная карточка осталась на месте",
        |ui| ui.debug_bounds(OPEN_SECOND).is_some(),
    );
    let server_rows = futures::executor::block_on(runtime.list_server_drafts(session)).unwrap();
    assert!(server_rows.is_empty());
    until(
        ui,
        "отправленный серверный черновик остаётся скрытым в снимке списка",
        |ui| {
            ui.read(|app| {
                client
                    .read(app)
                    .activity_drafts
                    .read(app)
                    .tested_draft_version(REMOTE_DRAFT.parse().unwrap())
                    .is_none()
            })
        },
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}

/// При 412 показывает изменённую карточку и удаляет её только после нового клика.
#[gpui::test]
fn remote_draft_delete_requires_fresh_explicit_click_after_precondition_failure(
    cx: &mut TestAppContext,
) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        server_drafts: Some(Fixture {
            change_on_delete: AtomicBool::new(true),
            ..Fixture::default()
        }),
        ..Received::default()
    });
    let fixture = server.state.server_drafts.as_ref().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, events) =
        start_runtime(profile.path(), saved_account(&server, USER, Uuid::nil()));
    let (client, ui) =
        cx.add_window_view(|_, cx| Client::new(Options::default(), runtime.clone(), cx));
    ui.update(|_, app| {
        let task = events.attach(&client, app, Client::handle_runtime);
        client.update(app, |client, cx| client.attach(task, cx));
    });
    ui.simulate_resize(size(px(1440.), px(900.)));
    until(
        ui,
        "доступна страница черновиков",
        |ui| ui.debug_bounds(CHAT_ROW).is_some(),
    );
    click(ui, "activity-drafts");
    until(
        ui,
        "получена серверная версия 3",
        |ui| ui.debug_bounds(DELETE_REMOTE).is_some(),
    );
    click(ui, DELETE_REMOTE);
    until(
        ui,
        "412 показан рядом с обновлённой версией 4",
        |ui| {
            ui.debug_bounds("draft-server-delete-changed").is_some()
                && ui.read(|app| {
                    client
                        .read(app)
                        .activity_drafts
                        .read(app)
                        .tested_draft_version(REMOTE_DRAFT.parse().unwrap())
                        == Some(4)
                })
        },
    );
    assert_eq!(*fixture.if_match.lock().unwrap(), ["\"3\""]);
    assert!(!fixture.removed.load(Ordering::SeqCst));
    click(ui, DELETE_REMOTE);
    until(
        ui,
        "удалена только выбранная серверная версия",
        |ui| {
            fixture.removed.load(Ordering::SeqCst)
                && ui.read(|app| {
                    let page = client.read(app).activity_drafts.read(app);
                    page.tested_draft_version(REMOTE_DRAFT.parse().unwrap())
                        .is_none()
                        && page.tested_draft_version(SECOND_DRAFT.parse().unwrap()) == Some(3)
                })
        },
    );
    assert_eq!(*fixture.if_match.lock().unwrap(), ["\"3\"", "\"4\""]);
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
