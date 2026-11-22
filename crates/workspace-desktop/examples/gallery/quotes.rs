//! Показывает рамки цитат в сообщениях и короткий предпросмотр в композере.

use gpui::{Div, div, prelude::*, px};
use workspace_desktop::gpui_client::ui::{
    Palette, QuoteSurface, TextRole, label, quote_frame, quote_preview,
};

/// Собирает короткую цитату, соседние цитаты и два уровня вложенности.
pub(super) fn messages(palette: Palette) -> Div {
    let source = quote_frame(
        palette,
        QuoteSurface::Message,
        "Павел",
        label(palette, TextRole::Body, "Обновил сценарий встречи."),
    );
    let nested = quote_frame(
        palette,
        QuoteSurface::TintedMessage,
        "Мария",
        div().flex().flex_col().gap_1().child(source).child(label(
            palette,
            TextRole::Body,
            "Проверю его до встречи.",
        )),
    );

    div()
        .flex()
        .flex_col()
        .gap_2()
        .min_w_0()
        .child(
            div()
                .max_w(px(320.))
                .flex()
                .flex_col()
                .gap_2()
                .p(px(12.))
                .rounded(px(8.))
                .bg(palette.message)
                .child(quote_frame(
                    palette,
                    QuoteSurface::Message,
                    "Мария",
                    label(palette, TextRole::Body, "Проверю его до встречи."),
                ))
                .child(label(palette, TextRole::Body, "Спасибо!")),
        )
        .child(
            div().flex().justify_end().child(
                div()
                    .max_w(px(320.))
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(palette.message_own)
                    .child(nested)
                    .child(quote_frame(
                        palette,
                        QuoteSurface::Message,
                        "Елена",
                        label(palette, TextRole::Body, "Согласовала время."),
                    ))
                    .child(label(palette, TextRole::Body, "Вижу оба сообщения.")),
            ),
        )
}

/// Показывает длинный фрагмент в том же месте, где он появится при ответе.
pub(super) fn composer_preview(palette: Palette) -> Div {
    quote_preview(
        palette,
        "Ответить: Мария Петрова",
        "Подготовила план встречи и приложила примеры.\nПроверьте второй пример, пожалуйста.\nСоберём вопросы перед встречей.",
    )
}
