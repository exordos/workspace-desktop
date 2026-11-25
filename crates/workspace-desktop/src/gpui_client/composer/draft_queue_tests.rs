//! Проверяет сохранение независимых черновиков при задержанной записи одной темы.

use super::*;
use workspace_core::context::AccountId;

/// Пока запись занята, снимки разных UUID остаются в очереди, а версии одного UUID объединяются.
#[test]
fn delayed_writer_retains_the_latest_version_of_each_draft() {
    let storage = workspace_core::storage::Storage::in_memory().unwrap();
    let owner = OwnerContext {
        account_id: AccountId("queue-owner".into()),
        server: "https://workspace.test".into(),
        organization_id: "org".into(),
        project_id: "project".into(),
        user_id: "user".into(),
    };
    let scope = DraftScope {
        stream_id: Uuid::new_v4(),
        topic_id: Uuid::new_v4(),
    };
    let access = storage.prepare_draft_access(&owner, scope).unwrap();
    let key = Key {
        owner: owner.clone(),
        generation: SessionGeneration(1),
        stream: scope.stream_id,
        topic: scope.topic_id,
    };
    let buffer = DraftBuffer::default();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let snapshot = |id, version, markdown: &str| Pending {
        access: access.clone(),
        literal_target: None,
        value: PlaintextDraft {
            id,
            version,
            markdown: markdown.into(),
            replies: None,
        },
    };

    assert!(buffer.stage(key.clone(), snapshot(first, 1, "Первый")));
    assert!(!buffer.stage(key.clone(), snapshot(second, 1, "Второй")));
    assert!(!buffer.stage(key.clone(), snapshot(first, 2, "Первый обновлён")));
    assert!(!buffer.stage(key.clone(), snapshot(second, 2, "Второй обновлён")));

    let slots = buffer.slots.lock().unwrap();
    let slot = &slots[&key];
    assert_eq!(slot.queued.len(), 1);
    assert_eq!(slot.queued[0].value.id, first);
    assert_eq!(slot.queued[0].value.version, 2);
    assert_eq!(slot.queued[0].value.markdown, "Первый обновлён");
    let pending = slot.pending.as_ref().unwrap();
    assert_eq!(pending.value.id, second);
    assert_eq!(pending.value.version, 2);
    assert_eq!(pending.value.markdown, "Второй обновлён");
    drop(slots);

    let address = ConversationAddress {
        route: crate::gpui_client::contracts::ChatRoute {
            owner,
            scope: workspace_core::storage::HistoryScope {
                stream_id: scope.stream_id,
                topic_id: Some(scope.topic_id),
            },
            entry: crate::gpui_client::contracts::ChatEntry::Latest,
        },
        generation: SessionGeneration(1),
        revision: crate::gpui_client::contracts::NavigationRevision(1),
    };
    let (_, queued, _) = buffer.current(&address, scope, Some(first)).unwrap();
    assert_eq!(queued.version, 2);
    assert_eq!(queued.markdown, "Первый обновлён");
}
