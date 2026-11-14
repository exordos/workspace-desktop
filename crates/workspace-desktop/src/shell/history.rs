//! Показывает историю личного чата и измеряет положение сообщения из уведомления.

use super::{
    Message, NotificationTarget, OpenHistoryError, SessionGeneration, history_input::HistoryInput,
};
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
                let mut messages = column![].spacing(8);
                for message in &window.messages {
                    let row = container(text(&message.text))
                        .padding(8)
                        .width(Fill)
                        .id(row_id(revision, message.id));
                    messages = messages.push(row);
                }
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
