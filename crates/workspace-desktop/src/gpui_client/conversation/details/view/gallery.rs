//! Рисует вход в разделы вложений, сетку медиа и списки файлов и ссылок.

use gpui::{
    Div, FocusHandle, ImageSource, ObjectFit, SharedString, Stateful, StyledImage, div, img,
    prelude::*, px,
};

use super::{Dispatch, PanelAction};
use crate::gpui_client::{
    conversation::{
        Conversation,
        details::gallery::{Action, CATEGORIES, Entry, Gallery, State},
        gallery_source::{GalleryCategory, GalleryItem},
    },
    ui::{
        Button, ButtonAppearance, Icon, Palette, TextRole, activate, ellipsized_label, label,
        popup::tooltip,
    },
};

/// Ширина сетки сохраняется и в последней неполной строке месяца.
const COLUMNS: usize = 3;

/// Заголовок раздела в общей рамке правой панели.
pub(in crate::gpui_client::conversation::details) fn title(
    category: GalleryCategory,
) -> &'static str {
    match category {
        GalleryCategory::Photos => "Фотографии",
        GalleryCategory::Videos => "Видео",
        GalleryCategory::Links => "Ссылки",
        GalleryCategory::Files => "Файлы",
    }
}

/// Возвращает стабильный идентификатор входа в раздел.
fn trigger_id(category: GalleryCategory) -> &'static str {
    match category {
        GalleryCategory::Photos => "conversation-gallery-open-photos",
        GalleryCategory::Videos => "conversation-gallery-open-videos",
        GalleryCategory::Links => "conversation-gallery-open-links",
        GalleryCategory::Files => "conversation-gallery-open-files",
    }
}

/// Добавляет четыре доступных раздела в прокручиваемые сведения разговора.
pub(super) fn triggers(focus: &[FocusHandle; 4], palette: Palette, dispatch: &Dispatch) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(label(palette, TextRole::Label, "Вложения").px_2().mb_1())
        .children(CATEGORIES.into_iter().zip(focus).map(|(category, focus)| {
            let id = trigger_id(category);
            let trigger = div()
                .id(id)
                .debug_selector(move || id.into())
                .h(px(36.))
                .px_3()
                .flex()
                .justify_between()
                .items_center()
                .rounded_md()
                .border_1()
                .border_color(gpui::transparent_black())
                .bg(palette.card)
                .hover(|style| style.bg(palette.card_hover))
                .child(label(palette, TextRole::Body, title(category)))
                .child(Icon::ChevronRight.element(px(16.), palette.icon));
            let dispatch = dispatch.clone();
            activate(trigger, Some(focus), palette, move |window, app| {
                dispatch(PanelAction::Gallery(Action::Open(category)), window, app);
            })
        }))
}

/// Рисует открытый раздел. Признак неполноты остаётся над прокруткой и пустым состоянием.
pub(super) fn render(
    conversation: &Conversation,
    gallery: &Gallery,
    palette: Palette,
    dispatch: &Dispatch,
) -> Div {
    div()
        .debug_selector(|| "conversation-gallery-panel".into())
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .gap_2()
        .when(gallery.has_topic, |panel| {
            panel.child(scope(gallery, palette, dispatch))
        })
        .child(
            label(
                palette,
                TextRole::Caption,
                "Из загруженной истории. Список может быть неполным.",
            )
            .px_2()
            .text_color(palette.text_muted)
            .debug_selector(|| "conversation-gallery-coverage".into()),
        )
        .child(
            div()
                .id("conversation-gallery-scroll")
                .flex_1()
                .min_h_0()
                .overflow_x_hidden()
                .overflow_y_scroll()
                .track_scroll(&gallery.scroll)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(20.))
                        .pb_2()
                        .children(months(conversation, gallery, palette, dispatch))
                        .children(footer(gallery, palette, dispatch)),
                ),
        )
}

/// Даёт выбрать текущую тему или все известные темы того же канала.
fn scope(gallery: &Gallery, palette: Palette, dispatch: &Dispatch) -> Div {
    let buttons = [
        (false, "conversation-gallery-scope-topic", "Эта тема"),
        (true, "conversation-gallery-scope-stream", "Весь канал"),
    ];
    div()
        .flex()
        .gap_1()
        .px_2()
        .children(buttons.map(|(whole, id, caption)| {
            let mut button = Button::new(id, caption);
            if gallery.whole_stream != whole {
                button.appearance = ButtonAppearance::Ghost;
            }
            let dispatch = dispatch.clone();
            button
                .render(palette, None, move |window, app| {
                    dispatch(PanelAction::Gallery(Action::Scope(whole)), window, app);
                })
                .debug_selector(move || id.into())
        }))
}

/// Показывает текущую операцию, пустой результат или команду следующей страницы.
fn footer(gallery: &Gallery, palette: Palette, dispatch: &Dispatch) -> Option<Div> {
    let content = div().flex().flex_col().items_start().gap_2().px_2();
    let content = match gallery.state {
        State::Loading { .. } => content.child(label(palette, TextRole::Body, "Загружаем…")),
        State::Failed { .. } => content
            .child(label(
                palette,
                TextRole::Body,
                "Не удалось загрузить вложения",
            ))
            .child(command(
                "conversation-gallery-retry",
                "Повторить",
                Action::Retry,
                palette,
                dispatch,
            )),
        State::Ready if gallery.has_next() => content.child(command(
            "conversation-gallery-more",
            "Показать ещё",
            Action::More,
            palette,
            dispatch,
        )),
        State::Ready if gallery.entries.is_empty() => {
            content.child(label(palette, TextRole::Body, "Здесь пока пусто"))
        }
        State::Ready if gallery.needs_refresh => content,
        State::Ready => return None,
    };
    Some(content.when(gallery.needs_refresh, |content| {
        content.child(command(
            "conversation-gallery-refresh",
            "Обновить список",
            Action::Refresh,
            palette,
            dispatch,
        ))
    }))
}

/// Связывает кнопку управления списком с действием экрана.
fn command(
    id: &'static str,
    caption: &'static str,
    action: Action,
    palette: Palette,
    dispatch: &Dispatch,
) -> Stateful<Div> {
    let dispatch = dispatch.clone();
    Button::new(id, caption)
        .render(palette, None, move |window, app| {
            dispatch(PanelAction::Gallery(action), window, app);
        })
        .debug_selector(move || id.into())
}

/// Собирает непрерывные группы одного месяца из уже отсортированной выдачи Core.
fn months(
    conversation: &Conversation,
    gallery: &Gallery,
    palette: Palette,
    dispatch: &Dispatch,
) -> Vec<Div> {
    let first_year = gallery
        .entries
        .first()
        .and_then(|entry| date(entry).map(|date| date.year));
    let last_year = gallery
        .entries
        .last()
        .and_then(|entry| date(entry).map(|date| date.year));
    let show_year = first_year != last_year;
    gallery
        .entries
        .chunk_by(|left, right| month_key(left) == month_key(right))
        .map(|entries| {
            let heading = month_key(&entries[0]).map_or_else(
                || "Дата неизвестна".to_owned(),
                |(year, month)| {
                    let name = MONTHS[usize::from(month - 1)].0;
                    if show_year {
                        format!("{name} {year}")
                    } else {
                        name.to_owned()
                    }
                },
            );
            let grid = matches!(
                gallery.category,
                GalleryCategory::Photos | GalleryCategory::Videos
            );
            let rows = if grid {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(entries.chunks(COLUMNS).map(|row| {
                        div()
                            .flex()
                            .gap_2()
                            .children(row.iter().map(|entry| {
                                tile(conversation, entry, gallery.category, palette, dispatch)
                            }))
                            .children((row.len()..COLUMNS).map(|_| {
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .border_1()
                                    .border_color(gpui::transparent_black())
                            }))
                    }))
            } else {
                div().flex().flex_col().gap_1().children(
                    entries
                        .iter()
                        .map(|entry| row(conversation, entry, palette, dispatch)),
                )
            };
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(label(palette, TextRole::Body, heading).px_2())
                .child(rows)
        })
        .collect()
}

/// Плитка фото или видео. Превью приходит с диска либо из уже открытой фотографии.
fn tile(
    conversation: &Conversation,
    entry: &Entry,
    category: GalleryCategory,
    palette: Palette,
    dispatch: &Dispatch,
) -> Stateful<Div> {
    let mut tile = entry_control(entry, palette)
        .relative()
        .flex_1()
        .min_w_0()
        .h(px(91.))
        .justify_center()
        .overflow_hidden();
    if let GalleryItem::Media { item, preview } = &entry.item {
        if let Some(image) = conversation
            .gallery_photo_thumbnail(item)
            .or(preview.as_ref())
        {
            let (message, file) = (item.message_id, item.file_id);
            tile = tile.child(
                div()
                    .size_full()
                    .debug_selector(move || {
                        format!("conversation-gallery-preview-{message}-{file}")
                    })
                    .child(
                        img(ImageSource::Render(image.clone()))
                            .size_full()
                            .object_fit(ObjectFit::Cover),
                    ),
            );
        }
        if let Some(status) = conversation.gallery_export_status(item) {
            tile = tile.child(
                label(palette, TextRole::Caption, status.message())
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .p_1()
                    .bg(palette.surface),
            );
        } else if category == GalleryCategory::Videos {
            tile = tile.child(
                label(palette, TextRole::Caption, "Сохранить…")
                    .absolute()
                    .bottom_2()
                    .left_2()
                    .right_2()
                    .truncate(),
            );
        }
        let action = if category == GalleryCategory::Photos {
            "Открыть фотографию"
        } else {
            "Сохранить видео…"
        };
        let description: SharedString = item
            .name
            .as_deref()
            .map_or_else(|| action.into(), |name| format!("{action} · {name}").into());
        tile = tile.tooltip(move |_, app| tooltip(palette, description.clone(), app));
    }
    activate_entry(tile, entry, palette, dispatch)
}

/// Строка файла или ссылки с фактически известными метаданными.
fn row(
    conversation: &Conversation,
    entry: &Entry,
    palette: Palette,
    dispatch: &Dispatch,
) -> Stateful<Div> {
    let mut details = div()
        .min_w_0()
        .flex_1()
        .flex()
        .flex_col()
        .justify_center()
        .pr_2();
    let (icon, action) = match &entry.item {
        GalleryItem::Media { item, .. } => {
            let name = item.name.as_deref().unwrap_or("Файл").to_owned();
            details = details.child(ellipsized_label(palette, TextRole::Body, name));
            if let Some(status) = conversation.gallery_export_status(item) {
                details = details.child(label(palette, TextRole::Caption, status.message()));
            } else if let Some(size) = item.size {
                details = details.child(label(palette, TextRole::Caption, size_label(size)));
            }
            if let Some(date) = date(entry) {
                let month = MONTHS[usize::from(date.month - 1)].1;
                details = details.child(label(
                    palette,
                    TextRole::Caption,
                    format!("{} {month}", date.day),
                ));
            }
            (Icon::Attach, "Сохранить файл…")
        }
        GalleryItem::Link(link) => {
            details = details
                .when(
                    link.label.trim_end_matches('/') != link.url.trim_end_matches('/'),
                    |details| {
                        details.child(ellipsized_label(
                            palette,
                            TextRole::Body,
                            link.label.clone(),
                        ))
                    },
                )
                .child(
                    ellipsized_label(palette, TextRole::Caption, link.url.clone())
                        .text_color(palette.accent),
                );
            (Icon::Links, "Открыть в браузере")
        }
    };
    let row = entry_control(entry, palette)
        .min_h(px(65.))
        .gap_3()
        .child(
            div()
                .size(px(65.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .bg(palette.field)
                .child(icon.element(px(24.), palette.icon)),
        )
        .child(details)
        .tooltip(move |_, app| tooltip(palette, action.into(), app));
    activate_entry(row, entry, palette, dispatch)
}

/// Даёт плитке или строке подложку, фокусную рамку и устойчивый селектор.
fn entry_control(entry: &Entry, palette: Palette) -> Stateful<Div> {
    let id = match &entry.item {
        GalleryItem::Media { item, .. } => format!(
            "conversation-gallery-entry-{}-{}",
            item.message_id, item.file_id
        ),
        GalleryItem::Link(link) => {
            format!("conversation-gallery-link-{}-{}", link.message_id, entry.id)
        }
    };
    div()
        .id(SharedString::from(format!("gallery-entry-{}", entry.id)))
        .debug_selector(move || id.clone())
        .flex()
        .items_center()
        .rounded(px(8.))
        .border_1()
        .border_color(gpui::transparent_black())
        .bg(palette.card)
        .hover(|style| style.bg(palette.card_hover))
}

/// Передаёт намерение активировать конкретный элемент текущей выдачи.
fn activate_entry(
    control: Stateful<Div>,
    entry: &Entry,
    palette: Palette,
    dispatch: &Dispatch,
) -> Stateful<Div> {
    let dispatch = dispatch.clone();
    let id = entry.id;
    activate(control, Some(&entry.focus), palette, move |window, app| {
        dispatch(PanelAction::Gallery(Action::Activate(id)), window, app);
    })
}

/// Дата сообщения из нормализованного серверного времени.
struct Date {
    year: u16, // Год разделяет одинаковые месяцы разных лет.
    month: u8, // Месяц с января по декабрь, от 1 до 12.
    day: u8,   // День месяца для подписи файла.
}

/// Две формы названия месяца для заголовка и даты строки.
const MONTHS: [(&str, &str); 12] = [
    ("Январь", "января"),
    ("Февраль", "февраля"),
    ("Март", "марта"),
    ("Апрель", "апреля"),
    ("Май", "мая"),
    ("Июнь", "июня"),
    ("Июль", "июля"),
    ("Август", "августа"),
    ("Сентябрь", "сентября"),
    ("Октябрь", "октября"),
    ("Ноябрь", "ноября"),
    ("Декабрь", "декабря"),
];

/// Берёт год и месяц записи для группировки списка.
fn month_key(entry: &Entry) -> Option<(u16, u8)> {
    date(entry).map(|date| (date.year, date.month))
}

/// Получает календарный день из подтверждённой строки UTC без угадывания неизвестной даты.
fn date(entry: &Entry) -> Option<Date> {
    let time = match &entry.item {
        GalleryItem::Media { item, .. } => &item.created_at,
        GalleryItem::Link(link) => &link.created_at,
    };
    let mut parts = time.get(..10)?.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    ((1..=12).contains(&month) && (1..=31).contains(&day)).then_some(Date { year, month, day })
}

/// Показывает реальный известный размер с русской десятичной запятой.
fn size_label(size: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1 << 30, "ГиБ"), (1 << 20, "МиБ"), (1 << 10, "КиБ")];
    for (unit, label) in UNITS {
        if size >= unit {
            return format!("{:.1} {label}", size as f64 / unit as f64).replace('.', ",");
        }
    }
    format!("{size} Б")
}
