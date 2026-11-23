//! Карточка модального диалога с прокручиваемым содержимым и закреплёнными действиями.

use gpui::{Div, ElementId, IntoElement, SharedString, Stateful, Window, div, prelude::*, px};

use super::{
    Palette, TextRole, label,
    theme::{CONTROL_BORDER, SURFACE_RADIUS},
};

/// Рисует карточку внутри общего модального хоста окна.
pub fn dialog_card(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    body: impl IntoElement,
    footer: impl IntoElement,
    palette: Palette,
    window: &Window,
) -> Stateful<Div> {
    let viewport = window.viewport_size();
    div()
        .id(id)
        .w(px(351.).min((viewport.width - px(24.)).max(px(0.))))
        .max_h(px(652.).min((viewport.height - px(24.)).max(px(0.))))
        .flex()
        .flex_col()
        .gap(px(16.))
        .p(px(12.))
        .rounded(SURFACE_RADIUS)
        .border(CONTROL_BORDER)
        .border_color(palette.border)
        .bg(palette.surface)
        .shadow_md()
        .child(
            label(palette, TextRole::Heading, title)
                .flex_none()
                .debug_selector(|| "dialog-card-title".into()),
        )
        .child(
            div()
                .id("dialog-card-body")
                .min_h_0()
                .flex_shrink()
                .overflow_y_scroll()
                .child(body),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_end()
                .gap(px(8.))
                .child(footer)
                .debug_selector(|| "dialog-card-footer".into()),
        )
}
