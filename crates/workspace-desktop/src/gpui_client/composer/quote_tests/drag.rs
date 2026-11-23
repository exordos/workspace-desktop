//! Проверяет перетаскивание вкладок ответа настоящими событиями указателя GPUI.

use std::time::Duration;

use gpui::{Pixels, Point, ScrollDelta, ScrollWheelEvent, TouchPhase};

use super::*;

const BAR: &str = "composer-reply-tabs";
const INDICATOR: &str = "composer-reply-drop-indicator";
const TABS: [&str; 4] = [
    "composer-reply-tab-0",
    "composer-reply-tab-1",
    "composer-reply-tab-2",
    "composer-reply-tab-3",
];
const REMOVE: [&str; 3] = [
    "composer-reply-remove-0",
    "composer-reply-remove-1",
    "composer-reply-remove-2",
];

/// Заполняет начало разговора, чтобы вкладки открывались из штатного меню истории.
fn open(cx: &mut TestAppContext) -> (Entity<Conversation>, &mut VisualTestContext, Requests) {
    let (view, ui, requests) = start(cx);
    publish(
        &requests,
        Load::Open(ChatEntry::Latest),
        history(),
        vec![],
        ui,
    );
    allow(&requests, Ok(TOPIC), ui);
    (view, ui, requests)
}

/// Начинает движение из одной точки и переводит указатель в другую, удерживая левую кнопку.
fn hold_drag(ui: &mut VisualTestContext, from: Point<Pixels>, to: Point<Pixels>) {
    ui.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    ui.run_until_parked();
    move_drag(ui, to);
    move_drag(ui, to);
}

/// Переносит указатель во время удержания вкладки.
fn move_drag(ui: &mut VisualTestContext, to: Point<Pixels>) {
    ui.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    ui.run_until_parked();
}

/// Отпускает левую кнопку и дожидается перерисовки.
fn release(ui: &mut VisualTestContext, at: Point<Pixels>) {
    ui.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    ui.run_until_parked();
}

/// Переносит соседнюю и активную вкладки и проверяет порядок ответов в очереди.
#[gpui::test]
fn reply_drag_reorders_both_directions_without_selecting_source(cx: &mut TestAppContext) {
    let (view, ui, requests) = open(cx);
    for (id, answer) in [(MESSAGE, "Ввод A"), (SECOND, "Ввод Б"), (MESSAGE, "Ввод В")]
    {
        choose(ui, id, "history-reply-add");
        paste(ui, answer);
    }

    let from = ui.debug_bounds(TABS[0]).unwrap().center();
    let after_middle = ui.debug_bounds(REMOVE[1]).unwrap().center();
    hold_drag(ui, from, after_middle);
    assert_eq!(
        ui.debug_bounds(INDICATOR).unwrap().size,
        size(px(2.), px(32.))
    );
    assert_eq!(draft(&view, ui), "Ввод В");
    release(ui, after_middle);
    assert!(!ui.read(gpui::App::has_active_drag));
    assert_eq!(draft(&view, ui), "Ввод В");

    let from = ui.debug_bounds(TABS[2]).unwrap().center();
    let first = ui.debug_bounds(TABS[0]).unwrap();
    let before_first = point(first.left() + px(3.), first.center().y);
    hold_drag(ui, from, before_first);
    assert!(ui.debug_bounds(INDICATOR).unwrap().left() <= before_first.x);
    release(ui, before_first);
    assert_eq!(draft(&view, ui), "Ввод В");

    click(ui, "conversation-send");
    let reordered = next(&requests.save, "снимок после переноса вкладок");
    assert_eq!(
        reordered.input.markdown,
        format!(
            "[Участник](urn:quote:{MESSAGE})\n\nВвод В\n\n[Вы](urn:quote:{SECOND})\n\nВвод Б\n\n[Участник](urn:quote:{MESSAGE})\n\nВвод A"
        )
    );
    saved(reordered, None, ui);
}

/// Отмена за границей ряда и по Escape оставляет исходный порядок и следующий клик.
#[gpui::test]
fn reply_drag_cancels_outside_row_and_on_escape(cx: &mut TestAppContext) {
    let (view, ui, requests) = open(cx);
    choose(ui, MESSAGE, "history-reply-add");
    paste(ui, "Слева");
    choose(ui, SECOND, "history-reply-add");
    paste(ui, "Справа");

    let first = ui.debug_bounds(TABS[0]).unwrap().center();
    let after_second = ui.debug_bounds(REMOVE[1]).unwrap().center();
    let bar = ui.debug_bounds(BAR).unwrap();
    let outside = point(bar.right() + px(3.), bar.bottom() + px(6.));
    let close_first = ui.debug_bounds(REMOVE[0]).unwrap().center();
    hold_drag(ui, close_first, outside);
    assert!(!ui.read(gpui::App::has_active_drag));
    release(ui, outside);

    hold_drag(ui, first, after_second);
    assert!(ui.debug_bounds(INDICATOR).is_some());
    move_drag(ui, outside);
    release(ui, outside);
    assert!(!ui.read(gpui::App::has_active_drag));

    hold_drag(ui, first, after_second);
    assert!(ui.read(gpui::App::has_active_drag));
    ui.simulate_mouse_up(after_second, MouseButton::Right, Modifiers::none());
    ui.run_until_parked();
    assert!(!ui.read(gpui::App::has_active_drag));

    hold_drag(ui, first, after_second);
    assert!(ui.read(gpui::App::has_active_drag));
    ui.simulate_keystrokes("escape");
    ui.run_until_parked();
    assert!(!ui.read(gpui::App::has_active_drag));
    release(ui, after_second);
    assert_eq!(draft(&view, ui), "Справа");

    ui.simulate_keystrokes("enter");
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    ui.run_until_parked();
    assert_eq!(draft(&view, ui), "Слева");
    click(ui, REMOVE[1]);
    assert_eq!(draft(&view, ui), "Слева");
    assert!(requests.save.try_recv().is_err());
}

/// Прокручивает ряд тем же горизонтальным событием, что даёт трекпад.
fn scroll(ui: &mut VisualTestContext, position: Point<Pixels>, delta: Pixels) {
    ui.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Pixels(point(delta, px(0.))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    ui.run_until_parked();
}

/// Переносит вкладку в видимой части узкого ряда и отменяет сброс за его границей.
#[gpui::test]
fn reply_drag_respects_scrolled_viewport_and_keeps_visible_offset(cx: &mut TestAppContext) {
    let clock = cx.executor().clone();
    let (view, ui, requests) = open(cx);
    ui.simulate_resize(size(px(340.), px(650.)));
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.heading.direct = true;
            view.heading.chat = Some("Александра Тестовая с длинным именем".into());
            cx.notify();
        })
    });
    ui.run_until_parked();
    for (id, answer) in [
        (MESSAGE, "Первый"),
        (SECOND, "Второй"),
        (MESSAGE, "Третий"),
        (MESSAGE, "Четвёртый"),
    ] {
        choose(ui, id, "history-reply-add");
        paste(ui, answer);
    }

    let bar = ui.debug_bounds(BAR).unwrap();
    scroll(ui, bar.center(), px(2000.));
    let first = ui.debug_bounds(TABS[0]).unwrap();
    let second = ui.debug_bounds(TABS[1]).unwrap();
    let tail = ui.debug_bounds(TABS[3]).unwrap();
    assert!(first.center().x >= bar.left() && second.center().x < bar.right());
    assert!(tail.left() >= bar.right());

    let before_first = point(first.left() + px(3.), first.center().y);
    hold_drag(ui, second.center(), before_first);
    release(ui, before_first);
    assert_eq!(ui.debug_bounds(TABS[3]).unwrap().left(), tail.left());
    assert_eq!(
        ui.debug_bounds(TABS[0]).unwrap().size.width,
        second.size.width
    );
    assert_eq!(draft(&view, ui), "Четвёртый");

    let new_first = ui.debug_bounds(TABS[0]).unwrap();
    let right = point(bar.right() - px(4.), bar.center().y);
    hold_drag(ui, new_first.center(), right);
    let advance = |ui: &mut VisualTestContext| {
        for _ in 0..50 {
            clock.advance_clock(Duration::from_millis(17));
            ui.run_until_parked();
        }
    };
    advance(ui);
    let after_scroll = ui.debug_bounds(TABS[3]).unwrap().right();
    assert!(after_scroll <= bar.right() + px(1.));
    release(ui, right);
    let new_last = ui.debug_bounds(TABS[3]).unwrap();
    assert_eq!(new_last.size.width, second.size.width);
    assert!(new_last.right() <= bar.right());

    hold_drag(
        ui,
        new_last.center(),
        point(bar.left() + px(4.), bar.center().y),
    );
    advance(ui);
    assert_eq!(ui.debug_bounds(TABS[0]).unwrap().left(), first.left());

    let outside = point(bar.right() + px(1.), bar.center().y);
    move_drag(ui, outside);
    advance(ui);
    assert_eq!(ui.debug_bounds(TABS[0]).unwrap().left(), first.left());
    release(ui, outside);
    assert!(!ui.read(gpui::App::has_active_drag));
    assert_eq!(draft(&view, ui), "Четвёртый");

    click(ui, "conversation-send");
    let reordered = next(&requests.save, "порядок ответов после прокрутки и отмены");
    assert_eq!(
        reordered.input.markdown,
        format!(
            "[Александра Тестовая с длинным именем](urn:quote:{MESSAGE})\n\nПервый\n\n[Александра Тестовая с длинным именем](urn:quote:{MESSAGE})\n\nТретий\n\n[Александра Тестовая с длинным именем](urn:quote:{MESSAGE})\n\nЧетвёртый\n\n[Вы](urn:quote:{SECOND})\n\nВторой"
        )
    );
    saved(reordered, None, ui);
}

/// Переставляет уже открытые ответы, когда проверка отправки требует повтора.
#[gpui::test]
fn reply_drag_and_keyboard_reorder_after_destination_failure(cx: &mut TestAppContext) {
    let (view, ui, requests) = open(cx);
    choose(ui, MESSAGE, "history-reply-add");
    paste(ui, "Ответ А");
    choose(ui, SECOND, "history-reply-add");
    paste(ui, "Ответ Б");

    click(ui, "conversation-send");
    let attempt = next(&requests.save, "попытка до сбоя проверки");
    assert!(
        attempt
            .reply
            .send(ConversationReply {
                address: attempt.input.address,
                value: QueueWriteOutcome::DestinationUnavailable(DestinationError::TopicChanged),
            })
            .is_ok()
    );
    ui.run_until_parked();
    assert!(ui.debug_bounds("conversation-send-retry").is_some());
    assert!(ui.update(|window, app| {
        let view = view.read(app);
        let (history, editor) = view.history_and_editor_focus(app);
        editor.is_none() && history.is_focused(window)
    }));

    click(ui, "conversation-send-retry");
    allow(&requests, Ok(TOPIC), ui);
    assert_eq!(draft(&view, ui), "Ответ Б");

    let first_width = ui.debug_bounds(TABS[0]).unwrap().size.width;
    let second_width = ui.debug_bounds(TABS[1]).unwrap().size.width;
    assert_ne!(first_width, second_width);
    click(ui, TABS[1]);
    ui.simulate_keystrokes("shift-left");
    ui.run_until_parked();
    assert_eq!(ui.debug_bounds(TABS[0]).unwrap().size.width, second_width);

    let first = ui.debug_bounds(TABS[0]).unwrap();
    let before_first = point(first.left() + px(3.), first.center().y);
    let from = ui.debug_bounds(TABS[1]).unwrap().center();
    hold_drag(ui, from, before_first);
    assert!(ui.read(gpui::App::has_active_drag));
    release(ui, before_first);
    assert_eq!(ui.debug_bounds(TABS[0]).unwrap().size.width, first_width);
    assert_eq!(draft(&view, ui), "Ответ Б");

    click(ui, "conversation-send");
    let reordered = next(&requests.save, "повтор после восстановления цели");
    assert_eq!(
        reordered.input.markdown,
        format!(
            "[Участник](urn:quote:{MESSAGE})\n\nОтвет А\n\n[Вы](urn:quote:{SECOND})\n\nОтвет Б"
        )
    );
    saved(reordered, None, ui);
}
