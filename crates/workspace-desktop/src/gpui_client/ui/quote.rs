//! Рисует общую рамку цитаты для истории и короткий предпросмотр для композера.

use gpui::{Div, SharedString, div, prelude::*, px};

use super::{Palette, TextRole, ellipsized_label, label, theme::CONTROL_RADIUS};

/// Выбирает фон рамки там, где экран показывает цитату.
#[derive(Clone, Copy)]
pub enum QuoteSurface {
    Message,       // Нейтральная подложка цитаты в сообщении.
    TintedMessage, // Акцентная подложка для разделения уровней вложенности.
    Composer,      // Та же поверхность, что и у композера под предпросмотром.
}

/// Рисует автора и тело цитаты. Тело может содержать текст и другие рамки.
pub fn quote_frame(
    palette: Palette,
    surface: QuoteSurface,
    heading: impl Into<SharedString>,
    body: impl IntoElement,
) -> Div {
    let heading = heading.into().replace(['\r', '\n'], " ");
    // Берём контрастный оттенок Blue Mist, чтобы подпись читалась на всех трёх подложках.
    let title_palette = Palette {
        text: palette.focus,
        ..palette
    };
    quote_frame_with_header(
        palette,
        surface,
        ellipsized_label(title_palette, TextRole::Label, heading)
            .debug_selector(|| "quote-frame-heading".into()),
        body,
    )
}

/// Оформляет рамку для цитаты с готовой строкой заголовка.
pub(in crate::gpui_client) fn quote_frame_with_header(
    palette: Palette,
    surface: QuoteSurface,
    heading: impl IntoElement,
    body: impl IntoElement,
) -> Div {
    let fill = match surface {
        QuoteSurface::Message => palette.field,
        QuoteSurface::TintedMessage => palette.accent_soft,
        QuoteSurface::Composer => palette.surface,
    };
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .max_w_full()
        .gap(px(4.))
        .p(px(8.))
        .rounded(CONTROL_RADIUS)
        .border_l_2()
        .border_color(palette.accent)
        .bg(fill)
        .child(heading)
        .child(body)
}

/// Показывает готовый текст ответа над полем. Состав и источник фрагмента задаёт экран.
pub fn quote_preview(
    palette: Palette,
    heading: impl Into<SharedString>,
    snippet: impl Into<SharedString>,
) -> Div {
    let snippet = snippet.into();
    let body = div().min_w_0().when(!snippet.is_empty(), |body| {
        // Ограничение высоты учитывает и переносы, и явные переводы строки в тексте.
        body.child(
            label(palette, TextRole::Body, snippet)
                .line_clamp(2)
                .max_h(px(40.)),
        )
    });
    quote_frame(palette, QuoteSurface::Composer, heading, body)
}
