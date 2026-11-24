//! Проверяет историю архива и восстановление разговора из штатного сайдбара.

use super::*;
use crate::gpui_client::contracts::ChatEntry;

const ENTRY: &str = "new-chats-stream-10000000-0000-4000-8000-000000000005";
const RESTORE: &str = "new-chats-restore-10000000-0000-4000-8000-000000000005";

/// Архив скрывает редактор, а подтверждённое восстановление возвращает чат в общий список.
#[gpui::test]
fn new_chats_opens_read_only_archive_and_restores_the_chat(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let server = Server::serve(Received {
        new_chats: Some(Fixture {
            archive_direct: true,
            ..Fixture::default()
        }),
        ..Received::default()
    });
    let fixture = server.state.new_chats.as_ref().unwrap();
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
    until(ui, "действующий вход", |_| {
        server.state.ready.load(Ordering::SeqCst) > 0
    });

    wait_click(ui, "new-chats-open");
    assert!(
        ui.debug_bounds("new-chats-unsubscribed-unavailable")
            .is_none()
    );
    wait_click(ui, "new-chats-filter-unsubscribed");
    until(
        ui,
        "видна подсказка о каталоге в работе",
        |ui| {
            ui.debug_bounds("new-chats-unsubscribed-unavailable")
                .is_some()
        },
    );
    wait_click(ui, "new-chats-filter-all");
    assert!(ui.debug_bounds(ENTRY).is_none());
    wait_click(ui, "new-chats-filter-archive");
    wait_click(ui, ENTRY);
    until(
        ui,
        "открыта подтверждённая архивная история",
        |ui| {
            ui.read(|app| {
                client.read(app).conversation.as_ref().is_some_and(|view| {
                    let view = view.read(app);
                    view.has_access() && view.address().route.entry == ChatEntry::Archived
                })
            }) && ui.debug_bounds("conversation-archive-notice").is_some()
        },
    );
    assert!(ui.debug_bounds("workspace-composer-editor").is_none());

    wait_click(ui, "new-chats-open");
    wait_click(ui, "new-chats-filter-archive");
    wait_click(ui, RESTORE);
    until(
        ui,
        "сервер принял единственное восстановление",
        |_| fixture.restore_posts.load(Ordering::SeqCst) == 1,
    );
    wait_click(ui, "new-chats-filter-all");
    wait_click(ui, ENTRY);
    until(
        ui,
        "восстановленная личка снова открыта для отправки",
        |ui| {
            ui.read(|app| {
                client.read(app).conversation.as_ref().is_some_and(|view| {
                    let view = view.read(app);
                    view.has_access() && view.address().route.entry == ChatEntry::Latest
                })
            }) && ui.debug_bounds("workspace-composer-editor").is_some()
        },
    );
    assert_eq!(
        futures::executor::block_on(runtime.shutdown()),
        RuntimeExit::Clean
    );
}
