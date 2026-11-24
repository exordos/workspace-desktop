//! Показывает самостоятельный календарь, часы с секундами и общее значение даты и времени.

use chrono::NaiveDate;
use gpui::{App, Context, Div, Entity, Subscription, Window, div, prelude::*};
use workspace_desktop::gpui_client::{
    Language,
    ui::{
        DatePicker, DateTimePicker, DateTimePickerChanged, Palette, PickerOptions, TextRole,
        TimePicker, TimePrecision, label,
    },
};

use super::Panel;

/// Примеры с независимыми значениями в каждой теме галереи.
pub(super) struct Pickers {
    date: Entity<DatePicker>,         // Календарь без времени.
    time: Entity<TimePicker>,         // Часы с секундами и стрелками.
    datetime: Entity<DateTimePicker>, // Общая карточка с календарём и часами.
    _change: Subscription,            // Обновляет строку с выбранным значением.
}

impl Pickers {
    /// Создаёт примеры на дате из макета Figma.
    pub(super) fn new(palette: Palette, window: &mut Window, cx: &mut Context<Panel>) -> Self {
        let options = PickerOptions {
            language: Language::Ru,
            palette: Some(palette),
            ..PickerOptions::default()
        };
        let day = NaiveDate::from_ymd_opt(2025, 11, 10).expect("дата из макета");
        let value = day.and_hms_opt(10, 58, 0).expect("время из макета");
        let date = cx.new(|cx| DatePicker::new(Some(day), day, options, cx));
        let time = cx.new(|cx| {
            TimePicker::new(
                Some(value.time()),
                TimePrecision::Seconds,
                true,
                options,
                window,
                cx,
            )
        });
        let datetime = cx.new(|cx| {
            DateTimePicker::new(
                Some(value),
                day,
                TimePrecision::Minutes,
                options,
                window,
                cx,
            )
            .with_time_caption(options.language.tr("timePicker.label"))
        });
        let change = cx.subscribe(&datetime, |_, _, _: &DateTimePickerChanged, cx| cx.notify());
        Self {
            date,
            time,
            datetime,
            _change: change,
        }
    }

    /// Показывает выбранное значение и оба самостоятельных компонента.
    pub(super) fn render(&self, palette: Palette, cx: &App) -> Div {
        let language = Language::Ru;
        let value = self.datetime.read(cx).value().map_or_else(
            || "—".into(),
            |value| value.format("%d.%m.%Y %H:%M").to_string(),
        );
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(label(
                palette,
                TextRole::Heading,
                language.tr("dateTimePicker.label"),
            ))
            .child(self.datetime.clone())
            .child(label(palette, TextRole::Caption, value))
            .child(label(
                palette,
                TextRole::Heading,
                language.tr("datePicker.label"),
            ))
            .child(self.date.clone())
            .child(label(
                palette,
                TextRole::Heading,
                language.tr("timePicker.label"),
            ))
            .child(self.time.clone())
    }
}
