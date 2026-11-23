//! Варианты плашки разговора, которые галерея показывает в обеих палитрах.

use gpui::{App, Div, Window, div, prelude::*};
use workspace_desktop::gpui_client::ui::{Button, ConversationNotice, Palette};

/// Показывает постоянные состояния и кнопку повтора с локальной фазой проверки.
pub(super) fn examples(
    palette: Palette,
    retrying: bool,
    on_retry: impl Fn(&mut Window, &mut App) + 'static,
) -> Div {
    const CHECKING: &str = "Проверяем доступ к отправке…";
    let retry = ConversationNotice {
        text: if retrying {
            CHECKING
        } else {
            "Не удалось проверить доступ к отправке"
        }
        .into(),
        action: (!retrying).then(|| Button::new("gallery-send-retry", "Повторить проверку")),
    };

    div()
        .flex()
        .flex_col()
        .gap_2()
        .children(
            [CHECKING, "Эта тема решена", "Отправка сообщений недоступна"].map(|text| {
                ConversationNotice {
                    text: text.into(),
                    action: None,
                }
                .render(palette, |_, _| {})
            }),
        )
        .child(retry.render(palette, on_retry))
}
