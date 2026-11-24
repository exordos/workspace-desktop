//! Проверяет, что ответ о создании «Избранного» относится к нужному входу.

use super::*;
use crate::context::AccountId;

/// Подготавливает владельца и корректный ответ о его личном чате.
fn stream_response() -> (OwnerContext, Uuid, Value) {
    let user = Uuid::from_u128(1);
    let project = Uuid::from_u128(2);
    (
        OwnerContext {
            account_id: AccountId("a".into()),
            server: "https://workspace.test".into(),
            organization_id: "org".into(),
            project_id: project.to_string(),
            user_id: user.to_string(),
        },
        user,
        json!({
            "uuid": Uuid::from_u128(3),
            "project_id": project,
            "user_uuid": user,
            "direct_user_uuid": user,
            "private": true,
            "is_archived": false,
            "source_name": "native",
            "default_topic_uuid": Uuid::from_u128(4),
            "notification_mode": "all_messages",
        }),
    )
}

/// Повтор принимает тот же чат, а расхождения в любом обязательном поле отклоняются.
#[test]
fn verifies_stream_binding_and_default_topic() {
    let (owner, user, response) = stream_response();
    let stream = serde_json::from_value(response.clone()).unwrap();
    let expected = Uuid::from_u128(3);
    assert_eq!(
        self_chat_ids(&owner, user, Some(expected), &stream),
        Ok((expected, Uuid::from_u128(4)))
    );

    for (field, invalid) in [
        ("uuid", json!(Uuid::from_u128(5))),
        ("project_id", json!(Uuid::from_u128(5))),
        ("user_uuid", json!(null)),
        ("direct_user_uuid", json!(Uuid::from_u128(5))),
        ("private", json!(false)),
        ("is_archived", json!(true)),
        ("source_name", json!("zulip")),
        ("default_topic_uuid", json!(Uuid::nil())),
    ] {
        let mut invalid_response = response.clone();
        invalid_response[field] = invalid;
        let stream = serde_json::from_value(invalid_response).unwrap();
        assert_eq!(
            self_chat_ids(&owner, user, Some(expected), &stream),
            Err(SourceError::InvalidResponse),
            "принято неверное поле {field}"
        );
    }
}
