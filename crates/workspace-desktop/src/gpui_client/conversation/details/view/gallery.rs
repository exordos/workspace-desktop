//! Рисует вход в разделы вложений, сетку медиа и списки файлов и ссылок.

use gpui::{
    Div, FocusHandle, ImageSource, ObjectFit, SharedString, Stateful, StyledImage, div, img,
    prelude::*, px,
};

use workspace_core::storage::{HistoryTime, media::GalleryCounts};

use super::{Dispatch, GalleryTriggers, PanelAction};
use crate::gpui_client::{
    conversation::{
        Conversation,
        details::gallery::{Action, CATEGORIES, Entry, Gallery, State},
        gallery_source::{GalleryCategory, GalleryItem},
    },
    preferences::Language,
    ui::{
        Button, ButtonAppearance, Icon, Palette, TextRole, activate, ellipsized_label, label,
        popup::tooltip, sidebar_item,
    },
};
use crate::platform::message_time::{CalendarDate, LocalTime};

/// Ширина сетки сохраняется и в последней неполной строке месяца.
const COLUMNS: usize = 3;

/// Заголовок раздела в общей рамке правой панели.
pub(in crate::gpui_client::conversation::details) fn title(
    category: GalleryCategory,
    language: Language,
) -> &'static str {
    language.tr(category_key(category))
}

/// Даёт общий ключ заголовка раздела и его числовых форм в сайдбаре.
fn category_key(category: GalleryCategory) -> &'static str {
    match category {
        GalleryCategory::Photos => "desktop.conversation.gallery.photos",
        GalleryCategory::Videos => "desktop.conversation.gallery.videos",
        GalleryCategory::Links => "desktop.conversation.gallery.links",
        GalleryCategory::Files => "desktop.conversation.gallery.files",
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
pub(super) fn triggers(
    data: GalleryTriggers<'_>,
    palette: Palette,
    language: Language,
    dispatch: &Dispatch,
) -> Div {
    div()
        .debug_selector(|| "conversation-gallery-triggers".into())
        .flex()
        .flex_col()
        .px_2()
        .gap_2()
        .children(
            CATEGORIES
                .into_iter()
                .zip(data.focus)
                .map(|(category, focus)| {
                    trigger(category, focus, data.counts, palette, language, dispatch)
                }),
        )
}

/// Рисует строку категории и открывает её из мыши или с клавиатуры.
fn trigger(
    category: GalleryCategory,
    focus: &FocusHandle,
    counts: Option<GalleryCounts>,
    palette: Palette,
    language: Language,
    dispatch: &Dispatch,
) -> Stateful<Div> {
    let id = trigger_id(category);
    let (icon, count) = match category {
        GalleryCategory::Photos => (Icon::GalleryPhoto, counts.map(|count| count.photos)),
        GalleryCategory::Videos => (Icon::GalleryVideo, counts.map(|count| count.videos)),
        GalleryCategory::Files => (Icon::GalleryFile, counts.map(|count| count.files)),
        GalleryCategory::Links => (Icon::GalleryLink, counts.map(|count| count.links)),
    };
    let caption = count.map_or_else(
        || title(category, language).to_owned(),
        |count| language.plural(category_key(category), count),
    );
    let row = sidebar_item(
        palette,
        icon.element(px(32.), palette.icon),
        label(palette, TextRole::Body, caption)
            .line_height(px(16.))
            .truncate(),
        px(12.),
    )
    .id(id)
    .debug_selector(move || id.into())
    .h(px(32.))
    .rounded_md()
    .when(count.is_some(), |row| {
        row.tooltip(move |_, app| {
            tooltip(
                palette,
                language.tr("desktop.conversation.gallery.coverage").into(),
                app,
            )
        })
    });
    let dispatch = dispatch.clone();
    activate(row, Some(focus), palette, move |window, app| {
        dispatch(PanelAction::Gallery(Action::Open(category)), window, app);
    })
    .when(palette.keyboard_focus_visible(), |row| {
        row.focus(|style| style.bg(palette.card_hover))
    })
}

/// Рисует открытый раздел. Признак неполноты остаётся над прокруткой и пустым состоянием.
pub(super) fn render(
    conversation: &Conversation,
    gallery: &Gallery,
    palette: Palette,
    language: Language,
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
            panel.child(scope(gallery, palette, language, dispatch))
        })
        .child(
            label(
                palette,
                TextRole::Caption,
                language.tr("desktop.conversation.gallery.coverage"),
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
                        .children(months(conversation, gallery, palette, language, dispatch))
                        .children(footer(gallery, palette, language, dispatch)),
                ),
        )
}

/// Даёт выбрать текущую тему или все известные темы того же канала.
fn scope(gallery: &Gallery, palette: Palette, language: Language, dispatch: &Dispatch) -> Div {
    let buttons = [
        (
            false,
            "conversation-gallery-scope-topic",
            "desktop.conversation.gallery.thisTopic",
        ),
        (
            true,
            "conversation-gallery-scope-stream",
            "desktop.conversation.gallery.wholeChannel",
        ),
    ];
    div()
        .flex()
        .gap_1()
        .px_2()
        .children(buttons.map(|(whole, id, key)| {
            let mut button = Button::new(id, language.tr(key));
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
fn footer(
    gallery: &Gallery,
    palette: Palette,
    language: Language,
    dispatch: &Dispatch,
) -> Option<Div> {
    let content = div().flex().flex_col().items_start().gap_2().px_2();
    let content = match gallery.state {
        State::Loading { .. } => content.child(label(
            palette,
            TextRole::Body,
            language.tr("desktop.conversation.gallery.loading"),
        )),
        State::Failed { .. } => content
            .child(label(
                palette,
                TextRole::Body,
                language.tr("desktop.conversation.gallery.loadFailed"),
            ))
            .child(command(
                "conversation-gallery-retry",
                language.tr("common.retry"),
                Action::Retry,
                palette,
                dispatch,
            )),
        State::Ready if gallery.has_next() => content.child(command(
            "conversation-gallery-more",
            language.tr("desktop.conversation.gallery.more"),
            Action::More,
            palette,
            dispatch,
        )),
        State::Ready if gallery.entries.is_empty() => content.child(label(
            palette,
            TextRole::Body,
            language.tr("desktop.conversation.gallery.empty"),
        )),
        State::Ready if gallery.needs_refresh => content,
        State::Ready => return None,
    };
    Some(content.when(gallery.needs_refresh, |content| {
        content.child(command(
            "conversation-gallery-refresh",
            language.tr("desktop.conversation.gallery.refresh"),
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
    language: Language,
    dispatch: &Dispatch,
) -> Vec<Div> {
    let local = LocalTime::system();
    let entries: Vec<_> = gallery
        .entries
        .iter()
        .map(|entry| (entry, date(entry, &local)))
        .collect();
    let mut years = entries
        .iter()
        .filter_map(|(_, date)| date.map(|date| date.year));
    let first_year = years.next();
    let show_year = years.any(|year| Some(year) != first_year);
    entries
        .chunk_by(|left, right| month_key(left.1) == month_key(right.1))
        .map(|entries| {
            let heading = month_label(month_key(entries[0].1), show_year, language);
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
                            .children(row.iter().map(|(entry, _)| {
                                tile(
                                    conversation,
                                    entry,
                                    gallery.category,
                                    palette,
                                    language,
                                    dispatch,
                                )
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
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(entries.iter().map(|(entry, date)| {
                        row(conversation, entry, *date, palette, language, dispatch)
                    }))
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
    language: Language,
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
                label(palette, TextRole::Caption, status.message(language))
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .p_1()
                    .bg(palette.surface),
            );
        } else if category == GalleryCategory::Videos {
            tile = tile.child(
                label(
                    palette,
                    TextRole::Caption,
                    language.tr("desktop.conversation.gallery.save"),
                )
                    .absolute()
                    .bottom_2()
                    .left_2()
                    .right_2()
                    .truncate(),
            );
        }
        let action = language.tr(if category == GalleryCategory::Photos {
            "desktop.conversation.gallery.openPhoto"
        } else {
            "desktop.conversation.gallery.saveVideo"
        });
        let description: SharedString = item.name.as_deref().map_or_else(
            || action.into(),
            |name| {
                language
                    .tr_with(
                        "desktop.conversation.gallery.namedAction",
                        &[("action", action), ("name", name)],
                    )
                    .into()
            },
        );
        tile = tile.tooltip(move |_, app| tooltip(palette, description.clone(), app));
    }
    activate_entry(tile, entry, palette, dispatch)
}

/// Строка файла или ссылки с фактически известными метаданными.
fn row(
    conversation: &Conversation,
    entry: &Entry,
    date: Option<CalendarDate>,
    palette: Palette,
    language: Language,
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
            let name = item
                .name
                .as_deref()
                .unwrap_or_else(|| language.tr("attachmentCard.unnamedFile"))
                .to_owned();
            details = details.child(ellipsized_label(palette, TextRole::Body, name));
            if let Some(status) = conversation.gallery_export_status(item) {
                details =
                    details.child(label(palette, TextRole::Caption, status.message(language)));
            } else if let Some(size) = item.size {
                details = details.child(label(
                    palette,
                    TextRole::Caption,
                    size_label(size, language),
                ));
            }
            if let Some(date) = date {
                details = details.child(label(
                    palette,
                    TextRole::Caption,
                    day_label(&date, language),
                ));
            }
            (
                Icon::Attach,
                language.tr("desktop.conversation.gallery.saveFile"),
            )
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
            (
                Icon::Links,
                language.tr("desktop.conversation.gallery.openBrowser"),
            )
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

/// Ключи двух форм названия месяца для заголовка и даты строки.
const MONTHS: [(&str, &str); 12] = [
    (
        "desktop.conversation.gallery.month.jan",
        "desktop.conversation.gallery.dayMonth.jan",
    ),
    (
        "desktop.conversation.gallery.month.feb",
        "desktop.conversation.gallery.dayMonth.feb",
    ),
    (
        "desktop.conversation.gallery.month.mar",
        "desktop.conversation.gallery.dayMonth.mar",
    ),
    (
        "desktop.conversation.gallery.month.apr",
        "desktop.conversation.gallery.dayMonth.apr",
    ),
    (
        "desktop.conversation.gallery.month.may",
        "desktop.conversation.gallery.dayMonth.may",
    ),
    (
        "desktop.conversation.gallery.month.jun",
        "desktop.conversation.gallery.dayMonth.jun",
    ),
    (
        "desktop.conversation.gallery.month.jul",
        "desktop.conversation.gallery.dayMonth.jul",
    ),
    (
        "desktop.conversation.gallery.month.aug",
        "desktop.conversation.gallery.dayMonth.aug",
    ),
    (
        "desktop.conversation.gallery.month.sep",
        "desktop.conversation.gallery.dayMonth.sep",
    ),
    (
        "desktop.conversation.gallery.month.oct",
        "desktop.conversation.gallery.dayMonth.oct",
    ),
    (
        "desktop.conversation.gallery.month.nov",
        "desktop.conversation.gallery.dayMonth.nov",
    ),
    (
        "desktop.conversation.gallery.month.dec",
        "desktop.conversation.gallery.dayMonth.dec",
    ),
];

/// Формирует заголовок группы, добавляя год только для многолетней истории.
fn month_label(date: Option<(u16, u8)>, show_year: bool, language: Language) -> String {
    let Some((year, month)) = date else {
        return language
            .tr("desktop.conversation.gallery.unknownDate")
            .to_owned();
    };
    let name = language.tr(MONTHS[usize::from(month - 1)].0);
    if show_year {
        language.tr_with(
            "desktop.conversation.gallery.monthYear",
            &[("month", name), ("year", &year.to_string())],
        )
    } else {
        name.to_owned()
    }
}

/// Формирует подпись даты файла, оставляя порядок частей каталогу.
fn day_label(date: &CalendarDate, language: Language) -> String {
    language.tr_with(
        "desktop.conversation.gallery.day",
        &[
            ("month", language.tr(MONTHS[usize::from(date.month - 1)].1)),
            ("day", &date.day.to_string()),
        ],
    )
}

/// Берёт год и месяц записи для группировки списка.
fn month_key(date: Option<CalendarDate>) -> Option<(u16, u8)> {
    date.map(|date| (date.year, date.month))
}

/// Переводит время исходного сообщения в местный день для группы и подписи файла.
fn date(entry: &Entry, local: &LocalTime) -> Option<CalendarDate> {
    let time = match &entry.item {
        GalleryItem::Media { item, .. } => &item.created_at,
        GalleryItem::Link(link) => &link.created_at,
    };
    local
        .history_stamp(&HistoryTime::parse(time)?)
        .map(|stamp| stamp.date)
}

/// Подставляет размер в шаблон локальной единицы и использует десятичный разделитель языка.
fn size_label(size: u64, language: Language) -> String {
    const UNITS: [(u64, &str); 3] = [
        (1 << 30, "desktop.conversation.gallery.size.gib"),
        (1 << 20, "desktop.conversation.gallery.size.mib"),
        (1 << 10, "desktop.conversation.gallery.size.kib"),
    ];
    for (unit, key) in UNITS {
        if size >= unit {
            let value = format!("{:.1}", size as f64 / unit as f64).replace(
                '.',
                language.tr("desktop.conversation.gallery.size.decimalSeparator"),
            );
            return language.tr_with(key, &[("size", &value)]);
        }
    }
    language.tr_with(
        "desktop.conversation.gallery.size.bytes",
        &[("size", &size.to_string())],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Использует каталог для названия месяца, порядка даты и единицы размера.
    #[test]
    fn gallery_metadata_uses_the_conversation_locale() {
        let date = CalendarDate {
            year: 2026,
            month: 3,
            day: 22,
        };
        assert_eq!(title(GalleryCategory::Photos, Language::En), "Photos");
        assert_eq!(
            month_label(Some((2026, 3)), true, Language::En),
            "March 2026"
        );
        assert_eq!(day_label(&date, Language::Ru), "22 марта");
        assert_eq!(day_label(&date, Language::En), "March 22");
        assert_eq!(size_label(1536, Language::Ru), "1,5 КиБ");
        assert_eq!(size_label(1536, Language::En), "1.5 KiB");
    }
}
