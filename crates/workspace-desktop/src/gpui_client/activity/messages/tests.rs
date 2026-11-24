//! Проверяет пограничные случаи объединения серверных страниц активности.

use workspace_core::storage::HistoryTime;

use super::*;

/// Создаёт строку с фиксированным временем, чтобы проверить сортировку при совпадении.
fn row(id: u128) -> ActivityMessage {
    ActivityMessage {
        id: Uuid::from_u128(id),
        stream_id: Uuid::from_u128(90),
        topic_id: Uuid::from_u128(91),
        author_id: Uuid::from_u128(92),
        author_name: None,
        created_at: HistoryTime::parse("2026-07-16T12:30:00Z").unwrap(),
        preview: ActivityMessagePreview::Text(format!("Сообщение {id}").into()),
        read: false,
    }
}

/// Сохраняет одну строку по UUID и явный конец после следующей страницы.
#[test]
fn deduplicates_boundary_and_orders_same_timestamp_by_uuid() {
    let mut collection = Collection::default();
    let cursor = Uuid::from_u128(3);
    collection
        .append(ActivityMessagePage {
            messages: vec![row(3), row(2)],
            next: Some(cursor),
        })
        .unwrap();
    let mut duplicate = row(2);
    duplicate.read = true;
    collection
        .append(ActivityMessagePage {
            messages: vec![duplicate, row(1)],
            next: None,
        })
        .unwrap();
    assert_eq!(collection.messages, [row(3), row(2), row(1)]);
    assert_eq!(collection.next, None);
}

/// Повтор циклического маркера сохраняет прежний снимок для пользователя.
#[test]
fn invalid_marker_leaves_visible_rows_unchanged() {
    let mut collection = Collection::default();
    let cursor = Uuid::from_u128(3);
    collection
        .append(ActivityMessagePage {
            messages: vec![row(3)],
            next: Some(cursor),
        })
        .unwrap();
    assert!(
        collection
            .append(ActivityMessagePage {
                messages: vec![row(2)],
                next: Some(cursor),
            })
            .is_err()
    );
    assert_eq!(collection.messages, [row(3)]);
    assert_eq!(collection.next, Some(cursor));
}
