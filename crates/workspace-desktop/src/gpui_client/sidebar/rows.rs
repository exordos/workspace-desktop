//! Рисует строки каталога, подложку канала и управление списком его тем.
//! Родитель передаёт метаданные, фокус и действия для каждой области строки.

use super::protected_previews::SecurePreview;
use gpui::{
    App, Div, ElementId, FocusHandle, Hsla, ImageSource, MouseButton, SharedString, Stateful,
    Transformation, Window, div, prelude::*, px, radians, rgb,
};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

use crate::gpui_client::ui::{
    Avatar, AvatarPresence, AvatarSize, BadgeTone, ChatRow, ChatRowPreview, Icon, PANEL_RADIUS,
    Palette, TextRole, activate, ellipsized_label, label, popup::tooltip, unread_badge,
};

/// Имя группы наведения для кнопки раскрытия внутри канала.
const STREAM_HOVER_GROUP: &str = "catalog-stream-row";

/// Готовые визуальные сведения строки каталога.
#[derive(Clone, Default)]
pub(super) struct ChatRowDetails {
    pub(super) preview: Option<ChatRowPreview>, // Превью последнего сообщения.
    pub(super) protected_preview: Option<gpui::Entity<SecurePreview>>, // Глифы держит дочернее представление.
    pub(super) time: Option<SharedString>, // Подпись времени последнего сообщения.
    pub(super) unread: Option<(u64, BadgeTone)>, // Число и стиль бейджа этого разговора.
    pub(super) mention: bool,              // Есть личное упоминание.
    pub(super) presence: AvatarPresence,   // Статус собеседника, если известен.
    pub(super) image: Option<ImageSource>, // Готовое фото собеседника в личном чате.
    pub(super) pinned: bool,               // Закрепление в папке, в которой показана строка.
    pub(super) muted: bool,                // Строка приглушается из-за режима уведомлений.
    pub(super) compact: bool,              // Уменьшает строку и скрывает превью и время.
    pub(super) hide_avatar: bool,          // Прячет аватар, если он не служит отдельным переходом.
}

/// Действие, которое оболочка связывает с состоянием каталога.
pub(super) type RowAction = Box<dyn Fn(&mut Window, &mut App)>;

/// Области канала могут вести к разным темам или переключать раскрытие.
pub(super) struct ChannelActions {
    pub(super) open: Option<RowAction>, // Открывает основной маршрут канала.
    pub(super) toggle: RowAction,       // Разворачивает или сворачивает список тем.
    pub(super) preview: Option<PreviewAction>, // Открывает тему последнего сообщения.
    pub(super) avatar: Option<(FocusHandle, RowAction)>, // Фокус и действие для перехода в общую тему.
}

/// Отдельная область перехода к теме последнего сообщения.
pub(super) struct PreviewAction {
    pub(super) id: Uuid, // UUID сообщения. При смене сообщения нажатие прежней кнопки теряется.
    pub(super) focus: FocusHandle, // Фокус области, который восстанавливает родитель.
    pub(super) on_open: RowAction, // Открывает тему, к которой относится превью.
}

/// Рисует личный чат общим компонентом, сохраняя геометрию каталога.
pub(super) fn direct_row(
    id: Uuid,
    title: SharedString,
    details: ChatRowDetails,
    selected: bool,
    palette: Palette,
    focus: Option<&FocusHandle>,
    on_open: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let mut avatar = row_avatar(&title, details.presence);
    avatar.image = details.image;
    if details.compact {
        avatar.size = AvatarSize::Small;
        let row = compact_stream(row_surface(ElementId::Uuid(id), palette, selected))
            .bg(if selected {
                palette.card_selected
            } else {
                palette.card
            })
            .children((!details.hide_avatar).then(|| avatar.render(palette)))
            .child(ChatRow::text(palette, title, None))
            .children(
                ChatRow::metadata(
                    palette,
                    None,
                    details.unread,
                    details.mention,
                    details.pinned.then(|| pin(palette)),
                )
                .map(|column| column.min_w(px(32.))),
            )
            .when(details.muted, |row| row.opacity(0.7))
            .debug_selector(move || format!("catalog-chat-{id}"));
        return activate(row, focus, palette, on_open);
    }
    ChatRow {
        id: ElementId::Uuid(id),
        avatar,
        title,
        preview: details.preview,
        time: details.time,
        unread: details.unread,
        mention: details.mention,
        selected,
        disabled: false,
    }
    .render_with_caption(
        palette,
        focus,
        details.pinned.then(|| pin(palette)),
        details
            .protected_preview
            .map(|preview| preview.into_any_element()),
        on_open,
    )
    .min_h(px(64.))
    .when(details.muted, |row| row.opacity(0.7))
    .debug_selector(move || format!("catalog-chat-{id}"))
}

/// Задаёт плотные внутренние отступы для строки канала или личного чата.
fn compact_stream(row: Stateful<Div>) -> Stateful<Div> {
    row.min_h(px(32.)).gap_2().px(px(7.)).py(px(5.))
}

/// Рисует общую подложку для заголовка канала, его тем и кнопки раскрытия.
pub(super) fn channel_card(palette: Palette) -> Div {
    div()
        .w_full()
        .min_w_0()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(2.))
        .rounded(PANEL_RADIUS)
        .overflow_hidden()
        .bg(palette.card)
}

/// Данные верхней строки карточки канала.
pub(super) struct ChannelRow {
    pub(super) id: Uuid,                // UUID канала.
    pub(super) title: SharedString,     // Название канала.
    pub(super) details: ChatRowDetails, // Последнее сообщение и индикаторы.
    pub(super) selected: bool,          // Подсветка маршрута, выбранная экраном.
    pub(super) color: Option<u32>,      // Цвет аватара в RGB.
    pub(super) expanded: Option<bool>,  // None скрывает кнопку раскрытия.
}

impl ChannelRow {
    /// Рисует заголовок и отдельные действия. Контекстное меню добавляет родитель.
    pub(super) fn render(
        self,
        palette: Palette,
        focus: Option<&FocusHandle>,
        toggle_focus: Option<&FocusHandle>,
        actions: ChannelActions,
    ) -> Stateful<Div> {
        let ChannelActions {
            open,
            toggle,
            preview,
            avatar,
        } = actions;
        let id = self.id;
        let mut icon = row_avatar(&self.title, self.details.presence);
        if self.details.compact {
            icon.size = AvatarSize::Small;
        }
        let icon = icon.render_with_background(palette, sidebar_color(self.color));
        let avatar = div()
            .relative()
            .size(px(if self.details.compact { 32. } else { 40. }))
            .flex_none()
            .child(icon)
            .children(avatar.map(|(focus, action)| {
                let text: SharedString = format!("Общий чат: {}", self.title).into();
                // Отдельный маршрут накрывает существующий аватар целиком.
                inner_action(
                    "avatar".into(),
                    palette,
                    palette.card_hover.opacity(0.25),
                    Some(&focus),
                    action,
                )
                .absolute()
                .inset_0()
                .rounded_full()
                .tooltip(move |_, app| tooltip(palette, text.clone(), app))
                .debug_selector(move || format!("catalog-stream-avatar-{id}"))
            }));
        let title = ellipsized_label(palette, TextRole::Label, clean_text(&self.title))
            .debug_selector(|| "chat-row-title".into());
        let body = div()
            .flex()
            .flex_col()
            .justify_between()
            .flex_1()
            .min_w_0()
            .child(title)
            .children(
                self.details
                    .preview
                    .filter(|_| !self.details.compact)
                    .map(|message| {
                        let caption =
                            preview_label(palette, message, self.details.protected_preview);
                        match preview {
                            Some(preview) => inner_action(
                                ElementId::Uuid(preview.id),
                                palette,
                                palette.card,
                                Some(&preview.focus),
                                preview.on_open,
                            )
                            .w_full()
                            .min_w_0()
                            .child(caption)
                            .debug_selector(move || format!("catalog-stream-preview-{id}"))
                            .into_any_element(),
                            None => caption,
                        }
                    }),
            );
        let action = self.expanded.map(|expanded| {
            expand_slot(
                id,
                palette,
                self.details.unread,
                toggle_focus,
                expanded,
                self.selected,
                toggle,
            )
        });
        let unread = if action.is_some() {
            None
        } else {
            self.details.unread
        };
        let action = if self.details.pinned {
            Some(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .flex_none()
                    .child(pin(palette))
                    .children(action),
            )
        } else {
            action
        };
        let row = row_surface(ElementId::Uuid(id), palette, self.selected)
            .group(STREAM_HOVER_GROUP)
            .min_h(px(64.))
            .px(px(9.))
            .py(px(9.))
            .when(self.details.compact, compact_stream)
            .children((!self.details.hide_avatar).then_some(avatar))
            .child(body)
            .children(
                ChatRow::metadata(
                    palette,
                    self.details.time.filter(|_| !self.details.compact),
                    unread,
                    self.details.mention,
                    action,
                )
                .map(|column| column.when(self.details.compact, |column| column.min_w(px(32.)))),
            )
            .when(self.details.muted, |row| row.opacity(0.7))
            .debug_selector(move || format!("catalog-chat-{id}"));
        row.when_some(open, |row, open| activate(row, focus, palette, open))
    }
}

/// Данные вложенной темы. Цвет отсутствует, если источник его не передал.
pub(super) struct TopicRow {
    pub(super) stream_id: Uuid,         // UUID родительского канала.
    pub(super) id: Uuid,                // UUID темы.
    pub(super) title: SharedString,     // Название темы с решёткой или без неё.
    pub(super) details: ChatRowDetails, // Превью, время и индикаторы темы.
    pub(super) selected: bool,          // Тема открыта в окне.
    pub(super) color: Option<u32>,      // Цвет полоски в RGB.
    pub(super) completed: bool,         // Заголовок зачёркивается у завершённой темы.
}

impl TopicRow {
    /// Рисует тему внутри карточки канала и открывает её по клику или Enter.
    pub(super) fn render(
        self,
        palette: Palette,
        focus: Option<&FocusHandle>,
        on_open: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        let selector = format!("catalog-topic-{}-{}", self.stream_id, self.id);
        let title = topic_title(&self.title);
        let title = ellipsized_label(palette, TextRole::Label, title)
            .when(self.completed, |title| title.line_through().opacity(0.7))
            .debug_selector(|| "chat-row-title".into());
        let row =
            row_surface(
                ElementId::Name(selector.clone().into()),
                palette,
                self.selected,
            )
            .min_h(px(36.))
            .pl(px(37.))
            .pr(px(7.))
            .py(px(7.))
            .when(self.details.compact, |row| {
                row.min_h(px(32.)).gap_2().pl(px(35.)).py(px(5.))
            })
            .child(
                div()
                    .w(px(3.))
                    .flex_none()
                    .rounded_full()
                    .bg(sidebar_color(self.color).unwrap_or(palette.icon))
                    .debug_selector(|| "catalog-topic-bar".into()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(title)
                    .children(self.details.preview.filter(|_| !self.details.compact).map(
                        |preview| preview_label(palette, preview, self.details.protected_preview),
                    )),
            )
            .children(
                ChatRow::metadata(
                    palette,
                    self.details.time.filter(|_| !self.details.compact),
                    self.details.unread,
                    self.details.mention,
                    None,
                )
                .map(|column| column.when(self.details.compact, |column| column.min_w(px(32.)))),
            )
            .when(self.details.muted, |row| row.opacity(0.7))
            .debug_selector(move || selector.clone());
        activate(row, focus, palette, on_open)
    }
}

/// Рисует кнопку показа дополнительных или завершённых тем, а также их скрытия.
pub(super) fn topic_toggle(
    id: Uuid,
    revision: u64,
    title: SharedString,
    collapse: bool,
    palette: Palette,
    focus: Option<&FocusHandle>,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let selector = format!("catalog-topic-toggle-{id}");
    let tooltip_text = title.clone();
    // Нажатие со старого кадра не должно попасть в кнопку с тем же UUID.
    let element_id = ElementId::Name(format!("{selector}-{revision}").into());
    let row = row_surface(element_id, palette, false)
        .min_h(px(36.))
        .pl(px(23.))
        .pr(px(7.))
        .py(px(7.))
        .rounded_none()
        .items_center()
        .child(div().w(px(3.)).flex_none())
        .child(
            label(palette, TextRole::Label, title)
                .flex_1()
                .min_w_0()
                .whitespace_normal(),
        )
        .child(chevron(collapse, palette))
        .tooltip(move |_, app| tooltip(palette, tooltip_text.clone(), app))
        .debug_selector(move || selector.clone());
    activate(row, focus, palette, on_toggle)
}

/// Отмечает границу между совпадениями поиска и остальными темами.
pub(super) fn other_topics_divider(palette: Palette) -> Div {
    div()
        .w_full()
        .min_w_0()
        .px(px(8.))
        .py(px(6.))
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            label(palette, TextRole::Caption, "Остальные темы")
                .text_color(palette.text_muted)
                .flex_none(),
        )
        .child(div().h(px(1.)).flex_1().bg(palette.border))
        .debug_selector(|| "catalog-other-topics".into())
}

/// Даёт вложенной кнопке область фокуса и оставляет её клик у этой кнопки.
fn inner_action(
    id: ElementId,
    palette: Palette,
    hover: Hsla,
    focus: Option<&FocusHandle>,
    action: RowAction,
) -> Stateful<Div> {
    activate_child(
        div()
            .id(id)
            .rounded(px(6.))
            .border_1()
            .border_color(gpui::transparent_black())
            .hover(|style| style.bg(hover)),
        focus,
        palette,
        action,
    )
}

/// Размещает бейдж и кнопку раскрытия в одном месте, не сдвигая время и текст.
fn expand_slot(
    id: Uuid,
    palette: Palette,
    unread: Option<(u64, BadgeTone)>,
    focus: Option<&FocusHandle>,
    expanded: bool,
    selected: bool,
    action: RowAction,
) -> Div {
    let badge = unread.and_then(|(count, tone)| unread_badge(palette, count, tone));
    let has_badge = badge.is_some();
    let title = if expanded {
        "Свернуть темы"
    } else {
        "Развернуть темы"
    };
    let shade = if selected {
        palette.card_selected
    } else {
        palette.card
    };
    let button = div()
        .id("expand")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .border_1()
        .border_color(gpui::transparent_black())
        .when(has_badge, |button| {
            // Сохраняем место для счётчика, даже когда стрелка его перекрывает.
            button.opacity(0.).group_hover(STREAM_HOVER_GROUP, |style| {
                style.opacity(1.).bg(palette.card_hover)
            })
        })
        .child(chevron(expanded, palette))
        .tooltip(move |_, app| tooltip(palette, title.into(), app))
        .debug_selector(move || format!("catalog-stream-expand-{id}"));
    let button = activate_child(button, focus, palette, action)
        .when(palette.keyboard_focus_visible(), |button| {
            button.focus(|style| style.opacity(1.).bg(shade).border_color(palette.focus))
        });
    div()
        .relative()
        .h(px(20.))
        .min_w(px(20.))
        .flex_none()
        .children(badge)
        .child(button)
}

/// Оставляет нажатие и отпускание мыши у вложенного действия.
fn activate_child(
    button: Stateful<Div>,
    focus: Option<&FocusHandle>,
    palette: Palette,
    action: RowAction,
) -> Stateful<Div> {
    activate(button, focus, palette, move |window, app| {
        app.stop_propagation();
        action(window, app);
    })
    .when_some(focus.cloned(), |button, focus| {
        button.on_mouse_down(MouseButton::Left, move |_, window, app| {
            // Иначе после исчезновения кнопки отпускание мыши откроет родительский канал.
            window.focus(&focus);
            app.stop_propagation();
        })
    })
}

/// Показывает закрепление разговора в его текущей папке.
fn pin(palette: Palette) -> Div {
    div()
        .flex_none()
        .child(Icon::Pin.element(px(14.), palette.icon_hover))
        .debug_selector(|| "catalog-row-pinned".into())
}

/// Возвращает обычную или перевёрнутую стрелку раскрытия из ресурсов Workspace.
fn chevron(up: bool, palette: Palette) -> gpui::Svg {
    let icon = Icon::ChevronDown.element(px(16.), palette.icon_hover);
    if up {
        icon.with_transformation(Transformation::rotate(radians(std::f32::consts::PI)))
    } else {
        icon
    }
}

/// Даёт темам и каналу общее оформление наведения и клавиатурного фокуса.
fn row_surface(id: ElementId, palette: Palette, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .min_w_0()
        .flex_none()
        .flex()
        .gap(px(12.))
        .rounded(PANEL_RADIUS)
        .border_1()
        .border_color(gpui::transparent_black())
        .when(selected, |row| row.bg(palette.card_selected))
        .hover(|style| style.bg(palette.card_hover))
        .active(|style| style.bg(palette.card_selected))
}

/// Оставляет видимый первый символ названия и поддерживает составные эмодзи.
fn row_avatar(title: &str, presence: AvatarPresence) -> Avatar {
    let initial = title
        .trim_start()
        .graphemes(true)
        .next()
        .unwrap_or_default();
    let mut avatar = Avatar::from_initials(initial.to_owned());
    avatar.presence = presence;
    avatar
}

/// Сокращает превью до одной строки и оставляет автора перед текстом.
fn preview_label(
    palette: Palette,
    preview: ChatRowPreview,
    protected: Option<gpui::Entity<SecurePreview>>,
) -> gpui::AnyElement {
    if let Some(protected) = protected {
        return protected.into_any_element();
    }
    let text = match preview.sender {
        Some(sender) => format!("{sender}: {}", preview.text),
        None => preview.text.to_string(),
    };
    ellipsized_label(palette, TextRole::Caption, clean_text(&text))
        .debug_selector(|| "chat-row-preview".into())
        .into_any_element()
}

/// Сохраняет высоту элемента даже при многострочных данных от сервера.
fn clean_text(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

/// Добавляет решётку только к непустому названию, у которого её ещё нет.
fn topic_title(title: &str) -> String {
    let text = clean_text(title.trim());
    if text.is_empty() || text.starts_with('#') {
        text
    } else {
        format!("#{text}")
    }
}

/// Преобразует корректный серверный RGB в цвет GPUI.
fn sidebar_color(color: Option<u32>) -> Option<Hsla> {
    color
        .filter(|color| *color <= 0xffffff)
        .map(|color| rgb(color).into())
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests;
