//! Удерживает строку простого текста при правке опорного сообщения. Ищет её в новой
//! версии и измеряет новое положение до подготовки кадра виртуального списка.

use std::{collections::HashSet, ops::Range};

use gpui::{App, AvailableSpace, IntoElement, Pixels, TextLayout, Window, point, px, size};

/// Положение текста, которое человек видел до правки.
pub(super) struct ReadingLine {
    layout: TextLayout,   // Разметка реально нарисованного текста с переносами.
    viewport_top: Pixels, // Верх списка в координатах того же кадра.
}

/// Байтовая позиция в новой версии и её целевая высота в области прокрутки.
#[derive(Clone, Copy)]
pub(super) struct LineTarget {
    pub(super) byte: usize, // Позиция внутри сохранившегося фрагмента нового текста.
    pub(super) y: Pixels,   // Высота верхней границы строки относительно верха списка.
}

impl ReadingLine {
    /// Берёт опору внутри текста, если верх области прокрутки пересекает этот текст.
    pub(super) fn at_top(layout: TextLayout, viewport_top: Pixels) -> Option<Self> {
        let bounds = layout.bounds();
        (bounds.top() <= viewport_top && viewport_top < bounds.bottom()).then_some(Self {
            layout,
            viewport_top,
        })
    }

    /// Ищет ту же строку. Если её удалили, проверяет соседние по расстоянию в прежнем кадре.
    pub(super) fn target(&self, new: &str) -> Option<LineTarget> {
        let old = self.layout.text();
        let wrapped = self.layout.wrapped_text();
        let lines = visual_ranges(&old, &wrapped)?;
        let line_height = self.layout.line_height();
        let text_top = self.layout.bounds().top();
        let visible = ((self.viewport_top - text_top) / line_height) as usize;
        if visible >= lines.len() {
            return None;
        }
        let y = text_top + line_height * visible as f32 - self.viewport_top;
        let (prefix, suffix) = shared_edges(&old, new);
        let mut missed = HashSet::new();
        let byte = std::iter::once(visible)
            .chain((1..lines.len()).flat_map(|distance| {
                // На одинаковом расстоянии продолжаем читать со строки ниже.
                [visible.checked_add(distance), visible.checked_sub(distance)]
                    .into_iter()
                    .flatten()
            }))
            .filter_map(|index| lines.get(index))
            .find_map(|range| {
                let text = old[range.clone()].trim();
                if text.is_empty() {
                    return None;
                }
                let start =
                    range.start + old[range.clone()].len() - old[range.clone()].trim_start().len();
                let end = start + text.len();
                if end <= prefix {
                    Some(start)
                } else if start >= old.len() - suffix {
                    Some(new.len() - (old.len() - start))
                } else if missed.contains(text) {
                    None
                } else {
                    let expected = if start.saturating_sub(prefix)
                        <= (old.len() - suffix).saturating_sub(end)
                    {
                        start
                    } else {
                        new.len().saturating_sub(old.len() - start)
                    };
                    let found = new
                        .match_indices(text)
                        .map(|(index, _)| index)
                        .min_by_key(|index| index.abs_diff(expected));
                    if found.is_none() {
                        missed.insert(text);
                    }
                    found
                }
            })
            // При полной замене текста начинаем с первой строки новой версии.
            .unwrap_or_else(|| new.len() - new.trim_start().len());
        // Граница мягкого переноса в GPUI относится к концу предыдущей
        // строки. Берём байт после первого символа нужного фрагмента.
        let byte = byte + new[byte..].chars().next()?.len_utf8();
        Some(LineTarget { byte, y })
    }
}

/// Переводит видимые строки GPUI в диапазоны исходного текста с учётом мягких переносов.
fn visual_ranges(old: &str, wrapped: &str) -> Option<Vec<Range<usize>>> {
    let mut start = 0usize;
    wrapped
        .split('\n')
        .map(|line| {
            let end = start.checked_add(line.len())?;
            if old.get(start..end)? != line {
                return None;
            }
            let range = start..end;
            start = end + usize::from(old.as_bytes().get(end) == Some(&b'\n'));
            Some(range)
        })
        .collect()
}

/// Находит общие края старого и нового текста по границам UTF-8.
fn shared_edges(old: &str, new: &str) -> (usize, usize) {
    let prefix = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    let suffix = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    (prefix, suffix)
}

impl LineTarget {
    /// Измеряет строку в той же ширине, что и у списка, и возвращает смещение внутри элемента.
    pub(super) fn measure(
        self,
        width: Pixels,
        window: &mut Window,
        app: &mut App,
        row: impl FnOnce(&mut App) -> (gpui::Div, Option<TextLayout>),
    ) -> Option<Pixels> {
        let (row, layout) = row(app);
        let layout = layout?;
        let mut row = row.into_any_element();
        // Такой же пробный проход GPUI делает при автопрокрутке. Хитбоксы пробной
        // строки удаляются из кадра, а окончательный список сразу рисуется со сдвигом.
        let result: Result<(), Option<Pixels>> = window.transact(|window| {
            row.prepaint_as_root(
                point(px(0.), px(0.)),
                size(AvailableSpace::Definite(width), AvailableSpace::MinContent),
                window,
                app,
            );
            Err(layout
                .position_for_index(self.byte)
                .map(|point| point.y - self.y))
        });
        result.err().flatten()
    }
}
