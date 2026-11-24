//! Проверяет высоту и фокус планки при изменении настройки анимаций.

use gpui::{Modifiers, Pixels, TestAppContext, VisualTestContext, size};

use super::*;

/// Меняет сохранённый флаг тем же способом, что и «Внешний вид».
fn set_motion(ui: &mut VisualTestContext, enabled: bool) {
    ui.update(|_, app| Preferences::set_animations_enabled(app, enabled).unwrap());
    ui.run_until_parked();
}

/// Возвращает видимую высоту планки в тестовом окне.
fn height(ui: &mut VisualTestContext) -> Pixels {
    ui.debug_bounds("notification-permission-banner")
        .unwrap()
        .size
        .height
}

/// Выключение завершает движение и сохраняет фокус, а включение действует со следующего клика.
#[gpui::test]
fn notification_banner_finishes_in_progress_slide_when_animations_are_disabled(
    cx: &mut TestAppContext,
) {
    let clock = cx.executor();
    cx.update(|app| Preferences::set_animations_enabled(app, false).unwrap());
    let (banner, ui) = cx.add_window_view(|_, cx| {
        let return_focus = cx.focus_handle();
        NotificationBanner::new(
            NotificationBannerActions {
                status: |_| (Some(NotificationPermission::NeedsPermission), false),
                request: |_| panic!("пользователь не запрашивал разрешение"),
                open_settings: |_| panic!("пользователь не открывал настройки ОС"),
            },
            return_focus,
            cx,
        )
    });
    ui.simulate_resize(size(px(700.), px(600.)));
    ui.run_until_parked();

    let collapsed = px(STRIP_HEIGHT);
    let expanded = px(STRIP_HEIGHT + CONTENT_HEIGHT);
    assert_eq!(height(ui), expanded);
    assert!(ui.read(|app| banner.read(app).slide_started.is_none()));

    set_motion(ui, true);
    assert_eq!(height(ui), expanded);
    assert!(ui.read(|app| banner.read(app).slide_started.is_none()));

    let later = ui.debug_bounds("notification-permission-later").unwrap();
    ui.simulate_click(later.center(), Modifiers::none());
    clock.advance_clock(SLIDE_TIME / 2);
    ui.update(|window, _| window.refresh());
    ui.run_until_parked();
    let closing = height(ui);
    assert!(closing > collapsed && closing < expanded);

    set_motion(ui, false);
    assert_eq!(height(ui), collapsed);
    assert!(ui.read(|app| banner.read(app).slide_started.is_none()));
    assert!(ui.update(|window, app| banner.read(app).strip.is_focused(window)));

    set_motion(ui, true);
    assert_eq!(height(ui), collapsed);
    assert!(ui.read(|app| banner.read(app).slide_started.is_none()));

    let strip = ui.debug_bounds("notification-permission-strip").unwrap();
    ui.simulate_click(strip.center(), Modifiers::none());
    clock.advance_clock(SLIDE_TIME / 2);
    ui.update(|window, _| window.refresh());
    ui.run_until_parked();
    let opening = height(ui);
    assert!(opening > collapsed && opening < expanded);

    set_motion(ui, false);
    assert_eq!(height(ui), expanded);
    assert!(ui.read(|app| banner.read(app).slide_started.is_none()));
    assert!(ui.update(|window, app| banner.read(app).primary.is_focused(window)));
}
