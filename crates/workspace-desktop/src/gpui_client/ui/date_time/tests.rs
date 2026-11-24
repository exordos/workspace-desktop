//! Проверяет округление границ и изменение значений через обычные действия в окне.

use chrono::NaiveDate;

use super::TimePrecision;

/// Проверяет, что после последней минуты нижняя граница переходит на завтра.
#[test]
fn minute_precision_rounds_lower_bound_into_next_day() {
    let day = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    let time = day.and_hms_opt(23, 59, 5).unwrap();
    assert_eq!(
        TimePrecision::Minutes.lower_bound(time),
        day.succ_opt().unwrap().and_hms_opt(0, 0, 0)
    );
    assert_eq!(TimePrecision::Seconds.lower_bound(time), Some(time));
}

#[cfg(feature = "gpui-test")]
mod interactions {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, point, px, size};

    use super::*;
    use crate::gpui_client::ui::{
        DatePicker, DatePickerChanged, DateTimePicker, DateTimePickerChanged, PickerOptions,
        TimePicker, TimePickerChanged,
    };

    /// Даёт календарный день для тестов перехода через февраль.
    fn february(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 2, day).expect("день февраля 2024 года")
    }

    /// Проверяет, что мышь меняет день, а клавиатура может перейти через границу месяца.
    #[gpui::test]
    fn date_picker_navigates_without_committing_until_selection(cx: &mut TestAppContext) {
        let start = february(28);
        let (picker, ui) = cx.add_window_view(|_, cx| {
            DatePicker::new(Some(start), start, PickerOptions::default(), cx)
        });
        let changes = Rc::new(RefCell::new(Vec::new()));
        let received = Rc::clone(&changes);
        ui.update(|_, app| {
            app.subscribe(&picker, move |_, change: &DatePickerChanged, _| {
                received.borrow_mut().push(change.0);
            })
            .detach();
        });

        let leap_day = ui.debug_bounds("date-picker-day-2024-02-29").unwrap();
        ui.simulate_click(leap_day.center(), Modifiers::none());
        ui.simulate_keystrokes("right");
        ui.run_until_parked();
        assert_eq!(&*changes.borrow(), &[february(29)]);
        assert!(ui.debug_bounds("date-picker-day-2024-03-25").is_some());

        ui.simulate_keystrokes("enter");
        let march = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        ui.read(|app| assert_eq!(picker.read(app).value(), Some(march)));
        assert_eq!(&*changes.borrow(), &[february(29), march]);
    }

    /// Проверяет ввод секунд, шаг кнопкой и колесом и отказ принять 60 секунд.
    #[gpui::test]
    fn time_picker_commits_only_valid_segments(cx: &mut TestAppContext) {
        let start = february(28).and_hms_opt(10, 58, 12).unwrap().time();
        let (picker, ui) = cx.add_window_view(|window, cx| {
            TimePicker::new(
                Some(start),
                TimePrecision::Seconds,
                true,
                PickerOptions::default(),
                window,
                cx,
            )
        });
        let changes = Rc::new(RefCell::new(Vec::new()));
        let received = Rc::clone(&changes);
        ui.update(|_, app| {
            app.subscribe(&picker, move |_, change: &TimePickerChanged, _| {
                received.borrow_mut().push(change.0);
            })
            .detach();
        });

        let seconds = ui.debug_bounds("time-picker-second").unwrap();
        ui.simulate_click(seconds.center(), Modifiers::none());
        ui.simulate_keystrokes("6 0");
        assert!(changes.borrow().is_empty());

        ui.simulate_keystrokes("4 2");
        let after_typing = february(28).and_hms_opt(10, 58, 42).unwrap().time();
        assert_eq!(&*changes.borrow(), &[after_typing]);
        let arrow = ui.debug_bounds("time-picker-minute-up").unwrap();
        ui.simulate_click(arrow.center(), Modifiers::none());
        let after_step = february(28).and_hms_opt(10, 59, 42).unwrap().time();
        ui.simulate_event(ScrollWheelEvent {
            position: seconds.center(),
            delta: ScrollDelta::Lines(point(0., -1.)),
            ..ScrollWheelEvent::default()
        });
        let after_scroll = february(28).and_hms_opt(10, 59, 41).unwrap().time();
        ui.read(|app| assert_eq!(picker.read(app).value(), Some(after_scroll)));
        assert_eq!(
            &*changes.borrow(),
            &[after_typing, after_step, after_scroll]
        );
    }

    /// Проверяет прокрутку часов и минут за рамкой компактного поля.
    #[gpui::test]
    fn compact_clock_scrolls_beyond_its_visible_frame(cx: &mut TestAppContext) {
        let day = february(28);
        let value = day.and_hms_opt(10, 58, 0).unwrap();
        let (picker, ui) = cx.add_window_view(|window, cx| {
            DateTimePicker::new(
                Some(value),
                day,
                TimePrecision::Minutes,
                PickerOptions::default(),
                window,
                cx,
            )
        });
        let clock = ui.debug_bounds("time-picker-clock").unwrap();
        let frame = ui.debug_bounds("time-picker-frame").unwrap();
        assert_eq!(frame.size, size(px(75.), px(30.)));
        assert_eq!(clock.size, size(px(85.), px(40.)));

        for (x, expected) in [
            (clock.left() + px(2.), day.and_hms_opt(11, 58, 0)),
            (clock.right() - px(2.), day.and_hms_opt(11, 59, 0)),
        ] {
            let position = point(x, clock.top() + px(2.));
            ui.simulate_mouse_move(position, None, Modifiers::none());
            ui.simulate_event(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Lines(point(0., 1.)),
                ..ScrollWheelEvent::default()
            });
            ui.read(|app| assert_eq!(picker.read(app).value(), expected));
        }
    }

    /// Проверяет, что выбор дня и ввод времени публикуют только общее допустимое значение.
    #[gpui::test]
    fn date_time_picker_emits_the_merged_value_and_respects_day_bounds(cx: &mut TestAppContext) {
        let start = february(28).and_hms_opt(10, 58, 0).unwrap();
        let same_time = february(29).and_time(start.time());
        let maximum = february(29).and_hms_opt(11, 0, 0).unwrap();
        let (picker, ui) = cx.add_window_view(|window, cx| {
            DateTimePicker::new(
                Some(start),
                start.date(),
                TimePrecision::Minutes,
                PickerOptions::default(),
                window,
                cx,
            )
        });
        let changes = Rc::new(RefCell::new(Vec::new()));
        let received = Rc::clone(&changes);
        ui.update(|_, app| {
            picker.update(app, |picker, cx| picker.set_bounds(None, Some(maximum), cx));
            app.subscribe(&picker, move |_, change: &DateTimePickerChanged, _| {
                received.borrow_mut().push(change.0);
            })
            .detach();
        });

        let day = ui.debug_bounds("date-picker-day-2024-02-29").unwrap();
        ui.simulate_click(day.center(), Modifiers::none());
        assert_eq!(&*changes.borrow(), &[same_time]);

        let minutes = ui.debug_bounds("time-picker-minute").unwrap();
        ui.simulate_click(minutes.center(), Modifiers::none());
        ui.simulate_keystrokes("up up");
        let at_59 = february(29).and_hms_opt(10, 59, 0).unwrap();
        assert_eq!(&*changes.borrow(), &[same_time, at_59]);

        let hours = ui.debug_bounds("time-picker-hour").unwrap();
        ui.simulate_click(hours.center(), Modifiers::none());
        ui.simulate_keystrokes("up");
        ui.read(|app| assert_eq!(picker.read(app).value(), Some(maximum)));
        assert_eq!(&*changes.borrow(), &[same_time, at_59, maximum]);

        // Однозначный ввод подтверждается также при переходе из часов в календарь.
        ui.simulate_keystrokes("9");
        let previous_day = ui.debug_bounds("date-picker-day-2024-02-28").unwrap();
        ui.simulate_click(previous_day.center(), Modifiers::none());
        let earlier_day = february(28).and_hms_opt(9, 0, 0).unwrap();
        ui.read(|app| assert_eq!(picker.read(app).value(), Some(earlier_day)));
        assert_eq!(changes.borrow().last(), Some(&earlier_day));
    }

    /// Проверяет, что часы активируются только после выбора даты и получают её нижнюю границу.
    #[gpui::test]
    fn empty_date_time_picker_activates_clock_after_selecting_a_day(cx: &mut TestAppContext) {
        let day = february(28);
        let minimum = day.and_hms_opt(10, 58, 10).unwrap();
        let (picker, ui) = cx.add_window_view(|window, cx| {
            let mut picker = DateTimePicker::new(
                None,
                day,
                TimePrecision::Minutes,
                PickerOptions::default(),
                window,
                cx,
            );
            picker.set_bounds(Some(minimum), None, cx);
            picker
        });
        let hours = ui.debug_bounds("time-picker-hour").unwrap();
        ui.simulate_click(hours.center(), Modifiers::none());
        ui.simulate_keystrokes("up");
        ui.read(|app| assert_eq!(picker.read(app).value(), None));

        let date = ui.debug_bounds("date-picker-day-2024-02-28").unwrap();
        ui.simulate_click(date.center(), Modifiers::none());
        ui.read(|app| assert_eq!(picker.read(app).value(), day.and_hms_opt(10, 59, 0)));
    }
}
