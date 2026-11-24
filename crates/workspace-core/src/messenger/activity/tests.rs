//! Проверяет счётчики входящих, выбор точной темы и порядок показа.

use super::*;
use crate::messenger::catalog::{ChatMetadata, TopicMetadata};

/// Создаёт строку каталога с активным и пассивным непрочитанным.
fn chat(id: u128, direct: bool, active: Option<u64>) -> Chat {
    Chat {
        id: Uuid::from_u128(id),
        name: format!("Разговор {id}"),
        direct,
        topics: Vec::new(),
        metadata: ChatMetadata {
            unread_count: Some(20),
            active_unread_count: active,
            passive_unread_count: active.map(|active| 20 - active),
            ..Default::default()
        },
    }
}

/// Добавляет тему с накопленным счётчиком из ответа сервера.
fn topic(id: u128, active: Option<u64>) -> Topic {
    Topic {
        id: Uuid::from_u128(id),
        name: format!("Тема {id}"),
        metadata: TopicMetadata {
            unread_count: Some(10),
            active_unread_count: active,
            passive_unread_count: active.map(|active| 10 - active),
            ..Default::default()
        },
    }
}

/// Подставляет точное время последнего сообщения для проверки порядка.
fn preview(day: u8) -> SidebarPreview {
    SidebarPreview {
        message_id: Uuid::from_u128(12),
        topic_id: Uuid::from_u128(13),
        author_id: Uuid::from_u128(14),
        author_name: None,
        created_at: HistoryTime::parse(&format!("2026-01-{day:02}T00:00:00Z")).unwrap(),
        text: "Сообщение".into(),
    }
}

/// Показывает точные темы и личные, пропуская пассивные, старые и собственные записи.
#[test]
fn splits_groups_without_using_raw_unread_as_active() {
    let mut channel = chat(10, false, Some(5));
    channel.topics = vec![topic(11, Some(2)), topic(12, Some(0)), topic(13, None)];
    let mut direct = chat(20, true, Some(3));
    direct.topics.push(topic(23, Some(9)));
    let mut own = chat(21, true, Some(4));
    let user = Uuid::from_u128(100);
    own.metadata.direct_peer_id = Some(user);
    let chats = [channel, direct, own, chat(22, true, None)];

    let inbox = project_inbox(&chats, Some(user));
    assert_eq!(inbox.personal.len(), 1);
    assert_eq!(inbox.personal[0].rows[0].scope.stream_id, chats[1].id);
    assert_eq!(
        inbox.personal[0].rows[0].scope.topic_id,
        Some(chats[1].topics[0].id)
    );
    assert_eq!(inbox.personal[0].rows[0].active_unread_count, 9);
    assert_eq!(inbox.channels.len(), 1);
    assert_eq!(inbox.channels[0].active_unread_count, Some(5));
    assert_eq!(inbox.channels[0].rows.len(), 1);
    let row = &inbox.channels[0].rows[0];
    assert_eq!(row.scope.stream_id, chats[0].id);
    assert_eq!(row.scope.topic_id, Some(chats[0].topics[0].id));
    assert_eq!(row.active_unread_count, 2);
}

/// Оставляет строку канала с активным счётчиком и проверяет его доступную общую тему.
#[test]
fn stream_count_remains_visible_without_unread_topics() {
    let mut channel = chat(10, false, Some(8));
    channel.topics = vec![topic(11, Some(0))];
    channel.metadata.default_topic_id = Some(channel.topics[0].id);
    let mut unavailable = chat(20, false, Some(4));
    unavailable.metadata.default_topic_id = channel.metadata.default_topic_id;
    let chats = [channel, unavailable];

    let inbox = project_inbox(&chats, None);
    assert_eq!(inbox.channels.len(), 2);
    let first = &inbox.channels[0].rows[0];
    assert!(first.topic.is_none());
    assert_eq!(first.scope.topic_id, chats[0].metadata.default_topic_id);
    assert_eq!(first.active_unread_count, 8);
    assert_eq!(inbox.channels[1].rows[0].scope.topic_id, None);
}

/// Обновляет список по новому снимку и сохраняет порядок каталога при равенстве времени.
#[test]
fn orders_confirmed_times_and_drops_read_rows() {
    let mut first = chat(10, false, None);
    first.topics = vec![topic(11, Some(3)), topic(12, Some(1))];
    first.topics[1].metadata.preview = Some(preview(2));
    let mut second = chat(20, false, Some(1));
    second.metadata.preview = Some(preview(1));
    let third = chat(30, false, Some(1));
    let mut chats = [first, second, third];

    let inbox = project_inbox(&chats, None);
    assert_eq!(
        inbox
            .channels
            .iter()
            .map(|group| group.chat.id)
            .collect::<Vec<_>>(),
        [chats[1].id, chats[0].id, chats[2].id]
    );
    assert_eq!(inbox.channels[1].active_unread_count, None);
    assert_eq!(
        inbox.channels[1].rows[0].scope.topic_id,
        Some(chats[0].topics[1].id)
    );

    chats[0].topics[1].metadata.active_unread_count = Some(0);
    let inbox = project_inbox(&chats, None);
    assert_eq!(inbox.channels[1].rows.len(), 1);
    assert_eq!(
        inbox.channels[1].rows[0].scope.topic_id,
        Some(chats[0].topics[0].id)
    );
}
