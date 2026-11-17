//! Собирает фиксированную историю и ввод для проверки нагрузки UI-11.

use uuid::Uuid;
use workspace_core::storage::{HistoryMessage, HistoryMetadata, HistoryScope, HistoryTime};

pub(super) const PAGED_ROWS: u128 = 50;
pub(super) const LAST_ROW: u128 = 650;
pub(super) const ANCHOR: Uuid = Uuid::from_u128(272);
pub(super) const GROWING_ROW: Uuid = Uuid::from_u128(271);
pub(super) const TOLERANCE_PX: f32 = 1.;
const SCOPE_TOPIC: Uuid = Uuid::from_u128(2002);
pub(super) const INPUT_COUNT: usize = 64;

/// Создаёт строки разной высоты для одного разговора, включая два дня и смену автора.
pub(super) fn fixture_rows(scope: HistoryScope, start: u128, end: u128) -> Vec<HistoryMessage> {
    (start..=end)
        .map(|number| HistoryMessage {
            id: Uuid::from_u128(number),
            stream_id: scope.stream_id,
            topic_id: SCOPE_TOPIC,
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

/// Обновляет высоту синтетической строки над опорой, имитируя поздний размер вложения.
pub(super) fn grow_above(messages: &mut [HistoryMessage]) {
    let row = messages
        .iter_mut()
        .find(|row| row.id == GROWING_ROW)
        .unwrap();
    row.text
        .push_str(&"\nСтрока после получения размера изображения.".repeat(7));
}
