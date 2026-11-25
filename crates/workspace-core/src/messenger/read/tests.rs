//! Проверяет объединение видимых границ, изоляцию и итог фоновой очереди.

use super::*;
use crate::{context::AccountId, storage::Storage};

/// Создаёт адрес независимого входа и темы для проверки очереди.
fn key(user: &str, generation: u64, topic: u128) -> ReadKey {
    ReadKey {
        owner: OwnerContext {
            account_id: AccountId(user.into()),
            server: "https://workspace.test".into(),
            organization_id: "organization".into(),
            project_id: "project".into(),
            user_id: user.into(),
        },
        generation: SessionGeneration(generation),
        scope: HistoryScope {
            stream_id: Uuid::from_u128(1),
            topic_id: Some(Uuid::from_u128(topic)),
        },
    }
}

/// Готовит заявку с одним временем, чтобы проверить сравнение UUID при совпадении времени.
fn intent(key: &ReadKey, id: u128, observed: &[u128], revision: i64) -> ReadIntent {
    ReadIntent {
        boundary: ReadBoundary {
            target: MessageTarget {
                scope: key.scope,
                topic_id: key.scope.topic_id.unwrap(),
                message_id: Uuid::from_u128(id),
            },
            created_at: HistoryTime::parse("2026-01-01T12:00:00Z").unwrap(),
        },
        observed: observed.iter().copied().map(Uuid::from_u128).collect(),
        access_revision: revision,
        id: Uuid::new_v4(),
    }
}

/// Сохраняет одну активную заявку, объединяет наблюдения и не требует открытого окна.
#[test]
fn queue_coalesces_new_evidence_and_finishes_after_navigation() {
    let queues = ReadRequests::default();
    let first = key("first", 1, 2);
    let early = intent(&first, 7, &[7], 0);
    let later = intent(&first, 8, &[8], 0);
    let (screen, spawn) = queues.enqueue(first.clone(), early);
    assert!(spawn);
    drop(screen);
    let (outcome, spawn) = queues.enqueue(first.clone(), later.clone());
    assert!(!spawn);

    let active = queues.take(&first).unwrap();
    assert_eq!(active.boundary, later.boundary);
    assert!(active.observed.contains(&Uuid::from_u128(7)));
    assert!(active.observed.contains(&Uuid::from_u128(8)));
    let (_, spawn) = queues.enqueue(first.clone(), later);
    assert!(!spawn);

    let last = intent(&first, 9, &[9], 0);
    let (following, spawn) = queues.enqueue(first.clone(), last.clone());
    assert!(!spawn);
    queues.publish(&first, &active, ReadPhase::Confirming);
    assert_eq!(*following.borrow(), active.phase(ReadPhase::Confirming));
    assert!(queues.finish(&first, ReadRequestState::Idle));
    let next = queues.take(&first).unwrap();
    assert_eq!(next.boundary, last.boundary);
    assert_eq!(next.observed.len(), 3);
    assert!(!queues.finish(&first, ReadRequestState::Idle));
    assert_eq!(*outcome.borrow(), ReadRequestState::Idle);
}

/// Не смешивает два входа, поколения, темы и наблюдения до отзыва доступа.
#[test]
fn queue_preserves_owner_topic_and_scope_versions() {
    let queues = ReadRequests::default();
    let keys = [
        key("first", 1, 2),
        key("second", 1, 2),
        key("first", 2, 2),
        key("first", 1, 3),
    ];
    for key in &keys {
        let (_, spawn) = queues.enqueue(key.clone(), intent(key, 9, &[9], 0));
        assert!(spawn);
    }
    let first = &keys[0];
    let (_, spawn) = queues.enqueue(first.clone(), intent(first, 8, &[8], 1));
    assert!(!spawn);
    queues.enqueue(first.clone(), intent(first, 10, &[10], 0));

    assert_eq!(queues.take(first).unwrap().observed, [Uuid::from_u128(8)]);
    for key in &keys[1..] {
        assert_eq!(queues.take(key).unwrap().observed, [Uuid::from_u128(9)]);
    }
}

/// Сохраняет принятую сервером команду после паузы и не теряет следующую видимую границу.
#[test]
fn queue_retains_receipt_while_a_newer_boundary_waits() {
    let queues = ReadRequests::default();
    let first = key("first", 1, 2);
    queues.enqueue(first.clone(), intent(&first, 7, &[7], 0));
    let active = queues.take(&first).unwrap();
    queues.set_receipt(
        &first,
        Some(HeldRead {
            intent: active.clone(),
            ticket: MutationTicket::new(&first.owner, first.generation, Uuid::new_v4()),
            before: Storage::in_memory()
                .unwrap()
                .history_read_revision(&first.owner)
                .unwrap(),
        }),
    );
    let next = intent(&first, 8, &[7, 8], 0);
    let (screen, spawn) = queues.enqueue(first.clone(), next.clone());
    assert!(!spawn);
    assert!(!queues.finish(
        &first,
        ReadRequestState::Failed(active.boundary.target.message_id)
    ));
    assert_eq!(
        *screen.borrow(),
        ReadRequestState::Failed(active.boundary.target.message_id)
    );
    assert_eq!(
        queues.receipt(&first).unwrap().intent.boundary,
        active.boundary
    );
    let (_, spawn) = queues.enqueue(first.clone(), next.clone());
    assert!(spawn);
    assert_eq!(queues.take(&first).unwrap().boundary, next.boundary);
    assert!(queues.receipt(&first).is_some());
    queues.set_receipt(&first, None);
    assert!(!queues.finish(&first, ReadRequestState::Idle));
}

/// Возвращает безопасную младшую границу, если в наблюдаемом диапазоне есть пропуск.
#[test]
fn read_prefix_stops_before_unseen_or_unsupported_rows() {
    let first = key("first", 1, 2);
    let boundary = intent(&first, 9, &[7, 9], 0);
    let earlier = intent(&first, 7, &[7], 0).boundary;
    let missing = intent(&first, 8, &[8], 0).boundary;
    for eligible in [false, true] {
        let rows = vec![
            UnreadAnchor {
                boundary: earlier.clone(),
                eligible: true,
            },
            UnreadAnchor {
                boundary: missing.clone(),
                eligible,
            },
            UnreadAnchor {
                boundary: boundary.boundary.clone(),
                eligible: true,
            },
        ];
        let (safe, blocked) = observed_prefix(rows, &boundary);
        assert_eq!(safe.as_ref(), Some(&earlier));
        assert_eq!(blocked.unwrap().boundary, missing);
    }
}
