//! Упорядочивает готовые строки каталога и темы. Сохраняет группы поиска,
//! закрепления и порядок снимка, пока времени последнего сообщения ещё нет.

use std::cmp::Ordering;

use workspace_core::messenger::{
    catalog::{Chat, Topic},
    folders::FolderItem,
};

use crate::gpui_client::preferences::{Language, appearance::SidebarSort};

/// Одна строка в своей папке или в дополнительных результатах поиска.
pub(super) type FolderRow<'a> = (&'a Chat, Option<&'a FolderItem>, bool);

/// Сортирует строки внутри группы поиска с учётом закреплений и режима уведомлений.
pub(super) fn sort_chats(rows: &mut [FolderRow<'_>], mode: SidebarSort, language: Language) {
    rows.sort_by(|(a, a_item, a_other), (b, b_item, b_other)| {
        a_other
            .cmp(b_other)
            .then_with(|| pinned(*b_item).cmp(&pinned(*a_item)))
            .then_with(|| {
                if mode == SidebarSort::Alphabetical {
                    return compare_names(&a.name, &b.name, language).then_with(|| a.id.cmp(&b.id));
                }
                let a_muted = stream_muted(a);
                let b_muted = stream_muted(b);
                a_muted
                    .cmp(&b_muted)
                    .then_with(|| {
                        if mode == SidebarSort::UnreadFirst && !a_muted {
                            let a_unread = chat_unread(a, *a_item);
                            let b_unread = chat_unread(b, *b_item);
                            b_unread.cmp(&a_unread)
                        } else {
                            Ordering::Equal
                        }
                    })
                    .then_with(|| chat_time(b).cmp(&chat_time(a)))
                    .then_with(|| {
                        let order = |item: Option<&FolderItem>| {
                            item.and_then(|item| item.order_index).unwrap_or(i32::MAX)
                        };
                        order(*a_item).cmp(&order(*b_item))
                    })
            })
    });
}

/// Возвращает порядок тем как в Electron: обычные, приглушенные и завершённые.
pub(in crate::gpui_client) fn sort_topics(chat: &Chat, mode: SidebarSort) -> Vec<&Topic> {
    let mut topics: Vec<_> = chat.topics.iter().collect();
    topics.sort_by(|a, b| {
        let rank = |topic: &Topic| {
            if topic.metadata.is_done {
                2
            } else {
                u8::from(topic_muted(chat, topic))
            }
        };
        let a_rank = rank(a);
        a_rank
            .cmp(&rank(b))
            .then_with(|| {
                if mode == SidebarSort::UnreadFirst && a_rank == 0 {
                    topic_unread(b).cmp(&topic_unread(a))
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| {
                let b = b
                    .metadata
                    .preview
                    .as_ref()
                    .map(|message| &message.created_at);
                let a = a
                    .metadata
                    .preview
                    .as_ref()
                    .map(|message| &message.created_at);
                b.cmp(&a)
            })
    });
    topics
}

/// Показывает, что в папке есть явное закрепление разговора.
fn pinned(item: Option<&FolderItem>) -> bool {
    item.is_some_and(|item| item.pinned_at.is_some())
}

/// Даёт время последнего сообщения, если каталог уже прочитал его превью.
fn chat_time(chat: &Chat) -> Option<&str> {
    chat.metadata
        .preview
        .as_ref()
        .map(|message| message.created_at.as_str())
}

/// Сравнивает все непрочитанные при отсутствии отдельных активных счётчиков.
fn chat_unread(chat: &Chat, item: Option<&FolderItem>) -> bool {
    if let Some(item) = item {
        return item.active_unread_count > 0;
    }
    chat.metadata
        .active_unread_count
        .or(chat.metadata.unread_count)
        .is_some_and(|count| count > 0)
        || chat.topics.iter().any(|topic| {
            !topic.metadata.is_done && !topic_muted(chat, topic) && topic_unread(topic)
        })
}

/// Учитывает активный счётчик темы и значение старого снимка без деления счётчиков.
fn topic_unread(topic: &Topic) -> bool {
    topic
        .metadata
        .active_unread_count
        .or(topic.metadata.unread_count)
        .is_some_and(|count| count > 0)
}

/// Возвращает true, пока приглушённый канал не переопределён отдельной темой.
pub(super) fn stream_muted(chat: &Chat) -> bool {
    chat.metadata.notification_mode.as_deref() == Some("muted")
        && !chat.topics.iter().any(|topic| {
            matches!(
                topic.metadata.notification_mode.as_deref(),
                Some("unmute" | "follow")
            )
        })
}

/// Применяет выбранный для темы режим с наследованием режима канала.
pub(super) fn topic_muted(chat: &Chat, topic: &Topic) -> bool {
    match topic.metadata.notification_mode.as_deref() {
        Some("mute") => true,
        Some("default") | None => chat.metadata.notification_mode.as_deref() == Some("muted"),
        _ => false,
    }
}

/// Сравнивает названия без учёта регистра, используя язык для русского алфавита и цифр.
fn compare_names(mut a: &str, mut b: &str, language: Language) -> Ordering {
    while let (Some(a_start), Some(b_start)) = (a.chars().next(), b.chars().next()) {
        if a_start.is_ascii_digit() && b_start.is_ascii_digit() {
            let a_tail = a.trim_start_matches(|c: char| c.is_ascii_digit());
            let b_tail = b.trim_start_matches(|c: char| c.is_ascii_digit());
            let a_digits = a[..a.len() - a_tail.len()].trim_start_matches('0');
            let b_digits = b[..b.len() - b_tail.len()].trim_start_matches('0');
            let order = a_digits
                .len()
                .cmp(&b_digits.len())
                .then_with(|| a_digits.cmp(b_digits));
            if order != Ordering::Equal {
                return order;
            }
            a = a_tail;
            b = b_tail;
        } else {
            let character = |c: char| {
                let latin_after_cyrillic = language == Language::Ru && c.is_ascii_alphabetic();
                let c = c.to_lowercase().next().unwrap_or(c);
                (latin_after_cyrillic, if c == 'ё' { 'е' } else { c })
            };
            let order = character(a_start).cmp(&character(b_start));
            if order != Ordering::Equal {
                return order;
            }
            a = &a[a_start.len_utf8()..];
            b = &b[b_start.len_utf8()..];
        }
    }
    b.is_empty().cmp(&a.is_empty())
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;
    use workspace_core::messenger::catalog::ChatMetadata;

    use super::*;

    /// Смена режима переставляет живые группы без смешивания результатов других папок.
    #[test]
    fn chat_sort_respects_unread_alphabetical_and_search_groups() {
        let chat = |id, name: &str, unread| Chat {
            id: Uuid::from_u128(id),
            name: name.into(),
            direct: true,
            topics: Vec::new(),
            metadata: ChatMetadata {
                active_unread_count: Some(unread),
                ..Default::default()
            },
        };
        let a = chat(10, "Стрим 10", 0);
        let b = chat(2, "Стрим 2", 1);
        let other = chat(1, "Стрим 1", 1);
        let mut rows = vec![(&a, None, false), (&b, None, false), (&other, None, true)];
        sort_chats(&mut rows, SidebarSort::LastMessage, Language::Ru);
        assert_eq!(
            rows.iter().map(|row| row.0.id).collect::<Vec<_>>(),
            [a.id, b.id, other.id]
        );
        sort_chats(&mut rows, SidebarSort::UnreadFirst, Language::Ru);
        assert_eq!(
            rows.iter().map(|row| row.0.id).collect::<Vec<_>>(),
            [b.id, a.id, other.id]
        );
        rows.swap(0, 1);
        sort_chats(&mut rows, SidebarSort::Alphabetical, Language::Ru);
        assert_eq!(
            rows.iter().map(|row| row.0.id).collect::<Vec<_>>(),
            [b.id, a.id, other.id]
        );
    }
}
