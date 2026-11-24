//! Проверяет адреса Workspace, которые редактор может заменить ссылкой на чат или тему.

use markdown::{ParseOptions, mdast::Node, to_mdast};
use reqwest::Url;
use uuid::Uuid;

use crate::context::OwnerContext;

/// Разрешает автоподстановку там, где Markdown прочитает ссылку как ссылку.
pub fn workspace_reference_prose_at(markdown: &str, offset: usize) -> bool {
    if offset >= markdown.len()
        || !markdown.is_char_boundary(offset)
        || markdown.len() > crate::sync::MAX_MESSAGE_CHARACTERS * 4
    {
        return false;
    }
    let Ok(root) = to_mdast(markdown, &ParseOptions::gfm()) else {
        return false;
    };
    let mut next = vec![&root];
    while let Some(node) = next.pop() {
        if node
            .position()
            .is_some_and(|position| position.start.offset > offset || position.end.offset <= offset)
        {
            continue;
        }
        if matches!(
            node,
            Node::Code(_)
                | Node::InlineCode(_)
                | Node::Html(_)
                | Node::Link(_)
                | Node::LinkReference(_)
                | Node::Image(_)
                | Node::ImageReference(_)
                | Node::Definition(_)
        ) {
            return false;
        }
        if let Some(children) = node.children() {
            next.extend(children);
        }
    }
    true
}

/// Возвращает адрес разговора, если ссылка принадлежит серверу и проекту текущего аккаунта.
pub fn parse_workspace_conversation_link(
    raw: &str,
    owner: &OwnerContext,
) -> Option<(Uuid, Option<Uuid>)> {
    if raw
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    let server = Url::parse(&owner.server).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    match url.scheme() {
        "http" | "https" if url.origin() == server.origin() => {}
        "ew" if url.host_str() == Some("open") && url.port().is_none() => {}
        _ => return None,
    }
    let path: Vec<_> = url
        .path()
        .strip_prefix('/')?
        .trim_end_matches('/')
        .split('/')
        .collect();
    let ["org", org, "project", project, "stream", stream, tail @ ..] = path.as_slice() else {
        return None;
    };
    // В веб-маршруте организация может быть представлена именем из адреса сервера.
    let web_org = server_org_slug(&server);
    if !(matches_segment(org, &owner.organization_id) || Some(*org) == web_org.as_deref())
        || !matches_segment(project, &owner.project_id)
    {
        return None;
    }
    let stream = route_uuid(stream)?;
    let topic = match tail {
        [] => None,
        ["topic", topic] => Some(route_uuid(topic)?),
        _ => return None,
    };
    Some((stream, topic))
}

/// Сравнивает сегмент и ID в обычном и URL-кодированном виде.
fn matches_segment(segment: &str, value: &str) -> bool {
    let mut encoded = Url::parse("https://example.invalid").expect("постоянный URL");
    encoded
        .path_segments_mut()
        .expect("у HTTPS есть путь")
        .push(value);
    segment == value || encoded.path().strip_prefix('/') == Some(segment)
}

/// Восстанавливает идентификатор организации, который веб-клиент строит из хоста и порта.
fn server_org_slug(server: &Url) -> Option<String> {
    let host = server.host_str()?;
    let origin = server
        .port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}-{port}"));
    let mut slug = String::new();
    for character in origin.chars() {
        let character = if character.is_ascii_alphanumeric() || character == '.' {
            character
        } else {
            '-'
        };
        if character != '-' || (!slug.is_empty() && !slug.ends_with('-')) {
            slug.push(character);
        }
    }
    Some(slug.trim_end_matches('-').to_owned())
}

/// Принимает только UUID в каноническом формате пути Workspace.
fn route_uuid(segment: &str) -> Option<Uuid> {
    if segment.len() != 36 {
        return None;
    }
    Uuid::parse_str(segment).ok().filter(|id| !id.is_nil())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AccountId;

    /// Исключает код с отступом, незакрытые блоки и вложенный код, оставляя обычные абзацы.
    #[test]
    fn limits_reference_input_to_markdown_prose() {
        for code in [
            "```rust\nlet value = 42;\n@user",
            "    #topic\n",
            "> ~~~\n> @user",
            "``name @user``",
        ] {
            let offset = code.find(['@', '#']).unwrap();
            assert!(!workspace_reference_prose_at(code, offset), "{code}");
        }
        for prose in ["Первый абзац @user", "```\nкод\n```\n\n#topic", "> @user"] {
            let offset = prose.find(['@', '#']).unwrap();
            assert!(workspace_reference_prose_at(prose, offset), "{prose}");
        }
    }

    /// Использует тот же проект для HTTP и deep link и оставляет чужие ссылки нетронутыми.
    #[test]
    fn limits_pasted_links_to_the_current_workspace() {
        let owner = OwnerContext {
            account_id: AccountId("account".into()),
            server: "https://chat.example.test:8443".into(),
            organization_id: "org-id".into(),
            project_id: "project id".into(),
            user_id: "person".into(),
        };
        let stream = Uuid::from_u128(1);
        let topic = Uuid::from_u128(2);
        let path = format!("/org/chat.example.test-8443/project/project%20id/stream/{stream}");
        for prefix in ["https://chat.example.test:8443", "ew://open"] {
            assert_eq!(
                parse_workspace_conversation_link(&format!("{prefix}{path}/topic/{topic}"), &owner),
                Some((stream, Some(topic)))
            );
        }
        for prefix in [
            "https://chat.example.test",
            "https://elsewhere.test:8443",
            "https://person@chat.example.test:8443",
            "ew://other",
        ] {
            assert_eq!(
                parse_workspace_conversation_link(&format!("{prefix}{path}"), &owner),
                None
            );
        }
        let foreign = path.replace("project%20id", "other");
        assert_eq!(
            parse_workspace_conversation_link(&format!("ew://open{foreign}"), &owner),
            None
        );
    }
}
