//! Связывает перетаскивание вкладок с полосой прокрутки и показывает место вставки.

use std::time::Duration;

use gpui::{
    App, Context, Div, EntityId, MouseButton, Pixels, Point, Render, Stateful, Task, WeakEntity,
    Window, div, prelude::*, px,
};
use uuid::Uuid;

use super::{Composer, Replies, Tab};
use crate::gpui_client::ui::{Palette, TextRole, label};

const EDGE_SCROLL_PERIOD: Duration = Duration::from_millis(16);
const EDGE_WIDTH: Pixels = px(20.);

/// Источник перетаскивания из конкретного редактора и версии набора вкладок.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ReplyDrag {
    editor: EntityId, // Идентификатор живого композера.
    tab: Uuid,        // Вкладка, за кнопку автора которой потянули.
    revision: u64,    // Версия вкладок на момент захвата.
}

/// Локальная ссылка на перетаскивание, которую нативная сцена держит до отпускания.
pub(super) struct ActiveDrag {
    source: ReplyDrag,                // Откуда началось перетаскивание.
    preview: WeakEntity<DragPreview>, // Живая копия вкладки, пока GPUI ведёт это перетаскивание.
    _edge_scroll: Task<()>,           // Прокрутка у края до отпускания или отмены.
}

/// Положение между вкладками до удаления переносимой вкладки.
pub(super) struct DropSlot {
    index: usize,   // Индекс вставки, включая позицию после последней вкладки.
    marker: Pixels, // Координата линии относительно видимой полосы.
}

/// Отображаемая под указателем копия кнопки автора.
struct DragPreview {
    title: String,    // Текст на кнопке.
    palette: Palette, // Цвета текущей темы окна.
}

impl Render for DragPreview {
    /// Показывает полупрозрачную подпись перетаскиваемой вкладки.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(32.))
            .max_w(px(180.))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(self.palette.border)
            .bg(self.palette.surface)
            .opacity(0.85)
            .child(
                label(self.palette, TextRole::Label, self.title.clone())
                    .whitespace_nowrap()
                    .overflow_hidden(),
            )
    }
}

impl Replies {
    /// Определяет позицию по серединам вкладок с учётом текущей прокрутки.
    pub(super) fn drop_slot(&self, position: Point<Pixels>) -> Option<DropSlot> {
        let viewport = self.scroll.bounds();
        if !viewport.contains(&position) {
            return None;
        }
        let offset = self.scroll.offset().x;
        let mut previous_right = None;
        for index in 0..self.tabs.len() {
            let bounds = self.scroll.bounds_for_item(index)?;
            if position.x < bounds.center().x + offset {
                let left = bounds.left() + offset;
                let x = previous_right.map_or(left, |right| (right + left) / 2.);
                return Some(DropSlot::at(
                    index,
                    x - viewport.left(),
                    viewport.size.width,
                ));
            }
            previous_right = Some(bounds.right() + offset);
        }
        Some(DropSlot::at(
            self.tabs.len(),
            previous_right? - viewport.left(),
            viewport.size.width,
        ))
    }
}

impl DropSlot {
    /// Удерживает отметку внутри видимого края даже у частично скрытой вкладки.
    fn at(index: usize, x: Pixels, width: Pixels) -> Self {
        Self {
            index,
            marker: (x - px(1.)).clamp(px(0.), (width - px(2.)).max(px(0.))),
        }
    }

    /// Рисует линию поверх полосы, не сдвигая вкладки при наведении.
    pub(super) fn indicator(&self, palette: Palette) -> Div {
        div()
            .absolute()
            .top(px(1.))
            .bottom(px(1.))
            .left(self.marker)
            .w(px(2.))
            .rounded_full()
            .bg(palette.accent)
            .debug_selector(|| "composer-reply-drop-indicator".into())
    }
}

impl Composer {
    /// Проверяет, существует ли перетаскивание из этого композера.
    pub(super) fn active_reply_drag(&self, app: &App) -> Option<Uuid> {
        let drag = self.replies.drag.as_ref()?;
        (app.has_active_drag()
            && drag.preview.upgrade().is_some()
            && drag.source.revision == self.replies.revision
            && !self.editor.read(app).composing())
        .then_some(drag.source.tab)
    }

    /// Стартует нативное перетаскивание только с кнопки автора.
    pub(super) fn reply_drag_handle(
        &self,
        button: Stateful<Div>,
        tab: &Tab,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        if self.replies.tabs.len() < 2 || self.editor.read(cx).composing() {
            return button;
        }
        let source = ReplyDrag {
            editor: cx.entity_id(),
            tab: tab.id,
            revision: self.replies.revision,
        };
        let target = cx.entity().downgrade();
        let title = tab.quote.author.clone();
        button.on_drag(source, move |source, _, window, app| {
            let preview = app.new(|_| DragPreview {
                title: title.clone(),
                palette,
            });
            let editor = target.clone();
            let edge_scroll = window.spawn(app, async move |cx| {
                loop {
                    cx.background_executor().timer(EDGE_SCROLL_PERIOD).await;
                    let running = cx
                        .update(|window, app| {
                            editor
                                .update(app, |composer, cx| composer.scroll_reply_drag(window, cx))
                                .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    if !running {
                        break;
                    }
                }
            });
            let _ = target.update(app, |composer, cx| {
                composer.replies.drag = Some(ActiveDrag {
                    source: *source,
                    preview: preview.downgrade(),
                    _edge_scroll: edge_scroll,
                });
                cx.notify();
            });
            preview
        })
    }

    /// Прокручивает полосу, пока указатель с вкладкой удерживают у видимого края.
    fn scroll_reply_drag(&self, window: &Window, cx: &mut Context<Self>) -> bool {
        if self.active_reply_drag(cx).is_none() {
            return false;
        }
        let scroll = &self.replies.scroll;
        let viewport = scroll.bounds();
        let cursor = window.mouse_position();
        if viewport.contains(&cursor) {
            let band = EDGE_WIDTH.min(viewport.size.width / 2.);
            let left = (band - (cursor.x - viewport.left())).max(px(0.));
            let right = (band - (viewport.right() - cursor.x)).max(px(0.));
            let mut offset = scroll.offset();
            let next = (offset.x + (left - right) * 0.5).clamp(-scroll.max_offset().width, px(0.));
            if offset.x != next {
                offset.x = next;
                scroll.set_offset(offset);
                cx.notify();
            }
        }
        true
    }

    /// Принимает перенос внутри этой полосы и обрабатывает отмену.
    pub(super) fn reply_drag_target(
        &self,
        bar: Stateful<Div>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let editor = cx.entity_id();
        bar.on_drag_move(cx.listener(
            move |composer, event: &gpui::DragMoveEvent<ReplyDrag>, _, cx| {
                if event.drag(cx).editor == editor && composer.replies.drag.is_some() {
                    cx.notify();
                }
            },
        ))
        .capture_any_mouse_up(
            cx.listener(|composer, event: &gpui::MouseUpEvent, window, cx| {
                if event.button != MouseButton::Left {
                    composer.cancel_reply_drag(window, cx);
                }
            }),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|composer, _: &gpui::MouseUpEvent, window, cx| {
                composer.cancel_reply_drag(window, cx);
            }),
        )
        .capture_key_down(
            cx.listener(|composer, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && composer.cancel_reply_drag(window, cx) {
                    cx.stop_propagation();
                }
            }),
        )
        .on_drop(
            cx.listener(move |composer, source: &ReplyDrag, window, cx| {
                if source.editor != editor {
                    return;
                }
                let active = composer.replies.drag.take();
                // На отпускании используем фактическую позицию указателя и последнюю раскладку.
                if active.is_some_and(|drag| drag.source == *source)
                    && source.revision == composer.replies.revision
                    && let Some(slot) = composer.replies.drop_slot(window.mouse_position())
                    && composer.move_reply(source.tab, slot.index, cx)
                {
                    // При переносе оставляем видимым текущий участок полосы, где был указатель.
                    let replies = &composer.replies;
                    replies.last_reveal.set(Some((
                        replies.reveal_index(window),
                        replies.revision,
                        replies.scroll.bounds().size.width,
                    )));
                } else {
                    cx.notify();
                }
            }),
        )
    }

    /// Снимает только перетаскивание данной полосы и возвращает обычную обработку клавиш.
    fn cancel_reply_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self
            .replies
            .drag
            .take()
            .and_then(|drag| drag.preview.upgrade())
            .is_none()
        {
            return false;
        }
        cx.notify();
        cx.stop_active_drag(window)
    }
}
