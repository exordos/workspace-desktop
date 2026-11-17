//! Собирает отдельный оконный клиент GPUI.
//! Контракты отделяют фоновые события от состояния и действий окна; здесь находится точка сборки.

pub mod composer;
pub mod contracts;
pub mod conversation;
pub mod platform_window;
pub mod runtime;
pub mod shell;
pub mod sidebar;
pub mod ui;

use gpui::{Application, Context, Window, WindowOptions, div, prelude::*};

/// Начальный экран до подключения оболочки, профиля и фоновой работы.
struct ClientFrame;

impl Render for ClientFrame {
    /// Показывает, что отдельная точка запуска открыла окно нового клиента.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child("Workspace Desktop — каркас клиента GPUI")
    }
}

/// Открывает отдельное окно GPUI. На этом этапе окно не читает профиль.
pub fn run() {
    Application::new().run(|app| {
        app.on_window_closed(|app| {
            if app.windows().is_empty() {
                app.quit();
            }
        })
        .detach();
        app.open_window(WindowOptions::default(), |_, app| app.new(|_| ClientFrame))
            .expect("Не удалось открыть окно Workspace Desktop GPUI");
        app.activate(true);
    });
}
