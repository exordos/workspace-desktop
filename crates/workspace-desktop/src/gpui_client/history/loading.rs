//! Заглушка на время открытия истории. Показывает силуэты пузырей в цветах текущей темы.

use gpui::{Div, div, prelude::*, px, relative};

use crate::gpui_client::ui::{Palette, SURFACE_RADIUS};

/// Рисует короткую цепочку у нижней границы, откуда обычно начинается чтение чата.
pub(super) fn skeleton(palette: Palette) -> Div {
    div()
        .debug_selector(|| "history-loading".into())
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .justify_end()
        .gap_3()
        .px_4()
        .py_5()
        .overflow_hidden()
        .children([
            bubble(palette, false, 0.58, &[0.83, 0.52]),
            bubble(palette, true, 0.39, &[0.72]),
            bubble(palette, false, 0.64, &[0.9, 0.76, 0.41]),
            bubble(palette, true, 0.5, &[0.86, 0.5]),
            bubble(palette, false, 0.46, &[0.68]),
        ])
}

/// Собирает силуэт сообщения из подложки и полос. Ширины заданы долями от родителя.
fn bubble(palette: Palette, own: bool, width: f32, lines: &[f32]) -> Div {
    let fill = if own {
        palette.message_own
    } else {
        palette.message
    };
    let mark = palette.text_muted.opacity(0.23);
    div()
        .flex()
        .flex_none()
        .items_end()
        .gap_2()
        .when(own, |row| row.justify_end())
        .when(!own, |row| {
            row.child(div().size_10().flex_none().rounded_full().bg(mark))
        })
        .child(
            div()
                .w(relative(width))
                .max_w(px(400.))
                .min_w_0()
                .rounded(SURFACE_RADIUS)
                .bg(fill.opacity(0.8))
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_3()
                .children(
                    lines
                        .iter()
                        .map(|&line| div().w(relative(line)).h_2().rounded_full().bg(mark)),
                ),
        )
}
