//! Рисует всплывающие меню и подсказки. Экран решает, какие действия доступны.

use gpui::{
    AnyView, App, AppContext, Context, Deferred, Div, FocusHandle, KeyBinding, KeyUpEvent, Pixels,
    Point, Render, SharedString, Size, Stateful, Window, actions, anchored, deferred, div, point,
    prelude::*, px,
};

use super::{PANEL_RADIUS, Palette, SURFACE_RADIUS, TextRole, label};

actions!(
    workspace_popup,
    [Previous, Next, First, Last, Close, KeepFocus]
);

const MENU_CONTEXT: &str = "WorkspacePopupMenu";
const MARGIN: Pixels = px(4.);

/// Намерение клавиатуры внутри уже открытого меню.
#[derive(Clone, Copy)]
pub(in crate::gpui_client) enum MenuInput {
    Previous, // Перейти к предыдущему доступному действию.
    Next,     // Перейти к следующему доступному действию.
    First,    // Перейти к первому доступному действию.
    Last,     // Перейти к последнему доступному действию.
    Select,   // Выполнить подсвеченное действие.
    Close,    // Закрыть список и вернуть фокус.
}

/// Регистрирует клавиши только для области открытого меню.
pub(super) fn bind_menu_keys(app: &mut App) {
    let context = Some(MENU_CONTEXT);
    app.bind_keys([
        KeyBinding::new("up", Previous, context),
        KeyBinding::new("down", Next, context),
        KeyBinding::new("home", First, context),
        KeyBinding::new("end", Last, context),
        KeyBinding::new("escape", Close, context),
        KeyBinding::new("tab", KeepFocus, context),
        KeyBinding::new("shift-tab", KeepFocus, context),
    ]);
}

/// Оставляет клавиши у меню, пока экран рисует его поверх остального окна.
pub(in crate::gpui_client) fn menu_navigation<V: 'static>(
    root: Stateful<Div>,
    focus: &FocusHandle,
    cx: &Context<V>,
    input: fn(&mut V, MenuInput, &mut Window, &mut Context<V>),
) -> Stateful<Div> {
    root.track_focus(focus)
        .key_context(MENU_CONTEXT)
        .on_action(cx.listener(move |view, _: &Previous, window, cx| {
            input(view, MenuInput::Previous, window, cx);
        }))
        .on_action(cx.listener(move |view, _: &Next, window, cx| {
            input(view, MenuInput::Next, window, cx);
        }))
        .on_action(cx.listener(move |view, _: &First, window, cx| {
            input(view, MenuInput::First, window, cx);
        }))
        .on_action(cx.listener(move |view, _: &Last, window, cx| {
            input(view, MenuInput::Last, window, cx);
        }))
        .on_action(cx.listener(move |view, _: &Close, window, cx| {
            input(view, MenuInput::Close, window, cx);
        }))
        .on_key_up(cx.listener(move |view, event: &KeyUpEvent, window, cx| {
            // Подтверждаем на отпускании, чтобы одна клавиша не активировала
            // и действие меню, и кнопку, которой мы сразу вернём фокус.
            if matches!(event.keystroke.key.as_str(), "enter" | "space")
                && !event.keystroke.modifiers.modified()
            {
                cx.stop_propagation();
                input(view, MenuInput::Select, window, cx);
            }
        }))
        .on_action(|_: &KeepFocus, _, cx| cx.stop_propagation())
}

/// Ограничивает размер меню доступной областью, оставляя у окна свободный край.
pub(in crate::gpui_client) fn menu_surface(
    id: &'static str,
    palette: Palette,
    viewport: Size<Pixels>,
    width: Pixels,
    max_height: Pixels,
) -> Stateful<Div> {
    div()
        .id(id)
        .w(width.min((viewport.width - MARGIN * 2.).max(px(0.))))
        .max_h(max_height.min((viewport.height - MARGIN * 2.).max(px(0.))))
        .p_1()
        .flex()
        .flex_col()
        .rounded(PANEL_RADIUS)
        .border_1()
        .border_color(palette.border)
        .bg(palette.surface)
        .shadow_md()
        .occlude()
}

/// Кладёт меню над прозрачным слоем, который блокирует управление окном под ним.
pub(in crate::gpui_client) fn menu_layer(
    viewport: Size<Pixels>,
    anchor: Point<Pixels>,
    surface: Stateful<Div>,
) -> [Deferred; 2] {
    let backdrop = div().w(viewport.width).h(viewport.height).occlude();
    [
        deferred(anchored().position(point(px(0.), px(0.))).child(backdrop)).with_priority(1),
        deferred(
            anchored()
                .position(anchor)
                .snap_to_window_with_margin(MARGIN)
                .child(surface),
        )
        .with_priority(2),
    ]
}

/// Размещает вложенное меню поверх основного без второго блокирующего слоя.
pub(in crate::gpui_client) fn submenu_layer(
    anchor: Point<Pixels>,
    surface: Stateful<Div>,
) -> Deferred {
    deferred(
        anchored()
            .position(anchor)
            .snap_to_window_with_margin(MARGIN)
            .child(surface),
    )
    .with_priority(3)
}

/// Показывает полную подпись на фоне всплывающих панелей.
struct Tooltip {
    palette: Palette,   // Цвета текущего окна.
    text: SharedString, // Подпись, которую не нужно сокращать по ширине кнопки.
}

impl Render for Tooltip {
    /// Ограничивает ширину подсказки доступным местом.
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        label(self.palette, TextRole::Caption, self.text.clone())
            .max_w(px(320.).min((window.viewport_size().width - MARGIN * 2.).max(px(0.))))
            .px_2()
            .py_1()
            .rounded(SURFACE_RADIUS)
            .border_1()
            .border_color(self.palette.border)
            .bg(self.palette.surface)
    }
}

/// Создаёт всплывающую подпись и передаёт GPUI управление её показом.
pub(in crate::gpui_client) fn tooltip(
    palette: Palette,
    text: SharedString,
    app: &mut App,
) -> AnyView {
    app.new(|_| Tooltip { palette, text }).into()
}
