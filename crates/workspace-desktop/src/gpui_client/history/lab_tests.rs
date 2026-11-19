//! Проверяет ранний запрос страницы и геометрию рабочего списка с задержанным ответом.

use gpui::{Modifiers, ScrollDelta, TestAppContext, VisualTestContext, point};

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
