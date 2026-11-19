//! Измеряет запас истории у краёв и сообщает, в какую сторону нужна следующая страница.

use gpui::{Pixels, px};
use workspace_core::storage::{HistoryDirection, HistoryEdge};

use super::{BOTTOM_PADDING, Viewport};

/// Намерение списка, которое хост может превратить в запрос страницы.
#[derive(Clone, Copy, Debug)]
pub(in super::super) struct PageNeed {
    pub direction: HistoryDirection, // С какой стороны добавить сообщения.
    pub distance: Pixels,            // Сколько px до края, ноль при заполнении окна.
}

impl Viewport {
    /// Измеряет достаточно строк за краем, чтобы проверять запас до указанного предела.
    pub(in super::super) fn with_page_ahead(max_ahead: Pixels) -> Self {
        Self::with_overdraw(max_ahead.max(px(100.)) + BOTTOM_PADDING + px(1.))
    }

    /// Решает по геометрии последнего кадра, нужна ли следующая страница.
    pub(in super::super) fn page_need(
        &self,
        older: &HistoryEdge,
        newer: &HistoryEdge,
        direction: Option<HistoryDirection>,
        max_ahead: Pixels,
    ) -> Option<PageNeed> {
        use HistoryDirection::{Newer, Older};

        let bounds = self.list.viewport_bounds();
        if bounds.size.height <= px(0.) {
            return None;
        }
        if let Some(direction) = direction {
            let edge = match direction {
                Older => older,
                Newer => newer,
            };
            let distance = self.distance_to_edge(direction);
            return (matches!(edge, HistoryEdge::More(_))
                && distance <= bounds.size.height.min(max_ahead))
            .then_some(PageNeed {
                direction,
                distance,
            });
        }

        let rows = self.painted.borrow();
        // Проверяем фактические зазоры, включая смещение короткой истории к нижнему краю.
        let gap_below = self
            .rows
            .last()
            .and_then(|row| rows.get(&row.id))
            .is_some_and(|last| bounds.bottom() - last.bottom() > BOTTOM_PADDING + px(1.));
        let gap_above = self.rows.first().is_some_and(|row| {
            rows.get(&row.id)
                .is_some_and(|first| first.top() - row.inset() - bounds.top() > px(1.))
        });
        let direction = if gap_below && matches!(newer, HistoryEdge::More(_)) {
            Newer
        } else if gap_above && matches!(older, HistoryEdge::More(_)) {
            Older
        } else {
            return None;
        };
        Some(PageNeed {
            direction,
            distance: px(0.),
        })
    }

    /// Считает запас измеренных строк от видимой области до указанного края.
    fn distance_to_edge(&self, direction: HistoryDirection) -> Pixels {
        let offset = self.list.logical_scroll_top();
        let max_scroll = self.list.max_offset_for_scrollbar().height + BOTTOM_PADDING;
        let top = if offset.item_ix == self.rows.len() {
            max_scroll
        } else {
            -self.list.scroll_px_offset_for_scrollbar().y
        };
        // За пределами запаса GPUI ещё может не знать высоту строки. Уже измеренного
        // участка достаточно, чтобы не запрашивать страницу слишком рано.
        match direction {
            HistoryDirection::Older => top.max(px(0.)),
            HistoryDirection::Newer => (max_scroll - top).max(px(0.)),
        }
    }
}
