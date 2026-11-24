//! Разбирает и строит ссылки Workspace на пользователя, разговор, сообщение, веб-адрес и время.
//! Markdown определяет границы элементов. Разбор сам по себе не подтверждает доступ к цели.

use std::ops::Range;

use markdown::{
    ParseOptions,
    mdast::{Link, Node},
    to_mdast,
};
use uuid::Uuid;

use crate::{storage::HistoryTime, sync::MAX_MESSAGE_CHARACTERS};

use super::super::{forward::forward_original_id, quote::quote_original_id};
use super::{display_label, escape_link_label, external_url, parse_media_url, time};

/// Цель ссылки из сообщения или нового элемента редактора.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceReferenceTarget {
    User {
        user_id: Uuid, // UUID пользователя в Workspace.
        silent: bool,  // Заголовок ссылки `silent` подавляет уведомление на сервере.
    },
    Message(Uuid), // Публичный UUID сообщения.
    Stream(Uuid),  // UUID канала или личного разговора.
    Topic {
        topic_id: Uuid,          // UUID темы.
        stream_id: Option<Uuid>, // Старый формат хранит также UUID канала.
    },
    Url(String),       // Проверенный исходный адрес HTTP или HTTPS.
    Time(HistoryTime), // Момент времени, приведённый к UTC.
}

/// Распознанный элемент с границами в исходном теле сообщения.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceReference {
    pub range: Range<usize>, // Байты всего элемента в исходном Markdown.
    pub label_range: Option<Range<usize>>, // Байты подписи, None для ссылки без Markdown-метки.
    pub label: String,       // Читаемая подпись после разбора Markdown.
    pub target: WorkspaceReferenceTarget, // Цель для обработки владельцем действия.
    pub image_syntax: bool,  // Обычные Workspace-ссылки распознаются только в тексте или ссылке.
}

/// Подпись ссылки, для которой в истории нет безопасного действия.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceFallbackReference {
    pub range: Range<usize>, // Байты всего элемента в исходном Markdown.
    pub label_range: Option<Range<usize>>, // Байты подписи, None при пустой подписи.
    pub label: String,       // Читаемая подпись для обычного сообщения.
}

/// Принимает документированные URN в назначении ссылки, независимо от регистра префикса.
pub fn parse_workspace_reference(value: &str) -> Option<WorkspaceReferenceTarget> {
    let value = value.trim();
    if value.len() > MAX_MESSAGE_CHARACTERS {
        return None;
    }
    let (kind, target) = urn_parts(value)?;
    Some(match kind.to_ascii_lowercase().as_str() {
        "user" => WorkspaceReferenceTarget::User {
            user_id: reference_uuid(target)?,
            silent: false,
        },
        "message" => WorkspaceReferenceTarget::Message(reference_uuid(target)?),
        "stream" => WorkspaceReferenceTarget::Stream(reference_uuid(target)?),
        "topic" => {
            let (stream_id, topic_id) = match target.split_once(':') {
                Some((stream_id, topic_id)) => {
                    (Some(reference_uuid(stream_id)?), reference_uuid(topic_id)?)
                }
                None => (None, reference_uuid(target)?),
            };
            WorkspaceReferenceTarget::Topic {
                topic_id,
                stream_id,
            }
        }
        "url" => {
            let href = workspace_web_url(value)?;
            external_url(href)?;
            WorkspaceReferenceTarget::Url(href.to_owned())
        }
        "time" => WorkspaceReferenceTarget::Time(time::parse(target)?),
        _ => return None,
    })
}

/// Собирает URI для ссылки. Тихий режим хранится в заголовке Markdown-ссылки.
pub fn build_workspace_reference_urn(target: &WorkspaceReferenceTarget) -> Option<String> {
    let urn = match target {
        WorkspaceReferenceTarget::User { user_id, .. } => format!("urn:user:{user_id}"),
        WorkspaceReferenceTarget::Message(id) => format!("urn:message:{id}"),
        WorkspaceReferenceTarget::Stream(id) => format!("urn:stream:{id}"),
        WorkspaceReferenceTarget::Topic {
            topic_id,
            stream_id: Some(stream_id),
        } => {
            format!("urn:topic:{stream_id}:{topic_id}")
        }
        WorkspaceReferenceTarget::Topic {
            topic_id,
            stream_id: None,
        } => format!("urn:topic:{topic_id}"),
        WorkspaceReferenceTarget::Url(url) => {
            external_url(url)?;
            format!("urn:url:{url}")
        }
        WorkspaceReferenceTarget::Time(time) => format!("urn:time:{}", time::display_utc(time)?),
    };
    parse_workspace_reference(&urn)?;
    Some(urn)
}

/// Экранирует подпись и создаёт одну ссылку; глобальное время записывает открытым URN.
pub fn build_workspace_reference_markdown(
    label: &str,
    target: &WorkspaceReferenceTarget,
) -> Option<String> {
    let urn = build_workspace_reference_urn(target)?;
    if matches!(target, WorkspaceReferenceTarget::Time(_)) {
        return Some(urn);
    }
    let label = label.trim();
    if label.is_empty() || label.chars().any(char::is_control) {
        return None;
    }
    let escaped = escape_link_label(label);
    let link = match target {
        WorkspaceReferenceTarget::User { silent: true, .. } => {
            format!("[{escaped}]({urn} \"silent\")")
        }
        WorkspaceReferenceTarget::Url(_) => {
            // Скобки внутри веб-адреса могут встречаться по отдельности.
            let mut destination = String::new();
            for ch in urn.chars() {
                if matches!(ch, '\\' | '(' | ')') {
                    destination.push('\\');
                }
                destination.push(ch);
            }
            format!("[{escaped}]({destination})")
        }
        _ => format!("[{escaped}]({urn})"),
    };
    (link.chars().count() <= MAX_MESSAGE_CHARACTERS).then_some(link)
}

/// Сохраняет порядок ссылок и отфильтровывает код, HTML и адреса внутри других ссылок.
pub fn workspace_references(text: &str) -> Vec<WorkspaceReference> {
    if text.len() > MAX_MESSAGE_CHARACTERS * 4
        || !text
            .as_bytes()
            .windows(4)
            .any(|bytes| bytes.eq_ignore_ascii_case(b"urn:"))
            && !text.contains("<@")
    {
        return Vec::new();
    }
    let Ok(document) = to_mdast(text, &ParseOptions::gfm()) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut pending = vec![&document];
    while let Some(node) = pending.pop() {
        match node {
            Node::Link(link) => {
                if let Some(reference) = link_reference(text, node, link) {
                    result.push(reference);
                }
            }
            Node::Text(node) => {
                if let Some(position) = &node.position {
                    let range = position.start.offset..position.end.offset;
                    if let Some(raw) = text.get(range.clone()) {
                        text_references(raw, range.start, &mut result);
                    }
                }
            }
            Node::Html(_) | Node::Image(_) | Node::LinkReference(_) => {}
            _ => pending.extend(node.children().into_iter().flatten().rev()),
        }
    }
    result
}

/// Разбирает явные ссылки без разрешённой цели, сохраняя только их видимую подпись.
pub fn workspace_fallback_references(text: &str) -> Vec<WorkspaceFallbackReference> {
    if text.len() > MAX_MESSAGE_CHARACTERS * 4 || !text.contains("](") {
        return Vec::new();
    }
    let Ok(document) = to_mdast(text, &ParseOptions::gfm()) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut pending = vec![&document];
    while let Some(node) = pending.pop() {
        match node {
            Node::Link(link) => {
                let Some(range) = explicit_link_range(text, link) else {
                    continue;
                };
                if !is_reserved_reference(&link.url)
                    && parse_workspace_reference(&link.url).is_none()
                    && parse_media_url(&link.url).is_none()
                    && external_url(&link.url).is_none()
                {
                    result.push(WorkspaceFallbackReference {
                        range,
                        label_range: link_label_range(link),
                        label: display_label(node),
                    });
                }
            }
            Node::LinkReference(_) => {}
            _ => pending.extend(node.children().into_iter().flatten().rev()),
        }
    }
    result
}

/// Возвращает ссылки на оригиналы открытых цитат и пересылок, включая встроенные в строку.
/// Этот разбор используется только для открытых сообщений и никогда не выдаёт снимок или выдержку.
pub fn workspace_original_references(text: &str) -> Vec<WorkspaceReference> {
    const PREFIXES: [&[u8]; 2] = [b"urn:quote:", b"urn:forward:"];
    if text.len() > MAX_MESSAGE_CHARACTERS * 4
        || !PREFIXES.iter().any(|prefix| {
            text.as_bytes()
                .windows(prefix.len())
                .any(|part| part.eq_ignore_ascii_case(prefix))
        })
    {
        return Vec::new();
    }
    let Ok(document) = to_mdast(text, &ParseOptions::gfm()) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut pending = vec![&document];
    while let Some(node) = pending.pop() {
        match node {
            Node::Link(link) => {
                if let Some(range) = explicit_link_range(text, link)
                    && link.url.len() <= MAX_MESSAGE_CHARACTERS
                    && let Some(message) =
                        quote_original_id(&link.url).or_else(|| forward_original_id(&link.url))
                {
                    result.push(link_projection(
                        node,
                        link,
                        range,
                        WorkspaceReferenceTarget::Message(message),
                    ));
                }
            }
            Node::LinkReference(_) => {}
            _ => pending.extend(node.children().into_iter().flatten().rev()),
        }
    }
    result
}

/// Отдаёт зарезервированные ссылки только тем модулям, которые проверяют цитаты и защищённые данные.
pub(super) fn is_reserved_reference(url: &str) -> bool {
    urn_parts(url.trim()).is_some_and(|(kind, _)| {
        ["quote", "forward", "secret"]
            .iter()
            .any(|reserved| kind.eq_ignore_ascii_case(reserved))
    })
}

/// Возвращает адрес внутри `urn:url`; вызывающий код проверяет правила HTTP и HTTPS.
pub(super) fn workspace_web_url(value: &str) -> Option<&str> {
    let (kind, href) = urn_parts(value.trim())?;
    if !kind.eq_ignore_ascii_case("url") || href.len() > MAX_MESSAGE_CHARACTERS {
        return None;
    }
    Some(href)
}

/// Возвращает исходные границы подписи по дочерним узлам разбора Markdown.
pub(super) fn link_label_range(link: &Link) -> Option<Range<usize>> {
    Some(
        link.children.first()?.position()?.start.offset
            ..link.children.last()?.position()?.end.offset,
    )
}

/// Читает ссылку Markdown и применяет тихий режим только к упоминанию пользователя.
fn link_reference(text: &str, node: &Node, link: &Link) -> Option<WorkspaceReference> {
    let range = explicit_link_range(text, link)?;
    let mut target = parse_workspace_reference(&link.url)?;
    if let WorkspaceReferenceTarget::User { silent, .. } = &mut target {
        *silent = link.title.as_deref() == Some("silent");
    }
    Some(link_projection(node, link, range, target))
}

/// Отличает явную Markdown-ссылку от автоссылки и возвращает её байтовые границы.
fn explicit_link_range(text: &str, link: &Link) -> Option<Range<usize>> {
    let position = link.position.as_ref()?;
    let range = position.start.offset..position.end.offset;
    text.get(range.clone())?.starts_with('[').then_some(range)
}

/// Собирает готовую проекцию с уже проверенной целью.
fn link_projection(
    node: &Node,
    link: &Link,
    range: Range<usize>,
    target: WorkspaceReferenceTarget,
) -> WorkspaceReference {
    WorkspaceReference {
        range,
        label_range: link_label_range(link),
        label: display_label(node),
        target,
        image_syntax: false,
    }
}

/// Находит открытые URN времени и старые упоминания `<@UUID>` в текстовом узле.
fn text_references(raw: &str, offset: usize, output: &mut Vec<WorkspaceReference>) {
    for (start, candidate) in raw.match_indices(['u', 'U', '<']) {
        let suffix = &raw[start..];
        let parsed = match candidate {
            "<" => legacy_mention(suffix),
            _ if !raw[..start]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_alphanumeric() || ch == '_') =>
            {
                time_reference(suffix)
            }
            _ => None,
        };
        let Some((length, target, label, label_range)) = parsed else {
            continue;
        };
        if raw.as_bytes()[..start]
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count()
            % 2
            != 0
        {
            continue;
        }
        output.push(WorkspaceReference {
            range: offset + start..offset + start + length,
            label_range: label_range
                .map(|range| offset + start + range.start..offset + start + range.end),
            label,
            target,
            image_syntax: false,
        });
    }
}

/// Даёт границы и цель старого упоминания. Имя может стать известно позже из каталога.
fn legacy_mention(
    value: &str,
) -> Option<(
    usize,
    WorkspaceReferenceTarget,
    String,
    Option<Range<usize>>,
)> {
    let uuid = value.strip_prefix("<@")?.get(..36)?;
    if value.as_bytes().get(38) != Some(&b'>') {
        return None;
    }
    let user_id = reference_uuid(uuid)?;
    Some((
        39,
        WorkspaceReferenceTarget::User {
            user_id,
            silent: false,
        },
        uuid.to_owned(),
        Some(2..38),
    ))
}

/// Читает один открытый URN времени, оставляя соседнюю пунктуацию в исходном Markdown.
fn time_reference(
    value: &str,
) -> Option<(
    usize,
    WorkspaceReferenceTarget,
    String,
    Option<Range<usize>>,
)> {
    const PREFIX: &str = "urn:time:";
    if !value.get(..PREFIX.len())?.eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    let value = &value[PREFIX.len()..];
    let end = time::prefix_len(value)?;
    let remaining = value.get(end..)?.as_bytes();
    if remaining
        .first()
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        || matches!(remaining, [b'+' | b'-' | b':' | b'.', next, ..] if next.is_ascii_digit())
    {
        return None;
    }
    let time = time::parse(value.get(..end)?)?;
    Some((
        PREFIX.len() + end,
        WorkspaceReferenceTarget::Time(time),
        String::new(),
        None,
    ))
}

/// Делит URI на известную схему, тип и цель.
fn urn_parts(value: &str) -> Option<(&str, &str)> {
    let (scheme, address) = value.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("urn") {
        return None;
    }
    address.split_once(':')
}

/// Проверяет каноническую длину UUID и отбрасывает нулевое значение.
fn reference_uuid(value: &str) -> Option<Uuid> {
    (value.len() == 36).then_some(())?;
    Uuid::parse_str(value).ok().filter(|id| !id.is_nil())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Расставляет типы и сохраняет байтовые границы UTF-8 по порядку чтения.
    #[test]
    fn reads_current_and_legacy_workspace_links() {
        let id = Uuid::from_u128(10);
        let old_stream = Uuid::from_u128(11);
        let modern_mention = format!("[**Собеседник**](URN:USER:{id} \"silent\")");
        let source = format!(
            "От {modern_mention} [сообщение](urn:message:{id}) [чат](urn:stream:{id}) [тема](urn:topic:{id}) [старая тема](urn:topic:{old_stream}:{id}) и <@{id}>"
        );
        let references = workspace_references(&source);
        assert_eq!(references.len(), 6);
        assert_eq!(&source[references[0].range.clone()], modern_mention);
        assert_eq!(
            &source[references[0].label_range.clone().unwrap()],
            "**Собеседник**"
        );
        assert_eq!(references[0].label, "Собеседник");
        assert_eq!(
            references
                .into_iter()
                .map(|reference| reference.target)
                .collect::<Vec<_>>(),
            [
                WorkspaceReferenceTarget::User {
                    user_id: id,
                    silent: true
                },
                WorkspaceReferenceTarget::Message(id),
                WorkspaceReferenceTarget::Stream(id),
                WorkspaceReferenceTarget::Topic {
                    stream_id: None,
                    topic_id: id
                },
                WorkspaceReferenceTarget::Topic {
                    stream_id: Some(old_stream),
                    topic_id: id
                },
                WorkspaceReferenceTarget::User {
                    user_id: id,
                    silent: false
                },
            ]
        );
    }

    /// Оставляет в исходном Markdown адреса в коде, HTML, картинках и повреждённых ссылках.
    #[test]
    fn leaves_unsupported_and_escaped_workspace_links_literal() {
        let id = Uuid::from_u128(10);
        let nil = Uuid::nil();
        let source = format!(
            "`[код](urn:user:{id})`\n\n    [блок](urn:user:{id})\n\n\\[литерал](urn:user:{id}) ![картинка](urn:user:{id}) <urn:user:{id}> [битый](urn:user:{id}?mode=quiet) [неизвестный](urn:user:{nil}) \\<@{id}>\n\n<div>[HTML](urn:user:{id})</div>"
        );
        assert!(workspace_references(&source).is_empty());
    }

    /// Строит обычное и тихое упоминания, не давая имени открыть новую разметку.
    #[test]
    fn builds_round_tripping_mentions() {
        let id = Uuid::from_u128(10);
        for silent in [false, true] {
            let target = WorkspaceReferenceTarget::User {
                user_id: id,
                silent,
            };
            let label = "Иван [из R&D]";
            let markdown = build_workspace_reference_markdown(label, &target).unwrap();
            assert_eq!(markdown.ends_with(" \"silent\")"), silent);
            let [reference] = workspace_references(&markdown).try_into().unwrap();
            let reference: WorkspaceReference = reference;
            assert_eq!(reference.label, label);
            assert_eq!(reference.target, target);
        }
        let target = WorkspaceReferenceTarget::Stream(Uuid::nil());
        assert!(build_workspace_reference_markdown("Чат", &target).is_none());
    }

    /// Один и тот же безопасный адрес отображается в теле и в списке ссылок.
    #[test]
    fn unwraps_http_links_for_history_and_the_external_index() {
        let target = WorkspaceReferenceTarget::Url("https://example.test/a)?item=%5B".into());
        let source = build_workspace_reference_markdown("Веб-страница", &target).unwrap();
        let references = workspace_references(&source);
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].target, target);
        let external = super::super::external_link_references(&source);
        assert_eq!(external.len(), 1);
        assert_eq!(external[0].href, "https://example.test/a)?item=%5B");
        assert_eq!(
            &source[external[0].label_range.clone().unwrap()],
            "Веб\\-страница"
        );
        for invalid in [
            "urn:url:javascript:alert(1)",
            "urn:url:https://alice@example.test/",
            "urn:url://example.test/",
            "urn:url:https://example.test/append whitespace",
        ] {
            assert_eq!(parse_workspace_reference(invalid), None, "{invalid}");
        }
    }

    /// Показывает пользовательские метки недопустимых ссылок, оставляя зарезервированные владельцам.
    #[test]
    fn projects_only_labels_of_unsupported_links() {
        let link = "[**Метка**](urn:user:bad)";
        let source = format!(
            "До {link} [другая](javascript:alert()) [внешняя](https://example.test/) [секрет](urn:secret:bad) [пересылка](urn:forward:bad) [цитата](urn:quote:bad) `[{link}]`"
        );
        let references = workspace_fallback_references(&source);
        assert_eq!(references.len(), 2);
        assert_eq!(&source[references[0].range.clone()], link);
        assert_eq!(references[0].label, "Метка");
        assert_eq!(references[1].label, "другая");
    }

    /// Предлагает переход к оригиналу встроенной открытой ссылки без текста или снимка.
    #[test]
    fn projects_only_valid_original_addresses_from_inline_quote_and_forward() {
        let id = Uuid::from_u128(10);
        let quote = format!("[Цитата](URN:QUOTE:{id}?text=private%20words)");
        let forward = format!("[Пересылка](urn:forward:{id}?snapshot=bWVzc2FnZQ)");
        let source = format!(
            "До {quote}, {forward} и [битый](urn:forward:{id}?snapshot=!) `[код](urn:quote:{id})`."
        );
        let references = workspace_original_references(&source);
        assert_eq!(references.len(), 2);
        for (reference, (raw, label)) in references
            .iter()
            .zip([(quote, "Цитата"), (forward, "Пересылка")])
        {
            assert_eq!(&source[reference.range.clone()], &raw);
            assert_eq!(reference.label, label);
            assert_eq!(reference.target, WorkspaceReferenceTarget::Message(id));
        }
        assert!(workspace_references(&source).is_empty());
    }

    /// Пропускает код, маркеры экранирования и неверные продолжения времени.
    #[test]
    fn recognizes_bare_times_at_markdown_boundaries() {
        let token = "urn:time:2024-02-29T23:59:01.123-00:01";
        let source = format!(
            "`{token}`\n\n\\{token}\n\nword{token}\n\n{token}+13:00\n\n{token}A\n\n**{token}**\n\n> {token}.\n\n| Время |\n| --- |\n| {token} |\n\n[метка](urn:time:2024-02-29T21:59:01.123-02:01)"
        );
        let references = workspace_references(&source);
        assert_eq!(references.len(), 4);
        for reference in references {
            let WorkspaceReferenceTarget::Time(time) = &reference.target else {
                panic!("ожидалось время")
            };
            assert_eq!(time.as_str(), "2024-03-01T00:00:01.123000Z");
            assert_eq!(
                build_workspace_reference_markdown("", &reference.target).as_deref(),
                Some("urn:time:2024-03-01T00:00:01.123Z")
            );
        }
    }
}
