//! Проверяет отбор строк активности и безопасные данные карточек.

use serde_json::{Value, json};

use crate::{context::AccountId, sync::Event};

use super::*;

/// Возвращает базовый ответ Workspace с личными признаками владельца.
fn row(owner: &OwnerContext, payload: Value) -> Value {
    json!({
        "uuid": Uuid::from_u128(1),
        "stream_uuid": Uuid::from_u128(2),
        "topic_uuid": Uuid::from_u128(3),
        "author_uuid": Uuid::from_u128(4),
        "project_id": owner.project_id,
        "user_uuid": owner.user_id,
        "created_at": "2026-07-16T12:30:00Z",
        "read": false,
        "mentioned": true,
        "starred": false,
        "payload": payload
    })
}

/// Создаёт отдельный контекст, которым ограничен тестовый ответ.
fn owner() -> OwnerContext {
    OwnerContext {
        account_id: AccountId("account".into()),
        server: "https://workspace.test".into(),
        organization_id: "organization".into(),
        project_id: Uuid::from_u128(5).to_string(),
        user_id: Uuid::from_u128(6).to_string(),
    }
}

/// Принимает точные поля и не переносит доступ по ссылке в превью.
#[test]
fn unread_card_uses_public_placement_and_safe_preview() {
    let owner = owner();
    let input = row(
        &owner,
        json!({
            "kind": "markdown",
            "content": "Привет [вложение](https://workspace.test/api/private-file)"
        }),
    );
    let message: WireActivityMessage = serde_json::from_value(input).unwrap();
    let message = message
        .into_activity(&owner, ActivityMessageFilter::UnreadMentions)
        .unwrap();
    assert_eq!(message.id, Uuid::from_u128(1));
    assert_eq!(message.topic_id, Uuid::from_u128(3));
    assert!(!message.read);
    assert_eq!(
        message.preview,
        ActivityMessagePreview::Text("Привет вложение".into())
    );
}

/// Отклоняет неверного владельца и строку другого фильтра.
#[test]
fn rejects_mismatched_owner_and_collection() {
    let owner = owner();
    let input = row(&owner, json!({"kind": "e2ee", "content": "ciphertext"}));
    let message: WireActivityMessage = serde_json::from_value(input.clone()).unwrap();
    assert!(
        message
            .into_activity(&owner, ActivityMessageFilter::Marked)
            .is_err()
    );
    let mut stale = input;
    stale["user_uuid"] = json!(Uuid::from_u128(99));
    let message: WireActivityMessage = serde_json::from_value(stale).unwrap();
    assert!(
        message
            .into_activity(&owner, ActivityMessageFilter::Mentions)
            .is_err()
    );
}

/// Закрытый снимок имеет UI-метку и не передаёт зашифрованное тело.
#[test]
fn encrypted_card_only_exposes_placeholder() {
    let owner = owner();
    let message: WireActivityMessage = serde_json::from_value(row(
        &owner,
        json!({"kind": "e2ee", "content": "ciphertext"}),
    ))
    .unwrap();
    let message = message
        .into_activity(&owner, ActivityMessageFilter::Mentions)
        .unwrap();
    assert_eq!(message.preview, ActivityMessagePreview::Encrypted);
}

/// Проверяет область, направление, размер страницы и отличия трёх HTTP-запросов.
#[test]
fn rest_query_scopes_filters_and_uses_server_marker() {
    let owner = owner();
    let rest = RestClient::new(None).unwrap();
    let marker = Uuid::from_u128(18);
    for (filter, marked, mentioned, read) in [
        (ActivityMessageFilter::Marked, Some("true"), None, None),
        (ActivityMessageFilter::Mentions, None, Some("true"), None),
        (
            ActivityMessageFilter::UnreadMentions,
            None,
            Some("true"),
            Some("false"),
        ),
    ] {
        let request = rest
            .activity_request(&owner, "test", filter, Some(marker), 50)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.url().path(), MESSAGES_PATH);
        let query: HashMap<_, _> = request.url().query_pairs().collect();
        let value = |key| query.get(key).map(|value| value.as_ref());
        assert_eq!(value("project_id"), Some(owner.project_id.as_str()));
        assert_eq!(value("page_marker"), Some(marker.to_string().as_str()));
        assert_eq!(value("page_limit"), Some("50"));
        assert_eq!(value("sort_key"), Some("created_at"));
        assert_eq!(value("sort_dir"), Some("desc"));
        assert_eq!(value("starred"), marked);
        assert_eq!(value("mentioned"), mentioned);
        assert_eq!(value("read"), read);
    }
}

/// Отсутствие заголовка закрывает список, повтор предыдущего UUID считается ошибкой.
#[test]
fn rejects_empty_or_repeated_pagination_marker() {
    let marker = Uuid::from_u128(18);
    let mut headers = HeaderMap::new();
    assert_eq!(next_marker(&headers, None).unwrap(), None);
    headers.insert("x-pagination-marker", marker.to_string().parse().unwrap());
    assert_eq!(next_marker(&headers, None).unwrap(), Some(marker));
    assert!(next_marker(&headers, Some(marker)).is_err());
    headers.insert("x-pagination-marker", "".parse().unwrap());
    assert!(next_marker(&headers, None).is_err());
}

/// Сохраняет сигнал подтверждённого чтения и отбрасывает чужое событие.
#[test]
fn read_events_invalidate_collections_in_their_owner() {
    let owner = owner();
    for kind in ["message.read", "messages.read"] {
        let event = json!({
            "schema_version": 1,
            "uuid": Uuid::from_u128(80),
            "epoch_version": 10,
            "project_id": owner.project_id,
            "user_uuid": owner.user_id,
            "object_type": "message",
            "action": "read",
            "payload": {"kind": kind, "project_id": owner.project_id}
        });
        let (parsed, _) = super::super::wire_event(event.clone(), &owner, "epoch").unwrap();
        assert!(matches!(parsed, Event::ActivityRead(cursor) if cursor.epoch_version == 10));
        let mut other = event;
        other["payload"]["project_id"] = json!(Uuid::from_u128(99));
        assert!(super::super::wire_event(other, &owner, "epoch").is_err());
    }
}
