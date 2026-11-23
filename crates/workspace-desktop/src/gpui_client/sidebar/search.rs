//! Собирает поиск каталога из общего однострочного поля, значка и кнопки очистки.

use gpui::{App, Div, Entity, KeyBinding, Window, div, prelude::*, px};

use crate::gpui_client::ui::{
    Button, ButtonAppearance, FieldAppearance, Icon, Palette, SEARCH_ICON_SIZE, TextInput, field,
};

gpui::actions!(catalog_search, [Clear]);

/// Регистрирует ввод поля и отдельный Escape для поиска.
pub(super) fn bind_keys(app: &mut App) {
    TextInput::bind_keys(app);
    app.bind_keys([KeyBinding::new("escape", Clear, Some("CatalogSearch"))]);
}

/// Показывает однострочный поиск со значком или кнопкой очистки.
pub(super) fn render(
    search: &Entity<TextInput>,
    palette: Palette,
    window: &Window,
    app: &App,
) -> Div {
    let input = search.read(app);
    let appearance = if input.is_focused(window) {
        FieldAppearance::Focused
    } else {
        FieldAppearance::Normal
    };
    let clear_input = search.downgrade();
    let clear_button_input = clear_input.clone();
    let mut control = div()
        .id("catalog-search-control")
        .key_context("CatalogSearch")
        .on_action(move |_: &Clear, window, app| clear(&clear_input, window, app))
        .flex()
        .w_full()
        .min_w_0()
        .items_center()
        .gap_2()
        .child(div().flex_1().min_w_0().child(search.clone()));
    if input.text().is_empty() {
        control = control.child(Icon::Search.element(SEARCH_ICON_SIZE, palette.icon));
    } else {
        let mut button = Button::new("catalog-clear", "");
        button.appearance = ButtonAppearance::Ghost;
        button.icon = Some(Icon::Close);
        control = control.child(
            button
                .render(palette, None, move |window, app| {
                    clear(&clear_button_input, window, app);
                })
                .h(px(24.))
                .min_w(px(24.))
                .px(px(3.))
                .tooltip(move |_, app| {
                    let locale = crate::gpui_client::preferences::Preferences::read(app).language;
                    crate::gpui_client::ui::popup::tooltip(
                        palette,
                        locale.text("Очистить поиск", "Clear search").into(),
                        app,
                    )
                })
                .debug_selector(|| "catalog-clear".into()),
        );
    }
    field(palette, appearance, control)
}

/// Очищает запрос и возвращает ввод в поиск.
fn clear(search: &gpui::WeakEntity<TextInput>, window: &mut Window, app: &mut App) {
    let _ = search.update(app, |input, cx| {
        input.clear(cx);
        input.focus(window);
    });
}
