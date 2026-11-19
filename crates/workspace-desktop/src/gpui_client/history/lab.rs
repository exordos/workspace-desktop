//! Открывает рабочий список в отдельном окне с синтетической историей и задержкой страниц.

use std::time::Duration;

use gpui::{
    App, AppContext, Application, Bounds, Context, IntoElement, Pixels, Render, ScrollWheelEvent,
    TitlebarOptions, Window, WindowBounds, WindowOptions, canvas, div, prelude::*, px, size,
};
use uuid::Uuid;
use workspace_core::storage::{HistoryDirection, HistoryEdge, HistoryScope, HistoryWindow};

use crate::gpui_client::ui::{Button, Palette, TextRole, ThemeMode, WorkspaceAssets, label};

use super::{
    history_fixtures::fixture_rows,
    viewport::{PageNeed, Position, Viewport},
};

const HISTORY_SIZE: u128 = 20_000;
const PAGE_SIZE: u128 = 50;
const FIRST: u128 = 201;
const LAST: u128 = 800;
const PAGE_DELAY: Duration = Duration::from_millis(240);
const MAX_AHEAD: Pixels = px(1200.);
const SCOPE: HistoryScope = HistoryScope {
    stream_id: Uuid::from_u128(2001),
    topic_id: Some(Uuid::from_u128(2002)),
};

/// Стартовые снимки для повторения догрузки в двух направлениях и заполнения экрана.
#[derive(Clone, Copy)]
enum Scene {
    Older,     // 600 строк, старт в нескольких жестах от старого края.
    Newer,     // Те же строки, старт недалеко от нового края.
    ShortTail, // Намеренно короткое первое окно у подтверждённого хвоста.
}

/// Запрос, пока источник имитирует сетевую задержку.
#[derive(Clone, Copy)]
struct Flight {
    id: Uuid,       // Определяет, относится ли ещё ответ к запрошенному окну.
    need: PageNeed, // Направление и запас в момент запроса.
}

/// Стенд связывает рабочий список с локальными страницами истории.
struct HistoryLab {
    viewport: Viewport,                  // Тот же список, который рисует рабочий чат.
    history: HistoryWindow,              // Загруженный участок фикстуры.
    direction: Option<HistoryDirection>, // Последнее направление жеста. До жеста заполняем окно.
    flight: Option<Flight>,              // Единственная страница, которую ждёт открытое окно.
    last_page: Option<Flight>,           // Параметры последней вставки для строки состояния.
}

impl HistoryLab {
    /// Создаёт вид с длинной историей у старого края.
    fn new() -> Self {
        let (history, position) = initial_history(Scene::Older);
        let mut viewport = Viewport::with_page_ahead(MAX_AHEAD);
        viewport.replace(&history, &[], None, position, Uuid::new_v4());
        Self {
            viewport,
            history,
            direction: None,
            flight: None,
            last_page: None,
        }
    }

    /// Переоткрывает фиксированную сцену и делает ответ старого запроса неприменимым.
    fn reset(&mut self, scene: Scene, cx: &mut Context<Self>) {
        let (history, position) = initial_history(scene);
        self.history = history;
        self.direction = None;
        self.flight = None;
        self.last_page = None;
        self.viewport.clear();
        self.viewport
            .replace(&self.history, &[], None, position, Uuid::new_v4());
        cx.notify();
    }

    /// Запоминает направление вертикального жеста. Саму прокрутку обрабатывает список GPUI.
    fn wheel(&mut self, scroll: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = scroll.delta.pixel_delta(px(20.)).y;
        if delta == px(0.) {
            return;
        }
        self.direction = Some(if delta > px(0.) {
            HistoryDirection::Older
        } else {
            HistoryDirection::Newer
        });
        cx.notify();
    }

    /// Запускает предлагаемую списком догрузку после измерения кадра.
    fn request_page(&mut self, cx: &mut Context<Self>) {
        if self.flight.is_some() {
            return;
        }
        let Some(need) = self.viewport.page_need(
            &self.history.older,
            &self.history.newer,
            self.direction,
            MAX_AHEAD,
        ) else {
            return;
        };
        let id = Uuid::new_v4();
        self.flight = Some(Flight { id, need });
        eprintln!(
            "history_lab op_id={id} event=page_requested direction={:?} distance_px={} rows={}",
            need.direction,
            f32::from(need.distance),
            self.history.messages.len(),
        );
        let delay = cx.background_executor().timer(PAGE_DELAY);
        cx.spawn(async move |view, cx| {
            delay.await;
            let _ = view.update(cx, |lab, cx| lab.receive_page(id, cx));
        })
        .detach();
        cx.notify();
    }

    /// Добавляет одну страницу к текущей истории и сохраняет опору к моменту ответа.
    fn receive_page(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(flight) = self.flight.filter(|flight| flight.id == id) else {
            return;
        };
        self.flight = None;
        let position = if self.history.newer == HistoryEdge::End && self.viewport.at_tail() {
            Position::Tail
        } else {
            Position::Reading
        };
        extend_history(&mut self.history, flight.need.direction);
        self.viewport
            .replace(&self.history, &[], None, position, id);
        self.last_page = Some(flight);
        eprintln!(
            "history_lab op_id={id} event=page_applied direction={:?} rows={}",
            flight.need.direction,
            self.history.messages.len(),
        );
        cx.notify();
    }

    /// Показывает число загруженных строк и расстояние, с которого запрошена последняя страница.
    fn status(&self) -> String {
        let progress = self.flight.or(self.last_page).map_or_else(
            || "Прокрутите историю к ближайшему краю".into(),
            |flight| {
                let phase = if self.flight.is_some() {
                    "Загрузка"
                } else {
                    "Вставлено"
                };
                let side = match flight.need.direction {
                    HistoryDirection::Older => "сверху",
                    HistoryDirection::Newer => "снизу",
                };
                let distance = f32::from(flight.need.distance);
                format!("{phase} {side} · запрос за {distance:.0} px до края")
            },
        );
        format!(
            "{} из {HISTORY_SIZE} · {progress}",
            self.history.messages.len()
        )
    }
}

impl Render for HistoryLab {
    /// Рисует управление и список, затем даёт хосту проверить запрос по готовой геометрии.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::Light);
        let target = cx.entity().downgrade();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.background)
            .child(
                div()
                    .flex_none()
                    .bg(palette.surface)
                    .p_3()
                    .child(div().flex().gap_2().children([
                        scene_button("history-older", "К старому краю", Scene::Older, palette, cx),
                        scene_button("history-newer", "К новому краю", Scene::Newer, palette, cx),
                        scene_button(
                            "history-short",
                            "Короткий хвост",
                            Scene::ShortTail,
                            palette,
                            cx,
                        ),
                    ]))
                    .child(
                        label(palette, TextRole::Body, self.status())
                            .mt_2()
                            .truncate(),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .on_scroll_wheel(cx.listener(Self::wheel))
                    .child(self.viewport.render_standalone(palette, "Собеседник".into()))
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |_, (), _, app| {
                                let target = target.clone();
                                app.defer(move |app| {
                                    let _ = target.update(app, HistoryLab::request_page);
                                });
                            },
                        )
                        .absolute()
                        .inset_0(),
                    ),
            )
    }
}

/// Перезапускает выбранную сцену из шапки стенда.
fn scene_button(
    id: &'static str,
    text: &'static str,
    scene: Scene,
    palette: Palette,
    cx: &Context<HistoryLab>,
) -> impl IntoElement {
    let target = cx.entity().downgrade();
    Button::new(id, text)
        .render(palette, None, move |_, app| {
            let _ = target.update(app, |lab, cx| lab.reset(scene, cx));
        })
        .debug_selector(move || id.to_owned())
}

/// Создаёт начальное окно с маркерами на доступных краях.
fn initial_history(scene: Scene) -> (HistoryWindow, Position) {
    let (start, end, position) = match scene {
        Scene::Older => (FIRST, LAST, Position::Around(Uuid::from_u128(FIRST + 22))),
        Scene::Newer => (FIRST, LAST, Position::Around(Uuid::from_u128(LAST - 26))),
        Scene::ShortTail => (HISTORY_SIZE - 2, HISTORY_SIZE, Position::Tail),
    };
    (
        HistoryWindow {
            id: 1,
            scope: SCOPE,
            messages: fixture_rows(SCOPE, start, end),
            older: HistoryEdge::More(start.to_string()),
            newer: fixture_edge(end, HISTORY_SIZE),
            needs_refresh: false,
        },
        position,
    )
}

/// Выдаёт маркер текущей границы или подтверждённый конец синтетической истории.
fn fixture_edge(message: u128, last: u128) -> HistoryEdge {
    if message == last {
        HistoryEdge::End
    } else {
        HistoryEdge::More(message.to_string())
    }
}

/// Добавляет соседнюю страницу фикстуры без пересоздания уже загруженных сообщений.
fn extend_history(history: &mut HistoryWindow, direction: HistoryDirection) {
    match direction {
        HistoryDirection::Older => {
            let first = history.messages[0].id.as_u128();
            let start = first.saturating_sub(PAGE_SIZE).max(1);
            history
                .messages
                .splice(0..0, fixture_rows(SCOPE, start, first - 1));
            history.older = fixture_edge(start, 1);
        }
        HistoryDirection::Newer => {
            let last = history.messages.last().unwrap().id.as_u128();
            let end = (last + PAGE_SIZE).min(HISTORY_SIZE);
            history.messages.extend(fixture_rows(SCOPE, last + 1, end));
            history.newer = fixture_edge(end, HISTORY_SIZE);
        }
    }
}

/// Открывает стенд на macOS и завершает приложение при закрытии окна.
pub fn run() {
    Application::new()
        .with_assets(WorkspaceAssets)
        .run(|cx: &mut App| {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(760.), px(560.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Workspace · Стенд истории".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |_, cx| cx.new(|_| HistoryLab::new()),
            )
            .expect("Не удалось открыть стенд истории GPUI");
            cx.activate(true);
        });
}

#[cfg(all(test, feature = "gpui-test"))]
#[path = "lab_tests.rs"]
mod tests;
