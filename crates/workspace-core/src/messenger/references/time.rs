//! Проверяет календарное время из сообщения, переводит его в UTC и строит обратный формат URN.

use crate::storage::HistoryTime;

/// Даёт длину даты с часовым поясом в начале текстового узла.
pub(super) fn prefix_len(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut index = 16;
    if bytes.get(index) == Some(&b':') {
        index += 3;
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            index += bytes
                .get(index..)?
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
        }
    }
    match bytes.get(index)? {
        b'Z' => Some(index + 1),
        b'+' | b'-' if bytes.get(index + 1..index + 6).is_some() => Some(index + 6),
        _ => None,
    }
}

/// Принимает ISO время с явным часовым поясом и сохраняет момент с точностью до миллисекунд.
pub(super) fn parse(value: &str) -> Option<HistoryTime> {
    if prefix_len(value)? != value.len() {
        return None;
    }
    let (date, time) = value.split_once('T')?;
    let [year, month, day] = digits::<3>(date, '-', [4, 2, 2])?;
    if year == 0 || !(1..=days_in_month(year, month)?).contains(&day) {
        return None;
    }
    let (clock, offset) = split_zone(time)?;
    let (hour_minute, seconds) = if clock.len() > 5 {
        let value = clock.get(6..)?.split_once('.');
        let (seconds, fraction) = value.unwrap_or((clock.get(6..)?, ""));
        if clock.as_bytes().get(5) != Some(&b':')
            || seconds.len() != 2
            || value.is_some_and(|(_, f)| f.is_empty() || f.len() > 3)
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        (clock.get(..5)?, (number(seconds)?, fraction))
    } else {
        (clock, (0, ""))
    };
    let [hour, minute] = digits::<2>(hour_minute, ':', [2, 2])?;
    let (seconds, fraction) = seconds;
    if hour > 23 || minute > 59 || seconds > 59 {
        return None;
    }

    // Смещение меньше суток, поэтому переход в UTC меняет дату максимум на один день.
    let minutes = hour * 60 + minute - offset;
    let (year, month, day) = shift_day(year, month, day, minutes.div_euclid(1440))?;
    let minutes = minutes.rem_euclid(1440);
    let hour = minutes / 60;
    let minute = minutes % 60;
    HistoryTime::parse(&format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{seconds:02}.{fraction:0<6}Z",
    ))
}

/// Проверяет и убирает часовой пояс, оставляя смещение в минутах.
fn split_zone(value: &str) -> Option<(&str, i32)> {
    if let Some(clock) = value.strip_suffix('Z') {
        return Some((clock, 0));
    }
    let boundary = value.len().checked_sub(6)?;
    let zone = value.get(boundary..)?;
    let sign = match zone.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let [hour, minute] = digits::<2>(&zone[1..], ':', [2, 2])?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((&value[..boundary], sign * (hour * 60 + minute)))
}

/// Записывает секундную или миллисекундную форму, которую понимает Electron.
pub(super) fn display_utc(value: &HistoryTime) -> Option<String> {
    let utc = value.as_str();
    let (seconds, fraction) = utc.strip_suffix('Z')?.split_once('.')?;
    let fraction = fraction.get(..3)?;
    if fraction == "000" {
        Some(format!("{seconds}Z"))
    } else {
        Some(format!("{seconds}.{fraction}Z"))
    }
}

/// Читает группу десятичных чисел с ожидаемыми разделителями и длинами.
fn digits<const N: usize>(value: &str, delimiter: char, lengths: [usize; N]) -> Option<[i32; N]> {
    let mut parts = value.split(delimiter);
    let mut parsed = [0; N];
    for (index, length) in lengths.into_iter().enumerate() {
        let part = parts.next()?;
        if part.len() != length {
            return None;
        }
        parsed[index] = number(part)?;
    }
    parts.next().is_none().then_some(parsed)
}

/// Отклоняет знаки и нецифровые символы до преобразования.
fn number(value: &str) -> Option<i32> {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then_some(())?;
    value.parse().ok()
}

/// Возвращает число дней месяца в обычном григорианском календаре.
fn days_in_month(year: i32, month: i32) -> Option<i32> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => Some(29),
        2 => Some(28),
        _ => None,
    }
}

/// Сдвигает дату на соседние сутки с учётом длины месяца и границы года.
fn shift_day(mut year: i32, mut month: i32, mut day: i32, delta: i32) -> Option<(i32, i32, i32)> {
    day += delta;
    if day < 1 {
        month -= 1;
        if month == 0 {
            month = 12;
            year -= 1;
        }
        day = days_in_month(year, month)?;
    } else if day > days_in_month(year, month)? {
        day = 1;
        month += 1;
        if month == 13 {
            month = 1;
            year += 1;
        }
    }
    (1..=9999).contains(&year).then_some((year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет календарь, обязательный пояс и нормализацию на границах суток и года.
    #[test]
    fn normalizes_valid_instants_and_rejects_bad_calendar_time() {
        for (input, utc) in [
            ("2026-10-13T10:02Z", "2026-10-13T10:02:00.000000Z"),
            ("2026-10-13T13:02:00+03:00", "2026-10-13T10:02:00.000000Z"),
            ("2026-01-01T00:00:12.1+01:00", "2025-12-31T23:00:12.100000Z"),
            ("2024-03-01T00:00+23:59", "2024-02-29T00:01:00.000000Z"),
            ("2024-12-31T23:00-03:00", "2025-01-01T02:00:00.000000Z"),
        ] {
            assert_eq!(parse(input).unwrap().as_str(), utc);
        }
        for input in [
            "2026-02-29T10:02Z",
            "2024-04-31T10:02Z",
            "2024-00-13T10:02Z",
            "2024-12-00T10:02Z",
            "2024-02-29T10:02",
            "2024-02-29T24:02Z",
            "2024-02-29T10:60Z",
            "2024-02-29T10:02:60Z",
            "2024-02-29T10:02:00.Z",
            "2024-02-29T10:02:00.0000Z",
            "2024-02-29T10:02+24:00",
            "2024-02-29T10:02-03:60",
            "0000-01-01T10:02Z",
        ] {
            assert!(parse(input).is_none(), "{input}");
        }
    }
}
