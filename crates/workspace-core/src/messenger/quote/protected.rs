//! Убирает копии исходного текста из защищённого Markdown и хранит ссылки для проверки оригиналов.

use std::{borrow::Cow, ops::Range};

use markdown::{ParseOptions, mdast::Node, to_mdast};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{quote_lines, reference};

/// Отдельные структурные ссылки. Встроенный фрагмент никогда не выходит из Core.
#[derive(Default)]
pub(in crate::messenger) struct ProtectedReferences(Vec<ProtectedReference>); // Порядок ссылок в безопасном теле.

/// Структурный источник, перенесённый из старого шифротекста.
struct ProtectedReference {
    source: Uuid,                              // Публичный UUID для авторитетного GET.
    legacy_excerpt: Option<Zeroizing<String>>, // Проверяемая подсказка, а не тело оригинала.
    whole: bool, // Источник точно просили целиком, поэтому доступны его вложения.
}

impl ProtectedReferences {
    /// Подтверждает ссылку на этой позиции и возвращает возможную старую подсказку внутри Core.
    pub(in crate::messenger) fn excerpt(
        &self,
        source: Uuid,
        position: usize,
    ) -> Option<Option<&str>> {
        let reference = self
            .0
            .get(position)
            .filter(|reference| reference.source == source)?;
        Some(reference.legacy_excerpt.as_deref().map(String::as_str))
    }

    /// Проверяет допуск к вложениям только на указанном месте этой цитаты.
    pub(in crate::messenger) fn whole_at(&self, source: Uuid, position: usize) -> bool {
        self.0
            .get(position)
            .is_some_and(|reference| reference.source == source && reference.whole)
    }
}

/// Замена структурного поля и служебное положение цитаты после удаления параметров.
struct Replacement {
    range: Range<usize>,           // Исходный URI либо целая старая цитата-снимок.
    markdown: String,              // Каноническая ссылка без аргументов.
    reference: ProtectedReference, // UUID и скрытый исходный параметр.
}

/// Даёт одинаковый проецируемый Markdown редактору, истории, буферу обмена и отправке.
pub(in crate::messenger) fn project_protected(
    markdown: &str,
) -> (Cow<'_, str>, ProtectedReferences) {
    if !(has_ascii(markdown, b"urn:quote:")
        || has_ascii(markdown, b"urn:message:")
        || markdown.contains("&#"))
    {
        return (Cow::Borrowed(markdown), ProtectedReferences::default());
    }
    let mut replacements = Vec::new();
    let mut literals = Vec::new();
    if let Ok(document) = to_mdast(markdown, &ParseOptions::gfm()) {
        let mut pending = vec![&document];
        while let Some(node) = pending.pop() {
            match node {
                Node::Code(_) | Node::InlineCode(_) | Node::Html(_) => {
                    literals.extend(node_range(node));
                }
                _ => {
                    if let Some(replacement) = candidate(markdown, node) {
                        replacements.push(replacement);
                    } else {
                        pending.extend(node.children().into_iter().flatten().rev());
                    }
                }
            }
        }
    }
    // Старый редактор принимал параметры с пробелами и не требовал CommonMark-разбора.
    // Поддерживаем этот формат на отдельной строке, оставляя литеральный код без изменений.
    let mut offset = 0;
    for raw in markdown.split_inclusive('\n') {
        let line = raw
            .trim_end_matches(['\r', '\n'])
            .trim_end_matches([' ', '\t']);
        if let Some((label, destination)) = line
            .strip_suffix(')')
            .and_then(|line| line.rsplit_once("]("))
            && label.starts_with('[')
            && let Some(parsed) = parse_uri(destination)
        {
            let start = offset + label.len() + 2;
            let end = offset + line.len() - 1;
            if !literals
                .iter()
                .chain(replacements.iter().map(|replace| &replace.range))
                .any(|range| range.start < end && start < range.end)
            {
                replacements.push(uri_replacement(start..end, parsed));
            }
        }
        offset += raw.len();
    }
    if replacements.is_empty() {
        return (Cow::Borrowed(markdown), ProtectedReferences::default());
    }
    replacements.sort_unstable_by_key(|replace| replace.range.start);
    let changed = replacements
        .iter()
        .any(|replace| markdown.get(replace.range.clone()) != Some(replace.markdown.as_str()));
    let mut safe = changed.then(|| String::with_capacity(markdown.len()));
    let mut copied = 0;
    let mut positions = Vec::new();
    for replacement in replacements {
        if replacement.range.start < copied {
            continue;
        }
        let projected_start = if let Some(safe) = &mut safe {
            safe.push_str(&markdown[copied..replacement.range.start]);
            let start = safe.len();
            safe.push_str(&replacement.markdown);
            start
        } else {
            replacement.range.start
        };
        positions.push((projected_start, replacement.reference));
        copied = replacement.range.end;
    }
    let safe = safe.map_or(Cow::Borrowed(markdown), |mut safe| {
        safe.push_str(&markdown[copied..]);
        Cow::Owned(safe)
    });
    let mut positions = positions.into_iter().peekable();
    let references = quote_lines(&safe)
        .map(|(line, reference)| {
            while positions
                .peek()
                .is_some_and(|(start, _)| *start < line.start)
            {
                positions.next();
            }
            if positions.peek().is_some_and(|(start, item)| {
                line.contains(start) && item.source == reference.message_id
            }) {
                return positions.next().unwrap().1;
            }
            ProtectedReference {
                source: reference.message_id,
                legacy_excerpt: None,
                whole: true,
            }
        })
        .collect();
    (safe, ProtectedReferences(references))
}

/// Переносит уже защищённую строку без копии или заменяет её очищенным телом.
pub(in crate::messenger) fn project_owned(
    markdown: Zeroizing<String>,
) -> (Zeroizing<String>, ProtectedReferences) {
    let (projected, references) = project_protected(&markdown);
    let safe = match projected {
        Cow::Borrowed(_) => markdown,
        Cow::Owned(text) => Zeroizing::new(text),
    };
    (safe, references)
}

/// Возвращает исходный диапазон узла Markdown.
fn node_range(node: &Node) -> Option<Range<usize>> {
    let pos = node.position()?;
    Some(pos.start.offset..pos.end.offset)
}

/// Использует разобранный Markdown для отличия настоящих ссылок от похожего текста и кода.
fn candidate(markdown: &str, node: &Node) -> Option<Replacement> {
    let range = node_range(node)?;
    let raw = markdown.get(range.clone())?;
    let url = match node {
        Node::Link(link) => &link.url,
        Node::Image(image) => &image.url,
        Node::Definition(definition) => &definition.url,
        Node::Blockquote(_) => {
            let source = legacy_quote_source(node)?;
            return Some(Replacement {
                range,
                markdown: reference(source, "Quote", None),
                reference: ProtectedReference {
                    source,
                    legacy_excerpt: None,
                    whole: false,
                },
            });
        }
        _ => return None,
    };
    let parsed = parse_uri(url)?;
    let destination = destination_range(raw, matches!(node, Node::Definition(_)))?;
    Some(uri_replacement(
        range.start + destination.start..range.start + destination.end,
        parsed,
    ))
}

/// Узнаёт устаревший ответ со встроенным блочным снимком по двум ссылкам в заголовке.
fn legacy_quote_source(node: &Node) -> Option<Uuid> {
    let Node::Paragraph(paragraph) = node.children()?.first()? else {
        return None;
    };
    let [
        Node::Link(author),
        Node::Text(space),
        Node::Link(message),
        Node::Text(separator),
        ..,
    ] = paragraph.children.as_slice()
    else {
        return None;
    };
    if !space.value.trim().is_empty() || !separator.value.starts_with(':') {
        return None;
    }
    exact_urn(&author.url, "urn:user:")?;
    exact_urn(&message.url, "urn:message:")
}

/// Возвращает только UUID из старого авторитетного заголовка.
fn exact_urn(url: &str, prefix: &str) -> Option<Uuid> {
    url.get(..prefix.len())?
        .eq_ignore_ascii_case(prefix)
        .then_some(())?;
    let value = url.get(prefix.len()..)?.parse::<Uuid>().ok()?;
    (!value.is_nil()).then_some(value)
}

/// Собирает замену URI, сохраняя подпись, пробелы, заголовок ссылки и переносы строк.
fn uri_replacement(range: Range<usize>, reference: ProtectedReference) -> Replacement {
    Replacement {
        range,
        markdown: format!("urn:quote:{}", reference.source),
        reference,
    }
}

/// Читает UUID из известного URN. Любые параметры, даже повреждённые, не идут в проекцию.
fn parse_uri(url: &str) -> Option<ProtectedReference> {
    let url = url.trim();
    let prefix = "urn:quote:";
    url.get(..prefix.len())?
        .eq_ignore_ascii_case(prefix)
        .then_some(())?;
    let end = prefix.len() + 36;
    let id = Uuid::parse_str(url.get(prefix.len()..end)?)
        .ok()
        .filter(|id| !id.is_nil())?;
    let suffix = url.get(end..)?;
    if !(suffix.is_empty() || suffix.starts_with(['?', '#'])) {
        return None;
    }
    let legacy_excerpt = suffix
        .strip_prefix("?text=")
        .filter(|text| !text.contains('&'))
        .and_then(decode_excerpt);
    Some(ProtectedReference {
        source: id,
        legacy_excerpt,
        whole: suffix.is_empty() || suffix == "?text=",
    })
}

/// Декодирует старый URI так же, как клиент, включая Unicode и неэкранированные символы.
fn decode_excerpt(text: &str) -> Option<Zeroizing<String>> {
    let mut bytes = Zeroizing::new(Vec::new());
    let mut input = text.bytes();
    while let Some(byte) = input.next() {
        let byte = if byte == b'%' {
            let high = char::from(input.next()?).to_digit(16)?;
            let low = char::from(input.next()?).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        };
        bytes.push(byte);
    }
    let decoded = std::str::from_utf8(&bytes).ok()?;
    (!decoded.is_empty()).then(|| Zeroizing::new(decoded.to_owned()))
}

/// Находит границы адреса в исходном синтаксисе ссылки с угловыми скобками, заголовком или определением.
fn destination_range(raw: &str, definition: bool) -> Option<Range<usize>> {
    let mut start = 0;
    if !raw.starts_with('<') {
        let open = usize::from(raw.starts_with('!'));
        if raw.as_bytes().get(open) != Some(&b'[') {
            return None;
        }
        let mut depth = 1;
        let mut escaped = false;
        for (index, byte) in raw.bytes().enumerate().skip(open + 1) {
            if escaped {
                escaped = false;
                continue;
            }
            match byte {
                b'\\' => escaped = true,
                b'[' => depth += 1,
                b']' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                start = index + 1;
                break;
            }
        }
        if raw.as_bytes().get(start) != Some(&if definition { b':' } else { b'(' }) {
            return None;
        }
        start += 1;
        start += raw[start..].len() - raw[start..].trim_start().len();
    }
    let remaining = &raw[start..];
    if let Some(inside) = remaining.strip_prefix('<') {
        return Some(start + 1..start + 1 + inside.find('>')?);
    }
    let mut nested = 0;
    let mut escaped = false;
    for (index, byte) in remaining.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte.is_ascii_whitespace() || (byte == b')' && nested == 0) {
            return Some(start..start + index);
        }
        match byte {
            b'\\' => escaped = true,
            b'(' => nested += 1,
            b')' => nested -= 1,
            _ => {}
        }
    }
    Some(start..raw.len())
}

/// Проверяет префикс без выделения второй копии сообщения.
fn has_ascii(markdown: &str, needle: &[u8]) -> bool {
    markdown
        .as_bytes()
        .windows(needle.len())
        .any(|part| part.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: Uuid = Uuid::from_u128(23);

    /// Убирает параметры с разных видов ссылок. Код, подписи и обычный Markdown остаются.
    #[test]
    fn normalizes_structural_legacy_uris_for_every_whole_message_reader() {
        let legacy = format!("[Legacy](URN:QUOTE:{SOURCE}?text=private%20excerpt)");
        let inline =
            format!("See [here](<urn:quote:{SOURCE}?unknown=private-metadata> \"title\") today");
        let definition = format!("[def]: urn:quote:{SOURCE}?text=definition-secret");
        let fallback = format!("[Fallback](urn:quote:{SOURCE}?text=literal private text)");
        let code = format!("`{legacy}`\n\n```text\n{legacy}\n```");
        let original = format!("{legacy}\n\n{inline}\n\n{definition}\n\n{fallback}\n\n{code}");
        let (safe, refs) = project_protected(&original);
        assert_eq!(
            safe,
            format!(
                "[Legacy](urn:quote:{SOURCE})\n\nSee [here](<urn:quote:{SOURCE}> \"title\") today\n\n[def]: urn:quote:{SOURCE}\n\n[Fallback](urn:quote:{SOURCE})\n\n{code}"
            )
        );
        assert_eq!(refs.excerpt(SOURCE, 0), Some(Some("private excerpt")));
        assert_eq!(refs.excerpt(SOURCE, 1), Some(Some("literal private text")));
        assert!(!refs.whole_at(SOURCE, 0));
        assert!(!refs.whole_at(SOURCE, 1));
    }

    /// Старый блочный снимок исчезает целиком, а авторская цитата Markdown сохраняется.
    #[test]
    fn removes_recognized_old_blockquote_snapshot_without_removing_authored_blockquote() {
        let author = Uuid::from_u128(99);
        let original = format!(
            "> [User](urn:user:{author}) [message](urn:message:{SOURCE}):\n> legacy snapshot\n> another line\n\n> authored quote\n\nBody"
        );
        let (safe, refs) = project_protected(&original);
        assert_eq!(
            safe,
            format!("[Quote](urn:quote:{SOURCE})\n\n> authored quote\n\nBody")
        );
        assert_eq!(refs.excerpt(SOURCE, 0), Some(None));
        assert!(!refs.whole_at(SOURCE, 0));
    }
}
