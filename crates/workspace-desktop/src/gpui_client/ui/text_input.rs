//! Однострочное поле GPUI с системным вводом, выделением и горизонтальной прокруткой.
//! В защищённом режиме поле рисует маску вместо символов пароля или кода.

use std::{borrow::Cow, ops::Range};

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementInputHandler,
    EntityInputHandler, EventEmitter, FocusHandle, KeyBinding, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Point, Render, ShapedLine, TextRun, UTF16Selection,
    Window, canvas, div, fill, point, prelude::*, px, size,
};
use unicode_segmentation::UnicodeSegmentation;

use super::{FieldAppearance, Palette, field};

gpui::actions!(
    workspace_text_input,
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
    ]
);

/// Сообщает, что изменилось значение поля.
pub(crate) struct TextInputChanged;

/// Передаёт форме нажатие Enter после завершения системного ввода.
pub(crate) struct TextInputSubmitted;

/// Однострочное состояние поля ввода.
pub(crate) struct TextInput {
    id: &'static str,             // ID поля в дереве элементов и в тестах.
    placeholder: &'static str,    // Подсказка, которую видно до ввода.
    masked: bool, // Пароль или код скрыты и не передаются при чтении системе или буферу.
    enabled: bool, // Можно принимать ввод и ставить фокус через Tab.
    invalid: bool, // Экран обнаружил ошибку и просит выделить рамку поля.
    framed: bool, // Поле рисует собственную рамку, в поиске рамку рисует каталог.
    text: String, // Введённое значение; позиции хранятся в байтах UTF-8.
    anchor: usize, // Неподвижный край выделения.
    caret: usize, // Подвижный край выделения и положение курсора.
    marked: Option<Range<usize>>, // Диапазон незавершённого системного ввода.
    focus: FocusHandle, // Фокус только этого показа поля.
    dragging: bool, // Мышь меняет подвижный край выделения, пока нажата кнопка.
    line: Option<Line>, // Геометрия актуального текста для мыши и IME, если он уже нарисован.
}

/// Положение сформированных символов внутри видимой области.
struct Line {
    shape: ShapedLine,      // Сформированная строка пользователя или подсказки.
    bounds: Bounds<Pixels>, // Видимое поле в координатах окна.
    scroll: Pixels,         // Горизонтальное смещение, оставляющее курсор видимым.
    mask_offsets: Option<Vec<usize>>, // Границы символов исходного текста, когда строка скрыта.
}

impl TextInput {
    /// Создаёт поле формы в общей рамке. Для пароля и кода включает маску.
    pub(crate) fn new(
        id: &'static str,
        placeholder: &'static str,
        masked: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            id,
            placeholder,
            masked,
            enabled: true,
            invalid: false,
            framed: true,
            text: String::new(),
            anchor: 0,
            caret: 0,
            marked: None,
            focus: cx.focus_handle().tab_stop(true),
            dragging: false,
            line: None,
        }
    }

    /// Создаёт ввод для поля, рамку и значок которого рисует каталог.
    pub(in crate::gpui_client) fn for_search(text: String, cx: &mut Context<Self>) -> Self {
        let end = text.len();
        Self {
            text,
            anchor: end,
            caret: end,
            framed: false,
            ..Self::new("catalog-search", "Найти", false, cx)
        }
    }

    /// Использует рамку и подложку, которые рисует родительская форма.
    pub(in crate::gpui_client) fn unframed(mut self) -> Self {
        self.framed = false;
        self
    }

    /// Регистрирует команды редактирования текста.
    pub(crate) fn bind_keys(app: &mut App) {
        let context = Some("WorkspaceTextInput");
        app.bind_keys([
            KeyBinding::new("backspace", Backspace, context),
            KeyBinding::new("delete", Delete, context),
            KeyBinding::new("left", Left, context),
            KeyBinding::new("right", Right, context),
            KeyBinding::new("shift-left", SelectLeft, context),
            KeyBinding::new("shift-right", SelectRight, context),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-left", Start, context),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-right", End, context),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("home", Start, context),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("end", End, context),
            KeyBinding::new("secondary-a", SelectAll, context),
            KeyBinding::new("secondary-c", Copy, context),
            KeyBinding::new("secondary-x", Cut, context),
            KeyBinding::new("secondary-v", Paste, context),
        ]);
    }

    /// Возвращает значение поля экрану. Для пароля и кода это исходный текст.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Меняет подсказку при смене языка, сохраняя введённую строку.
    pub(in crate::gpui_client) fn set_placeholder(
        &mut self,
        text: &'static str,
        cx: &mut Context<Self>,
    ) {
        if self.placeholder != text {
            self.placeholder = text;
            cx.notify();
        }
    }

    /// Обновляет значение одной строкой и ставит курсор в конец.
    pub(crate) fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.replace(0..self.text.len(), &single_line(text), cx);
    }

    /// Убирает введённое значение и сбрасывает выделение и композицию.
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_text("", cx);
    }

    /// Меняет доступность ввода, сохраняя значение и текущий фокус.
    pub(crate) fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.focus = self.focus.clone().tab_stop(enabled);
            self.dragging = false;
            cx.notify();
        }
    }

    /// Отмечает ошибку, определённую экраном. Само поле не проверяет значение.
    pub(crate) fn set_invalid(&mut self, invalid: bool, cx: &mut Context<Self>) {
        if self.invalid != invalid {
            self.invalid = invalid;
            cx.notify();
        }
    }

    /// Указывает, что клавиатурный ввод сейчас направлен в это поле.
    pub(crate) fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// Передаёт фокус этому полю.
    pub(crate) fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    /// Даёт рабочему окну цель при переходе между областями.
    pub(in crate::gpui_client) fn keyboard_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Проверяет, что система ещё редактирует незавершённый фрагмент текста.
    pub(crate) fn is_composing(&self) -> bool {
        self.marked.is_some()
    }

    /// Передаёт обычный Enter один раз. AppKit первым получает клавиши незавершённого IME.
    fn submit(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key != "enter" || event.keystroke.modifiers.modified() {
            return;
        }
        cx.stop_propagation();
        if self.enabled && !event.is_held && !self.is_composing() {
            cx.emit(TextInputSubmitted);
        }
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

    /// Применяет однострочную правку и сообщает экрану об изменении значения.
    fn replace(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) -> Range<usize> {
        let end = range.start + text.len();
        if self.text[range.clone()] != *text {
            self.text.replace_range(range.clone(), text);
            self.line = None;
            cx.emit(TextInputChanged);
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

    /// Копирует выделение, при вырезании удаляя его из поля. Пароль и код остаются в поле.
    fn copy(&mut self, cut: bool, cx: &mut Context<Self>) {
        if self.masked {
            return;
        }
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
        Some(match &line.mask_offsets {
            Some(offsets) => offsets[(index / MASK.len_utf8()).min(offsets.len() - 1)],
            None => self.text.floor_char_boundary(index.min(self.text.len())),
        })
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

impl EventEmitter<TextInputChanged> for TextInput {}
impl EventEmitter<TextInputSubmitted> for TextInput {}

impl EntityInputHandler for TextInput {
    /// Возвращает системе обычный текст и его фактические границы. Для пароля и кода текст закрыт.
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        if self.masked {
            return None;
        }
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
        if self.enabled {
            self.replace(self.input_range(range), &single_line(text), cx);
        }
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
        if !self.enabled {
            return;
        }
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
        self.bounds.left()
            + self
                .shape
                .x_for_index(display_index(self.mask_offsets.as_deref(), index))
            - self.scroll
    }
}

impl Render for TextInput {
    /// Рисует поле в общей рамке или отдаёт голый ввод для поля каталога.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let appearance = if !self.enabled {
            FieldAppearance::Disabled
        } else if self.invalid {
            FieldAppearance::Invalid
        } else if self.is_focused(window) {
            FieldAppearance::Focused
        } else {
            FieldAppearance::Normal
        };
        let entity = cx.entity();
        let layout_view = entity.clone();
        let paint_view = entity.clone();
        let id = self.id;
        let input = div()
            .id(id)
            .track_focus(&self.focus)
            .w_full()
            .min_w_0()
            .h(px(24.))
            .text_size(px(14.))
            .when(self.enabled, |input| {
                input
                    .key_context("WorkspaceTextInput")
                    .cursor(CursorStyle::IBeam)
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
                    .on_action(
                        cx.listener(|this, _: &Left, _, cx| this.horizontal(false, false, cx)),
                    )
                    .on_action(
                        cx.listener(|this, _: &Right, _, cx| this.horizontal(true, false, cx)),
                    )
                    .on_action(
                        cx.listener(|this, _: &SelectLeft, _, cx| this.horizontal(false, true, cx)),
                    )
                    .on_action(
                        cx.listener(|this, _: &SelectRight, _, cx| this.horizontal(true, true, cx)),
                    )
                    .on_action(cx.listener(|this, _: &Start, _, cx| this.move_to(0, false, cx)))
                    .on_action(
                        cx.listener(|this, _: &End, _, cx| {
                            this.move_to(this.text.len(), false, cx)
                        }),
                    )
                    .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                        this.anchor = 0;
                        this.move_to(this.text.len(), true, cx);
                    }))
                    .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(false, cx)))
                    .on_action(cx.listener(|this, _: &Cut, _, cx| this.copy(true, cx)))
                    .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
                    .when(self.framed, |input| {
                        input.on_key_down(cx.listener(Self::submit))
                    })
            })
            .child(
                canvas(
                    move |bounds, window, app| layout_view.read(app).shape(bounds, palette, window),
                    move |bounds, line, window, app| {
                        let view = paint_view.read(app);
                        if view.enabled {
                            window.handle_input(
                                &view.focus,
                                ElementInputHandler::new(bounds, paint_view.clone()),
                                app,
                            );
                        }
                        paint_view.update(app, |view, cx| {
                            view.paint_line(&line, palette, window, cx);
                            view.line = Some(line);
                        });
                    },
                )
                .size_full(),
            )
            .debug_selector(|| id.into());
        if self.framed {
            field(palette, appearance, input).into_any_element()
        } else {
            input.into_any_element()
        }
    }
}

impl TextInput {
    /// Раскладывает одну строку без штатной обрезки, сохраняя ширину курсора.
    fn shape(&self, bounds: Bounds<Pixels>, palette: Palette, window: &Window) -> Line {
        let placeholder = self.text.is_empty();
        let mask_offsets = (!placeholder && self.masked).then(|| {
            // GPUI измеряет позиции в байтах нарисованной строки, а IME редактирует оригинал.
            self.text
                .char_indices()
                .map(|(index, _)| index)
                .chain(std::iter::once(self.text.len()))
                .collect::<Vec<_>>()
        });
        let text = if placeholder {
            Cow::Borrowed(self.placeholder)
        } else if let Some(offsets) = &mask_offsets {
            Cow::Owned(std::iter::repeat_n(MASK, offsets.len() - 1).collect())
        } else {
            Cow::Borrowed(self.text.as_str())
        };
        let style = window.text_style();
        let shape = window.text_system().shape_line(
            text.as_ref().to_owned().into(),
            px(14.),
            &[TextRun {
                len: text.len(),
                font: style.font(),
                color: if placeholder || !self.enabled {
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
        let caret = shape.x_for_index(display_index(mask_offsets.as_deref(), self.caret));
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
            mask_offsets,
        }
    }

    /// Рисует текст, выделение и курсор только внутри видимой области поля.
    fn paint_line(&self, line: &Line, palette: Palette, window: &mut Window, app: &mut App) {
        let focused = self.enabled && self.is_focused(window);
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

const MASK: char = '•';

/// Переводит позицию курсора исходного текста в позицию нарисованного символа маски.
fn display_index(mask_offsets: Option<&[usize]>, text_index: usize) -> usize {
    match mask_offsets {
        Some(offsets) => {
            (offsets.partition_point(|&offset| offset <= text_index) - 1) * MASK.len_utf8()
        }
        None => text_index,
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

#[cfg(all(test, feature = "gpui-test"))]
mod tests;
