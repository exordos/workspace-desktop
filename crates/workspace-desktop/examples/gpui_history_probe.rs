//! Открывает прототип виртуальной истории с фиксированными данными и командами для проверки.

#[cfg(target_os = "macos")]
#[path = "gpui_probe/history.rs"]
mod history;

/// Открывает отдельное окно истории GPUI.
#[cfg(target_os = "macos")]
fn main() {
    use gpui::{
        AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
    };

    Application::new().run(|cx| {
        let bounds = Bounds::centered(None, size(px(720.), px(520.)), cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("GPUI — виртуальная история".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| history::HistoryProbe::new()),
        )
        .expect("Не удалось открыть окно истории GPUI");
        cx.activate(true);
    });
}

/// Сообщает, на какой платформе запускается прототип.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Прототип GPUI запускается на macOS");
}
