//! Переводит абсолютное время в календарь macOS и разбирает местное время из редактора.

use std::{cell::RefCell, ptr::NonNull};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_foundation::{
    MainThreadMarker, NSDate, NSDateFormatter, NSLocale, NSNotification, NSNotificationCenter,
    NSObject, NSString, NSSystemTimeZoneDidChangeNotification, NSTimeZone,
};
use tokio::sync::mpsc;
use workspace_core::storage::HistoryTime;

const WIRE: &str = "yyyy-MM-dd'T'HH:mm:ss'Z'";
const EDIT_FIELDS: &str = "yyyy-MM-dd HH:mm:ss";
const INPUT_LOCALE: &str = "en_US_POSIX";

thread_local! {
    static UTC_PARSER: Retained<NSDateFormatter> = formatter(INPUT_LOCALE, WIRE, true);
    static ISO_PARSER: Retained<NSDateFormatter> =
        formatter(INPUT_LOCALE, "yyyy-MM-dd'T'HH:mm:ssXXXXX", true);
    static SYSTEM_TIME: RefCell<Option<LocalTime>> = const { RefCell::new(None) };
}

/// День по григорианскому календарю в выбранном часовом поясе.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CalendarDate {
    pub year: u16, // Год.
    pub month: u8, // Месяц с января по декабрь, от 1 до 12.
    pub day: u8,   // День месяца.
}

/// Календарная дата и время для подписи одного момента в UI.
pub(crate) struct LocalStamp {
    pub date: CalendarDate, // День в часовом поясе читателя.
    hour: u8,               // Часы в формате 0–23.
    minute: u8,             // Минуты в формате 0–59.
}

impl LocalStamp {
    /// Показывает часы и минуты в 24-часовом формате.
    pub fn short_time(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
}

/// Снимок пояса для расчёта всех дат и подписей в одном списке.
#[derive(Clone)]
pub(crate) struct LocalTime {
    name: String, // Системный идентификатор пояса, например Europe/Moscow.
    fields: Retained<NSDateFormatter>, // Форматтер календарных полей для этого пояса.
}

impl LocalTime {
    /// Возвращает текущий пояс macOS, переиспользуя форматтер в этом потоке.
    pub fn system() -> Self {
        let current = unsafe { NSTimeZone::localTimeZone() };
        let name = unsafe { current.name().to_string() };
        SYSTEM_TIME.with_borrow_mut(|cached| {
            if let Some(local) = cached.as_ref().filter(|local| local.name == name) {
                return local.clone();
            }
            // Именованный пояс остаётся одним и тем же до края списка, даже если настройки меняются.
            let zone = unsafe { NSTimeZone::timeZoneWithName(&NSString::from_str(&name)) }
                .unwrap_or(current);
            let local = Self::in_zone(name, &zone);
            *cached = Some(local.clone());
            local
        })
    }

    /// Создаёт форматтер полей для заданного пояса.
    fn in_zone(name: String, zone: &NSTimeZone) -> Self {
        Self {
            name,
            fields: formatter_in_zone(INPUT_LOCALE, EDIT_FIELDS, zone),
        }
    }

    /// Создаёт расчёт для известного пояса, чтобы тесты не меняли настройки компьютера.
    #[cfg(test)]
    pub(crate) fn for_test(name: &str) -> Self {
        let zone = unsafe { NSTimeZone::timeZoneWithName(&NSString::from_str(name)) }.unwrap();
        Self::in_zone(name.to_owned(), &zone)
    }

    /// Возвращает идентификатор для проверки сохранённой группировки истории.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Преобразует серверный момент в дату и время по правилам этого пояса на нужный день.
    pub fn history_stamp(&self, utc: &HistoryTime) -> Option<LocalStamp> {
        let date = date_from_utc(utc)?;
        self.stamp(&date)
    }

    /// Показывает часы и минуты, а для другого местного дня добавляет короткую дату.
    pub fn compact_stamp(&self, utc: &HistoryTime, locale: &str) -> Option<String> {
        let date = date_from_utc(utc)?;
        let stamp = self.stamp(&date)?;
        let clock = stamp.short_time();
        // Текущий момент нужен только для сравнения календарных дней в том же поясе.
        let now = unsafe { NSDate::now() };
        if self
            .stamp(&now)
            .is_some_and(|today| today.date == stamp.date)
        {
            return Some(clock);
        }
        let calendar = unsafe {
            let format = formatter_in_zone(locale, "", &self.fields.timeZone());
            format.setLocalizedDateFormatFromTemplate(&NSString::from_str("ddMMMy"));
            format.stringFromDate(&date)
        };
        Some(format!("{calendar} {clock}"))
    }

    /// Преобразует дату создания профиля. Дата без часов уже обозначает календарный день.
    pub fn profile_date(&self, value: &str) -> Option<CalendarDate> {
        if value.len() == 10 {
            let utc = HistoryTime::parse(&format!("{value}T00:00:00Z"))?;
            date_from_utc(&utc)?;
            return calendar_date(value);
        }
        let suffix = value.get(19..)?;
        let zone = if let Some(fraction) = suffix.strip_prefix('.') {
            let end = fraction.find(|ch: char| !ch.is_ascii_digit())?;
            (end > 0).then(|| &fraction[end..])?
        } else {
            suffix
        };
        let date = ISO_PARSER.with(|parser| unsafe {
            parser.dateFromString(&NSString::from_str(&format!("{}{zone}", &value[..19])))
        })?;
        self.stamp(&date).map(|local| local.date)
    }

    /// Читает дату и время из числового формата Foundation в заданном поясе.
    fn stamp(&self, date: &NSDate) -> Option<LocalStamp> {
        let value = unsafe { self.fields.stringFromDate(date).to_string() };
        Some(LocalStamp {
            date: calendar_date(&value)?,
            hour: value.get(11..13)?.parse().ok()?,
            minute: value.get(14..16)?.parse().ok()?,
        })
    }
}

/// Возвращает местные часы и минуты для короткой подписи.
pub(crate) fn short_time(utc: &HistoryTime) -> Option<String> {
    LocalTime::system()
        .history_stamp(utc)
        .map(|local| local.short_time())
}

/// Читает календарные поля из проверенной строки Foundation или даты профиля.
fn calendar_date(value: &str) -> Option<CalendarDate> {
    Some(CalendarDate {
        year: value.get(..4)?.parse().ok()?,
        month: value.get(5..7)?.parse().ok()?,
        day: value.get(8..10)?.parse().ok()?,
    })
}

/// Создаёт форматтер с явным языком, календарём формата и часовым поясом.
fn formatter(locale: &str, pattern: &str, utc: bool) -> Retained<NSDateFormatter> {
    let zone = unsafe {
        if utc {
            NSTimeZone::timeZoneForSecondsFromGMT(0)
        } else {
            NSTimeZone::localTimeZone()
        }
    };
    formatter_in_zone(locale, pattern, &zone)
}

/// Настраивает Foundation на заданный пояс и формат строки.
fn formatter_in_zone(locale: &str, pattern: &str, zone: &NSTimeZone) -> Retained<NSDateFormatter> {
    // Используем только объекты Foundation, не зависящие от главного потока окна.
    unsafe {
        let formatter = NSDateFormatter::new();
        let locale = NSLocale::localeWithLocaleIdentifier(&NSString::from_str(locale));
        formatter.setLocale(Some(&locale));
        formatter.setTimeZone(Some(zone));
        formatter.setLenient(false);
        formatter.setDateFormat(Some(&NSString::from_str(pattern)));
        formatter
    }
}

/// Открывает момент из Core, отбрасывая дробь для отображения с точностью до секунды.
fn date_from_utc(utc: &HistoryTime) -> Option<Retained<NSDate>> {
    let seconds = format!("{}Z", utc.as_str().get(..19)?);
    UTC_PARSER.with(|parser| unsafe { parser.dateFromString(&NSString::from_str(&seconds)) })
}

/// Передаёт смену пояса из Foundation окну на главном потоке.
pub(crate) struct TimeZoneObserver {
    center: Retained<NSNotificationCenter>, // Источник системного уведомления.
    observer: Retained<NSObject>,           // Метка для снятия подписки.
    _main: MainThreadMarker,                // Сохраняет владение подпиской на главном потоке.
}

impl TimeZoneObserver {
    /// Подписывается на изменение пояса macOS. Один ожидающий сигнал уже обновит окно.
    pub fn subscribe(main: MainThreadMarker, changed: mpsc::Sender<()>) -> Self {
        let callback = RcBlock::new(move |_: NonNull<NSNotification>| {
            let _ = changed.try_send(());
        });
        let (center, observer) = unsafe {
            let center = NSNotificationCenter::defaultCenter();
            let observer = center.addObserverForName_object_queue_usingBlock(
                Some(NSSystemTimeZoneDidChangeNotification),
                None,
                None,
                &callback,
            );
            (center, observer)
        };
        Self {
            center,
            observer,
            _main: main,
        }
    }
}

impl Drop for TimeZoneObserver {
    /// Снимает подписку при закрытии окна.
    fn drop(&mut self) {
        unsafe { self.center.removeObserver(&self.observer) };
    }
}

/// Форматирует время ключа в системном поясе и на языке аккаунта.
pub(crate) fn format_unix_millis(unix_ms: u64, locale: &str) -> String {
    unsafe {
        let date = NSDate::dateWithTimeIntervalSince1970(unix_ms as f64 / 1000.);
        formatter(locale, "d MMM yyyy, HH:mm", false)
            .stringFromDate(&date)
            .to_string()
    }
}

/// Возвращает текущую местную дату и время по отдельности для двух полей формы.
pub(crate) fn local_time_fields() -> (String, String) {
    let value = unsafe {
        formatter(INPUT_LOCALE, EDIT_FIELDS, false)
            .stringFromDate(&NSDate::date())
            .to_string()
    };
    let (date, time) = value.split_once(' ').unwrap_or_default();
    (date.to_owned(), time.to_owned())
}

/// Переводит введённые местные дату и время в UTC при полном совпадении после разбора.
pub(crate) fn parse_local_time(date: &str, time: &str) -> Option<HistoryTime> {
    let date = date.trim();
    let time = time.trim();
    let value = if time.len() == 5 {
        format!("{date} {time}:00")
    } else {
        format!("{date} {time}")
    };
    if value.len() != 19 {
        return None;
    }
    let local = formatter(INPUT_LOCALE, EDIT_FIELDS, false);
    // Обратная проверка отсекает неполные даты и моменты между переводом часов.
    unsafe {
        let date = local.dateFromString(&NSString::from_str(&value))?;
        if local.stringFromDate(&date).to_string() != value {
            return None;
        }
        HistoryTime::parse(
            &formatter(INPUT_LOCALE, WIRE, true)
                .stringFromDate(&date)
                .to_string(),
        )
    }
}

/// Показывает момент в часовом поясе системы и языке аккаунта.
pub(crate) fn format_local_time(utc: &HistoryTime, locale: &str) -> String {
    let Some(date) = date_from_utc(utc) else {
        return utc.as_str().to_owned();
    };
    let pattern = if utc.as_str().get(17..19) == Some("00") {
        "EEE, d MMM yyyy, HH:mm"
    } else {
        "EEE, d MMM yyyy, HH:mm:ss"
    };
    unsafe {
        formatter(locale, pattern, false)
            .stringFromDate(&date)
            .to_string()
    }
}

/// Показывает название зоны и её смещение для указанного момента, включая летнее время.
pub(crate) fn format_local_time_zone(utc: &HistoryTime, locale: &str) -> String {
    let Some(date) = date_from_utc(utc) else {
        return String::new();
    };
    let local = formatter(locale, "zzzz", false);
    unsafe {
        let offset = local.timeZone().secondsFromGMTForDate(&date);
        let sign = if offset < 0 { '-' } else { '+' };
        let minutes = offset.unsigned_abs() / 60;
        format!(
            "{} (UTC{sign}{:02}:{:02})",
            local.stringFromDate(&date),
            minutes / 60,
            minutes % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Показывает короткую дату на языке читателя и минуты в его часовом поясе.
    #[test]
    fn formats_compact_stamp_in_the_readers_locale_and_zone() {
        let utc = HistoryTime::parse("2020-01-01T22:15:57Z").unwrap();
        let moscow = LocalTime::for_test("Europe/Moscow");
        let russian = moscow.compact_stamp(&utc, "ru").unwrap();
        assert!(russian.starts_with("02 янв."), "{russian}");
        assert!(russian.ends_with("01:15"), "{russian}");
        let new_york = LocalTime::for_test("America/New_York");
        let english = new_york.compact_stamp(&utc, "en").unwrap();
        assert!(english.starts_with("Jan 01,"), "{english}");
        assert!(english.ends_with("17:15"), "{english}");
    }

    /// Проверяет часы и календарный день одного момента в разных поясах.
    #[test]
    fn converts_history_across_midnight_and_year() {
        let utc = HistoryTime::parse("2026-12-31T22:15:00.123456Z").unwrap();
        let moscow = LocalTime::for_test("Europe/Moscow")
            .history_stamp(&utc)
            .unwrap();
        let india = LocalTime::for_test("Asia/Kolkata")
            .history_stamp(&utc)
            .unwrap();
        let new_york = LocalTime::for_test("America/New_York")
            .history_stamp(&utc)
            .unwrap();
        assert_eq!(moscow.short_time(), "01:15");
        assert_eq!(india.short_time(), "03:45");
        assert_eq!(
            moscow.date,
            CalendarDate {
                year: 2027,
                month: 1,
                day: 1
            }
        );
        assert_eq!(new_york.short_time(), "17:15");
        assert_eq!(
            new_york.date,
            CalendarDate {
                year: 2026,
                month: 12,
                day: 31
            }
        );
    }

    /// Проверяет историческое смещение для обоих переходов летнего времени.
    #[test]
    fn applies_offset_at_the_message_instant() {
        let local = LocalTime::for_test("America/New_York");
        for (utc, expected) in [
            ("2026-03-08T06:59:59Z", "01:59"),
            ("2026-03-08T07:00:00Z", "03:00"),
            ("2026-11-01T05:30:00Z", "01:30"),
            ("2026-11-01T06:30:00Z", "01:30"),
        ] {
            let stamp = local
                .history_stamp(&HistoryTime::parse(utc).unwrap())
                .unwrap();
            assert_eq!(stamp.short_time(), expected);
        }
    }

    /// Различает абсолютное время профиля и дату, которой часовой пояс не нужен.
    #[test]
    fn handles_profile_dates_and_rfc3339() {
        let local = LocalTime::for_test("Europe/Moscow");
        let first = CalendarDate {
            year: 2027,
            month: 1,
            day: 1,
        };
        assert_eq!(
            local.profile_date("2026-12-31T22:15:00.123456Z"),
            Some(first)
        );
        assert_eq!(local.profile_date("2026-12-31T23:15:00+01:00"), Some(first));
        assert_eq!(local.profile_date("2027-01-01"), Some(first));
        assert_eq!(local.profile_date("2026-02-29"), None);
        assert_eq!(local.profile_date("2026-12-31T23:15:00"), None);
        assert_eq!(local.profile_date("2026-12-31T23:15:00.badZ"), None);
    }

    /// Принимает полные местные дату и время и отсекает несуществующий день.
    #[test]
    fn validates_edit_fields() {
        assert!(parse_local_time("2026-02-28", "13:45:22").is_some());
        assert!(parse_local_time("2026-02-28", "13:45").is_some());
        assert!(parse_local_time("2026-02-29", "13:45:22").is_none());
        assert!(parse_local_time("2026-02-28", "25:45:22").is_none());
    }
}
