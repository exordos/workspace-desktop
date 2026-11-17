//! Проверяет, как GPUI скрывает и возвращает то же окно через меню AppKit.
//! Счётчик в окне показывает, сохранилось ли состояние и вернулся ли фокус.

#[cfg(target_os = "macos")]
mod macos {
    use futures::{StreamExt, channel::mpsc};
    use gpui::{
        App, Application, Bounds, Context, FocusHandle, KeyDownEvent, SharedString, Window,
        WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
    };
    use objc2::{class, msg_send, runtime::Bool};
    use objc2_foundation::MainThreadMarker;
    use workspace_desktop::platform::tray::{Command, Tray};

    /// Состояние одного окна, которое остаётся живым после скрытия.
    struct Probe {
        focus: FocusHandle, // Фокус для проверки клавиши пробела.
        count: u32,         // Сколько раз изменили счётчик в этом окне.
    }

    impl Probe {
        /// Запоминает изменение содержимого и просит GPUI обновить окно.
        fn increment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            self.count += 1;
            let id = window.window_handle().window_id();
            eprintln!("event=count window={id:?} value={}", self.count);
            cx.notify();
        }
    }

    impl Render for Probe {
        /// Показывает счётчик, который меняется по нажатию кнопки или пробела.
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(rgb(0x17191e))
                .text_color(rgb(0xf7f8fa))
                .track_focus(&self.focus)
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "space" && !event.is_held {
                        this.increment(window, cx);
                    }
                }))
                .child(format!("Счётчик: {}", self.count))
                .child(
                    div()
                        .id("increment")
                        .px_4()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x324157))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.increment(window, cx);
                        }))
                        .child("Увеличить (пробел)"),
                )
        }
    }

    /// Подключает существующее меню только на главном потоке и открывает окно.
    pub(super) fn run() {
        Application::new().run(|cx: &mut App| {
            // В сборке GPUI без Iced типизированный API NSThread не включён.
            let on_main_thread: Bool = unsafe { msg_send![class!(NSThread), isMainThread] };
            assert!(on_main_thread.as_bool(), "Оконный цикл должен работать на главном потоке");
            // AppKit получит маркер только после проверки текущего потока.
            let main_thread = unsafe { MainThreadMarker::new_unchecked() };
            let (commands, mut receiver) = mpsc::unbounded();
            let tray = if std::env::var_os("WORKSPACE_DESKTOP_DISABLE_TRAY").as_deref()
                == Some(std::ffi::OsStr::new("1"))
            {
                None
            } else {
                Tray::install(main_thread, commands)
            };
            let has_tray = tray.is_some();
            eprintln!("event=started main_thread=true tray={has_tray}");

            cx.on_app_quit(|_| async { eprintln!("event=app_quit") })
                .detach();
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    eprintln!("event=last_window_closed");
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(480.), px(260.)), cx);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some(SharedString::from("Workspace GPUI Window Probe")),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    |window, cx| {
                        window.on_window_should_close(cx, move |window, cx| {
                            let id = window.window_handle().window_id();
                            eprintln!("event=close_requested window={id:?} tray={has_tray}");
                            if has_tray {
                                // Скрытие приложения сохраняет окно и его содержимое.
                                cx.hide();
                            }
                            !has_tray
                        });
                        cx.new(|cx: &mut Context<Probe>| {
                            let focus = cx.focus_handle();
                            window.focus(&focus);
                            cx.observe_window_activation(window, |probe, window, cx| {
                                let id = window.window_handle().window_id();
                                let active = window.is_window_active();
                                let focused = probe.focus.is_focused(window);
                                eprintln!(
                                    "event=activation window={id:?} active={active} focused={focused} count={} entity={:?}",
                                    probe.count,
                                    cx.entity_id()
                                );
                            })
                            .detach();
                            Probe { focus, count: 0 }
                        })
                    },
                )
                .expect("Не удалось открыть окно прототипа");

            if let Some(tray) = tray {
                cx.spawn(async move |cx| {
                    // Меню и его получатель команд живут до окончания оконного цикла.
                    let _tray = tray;
                    while let Some(command) = receiver.next().await {
                        match command {
                            Command::Show => {
                                let result = window.update(cx, |probe, window, cx| {
                                    let id = window.window_handle().window_id();
                                    eprintln!("event=tray_show window={id:?} count={}", probe.count);
                                    cx.activate(true);
                                    window.activate_window();
                                });
                                if let Err(error) = result {
                                    eprintln!("event=show_failed error={error}");
                                }
                            }
                            Command::Quit => {
                                eprintln!("event=tray_quit");
                                let _ = cx.update(|cx| cx.quit());
                                break;
                            }
                        }
                    }
                })
                .detach();
            }

            cx.activate(true);
        });
    }
}

/// Запускает проверку жизненного цикла окна в графической сессии macOS.
#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

/// Сообщает, где доступна проверка меню AppKit.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Прототип окна GPUI с меню AppKit запускается на macOS");
}
