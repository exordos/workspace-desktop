//! Проверяет цитирование, вкладки и неизменяемый снимок очереди через рабочий разговор.

use gpui::point;
use workspace_core::{messenger::quote, storage::MessageBody};

use super::*;

#[path = "quote_tests/drag.rs"]
mod drag;

const SECOND: Uuid = Uuid::from_u128(2003);
const OTHER: Uuid = Uuid::from_u128(2004);
const STREAM: Uuid = Uuid::from_u128(1000);

/// Создаёт читаемую серверную строку того же разговора.
fn server_message(id: Uuid, body: &str, own: bool, seconds: &str) -> HistoryMessage {
    HistoryMessage {
        id,
        stream_id: STREAM,
        topic_id: TOPIC,
        body: MessageBody::Markdown(body.into()),
        metadata: HistoryMetadata {
            author_id: Some(Uuid::from_u128(if own { 900 } else { 901 })),
            created_at: HistoryTime::parse(&format!("2026-11-18T10:00:{seconds}Z")).unwrap(),
            updated_at: None,
        },
    }
}

/// Возвращает текст каждой серверной строки до действий пользователя.
fn history() -> Vec<HistoryMessage> {
    vec![
        server_message(MESSAGE, "Первый источник", false, "00"),
        server_message(SECOND, "abc", true, "01"),
    ]
}

/// Открывает реальное контекстное меню и выбирает способ добавления цитаты.
fn choose(ui: &mut VisualTestContext, id: Uuid, action: &'static str) {
    let body = ui
        .debug_bounds(match id {
            MESSAGE => "history-message-00000000-0000-0000-0000-0000000007d1",
            SECOND => "history-message-00000000-0000-0000-0000-0000000007d3",
            _ => panic!("Нет тестовой строки {id}"),
        })
        .expect("сообщение");
    let position = point(body.left() + body.size.width / 2., body.bottom() - px(5.));
    ui.simulate_mouse_down(position, MouseButton::Right, Modifiers::none());
    ui.simulate_mouse_up(position, MouseButton::Right, Modifiers::none());
    ui.run_until_parked();
    click(ui, action);
}

/// Проверяет восстановление разных текстов, удаление соседней и общей кнопкой закрытия.
#[gpui::test]
fn reply_tabs_remove_individually_and_restore_normal_draft(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(
        &requests,
        Load::Open(ChatEntry::Latest),
        history(),
        vec![],
        ui,
    );
    allow(&requests, Ok(TOPIC), ui);

    paste(ui, "Первый черновик");
    let row = ui
        .debug_bounds("history-message-00000000-0000-0000-0000-0000000007d1")
        .unwrap();
    ui.simulate_mouse_move(row.center(), None, Modifiers::none());
    ui.run_until_parked();
    click(ui, "history-reply-00000000-0000-0000-0000-0000000007d1");
    assert_eq!(draft(&view, ui), "Первый черновик");
    assert!(ui.debug_bounds("composer-reply-preview").is_some());
    choose(ui, SECOND, "history-reply-add");
    assert!(draft(&view, ui).is_empty());
    paste(ui, "Второй черновик");
    click(ui, "composer-reply-tab-0");
    assert_eq!(draft(&view, ui), "Первый черновик");
    ui.update(|window, _| {
        window.focus_prev();
        window.focus_prev();
        window.focus_prev();
    });
    ui.run_until_parked();
    ui.simulate_keystrokes("enter");
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    ui.run_until_parked();
    assert_eq!(draft(&view, ui), "Второй черновик");
    assert_ne!(
        ui.update(|window, app| view.read(app).keyboard_focus(window, app)),
        Some("редактор")
    );
    assert!(requests.save.try_recv().is_err());
    ui.simulate_keystrokes("shift-left");
    ui.run_until_parked();
    click(ui, "conversation-send");
    let reordered = next(&requests.save, "снимок после перестановки");
    assert_eq!(
        reordered.input.markdown,
        format!(
            "[Вы](urn:quote:{SECOND})\n\nВторой черновик\n\n[Участник](urn:quote:{MESSAGE})\n\nПервый черновик"
        )
    );
    saved(reordered, None, ui);
    click(ui, "composer-reply-tab-1");
    click(ui, "composer-reply-remove-0");
    assert_eq!(draft(&view, ui), "Первый черновик");

    choose(ui, SECOND, "history-reply-add");
    paste(ui, "Последующая попытка");
    click(ui, "composer-reply-remove-1");
    assert_eq!(draft(&view, ui), "Первый черновик");
    choose(ui, SECOND, "history-reply-add");
    paste(ui, "Последний ответ");
    click(ui, "composer-reply-clear");
    assert_eq!(draft(&view, ui), "Первый черновик");

    choose(ui, SECOND, "history-reply-add");
    assert!(draft(&view, ui).is_empty());
    paste(ui, "Ответ поверх обычного черновика");
    click(ui, "composer-reply-clear");
    assert_eq!(draft(&view, ui), "Первый черновик");
    assert!(requests.save.try_recv().is_err());
}

/// Отправляет цитаты с конкретным фрагментом, сохраняет попытку при ошибке и при позднем успехе.
#[gpui::test]
fn reply_selection_serializes_immutable_payload_and_preserves_new_tabs(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(
        &requests,
        Load::Open(ChatEntry::Latest),
        history(),
        vec![],
        ui,
    );
    allow(&requests, Ok(TOPIC), ui);
    paste(ui, "Ответ А");
    choose(ui, MESSAGE, "history-reply-replace");

    let body = ui
        .debug_bounds("history-reply-body-00000000-0000-0000-0000-0000000007d3")
        .expect("тело сообщения");
    let from = point(body.left() + px(1.), body.center().y);
    let to = point(body.left() + body.size.width * 0.68, body.center().y);
    ui.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    ui.run_until_parked();
    ui.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    ui.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    ui.simulate_keystrokes("ctrl-l");
    ui.run_until_parked();
    assert!(ui.debug_bounds("composer-reply-tab-1").is_some());
    assert!(draft(&view, ui).is_empty());
    paste(ui, "Ответ Б");

    click(ui, "conversation-send");
    let first = next(&requests.save, "запись двух ответов");
    let expected = format!(
        "[Участник](urn:quote:{MESSAGE})\n\nОтвет А\n\n[Вы](urn:quote:{SECOND}?text=ab)\n\nОтвет Б"
    );
    assert_eq!(first.input.markdown, expected);
    assert_eq!(first.input.quote_sources.len(), 2);
    assert_eq!(first.input.quote_sources[1].fragment.as_deref(), Some("ab"));
    assert!(
        first
            .reply
            .send(ConversationReply {
                address: first.input.address,
                value: QueueWriteOutcome::QuoteUnavailable(quote::SourceCheckError::Changed),
            })
            .is_ok()
    );
    ui.run_until_parked();
    assert_eq!(draft(&view, ui), "Ответ Б");
    assert!(ui.debug_bounds("conversation-send-explanation").is_some());

    click(ui, "conversation-send");
    let retry = next(&requests.save, "повтор с той же разметкой");
    assert_eq!(retry.input.markdown, expected);
    click(ui, "composer-reply-remove-0");
    paste(ui, "Ответ Б (новая версия)");
    saved(retry, Some(OTHER), ui);
    assert_eq!(draft(&view, ui), "Ответ Б (новая версия)");
    let confirmed = server_message(OTHER, &expected, true, "02");
    let mut messages = history();
    messages.push(confirmed);
    publish(&requests, Load::Read(7), messages, vec![], ui);
    assert!(
        ui.debug_bounds("history-quote-00000000-0000-0000-0000-0000000007d4-0")
            .is_some()
    );
    assert!(
        ui.debug_bounds("history-quote-00000000-0000-0000-0000-0000000007d4-2")
            .is_some()
    );
    assert!(requests.save.try_recv().is_err());
}

/// Проверяет видимость всей пары вкладка и X в горизонтальном ряду.
fn assert_reply_tab_visible(ui: &mut VisualTestContext, tab: &'static str, remove: &'static str) {
    let bar = ui
        .debug_bounds("composer-reply-tabs")
        .expect("полоса вкладок");
    let tab = ui.debug_bounds(tab).expect("вкладка");
    let remove = ui.debug_bounds(remove).expect("удаление вкладки");
    assert!(
        tab.left() >= bar.left() - px(1.),
        "{tab:?} вне ряда {bar:?}"
    );
    assert!(
        remove.right() <= bar.right() + px(1.),
        "{remove:?} вне ряда {bar:?}"
    );
}

/// Оставляет видимой активную вкладку при сужении и раскрывает пару с фокусом по Tab.
#[gpui::test]
fn reply_tabs_reveal_focused_pair_after_resize(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    ui.simulate_resize(size(px(1040.), px(530.)));
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.heading.direct = true;
            view.heading.chat = Some("Очень длинное имя автора из проверенного каталога".into());
            cx.notify();
        })
    });
    publish(
        &requests,
        Load::Open(ChatEntry::Latest),
        history(),
        vec![],
        ui,
    );
    allow(&requests, Ok(TOPIC), ui);
    for _ in 0..4 {
        choose(ui, MESSAGE, "history-reply-add");
    }
    paste(ui, "Остаюсь в четвёртой вкладке");
    assert_reply_tab_visible(ui, "composer-reply-tab-3", "composer-reply-remove-3");
    ui.simulate_resize(size(px(310.), px(530.)));
    ui.run_until_parked();
    assert_reply_tab_visible(ui, "composer-reply-tab-3", "composer-reply-remove-3");

    ui.update(|window, _| {
        for _ in 0..9 {
            window.focus_prev();
        }
    });
    ui.run_until_parked();
    assert_reply_tab_visible(ui, "composer-reply-tab-0", "composer-reply-remove-0");

    ui.update(|window, _| {
        window.focus_next();
        window.focus_next();
    });
    ui.run_until_parked();
    assert_reply_tab_visible(ui, "composer-reply-tab-1", "composer-reply-remove-1");
    assert_eq!(draft(&view, ui), "Остаюсь в четвёртой вкладке");
    assert!(requests.save.try_recv().is_err());
}

/// Не оставляет старое выделение, которое перебивало бы Ctrl+L по другой строке.
#[gpui::test]
fn disappeared_selection_allows_shortcut_on_current_history_row(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(
        &requests,
        Load::Open(ChatEntry::Latest),
        history(),
        vec![],
        ui,
    );
    allow(&requests, Ok(TOPIC), ui);
    let body = ui
        .debug_bounds("history-reply-body-00000000-0000-0000-0000-0000000007d3")
        .unwrap();
    let from = point(body.left() + px(1.), body.center().y);
    let to = point(body.left() + body.size.width * 0.68, body.center().y);
    ui.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    ui.run_until_parked();
    ui.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    ui.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());

    // Меню остаётся открыто, пока рабочий снимок не перестанет содержать его строку.
    ui.simulate_mouse_down(body.center(), MouseButton::Right, Modifiers::none());
    ui.simulate_mouse_up(body.center(), MouseButton::Right, Modifiers::none());
    ui.run_until_parked();
    assert!(ui.debug_bounds("history-reply-add").is_some());
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            let address = view.address().clone();
            view.apply_sync(
                &address,
                SyncUpdate {
                    context: address.route.owner.clone(),
                    generation: address.generation,
                    change: SyncChange::HistoryMessageChanged { message_id: SECOND },
                },
                cx,
            );
        })
    });
    ui.run_until_parked();
    publish(
        &requests,
        Load::Read(7),
        vec![history().remove(0)],
        vec![],
        ui,
    );

    let visible = ui
        .debug_bounds("history-reply-body-00000000-0000-0000-0000-0000000007d1")
        .unwrap();
    ui.simulate_mouse_move(visible.center(), None, Modifiers::none());
    ui.simulate_keystrokes("ctrl-l");
    ui.run_until_parked();
    paste(ui, "Отвечаю на видимую строку");
    click(ui, "conversation-send");
    let queued = next(&requests.save, "новый ответ после удаления источника");
    assert_eq!(queued.input.quote_sources.len(), 1);
    assert_eq!(queued.input.quote_sources[0].message_id, MESSAGE);
    assert!(queued.input.quote_sources[0].fragment.is_none());
    saved(queued, None, ui);
}

/// Показывает цитату из другого клиента даже при отсутствии исходной строки в окне.
#[gpui::test]
fn incoming_quote_projects_only_recognized_references(cx: &mut TestAppContext) {
    let (_view, ui, requests) = start(cx);
    let link = quote::reference(OTHER, "Внешний автор", Some("Ф"));
    let literal = quote::reference(SECOND, "Автор", None);
    publish(
        &requests,
        Load::Open(ChatEntry::Latest),
        vec![
            server_message(MESSAGE, &format!("```\n{literal}\n```"), false, "00"),
            server_message(SECOND, &format!("{link}\n\nОк"), true, "01"),
        ],
        vec![],
        ui,
    );
    allow(&requests, Ok(TOPIC), ui);
    assert!(
        ui.debug_bounds("history-quote-00000000-0000-0000-0000-0000000007d1-0")
            .is_none()
    );
    assert!(
        ui.debug_bounds("history-quote-00000000-0000-0000-0000-0000000007d3-0")
            .is_some()
    );
    let heading = ui
        .debug_bounds("quote-frame-heading")
        .expect("автор цитаты");
    assert!(heading.size.width >= px(160.), "{heading:?}");
    ui.simulate_resize(size(px(270.), px(470.)));
    ui.run_until_parked();
    let quote = ui
        .debug_bounds("history-quote-00000000-0000-0000-0000-0000000007d3-0")
        .unwrap();
    let heading = ui.debug_bounds("quote-frame-heading").unwrap();
    assert!(heading.size.width >= px(100.), "{heading:?}");
    assert!(
        quote.right() <= px(270.) && heading.right() <= quote.right(),
        "{quote:?} {heading:?}"
    );
}
