//! Проверяет ранний запрос страницы и геометрию первого кадра после изменения истории или бабла.

use gpui::{Entity, ListOffset, Modifiers, ScrollDelta, TestAppContext, VisualTestContext, point};

use super::*;

/// Прокручивает список через событие платформы GPUI.
fn wheel(ui: &mut VisualTestContext, delta: f32) {
    ui.simulate_event(ScrollWheelEvent {
        position: point(px(300.), px(220.)),
        delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
        ..Default::default()
    });
    ui.run_until_parked();
}

/// Выбирает стартовую сцену штатной кнопкой окна.
fn select(ui: &mut VisualTestContext, id: &'static str) {
    let button = ui.debug_bounds(id).expect("Кнопка сцены видна");
    ui.simulate_click(button.center(), Modifiers::none());
    ui.run_until_parked();
}

/// Проверяет запас до обоих краёв, один запрос и сохранение опоры на момент ответа.
#[gpui::test]
fn prefetch_keeps_the_current_anchor_in_both_directions(cx: &mut TestAppContext) {
    let clock = cx.background_executor.clone();
    let (view, ui) = cx.add_window_view(|_, _| HistoryLab::new());
    ui.simulate_resize(size(px(760.), px(560.)));
    ui.run_until_parked();

    for (scene, direction, delta) in [
        ("history-older", HistoryDirection::Older, 110.),
        ("history-newer", HistoryDirection::Newer, -110.),
    ] {
        select(ui, scene);
        assert!(ui.read(|app| view.read(app).flight.is_none()));
        let flight = (0..24).find_map(|_| {
            wheel(ui, delta);
            ui.read(|app| view.read(app).flight)
        });
        let flight = flight.expect("Прокрутка должна запустить догрузку");
        assert_eq!(flight.need.direction, direction);
        assert!(
            flight.need.distance > px(100.),
            "Запрос слишком поздний: {:?}",
            flight.need
        );
        let height = ui.read(|app| view.read(app).viewport.list().viewport_bounds().size.height);
        assert!(flight.need.distance <= height);
        let before_gesture = ui.read(|app| view.read(app).viewport.tested_reading_anchor());
        wheel(ui, delta / 2.);
        let (anchor, before) = ui.read(|app| {
            let lab = view.read(app);
            assert_eq!(lab.history.messages.len(), 600);
            assert_eq!(lab.flight.unwrap().id, flight.id, "Дублирующий запрос");
            lab.viewport.tested_reading_anchor().expect("Опора видна")
        });
        assert_ne!(
            before_gesture,
            Some((anchor, before)),
            "Прокрутка принята во время запроса"
        );

        clock.advance_clock(PAGE_DELAY);
        ui.run_until_parked();
        let after = ui.read(|app| {
            let lab = view.read(app);
            assert_eq!(lab.history.messages.len(), 650);
            assert_eq!(lab.last_page.unwrap().id, flight.id);
            lab.viewport.y(anchor).expect("То же сообщение в кадре")
        });
        eprintln!(
            "history_lab op_id={} event=anchor_measured anchor_uuid={anchor} y_before_px={before} y_after_px={after} delta_px={}",
            flight.id,
            after - before,
        );
        assert!(
            (after - before).abs() <= 1.,
            "Опора {anchor}: {before} → {after} px"
        );
    }
}

/// Проверяет автозапуск без жеста, нижнюю привязку и остановку после заполнения окна.
#[gpui::test]
fn fills_short_tail_upwards_and_stops(cx: &mut TestAppContext) {
    let clock = cx.background_executor.clone();
    let (view, ui) = cx.add_window_view(|_, _| HistoryLab::new());
    ui.simulate_resize(size(px(760.), px(560.)));
    ui.run_until_parked();
    select(ui, "history-short");

    let id = Uuid::from_u128(HISTORY_SIZE);
    let before = ui.read(|app| {
        let lab = view.read(app);
        assert_eq!(lab.history.messages.len(), 3);
        let flight = lab.flight.expect("Автодогрузка короткого окна");
        assert_eq!(flight.need.direction, HistoryDirection::Older);
        assert_eq!(flight.need.distance, px(0.));
        lab.viewport.y(id).expect("Хвост виден")
    });
    clock.advance_clock(PAGE_DELAY);
    ui.run_until_parked();
    ui.read(|app| {
        let lab = view.read(app);
        assert_eq!(lab.history.messages.len(), 53);
        assert!(lab.flight.is_none(), "Заполненное окно не требует страниц");
        assert!(lab.viewport.at_tail());
        let after = lab.viewport.y(id).unwrap();
        assert!((after - before).abs() <= 1., "Хвост: {before} → {after} px");
    });
}

/// Рисует именно первый кадр после изменения, пока отложенные задачи ещё не выполнились.
fn first_resize_frame<T>(
    ui: &mut VisualTestContext,
    view: &Entity<HistoryLab>,
    id: Uuid,
    sample: impl FnOnce(&HistoryLab) -> T,
) -> T {
    ui.update(|window, app| {
        view.update(app, |lab, cx| lab.toggle_bubble(id, cx));
        window.refresh();
        window.draw(app).clear();
        sample(view.read(app))
    })
}

/// Ставит верх списка внутрь нужной строки. Вызывается только при подготовке сцены.
fn scroll_to(
    ui: &mut VisualTestContext,
    view: &Entity<HistoryLab>,
    item_ix: usize,
    offset: Pixels,
) {
    ui.update(|_, app| {
        view.update(app, |lab, cx| {
            lab.viewport.list().scroll_to(ListOffset {
                item_ix,
                offset_in_item: offset,
            });
            cx.notify();
        });
    });
    ui.run_until_parked();
}

/// Сверяет фактическую координату с допуском в один логический пиксель.
fn same_position(before: Pixels, after: Pixels, step: &str) {
    assert!(
        (after - before).abs() <= px(1.),
        "{step}: {before:?} → {after:?}"
    );
}

/// Проверяет изменение высоты и опору в первом и установившемся кадрах каждого перехода.
fn check_resize(
    ui: &mut VisualTestContext,
    view: &Entity<HistoryLab>,
    id: Uuid,
    step: &str,
    sample: impl Fn(&HistoryLab) -> (Pixels, Pixels),
) {
    let (before, short) = ui.read(|app| sample(view.read(app)));

    let (grown, tall) = first_resize_frame(ui, view, id, &sample);
    same_position(PREVIEW_HEIGHT, tall - short, "рост бабла");
    same_position(before, grown, step);
    ui.run_until_parked();
    same_position(before, ui.read(|app| sample(view.read(app)).0), step);

    let (shrunk, height) = first_resize_frame(ui, view, id, &sample);
    same_position(short, height, "высота после сжатия");
    same_position(before, shrunk, step);
    ui.run_until_parked();
    same_position(before, ui.read(|app| sample(view.read(app)).0), step);
    eprintln!(
        "history_bubble message={id} target={step} first_frame_growth_delta_px={} first_frame_shrink_delta_px={} height_change_px={}",
        f32::from(grown - before),
        f32::from(shrunk - before),
        f32::from(tall - short)
    );
}

/// Проверяет рост и сжатие баблов выше, ниже и на частично видимой опоре.
#[gpui::test]
fn resized_bubbles_keep_reading_anchor_on_first_frame(cx: &mut TestAppContext) {
    let (view, ui) = cx.add_window_view(|_, _| HistoryLab::new());
    ui.simulate_resize(size(px(760.), px(560.)));
    ui.run_until_parked();

    let anchor = Uuid::from_u128(FIRST + 22);
    let above = Uuid::from_u128(FIRST + 21);
    scroll_to(ui, &view, 21, px(0.));
    assert!(ui.read(|app| view.read(app).viewport.tested_bounds(above).is_some()));
    scroll_to(ui, &view, 22, px(17.));
    assert!(ui.read(|app| view.read(app).viewport.y(anchor).unwrap()) < 0.);

    for (target, step) in [
        (BubbleTarget::Above, "выше"),
        (BubbleTarget::Anchor, "опора"),
        (BubbleTarget::Below, "ниже"),
    ] {
        let id = ui.read(|app| view.read(app).bubble_id(target).unwrap());
        let sample = |lab: &HistoryLab| {
            assert_eq!(lab.viewport.saved_anchor().unwrap().id, anchor);
            let y = lab.viewport.tested_bounds(anchor).unwrap().top();
            let height = if id == above {
                // Строка над областью уже измерялась. Суммарная высота доказывает,
                // что её пересчитали, хотя список больше не рисует сам бабл.
                lab.viewport.list().max_offset_for_scrollbar().height
            } else {
                lab.viewport.tested_bounds(id).unwrap().size.height
            };
            (y, height)
        };
        check_resize(ui, &view, id, step, sample);
    }
}

/// Проверяет нижнюю привязку, пока растёт и сжимается последнее или предпоследнее сообщение.
#[gpui::test]
fn resized_bubbles_keep_tail_on_first_frame(cx: &mut TestAppContext) {
    let clock = cx.background_executor.clone();
    let (view, ui) = cx.add_window_view(|_, _| HistoryLab::new());
    ui.simulate_resize(size(px(760.), px(560.)));
    ui.run_until_parked();
    select(ui, "history-short");
    clock.advance_clock(PAGE_DELAY);
    ui.run_until_parked();

    let last = Uuid::from_u128(HISTORY_SIZE);
    for (id, step) in [
        (Uuid::from_u128(HISTORY_SIZE - 1), "выше хвоста"),
        (last, "хвост"),
    ] {
        let sample = |lab: &HistoryLab| {
            assert!(lab.viewport.at_tail());
            let bottom = lab.viewport.list().viewport_bounds().bottom();
            let gap = bottom - lab.viewport.tested_bounds(last).unwrap().bottom();
            same_position(px(12.), gap, "нижний отступ списка");
            (gap, lab.viewport.tested_bounds(id).unwrap().size.height)
        };
        check_resize(ui, &view, id, step, sample);
    }
}
