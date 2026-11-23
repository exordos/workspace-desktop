//! Находит файлы Workspace и веб-ссылки в открытом Markdown сообщений.
//! Карточки вложений и индекс доступа используют общий разбор файловых ссылок.

use std::ops::Range;

use markdown::{ParseOptions, mdast::Node, to_mdast};
use reqwest::Url;
use uuid::Uuid;

use crate::storage::media::{ExternalLink, MediaKind, MediaLink};

/// Вложение и сведения, которые можно взять из самого сообщения для показа карточки.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaReference {
    pub range: Range<usize>,  // Границы всей ссылки в байтах исходного Markdown.
    pub label: String,        // Текст Markdown-ссылки или alt изображения.
    pub name: Option<String>, // Имя из параметра URN, если оно пригодно для показа.
    pub file_id: Uuid,        // UUID, который Core сверяет с текущей ссылкой сообщения.
    pub kind: MediaKind,      // Вид, объявленный в URN.
    pub image_syntax: bool,   // true для изображения Markdown, false для обычной ссылки.
}

/// Выделяет подтверждаемые Core вложения в порядке чтения и сохраняет исходные границы.
pub fn media_references(text: &str) -> Vec<MediaReference> {
    if !text.contains("urn") {
        return Vec::new();
    }
    let Ok(document) = to_mdast(text, &ParseOptions::default()) else {
        return Vec::new();
    };
    let mut references = Vec::new();
    let mut pending = vec![&document];
    while let Some(node) = pending.pop() {
        let details = match node {
            Node::Blockquote(_) => continue,
            Node::Image(image) => Some((image.url.as_str(), true)),
            Node::Link(link)
                if link.url.starts_with("urn:quote:") || link.url.starts_with("urn:forward:") =>
            {
                continue;
            }
            Node::Link(link)
                if link.position.as_ref().is_some_and(|range| {
                    text.as_bytes().get(range.start.offset) == Some(&b'[')
                }) =>
            {
                Some((link.url.as_str(), false))
            }
            _ => None,
        };
        if let Some(reference) = details.and_then(|(url, image_syntax)| {
            let position = node.position()?;
            let range = position.start.offset..position.end.offset;
            text.get(range.clone())?;
            let (file_id, kind, name) = parse_media_url(url)?;
            Some(MediaReference {
                range,
                label: display_label(node),
                name,
                file_id,
                kind,
                image_syntax,
            })
        }) {
            // Если внешняя ссылка уже стала карточкой, её дочерние ссылки входят в тот же диапазон.
            references.push(reference);
        } else {
            pending.extend(node.children().into_iter().flatten().rev());
        }
    }
    references
}

/// Извлекает те же вложения для локального индекса доступа.
pub(crate) fn media_links(text: &str) -> Vec<MediaLink> {
    media_references(text)
        .into_iter()
        .map(|reference| MediaLink {
            file_id: reference.file_id,
            kind: reference.kind,
            name: reference
                .name
                .as_deref()
                .and_then(clean_label)
                .or_else(|| clean_label(&reference.label)),
        })
        .collect()
}

/// Берёт ссылки HTTP и HTTPS из открытого Markdown без запросов к сайтам и разбора цитат.
pub(crate) fn external_links(text: &str) -> Vec<ExternalLink> {
    if !text.contains("://") && !text.contains("www.") {
        return Vec::new();
    }
    let Ok(document) = to_mdast(text, &ParseOptions::gfm()) else {
        return Vec::new();
    };
    let mut links = Vec::new();
    let mut pending = vec![&document];
    while let Some(node) = pending.pop() {
        match node {
            Node::Blockquote(_) => {}
            Node::Link(link) => {
                if let Some(url) = external_url(&link.url) {
                    let label = clean_label(&display_label(node)).unwrap_or_else(|| url.clone());
                    links.push(ExternalLink { url, label });
                }
            }
            _ => pending.extend(node.children().into_iter().flatten().rev()),
        }
    }
    links
}

/// Проверяет явный веб-адрес, отбрасывая относительные пути и логин в адресе.
fn external_url(value: &str) -> Option<String> {
    if value
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
        || !value.split_once("://").is_some_and(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http")
        })
    {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.into())
}

/// Пригодная для показа короткая подпись без управляющих символов.
fn clean_label(text: &str) -> Option<String> {
    let label: String = text
        .chars()
        .filter(|ch| !ch.is_control())
        .take(256)
        .collect();
    (!label.trim().is_empty()).then_some(label)
}

/// Собирает читаемую подпись, оставляя Markdown-разметку в исходной строке.
pub(super) fn display_label(node: &Node) -> String {
    let mut label = String::new();
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node {
            Node::Image(image) => label.push_str(&image.alt),
            Node::Text(text) => label.push_str(&text.value),
            Node::InlineCode(code) => label.push_str(&code.value),
            Node::Break(_) => label.push(' '),
            _ => pending.extend(node.children().into_iter().flatten().rev()),
        }
    }
    label.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Разбирает только документированные файловые URN и проверяет формат UUID и параметров.
fn parse_media_url(url: &str) -> Option<(Uuid, MediaKind, Option<String>)> {
    let (kind, resource) = url.strip_prefix("urn:")?.split_once(':')?;
    let kind = match kind {
        "file" => MediaKind::File,
        "image" => MediaKind::Image,
        "video" => MediaKind::Video,
        _ => return None,
    };
    let (id, name) = match resource.split_once('?') {
        Some((id, query)) => (id, query_name(query)?),
        None => (resource, None),
    };
    if id.len() != 36 {
        return None;
    }
    let file_id = Uuid::parse_str(id).ok().filter(|id| !id.is_nil())?;
    Some((file_id, kind, name))
}

/// Проверяет параметры URN и берёт первое допустимое имя для показа.
fn query_name(query: &str) -> Option<Option<String>> {
    let mut name = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return None;
        }
        let decoded = decode_query_value(value)?;
        if key == "name" && name.is_none() && !decoded.trim().is_empty() {
            name = Some(decoded.trim().to_owned());
        }
    }
    Some(name)
}

/// Декодирует параметр с полными UTF-8 последовательностями, без управляющих символов.
fn decode_query_value(value: &str) -> Option<String> {
    let mut input = value.bytes();
    let mut decoded = Vec::new();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = char::from(input.next()?).to_digit(16)?;
            let low = char::from(input.next()?).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
        } else if byte == b'#' || byte.is_ascii_control() {
            return None;
        } else {
            decoded.push(if byte == b'+' { b' ' } else { byte });
        }
    }
    String::from_utf8(decoded)
        .ok()
        .filter(|text| !text.chars().any(char::is_control))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Включает прямые веб-ссылки, пропускает цитаты, вставленные картинки и опасные схемы.
    #[test]
    fn extracts_external_links_only_from_visible_markdown() {
        let text = concat!(
            "[**Документация**](https://example.test/guide) https://example.test/bare\n\n",
            "<http://example.test/autolink>\n\n",
            "> [цитата](https://private.test/quote)\n\n",
            "\u{60}https://private.test/code\u{60} ![картинка](https://private.test/image)\n\n",
            "[js](javascript:alert()) [вход](https://example.test@private.test/) [путь](/local)",
        );
        let links = external_links(text);
        assert_eq!(
            links
                .into_iter()
                .map(|link| (link.label, link.url))
                .collect::<Vec<_>>(),
            vec![
                ("Документация".into(), "https://example.test/guide".into()),
                (
                    "https://example.test/bare".into(),
                    "https://example.test/bare".into()
                ),
                (
                    "http://example.test/autolink".into(),
                    "http://example.test/autolink".into()
                ),
            ],
        );
    }

    /// Проверяет разбор обычных вложений и отбрасывание ссылок вне доступного источника.
    #[test]
    fn extracts_only_workspace_links_in_the_message_body() {
        let file = Uuid::from_u128(10);
        let image = Uuid::from_u128(11);
        let excluded = Uuid::from_u128(12);
        let tick = char::from(96);
        let text = format!(
            "[Файл](urn:file:{file}?name=%D1%84) ![фото](urn:image:{image})\n\
             > [цитата](urn:video:{excluded})\n\n\
             {tick}[код](urn:file:{excluded}){tick}\n\n\
             \\[экранирование](urn:file:{excluded})\n\n\
             [invalid](urn:file:{excluded}?name=%ZZ) urn:image:{excluded} <urn:file:{excluded}>"
        );
        let ids = media_links(&text)
            .into_iter()
            .map(|link| (link.file_id, link.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![(file, MediaKind::File), (image, MediaKind::Image)]
        );
    }

    /// Сверяет границы UTF-8 и подписи для самостоятельной ссылки и изображения внутри текста.
    #[test]
    fn projects_file_labels_and_ranges_for_rendering() {
        let file = Uuid::from_u128(10);
        let image = Uuid::from_u128(11);
        let file_link = format!("[**Документ**](urn:file:{file}?name=%D0%B8%D0%BC%D1%8F.txt)");
        let image_link = format!("![Кадр](urn:image:{image}?content_type=image%2Fpng)");
        let text = format!("{file_link}\n\nТекст {image_link} после фото.");
        let references = media_references(&text);
        assert_eq!(references.len(), 2);
        let first = &references[0];
        assert_eq!(&text[first.range.clone()], &file_link);
        assert_eq!(first.label, "Документ");
        assert_eq!(first.name.as_deref(), Some("имя.txt"));
        assert_eq!(first.kind, MediaKind::File);
        assert!(!first.image_syntax);
        let second = &references[1];
        assert_eq!(&text[second.range.clone()], &image_link);
        assert_eq!(second.label, "Кадр");
        assert_eq!(second.kind, MediaKind::Image);
        assert!(second.image_syntax);
    }
}
