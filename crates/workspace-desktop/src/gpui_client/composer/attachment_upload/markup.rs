//! Собирает ссылки для отправки и исключает временные ссылки из черновиков.

use std::{collections::HashSet, ops::Range};

use uuid::Uuid;
use workspace_core::messenger::{
    encryption::fragments::protected_draft_spans,
    quote::{self, Block},
    references::{MediaReference, media_references},
};

/// Читает карточки правки только из области, которую можно сериализовать открыто.
pub(super) fn public_edit_media(markdown: &str, protect_fragments: bool) -> Vec<MediaReference> {
    let mut references = media_references(markdown);
    if protect_fragments {
        let protected = protected_draft_spans(markdown).collect::<Vec<_>>();
        references.retain(|reference| !overlaps_fragment(&reference.range, &protected));
    }
    references
}

/// Проверяет, пересекает ли ссылка защищённые байты, включая её границы.
fn overlaps_fragment(range: &Range<usize>, protected: &[Range<usize>]) -> bool {
    protected
        .iter()
        .any(|fragment| range.start < fragment.end && fragment.start < range.end)
}

/// Проверяет настоящее вложение Markdown, включая вложения внутри абзаца.
pub(super) fn contains_file(markdown: &str, file: Uuid) -> bool {
    media_references(markdown)
        .iter()
        .any(|reference| reference.file_id == file)
}

/// Снимает ссылки на файлы из текста, включая временную ссылку внутри кода.
pub(super) fn without_files(markdown: &str, links: &[(Uuid, &str)]) -> Option<String> {
    if links.is_empty() {
        return None;
    }
    let mut ranges = media_references(markdown)
        .into_iter()
        .filter(|reference| links.iter().any(|(id, _)| *id == reference.file_id))
        .map(|reference| reference.range)
        .collect::<Vec<_>>();
    ranges.extend(exact_ranges(markdown, links.iter().map(|(_, link)| *link)));
    (!ranges.is_empty()).then(|| without_ranges(markdown, ranges))
}

/// Находит точные временные ссылки, включая те, которые Markdown больше не считает вложениями.
fn exact_ranges<'a>(
    markdown: &'a str,
    links: impl Iterator<Item = &'a str> + 'a,
) -> impl Iterator<Item = Range<usize>> + 'a {
    links.filter(|link| !link.is_empty()).flat_map(|link| {
        markdown
            .match_indices(link)
            .map(|(start, token)| start..start + token.len())
    })
}

/// Вычёркивает объединение диапазонов с сохранением всех остальных байтов.
fn without_ranges(markdown: &str, mut ranges: Vec<Range<usize>>) -> String {
    ranges.sort_unstable_by_key(|range| range.start);
    let mut remaining = String::new();
    let mut cursor = 0;
    for range in ranges {
        if cursor < range.start {
            remaining.push_str(&markdown[cursor..range.start]);
        }
        cursor = cursor.max(range.end);
    }
    remaining.push_str(&markdown[cursor..]);
    remaining
}

/// Сохраняет порядок карточек и переносит временную ссылку из кода в отдельное вложение.
pub(super) fn with_attachments<'a>(
    markdown: &str,
    existing: impl Iterator<Item = (Uuid, &'a str)>,
    temporary: impl Iterator<Item = (Uuid, &'a str)>,
    editing: bool,
    protect_fragments: bool,
) -> String {
    let mut existing = existing.peekable();
    let temporary = temporary.collect::<Vec<_>>();
    if existing.peek().is_none() && temporary.is_empty() {
        return markdown.to_owned();
    }
    let references = media_references(markdown);
    let protected = if protect_fragments {
        protected_draft_spans(markdown).collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut public = references
        .iter()
        .filter(|reference| !overlaps_fragment(&reference.range, &protected))
        .map(|reference| &markdown[reference.range.clone()])
        .collect::<HashSet<_>>();
    let mut covered = references
        .iter()
        .map(|reference| &markdown[reference.range.clone()])
        .collect::<HashSet<_>>();
    let invalid = exact_ranges(markdown, temporary.iter().map(|(_, link)| *link))
        .filter(|range| {
            if overlaps_fragment(range, &protected) {
                // Ссылка, выбранная внутри секрета, не должна появиться перед ним открытым текстом.
                covered.insert(&markdown[range.clone()]);
                return false;
            }
            !references.iter().any(|reference| reference.range == *range)
        })
        .collect::<Vec<_>>();
    let attachments = existing
        .filter_map(|(_, link)| public.insert(link).then_some(link))
        .chain(
            temporary
                .into_iter()
                .filter_map(|(_, link)| covered.insert(link).then_some(link)),
        )
        .collect::<Vec<_>>()
        .join("\n");
    let restored;
    let markdown = if invalid.is_empty() {
        markdown
    } else {
        restored = without_ranges(markdown, invalid);
        &restored
    };
    if attachments.is_empty() {
        return markdown.to_owned();
    }
    let markdown = if editing { markdown } else { markdown.trim() };
    if markdown.is_empty() {
        return attachments;
    }
    if !editing && matches!(quote::blocks(markdown).first(), Some(Block::Quote(_))) {
        let (first, rest) = markdown.split_once('\n').unwrap_or((markdown, ""));
        let rest = rest.trim_start_matches(['\n', '\r']);
        return if rest.is_empty() {
            format!("{first}\n\n{attachments}")
        } else {
            format!("{first}\n\n{attachments}\n\n{rest}")
        };
    }
    format!("{attachments}\n\n{markdown}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Сохраняет порядок выбора после первой цитаты, а уже вставленную картинку оставляет в тексте.
    #[test]
    fn ready_files_follow_reply_and_keep_inline_position() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let third = Uuid::from_u128(3);
        let link = |id| format!("![Фото](urn:image:{id}?name=photo.png&size=16&w=4&h=4)");
        let reply = quote::reference(Uuid::from_u128(4), "Автор", None);
        let input = format!("{reply}\n\nНачало {} конец", link(second));
        let links = [
            (first, link(first)),
            (third, link(third)),
            (second, link(second)),
        ];
        let result = with_attachments(
            &input,
            std::iter::empty(),
            links.iter().map(|(id, link)| (*id, link.as_str())),
            false,
            false,
        );
        assert!(result.starts_with(&reply));
        assert!(result.ends_with(&format!("Начало {} конец", link(second))));
        assert_eq!(
            media_references(&result)
                .into_iter()
                .map(|file| file.file_id)
                .collect::<Vec<_>>(),
            [first, third, second],
        );
        assert_eq!(
            without_files(&input, &[(second, &link(second))]).unwrap(),
            format!("{reply}\n\nНачало  конец")
        );
    }

    /// Сохраняет отдельное вложение на уровне Markdown, когда пользователь не закрыл блок кода.
    #[test]
    fn unattached_code_does_not_absorb_files() {
        let id = Uuid::from_u128(1);
        let markdown = format!("[a.txt](urn:file:{id}?name=a.txt&size=10)");
        let body = with_attachments(
            "```\nabc",
            std::iter::empty(),
            [(id, markdown.as_str())].into_iter(),
            false,
            false,
        );
        assert_eq!(media_references(&body)[0].file_id, id);
        assert!(body.ends_with("```\nabc"));
    }

    /// Исключает временную ссылку из сохраняемого кода и прикладывает её к исходящему сообщению.
    #[test]
    fn temporary_link_inside_code_moves_out_before_send_and_never_reaches_draft() {
        let id = Uuid::from_u128(1);
        let link = format!("![a.png](urn:image:{id}?name=a.png)");
        let markdown = format!("Текст\n\n```\n{link}\n```");
        let links = [(id, link.as_str())];
        assert!(media_references(&markdown).is_empty());
        assert_eq!(
            without_files(&markdown, &links).unwrap(),
            "Текст\n\n```\n\n```",
        );
        let submitted = with_attachments(
            &markdown,
            std::iter::empty(),
            links.into_iter(),
            false,
            false,
        );
        assert_eq!(submitted, format!("{link}\n\nТекст\n\n```\n\n```"));
        assert_eq!(media_references(&submitted).len(), 1);
    }

    /// Исключает расшифрованные ссылки из карточек правки и не выносит их из фрагмента.
    #[test]
    fn protected_links_do_not_become_public_attachments() {
        let id = Uuid::from_u128(1);
        let link = format!("![a.png](urn:image:{id}?name=a.png)");
        let editing = format!("Открытый текст\n\n:::\n{link}\n:::");
        assert_eq!(media_references(&editing).len(), 1);
        assert!(public_edit_media(&editing, true).is_empty());
        let code_editing = format!("Открытый текст\n\n:::\n```\n{link}\n```\n:::");
        let submitted = with_attachments(
            &code_editing,
            std::iter::empty(),
            [(id, link.as_str())].into_iter(),
            true,
            true,
        );
        assert_eq!(submitted, code_editing);
        assert!(
            !without_files(&code_editing, &[(id, &link)])
                .unwrap()
                .contains(&link)
        );
    }

    /// Хранит две подписи одного опубликованного файла и отдельную копию ссылки внутри секрета.
    #[test]
    fn existing_links_deduplicate_by_markup_in_public_scope_only() {
        let id = Uuid::from_u128(1);
        let first = format!("![a](urn:image:{id}?name=a.png)");
        let second = format!("![b](urn:image:{id}?name=a.png)");
        let private = format!(":::\n{first}\n:::");
        let submitted = with_attachments(
            &private,
            [(id, first.as_str()), (id, second.as_str())].into_iter(),
            std::iter::empty(),
            true,
            true,
        );
        assert_eq!(submitted, format!("{first}\n{second}\n\n{private}"));
    }
}
