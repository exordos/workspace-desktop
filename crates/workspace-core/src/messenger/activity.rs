//! Собирает входящие из подтверждённых счётчиков каталога и открывает чат с собой.

mod drafts;
mod messages;
mod self_chat;

pub(in crate::messenger) use drafts::RemoteDraft;
pub use drafts::{ActivityDraftEntry, ServerDraftError};
pub use messages::{
    ActivityMessage, ActivityMessageError, ActivityMessageFilter, ActivityMessagePage,
    ActivityMessagePreview,
};
pub use self_chat::SelfChatError;
pub(super) use self_chat::SelfChatRequests;

use uuid::Uuid;

use crate::storage::{HistoryScope, HistoryTime};

use super::catalog::{Chat, SidebarPreview, Topic};

/// Разделы входящих в порядке показа.
#[derive(Default)]
pub struct Inbox<'a> {
    pub personal: Vec<InboxGroup<'a>>, // Личные разговоры с активным непрочитанным.
    pub channels: Vec<InboxGroup<'a>>, // Каналы с активным непрочитанным.
}

impl Inbox<'_> {
    /// Проверяет, остались ли группы для показа.
    pub fn is_empty(&self) -> bool {
        self.personal.is_empty() && self.channels.is_empty()
    }
}

/// Сводка разговора и строки, по которым можно перейти к истории.
pub struct InboxGroup<'a> {
    pub chat: &'a Chat,                   // Разговор из выбранного каталога.
    pub active_unread_count: Option<u64>, // Счётчик разговора. None означает, что он неизвестен.
    pub rows: Vec<InboxRow<'a>>,          // Непрочитанные темы или строка всего разговора.
}

/// Один вход во входящих с подтверждённым активным счётчиком.
pub struct InboxRow<'a> {
    pub scope: HistoryScope, // История темы или разговора. Для канала без темы нужен допуск к каналу целиком.
    pub topic: Option<&'a Topic>, // Тема строки. None показывает заголовок разговора.
    pub active_unread_count: u64, // Количество активных непрочитанных в строке.
    pub updated_at: Option<&'a HistoryTime>, // Время последнего сообщения, если есть его превью.
}

/// Выбирает активное непрочитанное из полного каталога одного владельца.
pub fn project_inbox(chats: &[Chat], current_user: Option<Uuid>) -> Inbox<'_> {
    let mut inbox = Inbox::default();
    for chat in chats {
        if chat.direct && current_user.is_some() && chat.metadata.direct_peer_id == current_user {
            continue;
        }
        let Some(group) = inbox_group(chat) else {
            continue;
        };
        if chat.direct {
            inbox.personal.push(group);
        } else {
            inbox.channels.push(group);
        }
    }
    for groups in [&mut inbox.personal, &mut inbox.channels] {
        groups.sort_by_key(|group| {
            std::cmp::Reverse(preview_time(group.chat.metadata.preview.as_ref()))
        });
    }
    inbox
}

/// Использует точные счётчики тем, а при их отсутствии показывает строку разговора.
fn inbox_group(chat: &Chat) -> Option<InboxGroup<'_>> {
    let active_unread_count = chat.metadata.active_unread_count;
    let mut rows: Vec<_> = chat
        .topics
        .iter()
        .filter_map(|topic| {
            let count = topic
                .metadata
                .active_unread_count
                .filter(|count| *count > 0)?;
            Some(InboxRow {
                scope: HistoryScope {
                    stream_id: chat.id,
                    topic_id: Some(topic.id),
                },
                topic: Some(topic),
                active_unread_count: count,
                updated_at: preview_time(topic.metadata.preview.as_ref()),
            })
        })
        .collect();

    if rows.is_empty() {
        let count = active_unread_count.filter(|count| *count > 0)?;
        // Если известна общая тема, открываем её из строки разговора.
        let topic_id = chat
            .metadata
            .default_topic_id
            .filter(|id| chat.topics.iter().any(|topic| topic.id == *id));
        rows.push(InboxRow {
            scope: HistoryScope {
                stream_id: chat.id,
                topic_id,
            },
            topic: None,
            active_unread_count: count,
            updated_at: preview_time(chat.metadata.preview.as_ref()),
        });
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at));
    Some(InboxGroup {
        chat,
        active_unread_count,
        rows,
    })
}

/// Берёт время последнего сообщения из уже загруженных данных каталога.
fn preview_time(preview: Option<&SidebarPreview>) -> Option<&HistoryTime> {
    preview.map(|message| &message.created_at)
}

#[cfg(test)]
mod tests;
