//! Следит за вводом в окне и определяет, когда показать рамку клавиатурного фокуса.

use std::collections::HashSet;

use gpui::{
    App, DispatchPhase, Global, IntoElement, MouseDownEvent, Window, WindowId, canvas, prelude::*,
};

/// Окна, в которых последнее нажатие было с клавиатуры.
#[derive(Default)]
struct KeyboardFocus(HashSet<WindowId>); // Остальные окна скрывают рамку фокуса.

impl Global for KeyboardFocus {}

/// Подключает к корню окна отслеживание клавиш и нажатий мыши.
pub fn track_focus_input(app: &mut App) -> impl IntoElement {
    if !app.has_global::<KeyboardFocus>() {
        app.set_global(KeyboardFocus::default());
        // Tab и другие сочетания могут обработаться раньше обычного on_key_down.
        app.intercept_keystrokes(|_, window, app| set_visible(true, window, app))
            .detach();
        app.on_window_closed(|app| {
            let windows = app.windows();
            app.global_mut::<KeyboardFocus>()
                .0
                .retain(|id| windows.iter().any(|window| window.window_id() == *id));
        })
        .detach();
    }

    // Слушаем окно целиком, включая меню, которые перекрывают попадание в корень.
    canvas(
        |_, _, _| (),
        |_, (), window, _| {
            window.on_mouse_event(|_: &MouseDownEvent, phase, window, app| {
                if phase == DispatchPhase::Capture {
                    set_visible(false, window, app);
                }
            });
        },
    )
    .absolute()
}

/// Возвращает, нужно ли сейчас выделять сфокусированный элемент рамкой.
pub(super) fn is_visible(window: &Window, app: &App) -> bool {
    app.try_global::<KeyboardFocus>()
        .is_some_and(|focus| focus.0.contains(&window.window_handle().window_id()))
}

/// Перерисовывает окно только при смене способа ввода.
fn set_visible(visible: bool, window: &mut Window, app: &mut App) {
    if is_visible(window, app) == visible {
        return;
    }
    let id = window.window_handle().window_id();
    let windows = &mut app.global_mut::<KeyboardFocus>().0;
    if visible {
        windows.insert(id);
    } else {
        windows.remove(&id);
    }
    window.refresh();
}
