//! Показывает месяц по неделям и обрабатывает выбор календарного дня мышью или клавиатурой.

use std::{cell::Cell, rc::Rc};

use chrono::{Datelike, Months, NaiveDate, TimeDelta};
use gpui::{
    Context, Div, FocusHandle, KeyDownEvent, Stateful, Transformation, Window, div, prelude::*, px,
    radians,
};

use super::{PickerMode, PickerOptions, card};
use crate::gpui_client::{
    Language,
    ui::{Icon, Palette, TextRole, activate, label, popup},
};

const WEEKDAYS: [&str; 7] = [
    "datePicker.mon",
    "datePicker.tue",
    "datePicker.wed",
    "datePicker.thu",
    "datePicker.fri",
    "datePicker.sat",
    "datePicker.sun",
];
const MONTHS: [&str; 12] = [
    "datePicker.january",
    "datePicker.february",
    "datePicker.march",
    "datePicker.april",
    "datePicker.may",
    "datePicker.june",
    "datePicker.july",
    "datePicker.august",
    "datePicker.september",
    "datePicker.october",
    "datePicker.november",
    "datePicker.december",
];

/// Сообщает о дне, который пользователь выбрал в календаре.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatePickerChanged(pub NaiveDate); // Выбранный календарный день.

/// Календарь с отдельным состоянием выбранного дня и открытого месяца.
pub struct DatePicker {
    value: Option<NaiveDate>, // Выбранный день, None до первого выбора.
    cursor: NaiveDate,        // День клавиатурного фокуса и открытый месяц.
    min: NaiveDate,           // Первый допустимый день включительно.
    max: NaiveDate,           // Последний допустимый день включительно.
    options: PickerOptions,   // Язык, палитра и доступность ввода.
    grid_focus: FocusHandle,  // Одна остановка Tab для всех дней месяца.
    previous: FocusHandle,    // Фокус стрелки предыдущего месяца.
    next: FocusHandle,        // Фокус стрелки следующего месяца.
    embedded: bool,           // Фон и отступы уже заданы общей карточкой.
    month_offset: Option<Rc<Cell<i32>>>, // Смещение месяца при пересоздании панели просмотра.
}

impl gpui::EventEmitter<DatePickerChanged> for DatePicker {}

impl DatePicker {
    /// Создаёт календарь. Если выбора нет, показывает месяц переданного дня.
    pub fn new(
        value: Option<NaiveDate>,
        default_day: NaiveDate,
        options: PickerOptions,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            value,
            cursor: value.unwrap_or(default_day),
            min: NaiveDate::MIN,
            max: NaiveDate::MAX,
            options,
            grid_focus: cx.focus_handle().tab_stop(true),
            previous: cx.focus_handle(),
            next: cx.focus_handle(),
            embedded: false,
            month_offset: None,
        }
    }

    /// Оставляет у календаря только содержимое для общей карточки даты и времени.
    pub(super) fn embedded(mut self) -> Self {
        self.embedded = true;
        self
    }

    /// Восстанавливает месяц просмотра и сохраняет его при переключении.
    pub(super) fn with_month_offset(&mut self, offset: Rc<Cell<i32>>) {
        if let Some(day) = shift_month(self.cursor, offset.get()) {
            self.cursor = day;
        }
        self.month_offset = Some(offset);
    }

    /// Возвращает календарный день для вызывающей формы.
    pub fn value(&self) -> Option<NaiveDate> {
        self.value
    }

    /// Переносит клавиатурный фокус в сетку на выбранный день.
    pub(super) fn focus(&self, window: &mut Window) {
        if self.selectable(self.cursor) {
            window.focus(&self.grid_focus);
        }
    }

    /// Отображает переданный формой день без отправки события обратно.
    pub fn set_value(&mut self, value: Option<NaiveDate>, cx: &mut Context<Self>) {
        if self.value != value {
            self.value = value;
            if let Some(day) = value {
                self.cursor = day;
            }
        }
        if self.min <= self.max {
            self.cursor = self.cursor.clamp(self.min, self.max);
        }
        cx.notify();
    }

    /// Задаёт первые и последние допустимые дни включительно.
    pub fn set_bounds(
        &mut self,
        min: Option<NaiveDate>,
        max: Option<NaiveDate>,
        cx: &mut Context<Self>,
    ) {
        self.min = min.unwrap_or(NaiveDate::MIN);
        self.max = max.unwrap_or(NaiveDate::MAX);
        self.set_value(self.value, cx);
    }

    /// Обновляет язык, палитру и доступность календаря.
    pub fn set_options(&mut self, options: PickerOptions, cx: &mut Context<Self>) {
        self.options = options;
        cx.notify();
    }

    /// Обновляет содержимое календаря внутри составного компонента.
    pub(super) fn configure(
        &mut self,
        value: Option<NaiveDate>,
        min: Option<NaiveDate>,
        max: Option<NaiveDate>,
        options: PickerOptions,
        cx: &mut Context<Self>,
    ) {
        self.options = options;
        self.min = min.unwrap_or(NaiveDate::MIN);
        self.max = max.unwrap_or(NaiveDate::MAX);
        self.set_value(value, cx);
    }

    /// Проверяет, может ли пользователь выбрать день или перейти к нему.
    fn selectable(&self, day: NaiveDate) -> bool {
        self.options.mode == PickerMode::Editable && (self.min..=self.max).contains(&day)
    }

    /// Возвращает доступный день в соседнем месяце.
    fn adjacent_month(&self, direction: i32) -> Option<NaiveDate> {
        if self.options.mode == PickerMode::Disabled || self.min > self.max {
            return None;
        }
        let target = shift_month(self.cursor, direction)?;
        let day = target.clamp(self.min, self.max);
        (target.year() == day.year() && target.month() == day.month()).then_some(day)
    }

    /// Выбирает день и возвращает фокус сетке, если пользователь нажал ячейку.
    fn select(&mut self, day: NaiveDate, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selectable(day) {
            return;
        }
        window.focus(&self.grid_focus);
        self.cursor = day;
        if self.value != Some(day) {
            self.value = Some(day);
            cx.emit(DatePickerChanged(day));
        }
        cx.notify();
    }

    /// Перемещает курсор по дням, неделям, месяцам и годам или выбирает день.
    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        if !self.selectable(self.cursor) || modifiers.control || modifiers.alt || modifiers.platform
        {
            return;
        }
        if matches!(event.keystroke.key.as_str(), "enter" | "space") && !modifiers.shift {
            cx.stop_propagation();
            self.select(self.cursor, window, cx);
            return;
        }

        let weekday = i64::from(self.cursor.weekday().num_days_from_monday());
        let next = match event.keystroke.key.as_str() {
            "pageup" => shift_month(self.cursor, if modifiers.shift { -12 } else { -1 }),
            "pagedown" => shift_month(self.cursor, if modifiers.shift { 12 } else { 1 }),
            "left" if !modifiers.shift => shift_days(self.cursor, -1),
            "right" if !modifiers.shift => shift_days(self.cursor, 1),
            "up" if !modifiers.shift => shift_days(self.cursor, -7),
            "down" if !modifiers.shift => shift_days(self.cursor, 7),
            "home" if !modifiers.shift => shift_days(self.cursor, -weekday),
            "end" if !modifiers.shift => shift_days(self.cursor, 6 - weekday),
            _ => return,
        };
        cx.stop_propagation();
        if let Some(next) = next {
            self.cursor = next.clamp(self.min, self.max);
            cx.notify();
        }
    }

    /// Рисует переключатель месяца с фокусом и подсказкой.
    fn month_arrow(&self, direction: i32, palette: Palette, cx: &Context<Self>) -> Stateful<Div> {
        let previous = direction < 0;
        let (id, key, focus) = if previous {
            (
                "date-picker-previous",
                "datePicker.previous",
                &self.previous,
            )
        } else {
            ("date-picker-next", "datePicker.next", &self.next)
        };
        let target = self.adjacent_month(direction);
        let arrow = Icon::CalendarNext.element(
            px(20.),
            if target.is_some() {
                palette.text
            } else {
                palette.disabled
            },
        );
        let arrow = if previous {
            arrow.with_transformation(Transformation::rotate(radians(std::f32::consts::PI)))
        } else {
            arrow
        };
        let button = div()
            .id(id)
            .debug_selector(move || id.into())
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_color(gpui::transparent_black())
            .child(arrow);
        let Some(target) = target else {
            return button;
        };
        let title = self.options.language.tr(key);
        if let Some(offset) = self.month_offset.clone() {
            let keys = offset.clone();
            return button
                .track_focus(&focus.clone().tab_stop(true))
                .tab_stop(true)
                .cursor_pointer()
                .hover(move |style| style.bg(palette.card_hover))
                .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                    offset.set(offset.get().saturating_add(direction));
                    cx.stop_propagation();
                    window.refresh();
                })
                .on_key_down(move |event, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        keys.set(keys.get().saturating_add(direction));
                        cx.stop_propagation();
                        window.refresh();
                    }
                })
                .tooltip(move |_, cx| popup::tooltip(palette, title.into(), cx));
        }
        let picker = cx.weak_entity();
        activate(
            button.hover(move |style| style.bg(palette.card_hover)),
            Some(focus),
            palette,
            move |_, cx| {
                let _ = picker.update(cx, |picker, cx| {
                    picker.cursor = target;
                    cx.notify();
                });
            },
        )
        .tooltip(move |_, cx| popup::tooltip(palette, title.into(), cx))
    }

    /// Рисует один день, выделяя выбор цветом и курсор клавиатуры рамкой.
    fn day(
        &self,
        day: Option<NaiveDate>,
        palette: Palette,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let cell = div()
            .flex_1()
            .min_w_0()
            .h(px(26.))
            .flex()
            .items_center()
            .justify_center();
        let Some(day) = day else {
            return cell;
        };

        let selected = Some(day) == self.value;
        let enabled = self.selectable(day);
        let muted = day.month() != self.cursor.month() || day.weekday().num_days_from_monday() >= 5;
        let visible = enabled || self.options.mode == PickerMode::ReadOnly;
        let color = match (selected, visible && !muted) {
            (true, _) => palette.on_accent,
            (false, true) => palette.text,
            (false, false) => palette.text_muted,
        };
        let marker = div()
            .id("date-picker-marker")
            .w_full()
            .max_w(px(25.))
            .h(px(25.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_1()
            .border_color(
                if day == self.cursor
                    && palette.keyboard_focus_visible()
                    && self.grid_focus.is_focused(window)
                    && enabled
                {
                    palette.focus
                } else {
                    gpui::transparent_black()
                },
            )
            .when(selected, |marker| {
                marker
                    .bg(palette.accent)
                    .debug_selector(|| "date-picker-selected-day".into())
            })
            .child(label(palette, TextRole::Body, day.day().to_string()).text_color(color));
        let element = cell
            .id(("day", day.num_days_from_ce() as u64))
            .debug_selector(move || format!("date-picker-day-{day}"))
            .child(marker);
        div()
            .flex_1()
            .min_w_0()
            .child(element.when(enabled, |element| {
                element
                    .cursor_pointer()
                    .hover(move |style| style.rounded_md().bg(palette.card_hover))
                    .on_click(
                        cx.listener(move |picker, _, window, cx| picker.select(day, window, cx)),
                    )
            }))
    }

    /// Рисует неделю начиная с понедельника и 5 или 6 строк дней.
    fn grid(&self, palette: Palette, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let (first, rows) = month_grid(self.cursor);
        let weekday_row = div()
            .flex()
            .gap(px(8.))
            .border_b_1()
            .border_color(palette.border)
            .children(WEEKDAYS.iter().enumerate().map(|(index, key)| {
                let color = if index < 5 {
                    palette.text
                } else {
                    palette.text_muted
                };
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        label(palette, TextRole::Body, self.options.language.tr(key))
                            .text_color(color),
                    )
            }));
        div()
            .id("date-picker-grid")
            .debug_selector(|| "date-picker-grid".into())
            .when(self.selectable(self.cursor), |grid| {
                grid.track_focus(&self.grid_focus)
                    .tab_stop(true)
                    .on_key_down(cx.listener(Self::handle_key))
            })
            .child(weekday_row)
            .children((0..rows).map(|row| {
                div().flex().gap(px(8.)).children((0..7).map(|column| {
                    self.day(
                        shift_days(
                            self.cursor.with_day(1).expect("первое число"),
                            row * 7 + column - first,
                        ),
                        palette,
                        window,
                        cx,
                    )
                }))
            }))
    }
}

impl Render for DatePicker {
    /// Показывает самостоятельную карточку или календарь внутри выбора даты и времени.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.options.palette(window, cx);
        let language = self.options.language;
        let surface = if self.embedded { div() } else { card(palette) };
        surface.child(
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .h(px(36.))
                        .child(self.month_arrow(-1, palette, cx))
                        .child(label(
                            palette,
                            TextRole::Heading,
                            month_title(self.cursor, language),
                        ))
                        .child(self.month_arrow(1, palette, cx)),
                )
                .child(self.grid(palette, window, cx)),
        )
    }
}

/// Формирует заголовок из названия месяца и года на языке аккаунта.
fn month_title(day: NaiveDate, language: Language) -> String {
    language.tr_with(
        "datePicker.monthYear",
        &[
            ("month", language.tr(MONTHS[day.month0() as usize])),
            ("year", &day.year().to_string()),
        ],
    )
}

/// Возвращает смещение первого дня и количество недель в видимом месяце.
fn month_grid(day: NaiveDate) -> (i64, i64) {
    let first = day.with_day(1).expect("первое число");
    let offset = i64::from(first.weekday().num_days_from_monday());
    let last = first
        .checked_add_months(Months::new(1))
        .and_then(|next| next.pred_opt())
        .unwrap_or(NaiveDate::MAX);
    let weeks = (offset + i64::from(last.day()) + 6) / 7;
    (offset, weeks.max(5))
}

/// Сдвигает дату на целые дни, учитывая границы календаря.
fn shift_days(day: NaiveDate, days: i64) -> Option<NaiveDate> {
    day.checked_add_signed(TimeDelta::days(days))
}

/// Сдвигает дату на месяцы, при необходимости выбирая последнее число.
fn shift_month(day: NaiveDate, months: i32) -> Option<NaiveDate> {
    if months < 0 {
        day.checked_sub_months(Months::new(months.unsigned_abs()))
    } else {
        day.checked_add_months(Months::new(months as u32))
    }
}
