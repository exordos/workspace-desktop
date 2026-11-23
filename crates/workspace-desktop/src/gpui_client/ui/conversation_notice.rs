//! Тонкая плашка под историей разговора с пояснением и возможным действием.

use gpui::{App, Div, SharedString, Window, div, prelude::*, px};

use super::{Button, CONTROL_BORDER, Palette, TextRole, label};

/// Показывает пояснение экрана и передаёт ему нажатие кнопки.
pub struct ConversationNotice {
    pub text: SharedString,     // Пояснение, которое выбрал экран.
    pub action: Option<Button>, // Кнопка, если в этом состоянии доступно действие.
}

impl ConversationNotice {
    /// Рисует плашку и передаёт нажатие кнопки вызывающему экрану.
    pub fn render(
        self,
        palette: Palette,
        on_action: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Div {
        div()
            .debug_selector(|| "conversation-send-notice".into())
            .flex()
            // Переносим кнопку целиком, когда пояснение и кнопка не помещаются рядом.
            .flex_wrap()
            .flex_none()
            .items_center()
            .justify_center()
            .gap_2()
            .min_w_0()
            .min_h(px(40.))
            .px_3()
            .py_1()
            .border_t(CONTROL_BORDER)
            .border_color(palette.border)
            .bg(palette.surface)
            .child(
                label(palette, TextRole::Caption, self.text)
                    .flex_none()
                    .max_w_full()
                    .truncate()
                    .text_center()
                    .debug_selector(|| "conversation-send-notice-text".into()),
            )
            .children(self.action.map(|button| {
                let id = button.id.clone();
                button
                    .render(palette, None, on_action)
                    .flex_none()
                    .max_w_full()
                    .debug_selector(move || id.to_string())
            }))
    }
}
