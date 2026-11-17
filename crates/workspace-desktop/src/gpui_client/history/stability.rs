//! Воспроизводит нагрузку на рабочую историю и редактор. Проверяет опору и записывает время кадров.

use std::time::Instant;

use super::*;

#[path = "stability_data.rs"]
mod data;
use data::*;

/// Читает фактическую координату той же опорной строки из готового кадра.
fn anchor_y(view: &Entity<Conversation>, ui: &VisualTestContext, step: &str) -> f32 {
    ui.read(|app| {
        view.read(app)
            .viewport
            .y(ANCHOR)
            .unwrap_or_else(|| panic!("Опора {ANCHOR} отсутствует после {step}"))
    })
}

/// Связывает измерение с идентификатором рабочего запроса, пока ответ ещё удерживается тестом.
fn request_id(view: &Entity<Conversation>, ui: &VisualTestContext, step: &str) -> Uuid {
    ui.read(|app| {
        view.read(app)
            .active
            .unwrap_or_else(|| panic!("Нет запроса для {step}"))
            .id
    })
}

/// Записывает координаты и проверяет допуск для чтения после изменения списка или редактора.
fn verify_anchor(
    view: &Entity<Conversation>,
    ui: &VisualTestContext,
    step: &str,
    op_id: Uuid,
    before: f32,
) {
    let after = anchor_y(view, ui, step);
    let rows = ui.read(|app| view.read(app).viewport.list().item_count());
    let delta = after - before;
    eprintln!(
        "ui11 op_id={op_id} event=anchor_measured step={step} mode=reading anchor_uuid={ANCHOR} rows_after={rows} y_before_px={before} y_after_px={after} delta_px={delta}"
    );
    assert!(
        delta.abs() <= TOLERANCE_PX,
        "Опора после {step}: {before} → {after} px"
    );
}

/// Измеряет CPU-подготовку одного кадра, принудительно обновляя корневой вид.
fn frame_us(ui: &mut VisualTestContext, step: &str, op_id: Uuid) -> u128 {
    let elapsed = ui.update(|window, app| {
        window.refresh();
        let start = Instant::now();
        window.draw(app).clear();
        start.elapsed().as_micros()
    });
    eprintln!("ui11 op_id={op_id} event=frame_measured step={step} cpu_draw_us={elapsed}");
    elapsed
}

/// Подаёт группу клавиш в редактор через GPUI и запоминает ожидаемое продолжение черновика.
fn type_burst(
    ui: &mut VisualTestContext,
    expected: &mut String,
    first: usize,
    end: usize,
    max_dispatch_us: &mut u128,
) {
    for index in first..end {
        let key = char::from(b'a' + (index % 26) as u8);
        expected.push(key);
        let keystroke = Keystroke::parse(&key.to_string()).unwrap();
        let started = Instant::now();
        let handled = ui.update(|window, app| window.dispatch_keystroke(keystroke, app));
        *max_dispatch_us = (*max_dispatch_us).max(started.elapsed().as_micros());
        assert!(handled, "GPUI не обработал клавишу {index}");
    }
    ui.run_until_parked();
}

/// Проверяет прокрутку с клавиатуры, догрузку старого края и возврат к хвосту.
#[gpui::test]
fn history_keys_scroll_and_load_older_page(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    ui.simulate_resize(size(px(340.), px(530.)));
    let scope = ui.read(|app| view.read(app).address().route.scope);
    publish_with_older(
        &requests,
        Load::Open(ChatEntry::Latest),
        fixture_rows(scope, PAGED_ROWS + 1, LAST_ROW),
        vec![],
        HistoryEdge::More("older".into()),
        ui,
    );
    allow(&requests, Ok(TOPIC), ui);
    ui.update(|window, app| window.focus(&view.read(app).history_focus));
    ui.run_until_parked();
    assert!(ui.read(|app| view.read(app).viewport.at_tail()));

    ui.simulate_keystrokes("pageup");
    ui.run_until_parked();
    let page = ui.read(|app| view.read(app).viewport.list().logical_scroll_top());
    assert!(
        page.item_ix > 0 && page.item_ix < (LAST_ROW - PAGED_ROWS) as usize,
        "Ожидался верх страницы, получен {page:?}"
    );
    assert!(!ui.read(|app| view.read(app).viewport.at_tail()));
    ui.simulate_keystrokes("down");
    ui.run_until_parked();
    let after = ui.read(|app| view.read(app).viewport.list().logical_scroll_top());
    assert!(after.item_ix > page.item_ix || after.offset_in_item > page.offset_in_item);

    ui.simulate_keystrokes("home");
    ui.run_until_parked();
    assert_eq!(
        ui.read(|app| view.read(app).viewport.list().logical_scroll_top().item_ix),
        0
    );
    ui.simulate_keystrokes("up");
    publish(
        &requests,
        Load::Extend(7, HistoryDirection::Older),
        fixture_rows(scope, 1, LAST_ROW),
        vec![],
        ui,
    );
    ui.simulate_keystrokes("end");
    ui.run_until_parked();
    assert!(ui.read(|app| view.read(app).viewport.at_tail()));
    assert!(ui.update(|window, app| view.read(app).history_focus.is_focused(window)));
}

/// Проверяет чтение длинной истории во время ввода, догрузки, изменения ширины и доставки.
#[gpui::test]
fn long_history_keeps_reading_position_and_input(cx: &mut TestAppContext) {
    let run = Uuid::new_v4();
    eprintln!(
        "ui11 op_id={run} event=run_started wide_px=1061 narrow_px=340 height_px=530 initial_rows={} page_rows={PAGED_ROWS} typing_events={INPUT_COUNT}",
        LAST_ROW - PAGED_ROWS
    );
    let (view, ui, requests) = start(cx);
    ui.simulate_resize(size(px(1061.), px(530.)));
    ui.run_until_parked();
    let scope = ui.read(|app| view.read(app).address().route.scope);
    publish_with_older(
        &requests,
        Load::Open(ChatEntry::Latest),
        fixture_rows(scope, PAGED_ROWS + 1, LAST_ROW),
        vec![],
        HistoryEdge::More("older".into()),
        ui,
    );
    allow(&requests, Ok(TOPIC), ui);

    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.viewport.list().scroll_to(ListOffset {
                item_ix: (ANCHOR.as_u128() - PAGED_ROWS - 1) as usize,
                offset_in_item: px(11.),
            });
            cx.notify();
        })
    });
    ui.run_until_parked();
    let before = anchor_y(&view, ui, "open_anchor");
    assert!(
        before.abs() <= 12.,
        "Ожидалась строка у верха истории, получено {before} px"
    );
    let mut frames = vec![frame_us(ui, "open_anchor", run)];
    let mut typed = String::new();
    let mut max_dispatch_us = 0;

    click(ui, "history-older");
    let prepend = request_id(&view, ui, "prepend");
    click(ui, "workspace-composer-editor");
    type_burst(ui, &mut typed, 0, 16, &mut max_dispatch_us);
    let mut messages = fixture_rows(scope, 1, LAST_ROW);
    publish(
        &requests,
        Load::Extend(7, HistoryDirection::Older),
        messages.clone(),
        vec![],
        ui,
    );
    verify_anchor(&view, ui, "prepend", prepend, before);
    frames.push(frame_us(ui, "prepend", prepend));

    let resize = Uuid::new_v4();
    ui.simulate_resize(size(px(340.), px(530.)));
    ui.run_until_parked();
    verify_anchor(&view, ui, "resize", resize, before);
    frames.push(frame_us(ui, "resize", resize));

    let grow_editor = Uuid::new_v4();
    let editor_height = ui
        .debug_bounds("workspace-composer-editor")
        .unwrap()
        .size
        .height;
    for _ in 0..5 {
        ui.simulate_keystrokes("shift-enter");
        typed.push('\n');
    }
    let grown_height = ui
        .debug_bounds("workspace-composer-editor")
        .unwrap()
        .size
        .height;
    assert_eq!(grown_height, px(126.));
    assert!(grown_height > editor_height);
    verify_anchor(&view, ui, "grow_editor", grow_editor, before);
    frames.push(frame_us(ui, "grow_editor", grow_editor));

    changed(
        &view,
        ui,
        SyncChange::HistoryMessageChanged {
            message_id: GROWING_ROW,
        },
    );
    let image = request_id(&view, ui, "late_height");
    type_burst(ui, &mut typed, 16, 32, &mut max_dispatch_us);
    grow_above(&mut messages);
    publish(&requests, Load::Read(7), messages.clone(), vec![], ui);
    verify_anchor(&view, ui, "late_height", image, before);
    frames.push(frame_us(ui, "late_height", image));

    let added = Uuid::from_u128(LAST_ROW + 1);
    changed(
        &view,
        ui,
        SyncChange::HistoryMessageCreated {
            scope,
            message_id: added,
        },
    );
    let incoming = request_id(&view, ui, "incoming_reading");
    type_burst(ui, &mut typed, 32, 48, &mut max_dispatch_us);
    messages.extend(fixture_rows(scope, LAST_ROW + 1, LAST_ROW + 1));
    publish(&requests, Load::Read(7), messages.clone(), vec![], ui);
    verify_anchor(&view, ui, "incoming_reading", incoming, before);
    frames.push(frame_us(ui, "incoming_reading", incoming));

    click(ui, "history-latest");
    ui.read(|app| {
        let viewport = &view.read(app).viewport;
        assert!(viewport.at_tail());
        assert!(viewport.y(added).is_some());
    });
    click(ui, "workspace-composer-editor");
    ui.simulate_keystrokes("cmd-a right");
    let new_tail = Uuid::from_u128(LAST_ROW + 2);
    changed(
        &view,
        ui,
        SyncChange::HistoryMessageCreated {
            scope,
            message_id: new_tail,
        },
    );
    let follow = request_id(&view, ui, "incoming_tail");
    type_burst(ui, &mut typed, 48, INPUT_COUNT, &mut max_dispatch_us);
    messages.extend(fixture_rows(scope, LAST_ROW + 2, LAST_ROW + 2));
    publish(&requests, Load::Read(7), messages, vec![], ui);
    let last = ui
        .debug_bounds("history-message-00000000-0000-0000-0000-00000000028c")
        .expect("Новый хвост виден");
    let gap = ui.read(|app| {
        let viewport = &view.read(app).viewport;
        let list = viewport.list();
        assert!(viewport.at_tail());
        assert!(viewport.y(new_tail).is_some());
        // Последний отступ 12 px задаётся List::pb_3() и не входит в высоту сообщения.
        f32::from(list.viewport_bounds().bottom() - last.bottom()) - 12.
    });
    eprintln!(
        "ui11 op_id={follow} event=anchor_measured mode=follow_end previous_uuid={added} anchor_uuid={new_tail} bottom_gap_px={gap}"
    );
    assert!(gap.abs() <= TOLERANCE_PX, "Отступ у нового хвоста {gap} px");
    frames.push(frame_us(ui, "incoming_tail", follow));

    assert!(
        draft(&view, ui) == typed,
        "Поток ввода потерял или переставил событие"
    );
    assert!(requests.save.try_recv().is_err());
    frames.sort_unstable();
    eprintln!(
        "ui11 op_id={run} event=run_finished result=ok typed_keys={INPUT_COUNT} newline_keys=5 content_matches=true max_dispatch_us={max_dispatch_us} cpu_draw_samples={} cpu_draw_median_us={} cpu_draw_max_us={}",
        frames.len(),
        frames[frames.len() / 2],
        frames.last().unwrap()
    );
}
