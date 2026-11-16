//! Показывает историю личного чата и измеряет положение сообщения из уведомления.

use super::{
    Message, NotificationTarget, OpenHistoryError, SessionGeneration, history_input::HistoryInput,
};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use iced::advanced::widget::{Id, Operation, operation::Outcome};
use iced::widget::{column, container, scrollable, text};
use iced::{Element, Fill, Rectangle, Task};
use std::collections::HashMap;
use std::sync::Arc;
use workspace_core::storage::HistoryWindow;

/// Маршрут истории вместе с состоянием запроса.
pub(super) struct HistoryScreen {
    pub(super) target: NotificationTarget, // Владелец, чат и UUID сообщения.
    pub(super) generation: SessionGeneration, // Вход, для которого открыта история.
    pub(super) state: HistoryState,        // Загрузка, подтверждённое окно или ошибка.
    pub(super) older_pending: bool,        // Измерение или запрос старой страницы уже выполняется.
    pub(super) older_failed: bool,         // Последняя попытка завершилась ошибкой.
}

/// Состояния, которые пользователь видит вместо каталога чатов.
pub(super) enum HistoryState {
    Loading,
    Ready(Arc<HistoryWindow>),     // Сообщения подтверждённого окна.
    Failed(Arc<OpenHistoryError>), // Причина, по которой историю не показали.
}

impl HistoryScreen {
    /// Рисует сообщения в порядке сервера и помечает цель для прокрутки.
    pub(super) fn view(&self, revision: u64) -> Element<'_, Message> {
        let heading = text("Личный чат").size(18);
        let body: Element<'_, Message> = match &self.state {
            HistoryState::Loading => text("Загрузка истории…").into(),
            HistoryState::Failed(error) => text(match error.as_ref() {
                OpenHistoryError::AccessLost => "Чат недоступен",
                OpenHistoryError::MessageMissing => "Сообщение недоступно",
                _ => "Не удалось загрузить историю",
            })
            .into(),
            HistoryState::Ready(window) => {
                let messages = history_messages(window, revision, &Local);
                let list = scrollable(messages)
                    .id(list_id(revision))
                    .height(Fill)
                    .into();
                let list = Element::new(HistoryInput::new(list, revision));
                let error = if self.older_failed {
                    "Не удалось загрузить предыдущие сообщения. Прокрутите вверх для повтора."
                } else {
                    ""
                };
                column![text(error), list].height(Fill).into()
            }
        };
        column![heading, body].spacing(8).height(Fill).into()
    }
}

/// Собирает строки и подписи дней, не меняя идентификаторы сообщений.
fn history_messages<'a, T: TimeZone>(
    window: &'a HistoryWindow,
    revision: u64,
    zone: &T,
) -> Element<'a, Message> {
    let mut messages = column![].spacing(8);
    let mut previous_day = None;
    for message in &window.messages {
        let day = local_day(&message.metadata.created_at, zone);
        if day != previous_day {
            if let Some(day) = day {
                messages = messages.push(crate::ui_kit::day_divider(
                    day.format("%d.%m.%Y").to_string(),
                ));
            }
            previous_day = day;
        }
        messages = messages.push(
            container(text(&message.text))
                .padding(8)
                .width(Fill)
                .id(row_id(revision, message.id)),
        );
    }
    messages.into()
}

/// Определяет календарный день сообщения в часовом поясе читателя.
fn local_day<T: TimeZone>(
    time: &workspace_core::storage::HistoryTime,
    zone: &T,
) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(time.as_str())
        .ok()
        .map(|value| value.with_timezone(zone).date_naive())
}

#[cfg(test)]
mod day_tests {
    use super::*;
    use chrono::FixedOffset;
    use iced::advanced::widget::Operation;
    use iced::{Font, Pixels, Size};
    use iced_runtime::user_interface::{Cache, UserInterface};
    use workspace_core::storage::HistoryTime;
    use workspace_core::storage::{
        HistoryEdge, HistoryMessage, HistoryMetadata, HistoryScope, HistoryWindow,
    };

    /// Граница дня определяется часовым поясом, а не датой UTC.
    #[test]
    fn groups_by_local_calendar_day() {
        let zone = FixedOffset::east_opt(3 * 3600).unwrap();
        let day = |value| local_day(&HistoryTime::parse(value).unwrap(), &zone).unwrap();
        assert_eq!(day("2026-01-01T22:00:00Z"), day("2026-01-02T01:00:00Z"));
        assert_ne!(day("2026-01-01T20:00:00Z"), day("2026-01-01T22:00:00Z"));
    }

    /// Проверяет подписи, порядок и идентификатор строки в разметке Iced.
    #[test]
    fn renders_dividers_and_keeps_message_anchor() {
        let zone = FixedOffset::east_opt(3 * 3600).unwrap();
        let stream = uuid::Uuid::from_u128(1);
        let mut window = HistoryWindow {
            id: 1,
            scope: HistoryScope {
                stream_id: stream,
                topic_id: None,
            },
            messages: Vec::new(),
            older: HistoryEdge::End,
            newer: HistoryEdge::End,
            needs_refresh: false,
        };
        let mut renderer = iced::Renderer::new(Font::DEFAULT, Pixels(16.0));
        assert!(render(&window, &zone, &mut renderer).0.is_empty());
        for (id, timestamp, body) in [
            (2, "2026-01-01T20:00:00Z", "Первое"),
            (3, "2026-01-01T22:00:00Z", "Второе"),
            (4, "2026-01-02T01:00:00Z", "Третье"),
        ] {
            window.messages.push(HistoryMessage {
                id: uuid::Uuid::from_u128(id),
                stream_id: stream,
                topic_id: stream,
                text: body.into(),
                metadata: HistoryMetadata {
                    author_id: None,
                    created_at: HistoryTime::parse(timestamp).unwrap(),
                    updated_at: None,
                },
            });
        }
        let (labels, anchored) = render(&window, &zone, &mut renderer);
        assert_eq!(
            labels,
            ["01.01.2026", "Первое", "02.01.2026", "Второе", "Третье"]
        );
        assert!(anchored);
    }

    /// Читает реальные текстовые виджеты и наличие ID цели после разметки.
    fn render(
        window: &HistoryWindow,
        zone: &FixedOffset,
        renderer: &mut iced::Renderer,
    ) -> (Vec<String>, bool) {
        #[derive(Default)]
        struct Observed {
            labels: Vec<String>,
            anchor: bool,
        }
        impl Operation for Observed {
            /// Обходит дочерние виджеты списка.
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            /// Запоминает видимые подписи.
            fn text(&mut self, _: Option<&Id>, _: Rectangle, value: &str) {
                self.labels.push(value.into());
            }
            /// Проверяет ID второй строки.
            fn container(&mut self, id: Option<&Id>, _: Rectangle) {
                self.anchor |= id == Some(&row_id(7, uuid::Uuid::from_u128(3)));
            }
        }
        let mut ui = UserInterface::build(
            history_messages(window, 7, zone),
            Size::new(620.0, 500.0),
            Cache::new(),
            renderer,
        );
        let mut observed = Observed::default();
        ui.operate(renderer, &mut observed);
        (observed.labels, observed.anchor)
    }
}

/// Различает списки разных переходов, чтобы отложенная прокрутка не задела новый экран.
pub(super) fn list_id(revision: u64) -> Id {
    Id::from(format!("history-list-{revision}"))
}

/// Даёт каждой строке стабильный ID внутри открытого списка.
fn row_id(revision: u64, id: uuid::Uuid) -> Id {
    Id::from(format!("history-row-{revision}-{id}"))
}

/// Запрашивает фактические координаты после разметки строк переменной высоты.
pub(super) fn measure_anchor(revision: u64, id: uuid::Uuid) -> Task<Message> {
    iced_runtime::task::widget(MeasureAnchor {
        list: list_id(revision),
        anchor: row_id(revision, id),
        list_bounds: None,
        anchor_bounds: None,
    })
    .map(move |offset| Message::ScrollToHistory { revision, offset })
}

/// Видимая строка и её отступ относительно верхней границы списка.
#[derive(Clone, Debug)]
pub struct HistoryPosition {
    pub(super) id: uuid::Uuid, // UUID первой видимой строки.
    pub(super) offset: f32,    // Положение строки внутри области просмотра в пикселях.
    pub(super) scroll: f32,    // Текущая абсолютная прокрутка в пикселях.
}

/// Измеряет текущую позицию или положение определённой строки после вставки.
pub(super) fn measure_position(
    revision: u64,
    ids: Vec<uuid::Uuid>,
    requested: Option<uuid::Uuid>,
) -> Task<Option<HistoryPosition>> {
    iced_runtime::task::widget(MeasurePosition {
        list: list_id(revision),
        rows: ids
            .into_iter()
            .map(|id| (row_id(revision, id), id))
            .collect(),
        requested,
        viewport: None,
        found: Vec::new(),
    })
}

/// Собирает геометрию списка и строк из одной разметки.
struct MeasurePosition {
    list: Id,                            // ID открытого списка.
    rows: HashMap<Id, uuid::Uuid>,       // ID виджетов и соответствующие UUID.
    requested: Option<uuid::Uuid>,       // Конкретная строка после вставки или первая видимая.
    viewport: Option<(Rectangle, f32)>,  // Область и сдвиг её содержимого.
    found: Vec<(uuid::Uuid, Rectangle)>, // Координаты найденных строк.
}

impl Operation<Option<HistoryPosition>> for MeasurePosition {
    /// Обходит виджеты открытого списка.
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<Option<HistoryPosition>>)) {
        visit(self);
    }
    /// Запоминает координаты строк истории.
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if let Some(uuid) = id.and_then(|id| self.rows.get(id)) {
            self.found.push((*uuid, bounds));
        }
    }
    /// Запоминает видимую область и абсолютный сдвиг.
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        viewport: Rectangle,
        _: Rectangle,
        translation: iced::Vector,
        _: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        if id == Some(&self.list) {
            self.viewport = Some((viewport, translation.y));
        }
    }
    /// Возвращает первую видимую строку либо координаты запрошенной строки.
    fn finish(&self) -> Outcome<Option<HistoryPosition>> {
        Outcome::Some(self.viewport.and_then(|(viewport, scroll)| {
            self.found
                .iter()
                .find(|(id, bounds)| {
                    self.requested.map_or(
                        bounds.y + bounds.height - scroll > viewport.y
                            && bounds.y - scroll < viewport.y + viewport.height,
                        |requested| *id == requested,
                    )
                })
                .map(|(id, bounds)| HistoryPosition {
                    id: *id,
                    offset: bounds.y - scroll - viewport.y,
                    scroll,
                })
        }))
    }
}

/// Собирает координаты списка и строки в одном обходе разметки.
struct MeasureAnchor {
    list: Id,                         // Список, который нужно прокрутить.
    anchor: Id,                       // Строка целевого сообщения.
    list_bounds: Option<(f32, f32)>,  // Верх содержимого и высота видимой области.
    anchor_bounds: Option<Rectangle>, // Положение и размер строки.
}

impl Operation<Option<f32>> for MeasureAnchor {
    /// Обходит дочерние виджеты до контейнера сообщения.
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<Option<f32>>)) {
        visit(self);
    }

    /// Запоминает верх строки цели.
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if id == Some(&self.anchor) {
            self.anchor_bounds = Some(bounds);
        }
    }

    /// Запоминает верх всего содержимого списка.
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        viewport: Rectangle,
        content: Rectangle,
        _: iced::Vector,
        _: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        if id == Some(&self.list) {
            self.list_bounds = Some((content.y, viewport.height));
        }
    }

    /// Возвращает расстояние от начала истории до строки.
    fn finish(&self) -> Outcome<Option<f32>> {
        Outcome::Some(
            self.anchor_bounds
                .zip(self.list_bounds)
                .map(|(anchor, (top, height))| {
                    // Запас сверху оставляет цель видимой при округлении координат Iced.
                    (anchor.y - top - ((height - anchor.height) / 2.0).max(0.0)).max(0.0)
                }),
        )
    }
}
