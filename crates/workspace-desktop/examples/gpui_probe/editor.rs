//! Текстовое поле прототипа. Сначала состояние и действия, затем связь с системным вводом и разметка.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId,
    InspectorElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels,
    Point, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, div,
    fill, point, prelude::*, px, relative, rgb, rgba, size,
};

/// Действия клавиатуры внутри редактора. Печатные символы передаёт система ввода macOS.
mod keys {
    use gpui::actions;

    actions!(
        editor,
        [
            Backspace,
            Delete,
            Newline,
            Left,
            Right,
            Up,
            Down,
            SelectLeft,
            SelectRight,
            SelectAll,
            Copy,
            Cut,
            Paste,
        ]
    );
}

const LINE_HEIGHT: Pixels = px(22.);
const VIEWPORT_HEIGHT: Pixels = px(110.);

/// Черновик прототипа и локальное состояние выделения. Индексы считаются в байтах UTF-8.
pub struct Editor {
    text: String,                 // Текст с системными переводами строк, приведёнными к LF.
    anchor: usize,                // Неподвижный конец выделения.
    caret: usize,                 // Положение курсора или подвижный конец выделения.
    marked: Option<Range<usize>>, // Незавершённая композиция IME.
    focus: FocusHandle,           // Связь состояния редактора с фокусом GPUI.
    layout: Option<TextLayout>,   // Последняя нарисованная геометрия для мыши и IME.
    layout_current: bool,         // Геометрия относится к текущей версии текста.
    dragging: bool,               // Зажата кнопка мыши при выделении текста.
}

impl Editor {
    /// Готовит пустой редактор, доступный через Tab.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            anchor: 0,
            caret: 0,
            marked: None,
            focus: cx.focus_handle().tab_stop(true),
            layout: None,
            layout_current: false,
            dragging: false,
        }
    }

    /// Подключает клавиши редактора. Регистрируется один раз при запуске приложения.
    pub fn bind_keys(cx: &mut App) {
        cx.bind_keys([
            KeyBinding::new("backspace", keys::Backspace, Some("ProbeEditor")),
            KeyBinding::new("delete", keys::Delete, Some("ProbeEditor")),
            KeyBinding::new("enter", keys::Newline, Some("ProbeEditor")),
            KeyBinding::new("shift-enter", keys::Newline, Some("ProbeEditor")),
            KeyBinding::new("left", keys::Left, Some("ProbeEditor")),
            KeyBinding::new("right", keys::Right, Some("ProbeEditor")),
            KeyBinding::new("up", keys::Up, Some("ProbeEditor")),
            KeyBinding::new("down", keys::Down, Some("ProbeEditor")),
            KeyBinding::new("shift-left", keys::SelectLeft, Some("ProbeEditor")),
            KeyBinding::new("shift-right", keys::SelectRight, Some("ProbeEditor")),
            KeyBinding::new("cmd-a", keys::SelectAll, Some("ProbeEditor")),
            KeyBinding::new("cmd-c", keys::Copy, Some("ProbeEditor")),
            KeyBinding::new("cmd-x", keys::Cut, Some("ProbeEditor")),
            KeyBinding::new("cmd-v", keys::Paste, Some("ProbeEditor")),
        ]);
    }

    /// Возвращает текущее содержимое, в том числе незавершённую композицию IME.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Возвращает границы выделения независимо от направления.
    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }

    /// Устанавливает курсор и по необходимости сохраняет начало выделения.
    fn move_to(&mut self, index: usize, extend: bool, cx: &mut Context<Self>) {
        self.caret = index;
        if !extend {
            self.anchor = index;
        }
        cx.notify();
    }

    /// Перемещает курсор по видимым символам либо сворачивает существующее выделение.
    fn horizontal(&mut self, right: bool, extend: bool, cx: &mut Context<Self>) {
        let selection = self.selection();
        let index = if extend || selection.is_empty() {
            adjacent_boundary(&self.text, self.caret, right)
        } else if right {
            selection.end
        } else {
            selection.start
        };
        self.move_to(index, extend, cx);
    }

    /// Возвращает геометрию только тогда, когда она относится к текущему тексту.
    fn current_layout(&self) -> Option<&TextLayout> {
        self.layout.as_ref().filter(|_| self.layout_current)
    }

    /// Перемещает курсор на строку выше или ниже по геометрии текущего кадра.
    fn vertical(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(layout) = self.current_layout() else {
            return;
        };
        let Some(mut position) = layout.position(self.caret) else {
            return;
        };
        position.y += if down { LINE_HEIGHT } else { -LINE_HEIGHT } + px(1.);
        self.move_to(layout.index_at(position), false, cx);
    }

    /// Удаляет выделение или один соседний символ.
    fn delete(&mut self, right: bool, cx: &mut Context<Self>) {
        let mut range = self.selection();
        if range.is_empty() {
            self.horizontal(right, true, cx);
            range = self.selection();
        }
        self.replace(range, "", cx);
    }

    /// Меняет выделенную область и возвращает байты вставленного текста.
    fn replace(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) -> Range<usize> {
        if self.text[range.clone()] != *text {
            self.text.replace_range(range.clone(), text);
            self.layout_current = false;
        }
        let end = range.start + text.len();
        self.anchor = end;
        self.caret = end;
        self.marked = None;
        cx.notify();
        range.start..end
    }

    /// Отдаёт приоритет системному диапазону, затем композиции и обычному выделению.
    fn input_range(&self, range: Option<Range<usize>>) -> Range<usize> {
        range
            .map(|range| utf8_range(&self.text, range))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection())
    }

    /// Копирует выделенный текст, при вырезании удаляет его после записи в буфер.
    fn copy(&mut self, cut: bool, cx: &mut Context<Self>) {
        let selection = self.selection();
        if !selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.text[selection.clone()].into(),
            ));
            if cut {
                self.replace(selection, "", cx);
            }
        }
    }

    /// Вставляет обычный текст, приводя переводы строк из буфера к LF.
    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace(self.input_range(None), &normalize_lines(&text), cx);
        }
    }

    /// Ставится в позицию клика и начинает выделение перетаскиванием.
    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.dragging = true;
        if let Some(layout) = self.current_layout() {
            self.move_to(layout.index_at(event.position), event.modifiers.shift, cx);
        }
    }

    /// Обновляет выделение, пока нажата кнопка мыши.
    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging
            && let Some(layout) = self.current_layout()
        {
            self.move_to(layout.index_at(event.position), true, cx);
        }
    }
}

/// Приводит Windows и старые переводы строк в обычные строки GPUI.
fn normalize_lines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Возвращает позицию в соседнем видимом символе.
fn adjacent_boundary(text: &str, index: usize, right: bool) -> usize {
    use unicode_segmentation::UnicodeSegmentation;

    if right {
        text.grapheme_indices(true)
            .map(|(offset, _)| offset)
            .find(|&offset| offset > index)
            .unwrap_or(text.len())
    } else {
        text.grapheme_indices(true)
            .map(|(offset, _)| offset)
            .take_while(|&offset| offset < index)
            .last()
            .unwrap_or(0)
    }
}

/// Переводит UTF-16 из AppKit в байты. Внутри суррогатной пары берёт начало символа.
fn byte_offset(text: &str, utf16_offset: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > utf16_offset {
            return index;
        }
    }
    text.len()
}

/// Переводит диапазон AppKit к границам символов в Rust.
fn utf8_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = byte_offset(text, range.start);
    start..byte_offset(text, range.end).max(start)
}

/// Переводит байтовую позицию курсора в индекс AppKit.
fn utf16_offset(text: &str, index: usize) -> usize {
    text[..index].encode_utf16().count()
}

impl Focusable for Editor {
    /// Возвращает указатель на фокус для экрана, который встраивает редактор.
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for Editor {
    /// Возвращает текст участка и сообщает системе фактические границы.
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let bytes = utf8_range(&self.text, range);
        *adjusted =
            Some(utf16_offset(&self.text, bytes.start)..utf16_offset(&self.text, bytes.end));
        Some(self.text[bytes].into())
    }

    /// Передаёт системе выделение и направление курсора.
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let selection = self.selection();
        Some(UTF16Selection {
            range: utf16_offset(&self.text, selection.start)
                ..utf16_offset(&self.text, selection.end),
            reversed: self.caret < self.anchor,
        })
    }

    /// Возвращает системный диапазон незавершённой композиции.
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let marked = self.marked.as_ref()?;
        Some(utf16_offset(&self.text, marked.start)..utf16_offset(&self.text, marked.end))
    }

    /// Оставляет текст композиции в черновике и снимает её подчёркивание.
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    /// Применяет обычный ввод и подтверждённую композицию через тот же путь редактирования.
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(self.input_range(range), &normalize_lines(text), cx);
    }

    /// Обновляет композицию и переводит её относительное выделение из UTF-16.
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let marked = self.replace(self.input_range(range), text, cx);
        if let Some(selected) = selected {
            let bytes = utf8_range(text, selected);
            self.anchor = marked.start + bytes.start;
            self.caret = marked.start + bytes.end;
        }
        self.marked = (!marked.is_empty()).then_some(marked);
    }

    /// Даёт AppKit позицию для окна вариантов ввода на первой строке диапазона.
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.current_layout()?;
        let range = utf8_range(&self.text, range);
        let start = layout.position(range.start)?;
        let end = layout.position(range.end)?;
        let right = if end.y == start.y {
            end.x
        } else {
            layout.bounds.right()
        };
        Some(Bounds::new(
            start,
            size((right - start.x).max(px(1.)), LINE_HEIGHT),
        ))
    }

    /// Переводит координату из системы в индекс UTF-16.
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(utf16_offset(
            &self.text,
            self.current_layout()?.index_at(point),
        ))
    }
}

impl Render for Editor {
    /// Рисует рамку общего поля и подключает клавиатуру, фокус и мышь.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = if self.focus.is_focused(window) {
            0x709dff
        } else {
            0x51545c
        };
        div()
            .id("gpui-probe-editor")
            .w_full()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(border))
            .bg(rgb(0x24272d))
            .text_color(rgb(0xf1f2f5))
            .text_size(px(16.))
            .key_context("ProbeEditor")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|editor, _, _, _| editor.dragging = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|editor, _, _, _| editor.dragging = false),
            )
            .on_action(
                cx.listener(|editor, _: &keys::Left, _, cx| editor.horizontal(false, false, cx)),
            )
            .on_action(
                cx.listener(|editor, _: &keys::Right, _, cx| editor.horizontal(true, false, cx)),
            )
            .on_action(cx.listener(|editor, _: &keys::Up, _, cx| editor.vertical(false, cx)))
            .on_action(cx.listener(|editor, _: &keys::Down, _, cx| editor.vertical(true, cx)))
            .on_action(
                cx.listener(|editor, _: &keys::SelectLeft, _, cx| {
                    editor.horizontal(false, true, cx)
                }),
            )
            .on_action(
                cx.listener(|editor, _: &keys::SelectRight, _, cx| {
                    editor.horizontal(true, true, cx)
                }),
            )
            .on_action(cx.listener(|editor, _: &keys::Backspace, _, cx| editor.delete(false, cx)))
            .on_action(cx.listener(|editor, _: &keys::Delete, _, cx| editor.delete(true, cx)))
            .on_action(cx.listener(|editor, _: &keys::Newline, _, cx| {
                editor.replace(editor.input_range(None), "\n", cx);
            }))
            .on_action(cx.listener(|editor, _: &keys::SelectAll, _, cx| {
                editor.anchor = 0;
                editor.move_to(editor.text.len(), true, cx);
            }))
            .on_action(cx.listener(|editor, _: &keys::Copy, _, cx| editor.copy(false, cx)))
            .on_action(cx.listener(|editor, _: &keys::Cut, _, cx| editor.copy(true, cx)))
            .on_action(cx.listener(|editor, _: &keys::Paste, _, cx| editor.paste(cx)))
            .child(EditorSurface(cx.entity()))
    }
}

/// Сформированная строка и её начало в общем тексте поля.
struct TextLine {
    shape: WrappedLine, // Глифы и разрывы, рассчитанные GPUI.
    start: usize,       // Начальный индекс в байтах, включая предыдущие переводы строки.
    top: Pixels,        // Вертикальная координата относительно начала текста.
}

/// Геометрия последнего кадра. Хранится вместе с её границами для мыши и IME.
struct TextLayout {
    bounds: Bounds<Pixels>, // Видимая площадь редактора.
    lines: Vec<TextLine>,   // Строки с переносами и исходным смещением.
    scroll: Pixels,         // Вертикальная прокрутка текста до курсора.
    text_len: usize,        // Длина черновика в байтах без подсказки.
}

impl TextLayout {
    /// Находит положение курсора в координатах окна.
    fn position(&self, index: usize) -> Option<Point<Pixels>> {
        if self.text_len == 0 {
            return Some(self.bounds.origin);
        }
        self.lines.iter().find_map(|line| {
            let local = index.checked_sub(line.start)?;
            let offset = line.shape.position_for_index(local, LINE_HEIGHT)?;
            Some(self.line_origin(line) + offset)
        })
    }

    /// Возвращает начало строки в окне с учётом текущей прокрутки.
    fn line_origin(&self, line: &TextLine) -> Point<Pixels> {
        self.bounds.origin + point(px(0.), line.top - self.scroll)
    }

    /// Находит ближайший байтовый индекс, в том числе для кликов за пределами текста.
    fn index_at(&self, position: Point<Pixels>) -> usize {
        if self.text_len == 0 {
            return 0;
        }
        if position.y < self.bounds.top() - self.scroll {
            return 0;
        }
        for line in &self.lines {
            let local = position - self.line_origin(line);
            if local.y < line.shape.size(LINE_HEIGHT).height {
                return (line.start
                    + line
                        .shape
                        .closest_index_for_position(local, LINE_HEIGHT)
                        .unwrap_or_else(|edge| edge))
                .min(self.text_len);
            }
        }
        self.text_len
    }
}

/// Поверхность текста. В фазе отрисовки регистрирует системный обработчик.
struct EditorSurface(Entity<Editor>); // Редактор, которому принадлежат текст и фокус.

impl IntoElement for EditorSurface {
    type Element = Self;

    /// Передаёт специализированный элемент разметке GPUI.
    fn into_element(self) -> Self {
        self
    }
}

impl Element for EditorSurface {
    type RequestLayoutState = ();
    type PrepaintState = TextLayout;

    /// Оставляет идентификатор за внешним контейнером.
    fn id(&self) -> Option<ElementId> {
        None
    }

    /// Не добавляет отдельную метку исходника к поверхности.
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// Запрашивает ширину поля и место для пяти видимых строк.
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = VIEWPORT_HEIGHT.into();
        (window.request_layout(style, [], cx), ())
    }

    /// Раскладывает текст с переносами и подбирает прокрутку для видимого курсора.
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> TextLayout {
        let editor = self.0.read(cx);
        let style = window.text_style();
        let placeholder = editor.text.is_empty();
        let text = if placeholder {
            "Сообщение…"
        } else {
            editor.text()
        };
        let selection = editor.selection();
        let mut boundaries = vec![0, text.len(), selection.start, selection.end];
        if let Some(marked) = &editor.marked {
            boundaries.extend([marked.start, marked.end]);
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        let runs = boundaries
            .windows(2)
            .map(|pair| TextRun {
                len: pair[1] - pair[0],
                font: style.font(),
                color: if placeholder {
                    rgb(0x9a9da8).into()
                } else {
                    style.color
                },
                background_color: selection
                    .contains(&pair[0])
                    .then(|| rgba(0x508eff70).into()),
                underline: editor
                    .marked
                    .as_ref()
                    .filter(|range| range.contains(&pair[0]))
                    .map(|_| UnderlineStyle {
                        color: Some(style.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                strikethrough: None,
            })
            .collect::<Vec<_>>();
        let mut top = px(0.);
        let mut start = 0;
        let lines = window
            .text_system()
            .shape_text(
                text.to_string().into(),
                px(16.),
                &runs,
                Some(bounds.size.width.max(px(1.))),
                None,
            )
            .expect("Не удалось разложить текст редактора")
            .into_iter()
            .map(|shape| {
                let next_top = top + shape.size(LINE_HEIGHT).height;
                let next_start = start + shape.text.len() + 1;
                let line = TextLine { shape, start, top };
                top = next_top;
                start = next_start;
                line
            })
            .collect();
        let mut layout = TextLayout {
            bounds,
            lines,
            scroll: px(0.),
            text_len: editor.text.len(),
        };
        let caret_y = layout
            .position(editor.caret)
            .map_or(px(0.), |position| position.y - bounds.top());
        let previous = editor
            .layout
            .as_ref()
            .map_or(px(0.), |layout| layout.scroll);
        layout.scroll = previous
            .clamp(
                (caret_y + LINE_HEIGHT - bounds.size.height).max(px(0.)),
                caret_y.max(px(0.)),
            )
            .min((top - bounds.size.height).max(px(0.)));
        layout
    }

    /// Рисует видимую часть строк, выделение и курсор. Запоминает геометрию для системного ввода.
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        layout: &mut TextLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let editor = self.0.read(cx);
        let focus = editor.focus.clone();
        let caret = editor
            .selection()
            .is_empty()
            .then(|| layout.position(editor.caret))
            .flatten();
        let focused = focus.is_focused(window);
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.0.clone()), cx);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for line in &layout.lines {
                let origin = layout.line_origin(line);
                if origin.y >= bounds.bottom()
                    || origin.y + line.shape.size(LINE_HEIGHT).height <= bounds.top()
                {
                    continue;
                }
                line.shape
                    .paint_background(origin, LINE_HEIGHT, TextAlign::Left, None, window, cx)
                    .expect("Не удалось нарисовать выделение");
                line.shape
                    .paint(origin, LINE_HEIGHT, TextAlign::Left, None, window, cx)
                    .expect("Не удалось нарисовать строку");
            }
            if focused && let Some(caret) = caret {
                window.paint_quad(fill(
                    Bounds::new(caret, size(px(1.5), LINE_HEIGHT)),
                    rgb(0x94b3ff),
                ));
            }
        });
        self.0.update(cx, |editor, _| {
            editor.layout_current = true;
            editor.layout = Some(TextLayout {
                bounds: layout.bounds,
                lines: std::mem::take(&mut layout.lines),
                scroll: layout.scroll,
                text_len: layout.text_len,
            });
        });
    }
}
