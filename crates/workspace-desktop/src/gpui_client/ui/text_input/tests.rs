//! Проверяет защиту значения и клавиатурное управление однострочным полем.

use std::{cell::Cell, rc::Rc};

use gpui::{Modifiers, TestAppContext};

use super::*;

/// Ввод остаётся доступен форме. Окно рисует маску, буфер и AppKit не получают пароль.
#[gpui::test]
fn secret_input_keeps_password_out_of_display_and_clipboard(cx: &mut TestAppContext) {
    cx.update(TextInput::bind_keys);
    cx.write_to_clipboard(ClipboardItem::new_string("ёж🔑".into()));
    let (input, ui) =
        cx.add_window_view(|_, cx| TextInput::new("test-password", "Пароль", true, cx));

    ui.run_until_parked();
    let bounds = ui.debug_bounds("test-password").expect("видимое поле");
    ui.simulate_click(bounds.center(), Modifiers::none());
    ui.simulate_keystrokes("cmd-v");
    ui.update(|window, app| {
        app.write_to_clipboard(ClipboardItem::new_string("содержимое буфера".into()));
        input.update(app, |view, cx| {
            assert_eq!(view.text(), "ёж🔑");
            assert_eq!(
                view.line
                    .as_ref()
                    .expect("нарисованный текст")
                    .shape
                    .text
                    .as_ref(),
                "•••"
            );
            let mut adjusted = None;
            assert_eq!(view.text_for_range(0..4, &mut adjusted, window, cx), None);
        });
    });
    ui.simulate_keystrokes("cmd-a cmd-c cmd-x");
    ui.read(|app| {
        let clipboard = app.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(clipboard.as_deref(), Some("содержимое буфера"));
        assert_eq!(input.read(app).text(), "ёж🔑");
    });
    ui.simulate_keystrokes("backspace");
    ui.read(|app| assert!(input.read(app).text().is_empty()));
}

/// Enter передаётся форме после системного ввода. Поле в ожидании ответа защищено от правок.
#[gpui::test]
fn input_submits_only_when_enabled_and_composition_is_complete(cx: &mut TestAppContext) {
    cx.update(TextInput::bind_keys);
    let (input, ui) = cx.add_window_view(|_, cx| TextInput::new("test-login", "Логин", false, cx));
    let submitted = Rc::new(Cell::new(0));
    let output = Rc::clone(&submitted);
    ui.update(|window, app| {
        input.update(app, |view, cx| {
            view.focus(window);
            view.replace_and_mark_text_in_range(None, "имя", None, window, cx);
        });
        app.subscribe(&input, move |_, _: &TextInputSubmitted, _| {
            output.set(output.get() + 1);
        })
        .detach();
    });
    ui.run_until_parked();
    ui.simulate_keystrokes("enter");
    assert_eq!(submitted.get(), 0, "композиция IME ещё не завершена");

    ui.update(|window, app| input.update(app, |view, cx| view.unmark_text(window, cx)));
    ui.simulate_keystrokes("enter");
    assert_eq!(
        submitted.get(),
        1,
        "после завершения IME Enter отправляет форму"
    );

    ui.update(|_, app| input.update(app, |view, cx| view.set_enabled(false, cx)));
    ui.run_until_parked();
    ui.simulate_keystrokes("backspace enter");
    ui.simulate_input("x");
    ui.read(|app| assert_eq!(input.read(app).text(), "имя"));
    assert_eq!(submitted.get(), 1);
}
