//! Рисует однострочный поиск каталога и принимает системный ввод GPUI, включая IME.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementInputHandler,
    EntityInputHandler, EventEmitter, FocusHandle, KeyBinding, MouseButton, MouseDownEvent,
    MouseMoveEvent, Pixels, Point, Render, ShapedLine, TextRun, UTF16Selection, Window, canvas,
    div, fill, point, prelude::*, px, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::gpui_client::ui::{
    Button, ButtonAppearance, FieldAppearance, Icon, Palette, ThemeMode, field,
};

gpui::actions!(
    catalog_search,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        Start,
        End,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Clear,
    ]
);

/// Уведомляет список только об изменении запроса, а не выделения или фокуса.
pub(super) struct QueryChanged;

/// Редактор поиска одного выбранного входа.
pub(super) struct Search {
    text: String,                 // Введённый запрос; позиции хранятся в байтах UTF-8.
    anchor: usize,                // Неподвижный край выделения.
    caret: usize,                 // Подвижный край выделения и положение курсора.
    marked: Option<Range<usize>>, // Диапазон незавершённого системного ввода.
    focus: FocusHandle,           // Фокус только этого показа поля.
    dragging: bool,               // Мышь меняет подвижный край выделения, пока нажата кнопка.
    line: Option<Line>, // Геометрия актуального текста для мыши и IME, если он уже нарисован.
}

/// Положение сформированных символов внутри видимой области.
struct Line {
    shape: ShapedLine,      // Сформированная строка пользователя или подсказки.
    bounds: Bounds<Pixels>, // Видимое поле в координатах окна.
    scroll: Pixels,         // Горизонтальное смещение, оставляющее курсор видимым.
}

impl Search {
    /// Возвращает прежний запрос этого входа без переноса фокуса от другого аккаунта.
    pub(super) fn new(text: String, cx: &mut Context<Self>) -> Self {
        let end = text.len();
        Self {
            text,
            anchor: end,
            caret: end,
            marked: None,
            focus: cx.focus_handle().tab_stop(true),
            dragging: false,
            line: None,
        }
    }

    /// Подключает только команды поиска, чтобы не перехватывать клавиши в истории и редакторе.
    pub(super) fn bind_keys(app: &mut App) {
        let context = Some("CatalogSearch");
        app.bind_keys([
            KeyBinding::new("backspace", Backspace, context),
            KeyBinding::new("delete", Delete, context),
            KeyBinding::new("left", Left, context),
            KeyBinding::new("right", Right, context),
            KeyBinding::new("shift-left", SelectLeft, context),
            KeyBinding::new("shift-right", SelectRight, context),
            KeyBinding::new("cmd-left", Start, context),
            KeyBinding::new("cmd-right", End, context),
            KeyBinding::new("cmd-a", SelectAll, context),
            KeyBinding::new("cmd-c", Copy, context),
            KeyBinding::new("cmd-x", Cut, context),
            KeyBinding::new("cmd-v", Paste, context),
            KeyBinding::new("escape", Clear, context),
        ]);
    }

    /// Возвращает запрос без изменения пробелов и регистра.
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// Возвращает диапазон независимо от направления выделения.
    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }

    /// Меняет позицию курсора и при необходимости сохраняет выделение.
    fn move_to(&mut self, end: usize, extend: bool, cx: &mut Context<Self>) {
        self.caret = end;
        if !extend {
            self.anchor = end;
        }
        cx.notify();
    }

    /// Перемещает курсор по видимым символам либо сворачивает выделение.
    fn horizontal(&mut self, right: bool, extend: bool, cx: &mut Context<Self>) {
        let selection = self.selection();
        let next = if extend || selection.is_empty() {
            self.adjacent(right)
        } else if right {
            selection.end
        } else {
            selection.start
        };
        self.move_to(next, extend, cx);
    }

    /// Ищет соседнюю границу графемы, не разрезая эмодзи и составные буквы.
    fn adjacent(&self, right: bool) -> usize {
        let mut edges = self.text.grapheme_indices(true).map(|(offset, _)| offset);
        if right {
            edges
                .find(|&offset| offset > self.caret)
                .unwrap_or(self.text.len())
        } else {
            edges
                .take_while(|&offset| offset < self.caret)
                .last()
                .unwrap_or(0)
        }
    }

    /// Применяет однострочную правку и сообщает каталогу о новом запросе.
    fn replace(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) -> Range<usize> {
        let end = range.start + text.len();
        if self.text[range.clone()] != *text {
            self.text.replace_range(range.clone(), text);
            self.line = None;
            cx.emit(QueryChanged);
        }
        self.anchor = end;
        self.caret = end;
        self.marked = None;
        cx.notify();
        range.start..end
    }

    /// Удаляет выделение или одну соседнюю графему.
    fn delete(&mut self, right: bool, cx: &mut Context<Self>) {
        let range = self.selection();
        let range = if range.is_empty() {
            let adjacent = self.adjacent(right);
            adjacent.min(self.caret)..adjacent.max(self.caret)
        } else {
            range
        };
        self.replace(range, "", cx);
    }

    /// Выбирает системный диапазон, текущую композицию или обычное выделение.
    fn input_range(&self, range: Option<Range<usize>>) -> Range<usize> {
        range
            .map(|range| self.to_bytes(range))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection())
    }

    /// Очищает запрос и возвращает фокус полю.
    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace(0..self.text.len(), "", cx);
        window.focus(&self.focus);
    }

    /// Копирует выделение, при вырезании удаляя его из поля.
    fn copy(&mut self, cut: bool, cx: &mut Context<Self>) {
        let range = self.selection();
        if !range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.text[range.clone()].to_owned(),
            ));
            if cut {
                self.replace(range, "", cx);
            }
        }
    }

    /// Вставляет текст буфера как одну строку.
    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace(self.input_range(None), &single_line(&text), cx);
        }
    }

    /// Ставит курсор в место нажатия и принимает фокус.
    fn click(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.dragging = true;
        if let Some(index) = self.index_at(event.position) {
            self.move_to(index, event.modifiers.shift, cx);
        }
    }

    /// Продлевает выделение за указателем, пока нажата левая кнопка.
    fn drag(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging
            && event.pressed_button == Some(MouseButton::Left)
            && let Some(index) = self.index_at(event.position)
        {
            self.move_to(index, true, cx);
        }
    }

    /// Находит байтовую позицию по координате, учитывая горизонтальную прокрутку.
    fn index_at(&self, position: Point<Pixels>) -> Option<usize> {
        let line = self.line.as_ref()?;
        let index = line
            .shape
            .closest_index_for_x(position.x - line.bounds.left() + line.scroll);
        Some(self.text.floor_char_boundary(index.min(self.text.len())))
    }

    /// Переводит системный диапазон UTF-16 в границы строки Rust.
    fn to_bytes(&self, range: Range<usize>) -> Range<usize> {
        let start = byte_offset(&self.text, range.start);
        start..byte_offset(&self.text, range.end).max(start)
    }

    /// Переводит границы текста Rust в системные позиции UTF-16.
    fn to_utf16(&self, range: Range<usize>) -> Range<usize> {
        self.text[..range.start].encode_utf16().count()
            ..self.text[..range.end].encode_utf16().count()
    }
}

impl EventEmitter<QueryChanged> for Search {}

impl EntityInputHandler for Search {
    /// Возвращает системе запрошенный фрагмент и фактические границы.
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.to_bytes(range);
        *adjusted = Some(self.to_utf16(range.clone()));
        Some(self.text[range].to_owned())
    }

    /// Передаёт актуальное выделение и его направление.
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.to_utf16(self.selection()),
            reversed: self.caret < self.anchor,
        })
    }

    /// Даёт системе границы текущей композиции.
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.clone().map(|range| self.to_utf16(range))
    }

    /// Завершает композицию, оставляя введённый текст.
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    /// Вставляет результат системного ввода.
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(self.input_range(range), &single_line(text), cx);
    }

    /// Показывает текущую композицию IME и её относительное выделение.
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = single_line(text);
        let marked = self.replace(self.input_range(range), &text, cx);
        if let Some(selected) = selected {
            self.anchor = marked.start + byte_offset(&text, selected.start);
            self.caret = marked.start + byte_offset(&text, selected.end);
        }
        self.marked = (!marked.is_empty()).then_some(marked);
    }

    /// Возвращает прямоугольник для окна подсказок IME.
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.line.as_ref()?;
        let range = self.to_bytes(range);
        let left = line
            .x_for(range.start)
            .clamp(line.bounds.left(), line.bounds.right());
        let right = line
            .x_for(range.end)
            .clamp(line.bounds.left(), line.bounds.right());
        Some(Bounds::new(
            point(left.min(right), line.bounds.top()),
            size((right - left).abs().max(px(1.)), line.bounds.size.height),
        ))
    }

    /// Возвращает системную позицию символа под указателем.
    fn character_index_for_point(
        &mut self,
        at: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.text[..self.index_at(at)?].encode_utf16().count())
    }
}

impl Line {
    /// Переводит байтовую позицию строки в горизонтальную координату окна.
    fn x_for(&self, index: usize) -> Pixels {
        self.bounds.left() + self.shape.x_for_index(index) - self.scroll
    }
}

impl Render for Search {
    /// Собирает поле из общих примитивов и рисует один слой системного ввода.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::from(window.appearance()));
        let appearance = if self.focus.is_focused(window) {
            FieldAppearance::Focused
        } else {
            FieldAppearance::Normal
        };
        let entity = cx.entity();
        let layout_view = entity.clone();
        let paint_view = entity.clone();
        let clear_view = entity.downgrade();
        let input = div()
            .id("catalog-search")
            .key_context("CatalogSearch")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .w_full()
            .min_w_0()
            .h(px(24.))
            .text_size(px(14.))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::click))
            .on_mouse_move(cx.listener(Self::drag))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.dragging = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.dragging = false),
            )
            .on_action(cx.listener(|this, _: &Backspace, _, cx| this.delete(false, cx)))
            .on_action(cx.listener(|this, _: &Delete, _, cx| this.delete(true, cx)))
            .on_action(cx.listener(|this, _: &Left, _, cx| this.horizontal(false, false, cx)))
            .on_action(cx.listener(|this, _: &Right, _, cx| this.horizontal(true, false, cx)))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| this.horizontal(false, true, cx)))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| this.horizontal(true, true, cx)))
            .on_action(cx.listener(|this, _: &Start, _, cx| this.move_to(0, false, cx)))
            .on_action(cx.listener(|this, _: &End, _, cx| this.move_to(this.text.len(), false, cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.anchor = 0;
                this.move_to(this.text.len(), true, cx);
            }))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(false, cx)))
            .on_action(cx.listener(|this, _: &Cut, _, cx| this.copy(true, cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
            .on_action(cx.listener(|this, _: &Clear, window, cx| this.clear(window, cx)))
            .child(
                canvas(
                    move |bounds, window, app| layout_view.read(app).shape(bounds, palette, window),
                    move |bounds, line, window, app| {
                        let view = paint_view.read(app);
                        window.handle_input(
                            &view.focus,
                            ElementInputHandler::new(bounds, paint_view.clone()),
                            app,
                        );
                        paint_view.update(app, |view, cx| {
                            view.paint_line(&line, palette, window, cx);
                            view.line = Some(line);
                        });
                    },
                )
                .size_full(),
            )
            .debug_selector(|| "catalog-search".into());
        let mut control = div()
            .flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap(px(6.))
            .child(Icon::Search.element(px(16.), palette.text_secondary))
            .child(div().flex_1().min_w_0().child(input));
        if !self.text.is_empty() {
            let mut clear = Button::new("catalog-clear", "");
            clear.appearance = ButtonAppearance::Ghost;
            clear.icon = Some(Icon::Close);
            control = control.child(
                clear
                    .render(palette, None, move |window, app| {
                        let _ = clear_view.update(app, |view, cx| view.clear(window, cx));
                    })
                    .h(px(24.))
                    .min_w(px(24.))
                    .px(px(3.))
                    .debug_selector(|| "catalog-clear".into()),
            );
        }
        field(palette, appearance, control)
    }
}

impl Search {
    /// Раскладывает одну строку без штатной обрезки, сохраняя ширину курсора.
    fn shape(&self, bounds: Bounds<Pixels>, palette: Palette, window: &Window) -> Line {
        let placeholder = self.text.is_empty();
        let text = if placeholder {
            "Поиск по чатам"
        } else {
            self.text.as_str()
        };
        let style = window.text_style();
        let shape = window.text_system().shape_line(
            text.to_owned().into(),
            px(14.),
            &[TextRun {
                len: text.len(),
                font: style.font(),
                color: if placeholder {
                    palette.text_muted
                } else {
                    palette.text
                },
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let width = (bounds.size.width - px(2.)).max(px(0.));
        let caret = shape.x_for_index(self.caret);
        let previous = self.line.as_ref().map_or(px(0.), |line| line.scroll);
        let scroll = if placeholder {
            px(0.)
        } else {
            previous
                .clamp((caret - width).max(px(0.)), caret.max(px(0.)))
                .min((shape.width - width).max(px(0.)))
        };
        Line {
            shape,
            bounds,
            scroll,
        }
    }

    /// Рисует текст, выделение и курсор только внутри видимой области поля.
    fn paint_line(&self, line: &Line, palette: Palette, window: &mut Window, app: &mut App) {
        let focused = self.focus.is_focused(window);
        let origin = point(line.bounds.left() - line.scroll, line.bounds.top());
        window.with_content_mask(
            Some(ContentMask {
                bounds: line.bounds,
            }),
            |window| {
                if focused && !self.selection().is_empty() {
                    let selected = self.selection();
                    let (a, b) = (line.x_for(selected.start), line.x_for(selected.end));
                    window.paint_quad(fill(
                        Bounds::new(
                            point(a.min(b), line.bounds.top()),
                            size((a - b).abs(), line.bounds.size.height),
                        ),
                        palette.accent_soft,
                    ));
                }
                let _ = line
                    .shape
                    .paint(origin, line.bounds.size.height, window, app);
                if focused {
                    if let Some(marked) = &self.marked {
                        let (a, b) = (line.x_for(marked.start), line.x_for(marked.end));
                        window.paint_quad(fill(
                            Bounds::new(
                                point(a.min(b), line.bounds.bottom() - px(3.)),
                                size((a - b).abs(), px(1.)),
                            ),
                            palette.accent,
                        ));
                    }
                    if self.selection().is_empty() {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(line.x_for(self.caret), line.bounds.top() + px(3.)),
                                size(px(1.), line.bounds.size.height - px(6.)),
                            ),
                            palette.accent,
                        ));
                    }
                }
            },
        );
    }
}

/// Сохраняет одну строку даже при вставке текста из буфера.
fn single_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

/// Находит границу символа Rust для индекса UTF-16 из AppKit.
fn byte_offset(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > utf16 {
            return index;
        }
    }
    text.len()
}
