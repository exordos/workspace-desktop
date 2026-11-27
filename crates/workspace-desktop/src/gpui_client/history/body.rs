//! Разбирает обычный Markdown при принятии снимка, чтобы сдвиг бабла менял только раскладку.

use std::ops::Range;

use gpui::SharedString;
use markdown::{ParseOptions, mdast::Node, to_mdast};
use uuid::Uuid;
use workspace_core::messenger::{
    catalog::MessagePreview,
    encryption::fragments::{
        FragmentDisplayKind, FragmentDisplaySpan, FragmentReference, fragment_display_spans,
    },
    forward::{ForwardReference, ForwardSource, ForwardSpan, MAX_FORWARD_DEPTH, forward_spans},
    quote::{self, Block},
    references::{MediaReference, WorkspaceReferenceTarget},
};
use zeroize::Zeroizing;

use super::super::inline_references::{InlineText, Reference, open_references};
use crate::gpui_client::{composer::short_reply_text, preferences::Language};

/// Стиль обычного Markdown, привязанный к байтам исходного сообщения.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::gpui_client) struct MarkdownFormatSpan {
    pub range: Range<usize>, // Границы текста в исходном теле.
    pub bold: bool,          // Сильное выделение включено на всём участке.
    pub italic: bool,        // Наклонное выделение включено на всём участке.
    pub strike: bool,        // Перечёркивание включено на всём участке.
    pub code: bool,          // Моноширинный стиль включён на всём участке.
}

/// Отображаемый фрагмент Markdown, связанный с исходными байтовыми границами.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::gpui_client) struct MarkdownRun {
    pub source_range: Range<usize>, // Полный участок исходного синтаксического узла.
    pub content_range: Range<usize>, // Исходные байты видимого содержимого без разделителей.
    pub text: SharedString, // Текст после разбора; карта защищённого тела хранит здесь пустую строку.
    pub bold: bool,         // Сильное выделение включено.
    pub italic: bool,       // Наклонное выделение включено.
    pub strike: bool,       // Перечёркивание включено.
    pub code: bool,         // Моноширинный стиль включён.
    pub break_kind: Option<MarkdownBreak>, // Явный перенос, если текст пустой.
    pub kind: MarkdownTokenKind, // Правило извлечения видимого текста из source range.
}

/// Описывает оформление видимого фрагмента без владения его текстом.
#[derive(Clone, Copy)]
pub(in crate::gpui_client) struct MarkdownRunStyle {
    pub bold: bool,                        // Сильное выделение включено.
    pub italic: bool,                      // Наклонное выделение включено.
    pub strike: bool,                      // Перечёркивание включено.
    pub code: bool,                        // Моноширинный стиль включён.
    pub break_kind: Option<MarkdownBreak>, // Видимый перенос вместо обычного текста.
}

/// Вид узла, необходимый для разбора текста прямо во время защищённого показа.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::gpui_client) enum MarkdownTokenKind {
    Plain,        // Обычный текст с экранированием и HTML-ссылками на символы.
    InlineCode,   // Код между обратными кавычками.
    CodeBlock,    // Текст между ограждениями блока кода.
    IndentedCode, // Блок кода с отступом в четыре пробела.
    Html,         // Текст HTML, который показывается как обычный текст.
    ImageAlt,     // Подпись изображения без адреса.
    Break,        // Явный разрыв строки.
    Paragraph,    // Пустое место между абзацами.
    Synthetic,    // Короткий служебный знак списка или таблицы.
}

/// Перенос строки из Markdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::gpui_client) enum MarkdownBreak {
    Soft,      // Перенос исходной строки без пустой строки.
    Paragraph, // Граница двух абзацев.
    Hard,      // Явный перенос строки.
}

/// Полный разбор Markdown с позициями стилей в исходном тексте.
#[derive(Eq, PartialEq)]
pub(in crate::gpui_client) struct MarkdownDocument {
    pub formats: Vec<MarkdownFormatSpan>, // Форматы собираются один раз для всего тела.
    source: SharedString,                 // Исходный текст для безопасных срезов видимых узлов.
    runs: Vec<MarkdownRun>,               // Видимые узлы в порядке исходного текста.
}

/// Карта форматирования без открытого текста для отзываемых сообщений.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::gpui_client) struct MarkdownFormatMap {
    pub formats: Vec<MarkdownFormatSpan>, // Только позиции и признаки оформления.
    runs: Vec<MarkdownRun>,               // Только виды токенов и их диапазоны, без текста тела.
    opaque_ranges: Vec<Range<usize>>,     // Адреса ссылок, которые заменены своими элементами.
}

impl MarkdownDocument {
    /// Разбирает весь текст, временно скрывая адреса собственных встроенных элементов.
    pub(in crate::gpui_client) fn new(source: &str, opaque_ranges: &[Range<usize>]) -> Self {
        let masked = mask_markdown(source, opaque_ranges);
        let mut formats = Vec::new();
        let mut runs = Vec::new();
        if let Ok(root) = to_mdast(&masked, &ParseOptions::gfm()) {
            collect_formats(&root, MarkdownStyle::default(), &mut formats);
            collect_visible_runs(&root, MarkdownStyle::default(), &mut runs, &masked, true);
        }
        Self {
            formats,
            source: masked.into(),
            runs,
        }
    }

    /// Возвращает стили, пересекающие участок, не меняя его координаты в исходном теле.
    pub(in crate::gpui_client) fn formats_in(
        &self,
        range: Range<usize>,
    ) -> impl Iterator<Item = &MarkdownFormatSpan> {
        self.formats
            .iter()
            .filter(move |format| format.range.start < range.end && range.start < format.range.end)
    }

    /// Возвращает видимые узлы, обрезанные по диапазону custom элемента или текстовой части.
    pub(in crate::gpui_client) fn visible_runs_in(&self, range: Range<usize>) -> Vec<MarkdownRun> {
        self.runs
            .iter()
            .filter_map(|run| clip_run(run, &range, &self.source))
            .collect()
    }
}

impl MarkdownFormatMap {
    /// Собирает только диапазоны стилей; открытые строки не сохраняются в карте.
    pub(in crate::gpui_client) fn new(source: &str, opaque_ranges: &[Range<usize>]) -> Self {
        let masked = mask_markdown(source, opaque_ranges);
        let mut formats = Vec::new();
        let mut runs = Vec::new();
        if let Ok(root) = to_mdast(&masked, &ParseOptions::gfm()) {
            collect_formats(&root, MarkdownStyle::default(), &mut formats);
            collect_visible_runs(&root, MarkdownStyle::default(), &mut runs, &masked, false);
        }
        Self {
            formats,
            runs,
            opaque_ranges: opaque_ranges.to_vec(),
        }
    }

    /// Возвращает стили текущего защищённого диапазона без чтения его текста.
    #[cfg(test)]
    pub(in crate::gpui_client) fn formats_in(
        &self,
        range: Range<usize>,
    ) -> impl Iterator<Item = &MarkdownFormatSpan> {
        self.formats
            .iter()
            .filter(move |format| format.range.start < range.end && range.start < format.range.end)
    }

    /// Передаёт видимый текст по одному фрагменту и стирает временную строку после callback.
    pub(in crate::gpui_client) fn for_each_visible_run_in(
        &self,
        source: &str,
        range: Range<usize>,
        mut visit: impl FnMut(MarkdownRunStyle, &str),
    ) {
        for run in &self.runs {
            let style = MarkdownRunStyle {
                bold: run.bold,
                italic: run.italic,
                strike: run.strike,
                code: run.code,
                break_kind: run.break_kind,
            };
            if run.break_kind.is_some() {
                if range.start < run.source_range.end && run.source_range.start < range.end {
                    visit(style, "");
                }
                continue;
            }
            if run.kind == MarkdownTokenKind::Synthetic {
                if range.start <= run.source_range.start && run.source_range.start < range.end {
                    visit(style, run.text.as_ref());
                }
                continue;
            }
            let clipped =
                run.content_range.start.max(range.start)..run.content_range.end.min(range.end);
            for segment in public_segments(clipped, &self.opaque_ranges) {
                let Some(raw) = source.get(segment) else {
                    continue;
                };
                let text = Zeroizing::new(match run.kind {
                    MarkdownTokenKind::Plain | MarkdownTokenKind::ImageAlt => {
                        decode_markdown_text(raw)
                    }
                    MarkdownTokenKind::InlineCode => normalize_code_span(raw),
                    MarkdownTokenKind::CodeBlock => raw.replace("\r\n", "\n"),
                    MarkdownTokenKind::IndentedCode => normalize_indented_code(raw),
                    MarkdownTokenKind::Html => raw.to_owned(),
                    MarkdownTokenKind::Break
                    | MarkdownTokenKind::Paragraph
                    | MarkdownTokenKind::Synthetic => String::new(),
                });
                visit(style, &text);
            }
        }
    }
}

/// Заменяет содержимое встроенных элементов пробелами той же байтовой длины.
fn mask_markdown(source: &str, opaque_ranges: &[Range<usize>]) -> String {
    let mut masked = source.as_bytes().to_vec();
    for range in opaque_ranges {
        if range.start > range.end || range.end > masked.len() {
            continue;
        }
        for byte in &mut masked[range.clone()] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(masked).unwrap_or_else(|_| source.to_owned())
}

/// Вычитает из видимого среза позиции, которые принадлежат встроенным элементам.
fn public_segments(range: Range<usize>, opaque: &[Range<usize>]) -> Vec<Range<usize>> {
    if range.start >= range.end {
        return Vec::new();
    }
    let mut hidden = opaque
        .iter()
        .filter_map(|opaque| {
            let start = opaque.start.max(range.start);
            let end = opaque.end.min(range.end);
            (start < end).then_some(start..end)
        })
        .collect::<Vec<_>>();
    hidden.sort_by_key(|range| range.start);
    let mut segments = Vec::new();
    let mut cursor = range.start;
    for range in hidden {
        if range.start > cursor {
            segments.push(cursor..range.start);
        }
        cursor = cursor.max(range.end);
    }
    if cursor < range.end {
        segments.push(cursor..range.end);
    }
    segments
}

/// Читает исходный диапазон видимого содержимого без разделителей кода.
fn visible_content_range(
    source: &str,
    range: Range<usize>,
    kind: MarkdownTokenKind,
) -> Range<usize> {
    let Some(raw) = source.get(range.clone()) else {
        return range;
    };
    match kind {
        MarkdownTokenKind::InlineCode => {
            let delimiter = raw
                .chars()
                .take_while(|character| *character == '`')
                .count();
            if delimiter == 0 || raw.len() < delimiter * 2 {
                return range;
            }
            let start = range.start + delimiter;
            let end = range.end - delimiter;
            let content = source.get(start..end).unwrap_or_default();
            if content.len() > 1
                && content.starts_with(' ')
                && content.ends_with(' ')
                && content.trim().chars().any(|character| character != ' ')
            {
                start + 1..end - 1
            } else {
                start..end
            }
        }
        MarkdownTokenKind::ImageAlt => image_alt_range(raw).map_or(range.clone(), |alt| {
            range.start + alt.start..range.start + alt.end
        }),
        MarkdownTokenKind::CodeBlock => fenced_code_content_range(raw, range),
        MarkdownTokenKind::IndentedCode => range,
        _ => range,
    }
}

/// Снимает ограждения только у fenced code; indented code хранит весь исходный диапазон.
fn fenced_code_content_range(raw: &str, range: Range<usize>) -> Range<usize> {
    let first_line_end = raw.find('\n').unwrap_or(raw.len());
    let opening = raw[..first_line_end].trim_start();
    let Some(fence) = opening
        .chars()
        .next()
        .filter(|char| matches!(char, '`' | '~'))
    else {
        return range;
    };
    let fence_len = opening.chars().take_while(|char| *char == fence).count();
    if fence_len < 3 {
        return range;
    }
    if first_line_end == raw.len() {
        return range.end..range.end;
    }
    let closing_line = raw
        .match_indices('\n')
        .map(|(index, _)| index + 1)
        .filter(|&line_start| {
            let line = raw[line_start..]
                .lines()
                .next()
                .unwrap_or_default()
                .trim_start();
            let count = line.chars().take_while(|char| *char == fence).count();
            count >= fence_len && line[count..].trim().is_empty()
        })
        .last();
    let Some(closing) = closing_line else {
        return range.start + first_line_end + 1..range.end;
    };
    let mut end = closing;
    if end > first_line_end + 1 && raw.as_bytes().get(end - 1) == Some(&b'\n') {
        end -= 1;
    }
    range.start + first_line_end.min(raw.len()) + usize::from(first_line_end < raw.len())
        ..range.start + end
}

/// Проверяет, начинается ли code block с Markdown-ограждения, а не с отступа.
fn is_fenced_code(raw: &str) -> bool {
    let line = raw.lines().next().unwrap_or_default().trim_start();
    let Some(fence) = line.chars().next().filter(|char| matches!(char, '`' | '~')) else {
        return false;
    };
    line.chars().take_while(|char| *char == fence).count() >= 3
}

/// Убирает по четыре начальных пробела с каждой строки indented code.
fn normalize_indented_code(source: &str) -> String {
    source
        .split_inclusive('\n')
        .map(|line| {
            let indent = line
                .bytes()
                .take_while(|byte| *byte == b' ')
                .take(4)
                .count();
            line.get(indent..).unwrap_or(line)
        })
        .collect::<String>()
        .replace("\r\n", "\n")
}

/// Находит содержимое alt с учётом вложенных скобок и экранирования.
fn image_alt_range(source: &str) -> Option<Range<usize>> {
    let start = source.find("![")? + 2;
    let mut depth = 1usize;
    let mut cursor = start;
    while cursor < source.len() {
        let rest = source.get(cursor..)?;
        if rest.starts_with('\\') {
            cursor += 1;
            cursor += source.get(cursor..)?.chars().next()?.len_utf8();
            continue;
        }
        let character = rest.chars().next()?;
        match character {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start..cursor);
                }
            }
            _ => {}
        }
        cursor += character.len_utf8();
    }
    None
}

/// Раскрывает HTML-ссылки на символы и обратную косую черту в обычном Markdown-тексте.
fn decode_markdown_text(source: &str) -> String {
    let mut output = String::new();
    let mut cursor = 0;
    while cursor < source.len() {
        let remaining = source.get(cursor..).unwrap_or_default();
        if let Some((length, decoded)) = decode_entity(remaining) {
            output.push_str(&decoded);
            cursor += length;
        } else if remaining.starts_with('\\') {
            let mut characters = remaining.char_indices();
            let _ = characters.next();
            if let Some((offset, character)) = characters.next() {
                if character.is_ascii_punctuation() {
                    output.push(character);
                    cursor += offset + character.len_utf8();
                } else {
                    output.push('\\');
                    cursor += 1;
                }
            } else {
                output.push('\\');
                cursor += 1;
            }
        } else if remaining.starts_with("\r\n") {
            output.push('\n');
            cursor += 2;
        } else if let Some(character) = remaining.chars().next() {
            output.push(character);
            cursor += character.len_utf8();
        } else {
            break;
        }
    }
    output
}

/// Приводит code span к отображаемому виду без обратных кавычек.
fn normalize_code_span(source: &str) -> String {
    let text = source.replace("\r\n", " ").replace('\n', " ");
    if text.len() > 1
        && text.starts_with(' ')
        && text.ends_with(' ')
        && text.trim().chars().any(|character| character != ' ')
    {
        text[1..text.len() - 1].to_owned()
    } else {
        text
    }
}

/// Собирает видимые узлы, не раскрывая строки, принадлежащие custom элементам.
fn collect_visible_runs(
    node: &Node,
    style: MarkdownStyle,
    runs: &mut Vec<MarkdownRun>,
    source: &str,
    retain_text: bool,
) {
    match node {
        Node::Text(value) => push_visible(
            value.position.as_ref(),
            &value.value,
            style,
            MarkdownTokenKind::Plain,
            runs,
            source,
            retain_text,
        ),
        Node::InlineCode(value) => {
            let mut nested = style;
            nested.code = true;
            push_visible(
                value.position.as_ref(),
                &value.value,
                nested,
                MarkdownTokenKind::InlineCode,
                runs,
                source,
                retain_text,
            );
        }
        Node::Code(value) => {
            let mut nested = style;
            nested.code = true;
            let position = value.position.as_ref();
            let kind = position
                .and_then(|position| source.get(position.start.offset..position.end.offset))
                .filter(|raw| is_fenced_code(raw))
                .map_or(MarkdownTokenKind::IndentedCode, |_| {
                    MarkdownTokenKind::CodeBlock
                });
            push_visible(
                position,
                &value.value,
                nested,
                kind,
                runs,
                source,
                retain_text,
            );
        }
        Node::Html(value) => push_visible(
            value.position.as_ref(),
            &value.value,
            style,
            MarkdownTokenKind::Html,
            runs,
            source,
            retain_text,
        ),
        Node::Break(value) => runs.push(MarkdownRun {
            source_range: value
                .position
                .as_ref()
                .map_or(0..0, |position| position.start.offset..position.end.offset),
            text: "".into(),
            content_range: value
                .position
                .as_ref()
                .map_or(0..0, |position| position.start.offset..position.end.offset),
            bold: style.bold,
            italic: style.italic,
            strike: style.strike,
            code: style.code,
            break_kind: Some(MarkdownBreak::Hard),
            kind: MarkdownTokenKind::Break,
        }),
        Node::Strong(value) => {
            let mut nested = style;
            nested.bold = true;
            collect_visible_children(&value.children, nested, runs, source, retain_text);
        }
        Node::Emphasis(value) => {
            let mut nested = style;
            nested.italic = true;
            collect_visible_children(&value.children, nested, runs, source, retain_text);
        }
        Node::Delete(value) => {
            let mut nested = style;
            nested.strike = true;
            collect_visible_children(&value.children, nested, runs, source, retain_text);
        }
        Node::Link(value) => {
            collect_visible_children(&value.children, style, runs, source, retain_text)
        }
        Node::Image(value) => push_visible(
            value.position.as_ref(),
            &value.alt,
            style,
            MarkdownTokenKind::ImageAlt,
            runs,
            source,
            retain_text,
        ),
        Node::Root(value) => {
            for (index, child) in value.children.iter().enumerate() {
                if index > 0 {
                    append_block_separator(&value.children[index - 1], child, runs);
                }
                collect_visible_runs(child, style, runs, source, retain_text);
            }
        }
        Node::Paragraph(value) => {
            collect_visible_children(&value.children, style, runs, source, retain_text)
        }
        Node::Heading(value) => {
            let mut heading = style;
            heading.bold = true;
            collect_visible_children(&value.children, heading, runs, source, retain_text)
        }
        Node::ThematicBreak(value) => {
            let range = value
                .position
                .as_ref()
                .map_or(0..0, |position| position.start.offset..position.end.offset);
            runs.push(MarkdownRun {
                source_range: range.clone(),
                content_range: range.start..range.start,
                // Горизонтальная линия создаётся интерфейсом, исходных данных в ней нет.
                text: "──────────".into(),
                bold: style.bold,
                italic: style.italic,
                strike: style.strike,
                code: style.code,
                break_kind: None,
                kind: MarkdownTokenKind::Synthetic,
            });
        }
        Node::Blockquote(value) => {
            collect_visible_block_children(&value.children, style, runs, source, retain_text)
        }
        Node::List(value) => {
            for (index, child) in value.children.iter().enumerate() {
                if index > 0 {
                    append_block_separator(&value.children[index - 1], child, runs);
                }
                if let Node::ListItem(item) = child {
                    let marker = if value.ordered {
                        format!("{}. ", value.start.unwrap_or(1) + index as u32)
                    } else {
                        "• ".to_owned()
                    };
                    let range = item.position.as_ref().map_or(0..0, |position| {
                        position.start.offset..position.start.offset
                    });
                    runs.push(MarkdownRun {
                        source_range: range,
                        content_range: 0..0,
                        // Это созданный рендерером знак, а не текст сообщения.
                        text: marker.into(),
                        bold: style.bold,
                        italic: style.italic,
                        strike: style.strike,
                        code: style.code,
                        break_kind: None,
                        kind: MarkdownTokenKind::Synthetic,
                    });
                }
                collect_visible_runs(child, style, runs, source, retain_text);
            }
        }
        Node::ListItem(value) => {
            collect_visible_block_children(&value.children, style, runs, source, retain_text)
        }
        Node::Table(value) => {
            for (index, child) in value.children.iter().enumerate() {
                if index > 0 {
                    append_block_separator_kind(
                        &value.children[index - 1],
                        child,
                        MarkdownBreak::Soft,
                        runs,
                    );
                }
                collect_visible_runs(child, style, runs, source, retain_text);
            }
        }
        Node::TableRow(value) => {
            for (index, child) in value.children.iter().enumerate() {
                if index > 0 {
                    let previous = node_position(&value.children[index - 1]);
                    let next = node_position(child);
                    if let (Some(previous), Some(next)) = (previous, next) {
                        runs.push(MarkdownRun {
                            source_range: previous.end.offset..next.start.offset,
                            content_range: 0..0,
                            // Разделитель таблицы задан рендерером и не содержит данных.
                            text: " | ".into(),
                            bold: style.bold,
                            italic: style.italic,
                            strike: style.strike,
                            code: style.code,
                            break_kind: None,
                            kind: MarkdownTokenKind::Synthetic,
                        });
                    }
                }
                collect_visible_runs(child, style, runs, source, retain_text);
            }
        }
        Node::TableCell(value) => {
            collect_visible_children(&value.children, style, runs, source, retain_text)
        }
        _ => {}
    }
}

/// Обходит дочерние узлы и передаёт им текущие стили.
fn collect_visible_children(
    children: &[Node],
    style: MarkdownStyle,
    runs: &mut Vec<MarkdownRun>,
    source: &str,
    retain_text: bool,
) {
    for child in children {
        collect_visible_runs(child, style, runs, source, retain_text);
    }
}

/// Обходит блочные дочерние узлы и добавляет видимые границы между ними.
fn collect_visible_block_children(
    children: &[Node],
    style: MarkdownStyle,
    runs: &mut Vec<MarkdownRun>,
    source: &str,
    retain_text: bool,
) {
    for (index, child) in children.iter().enumerate() {
        if index > 0 {
            append_block_separator(&children[index - 1], child, runs);
        }
        collect_visible_runs(child, style, runs, source, retain_text);
    }
}

/// Добавляет пустой промежуток исходника как видимую границу абзаца.
fn append_block_separator(previous: &Node, next: &Node, runs: &mut Vec<MarkdownRun>) {
    append_block_separator_kind(previous, next, MarkdownBreak::Paragraph, runs);
}

/// Добавляет заданный перенос между позициями двух соседних блочных узлов.
fn append_block_separator_kind(
    previous: &Node,
    next: &Node,
    kind: MarkdownBreak,
    runs: &mut Vec<MarkdownRun>,
) {
    let (Some(previous), Some(next)) = (node_position(previous), node_position(next)) else {
        return;
    };
    if previous.end.offset < next.start.offset {
        runs.push(MarkdownRun {
            source_range: previous.end.offset..next.start.offset,
            content_range: previous.end.offset..next.start.offset,
            text: "".into(),
            bold: false,
            italic: false,
            strike: false,
            code: false,
            break_kind: Some(kind),
            kind: MarkdownTokenKind::Paragraph,
        });
    }
}

/// Возвращает позиции блочного узла для точной вставки межблочного пробела.
fn node_position(node: &Node) -> Option<&markdown::unist::Position> {
    match node {
        Node::Paragraph(value) => value.position.as_ref(),
        Node::Heading(value) => value.position.as_ref(),
        Node::ThematicBreak(value) => value.position.as_ref(),
        Node::Blockquote(value) => value.position.as_ref(),
        Node::List(value) => value.position.as_ref(),
        Node::ListItem(value) => value.position.as_ref(),
        Node::Code(value) => value.position.as_ref(),
        Node::Html(value) => value.position.as_ref(),
        Node::Table(value) => value.position.as_ref(),
        Node::TableRow(value) => value.position.as_ref(),
        Node::TableCell(value) => value.position.as_ref(),
        _ => None,
    }
}

/// Добавляет отображаемый текст с исходной позицией и стилями родительских узлов.
fn push_visible(
    position: Option<&markdown::unist::Position>,
    text: &str,
    style: MarkdownStyle,
    kind: MarkdownTokenKind,
    runs: &mut Vec<MarkdownRun>,
    source: &str,
    retain_text: bool,
) {
    let Some(position) = position else {
        return;
    };
    runs.push(MarkdownRun {
        source_range: position.start.offset..position.end.offset,
        content_range: visible_content_range(
            source,
            position.start.offset..position.end.offset,
            kind,
        ),
        text: if retain_text {
            text.to_owned().into()
        } else {
            "".into()
        },
        bold: style.bold,
        italic: style.italic,
        strike: style.strike,
        code: style.code,
        break_kind: None,
        kind,
    });
}

/// Оставляет часть Markdown run, которая попадает в видимый участок исходника.
fn clip_run(run: &MarkdownRun, range: &Range<usize>, source: &str) -> Option<MarkdownRun> {
    if run.break_kind.is_some() {
        if range.start >= run.source_range.end || range.end <= run.source_range.start {
            return None;
        }
        return Some(run.clone());
    }
    if run.kind == MarkdownTokenKind::Synthetic {
        return (range.start <= run.source_range.start && run.source_range.start < range.end)
            .then(|| run.clone());
    }
    let start = run.content_range.start.max(range.start);
    let end = run.content_range.end.min(range.end);
    if start >= end {
        return None;
    }
    let raw = source.get(start..end)?;
    let display = match run.kind {
        MarkdownTokenKind::Plain | MarkdownTokenKind::ImageAlt => decode_markdown_text(raw),
        MarkdownTokenKind::InlineCode => normalize_code_span(raw),
        MarkdownTokenKind::CodeBlock => raw.replace("\r\n", "\n"),
        MarkdownTokenKind::IndentedCode => normalize_indented_code(raw),
        MarkdownTokenKind::Html => raw.to_owned(),
        MarkdownTokenKind::Break | MarkdownTokenKind::Paragraph | MarkdownTokenKind::Synthetic => {
            String::new()
        }
    };
    Some(MarkdownRun {
        source_range: start..end,
        content_range: start..end,
        text: display.into(),
        ..run.clone()
    })
}

/// Преобразует именованную или числовую ссылку по таблице CommonMark.
fn decode_entity(source: &str) -> Option<(usize, String)> {
    let end = source.find(';')?;
    let entity = source.get(1..end)?;
    let decoded = if let Some(hex) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        markdown::decode_numeric(hex, 16)
    } else if let Some(decimal) = entity.strip_prefix('#') {
        markdown::decode_numeric(decimal, 10)
    } else {
        markdown::decode_named(entity, true)?
    };
    Some((end + 1, decoded))
}

#[derive(Clone, Copy, Default)]
struct MarkdownStyle {
    bold: bool,   // Сильное выделение включено.
    italic: bool, // Наклонное выделение включено.
    strike: bool, // Перечёркивание включено.
    code: bool,   // Моноширинный стиль включён.
}

/// Собирает диапазоны стилей из исходных позиций узлов дерева Markdown.
fn collect_formats(node: &Node, style: MarkdownStyle, formats: &mut Vec<MarkdownFormatSpan>) {
    match node {
        Node::Text(value) => push_format(value.position.as_ref(), style, formats),
        Node::InlineCode(value) => {
            let mut nested = style;
            nested.code = true;
            push_format(value.position.as_ref(), nested, formats);
        }
        Node::Code(value) => {
            let mut nested = style;
            nested.code = true;
            push_format(value.position.as_ref(), nested, formats);
        }
        Node::Strong(value) => {
            let mut nested = style;
            nested.bold = true;
            collect_format_children(&value.children, nested, formats);
        }
        Node::Emphasis(value) => {
            let mut nested = style;
            nested.italic = true;
            collect_format_children(&value.children, nested, formats);
        }
        Node::Delete(value) => {
            let mut nested = style;
            nested.strike = true;
            collect_format_children(&value.children, nested, formats);
        }
        Node::Link(value) => collect_format_children(&value.children, style, formats),
        Node::Image(value) => push_format(value.position.as_ref(), style, formats),
        Node::Root(value) => collect_format_children(&value.children, style, formats),
        Node::Paragraph(value) => collect_format_children(&value.children, style, formats),
        Node::Heading(value) => {
            let mut heading = style;
            heading.bold = true;
            collect_format_children(&value.children, heading, formats);
        }
        Node::Blockquote(value) => collect_format_children(&value.children, style, formats),
        Node::List(value) => collect_format_children(&value.children, style, formats),
        Node::ListItem(value) => collect_format_children(&value.children, style, formats),
        Node::Table(value) => collect_format_children(&value.children, style, formats),
        Node::TableRow(value) => collect_format_children(&value.children, style, formats),
        Node::TableCell(value) => collect_format_children(&value.children, style, formats),
        _ => {}
    }
}

/// Добавляет стиль только при его наличии, сохраняя точный исходный диапазон узла.
fn push_format(
    position: Option<&markdown::unist::Position>,
    style: MarkdownStyle,
    formats: &mut Vec<MarkdownFormatSpan>,
) {
    let Some(position) = position else {
        return;
    };
    if !(style.bold || style.italic || style.strike || style.code) {
        return;
    }
    formats.push(MarkdownFormatSpan {
        range: position.start.offset..position.end.offset,
        bold: style.bold,
        italic: style.italic,
        strike: style.strike,
        code: style.code,
    });
}

/// Передаёт вложенным элементам действующий стиль их родителя.
fn collect_format_children(
    children: &[Node],
    style: MarkdownStyle,
    formats: &mut Vec<MarkdownFormatSpan>,
) {
    for child in children {
        collect_formats(child, style, formats);
    }
}

/// Данные тела, которыми могут пользоваться несколько проходов layout и paint.
#[derive(Eq, PartialEq)]
pub(super) struct Body {
    pub text: SharedString,        // Исходный обычный текст, нужен для выделения.
    pub parts: Vec<BodyPart>,      // Разобранный текст и ссылки на цитаты.
    excerpt: Option<SharedString>, // Короткий текст, если в исходном сообщении он есть.
    pub caption: Option<MediaCaption>, // Подпись после извлечения карточек, если у строки есть медиа.
    pub has_fragments: bool, // Нужны спойлеры или скрытие служебного адреса из исходного текста.
    pub markdown: MarkdownDocument, // Единая карта стилей и диапазонов исходного Markdown.
}

/// Подготовка подписи к карточкам, независимая от геометрии и загрузки файла.
#[derive(Eq, PartialEq)]
pub(super) struct MediaCaption {
    pub text: SharedString, // Подпись без ссылок, представленных отдельными карточками.
    pub parts: Vec<BodyPart>, // Текст и цитаты подписи.
    pub spans: Vec<CaptionSpan>, // Фрагменты подписи и их исходные байтовые границы.
    pub markdown: MarkdownDocument, // Карта стилей с координатами именно подписи.
}

/// Отдельный выделяемый участок открытой подписи рядом с медиа.
#[derive(Eq, PartialEq)]
pub(super) struct CaptionSpan {
    pub range: Range<usize>, // Байты участка в исходном Markdown, который проверит Core.
    pub text: SharedString,  // Исходный текст участка без окружающих отступов.
    pub inline: Option<InlineText>, // Подготовленные ссылки участка, если они есть.
}

/// Подготовленная часть обычного тела. Источник цитаты проверяется по текущему окну при показе.
#[derive(Eq, PartialEq)]
pub(super) enum BodyPart {
    Text {
        text: SharedString,  // Видимый исходный участок для GPUI.
        range: Range<usize>, // Байтовый диапазон участка в общем Markdown тела.
    },
    Inline {
        text: InlineText,    // Та же строка с подготовленными ссылочными участками.
        range: Range<usize>, // Байтовый диапазон строки в текущем владельце Markdown.
    },
    Preview(MessagePreview), // Открытый снимок пересылки с переводимыми служебными частями.
    Fragment {
        slot: usize,                          // Порядковое место спойлера среди вложений строки.
        reference: Option<FragmentReference>, // Точная ссылка либо закрытая ошибка формата.
        range: Range<usize>,                  // Байты ссылки, которые этот слот заменяет.
    },
    Forward {
        reference: Option<ForwardBody>, // Снимок пересылки или закрытая ошибка формата.
        range: Range<usize>,            // Байты ссылки пересылки в текущем Markdown.
    },
    Quote {
        id: Uuid,                       // Публичный UUID исходного сообщения.
        author: SharedString,           // Подпись, если источник отсутствует.
        fragment: Option<SharedString>, // Зафиксированный фрагмент, если был выбран.
    },
}

/// Части вложенной карточки без открытого текста её зашифрованных фрагментов.
#[derive(Eq, PartialEq)]
pub(super) struct ForwardBody {
    pub id: Uuid,                      // Публичный UUID исходного сообщения для перехода.
    pub author: SharedString,          // Подпись автора на исходной ссылке.
    pub source: Option<ForwardSource>, // Исходный чат и время, если были сохранены.
    pub parts: Vec<BodyPart>,          // Разобранный исходный Markdown без конвертов в тексте.
    pub markdown: MarkdownDocument,    // Карта стилей с координатами именно пересланного тела.
}

impl Body {
    /// Проверяет, что ссылка на тот же оригинал ещё находится в подтверждённом теле.
    pub(super) fn contains_forward(&self, source: Uuid) -> bool {
        forward_at(&self.parts, source)
    }

    /// Проверяет, осталась ли ссылка в актуальном теле или во вложенной пересылке.
    pub(super) fn contains_reference(&self, target: &WorkspaceReferenceTarget) -> bool {
        reference_at(&self.parts, target)
            || self
                .caption
                .as_ref()
                .is_some_and(|caption| reference_at(&caption.parts, target))
    }

    /// Собирает тело и вырезает ссылки на самостоятельные карточки только при смене снимка.
    pub(super) fn new(text: &str, media: &[MediaReference]) -> Self {
        let links = open_references(text);
        let opaque_ranges = fragment_display_spans(text)
            .into_iter()
            .map(|span| span.range)
            .chain(forward_spans(text).into_iter().map(|span| span.range))
            .collect::<Vec<_>>();
        let markdown = MarkdownDocument::new(text, &opaque_ranges);
        let caption = (!media.is_empty()).then(|| MediaCaption::new(text, media, &links));
        let text: SharedString = text.to_owned().into();
        let (parts, has_fragments) = parse(&text, 0, &mut 0, &links);
        let excerpt = parts.iter().find_map(|part| match part {
            BodyPart::Text { text: line, .. } if !line.trim().is_empty() => {
                Some(short_reply_text(line).into())
            }
            _ => None,
        });
        Self {
            text,
            parts,
            excerpt,
            caption,
            has_fragments,
            markdown,
        }
    }

    /// Показывает превью источника, подставляя подпись на языке открытого окна.
    pub(super) fn excerpt(&self, language: Language) -> SharedString {
        for part in &self.parts {
            match part {
                BodyPart::Text { text: line, .. } if !line.trim().is_empty() => break,
                BodyPart::Inline { text, .. } => {
                    return short_reply_text(&text.visible(language)).into();
                }
                _ => {}
            }
        }
        self.excerpt.clone().unwrap_or_else(|| {
            language
                .tr(
                    if self
                        .parts
                        .iter()
                        .any(|part| matches!(part, BodyPart::Forward { .. }))
                    {
                        "message.forwarded"
                    } else if self.has_fragments {
                        "e2ee.secretFragment"
                    } else {
                        "composer.quote"
                    },
                )
                .into()
        })
    }

    /// Узнаёт текст без ссылок на отдельные цитаты, в том числе при наличии карточек.
    pub(super) fn plain(&self) -> bool {
        !self.has_fragments && matches!(self.parts.as_slice(), [BodyPart::Text { .. }])
    }

    /// Узнаёт открытую подпись, пригодную для точного выделения фрагментов.
    pub(super) fn selectable_caption(&self) -> bool {
        !self.has_fragments
            && matches!(
                self.parts.as_slice(),
                [BodyPart::Text { .. } | BodyPart::Inline { .. }]
            )
    }

    /// Возвращает именно тот фрагмент, который может быть смонтирован для этой строки.
    pub(super) fn fragment(&self, position: usize) -> Option<&FragmentReference> {
        let parts = self
            .caption
            .as_ref()
            .map_or(&self.parts, |caption| &caption.parts);
        fragment_at(parts, position)
    }
}

impl MediaCaption {
    /// Собирает подпись и участки точного выделения за один проход по ссылкам.
    fn new(body: &str, media: &[MediaReference], links: &[Reference]) -> Self {
        let mut text = String::new();
        let mut spans = Vec::new();
        let mut shifted = Vec::new();
        let mut previous = 0;
        let ranges = media
            .iter()
            .map(|reference| reference.range.clone())
            .chain(std::iter::once(body.len()..body.len()));
        for range in ranges {
            let start = previous;
            previous = range.end;
            let Some(part) = body.get(start..range.start) else {
                continue;
            };
            shifted.extend(
                links
                    .iter()
                    .filter_map(|link| link.shift(start..range.start, text.len())),
            );
            text.push_str(part);
            let left = part.trim_start();
            let visible = left.trim_end();
            let start = start + part.len() - left.len();
            if !visible.is_empty() {
                let text = SharedString::from(visible.to_owned());
                spans.push(CaptionSpan {
                    range: start..start + text.len(),
                    inline: InlineText::new(text.clone(), start, links),
                    text,
                });
            }
        }
        let text: SharedString = text.into();
        let markdown = markdown_document(&text, &shifted);
        Self {
            parts: parse(&text, 0, &mut 0, &shifted).0,
            text,
            spans,
            markdown,
        }
    }
}

/// Сохраняет готовые отображаемые фрагменты и пересылки, один раз при чтении сообщения.
fn parse(
    markdown: &SharedString,
    depth: usize,
    slot: &mut usize,
    links: &[Reference],
) -> (Vec<BodyPart>, bool) {
    let fragments = fragment_display_spans(markdown);
    let mut forwards = forward_spans(markdown);
    // У обычной встроенной ссылки есть переход к оригиналу, но нет карточки со снимком.
    forwards.retain(|span| {
        span.reference.is_some()
            || !links.iter().any(|link| {
                span.range == link.range
                    && matches!(link.target, Some(WorkspaceReferenceTarget::Message(_)))
            })
    });
    let mut parts = Vec::new();
    for block in quote::blocks(markdown) {
        match block {
            Block::Text(text)
                if fragments.is_empty() && forwards.is_empty() && text == markdown.as_ref() =>
            {
                parts.push(inline_or_text(markdown.clone(), 0, links));
            }
            Block::Text(text) => {
                // Части цитаты ссылаются на тот же Markdown, что и позиции ссылок.
                let start = text.as_ptr().addr() - markdown.as_ref().as_ptr().addr();
                parse_placements(
                    text, start, &fragments, &forwards, links, &mut parts, depth, slot,
                );
            }
            Block::Quote(reference) => parts.push(BodyPart::Quote {
                id: reference.message_id,
                author: reference.author.into(),
                fragment: reference.fragment.map(Into::into),
            }),
        }
    }
    (parts, !fragments.is_empty() || !forwards.is_empty())
}

/// Подходящая ссылка в исходном тексте. Совпадающие вложения заменяет внешний блок.
enum Placement<'a> {
    Fragment(&'a FragmentDisplaySpan), // Интервал спойлера или определения ссылки.
    Forward(&'a ForwardSpan),          // Интервал снимка пересылки.
}

/// Ставит структурные блоки по порядку, оставляя открытые куски отдельным текстом.
#[allow(clippy::too_many_arguments)]
fn parse_placements(
    text: &str,
    start: usize,
    fragments: &[FragmentDisplaySpan],
    forwards: &[ForwardSpan],
    links: &[Reference],
    parts: &mut Vec<BodyPart>,
    depth: usize,
    slot: &mut usize,
) {
    let mut placements: Vec<_> = fragments
        .iter()
        .map(|span| (&span.range, Placement::Fragment(span)))
        .chain(
            forwards
                .iter()
                .map(|span| (&span.range, Placement::Forward(span))),
        )
        .filter(|(range, _)| range.start >= start && range.end <= start + text.len())
        .collect();
    placements.sort_by_key(|(range, _)| (range.start, std::cmp::Reverse(range.end)));
    let mut cursor = 0;
    for (range, placement) in placements {
        let range = (range.start - start)..(range.end - start);
        if range.start < cursor {
            continue;
        }
        if range.start > cursor {
            let between = &text[cursor..range.start];
            // Последовательные карточки уже разделены отступом потока.
            let card_separator = between.trim().is_empty()
                && matches!(parts.last(), Some(BodyPart::Forward { .. }))
                && matches!(placement, Placement::Forward(_));
            if !card_separator {
                parts.push(inline_or_text(
                    between.to_owned().into(),
                    start + cursor,
                    links,
                ));
            }
        }
        match placement {
            Placement::Fragment(span) if span.kind != FragmentDisplayKind::Definition => {
                let reference = match &span.kind {
                    FragmentDisplayKind::Reference(reference) => Some(reference.clone()),
                    _ => None,
                };
                parts.push(BodyPart::Fragment {
                    slot: *slot,
                    reference,
                    range: span.range.clone(),
                });
                *slot += 1;
            }
            Placement::Fragment(_) => {}
            Placement::Forward(span) => {
                let reference = span
                    .reference
                    .as_ref()
                    .filter(|_| depth < MAX_FORWARD_DEPTH)
                    .map(|reference| forward_body(reference, depth, slot));
                parts.push(BodyPart::Forward {
                    reference,
                    range: span.range.clone(),
                });
            }
        }
        cursor = range.end;
    }
    if cursor < text.len() || (text.is_empty() && fragments.is_empty() && forwards.is_empty()) {
        parts.push(inline_or_text(
            text[cursor..].to_owned().into(),
            start + cursor,
            links,
        ));
    }
}

/// Оставляет обычный текст на быстром пути виртуального списка.
fn inline_or_text(source: SharedString, start: usize, links: &[Reference]) -> BodyPart {
    let range = start..start + source.len();
    InlineText::new(source.clone(), start, links).map_or_else(
        || BodyPart::Text {
            range: range.clone(),
            text: source,
        },
        |text| BodyPart::Inline {
            text,
            range: range.clone(),
        },
    )
}

/// Собирает карту стилей, не разбирая содержимое ссылок и custom элементов как Markdown.
fn markdown_document(source: &str, _links: &[Reference]) -> MarkdownDocument {
    let opaque_ranges = fragment_display_spans(source)
        .into_iter()
        .map(|span| span.range)
        .chain(forward_spans(source).into_iter().map(|span| span.range))
        .collect::<Vec<_>>();
    MarkdownDocument::new(source, &opaque_ranges)
}

/// Ищет только ссылки, которые представлены пользователю как действия в текущем теле.
fn reference_at(parts: &[BodyPart], target: &WorkspaceReferenceTarget) -> bool {
    parts.iter().any(|part| match part {
        BodyPart::Inline { text, .. } => text.contains(target),
        BodyPart::Forward {
            reference: Some(forward),
            ..
        } => reference_at(&forward.parts, target),
        _ => false,
    })
}

/// Разбирает подписанный снимок, не перенося его служебный URL в отображаемое тело.
fn forward_body(reference: &ForwardReference, depth: usize, slot: &mut usize) -> ForwardBody {
    let links = open_references(&reference.snapshot);
    let markdown = markdown_document(&reference.snapshot, &links);
    let parts = if reference.plain {
        vec![BodyPart::Preview(MessagePreview::plain_forward(
            &reference.snapshot,
        ))]
    } else {
        parse(&reference.snapshot.clone().into(), depth + 1, slot, &links).0
    };
    ForwardBody {
        id: reference.id,
        author: reference.author.clone().into(),
        source: reference.source.as_ref().map(|source| ForwardSource {
            label: source.label.clone(),
            direct: source.direct,
            created_at: source.created_at.clone(),
        }),
        parts,
        markdown,
    }
}

/// Ищет оригинал в основной карточке и во вложенных пересылках.
fn forward_at(parts: &[BodyPart], source: Uuid) -> bool {
    parts.iter().any(|part| match part {
        BodyPart::Forward {
            reference: Some(reference),
            ..
        } => reference.id == source || forward_at(&reference.parts, source),
        _ => false,
    })
}

/// Находит тот же слот в основном теле или во вложенной карточке пересылки.
fn fragment_at(parts: &[BodyPart], position: usize) -> Option<&FragmentReference> {
    parts.iter().find_map(|part| match part {
        BodyPart::Fragment {
            slot, reference, ..
        } if *slot == position => reference.as_ref(),
        BodyPart::Forward {
            reference: Some(reference),
            ..
        } => fragment_at(&reference.parts, position),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Встроенная пересылка ведёт к оригиналу и оставляет снимок вне текста.
    #[test]
    fn inline_forward_is_a_link_without_embedded_snapshot() {
        let original = Uuid::from_u128(42);
        let wire = format!("До [Оригинал](urn:forward:{original}?snapshot=U2VjcmV0) после");
        let body = Body::new(&wire, &[]);
        let [BodyPart::Inline { text, .. }] = body.parts.as_slice() else {
            panic!("Встроенная ссылка должна остаться в строке с соседним текстом");
        };
        assert!(body.contains_reference(&WorkspaceReferenceTarget::Message(original)));
        assert_eq!(text.visible(Language::Ru), "До Оригинал после");
    }

    /// Переводит служебный текст буквальной пересылки на языке получателя.
    #[test]
    fn plain_forward_snapshot_retains_translatable_parts() {
        let body = forward_body(
            &ForwardReference {
                id: Uuid::from_u128(1),
                author: "Автор".into(),
                snapshot: "Зашифрованный фрагмент [ключ](urn:secret:v9:bad)".into(),
                plain: true,
                source: None,
            },
            0,
            &mut 0,
        );
        let [BodyPart::Preview(preview)] = body.parts.as_slice() else {
            panic!("Буквальная пересылка должна использовать готовые части превью");
        };
        assert_eq!(
            Language::En.preview_text(preview),
            "Зашифрованный фрагмент Encrypted fragment"
        );
        assert_eq!(
            Language::Ru.preview_text(preview),
            "Зашифрованный фрагмент Зашифрованный фрагмент"
        );
    }

    /// Использует текущий язык для пустого фрагмента одного и того же сообщения.
    #[test]
    fn quote_excerpt_uses_the_render_language() {
        let body = Body::new("", &[]);
        assert_eq!(body.excerpt(Language::Ru).as_ref(), "Цитата");
        assert_eq!(body.excerpt(Language::En).as_ref(), "Quote");
    }

    /// Разбирает отображаемые фрагменты отдельно от их адресов и от закрытой ошибки.
    #[test]
    fn body_keeps_public_text_without_playing_secret_urls_as_text() {
        let link = "urn:secret:v1:956f084e-8992-4334-a6de-fe84ac055e46:e70f0d95-6784-4125-8dfd-83eac755cecf:AQID";
        let markdown =
            format!("До [неизвестный](urn:secret:v2:bad) и [готовый][x] после\n\n[x]: {link}");
        let body = Body::new(&markdown, &[]);
        assert!(body.has_fragments && !body.plain());
        assert_eq!(body.text.as_ref(), markdown);
        assert!(body.parts.iter().any(|part| matches!(
            part,
            BodyPart::Fragment {
                reference: None,
                ..
            }
        )));
        assert!(body.parts.iter().any(|part| matches!(
            part,
            BodyPart::Fragment {
                reference: Some(_),
                ..
            }
        )));
        assert!(body.parts.iter().all(|part| match part {
            BodyPart::Text { text, .. } => !text.contains("urn:secret") && !text.contains("[x]"),
            _ => true,
        }));
    }

    /// Сохраняет границы абзацев и отображает форматированный текст без Markdown-разделителей.
    #[test]
    fn markdown_document_keeps_ranges_across_paragraphs_and_inline_code() {
        let source = "Первый **жирный**\n\nВторой `код & текст` и &amp;";
        let document = MarkdownDocument::new(source, &[]);
        let runs = document.visible_runs_in(0..source.len());
        let visible = runs.iter().map(|run| run.text.as_ref()).collect::<Vec<_>>();

        assert_eq!(
            visible,
            ["Первый ", "жирный", "", "Второй ", "код & текст", " и &"]
        );
        assert!(runs[1].bold);
        assert!(runs[2].break_kind == Some(MarkdownBreak::Paragraph));
        assert!(runs[4].code);
        assert_eq!(&source[runs[1].source_range.clone()], "жирный");
    }

    /// Сохраняет Unicode и раскрывает entity по обе стороны скрытой ссылки в общем выделении.
    #[test]
    fn markdown_document_clips_opaque_ranges_without_leaking_them() {
        let secret = "[секрет](urn:secret:v1:закрыто)";
        let source = format!("**до &amp; {secret} после**");
        let start = source.find(secret).unwrap();
        let opaque = start..start + secret.len();
        let document = MarkdownDocument::new(&source, std::slice::from_ref(&opaque));
        let before = document.visible_runs_in(0..opaque.start);
        let after = document.visible_runs_in(opaque.end..source.len());

        assert_eq!(
            before
                .iter()
                .map(|run| run.text.as_ref())
                .collect::<String>(),
            "до & "
        );
        assert_eq!(
            after
                .iter()
                .map(|run| run.text.as_ref())
                .collect::<String>(),
            " после"
        );
        assert!(before.iter().all(|run| run.bold));
        assert!(after.iter().all(|run| run.bold));
        assert!(
            before
                .iter()
                .chain(&after)
                .all(|run| !run.text.contains("urn:secret"))
        );
    }

    /// Связывает видимую подпись изображения и inline code только с байтами их содержимого.
    #[test]
    fn markdown_runs_map_image_alt_and_code_content_without_delimiters() {
        let source = "![alt [nested] &amp; \\] end](image.png) and `code &amp;`";
        let document = MarkdownDocument::new(source, &[]);
        let runs = document.visible_runs_in(0..source.len());
        let image = runs
            .iter()
            .find(|run| run.kind == MarkdownTokenKind::ImageAlt)
            .expect("image alt run");
        let code = runs
            .iter()
            .find(|run| run.kind == MarkdownTokenKind::InlineCode)
            .expect("inline code run");

        assert_eq!(image.text.as_ref(), "alt [nested] & ] end");
        assert_eq!(
            &source[image.source_range.clone()],
            "alt [nested] &amp; \\] end"
        );
        assert_eq!(code.text.as_ref(), "code &amp;");
        assert_eq!(&source[code.source_range.clone()], "code &amp;");
    }

    /// Не отбрасывает первую строку indented code и снимает только обязательный отступ.
    #[test]
    fn markdown_document_preserves_indented_code_lines() {
        let source = "    first\n    second";
        let document = MarkdownDocument::new(source, &[]);
        let runs = document.visible_runs_in(0..source.len());
        let code = runs
            .iter()
            .find(|run| run.kind == MarkdownTokenKind::IndentedCode)
            .expect("indented code run");

        assert_eq!(code.text.as_ref(), "first\nsecond");
        assert_eq!(&source[code.source_range.clone()], source);

        let fenced_source = "```rust\nfirst\nsecond\n```";
        let fenced = MarkdownDocument::new(fenced_source, &[]);
        let run = fenced
            .visible_runs_in(0..fenced_source.len())
            .into_iter()
            .find(|run| run.kind == MarkdownTokenKind::CodeBlock)
            .expect("fenced code run");
        assert_eq!(run.text.as_ref(), "first\nsecond");
    }

    /// Разбирает форматирование подписи custom-ссылки, сохраняя ссылку как отдельное действие.
    #[test]
    fn markdown_document_keeps_formatting_inside_reference_label() {
        let source = "[**bold** plain](urn:time:2026-10-13T10:02:00Z)";
        let links = open_references(source);
        let document = markdown_document(source, &links);
        let label_range = links[0].label_range.clone().expect("reference label");

        let formatted = document
            .formats_in(label_range.clone())
            .filter(|format| format.bold)
            .collect::<Vec<_>>();

        assert!(!formatted.is_empty());
        assert!(formatted.iter().all(|format| {
            format.range.start >= label_range.start && format.range.end <= label_range.end
        }));
    }

    /// Оставляет содержимое вложенных блочных элементов разделённым переносом.
    #[test]
    fn markdown_document_keeps_blockquote_and_table_boundaries() {
        let source = "> first\n>\n> second\n\n- item first\n\n  item second\n\n| A | B |\n| - | - |\n| C | D |";
        let document = MarkdownDocument::new(source, &[]);
        let visible = document
            .visible_runs_in(0..source.len())
            .iter()
            .map(|run| match run.break_kind {
                Some(MarkdownBreak::Paragraph) => "\n\n".to_owned(),
                Some(MarkdownBreak::Hard | MarkdownBreak::Soft) => "\n".to_owned(),
                None => run.text.to_string(),
            })
            .collect::<String>();

        assert!(visible.contains("first\n\nsecond"), "{visible:?}");
        assert!(visible.contains("item first\n\nitem second"), "{visible:?}");
        assert!(visible.contains("A | B\nC | D"), "{visible:?}");
    }

    /// Не теряет заголовок и горизонтальную линию при обычном разборе Markdown.
    #[test]
    fn markdown_document_renders_heading_and_thematic_break() {
        let source = "# Заголовок\n\n---\n\nТекст";
        let document = MarkdownDocument::new(source, &[]);
        let runs = document.visible_runs_in(0..source.len());

        assert!(
            runs.iter()
                .any(|run| run.text.as_ref() == "Заголовок" && run.bold)
        );
        assert!(runs.iter().any(|run| run.text.as_ref() == "──────────"));
        assert!(runs.iter().any(|run| run.text.as_ref() == "Текст"));
    }
}
