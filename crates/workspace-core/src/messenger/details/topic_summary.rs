//! Разбирает Markdown сводки в текстовые блоки с начертанием и вложенностью списка.

use std::ops::Range;

use markdown::{ParseOptions, mdast::Node, to_mdast};

const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_INDENT: u8 = 12;

/// Одновременное полужирное и курсивное начертание участка текста.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TopicSummaryStyle {
    pub bold: bool,   // Полужирный текст, например заголовок.
    pub italic: bool, // Курсив, может сочетаться с полужирным.
}

/// Абзац сводки или строка списка для нативного экрана.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TopicSummaryBlock {
    pub text: String, // Видимый текст с маркером строки, если он есть.
    pub styles: Vec<(Range<usize>, TopicSummaryStyle)>, // Байтовые диапазоны внутри text.
    pub indent: u8,   // Уровень вложенности списка или цитаты.
}

/// Разбирает обычную сводку без HTML и возвращает исходный текст для большого ввода.
pub fn parse_topic_summary(source: &str) -> Vec<TopicSummaryBlock> {
    if source.len() <= MAX_SOURCE_BYTES
        && let Ok(Node::Root(root)) = to_mdast(source, &ParseOptions::gfm())
    {
        let mut blocks = Vec::new();
        for node in &root.children {
            append_block(node, 0, String::new(), &mut blocks);
        }
        if !blocks.is_empty() {
            return blocks;
        }
    }
    vec![TopicSummaryBlock {
        text: source.to_owned(),
        ..TopicSummaryBlock::default()
    }]
}

/// Переводит каждый абзац и пункт списка в отдельный переносимый блок.
fn append_block(node: &Node, depth: u8, prefix: String, blocks: &mut Vec<TopicSummaryBlock>) {
    match node {
        Node::List(list) if depth < MAX_INDENT => {
            for (index, item) in list.children.iter().enumerate() {
                let Some(children) = item.children() else {
                    continue;
                };
                let marker = if list.ordered {
                    format!("{}. ", u64::from(list.start.unwrap_or(1)) + index as u64)
                } else {
                    "• ".to_owned()
                };
                for (line, child) in children.iter().enumerate() {
                    let indent = depth + u8::from(line > 0);
                    let prefix = if line == 0 {
                        marker.clone()
                    } else {
                        String::new()
                    };
                    append_block(child, indent, prefix, blocks);
                }
            }
        }
        Node::Blockquote(quote) if depth < MAX_INDENT => {
            for child in &quote.children {
                append_block(child, depth + 1, String::new(), blocks);
            }
        }
        _ => {
            let mut block = TopicSummaryBlock {
                text: prefix,
                indent: depth,
                ..TopicSummaryBlock::default()
            };
            append_inline(node, TopicSummaryStyle::default(), &mut block);
            if !block.text.is_empty() {
                blocks.push(block);
            }
        }
    }
}

/// Собирает непересекающиеся диапазоны начертания в байтовых позициях UTF-8.
fn append_inline(node: &Node, mut style: TopicSummaryStyle, block: &mut TopicSummaryBlock) {
    match node {
        Node::Strong(_) | Node::Heading(_) => style.bold = true,
        Node::Emphasis(_) => style.italic = true,
        _ => {}
    }
    if let Some(children) = node.children() {
        for child in children {
            append_inline(child, style, block);
        }
        return;
    }

    let start = block.text.len();
    match node {
        Node::Break(_) => block.text.push('\n'),
        Node::Image(image) => block.text.push_str(&image.alt),
        Node::ImageReference(image) => block.text.push_str(&image.alt),
        Node::Definition(_) | Node::Html(_) => {}
        _ => block.text.push_str(&node.to_string()),
    }
    if start < block.text.len() && style != TopicSummaryStyle::default() {
        block.styles.push((start..block.text.len(), style));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет маркер списка и диапазоны двух начертаний на русском тексте.
    #[test]
    fn summary_projects_markdown_into_native_blocks() {
        let blocks = parse_topic_summary("# Решения\n\n- **Принято *сегодня***\n- Следующий шаг");
        assert_eq!(blocks[0].text, "Решения");
        assert!(blocks[0].styles[0].1.bold);
        assert_eq!(blocks[1].text, "• Принято сегодня");
        assert_eq!(blocks[1].styles.len(), 2);
        let (range, style) = &blocks[1].styles[1];
        assert_eq!(&blocks[1].text[range.clone()], "сегодня");
        assert!(style.bold && style.italic);
        assert_eq!(blocks[2].text, "• Следующий шаг");
    }
}
