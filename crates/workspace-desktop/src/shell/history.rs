//! Показывает историю личного чата и измеряет положение сообщения из уведомления.

use super::{Message, NotificationTarget, OpenHistoryError, SessionGeneration};
use iced::advanced::widget::{Id, Operation, operation::Outcome};
use iced::widget::{column, container, scrollable, text};
use iced::{Element, Fill, Rectangle, Task};
use std::sync::Arc;
use workspace_core::storage::HistoryWindow;

/// Маршрут истории вместе с состоянием запроса.
pub(super) struct HistoryScreen {
    pub(super) target: NotificationTarget, // Владелец, чат и UUID сообщения.
    pub(super) generation: SessionGeneration, // Вход, для которого открыта история.
    pub(super) state: HistoryState,        // Загрузка, подтверждённое окно или ошибка.
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
                    let mut row = container(text(&message.text)).padding(8).width(Fill);
                    if self.target.message_id.parse() == Ok(message.id) {
                        row = row.id(anchor_id(revision));
                    }
                    messages = messages.push(row);
                }
                scrollable(messages)
                    .id(list_id(revision))
                    .height(Fill)
                    .into()
            }
        };
        column![heading, body].spacing(8).height(Fill).into()
    }
}

/// Различает списки разных переходов, чтобы отложенная прокрутка не задела новый экран.
pub(super) fn list_id(revision: u64) -> Id {
    Id::from(format!("history-list-{revision}"))
}

/// Помечает единственную строку с UUID цели.
fn anchor_id(revision: u64) -> Id {
    Id::from(format!("history-anchor-{revision}"))
}

/// Запрашивает фактические координаты после разметки строк переменной высоты.
pub(super) fn measure_anchor(revision: u64) -> Task<Message> {
    iced_runtime::task::widget(MeasureAnchor {
        list: list_id(revision),
        anchor: anchor_id(revision),
        list_bounds: None,
        anchor_bounds: None,
    })
    .map(move |offset| Message::ScrollToHistory { revision, offset })
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
