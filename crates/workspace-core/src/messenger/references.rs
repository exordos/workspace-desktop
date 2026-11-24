//! Строит постоянные ссылки на сообщения и разбирает ссылки Workspace в Markdown.
//! Карточки вложений и индекс доступа используют общий разбор файловых ссылок.

mod composer_links;
mod inline;
mod time;

pub use composer_links::{parse_workspace_conversation_link, workspace_reference_prose_at};
pub use inline::{
    WorkspaceFallbackReference, WorkspaceReference, WorkspaceReferenceTarget,
    build_workspace_reference_markdown, build_workspace_reference_urn, parse_workspace_reference,
    workspace_fallback_references, workspace_original_references, workspace_references,
};

use std::{fmt::Write as _, ops::Range};

use markdown::{ParseOptions, mdast::Node, to_mdast};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    context::OwnerContext,
    storage::media::{ExternalLink, MediaKind, MediaLink},
};

/// Строит ссылку для браузера по адресу Workspace и UUID подтверждённого сообщения.
pub fn message_permalink(owner: &OwnerContext, message_id: Uuid) -> Option<String> {
    if message_id.is_nil() {
        return None;
    }
    let url = super::rest::workspace_origin(owner).ok()?;
    let project = web_route_segment(&owner.project_id)?;

    // Веб-клиент определяет организацию в маршруте по хосту и порту сервера.
    let mut authority = url.host_str()?.to_ascii_lowercase();
    if let Some(port) = url.port() {
        write!(authority, "-{port}").unwrap();
    }
    let organization = authority
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let organization = match organization.as_str() {
        "" => "org",
        "." | ".." => return None,
        organization => organization,
    };

    let origin = url.origin().ascii_serialization();
    Some(format!(
        "{origin}/org/{organization}/project/{project}/message/{message_id}"
    ))
}

/// Проверяет и кодирует сегмент по правилам маршрутов веб-клиента.
fn web_route_segment(value: &str) -> Option<String> {
    if matches!(value, "" | "." | "..")
        || value.contains(|ch: char| {
            ch.is_whitespace() || ch.is_control() || matches!(ch, '/' | '\\' | '%' | '?' | '#')
        })
    {
        return None;
    }
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte.into());
        } else {
            write!(encoded, "%{byte:02X}").unwrap();
        }
    }
    Some(encoded)
}

/// Вложение и сведения, которые можно взять из самого сообщения для показа карточки.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaReference {
    pub range: Range<usize>,  // Границы всей ссылки в байтах исходного Markdown.
    pub label: String,        // Текст Markdown-ссылки или alt изображения.
    pub name: Option<String>, // Имя из параметра URN, если оно пригодно для показа.
    pub file_id: Uuid,        // UUID, который Core сверяет с текущей ссылкой сообщения.
    pub kind: MediaKind,      // Вид из URN или MIME обычной файловой ссылки.
    pub metadata: MediaReferenceMetadata, // Подсказки из URL, сервер проверяет файл отдельно.
    pub image_syntax: bool,   // true для изображения Markdown, false для обычной ссылки.
}

/// Метаданные Workspace, доступные до запроса байтов.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaReferenceMetadata {
    pub content_type: Option<String>, // Заявленный MIME файла.
    pub width: Option<u32>,           // Ширина исходного кадра в пикселях.
    pub height: Option<u32>,          // Высота исходного кадра в пикселях.
    pub size_bytes: Option<u64>,      // Заявленный размер файла в байтах.
}

/// Выделяет подтверждаемые Core вложения в порядке чтения и сохраняет исходные границы.
pub fn media_references(text: &str) -> Vec<MediaReference> {
    if !text
        .as_bytes()
        .windows(4)
        .any(|part| part.eq_ignore_ascii_case(b"urn:"))
    {
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
            Node::Link(link) if inline::is_reserved_reference(&link.url) => continue,
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
            let (file_id, mut kind, name, metadata) = parse_media_url(url)?;
            let label = display_label(node);
            if kind == MediaKind::File {
                kind = infer_file_kind(
                    metadata.content_type.as_deref(),
                    name.as_deref().unwrap_or(&label),
                );
            }
            Some(MediaReference {
                range,
                label,
                name,
                file_id,
                kind,
                metadata,
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
    external_link_references(text)
        .into_iter()
        .map(|link| ExternalLink {
            url: link.url,
            label: link.label,
        })
        .collect()
}

/// Видимая веб-ссылка и её местоположение в исходном теле сообщения.
pub struct ExternalLinkReference {
    pub range: Range<usize>, // Байты всего элемента ссылки в исходном Markdown.
    pub label_range: Option<Range<usize>>, // Исходная подпись, пригодная для привязки к телу.
    pub url: String,         // Проверенный нормализованный адрес HTTP или HTTPS.
    pub href: String,        // Исходный адрес для копирования без изменения представления.
    pub label: String,       // Имя ссылки, пригодное для показа.
}

/// Находит доступные для копирования ссылки и диапазоны для попадания указателем.
pub fn external_link_references(text: &str) -> Vec<ExternalLinkReference> {
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
                if let Some(position) = &link.position
                    && let Some(raw) = text.get(position.start.offset..position.end.offset)
                {
                    let href = inline::workspace_web_url(&link.url)
                        .filter(|_| raw.starts_with('['))
                        .unwrap_or(&link.url);
                    let Some(url) = external_url(href) else {
                        continue;
                    };
                    let label = clean_label(&display_label(node)).unwrap_or_else(|| url.clone());
                    links.push(ExternalLinkReference {
                        range: position.start.offset..position.end.offset,
                        label_range: inline::link_label_range(link),
                        url,
                        href: href.to_owned(),
                        label,
                    });
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

/// Экранирует знаки Markdown, чтобы метка осталась подписью одного элемента.
fn escape_link_label(label: &str) -> String {
    let mut escaped = String::new();
    for ch in label.chars() {
        if ch.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Создаёт ссылку на подтверждённое новое вложение в формате редактора Electron.
pub(super) fn build_media_reference_markdown(
    id: Uuid,
    name: &str,
    mime: &str,
    size: usize,
    dimensions: Option<(u32, u32)>,
) -> String {
    let (image_marker, kind) = if mime.starts_with("image/") {
        ("!", "image")
    } else if mime.starts_with("video/") {
        ("", "video")
    } else {
        ("", "file")
    };
    let mut urn = format!(
        "urn:{kind}:{id}?name={}&content_type={}",
        form_value(name),
        form_value(mime)
    );
    if kind != "file"
        && let Some((width, height)) = dimensions.filter(|(w, h)| *w > 0 && *h > 0)
    {
        write!(urn, "&w={width}&h={height}").unwrap();
    }
    write!(urn, "&size={size}").unwrap();
    format!("{image_marker}[{}]({urn})", escape_link_label(name))
}

/// Кодирует параметр UTF-8 по правилам URLSearchParams.
fn form_value(value: &str) -> String {
    let mut escaped = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"*-._".contains(&byte) {
            escaped.push(byte.into());
        } else if byte == b' ' {
            escaped.push('+');
        } else {
            write!(escaped, "%{byte:02X}").unwrap();
        }
    }
    escaped
}

/// Разбирает только документированные файловые URN и проверяет формат UUID и параметров.
fn parse_media_url(url: &str) -> Option<(Uuid, MediaKind, Option<String>, MediaReferenceMetadata)> {
    let (scheme, url) = url.trim().split_once(':')?;
    if !scheme.eq_ignore_ascii_case("urn") {
        return None;
    }
    let (kind, resource) = url.split_once(':')?;
    let kind = match kind.to_ascii_lowercase().as_str() {
        "file" => MediaKind::File,
        "image" => MediaKind::Image,
        "video" => MediaKind::Video,
        _ => return None,
    };
    let (id, query) = resource.split_once('?').unwrap_or((resource, ""));
    if id.len() != 36 {
        return None;
    }
    let file_id = Uuid::parse_str(id).ok().filter(|id| !id.is_nil())?;
    let (name, metadata) = query_metadata(query)?;
    Some((file_id, kind, name, metadata))
}

/// Проверяет синтаксис запроса и берёт первые пригодные подсказки для показа.
fn query_metadata(query: &str) -> Option<(Option<String>, MediaReferenceMetadata)> {
    let mut name = None;
    let mut metadata = MediaReferenceMetadata::default();
    if query.is_empty() {
        return Some((name, metadata));
    }
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
        let value = decoded.trim();
        match key {
            "name" if name.is_none() && !value.is_empty() => name = Some(value.to_owned()),
            "content_type" if metadata.content_type.is_none() && !value.is_empty() => {
                metadata.content_type = Some(value.to_ascii_lowercase());
            }
            "w" if metadata.width.is_none() => metadata.width = dimension(value),
            "h" if metadata.height.is_none() => metadata.height = dimension(value),
            "size" if metadata.size_bytes.is_none() => metadata.size_bytes = decimal(value),
            _ => {}
        }
    }
    Some((name, metadata))
}

/// Принимает только десятичное значение без знака и переполнения.
fn decimal(value: &str) -> Option<u64> {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| value.parse().ok())
        .flatten()
}

/// Нулевая или слишком большая сторона не участвует в раскладке.
fn dimension(value: &str) -> Option<u32> {
    u32::try_from(decimal(value)?).ok().filter(|side| *side > 0)
}

/// Распознаёт обычные файловые ссылки с MIME или именем старого формата.
fn infer_file_kind(content_type: Option<&str>, name: &str) -> MediaKind {
    let mime = content_type
        .and_then(|mime| mime.split(';').next())
        .map(str::trim);
    match mime {
        Some(mime) if mime.starts_with("image/") => MediaKind::Image,
        Some(mime) if mime.starts_with("video/") => MediaKind::Video,
        None | Some("" | "application/octet-stream") => {
            match name
                .trim()
                .rsplit_once('.')
                .map(|(_, ext)| ext.to_ascii_lowercase())
                .as_deref()
            {
                Some("png" | "jpg" | "jpeg" | "webp" | "gif" | "heic" | "heif") => MediaKind::Image,
                Some("mp4" | "m4v" | "mov" | "webm") => MediaKind::Video,
                _ => MediaKind::File,
            }
        }
        _ => MediaKind::File,
    }
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
    use crate::context::AccountId;

    /// Использует хост и порт аккаунта, кодирует сегмент проекта по правилам Electron.
    #[test]
    fn message_permalink_uses_the_web_route_for_the_active_owner() {
        let owner = OwnerContext {
            account_id: AccountId("account".into()),
            server: "https://Workspace.Example:8443".into(),
            organization_id: "iam-organization-id".into(),
            project_id: "проект:2026".into(),
            user_id: "user".into(),
        };
        let message = Uuid::from_u128(10);
        assert_eq!(
            message_permalink(&owner, message),
            Some(format!(
                "https://workspace.example:8443/org/workspace.example-8443/project/%D0%BF%D1%80%D0%BE%D0%B5%D0%BA%D1%82%3A2026/message/{message}"
            ))
        );
    }

    /// Не создаёт ссылку без подтверждённого UUID, серверного адреса или корректного проекта.
    #[test]
    fn message_permalink_rejects_an_invalid_target() {
        let mut owner = OwnerContext {
            account_id: AccountId("account".into()),
            server: "https://workspace.example".into(),
            organization_id: "organization".into(),
            project_id: "project".into(),
            user_id: "user".into(),
        };
        let message = Uuid::from_u128(10);
        assert_eq!(message_permalink(&owner, Uuid::nil()), None);
        for project in ["", "..", "a/b", "a%2fb", "a?b", "a b"] {
            owner.project_id = project.into();
            assert_eq!(message_permalink(&owner, message), None);
        }
        owner.project_id = "project".into();
        for server in [
            "https://workspace.example/api",
            "https://user@workspace.example",
        ] {
            owner.server = server.into();
            assert_eq!(message_permalink(&owner, message), None);
        }
    }

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
             [invalid](urn:file:{excluded}?name=%ZZ) urn:image:{excluded} <urn:file:{excluded}>\n\n\
             [![скрытый ярлык](urn:image:{excluded})](URN:FORWARD:{excluded}?snapshot=)"
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

    /// Использует MIME обычной ссылки и пропускает некорректные подсказки размера.
    #[test]
    fn reads_media_metadata_from_workspace_urns() {
        let id = Uuid::from_u128(10);
        let text = format!(
            "[Снимок](urn:file:{id}?name=pasted+image.png&content_type=image%2Fpng&w=1920&h=1080&size=2048)\n\n\
             [Видео](URN:FILE:{id}?content_type=Video%2FMP4&w=0&h=4294967296&size=0)\n\n\
             [Старое фото.JPG](urn:file:{id})\n\n\
             [Документ](urn:file:{id}?name=attachment.png&content_type=application%2Fpdf)"
        );
        let references = media_references(&text);
        let [image, video, legacy, doc] = references.as_slice() else {
            panic!("четыре ссылки в сообщении")
        };
        assert_eq!(image.name.as_deref(), Some("pasted image.png"));
        assert_eq!(image.kind, MediaKind::Image);
        assert_eq!(
            image.metadata,
            MediaReferenceMetadata {
                content_type: Some("image/png".into()),
                width: Some(1920),
                height: Some(1080),
                size_bytes: Some(2048),
            }
        );
        assert_eq!(video.kind, MediaKind::Video);
        assert_eq!(video.metadata.width.zip(video.metadata.height), None);
        assert_eq!(video.metadata.size_bytes, Some(0));
        assert_eq!(legacy.kind, MediaKind::Image);
        assert_eq!(doc.kind, MediaKind::File);
    }
}
