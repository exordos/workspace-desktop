//! Преобразует серверное время в календарные поля Windows и Linux.

use chrono::{
    DateTime, Datelike, Local, NaiveDate, NaiveDateTime, SecondsFormat, Timelike, Utc,
    offset::TimeZone,
};
use workspace_core::storage::HistoryTime;

/// День по григорианскому календарю в часовом поясе читателя.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CalendarDate {
    pub year: u16, // Год.
    pub month: u8, // Месяц с января по декабрь, от 1 до 12.
    pub day: u8,   // День месяца.
}

impl CalendarDate {
    /// Преобразует календарную дату в поля, которые читает история сообщений.
    fn from_date(date: NaiveDate) -> Option<Self> {
        Some(Self {
            year: date.year().try_into().ok()?,
            month: date.month() as u8,
            day: date.day() as u8,
        })
    }
}

/// Местный момент, используемый для подписи и группировки истории.
pub(crate) struct LocalStamp {
    pub date: CalendarDate, // День по местному времени.
    clock: NaiveDateTime,   // Календарные поля местного времени.
}

impl LocalStamp {
    /// Показывает часы и минуты в 24-часовом формате.
    pub fn short_time(&self) -> String {
        self.clock.format("%H:%M").to_string()
    }
}

/// Системный часовой пояс и признак для обновления сохранённой группировки.
#[derive(Clone)]
pub(crate) struct LocalTime {
    name: String, // Текущее смещение системы, которое сравнивает история.
}

impl LocalTime {
    /// Читает текущее смещение. Моменты истории рассчитываются по правилам ОС для их даты.
    pub fn system() -> Self {
        Self {
            name: Local::now().offset().to_string(),
        }
    }

    /// Возвращает текущее смещение для проверки календарной группировки.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Преобразует абсолютный момент в дату и время по местному календарю.
    pub fn history_stamp(&self, utc: &HistoryTime) -> Option<LocalStamp> {
        let clock = local_datetime_for_time(utc)?;
        Some(LocalStamp {
            date: CalendarDate::from_date(clock.date())?,
            clock,
        })
    }

    /// Добавляет дату для сообщений за пределами текущего календарного дня.
    pub fn compact_stamp(&self, utc: &HistoryTime, locale: &str) -> Option<String> {
        let stamp = self.history_stamp(utc)?;
        let clock = stamp.short_time();
        if stamp.clock.date() == Local::now().date_naive() {
            return Some(clock);
        }
        Some(format!(
            "{} {clock}",
            stamp.clock.format(date_pattern(locale))
        ))
    }

    /// Переводит дату профиля с зоной в местный день либо оставляет дату без часов.
    pub fn profile_date(&self, value: &str) -> Option<CalendarDate> {
        let date = if value.len() == 10 {
            NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?
        } else {
            DateTime::parse_from_rfc3339(value)
                .ok()?
                .with_timezone(&Local)
                .date_naive()
        };
        CalendarDate::from_date(date)
    }
}

/// Возвращает часы и минуты для короткой подписи.
pub(crate) fn short_time(utc: &HistoryTime) -> Option<String> {
    Some(LocalTime::system().history_stamp(utc)?.short_time())
}

/// Форматирует время ключа в языке аккаунта и часовом поясе системы.
pub(crate) fn format_unix_millis(unix_ms: u64, locale: &str) -> String {
    let Some(instant) = i64::try_from(unix_ms)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
    else {
        return String::new();
    };
    let local = instant.with_timezone(&Local);
    format!(
        "{}, {}",
        local.format(date_pattern(locale)),
        local.format("%H:%M")
    )
}

/// Возвращает местное время для открытия редактора, с точностью до секунды.
pub(crate) fn current_local_datetime() -> NaiveDateTime {
    Local::now()
        .naive_local()
        .with_nanosecond(0)
        .expect("целая секунда")
}

/// Переводит местные поля в UTC. Для повторённого часа выбирает ранний момент.
pub(crate) fn local_datetime_to_utc(local: NaiveDateTime) -> Option<HistoryTime> {
    let instant = Local.from_local_datetime(&local).earliest()?.to_utc();
    HistoryTime::parse(&instant.to_rfc3339_opts(SecondsFormat::Micros, true))
}

/// Возвращает точные местные поля сохранённого момента.
pub(crate) fn local_datetime_for_time(utc: &HistoryTime) -> Option<NaiveDateTime> {
    Some(local_instant(utc)?.naive_local())
}

/// Показывает дату и время в часовом поясе системы.
pub(crate) fn format_local_time(utc: &HistoryTime, locale: &str) -> String {
    let Some(local) = local_instant(utc) else {
        return utc.as_str().to_owned();
    };
    let time = if local.second() == 0 {
        "%H:%M"
    } else {
        "%H:%M:%S"
    };
    format!(
        "{}, {}",
        local.format(date_pattern(locale)),
        local.format(time)
    )
}

/// Показывает смещение UTC, действовавшее на момент события.
pub(crate) fn format_local_time_zone(utc: &HistoryTime, _: &str) -> String {
    local_instant(utc).map_or_else(String::new, |local| format!("UTC{}", local.format("%:z")))
}

/// Применяет правила системного пояса на момент события, включая переходы времени.
fn local_instant(utc: &HistoryTime) -> Option<DateTime<Local>> {
    utc.as_str()
        .parse::<DateTime<Utc>>()
        .ok()
        .map(|instant| instant.with_timezone(&Local))
}

/// Выбирает числовую дату для языка текущего аккаунта.
fn date_pattern(locale: &str) -> &'static str {
    if locale.starts_with("en") {
        "%m/%d/%Y"
    } else {
        "%d.%m.%Y"
    }
}
