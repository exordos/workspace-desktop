//! Проверяет строгую привязку точного серверного черновика к владельцу и его версии.

use serde_json::json;

use super::*;
use crate::context::AccountId;

/// Отклоняет совпавший текст, если версия, владелец, UUID или тема ответа отличаются.
#[test]
fn confirms_exact_owner_route_and_strong_etag() {
    let owner = OwnerContext {
        account_id: AccountId("a".into()),
        server: "https://workspace.test".into(),
        organization_id: "org".into(),
        project_id: "project".into(),
        user_id: "user".into(),
    };
    let id = Uuid::from_u128(1);
    let scope = DraftScope {
        stream_id: Uuid::from_u128(2),
        topic_id: Uuid::from_u128(3),
    };
    let response = json!({
        "uuid": id, "project_id": owner.project_id, "user_uuid": owner.user_id,
        "stream_uuid": scope.stream_id, "topic_uuid": scope.topic_id,
        "revision": 4, "updated_at": "2026-11-18T12:00:00Z",
        "payload": {"kind": "markdown", "content": "Набранный текст"},
    });
    let parse = |value| serde_json::from_value::<WireDraft>(value).unwrap();
    let valid = parse(response.clone())
        .into_exact_draft(&owner, scope, id, "\"4\"")
        .unwrap();
    assert_eq!(valid.markdown, "Набранный текст");

    for (field, invalid) in [
        ("uuid", json!(Uuid::from_u128(8))),
        ("project_id", json!("other-project")),
        ("user_uuid", json!("other-user")),
        ("topic_uuid", json!(Uuid::from_u128(8))),
        ("revision", json!(5)),
    ] {
        let mut wrong = response.clone();
        wrong[field] = invalid;
        assert!(
            parse(wrong)
                .into_exact_draft(&owner, scope, id, "\"4\"")
                .is_err(),
            "принято неверное поле {field}"
        );
    }
    assert!(
        parse(response)
            .into_exact_draft(&owner, scope, id, "W/\"4\"")
            .is_err()
    );
}
