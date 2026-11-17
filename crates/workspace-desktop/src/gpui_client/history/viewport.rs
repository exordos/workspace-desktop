//! Рисует виртуальную историю и восстанавливает опору по UUID после изменения состава строк.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use gpui::{
    Bounds, IntoElement, ListAlignment, ListOffset, ListState, Pixels, canvas, div, list,
    prelude::*, px,
};
use uuid::Uuid;
use workspace_core::storage::{DeliveryStatus, HistoryWindow, OutboxEntry, SendBlock};

use crate::gpui_client::ui::{DeliveryMark, MessageBubble, Palette};

/// Строка, которую можно рисовать после переноса снимка на поток UI.
#[derive(Clone, Eq, PartialEq)]
struct Row {
    id: Uuid,                       // UUID сообщения или публичный UUID очереди.
    own: bool,                      // Сообщение текущего пользователя.
    body: String,                   // Текст, полученный из истории или очереди.
    time: Option<String>,           // Время сервера или пояснение рядом со статусом очереди.
    delivery: Option<DeliveryMark>, // Состояние отправки исходящей строки.
}

/// Что должен показать список после принятия снимка.
pub(super) enum Position {
    Tail,         // Перейти к последней строке.
    Around(Uuid), // Поставить указанное сообщение в начало видимой истории.
    Reading,      // Сохранить верхнюю видимую строку и её смещение.
}

/// Видимая относительная координата строки и верхний отступ.
#[derive(Clone, Copy)]
struct Anchor {
    id: Uuid,       // Идентификатор строки, которую продолжают читать.
    y: Pixels,      // Координата верхней границы относительно списка, в логических px.
    offset: Pixels, // Сколько строки скрыто над верхним краем списка.
}

/// Запись для сравнения координат после нового прохода GPUI.
#[derive(Clone, Copy)]
struct Measurement {
    op_id: Uuid,    // Операция, которая поменяла список.
    anchor: Anchor, // Положение той же строки до замены снимка.
}

/// Снимок виртуального списка и реально нарисованные границы последнего кадра.
pub(super) struct Viewport {
    rows: Rc<Vec<Row>>, // Состав списка для отложенного построения видимых элементов.
    list: ListState,    // Позиция и размеры строк в GPUI.
    painted: Rc<RefCell<HashMap<Uuid, Bounds<Pixels>>>>, // Фактические прямоугольники строк.
    measure: Option<Measurement>, // Следующее измерение после замены снимка.
}

impl Viewport {
    /// Создаёт пустой список с нижней привязкой и ограниченной областью дополнительной отрисовки.
    pub(super) fn new() -> Self {
        Self {
            rows: Rc::default(),
            list: ListState::new(0, ListAlignment::Bottom, px(100.)),
            painted: Rc::default(),
            measure: None,
        }
    }

    /// Возвращает состояние для подключения обработчика прокрутки.
    pub(super) fn list(&self) -> &ListState {
        &self.list
    }

    /// Проверяет, читает ли пользователь хвост, к которому можно прикрепить новые строки.
    pub(super) fn at_tail(&self) -> bool {
        self.list.logical_scroll_top().item_ix == self.rows.len()
            || self
                .rows
                .last()
                .and_then(|row| self.painted.borrow().get(&row.id).copied())
                .is_some_and(|last| last.bottom() <= self.list.viewport_bounds().bottom() + px(2.))
    }

    /// Очищает список и координаты доступных раньше сообщений.
    pub(super) fn clear(&mut self) {
        self.rows = Rc::default();
        self.list.reset(0);
        self.painted.borrow_mut().clear();
        self.measure = None;
    }

    /// Берёт верхнюю видимую строку и сохраняет её смещение до замены данных.
    fn anchor(&self, index: &HashMap<Uuid, usize>) -> Option<Anchor> {
        let viewport = self.list.viewport_bounds();
        let logical = self.list.logical_scroll_top();
        let painted = self.painted.borrow();
        if let Some(row) = self
            .rows
            .get(logical.item_ix)
            .filter(|row| index.contains_key(&row.id))
        {
            let y = painted
                .get(&row.id)
                .map_or(-logical.offset_in_item, |bounds| {
                    bounds.top() - viewport.top()
                });
            return Some(Anchor {
                id: row.id,
                y,
                offset: logical.offset_in_item,
            });
        }
        self.rows
            .iter()
            .filter(|row| index.contains_key(&row.id))
            .find_map(|row| {
                let bounds = painted.get(&row.id)?;
                (bounds.bottom() > viewport.top() && bounds.top() < viewport.bottom()).then(|| {
                    let y = bounds.top() - viewport.top();
                    Anchor {
                        id: row.id,
                        y,
                        offset: -y,
                    }
                })
            })
    }

    /// Обновляет только изменившийся отрезок и переносит опору в его новый индекс.
    pub(super) fn replace(
        &mut self,
        history: &HistoryWindow,
        outbox: &[OutboxEntry],
        user: Option<Uuid>,
        position: Position,
        op_id: Uuid,
    ) {
        let rows = build_rows(history, outbox, user);
        let previous_count = self.rows.len();
        let index: HashMap<_, _> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| (row.id, i))
            .collect();
        let anchor = matches!(position, Position::Reading)
            .then(|| self.anchor(&index))
            .flatten();
        let prefix = self
            .rows
            .iter()
            .zip(&rows)
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = self.rows[prefix..]
            .iter()
            .rev()
            .zip(rows[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        if prefix != self.rows.len() || prefix != rows.len() {
            self.list.splice(
                prefix..self.rows.len() - suffix,
                rows.len() - prefix - suffix,
            );
            self.rows = Rc::new(rows);
        }
        self.measure = anchor.map(|anchor| Measurement { op_id, anchor });
        log::debug!(
            "gpui_history op_id={op_id} event=rows_applied rows_before={previous_count} rows_after={} row_count_delta={} position={}",
            self.rows.len(),
            self.rows.len() as i64 - previous_count as i64,
            match position {
                Position::Tail => "tail",
                Position::Around(_) => "target",
                Position::Reading => "reading",
            }
        );
        match position {
            Position::Tail => self.list.scroll_to(ListOffset {
                item_ix: self.rows.len(),
                offset_in_item: px(0.),
            }),
            Position::Around(id) => {
                if let Some(&item_ix) = index.get(&id) {
                    self.list.scroll_to(ListOffset {
                        item_ix,
                        offset_in_item: px(0.),
                    });
                }
            }
            Position::Reading => {
                if let Some(anchor) = anchor {
                    self.list.scroll_to(ListOffset {
                        item_ix: index[&anchor.id],
                        offset_in_item: anchor.offset,
                    });
                }
            }
        }
    }

    /// Рисует список и пишет фактическую координату опоры, когда GPUI дойдёт до её строки.
    pub(super) fn render(&mut self, palette: Palette) -> impl IntoElement {
        self.painted.borrow_mut().clear();
        let rows = self.rows.clone();
        let painted = self.painted.clone();
        let measurement = Rc::new(RefCell::new(self.measure.take()));
        let list_state = self.list.clone();
        list(self.list.clone(), move |index, _, _| {
            let row = &rows[index];
            let id = row.id;
            let painted = painted.clone();
            let measurement = measurement.clone();
            let list_state = list_state.clone();
            div().relative().w_full().px_3().py_1()
                .child(MessageBubble {
                    author: None,
                    body: row.body.clone().into(),
                    time: row.time.clone().map(Into::into),
                    own: row.own,
                    delivery: row.delivery,
                }.render(palette)
                    .when(row.delivery == Some(DeliveryMark::Unknown), |bubble| {
                        bubble.debug_selector(|| "history-delivery-unknown".into())
                    }))
                .child(canvas(|_, _, _| (), move |bounds, (), _, _| {
                    painted.borrow_mut().insert(id, bounds);
                    let mut measurement = measurement.borrow_mut();
                    if measurement.as_ref().is_some_and(|m| m.anchor.id == id)
                        && let Some(m) = measurement.take()
                    {
                        let y_after = bounds.top() - list_state.viewport_bounds().top();
                        log::debug!("gpui_history op_id={} event=anchor_measured anchor_uuid={id} y_before_px={} y_after_px={} target_inset_px={} observed_inset_px={} delta_px={}",
                            m.op_id, f32::from(m.anchor.y), f32::from(y_after), f32::from(m.anchor.offset), f32::from(-y_after), f32::from(y_after - m.anchor.y));
                    }
                }).absolute().inset_0())
                .into_any_element()
        }).flex_1().min_h_0().w_full()
    }

    /// Возвращает координату реально показанной строки для теста геометрии.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn y(&self, id: Uuid) -> Option<f32> {
        self.painted
            .borrow()
            .get(&id)
            .map(|bounds| f32::from(bounds.top() - self.list.viewport_bounds().top()))
    }
}

/// Готовит историю и добавляет уникальные строки очереди только после подтверждённого хвоста.
fn build_rows(history: &HistoryWindow, outbox: &[OutboxEntry], user: Option<Uuid>) -> Vec<Row> {
    let mut rows: Vec<_> = history
        .messages
        .iter()
        .map(|message| {
            let own = user.is_some() && message.metadata.author_id == user;
            Row {
                id: message.id,
                own,
                body: message.text.clone(),
                time: message
                    .metadata
                    .created_at
                    .as_str()
                    .get(11..16)
                    .map(str::to_owned),
                delivery: own.then_some(DeliveryMark::Sent),
            }
        })
        .collect();
    if history.newer == workspace_core::storage::HistoryEdge::End {
        let mut seen: HashSet<_> = rows.iter().map(|row| row.id).collect();
        rows.extend(
            outbox
                .iter()
                .filter(|entry| seen.insert(entry.snapshot.public_id))
                .map(|entry| Row {
                    id: entry.snapshot.public_id,
                    own: true,
                    body: entry.snapshot.markdown.clone(),
                    time: Some(delivery_caption(entry.status).into()),
                    delivery: Some(match entry.status {
                        DeliveryStatus::Waiting | DeliveryStatus::Sending => DeliveryMark::Pending,
                        DeliveryStatus::Unknown => DeliveryMark::Unknown,
                        DeliveryStatus::Blocked(_) | DeliveryStatus::Rejected(_) => {
                            DeliveryMark::Failed
                        }
                    }),
                }),
        );
    }
    rows
}

/// Подписывает временную строку до подтверждения, чтобы ожидание и неизвестный исход различались.
fn delivery_caption(status: DeliveryStatus) -> &'static str {
    match status {
        DeliveryStatus::Waiting => "В очереди",
        DeliveryStatus::Sending => "Отправляется…",
        DeliveryStatus::Unknown => "Исход неизвестен. Ищем сообщение",
        DeliveryStatus::Blocked(SendBlock::AccessLost) => "Чат или тема недоступны",
        DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable) => {
            "Отправка открытого текста недоступна"
        }
        DeliveryStatus::Rejected(_) => "Не отправлено",
    }
}
