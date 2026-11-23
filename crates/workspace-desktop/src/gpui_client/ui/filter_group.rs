//! Ряд фильтров с общей скруглённой рамкой, горизонтальной прокруткой и фокусом.

use std::{cell::Cell, rc::Rc};

use gpui::{
    App, Div, ElementId, FocusHandle, Pixels, ScrollHandle, SharedString, Stateful, Window, div,
    point, prelude::*, px,
};

use super::{Button, ButtonAppearance, Palette, rounded_surface_corners, theme::CONTROL_RADIUS};

/// Помнит положение полосы и стабильный клавиатурный фокус её вкладок.
pub struct FilterGroup {
    scroll: ScrollHandle,              // Прокрутка внутреннего горизонтального ряда.
    focus: Vec<FocusHandle>,           // Один фокус на вкладку в порядке показа.
    last_focused: Cell<Option<usize>>, // None, если фокус ушёл из ряда.
    last_width: Rc<Cell<Pixels>>,      // Последняя ширина области прокрутки.
}

impl FilterGroup {
    /// Готовит клавиатурный фокус для известного числа вкладок.
    pub fn new(count: usize, app: &App) -> Self {
        Self {
            scroll: ScrollHandle::new(),
            focus: (0..count).map(|_| app.focus_handle()).collect(),
            last_focused: Cell::new(None),
            last_width: Rc::new(Cell::new(px(0.))),
        }
    }

    /// Возвращает стабильный фокус кнопки с указанным порядковым номером.
    pub fn focus(&self, index: usize) -> &FocusHandle {
        &self.focus[index]
    }

    /// Возвращает первую вкладку к началу полосы при закрытии экрана.
    pub(in crate::gpui_client) fn reset(&self) {
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.last_focused.set(None);
    }

    /// Оставляет один ряд, который прокручивается только при нехватке ширины.
    pub fn render(
        &self,
        palette: Palette,
        window: &Window,
        buttons: impl IntoIterator<Item = Stateful<Div>>,
    ) -> Div {
        let focused = self.focus.iter().position(|focus| focus.is_focused(window));
        if self.last_focused.replace(focused) != focused
            && let Some(index) = focused
        {
            self.scroll.scroll_to_item(index);
        }
        let scroll = self.scroll.clone();
        let width = self.last_width.clone();
        let focus = focused.map(|index| (index, self.focus[index].clone()));
        let strip = div()
            .on_children_prepainted(move |_, window, app| {
                let viewport = scroll.bounds().size.width;
                if width.replace(viewport) != viewport
                    && let Some((index, handle)) = focus.clone()
                {
                    let scroll = scroll.clone();
                    window.defer(app, move |window, _| {
                        if handle.is_focused(window) {
                            scroll.scroll_to_item(index);
                            window.refresh();
                        }
                    });
                }
            })
            .id("channel-filter-strip")
            .debug_selector(|| "channel-filter-strip".into())
            .w_full()
            .min_w_0()
            .flex()
            .gap(px(1.))
            .overflow_x_scroll()
            .track_scroll(&self.scroll)
            .children(buttons);
        div()
            .relative()
            .min_w_0()
            .rounded(CONTROL_RADIUS)
            .overflow_hidden()
            .bg(palette.border)
            .child(strip)
            .child(rounded_surface_corners(CONTROL_RADIUS, palette.surface))
    }
}

/// Рисует прямоугольную вкладку и отдаёт её действия общему контролу кнопки.
pub fn filter_button(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    selected: bool,
    palette: Palette,
    focus: Option<&FocusHandle>,
    on_activate: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let mut button = Button::new(id, title);
    button.appearance = ButtonAppearance::Ghost;
    button
        .render(palette, focus, on_activate)
        .flex_grow()
        .flex_shrink_0()
        .h(px(40.))
        .px(px(2.))
        .rounded(px(0.))
        .bg(if selected {
            palette.card_selected
        } else {
            palette.card
        })
        .when(selected, |button| {
            button.debug_selector(|| "filter-selected".into())
        })
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests;
