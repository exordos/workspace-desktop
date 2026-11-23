//! Проверяет один ряд, жест трекпада и клавиатурный переход к скрытой вкладке.

use gpui::{
    Context, FocusHandle, KeyBinding, KeyUpEvent, Keystroke, Modifiers, Render, ScrollDelta,
    ScrollWheelEvent, TestAppContext, TouchPhase, Window, div, point, prelude::*, px, size,
};

use super::{FilterGroup, filter_button};
use crate::gpui_client::ui::{Palette, ThemeMode};

gpui::actions!(filter_group_test, [NextFilter]);

/// Четыре вкладки в узкой области с начальным фокусом перед рядом.
struct View {
    group: FilterGroup,       // Рабочая полоса с реальными кнопками.
    initial: FocusHandle,     // Откуда начинаем Tab.
    activated: Option<usize>, // Номер последнего активированного фильтра.
}

impl Render for View {
    /// Рисует тот же контрол, который используется в сайдбаре.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::Dark);
        let view = cx.entity().downgrade();
        div()
            .size_full()
            .track_focus(&self.initial)
            .on_action(|_: &NextFilter, window, _| window.focus_next())
            .child(
                self.group.render(
                    palette,
                    window,
                    ["С подпиской", "Без подписки", "Все", "Архив"]
                        .into_iter()
                        .enumerate()
                        .map(|(index, title)| {
                            let view = view.clone();
                            filter_button(
                                ("filter", index),
                                title,
                                index == 1,
                                palette,
                                Some(self.group.focus(index)),
                                move |_, app| {
                                    let _ =
                                        view.update(app, |view, _| view.activated = Some(index));
                                },
                            )
                        }),
                ),
            )
    }
}

/// Горизонтальный жест двигает полосу, а Tab раскрывает скрытый край для Enter.
#[gpui::test]
fn narrow_filters_scroll_to_an_offscreen_tab(cx: &mut TestAppContext) {
    cx.update(|app| app.bind_keys([KeyBinding::new("tab", NextFilter, None)]));
    let (view, ui) = cx.add_window_view(|window, cx| {
        let initial = cx.focus_handle();
        window.focus(&initial);
        View {
            group: FilterGroup::new(4, cx),
            initial,
            activated: None,
        }
    });
    ui.simulate_resize(size(px(180.), px(140.)));
    ui.run_until_parked();
    let strip = ui.debug_bounds("channel-filter-strip").unwrap();
    assert_eq!(strip.size.height, px(40.));
    ui.read(|app| {
        let group = &view.read(app).group;
        assert!(group.scroll.bounds_for_item(3).unwrap().right() > strip.right());
    });
    ui.simulate_event(ScrollWheelEvent {
        position: strip.center(),
        delta: ScrollDelta::Pixels(point(px(-80.), px(0.))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    ui.run_until_parked();
    ui.read(|app| assert!(view.read(app).group.scroll.offset().x < px(0.)));
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.group.reset();
            cx.notify();
        })
    });
    for _ in 0..4 {
        ui.simulate_keystrokes("tab");
        ui.run_until_parked();
    }
    ui.update(|window, app| {
        let group = &view.read(app).group;
        assert!(group.focus(3).is_focused(window));
        let archive = group.scroll.bounds_for_item(3).unwrap();
        let offset = group.scroll.offset().x;
        assert!(offset < px(0.));
        assert!(archive.right() + offset <= strip.right());
        assert!(archive.left() + offset >= strip.left());
    });
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    ui.read(|app| assert_eq!(view.read(app).activated, Some(3)));

    ui.simulate_resize(size(px(330.), px(140.)));
    ui.run_until_parked();
    let first = ui.read(|app| {
        let group = &view.read(app).group;
        assert_eq!(group.scroll.offset().x, px(0.));
        assert!(group.scroll.bounds_for_item(3).unwrap().right() <= group.scroll.bounds().right());
        group.scroll.bounds_for_item(0).unwrap().center()
    });
    ui.simulate_click(first, Modifiers::none());
    ui.read(|app| assert_eq!(view.read(app).activated, Some(0)));
}
