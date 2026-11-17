//! Запускает прототип многострочного редактора GPUI с отдельной точкой фокуса для проверки.

#[cfg(target_os = "macos")]
use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, KeyBinding, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
#[cfg(target_os = "macos")]
use workspace_desktop::gpui_client::composer::Editor;

/// Переходы между редактором и контрольным элементом прототипа.
#[cfg(target_os = "macos")]
mod focus_keys {
    gpui::actions!(editor_probe, [Next, Previous]);
}

/// Окно с редактором и кнопкой для проверки возврата фокуса.
#[cfg(target_os = "macos")]
struct Probe {
    editor: Entity<Editor>, // Поле, которое получает системный ввод.
    button: FocusHandle,    // Вторая точка фокуса для проверки Tab и мыши.
}

#[cfg(target_os = "macos")]
impl Probe {
    /// Создаёт пустой редактор и ставит в него фокус при открытии окна.
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(Editor::new);
        window.focus(&editor.focus_handle(cx));
        Self {
            editor,
            button: cx.focus_handle().tab_stop(true),
        }
    }
}

#[cfg(target_os = "macos")]
impl Render for Probe {
    /// Рисует редактор, кнопку и длину черновика для наблюдения без журнала текста.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let button_focus = self.button.clone();
        let button_color = if button_focus.is_focused(window) {
            0x465785
        } else {
            0x343843
        };
        let text = self.editor.read(cx).text();
        let count = text.chars().count();
        div()
            .size_full()
            .on_action(|_: &focus_keys::Next, window, _| window.focus_next())
            .on_action(|_: &focus_keys::Previous, window, _| window.focus_prev())
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(rgb(0x17191e))
            .text_color(rgb(0xf1f2f5))
            .child(self.editor.clone())
            .child(div().child(format!("Символов в черновике: {count}")))
            .child(
                div()
                    .id("probe-focus-target")
                    .track_focus(&self.button)
                    .bg(rgb(button_color))
                    .rounded_md()
                    .p_2()
                    .child("Проверить фокус")
                    .on_mouse_down(gpui::MouseButton::Left, move |_, window, _| {
                        window.focus(&button_focus)
                    }),
            )
    }
}

/// Подключает редактор и Tab-навигацию контрольного окна.
#[cfg(target_os = "macos")]
fn bind_keys(cx: &mut App) {
    Editor::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("tab", focus_keys::Next, None),
        KeyBinding::new("shift-tab", focus_keys::Previous, None),
    ]);
}

/// Открывает окно на фиксированных данных без профиля и сетевых запросов.
#[cfg(target_os = "macos")]
fn main() {
    Application::new().run(|cx: &mut App| {
        bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(560.), px(260.)), cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Workspace GPUI Editor Probe".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    let probe = Probe::new(window, cx);
                    cx.observe(&probe.editor, |_, _, cx| cx.notify()).detach();
                    probe
                })
            },
        )
        .expect("Не удалось открыть редактор GPUI");
        cx.activate(true);
    });
}

/// Сообщает об ограничении прототипа на других платформах.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Прототип GPUI запускается на macOS");
}

#[cfg(all(test, target_os = "macos", feature = "gpui-test"))]
mod tests {
    use gpui::{ClipboardItem, EntityInputHandler, TestAppContext};

    use super::*;

    /// Вводит многострочный текст, заменяет выделение и проверяет передачу фокуса.
    #[gpui::test]
    fn edits_multiline_text_and_respects_focus(cx: &mut TestAppContext) {
        cx.update(bind_keys);
        let (view, cx) = cx.add_window_view(Probe::new);
        let editor = cx.read(|cx| view.read(cx).editor.clone());
        cx.simulate_input("Привет");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("мир");
        cx.simulate_keystrokes("shift-enter");
        cx.write_to_clipboard(ClipboardItem::new_string("Здесь\r\nGPUI 🧪".into()));
        cx.simulate_keystrokes("cmd-v");
        cx.read(|cx| assert_eq!(editor.read(cx).text(), "Привет\nмир\nЗдесь\nGPUI 🧪"));

        cx.simulate_keystrokes("shift-left");
        cx.simulate_input("!");
        cx.simulate_keystrokes("tab");
        cx.update(|window, cx| assert!(view.read(cx).button.is_focused(window)));
        cx.simulate_input("лишний ввод");
        cx.simulate_keystrokes("shift-tab");
        cx.update(|window, cx| assert!(editor.focus_handle(cx).is_focused(window)));
        cx.simulate_input("?");
        cx.read(|cx| assert_eq!(editor.read(cx).text(), "Привет\nмир\nЗдесь\nGPUI !?"));
    }

    /// Проверяет относительные индексы AppKit и замену текущей композиции без дублей.
    #[gpui::test]
    fn commits_ime_at_utf16_offset(cx: &mut TestAppContext) {
        cx.update(bind_keys);
        let (editor, cx) = cx.add_window_view(|_, cx| Editor::new(cx));
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.replace_text_in_range(None, "🧪 ", window, cx);
                editor.replace_and_mark_text_in_range(None, "e\u{301}仮", Some(0..2), window, cx);
                assert_eq!(editor.marked_text_range(window, cx), Some(3..6));
                assert_eq!(
                    editor.selected_text_range(false, window, cx).unwrap().range,
                    3..5
                );
                editor.replace_text_in_range(None, "確定", window, cx);
                assert_eq!(editor.marked_text_range(window, cx), None);
                assert_eq!(editor.text(), "🧪 確定");
                assert_eq!(
                    editor.selected_text_range(false, window, cx).unwrap().range,
                    5..5
                );
            })
        });
    }
}
