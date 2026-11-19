//! Даёт сообщения с постоянными UUID и разной высотой для стенда истории и тестов GPUI.

use uuid::Uuid;
use workspace_core::storage::{HistoryMessage, HistoryMetadata, HistoryScope, HistoryTime};

/// Создаёт строки одного разговора, включая смену дня и автора.
pub(super) fn fixture_rows(scope: HistoryScope, start: u128, end: u128) -> Vec<HistoryMessage> {
    (start..=end)
        .map(|number| HistoryMessage {
            id: Uuid::from_u128(number),
            stream_id: scope.stream_id,
            topic_id: Uuid::from_u128(2002),
            text: "Синтетическая строка для переноса. ".repeat(1 + (number % 5) as usize),
            metadata: HistoryMetadata {
                author_id: Some(Uuid::from_u128(if number % 4 < 2 { 900 } else { 901 })),
                created_at: HistoryTime::parse(if number < 271 {
                    "2026-11-16T07:10:30Z"
                } else {
                    "2026-11-17T07:10:30Z"
                })
                .unwrap(),
                updated_at: None,
            },
        })
        .collect()
}
