//! Рисует виртуальную историю и восстанавливает опору по UUID после изменения состава строк.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ops::Range,
    rc::Rc,
};

use gpui::{
    AnyElement, App, Bounds, Div, ImageSource, IntoElement, ListAlignment, ListOffset, ListState,
    Pixels, StyledText, TextLayout, WeakEntity, canvas, div, list, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        quote::{self, Block},
        references::{MediaReference, media_references},
    },
    storage::{DeliveryStatus, HistoryWindow, OutboxEntry, SendBlock},
};

use super::{
    Conversation, ReadingAnchor,
    attachments::{AttachmentKey, Cards},
    reply::{ReplyControls, Selection, excerpt, history_quote},
};

#[path = "paging.rs"]
mod paging;
pub(super) use paging::PageNeed;

#[path = "text_anchor.rs"]
mod text_anchor;
use text_anchor::{LineTarget, ReadingLine};

use crate::gpui_client::ui::{
    Avatar, DeliveryMark, MessageBubble, Palette, TextRole, label, text_role,
};

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

/// Поправка ожидает ширину списка в первом кадре после правки.
struct EditAnchor {
    id: Uuid,            // Опорное сообщение.
    op_id: Uuid,         // Запрос, который принёс правку.
    scroll: ListOffset,  // Положение до поправки. Новый жест может его изменить.
    source: ReadingLine, // Та же строка в последнем показанном кадре.
}

/// Сохраняет тот же фрагмент при нескольких сменах ширины без прокрутки.
#[derive(Clone, Copy)]
struct ReflowAnchor {
    id: Uuid,           // Сообщение, которое человек продолжает читать.
    scroll: ListOffset, // Позиция после последней смены ширины.
    target: LineTarget, // Байтовая позиция фрагмента и высота его строки.
}

/// Снимок виртуального списка и реально нарисованные границы последнего кадра.
pub(super) struct Viewport {
    rows: Rc<Vec<Row>>, // Состав списка для отложенного построения видимых элементов.
    index: Rc<HashMap<Uuid, usize>>, // Позиции источников для цитат в текущем окне.
    selection: Rc<RefCell<Selection>>, // Выделенный фрагмент в виртуальном списке.
    list: ListState,    // Позиция и размеры строк в GPUI.
    painted: Rc<RefCell<HashMap<Uuid, Bounds<Pixels>>>>, // Фактические прямоугольники строк.
    painted_text: Rc<RefCell<HashMap<Uuid, TextLayout>>>, // Разметка простого текста последнего кадра.
    painted_offset: Rc<Cell<Option<ListOffset>>>, // Смещение, для которого измерены прямоугольники.
    measure: Option<Measurement>,                 // Следующее измерение после замены снимка.
    edit_anchor: Option<EditAnchor>, // Поправка внутри текста опоры для следующего кадра.
    reflow_anchor: Rc<Cell<Option<ReflowAnchor>>>, // Опора между сменами ширины без жеста.
}

impl Viewport {
    /// Задаёт, на сколько логических px за пределами окна список измеряет строки.
    fn with_overdraw(overdraw: Pixels) -> Self {
        Self {
            rows: Rc::default(),
            index: Rc::default(),
            selection: Rc::default(),
            list: ListState::new(0, ListAlignment::Bottom, overdraw),
            painted: Rc::default(),
            painted_text: Rc::default(),
            painted_offset: Rc::default(),
            measure: None,
            edit_anchor: None,
            reflow_anchor: Rc::default(),
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
        *self.selection.borrow_mut() = Selection::default();
        self.list.reset(0);
        self.painted.borrow_mut().clear();
        self.painted_text.borrow_mut().clear();
        self.painted_offset.set(None);
        self.measure = None;
        self.edit_anchor = None;
        self.reflow_anchor.set(None);
    }

    /// Сверяет последний отрисованный кадр с текущим смещением списка.
    fn paint_matches(&self, offset: ListOffset) -> bool {
        self.painted_offset
            .get()
            .is_some_and(|painted| same_offset(painted, offset))
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
        // Одна и та же байтовая позиция имеет смысл, пока текст и маршрут совпадают.
        if self.reflow_anchor.get().is_some_and(|reflow| {
            !matches!(position, Position::Reading)
                || !same_offset(self.list.logical_scroll_top(), reflow.scroll)
                || self.index.get(&reflow.id).map(|&ix| &self.rows[ix].body)
                    != index.get(&reflow.id).map(|&ix| &rows[ix].body)
        }) {
            self.reflow_anchor.set(None);
        }
        let source = if let (Position::Reading, Some(anchor)) = (&position, anchor) {
            self.reading_line(anchor.id, &rows[index[&anchor.id]])
        } else {
            None
        };
        self.edit_anchor = None;
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
        self.selection.borrow_mut().retain_current(|id| {
            let row = self.rows.get(*index.get(&id)?)?;
            (row.date.is_some() && !row.body.trim().is_empty()).then_some(row.body.as_str())
        });
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
                    self.edit_anchor = source.map(|source| EditAnchor {
                        id: anchor.id,
                        op_id,
                        scroll: self.list.logical_scroll_top(),
                        source,
                    });
                }
            }
        }
    }

    /// Берёт строку из показанного кадра или держит её до кадра при нескольких быстрых ответах.
    fn reading_line(&mut self, id: Uuid, next: &Row) -> Option<ReadingLine> {
        let offset = self.list.logical_scroll_top();
        if let Some(edit) = self.edit_anchor.take()
            && edit.id == id
            && same_offset(edit.scroll, offset)
        {
            return Some(edit.source);
        }
        let current = &self.rows[*self.index.get(&id)?];
        if current.body == next.body || !self.paint_matches(offset) {
            return None;
        }
        let layout = self.painted_text.borrow().get(&id)?.clone();
        ReadingLine::at_top(layout, self.list.viewport_bounds().top())
    }

    /// Возвращает общий диапазон для мыши, меню и клавиатуры.
    pub(super) fn selection(&self) -> Rc<RefCell<Selection>> {
        self.selection.clone()
    }

    /// Даёт имя известного автора из окна и каталога обычной лички.
    pub(super) fn source_author(&self, id: Uuid, peer: Option<&str>) -> Option<String> {
        self.index
            .get(&id)
            .and_then(|&index| row_author(&self.rows[index], peer))
    }

    /// Проверяет присутствие той же ссылки в текущем окне разговора.
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

    /// Просит GPUI заново измерить строку, сохраняя текущий индекс и смещение внутри него.
    pub(super) fn invalidate_row_height(&self, id: Uuid) {
        if let Some(&index) = self.index.get(&id) {
            let position = self.list.logical_scroll_top();
            self.list.splice(index..index + 1, 1);
            self.list.scroll_to(position);
        }
    }

    /// Берёт разбивку текста из последнего кадра, когда человек остановился внутри сообщения.
    fn reflow_reading(&self, scroll: ListOffset) -> Option<ReadingLine> {
        if !self.paint_matches(scroll) {
            return None;
        }
        let row = self.rows.get(scroll.item_ix)?;
        let layout = self.painted_text.borrow().get(&row.id)?.clone();
        ReadingLine::at_top(layout, self.list.viewport_bounds().top())
    }

    /// Показывает локальные строки с управляемым содержимым отдельных баблов.
    pub(super) fn render_standalone(
        &mut self,
        palette: Palette,
        peer: String,
        accessory: impl Fn(Uuid) -> Option<AnyElement> + 'static,
    ) -> impl IntoElement {
        self.render_rows(palette, Some(peer), None, HashMap::new(), None, accessory)
    }

    /// Рисует список и пишет фактическую координату опоры, когда GPUI дойдёт до её строки.
    pub(super) fn render(
        &mut self,
        palette: Palette,
        peer: Option<String>,
        reply: Option<ReplyControls>,
        images: HashMap<Uuid, ImageSource>,
        cards: Option<Cards>,
    ) -> impl IntoElement {
        self.render_rows(palette, peer, reply, images, cards, |_| None)
    }

    /// Собирает один виртуальный список для рабочего чата и окна стенда.
    fn render_rows(
        &mut self,
        palette: Palette,
        peer: Option<String>,
        reply: Option<ReplyControls>,
        images: HashMap<Uuid, ImageSource>,
        cards: Option<Cards>,
        accessory: impl Fn(Uuid) -> Option<AnyElement> + 'static,
    ) -> impl IntoElement {
        let previous_width = self.list.viewport_bounds().size.width;
        let previous_scroll = self.list.logical_scroll_top();
        let following_tail = self.at_tail();
        let reading = (!following_tail)
            .then(|| self.reflow_reading(previous_scroll))
            .flatten();
        let reflow_anchor = self.reflow_anchor.clone();
        let edit_anchor = self.edit_anchor.take();
        self.painted.borrow_mut().clear();
        self.painted_text.borrow_mut().clear();
        self.painted_offset.set(None);
        let rows = self.rows.clone();
        let row_index = self.index.clone();
        let painted = self.painted.clone();
        let painted_text = self.painted_text.clone();
        let painted_offset = self.painted_offset.clone();
        let measurement = Rc::new(RefCell::new(self.measure.take()));
        let list_state = self.list.clone();
        let message = Rc::new(move |row: &Row, app: &mut App| {
            let photo = row.avatar_author().and_then(|id| images.get(&id).cloned());
            message_row(
                row,
                palette,
                peer.as_deref(),
                (&rows, &row_index),
                reply.as_ref(),
                photo,
                RowBodyExtras {
                    media: cards.as_ref().map(|cards| (cards, app)),
                    accessory: accessory(row.id),
                },
            )
        });
        let measured_line = Rc::new(Cell::new(None::<(Uuid, Uuid, LineTarget)>));
        let correction = {
            let list = list_state.clone();
            let rows = self.rows.clone();
            let message = message.clone();
            let measured_line = measured_line.clone();
            canvas(
                move |bounds, window, app| {
                    let scroll = list.logical_scroll_top();
                    let row = rows.get(scroll.item_ix);
                    let retained = reflow_anchor.get().filter(|anchor| {
                        same_offset(scroll, anchor.scroll)
                            && row.is_some_and(|row| row.id == anchor.id)
                    });
                    reflow_anchor.set(retained);
                    let Some(row) = row else { return };
                    let resized = bounds.size.width != previous_width;
                    let edit = edit_anchor.filter(|edit| row.id == edit.id && same_offset(scroll, edit.scroll));
                    let (op_id, target) = if let Some(edit) = &edit {
                        (edit.op_id, edit.source.target(&row.body))
                    } else if resized && !following_tail && same_offset(scroll, previous_scroll) {
                        let target = retained.map(|anchor| anchor.target)
                            .or_else(|| reading?.target(&row.body));
                        (Uuid::new_v4(), target)
                    } else {
                        return;
                    };
                    let measured = target.and_then(|target| {
                        let offset = target.measure(bounds.size.width, window, app, |app| {
                            let (message, text) = message(row, app);
                            (row_container(row, palette).child(message), text)
                        })?;
                        Some((target, offset))
                    });
                    let offset_in_item = match measured {
                        Some((target, offset)) => {
                            measured_line.set(Some((row.id, op_id, target)));
                            offset
                        }
                        None if edit.is_some() => row.inset(),
                        None => return,
                    };
                    list.scroll_to(ListOffset { item_ix: scroll.item_ix, offset_in_item });
                    if resized && let Some((target, _)) = measured {
                        reflow_anchor.set(Some(ReflowAnchor {
                            id: row.id,
                            scroll: list.logical_scroll_top(),
                            target,
                        }));
                    }
                    log::debug!(
                        "gpui_history op_id={op_id} event=text_anchor_positioned anchor_uuid={} old_offset_px={} new_offset_px={} width_before_px={} width_after_px={}",
                        row.id, f32::from(scroll.offset_in_item), f32::from(offset_in_item),
                        f32::from(previous_width), f32::from(bounds.size.width)
                    );
                },
                |_, (), _, _| (),
            )
            .absolute()
            .inset_0()
        };
        let rows = self.rows.clone();
        let items = list(self.list.clone(), move |index, _, app| {
            let row = &rows[index];
            let id = row.id;
            let painted = painted.clone();
            let painted_text = painted_text.clone();
            let painted_offset = painted_offset.clone();
            let measurement = measurement.clone();
            let measured_line = measured_line.clone();
            let list_state = list_state.clone();
            let (message, text_layout) = message(row, app);
            row_container(row, palette)
                .child(message.relative().child(canvas(|_, _, _| (), move |bounds, (), _, _| {
                    painted.borrow_mut().insert(id, bounds);
                    if let Some(layout) = text_layout {
                        if let Some((_, op_id, target)) = measured_line.get().filter(|(anchor, _, _)| *anchor == id)
                            && let Some(point) = layout.position_for_index(target.byte)
                        {
                            let y_after = point.y - list_state.viewport_bounds().top();
                            log::debug!("gpui_history op_id={op_id} event=text_anchor_measured anchor_uuid={id} y_before_px={} y_after_px={} delta_px={}",
                                f32::from(target.y), f32::from(y_after), f32::from(y_after - target.y));
                        }
                        painted_text.borrow_mut().insert(id, layout);
                    }
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
        }).flex_1().min_h_0().w_full().pb(BOTTOM_PADDING);
        // GPUI подготавливает детей по порядку. Сначала измеряем и выставляем опору,
        // затем сам список строит видимую область для уже исправленного смещения.
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(correction)
            .child(items)
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
        self.tested_bounds(id)
            .map(|bounds| f32::from(bounds.top() - self.list.viewport_bounds().top()))
    }

    /// Возвращает фактические размеры строки, полученные при последней отрисовке.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_bounds(&self, id: Uuid) -> Option<Bounds<Pixels>> {
        self.painted.borrow().get(&id).copied()
    }

    /// Возвращает координату строки текста в окне для проверки первого кадра.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_text_y(&self, id: Uuid, byte: usize) -> Option<Pixels> {
        self.tested_text_layout(id)?
            .position_for_index(byte)
            .map(|point| point.y)
    }

    /// Даёт фактические переносы текста из кадра для проверки фрагмента внутри строки.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_text_layout(&self, id: Uuid) -> Option<TextLayout> {
        self.painted_text.borrow().get(&id).cloned()
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

/// Сверяет индекс и смещение, чтобы не потерять новый жест между кадрами.
fn same_offset(a: ListOffset, b: ListOffset) -> bool {
    a.item_ix == b.item_ix && a.offset_in_item == b.offset_in_item
}

/// Задаёт отступы для строки списка и для её пробного измерения.
fn row_container(row: &Row, palette: Palette) -> Div {
    div().relative().w_full().px_3().pt(row.inset()).children(
        row.date
            .as_deref()
            .filter(|_| row.starts_day)
            .map(|date| date_separator(palette, date, row.id)),
    )
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

/// Передаёт дополнительные элементы содержимого при отрисовке одного бабла.
struct RowBodyExtras<'a> {
    media: Option<(&'a Cards, &'a mut App)>, // Карточки вложений рабочего чата.
    accessory: Option<AnyElement>, // Управляемый блок, который появляется внутри бабла на стенде.
}

/// Рисует сообщение с общим пузырём и оставляет место аватару в группе собеседника.
fn message_row(
    row: &Row,
    palette: Palette,
    peer: Option<&str>,
    history: (&[Row], &HashMap<Uuid, usize>),
    reply: Option<&ReplyControls>,
    photo: Option<ImageSource>,
    extras: RowBodyExtras<'_>,
) -> (Div, Option<TextLayout>) {
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
    let reply = reply.filter(|_| row.date.is_some() && !row.body.trim().is_empty());
    let bubble = MessageBubble {
        author: (!row.own && row.starts_group).then(|| author.to_owned().into()),
        body: row.body.clone().into(),
        time: row.time.clone().map(Into::into),
        own: row.own,
        delivery: row.delivery,
    };
    let (rows, index) = history;
    let mut text_layout = None;
    let content = if let Some((cards, app)) = extras.media.filter(|_| !row.media.is_empty()) {
        let caption = div().flex().flex_col().min_w_0().max_w_full().gap_1();
        let caption = if let Some(reply) =
            reply.filter(|_| matches!(quote::blocks(&row.body).as_slice(), [Block::Text(_)]))
        {
            caption.children(
                media_caption_spans(row)
                    .into_iter()
                    .map(|span| reply.selectable_span(id, &row.body, span, bubble_palette)),
            )
        } else {
            let body = caption_without_media(row);
            let content =
                quote_content(row, quote::blocks(&body), bubble_palette, peer, rows, index)
                    .or_else(|| {
                        (!body.trim().is_empty())
                            .then(|| label(bubble_palette, TextRole::Body, body).into_any_element())
                    });
            caption.children(content)
        };
        Some(
            caption
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
                }))
                .into_any_element(),
        )
    } else {
        let blocks = quote::blocks(&row.body);
        if matches!(blocks.as_slice(), [Block::Text(_)]) && !row.body.trim().is_empty() {
            let (body, layout) = if let Some(controls) = reply {
                let (body, layout) = controls.selectable(id, &row.body, bubble_palette);
                (body.min_w_0().max_w_full().into_any_element(), layout)
            } else {
                let text = StyledText::new(row.body.clone());
                let layout = text.layout().clone();
                (
                    text_role(bubble_palette, TextRole::Body)
                        .min_w_0()
                        .max_w_full()
                        .child(text)
                        .into_any_element(),
                    layout,
                )
            };
            text_layout = Some(layout);
            Some(body)
        } else {
            quote_content(row, blocks, bubble_palette, peer, rows, index)
        }
    };
    let mut bubble = match (content, extras.accessory) {
        (content, Some(accessory)) => {
            bubble.render_with_accessory(bubble_palette, content, row.ends_group, accessory)
        }
        (Some(content), None) => bubble.render_with_body(bubble_palette, content, row.ends_group),
        (None, None) => bubble.render_in_group(bubble_palette, row.ends_group),
    }
    .debug_selector(move || format!("history-message-{id}"));
    if row.delivery == Some(DeliveryMark::Unknown) {
        bubble = bubble.debug_selector(|| "history-delivery-unknown".into());
    }
    let bubble = match reply {
        Some(controls) => controls.row(id, bubble, palette).into_any_element(),
        None => bubble.into_any_element(),
    };
    let message = div()
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
        .child(div().flex_1().min_w_0().child(bubble));
    (message, text_layout)
}

/// Выбирает подписанного автора, не выдавая имя собеседника за участника канала.
fn row_author(row: &Row, peer: Option<&str>) -> Option<String> {
    if row.own {
        Some("Вы".into())
    } else {
        peer.map(str::to_owned)
    }
}

/// Возвращает фрагменты подписи с исходными координатами для точного цитирования.
fn media_caption_spans(row: &Row) -> Vec<Range<usize>> {
    let mut previous = 0;
    row.media
        .iter()
        .map(|reference| reference.range.clone())
        .chain(std::iter::once(row.body.len()..row.body.len()))
        .filter_map(|range| {
            let start = previous;
            previous = range.end;
            let text = row.body.get(start..range.start)?;
            let trimmed_start = text.trim_start();
            let visible = trimmed_start.trim_end();
            let start = start + text.len() - trimmed_start.len();
            (!visible.is_empty()).then_some(start..start + visible.len())
        })
        .collect()
}

/// Скрывает в подписи только ссылки, которые уже отображены отдельными карточками.
fn caption_without_media(row: &Row) -> String {
    let mut caption = String::new();
    let mut offset = 0;
    for reference in &row.media {
        if let Some(text) = row.body.get(offset..reference.range.start) {
            caption.push_str(text);
            offset = reference.range.end;
        }
    }
    caption.push_str(&row.body[offset..]);
    caption
}

/// Показывает отдельные цитаты по контракту, сохраняя обычный пузырь для прочего текста.
fn quote_content(
    row: &Row,
    blocks: Vec<Block<'_>>,
    palette: Palette,
    peer: Option<&str>,
    rows: &[Row],
    index: &HashMap<Uuid, usize>,
) -> Option<gpui::AnyElement> {
    if matches!(blocks.as_slice(), [Block::Text(_)]) {
        return None;
    }
    Some(
        div()
            .flex()
            .flex_col()
            .min_w_0()
            // Заголовок цитаты рисуется внутри canvas и не задаёт собственную ширину.
            // Даём ему место даже рядом с коротким ответом или фрагментом.
            .w(px(312.))
            .max_w_full()
            .gap_1()
            .children(
                blocks
                    .into_iter()
                    .enumerate()
                    .map(|(position, block)| match block {
                        Block::Text(text) => label(palette, TextRole::Body, text.to_owned()),
                        Block::Quote(reference) => {
                            let source = index
                                .get(&reference.message_id)
                                .map(|&index| &rows[index])
                                .filter(|source| source.id != row.id && source.date.is_some());
                            let author = source
                                .and_then(|source| row_author(source, peer))
                                .unwrap_or(reference.author);
                            let snippet = reference.fragment.unwrap_or_else(|| {
                                source.map_or_else(String::new, |source| excerpt(&source.body))
                            });
                            let id = row.id;
                            history_quote(palette, row.own, author, snippet, source.is_some())
                                .debug_selector(move || format!("history-quote-{id}-{position}"))
                        }
                    }),
            )
            .into_any_element(),
    )
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
