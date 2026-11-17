//! Собирает чат и воспроизводит догрузку во время ввода. Здесь окно, шаги сценария и замеры.

use std::time::{Duration, Instant};

use futures::{channel::oneshot, future::Either};
use gpui::{
    App, Application, AsyncWindowContext, Bounds, Context, Entity, Focusable, Keystroke, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use uuid::Uuid;

use crate::{
    editor::Editor,
    history::{HistoryAction, HistoryProbe, JUMP_TARGET, RowGeometry},
};

const TOLERANCE_PX: f32 = 1.;
const KEY_INTERVAL: Duration = Duration::from_millis(12);
const INPUT_COUNT: usize = 64;
const LONG_FRAME: Duration = Duration::from_millis(34);
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// Экран с независимыми сущностями истории и редактора.
struct Chat {
    history: Entity<HistoryProbe>, // История на фиксированных данных.
    editor: Entity<Editor>,        // Редактор с системным обработчиком ввода.
    result: RunState,              // Состояние единственного прогона в текущем окне.
}

/// Говорит, можно ли начать воспроизведение и когда доступен итог прогона.
#[derive(Default)]
enum RunState {
    #[default]
    Ready, // Сценарий ещё не запущен.
    Running,    // Выполняются команды истории и ввода.
    Done(bool), // true, если проверки геометрии и ввода прошли.
}

impl Chat {
    /// Создаёт историю и редактор, сразу направляет ввод в редактор.
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(Editor::new);
        window.focus(&editor.focus_handle(cx));
        let history = cx.new(|_| HistoryProbe::new());
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        Self {
            history,
            editor,
            result: RunState::Ready,
        }
    }

    /// Начинает сценарий один раз и выводит результат в том же окне.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.result, RunState::Ready) {
            return;
        }
        self.result = RunState::Running;
        window.focus(&self.editor.focus_handle(cx));
        let (history, editor) = (self.history.clone(), self.editor.clone());
        let trace = std::env::var_os("WORKSPACE_GPUI_PROBE_TRACE_FRAMES").is_some();
        let chat = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let result = reproduce(cx, history, editor, trace).await;
                match &result {
                    Ok(()) => eprintln!("level=debug event=repro_finished result=ok"),
                    Err(step) => {
                        eprintln!("level=debug event=repro_finished result=failed step={step}")
                    }
                }
                let _ = cx.update(|_, cx| {
                    let _ = chat.update(cx, |chat, cx| {
                        chat.result = RunState::Done(result.is_ok());
                        cx.notify();
                    });
                });
            })
            .detach();
        cx.notify();
    }
}

impl Render for Chat {
    /// Размещает редактор под историей и оставляет состояние обоих компонентов при обновлении.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match self.result {
            RunState::Ready => "Запустить воспроизведение",
            RunState::Running => "Проверка выполняется…",
            RunState::Done(true) => "Проверка прошла",
            RunState::Done(false) => "Есть расхождение — см. журнал",
        };
        let ready = matches!(self.result, RunState::Ready);
        let count = self.editor.read(cx).text().chars().count();
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(rgb(0x17191e))
            .text_color(rgb(0xf1f2f5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .id("reproduce-scroll-jump")
                            .rounded_md()
                            .p_2()
                            .bg(rgb(0x303747))
                            .when(ready, |button| {
                                button.cursor_pointer().on_click(cx.listener(
                                    |chat, _, window, cx| {
                                        chat.start(window, cx);
                                    },
                                ))
                            })
                            .child(status),
                    )
                    .child(format!("Символов в редакторе: {count}")),
            )
            .child(div().flex_1().min_h_0().child(self.history.clone()))
            .child(self.editor.clone())
    }
}

/// Открывает отдельное окно; прикладной профиль и сетевые службы в сценарии не нужны.
pub(crate) fn open(reproduce_on_open: bool) {
    Application::new().run(move |cx: &mut App| {
        Editor::bind_keys(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(770.), px(665.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Workspace GPUI Chat Probe".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let chat = cx.new(|cx| Chat::new(window, cx));
                if reproduce_on_open {
                    chat.update(cx, |chat, cx| chat.start(window, cx));
                }
                chat
            },
        )
        .expect("Не удалось открыть прототип разговора GPUI");
        cx.activate(true);
    });
}

/// Снимок опорной строки и количества строк при начале одной операции чтения.
struct ReadingOperation {
    id: &'static str,    // Один op_id для запроса, изменения списка и измерения.
    before: RowGeometry, // Фактическая геометрия исходного кадра.
    rows: usize,         // Размер списка до операции.
}

impl ReadingOperation {
    /// Регистрирует запрос и сохраняет опору текущего кадра.
    fn begin(id: &'static str, history: &HistoryProbe) -> Result<Self, &'static str> {
        let before = history.anchor().ok_or("capture_reading_anchor")?;
        let rows = history.row_count();
        eprintln!(
            "level=debug op_id={id} event=update_requested mode=reading anchor_uuid={} rows_before={rows} y_before_px={}",
            before.uuid, before.y
        );
        Ok(Self { id, before, rows })
    }

    /// Регистрирует фактическое изменение модели после чтения страницы или получения сообщения.
    fn updated(&self, history: &HistoryProbe) {
        let rows_after = history.row_count();
        eprintln!(
            "level=debug op_id={} event=list_updated anchor_uuid={} rows_before={} rows_after={rows_after} added_rows={}",
            self.id,
            self.before.uuid,
            self.rows,
            rows_after.saturating_sub(self.rows)
        );
    }

    /// Сверяет UUID и координату опоры после отрисовки изменённого списка.
    fn finish(&self, history: &HistoryProbe) -> Result<(), &'static str> {
        let after = history.anchor().ok_or_else(|| {
            eprintln!(
                "level=debug op_id={} event=anchor_missing anchor_uuid={} rows_after={}",
                self.id,
                self.before.uuid,
                history.row_count()
            );
            "anchor_missing"
        })?;
        let delta = after.y - self.before.y;
        eprintln!(
            "level=debug op_id={} event=anchor_measured mode=reading anchor_uuid={} after_uuid={} rows_after={} y_before_px={} y_after_px={} target_inset_px={} observed_inset_px={} delta_px={delta}",
            self.id,
            self.before.uuid,
            after.uuid,
            history.row_count(),
            self.before.y,
            after.y,
            -self.before.y,
            -after.y
        );
        if self.before.uuid != after.uuid || delta.abs() > TOLERANCE_PX {
            return Err("reading_anchor_changed");
        }
        Ok(())
    }
}

/// Проверяет, что новое последнее сообщение дошло до видимого нижнего края.
fn verify_bottom(history: &HistoryProbe, previous: Uuid) -> Result<(), &'static str> {
    let last = history.last_uuid();
    let geometry = history.geometry(last).ok_or("new_last_row_not_visible")?;
    let gap = geometry.viewport_height - (geometry.y + geometry.height);
    eprintln!(
        "level=debug op_id=follow-4 event=last_row_measured mode=follow_end previous_uuid={previous} last_uuid={last} rows_after={} y_px={} bottom_gap_px={gap}",
        history.row_count(),
        geometry.y
    );
    if last == previous || gap.abs() > TOLERANCE_PX || geometry.y < 0. {
        return Err("follow_end_missed");
    }
    Ok(())
}

/// Применяет команду истории через её обычный обработчик.
fn apply(
    cx: &mut AsyncWindowContext,
    history: &Entity<HistoryProbe>,
    action: HistoryAction,
) -> Result<(), &'static str> {
    cx.update(|_, cx| history.update(cx, |history, cx| history.apply(action, cx)))
        .map_err(|_| "apply_to_closed_window")
}

/// Ждёт реального прохода отрисовки. В GPUI колбэк следующего кадра идёт перед рисованием.
async fn wait_for_paint(
    cx: &mut AsyncWindowContext,
    step: &'static str,
    trace: bool,
) -> Result<(), &'static str> {
    let (send, receive) = oneshot::channel();
    let start = Instant::now();
    cx.update(|window, _| {
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, _| {
                let _ = send.send(());
            });
            window.refresh();
        });
        window.refresh();
    })
    .map_err(|_| step)?;
    let timeout = cx.background_executor().timer(FRAME_TIMEOUT);
    let result = futures::future::select(receive, timeout).await;
    match result {
        Either::Left((Ok(()), _)) => {
            let elapsed = start.elapsed();
            if trace || elapsed >= LONG_FRAME {
                eprintln!(
                    "level=debug event=paint_wait step={step} elapsed_us={} over_34ms={}",
                    elapsed.as_micros(),
                    elapsed >= LONG_FRAME
                );
            }
            Ok(())
        }
        _ => Err(step),
    }
}

/// Выполняет догрузку и обновления в режиме чтения, затем проверяет нижнюю привязку.
async fn update_history(
    cx: &mut AsyncWindowContext,
    history: Entity<HistoryProbe>,
    trace: bool,
) -> Result<(), &'static str> {
    for (id, action) in [
        ("prepend-1", HistoryAction::Prepend),
        ("image-2", HistoryAction::GrowAbove),
        ("incoming-3", HistoryAction::Append),
    ] {
        let (operation, image) = cx
            .update(|_, cx| {
                let history = history.read(cx);
                (
                    ReadingOperation::begin(id, history),
                    history.above_anchor_uuid(),
                )
            })
            .map_err(|_| "read_closed_window")?;
        let operation = operation?;
        apply(cx, &history, action)?;
        if matches!(action, HistoryAction::GrowAbove) {
            let image = image.ok_or("identify_image_row")?;
            let start = Instant::now();
            while !cx
                .update(|_, cx| history.read(cx).image_loaded(image))
                .map_err(|_| "image_window_closed")?
            {
                if start.elapsed() >= FRAME_TIMEOUT {
                    return Err("await_delayed_image");
                }
                cx.background_executor().timer(KEY_INTERVAL).await;
            }
        }
        cx.update(|_, cx| operation.updated(history.read(cx)))
            .map_err(|_| "record_list_update")?;
        wait_for_paint(cx, id, trace).await?;
        cx.update(|_, cx| operation.finish(history.read(cx)))
            .map_err(|_| "measure_reading_anchor")??;
    }

    apply(cx, &history, HistoryAction::ToBottom)?;
    wait_for_paint(cx, "go_to_latest", trace).await?;
    let previous = cx.update(|_, cx| {
        let history = history.read(cx);
        let previous = history.last_uuid();
        eprintln!(
            "level=debug op_id=follow-4 event=update_requested mode=follow_end previous_uuid={previous} rows_before={}",
            history.row_count()
        );
        previous
    }).map_err(|_| "identify_previous_last_row")?;
    apply(cx, &history, HistoryAction::Append)?;
    wait_for_paint(cx, "follow-4", trace).await?;
    cx.update(|_, cx| verify_bottom(history.read(cx), previous))
        .map_err(|_| "measure_bottom_edge")?
}

/// Возвращает очередную клавишу из фиксированной последовательности.
fn input_key(index: usize) -> char {
    char::from(b'a' + (index % 26) as u8)
}

/// Подаёт клавиши через GPUI, сверяет полный результат и измеряет время обработки каждой клавиши.
async fn type_during_updates(
    cx: &mut AsyncWindowContext,
    editor: Entity<Editor>,
) -> Result<(), &'static str> {
    let original = cx
        .update(|_, cx| editor.read(cx).text().to_owned())
        .map_err(|_| "read_editor_before_input")?;
    let mut received = 0;
    let mut expected = String::new();
    let mut max_dispatch = Duration::ZERO;
    let mut max_gap = Duration::ZERO;
    let mut previous = Instant::now();
    let run = previous;
    for index in 0..INPUT_COUNT {
        let key = input_key(index);
        expected.push(key);
        let started = Instant::now();
        if index > 0 {
            max_gap = max_gap.max(started - previous);
        }
        previous = started;
        let keystroke = Keystroke::parse(&key.to_string()).map_err(|_| "parse_test_input")?;
        let changed = cx
            .update(|window, cx| {
                let before = editor.read(cx).text().chars().count();
                let handled = window.dispatch_keystroke(keystroke, cx);
                handled && editor.read(cx).text().chars().count() == before + 1
            })
            .map_err(|_| "dispatch_test_input")?;
        received += usize::from(changed);
        max_dispatch = max_dispatch.max(started.elapsed());
        cx.background_executor().timer(KEY_INTERVAL).await;
    }
    let matches = cx
        .update(|_, cx| editor.read(cx).text().strip_prefix(&original) == Some(&expected))
        .map_err(|_| "read_editor_after_input")?;
    eprintln!(
        "level=debug event=typing_result sent={INPUT_COUNT} received={received} content_matches={matches} interval_us={} max_dispatch_us={} max_gap_us={} elapsed_us={}",
        KEY_INTERVAL.as_micros(),
        max_dispatch.as_micros(),
        max_gap.as_micros(),
        run.elapsed().as_micros()
    );
    if received != INPUT_COUNT || !matches {
        return Err("typing_lost_or_reordered");
    }
    Ok(())
}

/// Готовит измеримую опору, затем совмещает поток клавиш и обновления истории.
async fn reproduce(
    cx: &mut AsyncWindowContext,
    history: Entity<HistoryProbe>,
    editor: Entity<Editor>,
    trace: bool,
) -> Result<(), &'static str> {
    apply(cx, &history, HistoryAction::Jump(JUMP_TARGET))?;
    wait_for_paint(cx, "open_anchor", trace).await?;
    let visible = cx
        .update(|_, cx| history.read(cx).geometry(JUMP_TARGET).is_some())
        .map_err(|_| "read_initial_target")?;
    if !visible {
        return Err("initial_target_not_drawn");
    }
    let mut input_context = cx.clone();
    let (input, history) = futures::join!(
        type_during_updates(&mut input_context, editor),
        update_history(cx, history, trace)
    );
    history?;
    input
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// Подгружает историю в том же окне между клавишами и проверяет опору и точный черновик.
    #[gpui::test]
    fn keeps_anchor_and_typing_during_history_updates(cx: &mut TestAppContext) {
        cx.update(Editor::bind_keys);
        let (chat, cx) = cx.add_window_view(Chat::new);
        let (history, editor) = cx.read(|cx| {
            let chat = chat.read(cx);
            (chat.history.clone(), chat.editor.clone())
        });
        cx.update(|_, cx| {
            history.update(cx, |history, cx| {
                history.apply(HistoryAction::Jump(JUMP_TARGET), cx);
            })
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("a b c");

        let before = cx
            .read(|cx| ReadingOperation::begin("combined-test", history.read(cx)))
            .unwrap();
        cx.update(|_, cx| {
            history.update(cx, |history, cx| history.apply(HistoryAction::Prepend, cx))
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("d e f");
        cx.read(|cx| {
            let history = history.read(cx);
            assert_eq!(history.row_count(), before.rows + 50);
            before.finish(history).expect("опора после подгрузки");
            assert_eq!(editor.read(cx).text(), "abcdef");
        });

        cx.update(|_, cx| {
            history.update(cx, |history, cx| history.apply(HistoryAction::ToBottom, cx))
        });
        cx.run_until_parked();
        let previous = cx.read(|cx| history.read(cx).last_uuid());
        cx.update(|_, cx| {
            history.update(cx, |history, cx| history.apply(HistoryAction::Append, cx))
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("g h");
        cx.read(|cx| {
            verify_bottom(history.read(cx), previous).expect("новое сообщение внизу");
            assert_eq!(editor.read(cx).text(), "abcdefgh");
        });
    }
}
