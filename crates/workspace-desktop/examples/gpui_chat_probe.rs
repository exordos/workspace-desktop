//! Запускает отдельное окно, чтобы проверить сборку и отрисовку GPUI на macOS.

#[cfg(target_os = "macos")]
use gpui::{
    App, Application, Bounds, Context, SharedString, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};

/// Содержимое окна для проверки, что GPUI рисует текст.
#[cfg(target_os = "macos")]
struct Probe;

#[cfg(target_os = "macos")]
impl Render for Probe {
    /// Рисует различимую надпись по центру окна.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x17191e))
            .text_color(rgb(0xf7f8fa))
            .text_size(px(24.))
            .child("Workspace Desktop на GPUI")
    }
}

/// Открывает окно без подключения к профилю, сети и системным секретам.
#[cfg(target_os = "macos")]
fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(480.), px(240.)), cx);
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
                    title: Some(SharedString::from("Workspace GPUI Probe")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Probe),
        )
        .expect("Не удалось открыть окно GPUI");
        cx.activate(true);
    });
}

/// Объясняет ограничение прототипа на других платформах.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Прототип GPUI запускается на macOS");
}
