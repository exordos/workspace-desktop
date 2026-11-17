//! Показывает историю разговора и измеряет положение сообщения из уведомления.

use super::composer::{Address, ConversationSend, pending_bubble};
use super::{
    ChatEntry, ChatRoute, Message, OpenHistoryError, SessionGeneration, history_input::HistoryInput,
};
use crate::ui_kit::{ButtonVariant, MessageDelivery, MessageDirection, button, message_bubble};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use iced::advanced::widget::{Id, Operation, operation::Outcome};
use iced::widget::{column, container, row, scrollable, text};
use iced::{Element, Fill, Rectangle, Task};
use std::sync::Arc;
use workspace_core::storage::{HistoryEdge, HistoryWindow, OutboxEntry};

/// Маршрут истории вместе с состоянием запроса.
pub(super) struct HistoryScreen {
    pub(super) route: ChatRoute, // Владелец, область и место открытия истории.
    pub(super) generation: SessionGeneration, // Вход, для которого открыта история.
    pub(super) state: HistoryState, // Загрузка, подтверждённое окно или ошибка.
    pub(super) send: ConversationSend, // Редактор и доставка только этого разговора.
    pub(super) refreshing: bool, // Свежая страница хвоста ещё загружается.
    pub(super) refresh_failed: bool, // Свежая страница не пришла, сохранённое окно пригодно.
    pub(super) user_scrolled: bool, // Читатель уже изменил позицию сохранённого хвоста.
    pub(super) older_pending: bool, // Измерение или запрос старой страницы уже выполняется.
    pub(super) older_failed: bool, // Последняя попытка завершилась ошибкой.
    pub(super) scroll_revision: u64, // Меняется при ручном движении в любую сторону.
    pub(super) layout: LayoutUpdate, // Подготовленная перестройка и её стадия.
}

/// Хранит самое новое состояние, пока Iced измеряет текущую разметку.
#[derive(Default)]
pub(super) struct LayoutUpdate {
    pub(super) revision: u64,   // Номер измерения в открытом разговоре.
    pub(super) measuring: bool, // Ещё не закончена пара замеров.
    pub(super) pending: Option<HistoryPatch>, // Новая разметка ждёт применения.
}

/// Новый снимок строк истории и очереди, полученный для того же открытого окна.
pub(super) struct HistoryPatch {
    pub(super) window: Arc<HistoryWindow>, // Только подтверждённые данные из БД.
    pub(super) outbox: Vec<OutboxEntry>,   // Ожидающие строки того же владельца.
    pub(super) older_completed: bool,      // Можно добавить уже загруженную старую страницу.
}

/// Состояния, которые пользователь видит вместо каталога чатов.
pub(super) enum HistoryState {
    Loading,
    Ready(Arc<HistoryWindow>),     // Сообщения подтверждённого окна.
    Failed(Arc<OpenHistoryError>), // Причина, по которой историю не показали.
}

impl HistoryScreen {
    /// Начинает загрузку для выбранного маршрута, скрывая сообщения предыдущего экрана.
    pub(super) fn loading(route: ChatRoute, generation: SessionGeneration) -> Self {
        Self {
            refreshing: route.entry == ChatEntry::Latest,
            route,
            generation,
            state: HistoryState::Loading,
            send: ConversationSend::default(),
            refresh_failed: false,
            user_scrolled: false,
            older_pending: false,
            older_failed: false,
            scroll_revision: 0,
            layout: LayoutUpdate::default(),
        }
    }

    /// Скрывает историю и останавливает действия, относящиеся к её окну.
    pub(super) fn fail(&mut self, error: Arc<OpenHistoryError>) {
        if matches!(
            *error,
            OpenHistoryError::AccessLost
                | OpenHistoryError::MessageMissing
                | OpenHistoryError::InvalidTarget
        ) {
            self.send = ConversationSend::default();
        }
        self.state = HistoryState::Failed(error);
        self.refreshing = false;
        self.refresh_failed = false;
        self.older_pending = false;
        self.older_failed = false;
        self.cancel_layout_update();
    }

    /// Отбрасывает измерения прежнего окна истории.
    pub(super) fn cancel_layout_update(&mut self) {
        self.layout.revision = self.layout.revision.wrapping_add(1);
        self.layout.measuring = false;
        self.layout.pending = None;
    }

    /// Возвращает стабильные пары виджетов и UUID для фактического измерения списка.
    pub(super) fn layout_rows(&self, revision: u64) -> Vec<(Id, uuid::Uuid)> {
        let HistoryState::Ready(window) = &self.state else {
            return Vec::new();
        };
        let mut ids = window
            .messages
            .iter()
            .map(|line| line.id)
            .collect::<Vec<_>>();
        let confirmed = ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        ids.extend(
            self.send
                .outbox
                .iter()
                .map(|entry| entry.snapshot.public_id)
                .filter(|id| !confirmed.contains(id)),
        );
        ids.into_iter()
            .map(|id| (row_id(revision, id), id))
            .collect()
    }

    /// Разрешает повтор, если история не открылась или не обновилась из-за сбоя.
    pub(super) fn can_retry(&self) -> bool {
        self.refresh_failed
            || matches!(&self.state, HistoryState::Failed(error) if !matches!(
                error.as_ref(),
                OpenHistoryError::AccessLost | OpenHistoryError::MessageMissing | OpenHistoryError::InvalidTarget
            ))
    }

    /// Рисует сообщения в порядке сервера и помечает цель для прокрутки.
    pub(super) fn view<'a>(&'a self, revision: u64, title: &'a str) -> Element<'a, Message> {
        let mut heading = row![text(title).size(18).wrapping(text::Wrapping::WordOrGlyph)]
            .spacing(8)
            .align_y(iced::Center);
        if let HistoryState::Ready(window) = &self.state
            && window.newer != HistoryEdge::End
        {
            heading = heading.push(button(
                "К последним сообщениям",
                ButtonVariant::Neutral,
                Some(Message::SeeLatest(revision)),
            ));
        }
        let body: Element<'_, Message> = match &self.state {
            HistoryState::Loading => text("Загрузка истории…").into(),
            HistoryState::Failed(error) => text(match error.as_ref() {
                OpenHistoryError::AccessLost if self.route.scope.topic_id.is_some() => {
                    "Тема недоступна"
                }
                OpenHistoryError::AccessLost => "Чат недоступен",
                OpenHistoryError::MessageMissing => "Сообщение недоступно",
                _ => "Не удалось загрузить историю",
            })
            .into(),
            HistoryState::Ready(window) => {
                let messages = if window.messages.is_empty() && self.send.outbox.is_empty() {
                    text("В чате пока нет сообщений").into()
                } else {
                    history_messages(
                        window,
                        &self.send.outbox,
                        self.route.owner.user_id.parse().ok(),
                        revision,
                        &Local,
                    )
                };
                let list = scrollable(messages)
                    .id(list_id(revision))
                    .height(Fill)
                    .into();
                let list = Element::new(HistoryInput::new(list, revision));
                let mut body = column![].spacing(8).height(Fill);
                if self.older_failed {
                    body = body.push(text(
                        "Не удалось загрузить предыдущие сообщения. Прокрутите вверх для повтора.",
                    ));
                }
                if self.refresh_failed {
                    body = body.push(text("Не удалось обновить историю"));
                }
                body.push(list).into()
            }
        };
        let mut content = column![heading, body].spacing(8).height(Fill);
        if self.can_retry() {
            content = content.push(button(
                "Повторить",
                ButtonVariant::Neutral,
                Some(Message::RetryHistory(revision)),
            ));
        }
        if matches!(self.state, HistoryState::Ready(_)) && self.route.scope.topic_id.is_none() {
            content = content.push(self.send.composer.view(
                self.send.address.unavailable(),
                move |action| Message::EditConversationText { revision, action },
                Message::SendConversationText(revision),
            ));
            if matches!(self.send.address, Address::Unavailable(_)) {
                content = content.push(button(
                    "Проверить отправку",
                    ButtonVariant::Neutral,
                    Some(Message::RetrySendAddress(revision)),
                ));
            }
        }
        content.into()
    }
}

/// Собирает строки и подписи дней, не меняя идентификаторы сообщений.
fn history_messages<'a, T: TimeZone>(
    window: &'a HistoryWindow,
    outbox: &'a [OutboxEntry],
    own_author: Option<uuid::Uuid>,
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
        let line = if own_author.is_some() && message.metadata.author_id == own_author {
            container(message_bubble(
                &message.text,
                None,
                "",
                MessageDirection::Outgoing(Some(MessageDelivery::Sent)),
            ))
            .align_right(Fill)
        } else {
            container(text(&message.text).wrapping(text::Wrapping::WordOrGlyph)).width(Fill)
        };
        messages = messages.push(line.padding(8).id(row_id(revision, message.id)));
    }
    for entry in outbox {
        let id = entry.snapshot.public_id;
        // История может уже содержать подтверждение, пока окно ещё не перечитало очередь.
        if !window.messages.iter().any(|message| message.id == id) {
            messages = messages.push(
                container(pending_bubble(entry))
                    .align_right(Fill)
                    .padding(8)
                    .id(row_id(revision, id)),
            );
        }
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
            history_messages(window, &[], None, 7, zone),
            Size::new(620.0, 500.0),
            Cache::new(),
            renderer,
        );
        let mut observed = Observed::default();
        ui.operate(renderer, &mut observed);
        (observed.labels, observed.anchor)
    }
}
