//! Многострочное текстовое поле GPUI. Сначала состояние и действия, затем связь с системным вводом и разметка.

use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

use workspace_core::messenger::encryption::fragments::FragmentEdit;
use zeroize::Zeroize;

use crate::gpui_client::{
    preferences::{Language, Preferences},
    ui::Palette,
};

mod formatting;
mod text_menu;

pub(super) use formatting::Format;

use text_menu::TextMenu;

use gpui::{
    App, AvailableSpace, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, InspectorElementId, KeyBinding, KeyDownEvent, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, Style, TextAlign, TextRun,
    TextStyle, UTF16Selection, UnderlineStyle, Window, WrappedLine, div, fill, point, prelude::*,
    px, relative, rgb, rgba, size,
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
            OpenTextMenu,
        ]
    );
}

const PROBE_LINE_HEIGHT: Pixels = px(22.);
const PROBE_HEIGHT: Pixels = px(110.);
const LINE_HEIGHT: Pixels = px(24.);
const MAX_TEXT_HEIGHT: Pixels = px(112.); // Вся рамка поля ограничена 128 px.
const FONT_SIZE: Pixels = px(16.);

/// Нажатие Enter в рабочем редакторе. Доступность отправки определяет Composer.
pub(super) struct SubmitRequested;

impl EventEmitter<SubmitRequested> for Editor {}

/// Стрелка вверх в пустом поле может открыть последнюю доступную правку.
pub(super) struct EditPreviousRequested;

impl EventEmitter<EditPreviousRequested> for Editor {}

/// Черновик разговора и локальное состояние выделения. Индексы считаются в байтах UTF-8.
pub struct Editor {
    version: u64,        // Меняется при каждом изменении текста, даже если его восстановили.
    framed_probe: bool,  // Демонстрационные окна сохраняют нейтральную рамку прототипа.
    send_on_enter: bool, // В рабочем поле Enter передаёт намерение отправки.
    text: String,        // Текст с системными переводами строк, приведёнными к LF.
    fragment_edit: Option<Arc<FragmentEdit>>, // Разрешение на показ и экспорт текста смешанной правки.
    anchor: usize,                            // Неподвижный конец выделения.
    caret: usize,                             // Положение курсора или подвижный конец выделения.
    marked: Option<Range<usize>>,             // Незавершённая композиция IME.
    focus: FocusHandle,                       // Связь состояния редактора с фокусом GPUI.
    layout: Option<TextLayout>,               // Последняя нарисованная геометрия для мыши и IME.
    layout_current: bool,                     // Геометрия относится к текущей версии текста.
    dragging: bool,                           // Зажата кнопка мыши при выделении текста.
    text_menu: TextMenu,                      // Доступность и фокус контекстных команд поля.
}

impl Editor {
    /// Готовит пустой редактор, доступный через Tab.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            version: 0,
            framed_probe: true,
            send_on_enter: false,
            text: String::new(),
            fragment_edit: None,
            anchor: 0,
            caret: 0,
            marked: None,
            focus: cx.focus_handle().tab_stop(true),
            layout: None,
            layout_current: false,
            dragging: false,
            text_menu: TextMenu::new(cx),
        }
    }

    /// Создаёт поле для общей рамки Workspace в разговоре и галерее.
    pub fn for_conversation(cx: &mut Context<Self>) -> Self {
        Self {
            framed_probe: false,
            ..Self::new(cx)
        }
    }

    /// Создаёт рабочее поле, в котором Enter запрашивает отправку.
    pub(super) fn for_composer(cx: &mut Context<Self>) -> Self {
        Self {
            send_on_enter: true,
            ..Self::for_conversation(cx)
        }
    }

    /// Возвращает версию текста, чтобы поздняя запись не очистила новый ввод.
    pub(super) fn version(&self) -> u64 {
        self.version
    }

    /// Не даёт отправить ещё не подтверждённую системную композицию.
    pub(super) fn composing(&self) -> bool {
        self.marked.is_some()
    }

    /// Очищает сохранённый текст, сохраняя фокус текущего редактора.
    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        self.replace(0..self.text.len(), "", cx);
    }

    /// Показывает снимок выбранной вкладки ответа.
    pub(in crate::gpui_client) fn show_reply(&mut self, text: &str, cx: &mut Context<Self>) {
        self.replace(0..self.text.len(), text, cx);
    }

    /// Показывает открытые фрагменты под разрешением всех исходных ключей.
    pub(super) fn show_fragment_edit(
        &mut self,
        text: &str,
        access: Arc<FragmentEdit>,
        cx: &mut Context<Self>,
    ) {
        self.fragment_edit = Some(access);
        self.layout = None;
        self.show_reply(text, cx);
    }

    /// Сразу убирает текст и его геометрию, прежде чем вернуть прежний черновик.
    pub(super) fn finish_fragment_edit(&mut self) {
        if self.fragment_edit.take().is_some() {
            self.text.zeroize();
            self.anchor = 0;
            self.caret = 0;
            self.layout = None;
            self.layout_current = false;
            self.version = self.version.wrapping_add(1);
        }
    }

    /// Выполняет чтение отредактированного текста до отзыва исходного доступа.
    fn with_current_text<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        match &self.fragment_edit {
            Some(edit) => edit.with_markdown(|_| read()),
            None => Some(read()),
        }
    }

    /// Подключает клавиши редактора. Регистрируется один раз при запуске приложения.
    pub fn bind_keys(cx: &mut App) {
        cx.bind_keys([
            KeyBinding::new(
                "backspace",
                keys::Backspace,
                Some("WorkspaceComposerEditor"),
            ),
            KeyBinding::new("delete", keys::Delete, Some("WorkspaceComposerEditor")),
            KeyBinding::new(
                "enter",
                keys::Newline,
                Some("WorkspaceComposerEditor && !ComposerSends"),
            ),
            KeyBinding::new(
                "shift-enter",
                keys::Newline,
                Some("WorkspaceComposerEditor"),
            ),
            KeyBinding::new("left", keys::Left, Some("WorkspaceComposerEditor")),
            KeyBinding::new("right", keys::Right, Some("WorkspaceComposerEditor")),
            KeyBinding::new("up", keys::Up, Some("WorkspaceComposerEditor")),
            KeyBinding::new("down", keys::Down, Some("WorkspaceComposerEditor")),
            KeyBinding::new(
                "shift-left",
                keys::SelectLeft,
                Some("WorkspaceComposerEditor"),
            ),
            KeyBinding::new(
                "shift-right",
                keys::SelectRight,
                Some("WorkspaceComposerEditor"),
            ),
            KeyBinding::new("cmd-a", keys::SelectAll, Some("WorkspaceComposerEditor")),
            KeyBinding::new("cmd-c", keys::Copy, Some("WorkspaceComposerEditor")),
            KeyBinding::new("cmd-x", keys::Cut, Some("WorkspaceComposerEditor")),
            KeyBinding::new("cmd-v", keys::Paste, Some("WorkspaceComposerEditor")),
            KeyBinding::new("shift-f10", keys::OpenTextMenu, Some("ComposerSends")),
            KeyBinding::new("menu", keys::OpenTextMenu, Some("ComposerSends")),
        ]);
    }

    /// Возвращает текущее содержимое, в том числе незавершённую композицию IME.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Проверяет, доступно ли форматирование текущего выделения.
    pub(super) fn can_format(&self) -> bool {
        !self.composing() && !self.text_menu.is_open()
    }

    /// Выполняет действие панели или сочетания клавиш тем же путём, что и обычный ввод.
    pub(super) fn apply_format(&mut self, format: Format, cx: &mut Context<Self>) {
        if !self.can_format() {
            return;
        }
        let selection = self.selection();
        let language = Preferences::read(cx).language;
        let (text, relative_selection) = format.edit(&self.text[selection.clone()], language);
        let inserted = self.replace(selection, &text, cx);
        self.anchor = inserted.start + relative_selection.start;
        self.caret = inserted.start + relative_selection.end;
    }

    /// Применяет выбранную комбинацию только в рабочем поле без активной композиции IME.
    fn format_shortcut(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::gpui_client::preferences::{Preferences, shortcuts::matching};
        if self.marked.is_some() || self.text_menu.is_open() {
            return;
        }
        let Some(shortcut) = matching(&event.keystroke, Preferences::read(cx)) else {
            return;
        };
        let format = match shortcut.id {
            "composer.bold" => Format::Bold,
            "composer.italic" => Format::Italic,
            "composer.strike" => Format::Strike,
            "composer.code" => Format::InlineCode,
            _ => return,
        };
        self.apply_format(format, cx);
        window.prevent_default();
        cx.stop_propagation();
    }

    /// Продолжает список по Shift+Enter или вставляет обычный перевод строки.
    fn insert_newline(&mut self, cx: &mut Context<Self>) {
        if self.composing() {
            return;
        }
        let selection = self.selection();
        if self.send_on_enter
            && selection.is_empty()
            && let Some((range, text)) = formatting::list_newline(&self.text, self.caret)
        {
            self.replace(range, &text, cx);
        } else {
            self.replace(selection, "\n", cx);
        }
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
        let line_height = layout.line_height;
        position.y += if down { line_height } else { -line_height } + px(1.);
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
            self.version = self.version.wrapping_add(1);
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

    /// Выделяет весь текст, оставляя курсор у конца.
    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.anchor = 0;
        self.move_to(self.text.len(), true, cx);
    }

    /// Копирует выделенный текст, при вырезании удаляет его после записи в буфер.
    fn copy(&mut self, cut: bool, cx: &mut Context<Self>) {
        let selection = self.selection();
        if !selection.is_empty() {
            let copied = self.with_current_text(|| {
                cx.write_to_clipboard(ClipboardItem::new_string(
                    self.text[selection.clone()].into(),
                ))
            });
            if cut && copied.is_some() {
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

    /// Передаёт обычный Enter один раз. AppKit первым получает клавиши незавершённого IME.
    fn handle_enter(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key != "enter" || event.keystroke.modifiers.modified() {
            return;
        }
        cx.stop_propagation();
        if !event.is_held && !self.composing() && !self.text_menu.is_open() {
            cx.emit(SubmitRequested);
        }
    }

    /// В непустом редакторе перемещает каретку, в пустом просит открыть правку.
    fn move_up(&mut self, cx: &mut Context<Self>) {
        if self.text.is_empty() && !self.composing() && !self.text_menu.is_open() {
            cx.emit(EditPreviousRequested);
        } else {
            self.vertical(false, cx);
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
        self.with_current_text(|| {
            let bytes = utf8_range(&self.text, range);
            *adjusted =
                Some(utf16_offset(&self.text, bytes.start)..utf16_offset(&self.text, bytes.end));
            self.text[bytes].into()
        })
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
            size((right - start.x).max(px(1.)), layout.line_height),
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
            .id("workspace-composer-editor")
            .debug_selector(|| "workspace-composer-editor".into())
            .w_full()
            .min_w_0()
            .py(if self.framed_probe { px(8.) } else { px(7.) })
            .when(self.framed_probe, |field| {
                field
                    .px_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(border))
                    .bg(rgb(0x24272d))
                    .text_color(rgb(0xf1f2f5))
            })
            .text_size(FONT_SIZE)
            .key_context(if self.text_menu.is_open() {
                ""
            } else if self.send_on_enter {
                "WorkspaceComposerEditor ComposerSends"
            } else {
                "WorkspaceComposerEditor"
            })
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .when(self.send_on_enter, |field| {
                field
                    .on_key_down(cx.listener(Self::handle_enter))
                    .on_key_down(cx.listener(Self::format_shortcut))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(Self::open_text_menu_at_pointer),
                    )
                    .on_action(cx.listener(|editor, _: &keys::OpenTextMenu, window, cx| {
                        editor.open_text_menu_from_keyboard(window, cx);
                    }))
            })
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
            .on_action(cx.listener(|editor, _: &keys::Up, _, cx| editor.move_up(cx)))
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
                editor.insert_newline(cx);
            }))
            .on_action(cx.listener(|editor, _: &keys::SelectAll, _, cx| {
                editor.select_all(cx);
            }))
            .on_action(cx.listener(|editor, _: &keys::Copy, _, cx| editor.copy(false, cx)))
            .on_action(cx.listener(|editor, _: &keys::Cut, _, cx| editor.copy(true, cx)))
            .on_action(cx.listener(|editor, _: &keys::Paste, _, cx| editor.paste(cx)))
            .child(EditorSurface(cx.entity()))
            .children(self.render_text_menu(window, cx))
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
    line_height: Pixels,    // Высота визуальной строки для курсора, мыши и AppKit.
}

impl TextLayout {
    /// Находит положение курсора в координатах окна.
    fn position(&self, index: usize) -> Option<Point<Pixels>> {
        if self.text_len == 0 {
            return Some(self.bounds.origin);
        }
        self.lines.iter().find_map(|line| {
            let local = index.checked_sub(line.start)?;
            let offset = line.shape.position_for_index(local, self.line_height)?;
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
            if local.y < line.shape.size(self.line_height).height {
                return (line.start
                    + line
                        .shape
                        .closest_index_for_position(local, self.line_height)
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

/// Текст, измеренный для одной ширины. Переносы нужны разметке и курсору.
struct SurfaceMeasurement {
    width: Pixels,        // Ширина, при которой посчитаны переносы.
    height: Pixels,       // Полная высота текста до ограничения редактора.
    lines: Vec<TextLine>, // Готовые строки с оформлением выделения и IME.
}

/// Сохраняет результат разметки для отрисовки на той же ширине.
type LayoutMeasurement = Rc<RefCell<Option<SurfaceMeasurement>>>;

/// Накладывает оформление и разбивает текст по ширине видимой поверхности.
fn measure_text(
    editor: &Editor,
    style: &TextStyle,
    palette: Palette,
    locale: Language,
    width: Pixels,
    line_height: Pixels,
    window: &Window,
) -> SurfaceMeasurement {
    editor
        .with_current_text(|| {
            measure_current_text(editor, style, palette, locale, width, line_height, window)
        })
        .unwrap_or_else(|| SurfaceMeasurement {
            width,
            height: line_height,
            lines: Vec::new(),
        })
}

/// Вычисляет строки для уже допущенного текста редактора.
fn measure_current_text(
    editor: &Editor,
    style: &TextStyle,
    palette: Palette,
    locale: Language,
    width: Pixels,
    line_height: Pixels,
    window: &Window,
) -> SurfaceMeasurement {
    let placeholder = editor.text.is_empty();
    let text = if !placeholder {
        editor.text()
    } else if editor.framed_probe {
        locale.tr("desktop.messenger.composer.compactPlaceholder")
    } else {
        locale.tr("desktop.messenger.composer.placeholder")
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
                if editor.framed_probe {
                    rgb(0x9a9da8).into()
                } else {
                    palette.text_muted
                }
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
            FONT_SIZE,
            &runs,
            Some(width.max(px(1.))),
            None,
        )
        .expect("Не удалось разложить текст редактора")
        .into_iter()
        .map(|shape| {
            let next_top = top + shape.size(line_height).height;
            let next_start = start + shape.text.len() + 1;
            let line = TextLine { shape, start, top };
            top = next_top;
            start = next_start;
            line
        })
        .collect();
    SurfaceMeasurement {
        width,
        height: top,
        lines,
    }
}

impl Element for EditorSurface {
    type RequestLayoutState = LayoutMeasurement;
    type PrepaintState = TextLayout;

    /// Оставляет идентификатор за внешним контейнером.
    fn id(&self) -> Option<ElementId> {
        None
    }

    /// Не добавляет отдельную метку исходника к поверхности.
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// Выбирает высоту видимых строк по текущей ширине, сохраняя исходный viewport пробы.
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, LayoutMeasurement) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let measurement: LayoutMeasurement = Rc::default();
        if self.0.read(cx).framed_probe {
            style.size.height = PROBE_HEIGHT.into();
            return (window.request_layout(style, [], cx), measurement);
        }

        let editor = self.0.clone();
        let text_style = window.text_style();
        let palette = Palette::for_window(window, cx);
        let cache = measurement.clone();
        let id = window.request_measured_layout(style, move |known, available, window, cx| {
            let width = match (known.width, available.width) {
                (Some(width), _) | (None, AvailableSpace::Definite(width)) => width,
                _ => return size(px(0.), LINE_HEIGHT),
            };
            let mut cache = cache.borrow_mut();
            let measured = cache
                .take()
                .filter(|previous| previous.width == width)
                .unwrap_or_else(|| {
                    measure_text(
                        editor.read(cx),
                        &text_style,
                        palette,
                        Preferences::read(cx).language,
                        width,
                        LINE_HEIGHT,
                        window,
                    )
                });
            let height = measured.height.clamp(LINE_HEIGHT, MAX_TEXT_HEIGHT);
            *cache = Some(measured);
            size(width, height)
        });
        (id, measurement)
    }

    /// Находит видимую область курсора и использует переносы, рассчитанные при измерении.
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        measurement: &mut LayoutMeasurement,
        window: &mut Window,
        cx: &mut App,
    ) -> TextLayout {
        let editor = self.0.read(cx);
        let line_height = if editor.framed_probe {
            PROBE_LINE_HEIGHT
        } else {
            LINE_HEIGHT
        };
        let SurfaceMeasurement { lines, height, .. } = measurement
            .borrow_mut()
            .take()
            .filter(|previous| previous.width == bounds.size.width)
            .unwrap_or_else(|| {
                measure_text(
                    editor,
                    &window.text_style(),
                    Palette::for_window(window, cx),
                    Preferences::read(cx).language,
                    bounds.size.width,
                    line_height,
                    window,
                )
            });
        let mut layout = TextLayout {
            bounds,
            lines,
            scroll: px(0.),
            text_len: editor.text.len(),
            line_height,
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
                (caret_y + line_height - bounds.size.height).max(px(0.)),
                caret_y.max(px(0.)),
            )
            .min((height - bounds.size.height).max(px(0.)));
        layout
    }

    /// Рисует видимую часть строк, выделение и курсор. Запоминает геометрию для системного ввода.
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut LayoutMeasurement,
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
        let caret_color = if editor.framed_probe {
            rgb(0x94b3ff).into()
        } else {
            Palette::for_window(window, cx).accent
        };
        let permission = editor.fragment_edit.clone();
        let mut draw = || {
            window.handle_input(&focus, ElementInputHandler::new(bounds, self.0.clone()), cx);
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                for line in &layout.lines {
                    let origin = layout.line_origin(line);
                    if origin.y >= bounds.bottom()
                        || origin.y + line.shape.size(layout.line_height).height <= bounds.top()
                    {
                        continue;
                    }
                    line.shape
                        .paint_background(
                            origin,
                            layout.line_height,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        )
                        .expect("Не удалось нарисовать выделение");
                    line.shape
                        .paint(
                            origin,
                            layout.line_height,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        )
                        .expect("Не удалось нарисовать строку");
                }
                if focused && let Some(caret) = caret {
                    window.paint_quad(fill(
                        Bounds::new(caret, size(px(1.5), layout.line_height)),
                        caret_color,
                    ));
                }
            });
        };
        if let Some(permission) = permission {
            if permission.with_markdown(|_| draw()).is_none() {
                layout.lines.clear();
                return;
            }
        } else {
            draw();
        }
        self.0.update(cx, |editor, _| {
            editor.layout_current = true;
            editor.layout = Some(TextLayout {
                bounds: layout.bounds,
                lines: std::mem::take(&mut layout.lines),
                scroll: layout.scroll,
                text_len: layout.text_len,
                line_height: layout.line_height,
            });
        });
    }
}
