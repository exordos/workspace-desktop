//! Показывает часы в 24-часовом формате и обрабатывает ввод по отдельным разрядам.

use chrono::{NaiveTime, Timelike};
use gpui::{
    Context, Div, FocusHandle, KeyDownEvent, Pixels, ScrollWheelEvent, Stateful, Subscription,
    Window, div, prelude::*, px,
};

use super::{PickerMode, PickerOptions, TimePrecision, card, time_from_seconds};
use crate::gpui_client::ui::{Icon, Palette, TextRole, label, popup, theme::CONTROL_RADIUS};

const CLOCK_HEIGHT: Pixels = px(30.);
const WHEEL_PADDING: Pixels = px(5.);

/// Сообщает о новом времени после ввода, нажатия стрелки или прокрутки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimePickerChanged(pub NaiveTime); // Выбранное время без даты и часового пояса.

/// Разряд цифровых часов в порядке отображения и обхода клавишей Tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Segment {
    Hour,   // Часы от 0 до 23.
    Minute, // Минуты от 0 до 59.
    Second, // Секунды от 0 до 59.
}

impl Segment {
    const ALL: [Self; 3] = [Self::Hour, Self::Minute, Self::Second];

    /// Возвращает число секунд в единице разряда.
    fn unit(self) -> u32 {
        match self {
            Self::Hour => 3600,
            Self::Minute => 60,
            Self::Second => 1,
        }
    }

    /// Возвращает наибольшее число для разряда.
    fn max(self) -> u32 {
        if self == Self::Hour { 23 } else { 59 }
    }

    /// Читает разряд из количества секунд с начала суток.
    fn part(self, seconds: u32) -> u32 {
        seconds / self.unit() % (self.max() + 1)
    }

    /// Возвращает ключ подписи для подсказки у цифр.
    fn label_key(self) -> &'static str {
        match self {
            Self::Hour => "timePicker.hours",
            Self::Minute => "timePicker.minutes",
            Self::Second => "timePicker.seconds",
        }
    }

    /// Возвращает стабильный ключ элемента для отрисовки и проверок.
    fn id(self) -> &'static str {
        match self {
            Self::Hour => "time-picker-hour",
            Self::Minute => "time-picker-minute",
            Self::Second => "time-picker-second",
        }
    }
}

/// Самостоятельный выбор времени с включительными границами внутри суток.
pub struct TimePicker {
    value: Option<NaiveTime>,      // Выбранное время, None до первого ввода.
    min: u32,                      // Нижняя граница в секундах с начала суток.
    max: u32,                      // Верхняя граница в секундах с начала суток.
    precision: TimePrecision,      // Показываемые разряды и точность результата.
    show_arrows: bool,             // Показывать кнопки рядом с каждым разрядом.
    options: PickerOptions,        // Язык, палитра и доступность ввода.
    focus: [FocusHandle; 3],       // Независимая остановка Tab для каждого видимого разряда.
    draft: Option<(Segment, u32)>, // Первая введённая цифра до подтверждения.
    wheel: Option<(Segment, f32)>, // Накопленное движение колеса в пикселях для одного разряда.
    _blur: [Subscription; 3],      // Подтверждают цифру при уходе из разряда.
    embedded: bool,                // Фон и отступы уже заданы общей карточкой.
}

impl gpui::EventEmitter<TimePickerChanged> for TimePicker {}

impl TimePicker {
    /// Создаёт часы с необязательным начальным значением и кнопками шага.
    pub fn new(
        value: Option<NaiveTime>,
        precision: TimePrecision,
        show_arrows: bool,
        options: PickerOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = Segment::ALL.map(|_| cx.focus_handle().tab_stop(true));
        let blur = Segment::ALL.map(|segment| {
            cx.on_blur(&focus[segment as usize], window, move |picker, _, cx| {
                if let Some(part) = picker.draft_part(segment) {
                    picker.commit(segment, part, cx);
                }
            })
        });
        Self {
            value: value.map(|time| precision.floor(time)),
            min: 0,
            max: 86_400 - precision.step(),
            precision,
            show_arrows,
            options,
            focus,
            draft: None,
            wheel: None,
            _blur: blur,
            embedded: false,
        }
    }

    /// Оставляет у часов только цифровое поле для общей карточки даты и времени.
    pub(super) fn embedded(mut self) -> Self {
        self.embedded = true;
        self
    }

    /// Возвращает точное время для вызывающей формы.
    pub fn value(&self) -> Option<NaiveTime> {
        self.value
    }

    /// Возвращает допустимый незавершённый ввод при переходе из часов в календарь.
    pub(super) fn pending_value(&self) -> Option<NaiveTime> {
        let (segment, part) = self.draft?;
        self.replace(segment, part)
    }

    /// Показывает время, переданное формой, без отправки события обратно.
    pub fn set_value(&mut self, value: Option<NaiveTime>, cx: &mut Context<Self>) {
        self.value = value.map(|time| self.precision.floor(time));
        self.draft = None;
        cx.notify();
    }

    /// Задаёт пределы внутри суток, округляя их к ближайшим допустимым разрядам.
    pub fn set_bounds(
        &mut self,
        min: Option<NaiveTime>,
        max: Option<NaiveTime>,
        cx: &mut Context<Self>,
    ) {
        let step = self.precision.step();
        self.min = min.map_or(0, |time| {
            (time.num_seconds_from_midnight() + u32::from(time.nanosecond() != 0)).div_ceil(step)
                * step
        });
        self.max = max.map_or(86_400 - step, |time| {
            self.precision.floor(time).num_seconds_from_midnight()
        });
        self.draft = None;
        cx.notify();
    }

    /// Обновляет язык, палитру и доступность часов.
    pub fn set_options(&mut self, options: PickerOptions, cx: &mut Context<Self>) {
        self.options = options;
        if !self.editable() {
            self.draft = None;
        }
        cx.notify();
    }

    /// Обновляет время и доступный интервал после изменения общего выбора.
    pub(super) fn configure(
        &mut self,
        value: Option<NaiveTime>,
        min: Option<NaiveTime>,
        max: Option<NaiveTime>,
        options: PickerOptions,
        cx: &mut Context<Self>,
    ) {
        self.options = options;
        self.set_bounds(min, max, cx);
        self.set_value(value, cx);
    }

    /// Проверяет доступность ввода и наличие хотя бы одного допустимого времени.
    fn editable(&self) -> bool {
        self.options.mode == PickerMode::Editable && self.min <= self.max
    }

    /// Возвращает текущую основу для правки, включая значение до первого ввода.
    fn base(&self) -> u32 {
        self.value
            .map_or(self.min, |time| time.num_seconds_from_midnight())
    }

    /// Заменяет один разряд, подстраивая остальные, если мешает граница интервала.
    fn replace(&self, segment: Segment, part: u32) -> Option<NaiveTime> {
        if !self.editable() || part > segment.max() {
            return None;
        }
        let base = self.base();
        let delta = (part as i32 - segment.part(base) as i32) * segment.unit() as i32;
        let candidate = base.checked_add_signed(delta)?.clamp(self.min, self.max);
        if segment.part(candidate) == part {
            time_from_seconds(candidate)
        } else {
            None
        }
    }

    /// Возвращает ближайшее допустимое значение в направлении стрелки.
    fn step(&self, segment: Segment, direction: i32) -> Option<NaiveTime> {
        let part = self
            .draft_part(segment)
            .unwrap_or_else(|| segment.part(self.base()));
        let mut next = part as i32
            + if self.value.is_some() || self.draft.is_some() {
                direction
            } else {
                0
            };
        while (0..=segment.max() as i32).contains(&next) {
            if let Some(time) = self.replace(segment, next as u32) {
                if Some(time) != self.value {
                    return Some(time);
                }
            }
            next += direction;
        }
        None
    }

    /// Принимает новое время и сообщает о нём форме.
    fn select(&mut self, next: Option<NaiveTime>, cx: &mut Context<Self>) {
        self.draft = None;
        if let Some(time) = next.filter(|&time| Some(time) != self.value && self.editable()) {
            self.value = Some(time);
            cx.emit(TimePickerChanged(time));
        }
        cx.notify();
    }

    /// Возвращает введённую цифру, если она относится к этому разряду.
    fn draft_part(&self, segment: Segment) -> Option<u32> {
        self.draft
            .and_then(|(owner, part)| (owner == segment).then_some(part))
    }

    /// Подтверждает подходящее число или возвращает прежнее отображение.
    fn commit(&mut self, segment: Segment, part: u32, cx: &mut Context<Self>) {
        self.select(self.replace(segment, part), cx);
    }

    /// Обрабатывает набор цифр, подтверждение, отмену и шаг с клавиатуры.
    fn handle_key(&mut self, segment: Segment, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.editable() || event.keystroke.modifiers.modified() {
            return;
        }
        let draft = self.draft_part(segment);
        match event.keystroke.key.as_str() {
            "up" => self.select(self.step(segment, 1), cx),
            "down" => self.select(self.step(segment, -1), cx),
            "enter" if draft.is_some() => self.commit(segment, draft.expect("введённая цифра"), cx),
            "escape" | "backspace" | "delete" if draft.is_some() => self.select(None, cx),
            key => {
                let Some(digit) = key.parse::<u32>().ok().filter(|_| key.len() == 1) else {
                    return;
                };
                if let Some(first) = draft {
                    self.commit(segment, first * 10 + digit, cx);
                } else {
                    self.draft = Some((segment, digit));
                    cx.notify();
                }
            }
        }
        cx.stop_propagation();
    }

    /// Меняет разряд после накопления небольшого вертикального движения колеса.
    fn scroll(&mut self, segment: Segment, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        const STEP: f32 = 24.;
        let delta = event.delta.pixel_delta(px(STEP));
        let x = f32::from(delta.x);
        let y = f32::from(delta.y);
        if !self.editable() || event.modifiers.modified() || y == 0. || x.abs() > y.abs() {
            return;
        }
        cx.stop_propagation();
        let previous = self
            .wheel
            .filter(|&(owner, previous)| owner == segment && previous.signum() == y.signum());
        let delta = previous.map_or(0., |(_, delta)| delta) + y.clamp(-STEP, STEP);
        if delta.abs() >= STEP {
            self.wheel = None;
            self.select(self.step(segment, if delta > 0. { 1 } else { -1 }), cx);
        } else {
            self.wheel = Some((segment, delta));
        }
    }

    /// Рисует кнопку мышиного шага, не отвлекая фокус от цифр.
    fn arrow(
        &self,
        segment: Segment,
        direction: i32,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let icon = if direction > 0 {
            Icon::ChevronUp
        } else {
            Icon::ChevronDown
        };
        let enabled = self.step(segment, direction).is_some();
        let id = (segment.id(), if direction > 0 { 0_u32 } else { 1 });
        div()
            .id(id)
            .debug_selector(move || {
                format!("{}-{}", id.0, if direction > 0 { "up" } else { "down" })
            })
            .w(px(14.))
            .h(px(13.))
            .flex()
            .justify_center()
            .items_center()
            .rounded_sm()
            .child(icon.element(
                px(10.),
                if enabled {
                    palette.text
                } else {
                    palette.disabled
                },
            ))
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(move |style| style.bg(palette.card_hover))
                    .on_click(cx.listener(move |picker, _, _, cx| {
                        picker.select(picker.step(segment, direction), cx);
                    }))
            })
    }

    /// Рисует разряд с подсказкой, фокусом и обработчиками ввода.
    fn digit(&self, segment: Segment, palette: Palette, cx: &Context<Self>) -> Stateful<Div> {
        let text = if let Some(part) = self.draft_part(segment) {
            part.to_string()
        } else {
            self.value.map_or_else(
                || "–".to_owned(),
                |time| format!("{:02}", segment.part(time.num_seconds_from_midnight())),
            )
        };
        let title = self.options.language.tr(segment.label_key());
        let digit = div()
            .id(segment.id())
            .debug_selector(move || segment.id().into())
            .w(px(26.))
            .h(px(26.))
            .flex()
            .justify_center()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(gpui::transparent_black())
            .child(label(palette, TextRole::Heading, text).when(
                !self.editable() && self.options.mode != PickerMode::ReadOnly,
                |text| text.text_color(palette.text_muted),
            ));
        if !self.editable() {
            return digit;
        }
        digit
            .track_focus(&self.focus[segment as usize])
            .tab_stop(true)
            .focus(move |style| style.bg(palette.card_hover))
            .when(palette.keyboard_focus_visible(), |digit| {
                digit.focus(move |style| style.bg(palette.card_hover).border_color(palette.focus))
            })
            .cursor_text()
            .on_click(cx.listener(move |picker, _, window, _| {
                window.focus(&picker.focus[segment as usize]);
            }))
            .on_key_down(cx.listener(move |picker, key, _, cx| picker.handle_key(segment, key, cx)))
            .tooltip(move |_, cx| popup::tooltip(palette, title.into(), cx))
    }

    /// Даёт каждому разряду полную ширину и высоту его части поля для прокрутки.
    fn segment(
        &self,
        segment: Segment,
        arrows: bool,
        palette: Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(("time-part", segment as usize))
            .relative()
            .flex_1()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .when(segment != Segment::Hour, |part| {
                part.child(
                    div()
                        .absolute()
                        .top_0()
                        .left(px(-5.))
                        .w(px(10.))
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(label(palette, TextRole::Heading, ":")),
                )
            })
            .child(self.digit(segment, palette, cx))
            .when(arrows, |part| {
                part.child(
                    div()
                        .flex()
                        .flex_col()
                        .child(self.arrow(segment, 1, palette, cx))
                        .child(self.arrow(segment, -1, palette, cx)),
                )
            })
            .when(self.editable(), |part| {
                part.on_scroll_wheel(cx.listener(move |picker, event, _, cx| {
                    picker.scroll(segment, event, cx);
                }))
                .on_hover(cx.listener(move |picker, hovered: &bool, _, _| {
                    if !hovered && picker.wheel.is_some_and(|(owner, _)| owner == segment) {
                        picker.wheel = None;
                    }
                }))
            })
    }
}

impl Render for TimePicker {
    /// Показывает часы отдельно или компактным полем под календарём.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.options.palette(window, cx);
        let count = if matches!(self.precision, TimePrecision::Seconds) {
            3
        } else {
            2
        };
        let arrows = self.show_arrows && self.options.mode != PickerMode::ReadOnly;
        let width = px(if arrows { 50. } else { 37.5 }) * count as f32;
        // Рамка сохраняет размеры из макета. Колесо работает и на 5 px вокруг неё.
        let clock = div()
            .id("time-picker-clock")
            .debug_selector(|| "time-picker-clock".into())
            .relative()
            .flex()
            .flex_none()
            .w(width + WHEEL_PADDING * 2.)
            .h(CLOCK_HEIGHT + WHEEL_PADDING * 2.)
            .ml(-WHEEL_PADDING)
            .mr(-WHEEL_PADDING)
            .my(-WHEEL_PADDING)
            .child(
                div()
                    .id("time-picker-frame")
                    .debug_selector(|| "time-picker-frame".into())
                    .absolute()
                    .top(WHEEL_PADDING)
                    .right(WHEEL_PADDING)
                    .bottom(WHEEL_PADDING)
                    .left(WHEEL_PADDING)
                    .rounded(CONTROL_RADIUS)
                    .border_1()
                    .border_color(palette.border),
            )
            .children(
                Segment::ALL
                    .into_iter()
                    .take(count)
                    .map(|segment| self.segment(segment, arrows, palette, cx)),
            );
        if self.embedded {
            div().child(clock)
        } else {
            card(palette).items_start().child(clock)
        }
    }
}
