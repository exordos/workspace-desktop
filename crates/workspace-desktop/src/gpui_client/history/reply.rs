//! Выбирает сообщение или фрагмент из истории и передаёт намерение в редактор разговора.

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::{
    Context, Deferred, Div, FocusHandle, HighlightStyle, KeyDownEvent, MouseButton, Pixels, Point,
    SharedString, Stateful, StyledText, TextLayout, WeakEntity, Window, div, prelude::*, px,
};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;
use workspace_core::messenger::quote::{self, Block};

use super::{Conversation, State};
use crate::gpui_client::{
    composer::{ReplyAction, ReplyQuote},
    ui::{
        Button, ButtonAppearance, Icon, Palette, QuoteSurface, TextRole, label,
        popup::{MenuInput, menu_layer, menu_navigation, menu_surface},
        quote_frame, text_role,
    },
};

/// Выделение одного проверенного снимка тела.
struct Fragment {
    id: Uuid,           // Сообщение, в котором началось выделение.
    body: SharedString, // Версия текста, из которой рассчитаны байты.
    anchor: usize,      // Исходная позиция курсора.
    caret: usize,       // Последняя позиция во время растягивания диапазона.
}

impl Fragment {
    /// Возвращает диапазон независимо от направления мыши.
    fn range(&self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }
}

/// Цель быстрого ответа и выделение одной строки виртуального списка.
#[derive(Default)]
pub(super) struct Selection {
    fragment: Option<Fragment>, // Выделение мышью. Допустим только текущий текст источника.
    hovered: Option<Uuid>,      // Сообщение под указателем.
    focused: Option<Uuid>,      // Последняя строка, на которой установлен фокус истории.
}

impl Selection {
    /// Сбрасывает указатели на исчезнувшие строки и выделение старой версии тела.
    pub(super) fn retain_current<'a>(&mut self, body: impl Fn(Uuid) -> Option<&'a str>) {
        if let Some(fragment) = &self.fragment
            && body(fragment.id) != Some(fragment.body.as_ref())
        {
            if self.focused == Some(fragment.id) {
                self.focused = None;
            }
            self.fragment = None;
        }
        self.hovered = self.hovered.filter(|&id| body(id).is_some());
        self.focused = self.focused.filter(|&id| body(id).is_some());
    }

    /// Выбирает выделенный фрагмент, затем строку с фокусом или под указателем.
    fn target(&self, history_focused: bool) -> Option<Uuid> {
        self.fragment
            .as_ref()
            .filter(|fragment| !fragment.range().is_empty())
            .map(|fragment| fragment.id)
            .or_else(|| history_focused.then_some(self.focused).flatten())
            .or(self.hovered)
    }

    /// Берёт только текст, выделенный в совпадающей версии строки.
    fn fragment(&self, id: Uuid, text: &str) -> Option<String> {
        let selected = self.fragment.as_ref()?;
        if selected.id != id || selected.body.as_ref() != text {
            return None;
        }
        let fragment = text.get(selected.range())?;
        (!fragment.trim().is_empty()).then(|| fragment.to_owned())
    }
}

/// Общая привязка живых строк к открытому экрану и одному выделению.
#[derive(Clone)]
pub(super) struct ReplyControls {
    pub conversation: WeakEntity<Conversation>, // Действия закрытого разговора пропадут.
    pub focus: FocusHandle,                     // Точка фокуса истории для быстрого ответа.
    pub selection: Rc<RefCell<Selection>>,      // Общий диапазон из одной строки списка.
}

impl ReplyControls {
    /// Оборачивает простой текст с живым выделением, не меняя метрики пузыря.
    pub(super) fn selectable(
        &self,
        id: Uuid,
        body: &str,
        palette: Palette,
    ) -> (Stateful<Div>, TextLayout) {
        self.selectable_text(id, body, 0..body.len(), palette)
    }

    /// Показывает часть тела и сохраняет выделение в координатах исходного сообщения.
    pub(super) fn selectable_span(
        &self,
        id: Uuid,
        body: &str,
        span: Range<usize>,
        palette: Palette,
    ) -> Stateful<Div> {
        self.selectable_text(id, body, span, palette).0
    }

    /// Возвращает текст и его фактическую разметку для выделения и фиксации позиции.
    fn selectable_text(
        &self,
        id: Uuid,
        body: &str,
        span: Range<usize>,
        palette: Palette,
    ) -> (Stateful<Div>, TextLayout) {
        let body: SharedString = body.to_owned().into();
        let visible: SharedString = body[span.clone()].to_owned().into();
        let start = span.start;
        let end = span.end;
        let range = self
            .selection
            .borrow()
            .fragment
            .as_ref()
            .filter(|selected| selected.id == id && selected.body == body)
            .map(Fragment::range)
            .and_then(|range| {
                let clipped = range.start.max(start)..range.end.min(end);
                (!clipped.is_empty()).then(|| clipped.start - start..clipped.end - start)
            });
        let styled = StyledText::new(visible.clone()).with_highlights(range.map(|range| {
            (
                range,
                HighlightStyle {
                    background_color: Some(palette.accent_soft),
                    ..HighlightStyle::default()
                },
            )
        }));
        let layout = styled.layout().clone();
        let drag_layout = layout.clone();
        let mouse_layout = layout.clone();
        let controls = self.clone();
        let drag = self.selection.clone();
        let drag_visible = visible.clone();
        let element = text_role(palette, TextRole::Body)
            .id(SharedString::from(format!(
                "selectable-history-{id}-{start}"
            )))
            .debug_selector(move || format!("history-reply-body-{id}"))
            .cursor(gpui::CursorStyle::IBeam)
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                let index = start + byte_at(&mouse_layout, &visible, event.position);
                let mut selection = controls.selection.borrow_mut();
                selection.focused = Some(id);
                selection.fragment = Some(Fragment {
                    id,
                    body: body.clone(),
                    anchor: index,
                    caret: index,
                });
                window.focus(&controls.focus);
                window.refresh();
                cx.stop_propagation();
            })
            .on_mouse_move(move |event, window, _| {
                if event.dragging()
                    && let Some(selection) = drag.borrow_mut().fragment.as_mut()
                    && selection.id == id
                    && (start..=end).contains(&selection.anchor)
                {
                    let index = start + byte_at(&drag_layout, &drag_visible, event.position);
                    if index != selection.caret {
                        selection.caret = index;
                        window.refresh();
                    }
                }
            })
            .child(styled);
        (element, layout)
    }

    /// Добавляет быстрый ответ и открывает меню по правому клику той же строки.
    pub(super) fn row(&self, id: Uuid, inner: Div, palette: Palette) -> Stateful<Div> {
        let target = self.conversation.clone();
        let menu = self.conversation.clone();
        let select = self.selection.clone();
        let focus = self.focus.clone();
        let focus_select = self.selection.clone();
        let mut button = Button::new((gpui::ElementId::Uuid(id), "history-reply"), "");
        button.appearance = ButtonAppearance::Ghost;
        button.icon = Some(Icon::Reply);
        inner
            .relative()
            .id(SharedString::from(format!("history-reply-row-{id}")))
            .group("history-reply-row")
            .on_hover(move |hovered, _, _| {
                let mut selection = select.borrow_mut();
                if *hovered {
                    selection.hovered = Some(id);
                } else if selection.hovered == Some(id) {
                    selection.hovered = None;
                }
            })
            .on_mouse_down(MouseButton::Left, move |_, window, _| {
                let mut selection = focus_select.borrow_mut();
                selection.focused = Some(id);
                if selection
                    .fragment
                    .as_ref()
                    .is_some_and(|fragment| fragment.id != id)
                {
                    selection.fragment = None;
                }
                window.focus(&focus);
                window.refresh();
            })
            .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                let _ = menu.update(cx, |view, cx| {
                    view.open_reply_menu(id, event.position, window, cx)
                });
                cx.stop_propagation();
            })
            .child(
                div().absolute().right_1().top(px(-14.)).child(
                    button
                        .render(palette, None, move |window, cx| {
                            let _ = target.update(cx, |view, cx| {
                                view.reply_to(id, ReplyAction::Replace, window, cx)
                            });
                        })
                        .bg(palette.surface)
                        .opacity(0.)
                        .tab_stop(false)
                        .group_hover("history-reply-row", |style| style.opacity(1.))
                        .when(palette.keyboard_focus_visible(), |button| {
                            button.focus(|style| style.opacity(1.).border_color(palette.focus))
                        })
                        .tooltip(move |_, app| {
                            crate::gpui_client::ui::popup::tooltip(palette, "Ответить".into(), app)
                        })
                        .debug_selector(move || format!("history-reply-{id}")),
                ),
            )
    }
}

/// Приводит координату к целой границе UTF-8 внутри нужной строки.
fn byte_at(layout: &TextLayout, text: &str, position: Point<Pixels>) -> usize {
    let index = layout
        .index_for_position(position)
        .unwrap_or_else(|index| index);
    let index = text.floor_char_boundary(index.min(text.len()));
    let Some(letter) = text[index..].graphemes(true).next() else {
        return index;
    };
    let next = index + letter.len();
    if let (Some(start), Some(end)) = (
        layout.position_for_index(index),
        layout.position_for_index(next),
    ) && start.y == end.y
        && {
            let midpoint = start.x + (end.x - start.x) / 2.;
            if start.x <= end.x {
                position.x >= midpoint
            } else {
                position.x <= midpoint
            }
        }
    {
        return next;
    }
    index
}

/// Строка, чья версия ещё должна совпасть при выборе действия.
struct MenuTarget {
    id: Uuid,              // Публичный UUID в проверенном окне.
    anchor: Point<Pixels>, // Координаты меню у указателя.
    body: String,          // Тело строки на момент открытия, минимум для проверки свежести.
}

/// Меню отвечает или добавляет вкладку для выбранной серверной строки.
pub(super) struct ReplyMenu {
    pub focus: FocusHandle,   // Клавиатурный фокус открытого меню.
    selected: bool,           // Выбран «Добавить ответ», если true.
    item: Option<MenuTarget>, // Исходная версия строки и якорь меню в окне.
}

impl ReplyMenu {
    /// Теряет прежнюю цель при сбросе подтверждённого окна истории.
    pub(super) fn clear(&mut self) {
        self.item = None;
    }

    /// Готовит закрытое меню одного разговора.
    pub(super) fn new(focus: FocusHandle) -> Self {
        Self {
            focus,
            selected: false,
            item: None,
        }
    }
}

impl Conversation {
    /// Выполняет быстрый ответ над выделением, строкой с фокусом или под указателем.
    pub(super) fn reply_hotkey(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stroke = &event.keystroke;
        if event.is_held
            || stroke.key != "l"
            || !stroke.modifiers.control
            || stroke.modifiers.platform
            || stroke.modifiers.shift
            || stroke.modifiers.alt
            || self.reply_menu.item.is_some()
        {
            return;
        }
        let selection = self.viewport.selection();
        let target = selection
            .borrow()
            .target(self.history_focus.is_focused(window));
        if let Some(id) = target
            && self.reply_to(id, ReplyAction::Add, window, cx)
        {
            cx.stop_propagation();
        }
    }

    /// Передаёт проверенное серверное сообщение только редактору текущего разговора.
    fn reply_to(
        &mut self,
        id: Uuid,
        action: ReplyAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(quote) = self.reply_quote(id) else {
            return false;
        };
        let Some(sending) = &self.sending else {
            return false;
        };
        if !sending.view.read(cx).reply_action_enabled(cx) {
            return false;
        }
        let op_id = Uuid::new_v4();
        log::debug!(
            "gpui_history op_id={op_id} event=reply_target_accepted source_id={id} fragment={}",
            quote.fragment.is_some()
        );
        sending
            .view
            .update(cx, |composer, cx| composer.reply(action, quote, window, cx));
        true
    }

    /// Ищет подтверждённую непустую строку этого окна.
    fn reply_message(&self, id: Uuid) -> Option<&workspace_core::storage::HistoryMessage> {
        let State::Ready(history) = &self.state else {
            return None;
        };
        history
            .messages
            .iter()
            .find(|message| message.id == id && !id.is_nil() && !message.text.trim().is_empty())
    }

    /// Получает автора и фрагмент только из актуального окна подтверждённой истории.
    fn reply_quote(&self, id: Uuid) -> Option<ReplyQuote> {
        let message = self.reply_message(id)?;
        let author = self
            .viewport
            .source_author(id, self.reply_peer())
            .unwrap_or_else(|| "Участник".into());
        let fragment = self
            .viewport
            .selection()
            .borrow()
            .fragment(id, &message.text);
        let preview = fragment
            .as_deref()
            .map_or_else(|| excerpt(&message.text), short_text);
        Some(ReplyQuote {
            id,
            author,
            preview,
            fragment,
        })
    }

    /// Показывает сохранённое выделение только если оно ещё есть в доступном сообщении.
    pub(super) fn restored_reply_quote(
        &self,
        id: Uuid,
        fragment: Option<&str>,
    ) -> Option<ReplyQuote> {
        let message = self.reply_message(id)?;
        if fragment.is_some_and(|fragment| !message.text.contains(fragment)) {
            return None;
        }
        let mut quote = self.reply_quote(id)?;
        quote.fragment = fragment.map(str::to_owned);
        quote.preview = fragment.map_or_else(|| excerpt(&message.text), short_text);
        Some(quote)
    }

    /// Использует известное имя из каталога только для обычной лички.
    pub(super) fn reply_peer(&self) -> Option<&str> {
        self.heading
            .direct
            .then_some(self.heading.chat.as_deref())
            .flatten()
    }

    /// Открывает меню только пока исходное сообщение и редактор доступны.
    fn open_reply_menu(
        &mut self,
        id: Uuid,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(message) = self.reply_message(id) else {
            return;
        };
        if !self
            .sending
            .as_ref()
            .is_some_and(|sending| sending.view.read(cx).reply_action_enabled(cx))
        {
            return;
        }
        self.reply_menu.item = Some(MenuTarget {
            id,
            anchor,
            body: message.text.clone(),
        });
        self.reply_menu.selected = false;
        window.focus(&self.reply_menu.focus);
        cx.notify();
    }

    /// Убирает меню при Esc или щелчке снаружи и возвращает фокус истории.
    fn close_reply_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.reply_menu.item.take().is_some() {
            window.focus(&self.history_focus);
            cx.notify();
        }
    }

    /// Выбирает операцию из меню и закрывает его до передачи управления полю ввода.
    fn run_reply_menu(&mut self, action: ReplyAction, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.current_menu_reply().map(|target| target.id);
        self.close_reply_menu(window, cx);
        if let Some(id) = id {
            self.reply_to(id, action, window, cx);
        }
    }

    /// Перемещает клавиатурный выбор в двухстрочном меню.
    fn reply_menu_input(&mut self, input: MenuInput, window: &mut Window, cx: &mut Context<Self>) {
        match input {
            MenuInput::Close => self.close_reply_menu(window, cx),
            MenuInput::Select => self.run_reply_menu(
                if self.reply_menu.selected {
                    ReplyAction::Add
                } else {
                    ReplyAction::Replace
                },
                window,
                cx,
            ),
            MenuInput::Next | MenuInput::Last => self.reply_menu.selected = true,
            MenuInput::Previous | MenuInput::First => self.reply_menu.selected = false,
        }
        cx.notify();
    }

    /// Принимает пункт меню только для исходной, по-прежнему доступной версии строки.
    fn current_menu_reply(&self) -> Option<&MenuTarget> {
        let target = self.reply_menu.item.as_ref()?;
        let current = self.reply_message(target.id)?;
        (current.text == target.body).then_some(target)
    }

    /// Показывает меню через общий слой GPUI, без второго меню на каждую строку.
    pub(super) fn render_reply_menu(
        &mut self,
        window: &mut Window,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Option<[Deferred; 2]> {
        self.reply_menu.item.as_ref()?;
        let Some(anchor) = self.current_menu_reply().map(|target| target.anchor) else {
            self.close_reply_menu(window, cx);
            return None;
        };
        let viewport = window.viewport_size();
        let surface = menu_surface("history-reply-menu", palette, viewport, px(190.), px(100.))
            .on_mouse_down_out(cx.listener(|view, _, window, cx| view.close_reply_menu(window, cx)))
            .children(
                [
                    ("history-reply-replace", "Ответить", ReplyAction::Replace),
                    ("history-reply-add", "Добавить ответ", ReplyAction::Add),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (id, title, action))| {
                    let mut button = Button::new(id, title);
                    button.appearance = ButtonAppearance::Ghost;
                    let target = cx.entity().downgrade();
                    button
                        .render(palette, None, move |window, app| {
                            let _ = target
                                .update(app, |view, cx| view.run_reply_menu(action, window, cx));
                        })
                        .when(self.reply_menu.selected == (index == 1), |row| {
                            row.bg(palette.card_selected)
                        })
                        .debug_selector(move || id.into())
                }),
            );
        Some(menu_layer(
            viewport,
            anchor,
            menu_navigation(surface, &self.reply_menu.focus, cx, Self::reply_menu_input),
        ))
    }
}

/// Показывает доступную в истории цитату или честное пояснение при отсутствии источника.
pub(super) fn history_quote(
    palette: Palette,
    own: bool,
    author: String,
    snippet: String,
    available: bool,
) -> Div {
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .when(!snippet.is_empty(), |body| {
            body.child(label(palette, TextRole::Body, snippet))
        })
        .when(!available, |body| {
            body.child(label(
                palette,
                TextRole::Caption,
                "Исходное сообщение недоступно",
            ))
        });
    quote_frame(
        palette,
        if own {
            QuoteSurface::TintedMessage
        } else {
            QuoteSurface::Message
        },
        author,
        body,
    )
}

/// Берёт краткий текст текущей строки, пропуская уже оформленные ссылки цитирования.
pub(super) fn excerpt(markdown: &str) -> String {
    quote::blocks(markdown)
        .into_iter()
        .find_map(|part| match part {
            Block::Text(text) if !text.trim().is_empty() => Some(short_text(text)),
            _ => None,
        })
        .unwrap_or_else(|| "Цитата".into())
}

/// Ограничивает предпросмотр одним коротким фрагментом без разрезания UTF-8.
fn short_text(text: &str) -> String {
    const WIDTH: usize = 80;
    let text = text.trim();
    match text.char_indices().nth(WIDTH) {
        Some((end, _)) => format!("{}…", text[..end].trim_end()),
        None => text.to_owned(),
    }
}
