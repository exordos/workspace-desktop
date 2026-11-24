//! Рисует виртуальную историю и восстанавливает опору по UUID после изменения состава строк.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};

use gpui::{
    AnyElement, App, Bounds, Div, ImageSource, IntoElement, ListAlignment, ListOffset, ListState,
    MouseButton, Pixels, Point, SharedString, StyledText, TextLayout, WeakEntity, canvas, div, list, point, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        calls::JitsiSettings,
        references::{MediaReference, media_references},
    },
    storage::{
        DeliveryStatus, HistoryWindow, KeyServicePayload, MessageBody, OutboxEntry, SendBlock,
    },
};

use super::{
    Conversation, ReadingAnchor,
    attachments::{AttachmentKey, Cards},
    calls::join_card,
    encryption::{EncryptedPresentation, HistoryEncryption},
    message_menu::MenuHit,
    fragments::{FragmentRuntime, FragmentViews},
    key_exchange::{KeyAction, KeyActionState, KeyPermission},
    protected_body::ProtectedLayout,
    protected_quotes::{QuotePresentation, QuoteSlot, render_quote},
    quick_actions::{QuickActions, QuickCommand, QuickReactions, reaction_chips},
    reactions::ReactionControls,
    reply::{ReplyControls, Selection, history_quote},
};

#[path = "body.rs"]
mod body;
use body::{Body, BodyPart, ForwardBody};

#[path = "paging.rs"]
mod paging;
pub(super) use paging::PageNeed;

#[path = "text_anchor.rs"]
mod text_anchor;
use text_anchor::{LineTarget, ReadingLine};
#[path = "protected_text.rs"]
mod protected_text;
pub(in crate::gpui_client) use protected_text::GuardedText;
pub(super) use protected_text::ProtectedText;
#[path = "text_rows.rs"]
mod text_rows;
use text_rows::{BubbleInput, TextRows};

use crate::gpui_client::{
    composer::ReplyText,
    preferences::Language,
    ui::{
        Avatar, Button, DeliveryMark, Icon, MessageBubble, Palette, TextRole, activate, label,
        text_role,
    },
};

const DATE_SPACE: Pixels = px(32.);
const BOTTOM_PADDING: Pixels = px(12.);

/// Строка, которую можно рисовать после переноса снимка на поток UI.
#[derive(Clone, Eq, PartialEq)]
struct Row {
    id: Uuid,                                 // UUID сообщения или публичный UUID очереди.
    own: bool,                                // Сообщение текущего пользователя.
    author: Option<Uuid>, // UUID для группировки, неизвестные авторы не объединяются.
    topic: Uuid,          // Смена темы начинает новую группу.
    date: Option<String>, // День создания UTC в ISO-порядке, если сообщение принято сервером.
    body: Rc<Body>,       // Данные обычного тела из снимка; не зависят от координаты строки.
    encrypted: Option<EncryptedPresentation>, // Состояние E2EE; None для обычной строки.
    service: Option<KeyServiceRow>, // Видимая часть служебного сообщения без криптографического тела.
    retry_send: Option<Uuid>, // Канонический UUID защищённой попытки, которую можно повторить.
    plain_text: bool,         // Обычное текстовое содержимое без зависимости от других строк.
    media: Vec<MediaReference>, // Прямые вложения подтверждённой строки.
    time: RowTime,            // Время сервера или ключ пояснения очереди.
    delivery: Option<DeliveryMark>, // Состояние отправки исходящей строки.
    starts_day: bool,         // Над этой строкой виден новый день.
    starts_group: bool,       // Имя автора и отступ перед началом группы.
    ends_group: bool,         // Аватар собеседника рядом с последней строкой группы.
}

/// Подпись рядом с отметкой доставки.
#[derive(Clone, Eq, PartialEq)]
enum RowTime {
    Sent(String),          // Время сервера для принятого сообщения.
    Pending(&'static str), // Ключ пояснения для локального сообщения.
}

impl RowTime {
    /// Подставляет текст очереди на языке видимой истории.
    fn caption(&self, language: Language) -> SharedString {
        match self {
            Self::Sent(time) => time.clone().into(),
            Self::Pending(key) => language.tr(key).into(),
        }
    }
}

/// Служебная строка истории без шифротекста, секретов и ключевого материала.
#[derive(Clone, Eq, PartialEq)]
enum KeyServiceRow {
    Request {
        device: String, // Публичный UUID устройства получателя.
        name: String,   // Имя устройства из запроса.
    },
    Grant,     // Опубликована выдача ключа.
    Reject,    // Опубликован отказ.
    Announced, // Опубликована смена ключа.
}

/// Готовые подписи входящего аватара и автора для всех баблов окна.
#[derive(Clone, Eq, PartialEq)]
struct PeerLabel {
    name: SharedString,     // Имя собеседника из текущего каталога.
    initials: SharedString, // Тот же запасной аватар для всех его строк.
}

impl From<String> for PeerLabel {
    /// Подготавливает подписи один раз до обхода списка.
    fn from(name: String) -> Self {
        let initials = name
            .chars()
            .next()
            .map(|initial| initial.to_uppercase().collect::<String>())
            .unwrap_or_default();
        Self {
            name: name.into(),
            initials: initials.into(),
        }
    }
}

/// Действия строки, связанные с текущим экраном и выбранным языком.
#[derive(Default)]
pub(super) struct HistoryControls {
    pub reply: Option<ReplyControls>, // Выделение текста и меню подтверждённых строк.
    pub quick_reply: bool, // Готовый обычный редактор позволяет показать быструю кнопку.
    pub quick_hidden: bool, // Активное контекстное меню закрывает быстрые действия.
    pub reactions: Option<ReactionControls>, // Состояние реакций в текущем разговоре.
    pub conversation: Option<WeakEntity<Conversation>>, // Повтор расшифровки в открытом экране.
    pub edit_protected: bool,         // Штатный редактор подтвердил защищённую запись в этой теме.
    pub key_device: Option<String>,   // Устройство активного обмена ключами.
    pub key_actions: HashMap<Uuid, KeyActionState>, // Ход действий по публичному UUID.
    pub key_permissions: HashMap<Uuid, KeyPermission>, // Права точных запросов после проверки Core.
    pub protected: HashMap<Uuid, Arc<ProtectedLayout>>, // Безопасная структура конкретных раскрытых баблов.
    pub quotes: HashMap<QuoteSlot, QuotePresentation>, // Отдельный доступ к разрешённым оригиналам.
    pub fragments: Option<FragmentRuntime>, // Рабочая сессия для спойлеров обычного Markdown.
    pub jitsi: Option<Arc<JitsiSettings>>, // Проверенный сервер конференций для карточек этой истории.
    pub locale: Language,                  // Язык подписей открытой истории.
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

/// Разобранное тело одного UUID, которое может вернуться при перечитывании окна.
struct CachedBody {
    body: Rc<Body>,             // Обычное тело или пустой оформитель закрытой строки.
    media: Vec<MediaReference>, // Только обычные прямые вложения подтверждённой строки.
    confirmed: bool,            // История и очередь по-разному показывают вложения.
    encrypted: bool,            // Вид тела, который может поменяться после правки.
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
    ordinary_views: Rc<RefCell<TextRows>>, // Представления обычных баблов рядом с E2EE.
    pub(super) fragment_views: FragmentViews, // Отдельные элементы спойлеров со своим состоянием.
    prepared_bodies: HashMap<Uuid, CachedBody>, // Подготовка последних снимков текущего разговора.
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
            ordinary_views: Rc::default(),
            fragment_views: FragmentViews::default(),
            prepared_bodies: HashMap::new(),
        }
    }

    /// Возвращает состояние для подключения обработчика прокрутки.
    pub(super) fn list(&self) -> &ListState {
        &self.list
    }

    /// Сообщает рабочей области, что история может получить точечную дельту ключа.
    pub(super) fn has_encrypted_rows(&self) -> bool {
        self.rows
            .iter()
            .any(|row| row.encrypted.is_some() || row.body.has_fragments)
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
        self.reset_for_reopen();
        self.prepared_bodies.clear();
    }

    /// Сбрасывает позицию перед загрузкой того же разговора, оставляя сверяемый по телу кеш.
    pub(super) fn reset_for_reopen(&mut self) {
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
        self.ordinary_views.borrow_mut().retain(|_| false);
        self.fragment_views.retain(|_, _, _| false);
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
        encryption: Option<&HistoryEncryption>,
    ) {
        let rows = build_rows(history, outbox, user, encryption, &mut self.prepared_bodies);
        let previous_count = self.rows.len();
        let index: HashMap<_, _> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| (row.id, i))
            .collect();
        self.fragment_views.retain(|id, position, reference| {
            index.get(&id).is_some_and(|&next| {
                self.index
                    .get(&id)
                    .is_some_and(|&previous| self.rows[previous].body == rows[next].body)
                    && rows[next].body.fragment(position) == Some(reference)
            })
        });
        // Допускаем краткое исчезновение соседней строки из снимка, но ограничиваем кэш одного окна.
        if self.prepared_bodies.len() > rows.len().saturating_mul(2).max(256) {
            self.prepared_bodies.retain(|id, _| index.contains_key(id));
        }
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
            (row.date.is_some()
                && row.service.is_none()
                && row.encrypted.is_none()
                && !row.body.text.trim().is_empty())
            .then_some(row.body.text.as_ref())
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

    /// Меняет только представления указанных UUID и сохраняет читаемую строку или хвост списка.
    pub(super) fn apply_encryption(
        &mut self,
        changes: impl IntoIterator<Item = (Uuid, EncryptedPresentation)>,
    ) -> bool {
        let tail = self.at_tail();
        let scroll = self.list.logical_scroll_top();
        let anchor = (!tail).then(|| self.saved_anchor()).flatten();
        let mut changed = false;
        for (id, presentation) in changes {
            let Some(&index) = self.index.get(&id) else {
                continue;
            };
            let row = &self.rows[index];
            if row.encrypted.is_none() || row.encrypted.as_ref() == Some(&presentation) {
                continue;
            }
            Rc::make_mut(&mut self.rows)[index].encrypted = Some(presentation);
            self.painted_text.borrow_mut().remove(&id);
            if self
                .reflow_anchor
                .get()
                .is_some_and(|anchor| anchor.id == id)
            {
                self.reflow_anchor.set(None);
            }
            self.list.splice(index..index + 1, 1);
            changed = true;
        }
        if changed {
            let offset = if tail {
                ListOffset {
                    item_ix: self.rows.len(),
                    offset_in_item: px(0.),
                }
            } else if let Some(anchor) = anchor {
                let item_ix = self.index[&anchor.id];
                ListOffset {
                    item_ix,
                    offset_in_item: self.rows[item_ix].inset() - anchor.y,
                }
            } else {
                scroll
            };
            self.list.scroll_to(offset);
        }
        changed
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
    pub(super) fn source_author(&self, id: Uuid, peer: Option<&str>) -> Option<ReplyText> {
        self.index
            .get(&id)
            .and_then(|&index| row_author(&self.rows[index], peer))
    }

    /// Проверяет, осталась ли ссылка на оригинал в той же подтверждённой строке.
    pub(super) fn has_forward_source(&self, row: Uuid, source: Uuid) -> bool {
        self.index.get(&row).is_some_and(|&index| {
            let row = &self.rows[index];
            row.date.is_some() && row.body.contains_forward(source)
        })
    }

    /// Проверяет присутствие той же ссылки в текущем окне разговора.
    pub(super) fn has_attachment(&self, key: AttachmentKey) -> bool {
        self.attachment(key).is_some()
    }

    /// Возвращает данные только для точной карточки на той же позиции сообщения.
    pub(super) fn attachment(&self, key: AttachmentKey) -> Option<&MediaReference> {
        let &index = self.index.get(&key.message)?;
        self.rows[index]
            .media
            .get(key.position)
            .filter(|reference| reference.file_id == key.file)
    }

    /// Проверяет, есть ли у карточки показанная строка, куда можно вернуть фокус.
    pub(super) fn has_visible_attachment(&self, key: AttachmentKey) -> bool {
        self.has_attachment(key) && self.has_visible_message(key.message)
    }

    /// Берёт правый край показанной строки для клавиатурного меню.
    pub(super) fn message_menu_anchor(&self, id: Uuid) -> Option<Point<Pixels>> {
        let bounds = self.painted.borrow().get(&id).copied()?;
        bounds
            .intersects(&self.list.viewport_bounds())
            .then(|| point(bounds.right(), bounds.center().y))
    }

    /// Проверяет, осталась ли строка видимой после закрытия защищённого просмотра.
    pub(super) fn has_visible_message(&self, id: Uuid) -> bool {
        self.painted.borrow().get(&id)
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

    /// Закрывает устаревшие фрагменты до нового подтверждённого окна.
    pub(super) fn invalidate_fragment_row(&self, id: Uuid, cx: &mut gpui::Context<Conversation>) {
        if self
            .index
            .get(&id)
            .is_some_and(|&index| self.rows[index].body.has_fragments)
        {
            self.fragment_views.invalidate_row(id, cx);
            self.invalidate_row_height(id);
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
        self.render_rows(
            palette,
            Some(peer),
            HistoryControls::default(),
            HashMap::new(),
            None,
            accessory,
        )
    }

    /// Рисует список и пишет фактическую координату опоры, когда GPUI дойдёт до её строки.
    pub(super) fn render(
        &mut self,
        palette: Palette,
        peer: Option<String>,
        controls: HistoryControls,
        images: HashMap<Uuid, ImageSource>,
        cards: Option<Cards>,
    ) -> impl IntoElement {
        self.render_rows(palette, peer, controls, images, cards, |_| None)
    }

    /// Собирает один виртуальный список для рабочего чата и окна стенда.
    fn render_rows(
        &mut self,
        palette: Palette,
        peer: Option<String>,
        controls: HistoryControls,
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
        let memoize = controls.conversation.is_some() && self.has_encrypted_rows();
        let ordinary_views = self.ordinary_views.clone();
        ordinary_views
            .borrow_mut()
            .retain(|id| memoize && self.index.contains_key(&id));
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
        let locale = controls.locale;
        let controls = Rc::new(controls);
        let peer = peer.map(PeerLabel::from);
        let images = Rc::new(images);
        let current_width = Rc::new(Cell::new(previous_width));
        let message = {
            let peer = peer.clone();
            let controls = controls.clone();
            let images = images.clone();
            let fragments = self.fragment_views.clone();
            Rc::new(move |row: &Row, app: &mut App| {
                let photo = row.avatar_author().and_then(|id| images.get(&id).cloned());
                message_row(
                    row,
                    palette,
                    peer.as_ref(),
                    (&rows, &row_index),
                    &controls,
                    photo,
                    RowBodyExtras {
                        media: cards.as_ref(),
                        accessory: accessory(row.id),
                        fragments: Some(&fragments),
                        app: Some(app),
                    },
                )
            })
        };
        let measured_line = Rc::new(Cell::new(None::<(Uuid, Uuid, LineTarget)>));
        let correction = {
            let list = list_state.clone();
            let rows = self.rows.clone();
            let message = message.clone();
            let measured_line = measured_line.clone();
            let current_width = current_width.clone();
            canvas(
                move |bounds, window, app| {
                    current_width.set(bounds.size.width);
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
                        (edit.op_id, edit.source.target(&row.body.text))
                    } else if resized && !following_tail && same_offset(scroll, previous_scroll) {
                        let target = retained.map(|anchor| anchor.target)
                            .or_else(|| reading?.target(&row.body.text));
                        (Uuid::new_v4(), target)
                    } else {
                        return;
                    };
                    let measured = target.and_then(|target| {
                        let offset = target.measure(bounds.size.width, window, app, |app| {
                            let (message, text) = message(row, app);
                            (row_container(row, palette, locale).child(message), text)
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
            let (message, text_layout) = if memoize && row.plain_text {
                let photo = row.avatar_author().and_then(|id| images.get(&id).cloned());
                let bubble = ordinary_views.borrow_mut().bubble(BubbleInput {
                    row, palette, peer: peer.as_ref(), photo, controls: controls.clone(), width: current_width.clone(),
                }, app);
                (bubble.element, RenderedText::Cached(bubble.layout))
            } else {
                let (message, layout) = message(row, app);
                (message.into_any_element(), RenderedText::Fresh(layout))
            };
            row_container(row, palette, locale)
                .child(div().relative().w_full().child(message).child(canvas(|_, _, _| (), move |bounds, (), _, _| {
                    painted.borrow_mut().insert(id, bounds);
                    if let Some(layout) = text_layout.layout() {
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

    /// Считает сборки обычного бабла средствами GPUI во время теста обновления ключа.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_bubble_renders(&self, id: Uuid, cx: &App) -> Option<usize> {
        self.ordinary_views.borrow().renders(id, cx)
    }

    /// Считывает идентичность подготовленного тела, чтобы проверить повторное использование на сдвиге.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_body_identity(&self, id: Uuid) -> Option<usize> {
        Some(Rc::as_ptr(&self.rows.get(*self.index.get(&id)?)?.body) as usize)
    }

    /// Проверяет состояние доступности по видимой модели, не читая сам текст.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(super) fn tested_encryption_state(&self, id: Uuid) -> Option<&'static str> {
        match self.rows.get(*self.index.get(&id)?)?.encrypted.as_ref()? {
            EncryptedPresentation::Ready(text) if text.with_current(|| ()).is_some() => {
                Some("readable")
            }
            EncryptedPresentation::Failed => Some("failed"),
            _ => Some("locked"),
        }
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

/// Разметка обычного текста приходит из нового прохода либо из переиспользованного вида.
enum RenderedText {
    Fresh(Option<TextLayout>), // Простая строка без кеша или только что измеренная сложная строка.
    Cached(Rc<RefCell<Option<TextLayout>>>), // Разметка постоянного GPUI-вида.
}

impl RenderedText {
    /// Берёт фактическую геометрию, когда GPUI уже выполнил подготовку кадра.
    fn layout(self) -> Option<TextLayout> {
        match self {
            Self::Fresh(layout) => layout,
            Self::Cached(layout) => layout.borrow().clone(),
        }
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
fn row_container(row: &Row, palette: Palette, locale: Language) -> Div {
    div().relative().w_full().px_3().pt(row.inset()).children(
        row.date
            .as_deref()
            .filter(|_| row.starts_day)
            .map(|date| date_separator(palette, date, row.id, locale)),
    )
}

/// Рисует дату перед первым сообщением дня, не добавляя отдельный адрес в список.
fn date_separator(palette: Palette, date: &str, id: Uuid, locale: Language) -> Div {
    let caption = locale.tr_with(
        "desktop.conversation.history.date",
        &[
            ("year", &date[..4]),
            ("month", &date[5..7]),
            ("day", &date[8..10]),
        ],
    );
    div()
        .absolute()
        .top_2()
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            label(palette, TextRole::Caption, caption)
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
    media: Option<&'a Cards>,             // Карточки вложений рабочего чата.
    accessory: Option<AnyElement>, // Управляемый блок, который появляется внутри бабла на стенде.
    fragments: Option<&'a FragmentViews>, // Кэш спойлеров этого списка.
    app: Option<&'a mut App>,      // Контекст для создания элементов видимой строки.
}

/// Рисует сообщение с общим пузырём и оставляет место аватару в группе собеседника.
fn message_row(
    row: &Row,
    palette: Palette,
    peer: Option<&PeerLabel>,
    history: (&[Row], &HashMap<Uuid, usize>),
    controls: &HistoryControls,
    photo: Option<ImageSource>,
    mut extras: RowBodyExtras<'_>,
) -> (Div, Option<TextLayout>) {
    let author = peer.map_or_else(
        || controls.locale.tr("desktop.messenger.participant").into(),
        |peer| peer.name.clone(),
    );
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
    let confirmed = controls.reply.as_ref().filter(|_| row.date.is_some() && row.service.is_none());
    let reply = confirmed.filter(|_| row.encrypted.is_none() && !row.body.text.trim().is_empty());
    let bubble = MessageBubble {
        author: (!row.own && row.starts_group).then_some(author),
        body: row.body.text.clone(),
        time: Some(row.time.caption(controls.locale)),
        own: row.own,
        delivery: row.delivery,
    };
    let (rows, index) = history;
    let mut text_layout = None;
    let mut readable = false;
    let content = if let Some(state) = &row.encrypted {
        let text = match state {
            EncryptedPresentation::Ready(text) if text.with_current(|| ()).is_some() => {
                let body = if let Some(layout) = controls.protected.get(&id) {
                    layout.render(
                        id,
                        text,
                        bubble_palette,
                        row.own,
                        extras
                            .media
                            .zip(extras.app)
                            .map(|(cards, app)| (cards, app, controls.locale)),
                        |position, palette, own, media| {
                            let slot = QuoteSlot {
                                message: id,
                                position,
                            };
                            render_quote(
                                controls.quotes.get(&slot),
                                palette,
                                own,
                                slot,
                                controls.locale,
                                media,
                            )
                        },
                    )
                } else {
                    text_role(bubble_palette, TextRole::Body)
                        .min_w_0()
                        .max_w_full()
                        .child(ProtectedText(text.clone()))
                };
                let call = row
                    .date
                    .is_some()
                    .then(|| {
                        join_card(
                            id,
                            text,
                            controls.jitsi.as_deref(),
                            controls.conversation.as_ref(),
                            bubble_palette,
                            controls.locale,
                        )
                    })
                    .flatten();
                Some(if let Some(call) = call {
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(body)
                        .child(call)
                        .into_any_element()
                } else {
                    body.into_any_element()
                })
            }
            _ => None,
        };
        readable = text.is_some();
        Some(text.unwrap_or_else(|| {
            encrypted_placeholder(id, matches!(state, EncryptedPresentation::Failed), bubble_palette, controls)
        }))
    } else if let Some(service) = &row.service {
        Some(key_service_body(id, service, bubble_palette, controls))
    } else if let Some(cards) = extras.media.filter(|_| !row.media.is_empty()) {
        let caption = div().flex().flex_col().min_w_0().max_w_full().gap_1();
        let prepared = row
            .body
            .caption
            .as_ref()
            .expect("подпись для строки с медиа");
        let caption = if let Some(reply) = reply.filter(|_| row.body.plain()) {
            caption.children(prepared.spans.iter().map(|(span, visible)| {
                reply.selectable_span(
                    id,
                    &row.body.text,
                    span.clone(),
                    visible.clone(),
                    bubble_palette,
                )
            }))
        } else {
            let content = quote_content(
                row,
                &prepared.parts,
                bubble_palette,
                peer,
                (rows, index),
                FragmentContent::new(controls, extras.fragments, extras.app.as_deref_mut()),
            )
            .or_else(|| {
                (!prepared.text.trim().is_empty()).then(|| {
                    label(bubble_palette, TextRole::Body, prepared.text.clone()).into_any_element()
                })
            });
            caption.children(content)
        };
        Some(
            caption
                .children(row.media.iter().enumerate().map(|(position, reference)| {
                    let key = AttachmentKey {
                        message: id,
                        file: reference.file_id,
                        position,
                    };
                    let card = cards.render(
                        key,
                        reference,
                        bubble_palette,
                        controls.locale,
                        extras
                            .app
                            .as_deref_mut()
                            .expect("контекст карточек истории"),
                    );
                    let Some(conversation) = controls.conversation.clone().filter(|_| {
                        reference.kind == workspace_core::storage::media::MediaKind::Image
                    }) else {
                        return card;
                    };
                    let original = reference
                        .name
                        .as_deref()
                        .unwrap_or(&reference.label)
                        .rsplit_once('.')
                        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("heic") || ext.eq_ignore_ascii_case("heif"));
                    card.on_mouse_down(MouseButton::Right, move |event, window, cx| {
                        let _ = conversation.update(cx, |view, cx| {
                            view.open_message_menu(id, event.position, MenuHit::Image { key, original }, window, cx)
                        });
                        cx.stop_propagation();
                    })
                }))
                .into_any_element(),
        )
    } else {
        if row.body.plain() && !row.body.text.trim().is_empty() {
            let (body, layout) = if let Some(controls) = reply {
                let (body, layout) = controls.selectable(id, &row.body.text, bubble_palette);
                (body.min_w_0().max_w_full().into_any_element(), layout)
            } else {
                let text = StyledText::new(row.body.text.clone());
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
            quote_content(
                row,
                &row.body.parts,
                bubble_palette,
                peer,
                (rows, index),
                FragmentContent::new(controls, extras.fragments, extras.app.as_deref_mut()),
            )
        }
    };
    let accessory = extras.accessory.or_else(|| {
        let canonical = row.retry_send?;
        let conversation = controls.conversation.clone()?;
        Some(
            Button::new(
                (gpui::ElementId::from(id), "retry-protected-send"),
                controls.locale.tr("message.retrySend"),
            )
            .render(bubble_palette, None, move |_, cx| {
                let _ = conversation
                    .update(cx, |view, cx| view.retry_protected_send(canonical, cx));
            })
            .into_any_element(),
        )
    });
    let confirmed_message = row.date.is_some() && row.service.is_none();
    let reactions = controls.reactions.as_ref().filter(|_| {
        confirmed_message && (row.encrypted.is_none() || readable)
    });
    let chips = reactions.and_then(|reactions| reaction_chips(id, reactions, palette, controls.locale));
    let accessory = match (accessory, chips) {
        (Some(existing), Some(chips)) => Some(div().flex().flex_col().gap_1().child(existing).child(chips).into_any_element()),
        (existing, chips) => existing.or(chips),
    };
    let commands = (!controls.quick_hidden && confirmed_message)
        .then(|| quick_command(row, controls, reply.is_some(), readable))
        .flatten()
        .into_iter()
        .collect::<Vec<_>>();
    let reserve_reaction_corner = reactions.is_some();
    let reactions = reactions.filter(|_| !controls.quick_hidden).and_then(|reactions| {
        let target = reactions.clone();
        QuickReactions::new(reactions.quick(id), move |emoji, cx| target.toggle(id, emoji, cx))
    });
    let has_reactions = reactions.is_some();
    let show_quick_actions = !commands.is_empty() || has_reactions;
    let mut bubble = match (content, accessory, show_quick_actions) {
        (content, accessory, true) => bubble.render_with_overlay(
            bubble_palette, content, row.ends_group, accessory,
            div().absolute().inset_0()
                .debug_selector(move || format!("history-quick-bubble-{id}"))
                .child(QuickActions { id, own: row.own, palette, commands, reactions }),
        ),
        (content, Some(accessory), false) => {
            bubble.render_with_accessory(bubble_palette, content, row.ends_group, accessory)
        }
        (Some(content), None, false) => bubble.render_with_body(bubble_palette, content, row.ends_group),
        (None, None, false) => bubble.render_in_group(bubble_palette, row.ends_group),
    }
    .debug_selector(move || format!("history-message-{id}"));
    if row.delivery == Some(DeliveryMark::Unknown) {
        bubble = bubble.debug_selector(|| "history-delivery-unknown".into());
    }
    if readable {
        bubble = bubble.debug_selector(move || format!("history-encrypted-{id}-readable"));
    }
    let bubble = match confirmed {
        Some(controls_row) => controls_row.row(id, bubble).into_any_element(),
        None => bubble.into_any_element(),
    };
    let message = div()
        .w_full()
        .flex()
        .items_end()
        .gap_2()
        .when(!row.own, |view| {
            let avatar = peer.map(|peer| peer.initials.clone()).unwrap_or_default();
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
            div()
                .flex_1()
                .min_w_0()
                // Место для колонки остаётся у угла пузыря и при скрытой панели.
                .when(reserve_reaction_corner, |slot| {
                    if row.own { slot.pl(px(20.)) } else { slot.pr(px(20.)) }
                })
                .child(bubble),
        );
    (message, text_layout)
}

/// Выбирает команду для открытого текста или запроса ключа у закрытого сообщения.
fn quick_command(row: &Row, controls: &HistoryControls, plain_reply: bool, readable: bool) -> Option<QuickCommand> {
    let id = row.id;
    let conversation = controls.conversation.clone()?;
    let locale = controls.locale;
    match &row.encrypted {
        Some(EncryptedPresentation::Ready(access)) if readable && controls.edit_protected => {
            let access = access.clone();
            Some(QuickCommand::on_hover("reply-protected-message", locale.tr("message.reply"), move |window, cx| {
                let _ = conversation.update(cx, |view, cx| {
                    view.reply_to_protected_body(id, access.clone(), window, cx)
                });
            }))
        }
        Some(state) if !readable && controls.key_device.is_some() => {
            let (action, name, caption) = if matches!(state, EncryptedPresentation::Failed) {
                (KeyAction::Recover, "recover-protected-key", locale.tr("desktop.messenger.history.requestReplacementKey"))
            } else {
                (KeyAction::Request, "request-protected-key", locale.tr("e2ee.requestMessageKey"))
            };
            let disabled = pending_key(controls.key_actions.get(&id));
            Some(QuickCommand::on_hover(name, caption, move |_, cx| {
                let _ = conversation.update(cx, |view, cx| view.key_action(id, action, cx));
            }).always(Icon::ChatLock, disabled))
        }
        None if plain_reply && controls.quick_reply => {
            Some(QuickCommand::on_hover("history-reply", locale.tr("message.reply"), move |window, cx| {
                let _ = conversation.update(cx, |view, cx| view.quick_reply(id, window, cx));
            }))
        }
        _ => None,
    }
}

/// Показывает безопасную заглушку, сохраняя общее оформление и UUID пузыря.
fn encrypted_placeholder(
    id: Uuid,
    failed: bool,
    palette: Palette,
    controls: &HistoryControls,
) -> AnyElement {
    let locale = controls.locale;
    let (icon, caption, retry) = if failed {
        (
            Icon::Error,
            locale.tr("e2ee.messageDecryptError"),
            controls.conversation.clone(),
        )
    } else {
        (
            Icon::Info,
            locale.tr("desktop.messenger.history.encryptedMessage"),
            None,
        )
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .debug_selector(move || {
            format!(
                "history-encrypted-{id}-{}",
                if failed { "failed" } else { "locked" }
            )
        })
        .child(
            div()
                .flex()
                .items_start()
                .gap_1()
                .child(icon.element(px(16.), palette.text_secondary))
                .child(label(palette, TextRole::Body, caption)),
        )
        .children(retry.map(|conversation| {
            Button::new(
                (gpui::ElementId::from(id), "retry-encrypted"),
                locale.tr("e2ee.retry"),
            )
            .render(palette, None, move |_, cx| {
                let _ = conversation.update(cx, |view, cx| view.retry_encrypted(id, cx));
            })
        }))
        .children(key_action_explanation(controls.key_actions.get(&id), locale)
            .map(|text| label(palette, TextRole::Caption, text)))
        .into_any_element()
}

/// Показывает действие и результат на той строке, где пользователь его начал.
fn key_action_row(
    id: Uuid,
    actions: &[(KeyAction, &'static str)],
    palette: Palette,
    controls: &HistoryControls,
) -> Div {
    let locale = controls.locale;
    let status = controls.key_actions.get(&id);
    let terminal = pending_key(status);
    let mut row = div().flex().flex_wrap().gap_1();
    if let Some(conversation) = &controls.conversation {
        for (action, caption) in actions.iter().copied() {
            let target = conversation.clone();
            let name = match action {
                KeyAction::Request => "request-protected-key",
                KeyAction::Recover => "recover-protected-key",
                KeyAction::Grant => "grant-protected-key",
                KeyAction::Reject => "reject-protected-key",
            };
            let mut button = Button::new((gpui::ElementId::from(id), name), caption);
            button.disabled = terminal;
            row = row.child(
                button
                    .render(palette, None, move |_, cx| {
                        let _ = target.update(cx, |view, cx| view.key_action(id, action, cx));
                    })
                    .debug_selector(move || format!("{name}-{id}")),
            );
        }
    }
    row.children(key_action_explanation(status, locale).map(|text| label(palette, TextRole::Caption, text)))
}

/// Блокирует новый запрос до окончания уже начатой операции.
fn pending_key(status: Option<&KeyActionState>) -> bool {
    matches!(status, Some(KeyActionState::Running(_) | KeyActionState::Waiting(_) | KeyActionState::ReplySent(_)))
}

/// Подписывает ход обмена ключами для строки запроса или закрытого сообщения.
fn key_action_explanation(status: Option<&KeyActionState>, locale: Language) -> Option<&'static str> {
    match status {
        Some(KeyActionState::Running(_)) => Some(locale.tr("message.sending")),
        Some(KeyActionState::Waiting(_)) => {
            Some(locale.tr("desktop.messenger.history.waitingForKeyReply"))
        }
        Some(KeyActionState::Rejected) => Some(locale.tr("e2ee.keyRejectTitle")),
        Some(KeyActionState::ReplySent(
            workspace_core::messenger::encryption::key_exchange::KeyRequestOutcome::Granted,
        )) => Some(locale.tr("e2ee.serviceRequestGranted")),
        Some(KeyActionState::ReplySent(
            workspace_core::messenger::encryption::key_exchange::KeyRequestOutcome::Rejected,
        )) => Some(locale.tr("desktop.messenger.history.requestDeclined")),
        Some(KeyActionState::Unavailable) => {
            Some(locale.tr("desktop.messenger.history.keyActionUnavailable"))
        }
        Some(KeyActionState::Failed) => {
            Some(locale.tr("desktop.messenger.history.keyActionFailed"))
        }
        None => None,
    }
}

/// Подписывает служебную историю и предлагает ответ только на открытые запросы другого устройства.
fn key_service_body(
    id: Uuid,
    service: &KeyServiceRow,
    palette: Palette,
    controls: &HistoryControls,
) -> AnyElement {
    let locale = controls.locale;
    let caption = match service {
        KeyServiceRow::Request { .. } => locale.tr("e2ee.keyRequestTitle"),
        KeyServiceRow::Grant => locale.tr("e2ee.serviceRequestGranted"),
        KeyServiceRow::Reject => locale.tr("e2ee.keyRejectTitle"),
        KeyServiceRow::Announced => locale.tr("desktop.messenger.history.chatKeyUpdated"),
    };
    let mut body = div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w_0()
        .child(label(palette, TextRole::Body, caption))
        .debug_selector(move || format!("history-key-service-{id}"));
    if let KeyServiceRow::Request { device, name } = service {
        body = body.child(label(palette, TextRole::Caption, name.clone()));
        if controls
            .key_device
            .as_ref()
            .is_some_and(|local| local != device)
            && let Some(permission) = controls.key_permissions.get(&id)
        {
            let mut actions = Vec::new();
            if permission.grant {
                actions.push((KeyAction::Grant, locale.tr("e2ee.serviceTransferKey")));
            }
            if permission.reject {
                actions.push((KeyAction::Reject, locale.tr("e2ee.rejectAccess")));
            }
            body = body.child(key_action_row(id, &actions, palette, controls));
        }
    }
    body.into_any_element()
}

/// Выбирает подписанного автора, не выдавая имя собеседника за участника канала.
fn row_author(row: &Row, peer: Option<&str>) -> Option<ReplyText> {
    if row.own {
        Some(ReplyText::Label("common.you"))
    } else {
        peer.map(|name| ReplyText::Source(name.to_owned()))
    }
}

/// Передаёт только возможности текущего экрана и кэш видимых элементов.
struct FragmentContent<'a> {
    views: Option<&'a FragmentViews>, // Ячейки с отдельным состоянием спойлеров.
    runtime: Option<&'a FragmentRuntime>, // Действующий владелец рабочего запроса.
    app: Option<&'a mut App>,         // Контекст при создании элемента из строки списка.
    locale: Language,                 // Язык нейтрального заполнителя.
    conversation: Option<&'a WeakEntity<Conversation>>, // Владелец переходов к оригиналам.
}

impl<'a> FragmentContent<'a> {
    /// Связывает одну раскладку тела с её рабочим экраном.
    fn new(
        controls: &'a HistoryControls,
        views: Option<&'a FragmentViews>,
        app: Option<&'a mut App>,
    ) -> Self {
        Self {
            views,
            runtime: controls.fragments.as_ref(),
            app,
            locale: controls.locale,
            conversation: controls.conversation.as_ref(),
        }
    }

    /// Показывает один спойлер, скрывая адрес даже в истории без исполнительного процесса.
    fn render(
        &mut self,
        slot: (Uuid, usize),
        reference: Option<&workspace_core::messenger::encryption::fragments::FragmentReference>,
        palette: Palette,
    ) -> AnyElement {
        if let Some(views) = self.views {
            return views.element(
                slot,
                reference,
                self.runtime,
                palette,
                self.locale,
                self.app.as_deref_mut(),
            );
        }
        label(
            palette,
            TextRole::Caption,
            self.locale.tr("e2ee.secretFragment"),
        )
        .into_any_element()
    }
}

/// Ставит спойлеры рядом с открытыми кусками и оставляет цитаты отдельными блоками.
fn quote_content(
    row: &Row,
    blocks: &[BodyPart],
    palette: Palette,
    peer: Option<&PeerLabel>,
    (rows, index): (&[Row], &HashMap<Uuid, usize>),
    mut fragments: FragmentContent<'_>,
) -> Option<gpui::AnyElement> {
    if !row.body.has_fragments && matches!(blocks, [BodyPart::Text(_)]) {
        return None;
    }
    let mut body = div()
        .flex()
        .flex_col()
        .min_w_0()
        .w(px(312.))
        .max_w_full()
        .gap_1();
    let mut inline = Vec::new();
    let id = row.id;
    for (position, block) in blocks.iter().enumerate() {
        match block {
            BodyPart::Text(text) if row.body.has_fragments => inline.push(
                label(palette, TextRole::Body, text.clone())
                    .min_w_0()
                    .into_any_element(),
            ),
            BodyPart::Text(text) => body = body.child(label(palette, TextRole::Body, text.clone())),
            BodyPart::Preview(preview) => {
                body = body.child(label(
                    palette,
                    TextRole::Body,
                    fragments.locale.preview_text(preview),
                ));
            }
            BodyPart::Fragment { slot, reference } => {
                inline.push(fragments.render((id, *slot), reference.as_ref(), palette))
            }
            BodyPart::Forward(forward) => {
                if !inline.is_empty() {
                    body = body.child(fragment_line(std::mem::take(&mut inline)));
                }
                body = body.child(forward_card(
                    id,
                    position,
                    forward.as_ref(),
                    palette,
                    &mut fragments,
                ));
            }
            BodyPart::Quote {
                id: source_id,
                author,
                fragment,
            } => {
                if !inline.is_empty() {
                    body = body.child(fragment_line(std::mem::take(&mut inline)));
                }
                let source = index
                    .get(source_id)
                    .map(|&index| &rows[index])
                    .filter(|source| {
                        source.id != id
                            && source.date.is_some()
                            && source.encrypted.is_none()
                            && source.service.is_none()
                    });
                let author = source
                    .and_then(|source| {
                        if source.own {
                            Some(fragments.locale.tr("common.you").into())
                        } else {
                            peer.map(|peer| peer.name.clone())
                        }
                    })
                    .unwrap_or_else(|| author.clone());
                let snippet = fragment
                    .clone()
                    .filter(|text| {
                        workspace_core::messenger::encryption::fragments::fragment_display_spans(
                            text,
                        )
                        .is_empty()
                            && workspace_core::messenger::forward::forward_spans(text).is_empty()
                    })
                    .or_else(|| source.map(|source| source.body.excerpt(fragments.locale)))
                    .unwrap_or_default();
                body = body.child(
                    history_quote(
                        palette,
                        row.own,
                        author,
                        snippet,
                        source.is_some(),
                        fragments.locale,
                    )
                    .debug_selector(move || format!("history-quote-{id}-{position}")),
                );
            }
        }
    }
    if !inline.is_empty() {
        body = body.child(fragment_line(inline));
    }
    Some(body.into_any_element())
}

/// Рисует снимок пересылки. Спойлеры сохраняют исходные чаты и свои отдельные слоты.
fn forward_card(
    id: Uuid,
    position: usize,
    reference: Option<&ForwardBody>,
    palette: Palette,
    fragments: &mut FragmentContent<'_>,
) -> gpui::Stateful<Div> {
    let mut card = div()
        .id(("forward-card", position))
        .flex()
        .flex_col()
        .min_w_0()
        .gap_1()
        .px_2()
        .py_1()
        .border_l_2()
        .border_color(palette.accent);
    let mut header = div().flex().flex_col().min_w_0().gap_1().child(label(
        palette,
        TextRole::Caption,
        fragments.locale.tr("message.forwarded"),
    ));
    let Some(reference) = reference else {
        return card.child(header);
    };
    if !reference.author.is_empty() {
        header = header.child(label(palette, TextRole::Label, reference.author.clone()));
    }
    if let Some(source) = &reference.source {
        header = header.child(label(palette, TextRole::Caption, source.clone()));
    }
    card = if let Some(conversation) = fragments.conversation.cloned() {
        let target = reference.id;
        let header = header
            .id((gpui::ElementId::from(target), "open-forward-original"))
            .border_1()
            .border_color(gpui::transparent_black());
        card.child(
            activate(header, None, palette, move |_, cx| {
                cx.stop_propagation();
                let _ =
                    conversation.update(cx, |view, cx| view.open_forward_original(id, target, cx));
            })
            .debug_selector(move || format!("forward-original-{id}-{target}")),
        )
    } else {
        card.child(header)
    };
    let mut inline = Vec::new();
    for (position, part) in reference.parts.iter().enumerate() {
        match part {
            BodyPart::Text(text) => {
                inline.push(
                    label(palette, TextRole::Body, text.clone())
                        .min_w_0()
                        .into_any_element(),
                );
            }
            BodyPart::Preview(preview) => inline.push(
                label(
                    palette,
                    TextRole::Body,
                    fragments.locale.preview_text(preview),
                )
                .min_w_0()
                .into_any_element(),
            ),
            BodyPart::Fragment { slot, reference } => {
                inline.push(fragments.render((id, *slot), reference.as_ref(), palette));
            }
            BodyPart::Quote { author, .. } => {
                inline.push(label(palette, TextRole::Caption, author.clone()).into_any_element());
            }
            BodyPart::Forward(reference) => {
                if !inline.is_empty() {
                    card = card.child(fragment_line(std::mem::take(&mut inline)));
                }
                card = card.child(forward_card(
                    id,
                    position,
                    reference.as_ref(),
                    palette,
                    fragments,
                ));
            }
        }
    }
    if !inline.is_empty() {
        card = card.child(fragment_line(inline));
    }
    card
}

/// Укладывает открытый текст и фрагменты в строки доступной ширины бабла.
fn fragment_line(children: Vec<AnyElement>) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .min_w_0()
        .gap_1()
        .children(children)
}

/// Готовит историю и добавляет уникальные строки очереди только после подтверждённого хвоста.
fn build_rows(
    history: &HistoryWindow,
    outbox: &[OutboxEntry],
    user: Option<Uuid>,
    encryption: Option<&HistoryEncryption>,
    prepared: &mut HashMap<Uuid, CachedBody>,
) -> Vec<Row> {
    let mut rows: Vec<_> = history
        .messages
        .iter()
        .map(|message| {
            let own = user.is_some() && message.metadata.author_id == user;
            let (encrypted, text) = match &message.body {
                MessageBody::Markdown(text) => (None, text.as_str()),
                MessageBody::Encrypted { .. } => {
                    let state = encryption
                        .and_then(|encryption| encryption.presentation(message.id))
                        .cloned()
                        .unwrap_or(EncryptedPresentation::Locked);
                    (Some(state), "")
                }
                MessageBody::KeyService { .. } => (None, ""),
            };
            let service = match &message.body {
                MessageBody::KeyService { payload, .. } => Some(match payload {
                    KeyServicePayload::Request {
                        device_id,
                        device_name,
                        ..
                    } => KeyServiceRow::Request {
                        device: device_id.clone(),
                        name: device_name.clone(),
                    },
                    KeyServicePayload::Grant { .. } => KeyServiceRow::Grant,
                    KeyServicePayload::Reject(_) => KeyServiceRow::Reject,
                    KeyServicePayload::Announced { .. } => KeyServiceRow::Announced,
                }),
                _ => None,
            };
            let stamp = message.metadata.created_at.as_str();
            let (body, media) =
                prepare_row_body(prepared, message.id, text, true, encrypted.is_some());
            let plain_text =
                encrypted.is_none() && service.is_none() && media.is_empty() && body.plain();
            Row {
                id: message.id,
                own,
                author: message.metadata.author_id,
                topic: message.topic_id,
                date: Some(stamp[..10].to_owned()),
                body,
                encrypted,
                service,
                retry_send: None,
                plain_text,
                media,
                time: RowTime::Sent(stamp[11..16].to_owned()),
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
                .map(|entry| {
                    let id = entry.snapshot.public_id;
                    let encrypted = entry.snapshot.body.markdown().is_none();
                    let state = encrypted.then(|| {
                        encryption
                            .and_then(|scope| scope.presentation(id))
                            .cloned()
                            .unwrap_or(EncryptedPresentation::Locked)
                    });
                    let retry_send = (encrypted
                        && matches!(
                            entry.status,
                            DeliveryStatus::Unknown
                                | DeliveryStatus::Blocked(_)
                                | DeliveryStatus::Rejected(_)
                        ))
                    .then_some(entry.snapshot.canonical_id);
                    Row {
                        id,
                        own: true,
                        author: user,
                        topic: entry.snapshot.topic_id,
                        date: None,
                        body: prepare_row_body(
                            prepared,
                            entry.snapshot.public_id,
                            entry.snapshot.body.markdown().unwrap_or_default(),
                            false,
                            encrypted,
                        )
                        .0,
                        encrypted: state,
                        service: None,
                        retry_send,
                        plain_text: false,
                        media: Vec::new(),
                        time: RowTime::Pending(delivery_caption_key(entry.status)),
                        delivery: Some(match entry.status {
                            DeliveryStatus::Waiting | DeliveryStatus::Sending => {
                                DeliveryMark::Pending
                            }
                            DeliveryStatus::Unknown => DeliveryMark::Unknown,
                            DeliveryStatus::Blocked(_) | DeliveryStatus::Rejected(_) => {
                                DeliveryMark::Failed
                            }
                        }),
                        starts_day: false,
                        starts_group: false,
                        ends_group: false,
                    }
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

/// Переиспользует разбор того же UUID, тела и вида строки после новой страницы или события.
fn prepare_row_body(
    cache: &mut HashMap<Uuid, CachedBody>,
    id: Uuid,
    text: &str,
    confirmed: bool,
    encrypted: bool,
) -> (Rc<Body>, Vec<MediaReference>) {
    if let Some(previous) = cache.get(&id).filter(|row| {
        row.confirmed == confirmed && row.encrypted == encrypted && row.body.text.as_ref() == text
    }) {
        return (previous.body.clone(), previous.media.clone());
    }
    let media = if confirmed && !encrypted {
        media_references(text)
    } else {
        Vec::new()
    };
    let body = Rc::new(Body::new(text, &media));
    cache.insert(
        id,
        CachedBody {
            body: body.clone(),
            media: media.clone(),
            confirmed,
            encrypted,
        },
    );
    (body, media)
}

/// Определяет подпись очереди, чтобы ожидание и неизвестный исход различались.
fn delivery_caption_key(status: DeliveryStatus) -> &'static str {
    match status {
        DeliveryStatus::Waiting => "desktop.messenger.history.deliveryQueued",
        DeliveryStatus::Sending => "message.sending",
        DeliveryStatus::Unknown => "desktop.messenger.history.deliveryUnknown",
        DeliveryStatus::Blocked(SendBlock::AccessLost) => {
            "desktop.messenger.history.deliveryNoAccess"
        }
        DeliveryStatus::Blocked(SendBlock::EncryptionUnavailable) => {
            "desktop.messenger.history.deliveryPlaintextUnavailable"
        }
        DeliveryStatus::Rejected(_) => "desktop.messenger.history.deliveryRejected",
    }
}
