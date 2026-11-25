//! Проверяет поиск запросов во всех сохранённых темах и соответствие ответов точному адресу.

use super::*;
use crate::{
    context::AccountId,
    messenger::SOURCE_ID,
    storage::{
        Cursor, EventWrite, HistoryChange, HistoryEdge, HistoryMessage, HistoryMetadata,
        HistoryPageEdges, HistoryPageTarget, HistoryScope, HistoryTime, KeyResponseTarget,
        KeyServicePayload, MessageBody, MessagePlacement,
    },
};

/// Размещает служебный пакет так же, как при загрузке истории с REST.
fn save(storage: &Storage, owner: &OwnerContext, rows: &[HistoryMessage]) {
    let first = rows.first().unwrap();
    let scope = HistoryScope {
        stream_id: first.stream_id,
        topic_id: Some(first.topic_id),
    };
    let guard = storage
        .prepare_history_page(owner, scope, HistoryPageTarget::New)
        .unwrap()
        .unwrap();
    storage
        .save_history_page(
            owner,
            guard,
            rows,
            HistoryPageEdges::New {
                older: HistoryEdge::End,
                newer: HistoryEdge::End,
            },
        )
        .unwrap();
}

/// Отмечает удаление или отзыв, обработанный журналом realtime.
fn change(storage: &Storage, owner: &OwnerContext, epoch_version: u64, change: HistoryChange) {
    storage
        .apply(
            owner,
            SOURCE_ID,
            &Cursor {
                epoch_generation: "key-requests".into(),
                epoch_version,
            },
            EventWrite {
                history: Some(&change),
                ..EventWrite::default()
            },
        )
        .unwrap();
}

/// Страницы сохраняют запросы, а индекс ответов ищет только точное совпадение.
#[test]
fn pending_key_requests_span_known_topics_and_require_exact_response() {
    let storage = Storage::in_memory().unwrap();
    let owner = OwnerContext {
        account_id: AccountId("keys".into()),
        server: "https://workspace.test".into(),
        organization_id: "org".into(),
        project_id: "project".into(),
        user_id: "user".into(),
    };
    let other_owner = OwnerContext {
        account_id: AccountId("other".into()),
        ..owner.clone()
    };
    let [
        stream,
        another_stream,
        first_topic,
        second_topic,
        key,
        wrong_key,
        author,
    ] = [(); 7].map(|()| Uuid::new_v4());
    let row = |stream_id, topic_id, id, payload| HistoryMessage {
        read: None,
        reactions: Vec::new(),
        id: Uuid::from_u128(id),
        stream_id,
        topic_id,
        body: MessageBody::KeyService {
            canonical_id: None,
            payload,
        },
        metadata: HistoryMetadata {
            native_source: None,
            author_id: Some(author),
            created_at: HistoryTime::parse("2026-01-01T10:00:00Z").unwrap(),
            updated_at: None,
        },
    };
    let request = |stream_id, topic_id, id| {
        row(
            stream_id,
            topic_id,
            id,
            KeyServicePayload::Request {
                key_uuid: key,
                device_id: "original-desktop-device".into(),
                device_name: "Тестовое устройство".into(),
                public_key: "synthetic".into(),
            },
        )
    };
    let response = |stream_id, topic_id, id, request, key_uuid, device: &str| {
        row(
            stream_id,
            topic_id,
            id,
            KeyServicePayload::Reject(KeyResponseTarget {
                key_uuid,
                request_message_uuid: Uuid::from_u128(request),
                sender_device_id: "other-device".into(),
                recipient_device_id: device.into(),
            }),
        )
    };
    let device = "original-desktop-device";
    let mut first: Vec<_> = (1..=34)
        .map(|id| request(stream, first_topic, id))
        .collect();
    first.extend([
        response(stream, first_topic, 100, 1, key, device),
        response(stream, first_topic, 101, 2, key, "other-device"),
        response(stream, first_topic, 102, 3, wrong_key, device),
        response(stream, first_topic, 103, 5, key, device),
    ]);
    save(&storage, &owner, &first);
    let mut second: Vec<_> = (35..=70)
        .map(|id| request(stream, second_topic, id))
        .collect();
    second.push(response(stream, second_topic, 104, 4, key, device));
    save(&storage, &owner, &second);
    save(
        &storage,
        &owner,
        &[response(another_stream, first_topic, 105, 6, key, device)],
    );
    save(
        &storage,
        &other_owner,
        &[response(stream, first_topic, 106, 7, key, device)],
    );
    change(
        &storage,
        &owner,
        1,
        HistoryChange::RemoveMessage(Uuid::from_u128(103).to_string()),
    );
    change(
        &storage,
        &owner,
        2,
        HistoryChange::RemoveMessage(Uuid::from_u128(35).to_string()),
    );
    let first_page = storage.key_service_requests(&owner, stream, None).unwrap();
    assert_eq!(first_page.requests.len(), KEY_REQUEST_PAGE_SIZE);
    assert_eq!(first_page.requests[0].device_id, device);
    assert_eq!(first_page.requests[0].topic_id, first_topic);
    assert_eq!(first_page.requests[0].device_name, "Тестовое устройство");
    let next = first_page.next.unwrap();
    let second_page = storage
        .key_service_requests(&owner, stream, Some(next.clone()))
        .unwrap();
    assert!(second_page.next.is_none());
    assert_eq!(second_page.requests[0].topic_id, second_topic);
    let ids: Vec<_> = first_page
        .requests
        .into_iter()
        .chain(second_page.requests)
        .map(|hint| hint.request_id.as_u128())
        .collect();
    assert_eq!(ids, (1..=70).filter(|id| *id != 35).collect::<Vec<_>>());
    let request = |id| KeyRequestHint {
        stream_id: stream,
        topic_id: first_topic,
        key_id: key,
        request_id: Uuid::from_u128(id),
        device_id: device.into(),
        device_name: String::new(),
    };
    assert_eq!(
        storage
            .key_reply_candidates(&owner, &request(1), None)
            .unwrap(),
        [Uuid::from_u128(100)]
    );
    for different in 2..=7 {
        assert!(
            storage
                .key_reply_candidates(&owner, &request(different), None)
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        storage
            .key_service_requests(&owner, another_stream, Some(next))
            .unwrap()
            .requests
            .is_empty()
    );
    assert!(
        storage
            .key_service_requests(&other_owner, stream, None)
            .unwrap()
            .requests
            .is_empty()
    );

    change(
        &storage,
        &owner,
        3,
        HistoryChange::RemoveTopic(MessagePlacement {
            stream_id: stream.to_string(),
            topic_id: second_topic.to_string(),
        }),
    );
    let after_revoke = storage.key_service_requests(&owner, stream, None).unwrap();
    assert!(after_revoke.next.is_none());
    assert_eq!(after_revoke.requests.len(), 34);
    assert!(
        after_revoke
            .requests
            .iter()
            .all(|hint| hint.topic_id == first_topic)
    );
}
