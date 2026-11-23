//! Проверяет UUID повтора и обязательные поля подтверждённого стрима.

use serde_json::json;

use super::*;
use crate::context::AccountId;

const STREAM: Uuid = Uuid::from_u128(61);
const TOPIC: Uuid = Uuid::from_u128(62);
const PEER: Uuid = Uuid::from_u128(63);

/// Даёт исходный проект и владельца для проверки ответа сервера.
fn owner() -> OwnerContext {
    OwnerContext {
        account_id: AccountId("test".into()),
        server: "https://workspace.test".into(),
        organization_id: "organization".into(),
        project_id: "project".into(),
        user_id: "user".into(),
    }
}

/// Преобразует ответ API по тем же правилам, что REST-клиент.
fn stream(peer: Option<Uuid>) -> WireStream {
    serde_json::from_value(json!({
        "uuid": STREAM, "project_id": "project", "user_uuid": "user",
        "source_name": "native", "private": peer.is_some(), "direct_user_uuid": peer,
        "is_archived": false, "default_topic_uuid": TOPIC, "notification_mode": "all_messages",
    }))
    .unwrap()
}

/// Повтор использует один UUID, а новый ввод остаётся отдельной попыткой.
#[test]
fn creation_preserves_normalized_attempt_identity() {
    let attempt = ChannelCreation::new("  Канал  ", "  Описание  ").unwrap();
    assert!(attempt.matches("Канал", "Описание"));
    assert_eq!(attempt, attempt.clone());
    assert!(!attempt.matches("Новый канал", "Описание"));
    assert!(ChannelCreation::new("  ", "Описание").is_err());
    assert!(ChannelCreation::new("Имя", &"т".repeat(256)).is_err());
}

/// Ошибочные адреса не могут открыть другую тему или чужую личку.
#[test]
fn creation_checks_the_conversation_kind_owner_and_topic() {
    let owner = owner();
    let mut channel = stream(None);
    assert_eq!(
        created_channel(&channel, &owner, STREAM),
        Some(CreatedChannel {
            stream_id: STREAM,
            topic_id: TOPIC,
        })
    );
    assert!(created_channel(&channel, &owner, TOPIC).is_none());
    channel.default_topic_uuid = Some(json!(Uuid::nil()));
    assert!(created_channel(&channel, &owner, STREAM).is_none());

    let mut direct = stream(Some(PEER));
    assert_eq!(direct_stream_id(&direct, &owner, PEER).unwrap(), STREAM);
    assert!(direct_stream_id(&direct, &owner, TOPIC).is_err());
    direct.is_archived = true;
    assert!(matches!(
        direct_stream_id(&direct, &owner, PEER),
        Err(ConversationCreationError::Archived)
    ));
    direct.user_uuid = Some("different-user".into());
    assert!(matches!(
        direct_stream_id(&direct, &owner, PEER),
        Err(ConversationCreationError::Source(
            SourceError::InvalidResponse
        ))
    ));
}
