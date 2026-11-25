//! Проверяет связь истории и событий с опубликованным превью каталога.

use super::*;
use crate::{
    bootstrap::{ChatList, ChatListStatus, ChatLists, publish_catalog},
    context::{AccountId, SessionGeneration},
    messenger::{MessengerSource, SOURCE_ID, rest::WireHistoryMessage},
    storage::{
        Cursor, EventWrite, HistoryChange, HistoryEdge, HistoryMetadata, HistoryPageEdges,
        HistoryPageTarget, HistoryScope, HistoryTime, Message, MessageBody, MessagePlacement,
    },
};

/// Проверяет обновление канала и темы из двух путей записи и запоздалый REST-снимок.
#[test]
fn latest_catalog_preview_survives_delayed_snapshot() {
    check_latest_preview(false);
}

/// Проверяет ту же последовательность в личке, где темы не входят в каталог.
#[test]
fn latest_direct_preview_survives_delayed_snapshot() {
    check_latest_preview(true);
}

/// Публикует один каталог с начальным превью, локальными сообщениями и старым ответом.
fn check_latest_preview(direct: bool) {
    let author = Uuid::from_u128(10);
    let owner = OwnerContext {
        account_id: AccountId("account".into()),
        server: "https://test".into(),
        organization_id: "org".into(),
        project_id: "project".into(),
        user_id: author.to_string(),
    };
    let stream = Uuid::from_u128(1);
    let topic = Uuid::from_u128(2);
    let other_topic = Uuid::from_u128(3);
    let old = Uuid::from_u128(4);
    let from_history = Uuid::from_u128(5);
    let from_event = Uuid::from_u128(6);
    let delayed = Uuid::from_u128(7);
    let topics = |last| {
        if direct {
            serde_json::json!([])
        } else {
            serde_json::json!([
                {"id": topic, "name": "one", "metadata": {"last_message_id": last}},
                {"id": other_topic, "name": "two"}
            ])
        }
    };
    let snapshot = |last| {
        serde_json::json!([{
            "id": stream, "name": "chat", "direct": direct,
            "metadata": {"last_message_id": last},
            "topics": topics(last)
        }])
        .to_string()
    };
    let storage = Arc::new(Storage::in_memory().unwrap());
    storage
        .save_chat_catalog(&owner, &snapshot(old), 0, None)
        .unwrap();
    let delayed_revision = storage.catalog_revision(&owner).unwrap();
    let messenger = MessengerSource::new().unwrap();
    let catalog = messenger.catalog(storage.clone());
    let mut changes = catalog.changes();
    let mut current = ChatLists::from([(
        owner.clone(),
        ChatList {
            generation: SessionGeneration(1),
            chats: None,
            status: ChatListStatus::Ready,
        },
    )]);
    let (lists, published) = tokio::sync::watch::channel(ChatLists::new());
    let time = |second: u8| HistoryTime::parse(&format!("2026-11-23T12:00:{second:02}Z")).unwrap();
    let preview = |message_id, second, text| {
        (
            message_id,
            (
                stream,
                SidebarPreview {
                    message_id,
                    topic_id: topic,
                    author_id: author,
                    author_name: None,
                    created_at: time(second),
                    text: SidebarPreviewBody::from(text),
                },
            ),
        )
    };
    let mut previews = HashMap::from([(owner.clone(), HashMap::from([preview(old, 1, "Старое")]))]);
    let mut check_rows = |previews: &HashMap<OwnerContext, SidebarPreviews>, expected, text| {
        publish_catalog(&catalog, &mut current, previews, &lists).unwrap();
        let lists = published.borrow();
        let chat = &lists[&owner].chats.as_ref().unwrap()[0];
        let rows = [(chat.metadata.last_message_id, &chat.metadata.preview)]
            .into_iter()
            .chain(
                chat.topics
                    .first()
                    .map(|topic| (topic.metadata.last_message_id, &topic.metadata.preview)),
            );
        for (id, preview) in rows {
            assert_eq!(id, Some(expected));
            let preview = preview.as_ref().unwrap();
            assert_eq!(preview.message_id, expected);
            assert_eq!(preview.body(), &SidebarPreviewBody::from(text));
        }
        if let Some(other) = chat.topics.get(1) {
            assert!(other.metadata.preview.is_none());
        }
    };
    check_rows(&previews, old, "Старое");

    let scope = HistoryScope {
        stream_id: stream,
        topic_id: Some(topic),
    };
    let metadata = |second| HistoryMetadata {
        native_source: None,
        author_id: Some(author),
        created_at: time(second),
        updated_at: None,
    };
    let guard = storage
        .prepare_history_page(&owner, scope, HistoryPageTarget::New)
        .unwrap()
        .unwrap();
    let remote_row: WireHistoryMessage = serde_json::from_value(serde_json::json!({
        "uuid": from_history, "stream_uuid": stream, "topic_uuid": topic,
        "project_id": owner.project_id, "user_uuid": author, "author_uuid": author,
        "created_at": time(2).as_str(),
        "payload": {"kind": "markdown", "content": "Из истории"}
    }))
    .unwrap();
    let history_row = remote_row.into_presentation(&owner).unwrap().unwrap();
    storage
        .save_history_page(
            &owner,
            guard,
            &[history_row],
            HistoryPageEdges::New {
                older: HistoryEdge::End,
                newer: HistoryEdge::End,
            },
        )
        .unwrap();
    check_rows(&previews, from_history, "Из истории");
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();

    let cursor = |epoch_version| Cursor {
        epoch_generation: "epoch".into(),
        epoch_version,
    };
    storage
        .apply(
            &owner,
            SOURCE_ID,
            &cursor(1),
            EventWrite {
                message: Some(&Message {
                    id: from_event.to_string(),
                    body: MessageBody::Markdown("Из события".into()),
                }),
                placement: Some(&MessagePlacement {
                    stream_id: stream.to_string(),
                    topic_id: topic.to_string(),
                }),
                metadata: Some(&metadata(3)),
                ..EventWrite::default()
            },
        )
        .unwrap();
    check_rows(&previews, from_event, "Из события");
    assert!(changes.has_changed().unwrap());

    storage
        .save_chat_catalog(&owner, &snapshot(delayed), delayed_revision, None)
        .unwrap();
    check_rows(&previews, from_event, "Из события");
    previews
        .get_mut(&owner)
        .unwrap()
        .extend([preview(delayed, 0, "Запоздалое")]);
    check_rows(&previews, from_event, "Из события");

    let before_removal = storage.catalog_revision(&owner).unwrap();
    storage
        .apply(
            &owner,
            SOURCE_ID,
            &cursor(2),
            EventWrite {
                history: Some(&HistoryChange::RemoveMessage(from_event.to_string())),
                ..EventWrite::default()
            },
        )
        .unwrap();
    // Удалённое сообщение ещё не было указано в снимке. Поздний ответ с его UUID допустим.
    storage
        .save_chat_catalog(&owner, &snapshot(from_event), before_removal, None)
        .unwrap();
    previews
        .get_mut(&owner)
        .unwrap()
        .extend([preview(from_event, 3, "Удалённое")]);
    check_rows(&previews, from_history, "Из истории");
}
