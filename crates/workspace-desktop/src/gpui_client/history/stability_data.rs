//! Собирает фиксированную историю и ввод для проверки нагрузки UI-11.

use uuid::Uuid;
use workspace_core::storage::HistoryMessage;

pub(super) const PAGED_ROWS: u128 = 50;
pub(super) const LAST_ROW: u128 = 650;
pub(super) const ANCHOR: Uuid = Uuid::from_u128(272);
pub(super) const GROWING_ROW: Uuid = Uuid::from_u128(271);
pub(super) const TOLERANCE_PX: f32 = 1.;
pub(super) const INPUT_COUNT: usize = 64;

/// Обновляет высоту синтетической строки над опорой, имитируя поздний размер вложения.
pub(super) fn grow_above(messages: &mut [HistoryMessage]) {
    let row = messages
        .iter_mut()
        .find(|row| row.id == GROWING_ROW)
        .unwrap();
    row.text
        .push_str(&"\nСтрока после получения размера изображения.".repeat(7));
}
