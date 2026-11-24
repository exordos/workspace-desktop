//! Собирает календарь и часы в одну карточку и передаёт экрану выбранную дату со временем.

mod date;
mod time;

#[cfg(test)]
mod tests;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Timelike};
use gpui::{App, Context, Div, Entity, EventEmitter, Subscription, Window, div, prelude::*, px};

use super::{Palette, SURFACE_RADIUS};
use crate::gpui_client::Language;

pub use date::{DatePicker, DatePickerChanged};
pub use time::{TimePicker, TimePickerChanged};

const PICKER_WIDTH: f32 = 371.;
const PICKER_INSET: f32 = 20.;

/// Определяет доступность действий календаря и часов.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PickerMode {
    #[default]
    Editable, // Можно менять значение.
    Disabled, // Контролы видны, но не принимают ввод.
    ReadOnly, // Дата и время видны, элементы управления скрыты.
}

/// Задаёт язык и оформление для отдельно стоящего или составного компонента.
#[derive(Clone, Copy, Default)]
pub struct PickerOptions {
    pub language: Language, // Язык аккаунта, для которого показываются подписи.
    pub palette: Option<Palette>, // При None используется палитра текущего окна.
    pub mode: PickerMode,   // Доступность ввода.
}

impl PickerOptions {
    /// Берёт палитру компонента и уточняет видимость клавиатурного фокуса.
    fn palette(self, window: &Window, cx: &App) -> Palette {
        self.palette.map_or_else(
            || Palette::for_window(window, cx),
            |palette| palette.with_window_input(window, cx),
        )
    }
}

/// Определяет видимые части часов и точность выбранного значения.
#[derive(Clone, Copy, Default)]
pub enum TimePrecision {
    #[default]
    Minutes, // Часы и минуты.
    Seconds, // Часы, минуты и секунды.
}

impl TimePrecision {
    /// Возвращает число секунд в одном шаге точности.
    fn step(self) -> u32 {
        match self {
            Self::Minutes => 60,
            Self::Seconds => 1,
        }
    }

    /// Отбрасывает части времени, которые не отображаются.
    fn floor(self, time: NaiveTime) -> NaiveTime {
        let seconds = time.num_seconds_from_midnight();
        time_from_seconds(seconds / self.step() * self.step())
            .expect("округление вниз остаётся в пределах суток")
    }

    /// Округляет нижнюю границу вверх, при необходимости переходя к следующему дню.
    fn lower_bound(self, value: NaiveDateTime) -> Option<NaiveDateTime> {
        let floor = value.date().and_time(self.floor(value.time()));
        if floor == value {
            Some(floor)
        } else {
            floor.checked_add_signed(TimeDelta::seconds(i64::from(self.step())))
        }
    }
}

/// Получает время из числа секунд с начала суток.
fn time_from_seconds(seconds: u32) -> Option<NaiveTime> {
    NaiveTime::from_num_seconds_from_midnight_opt(seconds, 0)
}

/// Рисует общий фон и отступы календаря или объединённого выбора.
fn card(palette: Palette) -> Div {
    div()
        .w_full()
        .max_w(px(PICKER_WIDTH))
        .flex()
        .flex_col()
        .flex_none()
        .p(px(PICKER_INSET))
        .rounded(SURFACE_RADIUS)
        .border_1()
        .border_color(palette.border)
        .bg(palette.surface)
}

/// Сообщает экрану о новом значении после выбора дня или редактирования времени.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTimePickerChanged(pub NaiveDateTime); // Календарная дата и время без часового пояса.

/// Объединяет самостоятельный календарь и часы и согласует их ограничения.
pub struct DateTimePicker {
    date: Entity<DatePicker>,                 // Отдельный календарь.
    time: Entity<TimePicker>,                 // Отдельный выбор времени.
    value: Option<NaiveDateTime>,             // Общее значение, None до первого выбора.
    min: Option<NaiveDateTime>,               // Включительная нижняя граница.
    max: Option<NaiveDateTime>,               // Включительная верхняя граница.
    precision: TimePrecision,                 // Точность значения для экрана.
    options: PickerOptions,                   // Язык, палитра и доступность обоих компонентов.
    time_caption: Option<gpui::SharedString>, // Подпись времени, если она нужна на экране.
    _subscriptions: [Subscription; 2],        // Связывают действия детей с общим значением.
}

impl EventEmitter<DateTimePickerChanged> for DateTimePicker {}

impl DateTimePicker {
    /// Создаёт карточку. Если значения ещё нет, календарь открывается на переданном дне.
    pub fn new(
        value: Option<NaiveDateTime>,
        default_day: NaiveDate,
        precision: TimePrecision,
        options: PickerOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let date = cx.new(|cx| {
            DatePicker::new(value.map(|value| value.date()), default_day, options, cx).embedded()
        });
        let time = cx.new(|cx| {
            TimePicker::new(
                value.map(|value| value.time()),
                precision,
                false,
                options,
                window,
                cx,
            )
            .embedded()
        });
        let mut picker = Self {
            _subscriptions: [
                cx.subscribe(&date, |picker, _, change: &DatePickerChanged, cx| {
                    picker.select_date(change.0, cx);
                }),
                cx.subscribe(&time, |picker, _, change: &TimePickerChanged, cx| {
                    if let Some(value) = picker.value {
                        picker.select(value.date().and_time(change.0), cx);
                    }
                }),
            ],
            date,
            time,
            value: None,
            min: None,
            max: None,
            precision,
            options,
            time_caption: None,
        };
        picker.set_value(value, cx);
        picker
    }

    /// Возвращает текущее значение для формы, которая решает, когда его сохранить.
    pub fn value(&self) -> Option<NaiveDateTime> {
        self.value
    }

    /// Обновляет оба компонента из состояния формы без отправки события обратно.
    pub fn set_value(&mut self, value: Option<NaiveDateTime>, cx: &mut Context<Self>) {
        self.value = value.map(|value| {
            let value = value.date().and_time(self.precision.floor(value.time()));
            self.bounds()
                .map_or(value, |(min, max)| value.clamp(min, max))
        });
        self.sync_children(cx);
        cx.notify();
    }

    /// Обновляет включительные пределы и подстраивает уже выбранное значение.
    pub fn set_bounds(
        &mut self,
        min: Option<NaiveDateTime>,
        max: Option<NaiveDateTime>,
        cx: &mut Context<Self>,
    ) {
        self.min = min;
        self.max = max;
        self.set_value(self.value, cx);
    }

    /// Обновляет язык, палитру и доступность карточки.
    pub fn set_options(&mut self, options: PickerOptions, cx: &mut Context<Self>) {
        self.options = options;
        self.sync_children(cx);
        cx.notify();
    }

    /// Добавляет подпись к часам, например название поля, заданное формой.
    pub fn with_time_caption(mut self, caption: impl Into<gpui::SharedString>) -> Self {
        self.time_caption = Some(caption.into());
        self
    }

    /// Округляет пределы к доступной точности, возвращая None для пустого интервала.
    fn bounds(&self) -> Option<(NaiveDateTime, NaiveDateTime)> {
        let lower = match self.min {
            Some(min) => self.precision.lower_bound(min)?,
            None => NaiveDate::MIN.and_time(NaiveTime::MIN),
        };
        let upper = self.max.map_or_else(
            || NaiveDate::MAX.and_hms_opt(23, 59, 59).expect("конец дня"),
            |value| value.date().and_time(self.precision.floor(value.time())),
        );
        (lower <= upper).then_some((lower, upper))
    }

    /// Сохраняет время при смене дня, подстраивая его в день начала или конца интервала.
    fn day_value(&self, day: NaiveDate, time: NaiveTime) -> Option<NaiveDateTime> {
        let (min, max) = self.bounds()?;
        let candidate = day.and_time(self.precision.floor(time)).clamp(min, max);
        (candidate.date() == day).then_some(candidate)
    }

    /// Обновляет выбор после нажатия на день или активации с клавиатуры.
    fn select_date(&mut self, day: NaiveDate, cx: &mut Context<Self>) {
        // Клик по дню может прийти до уведомления часов о потере фокуса.
        let time = self
            .time
            .read(cx)
            .pending_value()
            .or_else(|| self.value.map(|value| value.time()))
            .unwrap_or(NaiveTime::MIN);
        if let Some(next) = self.day_value(day, time) {
            self.select(next, cx);
        }
    }

    /// Принимает пользовательское изменение и сообщает форме уже согласованное значение.
    fn select(&mut self, next: NaiveDateTime, cx: &mut Context<Self>) {
        if self.value != Some(next)
            && self.options.mode == PickerMode::Editable
            && self
                .bounds()
                .is_some_and(|(min, max)| (min..=max).contains(&next))
        {
            self.set_value(Some(next), cx);
            cx.emit(DateTimePickerChanged(next));
        }
    }

    /// Обновляет дату и ограничения часов до того, как экран получит событие.
    fn sync_children(&self, cx: &mut Context<Self>) {
        let bounds = self.bounds();
        let date = self.value.map(|value| value.date());
        let mut options = self.options;
        if bounds.is_none() && options.mode == PickerMode::Editable {
            options.mode = PickerMode::Disabled;
        }
        self.date.update(cx, |picker, cx| {
            picker.configure(
                date,
                bounds.map(|(min, _)| min.date()),
                bounds.map(|(_, max)| max.date()),
                options,
                cx,
            );
        });
        if date.is_none() && options.mode == PickerMode::Editable {
            options.mode = PickerMode::Disabled;
        }
        let min = bounds.and_then(|(min, _)| (Some(min.date()) == date).then_some(min.time()));
        let max = bounds.and_then(|(_, max)| (Some(max.date()) == date).then_some(max.time()));
        self.time.update(cx, |picker, cx| {
            picker.configure(self.value.map(|value| value.time()), min, max, options, cx);
        });
    }
}

impl Render for DateTimePicker {
    /// Рисует календарь и часы на одной поверхности с разделителем.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.options.palette(window, cx);
        card(palette)
            .id("date-time-picker")
            .debug_selector(|| "date-time-picker".into())
            .gap(px(12.))
            .child(self.date.clone())
            .child(div().h(px(1.)).bg(palette.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .children(
                        self.time_caption.clone().map(|caption| {
                            super::label(palette, super::TextRole::Heading, caption)
                        }),
                    )
                    .child(div().ml_auto().child(self.time.clone())),
            )
    }
}
