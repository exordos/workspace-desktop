//! Находит прямые ссылки Workspace на вложения в обычном Markdown сообщений.

use markdown::{ParseOptions, mdast::Node, to_mdast};
use uuid::Uuid;

use crate::storage::media::{MediaKind, MediaLink};

/// Извлекает вложения из Markdown, пропуская код, цитаты и снимки других сообщений.
pub(crate) fn media_links(text: &str) -> Vec<MediaLink> {
    if !text.contains("urn") {
        return Vec::new();
    }
    let Ok(document) = to_mdast(text, &ParseOptions::default()) else {
        return Vec::new();
    };
    let mut links = Vec::new();
    let mut pending = vec![&document];
    while let Some(node) = pending.pop() {
        match node {
            Node::Blockquote(_) => continue,
            Node::Image(image) => links.extend(parse_media_url(&image.url)),
            Node::Link(link) => {
                if link.url.starts_with("urn:quote:") || link.url.starts_with("urn:forward:") {
                    continue;
                }
                // CommonMark также превращает <urn:...> в Link, но у ссылки Workspace есть метка.
                if link
                    .position
                    .as_ref()
                    .is_some_and(|range| text.as_bytes().get(range.start.offset) == Some(&b'['))
                {
                    links.extend(parse_media_url(&link.url));
                }
            }
            _ => {}
        }
        pending.extend(node.children().into_iter().flatten().rev());
    }
    links
}

/// Разбирает только документированные файловые URN и проверяет формат UUID и параметров.
fn parse_media_url(url: &str) -> Option<MediaLink> {
    let (kind, resource) = url.strip_prefix("urn:")?.split_once(':')?;
    let kind = match kind {
        "file" => MediaKind::File,
        "image" => MediaKind::Image,
        "video" => MediaKind::Video,
        _ => return None,
    };
    let (id, query) = resource.split_once('?').unwrap_or((resource, ""));
    if id.len() != 36 || (resource.contains('?') && !valid_query(query)) {
        return None;
    }
    let file_id = Uuid::parse_str(id).ok().filter(|id| !id.is_nil())?;
    Some(MediaLink { file_id, kind })
}

/// Проверяет целые UTF-8 последовательности и percent-encoding в параметрах ссылки.
fn valid_query(query: &str) -> bool {
    query.split('&').all(|pair| {
        let Some((key, value)) = pair.split_once('=') else {
            return false;
        };
        !key.is_empty()
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && valid_query_value(value)
    })
}

/// Проверяет, что параметр декодируется без пропуска байтов и управляющих символов.
fn valid_query_value(value: &str) -> bool {
    let mut input = value.bytes();
    let mut decoded = Vec::new();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let Some(high) = input.next().and_then(|part| char::from(part).to_digit(16)) else {
                return false;
            };
            let Some(low) = input.next().and_then(|part| char::from(part).to_digit(16)) else {
                return false;
            };
            decoded.push((high * 16 + low) as u8);
        } else if byte == b'#' || byte.is_ascii_control() {
            return false;
        } else {
            decoded.push(byte);
        }
    }
    std::str::from_utf8(&decoded).is_ok_and(|text| !text.chars().any(char::is_control))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
