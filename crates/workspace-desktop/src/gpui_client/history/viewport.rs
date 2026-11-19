//! Рисует виртуальную историю и восстанавливает опору по UUID после изменения состава строк.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
};

use gpui::{
    App, Bounds, Div, ImageSource, IntoElement, ListAlignment, ListOffset, ListState, Pixels,
    WeakEntity, canvas, div, list, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    messenger::references::{MediaReference, media_references},
    storage::{DeliveryStatus, HistoryWindow, OutboxEntry, SendBlock},
};

use super::{
    Conversation, ReadingAnchor,
    attachments::{AttachmentKey, Cards},
};

#[path = "paging.rs"]
mod paging;
pub(super) use paging::PageNeed;

use crate::gpui_client::ui::{Avatar, DeliveryMark, MessageBubble, Palette, TextRole, label};

const DATE_SPACE: Pixels = px(32.);
const BOTTOM_PADDING: Pixels = px(12.);

/// Строка, которую можно рисовать после переноса снимка на поток UI.
#[derive(Clone, Eq, PartialEq)]
struct Row {
    id: Uuid,                       // UUID сообщения или публичный UUID очереди.
    own: bool,                      // Сообщение текущего пользователя.
    author: Option<Uuid>,           // UUID для группировки, неизвестные авторы не объединяются.
    topic: Uuid,                    // Смена темы начинает новую группу.
    date: Option<String>,           // День создания UTC, если сообщение принято сервером.
    body: String,                   // Текст, полученный из истории или очереди.
    media: Vec<MediaReference>,     // Прямые вложения подтверждённой строки.
    time: Option<String>,           // Время сервера или пояснение рядом со статусом очереди.
    delivery: Option<DeliveryMark>, // Состояние отправки исходящей строки.
    starts_day: bool,               // Над этой строкой виден новый день.
    starts_group: bool,             // Имя автора и отступ перед началом группы.
    ends_group: bool,               // Аватар собеседника рядом с последней строкой группы.
}

impl Row {
    /// Возвращает высоту частей строки перед сообщением, чтобы выровнять сам пузырь.
    fn inset(&self) -> Pixels {
        let gap = if self.starts_group { px(6.) } else { px(3.) };
        gap + if self.starts_day { DATE_SPACE } else { px(0.) }
    }

    /// Сравнивает авторство, тему и день двух соседних сообщений.
    fn joins(&self, other: &Self) -> bool {
        self.author.is_some()
            && self.author == other.author
            && self.topic == other.topic
            && self.date == other.date
    }

    /// Возвращает автора строки, возле которой рисуется фото входящей группы.
    fn avatar_author(&self) -> Option<Uuid> {
        self.author
            .filter(|id| !self.own && self.ends_group && !id.is_nil())
    }
}

/// Что должен показать список после принятия снимка.
pub(super) enum Position {
    Tail,                   // Перейти к последней строке.
    Around(Uuid),           // Поставить указанное сообщение в начало видимой истории.
    Reading,                // Сохранить верхнюю видимую строку и её смещение.
    Restore(ReadingAnchor), // Выставить сохранённую координату после открытия.
}

/// Запись для сравнения координат после нового прохода GPUI.
#[derive(Clone, Copy)]
struct Measurement {
    op_id: Uuid,           // Операция, которая поменяла список.
    anchor: ReadingAnchor, // Положение той же строки до замены снимка.
}

/// Снимок виртуального списка и реально нарисованные границы последнего кадра.
pub(super) struct Viewport {
    rows: Rc<Vec<Row>>, // Состав списка для отложенного построения видимых элементов.
    index: Rc<HashMap<Uuid, usize>>, // Индекс строк истории по UUID сообщения.
    list: ListState,    // Позиция и размеры строк в GPUI.
    painted: Rc<RefCell<HashMap<Uuid, Bounds<Pixels>>>>, // Фактические прямоугольники строк.
    painted_offset: Rc<Cell<Option<ListOffset>>>, // Смещение, для которого измерены прямоугольники.
    measure: Option<Measurement>, // Следующее измерение после замены снимка.
}

impl Viewport {
    /// Задаёт, на сколько логических px за пределами окна список измеряет строки.
    fn with_overdraw(overdraw: Pixels) -> Self {
        Self {
            rows: Rc::default(),
            index: Rc::default(),
            list: ListState::new(0, ListAlignment::Bottom, overdraw),
            painted: Rc::default(),
            painted_offset: Rc::default(),
            measure: None,
        }
    }

    /// Возвращает состояние для подключения обработчика прокрутки.
    pub(super) fn list(&self) -> &ListState {
        &self.list
    }

    /// Проверяет, читает ли пользователь хвост, к которому можно прикрепить новые строки.
    pub(super) fn at_tail(&self) -> bool {
        let logical = self.list.logical_scroll_top();
        logical.item_ix == self.rows.len()
            || self.paint_matches(logical)
                && self
                    .rows
                    .last()
                    .and_then(|row| self.painted.borrow().get(&row.id).copied())
                    .is_some_and(|last| {
                        last.bottom() <= self.list.viewport_bounds().bottom() + px(2.)
                    })
    }

    /// Очищает список и координаты доступных раньше сообщений.
    pub(super) fn clear(&mut self) {
        self.rows = Rc::default();
        self.index = Rc::default();
        self.list.reset(0);
        self.painted.borrow_mut().clear();
        self.painted_offset.set(None);
        self.measure = None;
    }

    /// Сверяет последний отрисованный кадр с текущим смещением списка.
    fn paint_matches(&self, offset: ListOffset) -> bool {
        self.painted_offset.get().is_some_and(|painted| {
            painted.item_ix == offset.item_ix && painted.offset_in_item == offset.offset_in_item
        })
    }

    /// Берёт верхнюю видимую строку и сохраняет её смещение до замены данных.
    fn anchor(&self, index: &HashMap<Uuid, usize>) -> Option<ReadingAnchor> {
        let viewport = self.list.viewport_bounds();
        let logical = self.list.logical_scroll_top();
        let painted = self.painted.borrow();
        if let Some(row) = self
            .rows
            .get(logical.item_ix)
            .filter(|row| index.contains_key(&row.id))
        {
            // Жест уже мог сдвинуть логическое смещение между двумя кадрами.
            let y = self
                .paint_matches(logical)
                .then(|| painted.get(&row.id))
                .flatten()
                .map_or(row.inset() - logical.offset_in_item, |bounds| {
                    bounds.top() - viewport.top()
                });
            return Some(ReadingAnchor { id: row.id, y });
        }
        self.rows
            .iter()
            .filter(|row| index.contains_key(&row.id))
            .find_map(|row| {
                let bounds = painted.get(&row.id)?;
                (bounds.bottom() > viewport.top() && bounds.top() < viewport.bottom()).then(|| {
                    let y = bounds.top() - viewport.top();
                    ReadingAnchor { id: row.id, y }
                })
            })
    }

    /// Возвращает опору, которую сервер сможет открыть по UUID.
    pub(super) fn saved_anchor(&self) -> Option<ReadingAnchor> {
        self.anchor(&self.index)
            .filter(|anchor| self.rows[self.index[&anchor.id]].date.is_some())
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
        let anchor = match position {
            Position::Reading => self.anchor(&index),
            Position::Restore(anchor) => index.contains_key(&anchor.id).then_some(anchor),
            Position::Tail | Position::Around(_) => None,
        };
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
        self.index = Rc::new(index);
        self.measure = anchor.map(|anchor| Measurement { op_id, anchor });
        log::debug!(
            "gpui_history op_id={op_id} event=rows_applied rows_before={previous_count} rows_after={} row_count_delta={} position={}",
            self.rows.len(),
            self.rows.len() as i64 - previous_count as i64,
            match position {
                Position::Tail => "tail",
                Position::Around(_) => "target",
                Position::Reading => "reading",
                Position::Restore(_) => "restored",
            }
        );
        match position {
            Position::Tail => self.list.scroll_to(ListOffset {
                item_ix: self.rows.len(),
                offset_in_item: px(0.),
            }),
            Position::Around(id) => {
                if let Some(&item_ix) = self.index.get(&id) {
                    self.list.scroll_to(ListOffset {
                        item_ix,
                        offset_in_item: self.rows[item_ix].inset(),
                    });
                }
            }
            Position::Reading | Position::Restore(_) => {
                if let Some(anchor) = anchor {
                    let item_ix = self.index[&anchor.id];
                    // При догрузке дата и отступ группы могут исчезнуть над тем же UUID.
                    self.list.scroll_to(ListOffset {
                        item_ix,
                        offset_in_item: self.rows[item_ix].inset() - anchor.y,
                    });
                }
            }
        }
    }

    /// Проверяет присутствие прямой ссылки в текущем окне разговора.
    pub(super) fn has_attachment(&self, key: AttachmentKey) -> bool {
        self.index.get(&key.message).is_some_and(|&index| {
            self.rows[index]
                .media
                .get(key.position)
                .is_some_and(|reference| reference.file_id == key.file)
        })
    }

    /// Проверяет, есть ли у карточки показанная строка, куда можно вернуть фокус.
    pub(super) fn has_visible_attachment(&self, key: AttachmentKey) -> bool {
        self.has_attachment(key)
            && self
                .painted
                .borrow()
                .get(&key.message)
                .is_some_and(|bounds| bounds.intersects(&self.list.viewport_bounds()))
    }

    /// Показывает список с локальными строками в отдельном окне.
    pub(super) fn render_standalone(&mut self, palette: Palette, peer: String) -> impl IntoElement {
        self.render(palette, Some(peer), HashMap::new(), None)
    }

    /// Рисует список и пишет фактическую координату опоры, когда GPUI дойдёт до её строки.
    pub(super) fn render(
        &mut self,
        palette: Palette,
        peer: Option<String>,
        images: HashMap<Uuid, ImageSource>,
        cards: Option<Cards>,
    ) -> impl IntoElement {
        self.painted.borrow_mut().clear();
        self.painted_offset.set(None);
        let rows = self.rows.clone();
        let painted = self.painted.clone();
        let painted_offset = self.painted_offset.clone();
        let measurement = Rc::new(RefCell::new(self.measure.take()));
        let list_state = self.list.clone();
        list(self.list.clone(), move |index, _, app| {
            let row = &rows[index];
            let id = row.id;
            let photo = row.avatar_author().and_then(|id| images.get(&id).cloned());
            let painted = painted.clone();
            let painted_offset = painted_offset.clone();
            let measurement = measurement.clone();
            let list_state = list_state.clone();
            div().relative().w_full().px_3().pt(row.inset())
                .children(row.date.as_deref().filter(|_| row.starts_day).map(|date| date_separator(palette, date, id)))
                .child(message_row(row, palette, peer.as_deref(), photo, cards.as_ref().map(|cards| (cards, app))).relative().child(canvas(|_, _, _| (), move |bounds, (), _, _| {
                    painted.borrow_mut().insert(id, bounds);
                    painted_offset.set(Some(list_state.logical_scroll_top()));
                    let mut measurement = measurement.borrow_mut();
                    if measurement.as_ref().is_some_and(|m| m.anchor.id == id)
                        && let Some(m) = measurement.take()
                    {
                        let y_after = bounds.top() - list_state.viewport_bounds().top();
                        log::debug!("gpui_history op_id={} event=anchor_measured anchor_uuid={id} y_before_px={} y_after_px={} target_inset_px={} observed_inset_px={} delta_px={}",
                            m.op_id, f32::from(m.anchor.y), f32::from(y_after), f32::from(-m.anchor.y), f32::from(-y_after), f32::from(y_after - m.anchor.y));
                    }
                }).absolute().inset_0()))
                .into_any_element()
        }).flex_1().min_h_0().w_full().pb(BOTTOM_PADDING)
    }

    /// После отрисовки строк собирает аватары тех групп, что действительно пересекли область списка.
    pub(super) fn track_visible_avatars(
        &self,
        conversation: WeakEntity<Conversation>,
        previous: &[Uuid],
    ) -> impl IntoElement {
        let rows = self.rows.clone();
        let index = self.index.clone();
        let painted = self.painted.clone();
        let list_state = self.list.clone();
        let previous = previous.to_vec();
        canvas(
            |_, _, _| (),
            move |_, (), _, app| {
                let users = visible_avatar_users(
                    &rows,
                    &index,
                    &painted.borrow(),
                    list_state.viewport_bounds(),
                );
                if users == previous {
                    return;
                }
                // Пока кадр рисовался, история могла получить другой снимок. Применяем только текущий.
                app.defer(move |app| {
                    let _ = conversation.update(app, |view, cx| {
                        if Rc::ptr_eq(&view.viewport.rows, &rows) {
                            view.update_history_avatars(users, cx);
                        }
                    });
                });
            },
        )
        .absolute()
        .inset_0()
    }

    /// Возвращает координату реально показанной строки для теста геометрии.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn y(&self, id: Uuid) -> Option<f32> {
        self.painted
            .borrow()
            .get(&id)
            .map(|bounds| f32::from(bounds.top() - self.list.viewport_bounds().top()))
    }

    /// Возвращает опору чтения после показанного кадра для проверки вставки.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_reading_anchor(&self) -> Option<(Uuid, f32)> {
        self.anchor(&self.index)
            .map(|row| (row.id, f32::from(row.y)))
    }

    /// Возвращает авторов видимых групп для проверки виртуализации.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_visible_avatars(&self) -> Vec<Uuid> {
        visible_avatar_users(
            &self.rows,
            &self.index,
            &self.painted.borrow(),
            self.list.viewport_bounds(),
        )
    }
}

/// Отбирает авторов по реально нарисованным строкам, исключая запас списка за границами окна.
fn visible_avatar_users(
    rows: &[Row],
    index: &HashMap<Uuid, usize>,
    painted: &HashMap<Uuid, Bounds<Pixels>>,
    viewport: Bounds<Pixels>,
) -> Vec<Uuid> {
    if viewport.size.height <= px(0.) || viewport.size.width <= px(0.) {
        return Vec::new();
    }
    let mut users: Vec<_> = painted
        .iter()
        .filter_map(|(id, bounds)| {
            if !bounds.intersects(&viewport) {
                return None;
            }
            rows.get(*index.get(id)?)?.avatar_author()
        })
        .collect();
    users.sort_unstable();
    users.dedup();
    users
}

/// Рисует дату перед первым сообщением дня, не добавляя отдельный адрес в список.
fn date_separator(palette: Palette, date: &str, id: Uuid) -> Div {
    div()
        .absolute()
        .top_2()
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            label(palette, TextRole::Caption, date.to_owned())
                .px_3()
                .py_0p5()
                .rounded_full()
                .border_1()
                .border_color(palette.border)
                .bg(palette.surface)
                .debug_selector(move || format!("history-date-{id}")),
        )
}

/// Рисует сообщение с общим пузырём и оставляет место аватару в группе собеседника.
fn message_row(
    row: &Row,
    palette: Palette,
    peer: Option<&str>,
    photo: Option<ImageSource>,
    cards: Option<(&Cards, &mut App)>,
) -> Div {
    let author = peer.unwrap_or("Участник");
    let id = row.id;
    // Время и пояснения должны читаться и на более тёмной подложке исходящего.
    let bubble_palette = if row.own {
        Palette {
            message_time: palette.text,
            ..palette
        }
    } else {
        Palette {
            message_time: palette.text_secondary,
            accent: palette.text_secondary,
            ..palette
        }
    };
    let bubble = MessageBubble {
        author: (!row.own && row.starts_group).then(|| author.to_owned().into()),
        body: row.body.clone().into(),
        time: row.time.clone().map(Into::into),
        own: row.own,
        delivery: row.delivery,
    };
    let bubble = match cards.filter(|_| !row.media.is_empty()) {
        Some((cards, app)) => bubble.render_with_body(
            bubble_palette,
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .max_w_full()
                .gap_1()
                .children(attachment_caption(row).map(|caption| {
                    label(bubble_palette, TextRole::Body, caption)
                        .debug_selector(move || format!("history-media-caption-{id}"))
                }))
                .children(row.media.iter().enumerate().map(|(position, reference)| {
                    cards.render(
                        AttachmentKey {
                            message: id,
                            file: reference.file_id,
                            position,
                        },
                        reference,
                        bubble_palette,
                        app,
                    )
                })),
            row.ends_group,
        ),
        None => bubble.render_in_group(bubble_palette, row.ends_group),
    };
    div()
        .w_full()
        .flex()
        .items_end()
        .gap_2()
        .when(!row.own, |view| {
            let avatar = peer
                .and_then(|name| name.chars().next())
                .map(|initial| initial.to_uppercase().collect::<String>())
                .unwrap_or_default();
            view.child(div().w(px(40.)).flex_none().when(row.ends_group, |slot| {
                let mut avatar = Avatar::from_initials(avatar);
                avatar.image = photo;
                slot.child(
                    avatar
                        .render(palette)
                        .debug_selector(move || format!("history-avatar-{id}")),
                )
            }))
        })
        .child(
            div().flex_1().min_w_0().child(
                bubble
                    .debug_selector(move || format!("history-message-{id}"))
                    .when(row.delivery == Some(DeliveryMark::Unknown), |bubble| {
                        bubble.debug_selector(|| "history-delivery-unknown".into())
                    }),
            ),
        )
}

/// Убирает из подписи ссылки, которые уже показаны карточками.
fn attachment_caption(row: &Row) -> Option<String> {
    let mut caption = String::new();
    let mut offset = 0;
    for reference in &row.media {
        caption.push_str(&row.body[offset..reference.range.start]);
        offset = reference.range.end;
    }
    caption.push_str(&row.body[offset..]);
    let caption = caption.trim();
    (!caption.is_empty()).then(|| caption.to_owned())
}

/// Готовит историю и добавляет уникальные строки очереди только после подтверждённого хвоста.
fn build_rows(history: &HistoryWindow, outbox: &[OutboxEntry], user: Option<Uuid>) -> Vec<Row> {
    let mut rows: Vec<_> = history
        .messages
        .iter()
        .map(|message| {
            let own = user.is_some() && message.metadata.author_id == user;
            let stamp = message.metadata.created_at.as_str();
            Row {
                id: message.id,
                own,
                author: message.metadata.author_id,
                topic: message.topic_id,
                date: Some(format!(
                    "{}.{}.{}",
                    &stamp[8..10],
                    &stamp[5..7],
                    &stamp[0..4]
                )),
                body: message.text.clone(),
                media: media_references(&message.text),
                time: Some(stamp[11..16].to_owned()),
                delivery: own.then_some(DeliveryMark::Sent),
                starts_day: false,
                starts_group: false,
                ends_group: false,
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
                    author: user,
                    topic: entry.snapshot.topic_id,
                    date: None,
                    body: entry.snapshot.markdown.clone(),
                    media: Vec::new(),
                    time: Some(delivery_caption(entry.status).into()),
                    delivery: Some(match entry.status {
                        DeliveryStatus::Waiting | DeliveryStatus::Sending => DeliveryMark::Pending,
                        DeliveryStatus::Unknown => DeliveryMark::Unknown,
                        DeliveryStatus::Blocked(_) | DeliveryStatus::Rejected(_) => {
                            DeliveryMark::Failed
                        }
                    }),
                    starts_day: false,
                    starts_group: false,
                    ends_group: false,
                }),
        );
    }
    for ix in 0..rows.len() {
        let previous = ix.checked_sub(1).and_then(|previous| rows.get(previous));
        let current = &rows[ix];
        let starts_day =
            current.date.is_some() && previous.is_none_or(|row| row.date != current.date);
        let starts_group = previous.is_none_or(|row| !row.joins(current));
        let ends_group = rows.get(ix + 1).is_none_or(|row| !row.joins(current));
        let current = &mut rows[ix];
        current.starts_day = starts_day;
        current.starts_group = starts_group;
        current.ends_group = ends_group;
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
