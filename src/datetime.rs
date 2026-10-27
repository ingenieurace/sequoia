//! DATE, TIME and TIMESTAMP: dates of the proleptic Gregorian calendar from
//! year 1 to 9999, times of day to the microsecond, and both together, with
//! no time zone (as PostgreSQL's `timestamp without time zone`).
//!
//! ```text
//! DATE        days since 1970-01-01, an i32       DATE '2024-02-29'
//! TIME        microseconds since midnight, an i64  TIME '13:45:00.25'
//! TIMESTAMP   microseconds since 1970-01-01 00:00  TIMESTAMP '2024-02-29 13:45:00'
//! ```
//!
//! Each counts up as time goes on, so the numbers compare as the values do,
//! and a date's day of the epoch converts to and from year, month and day
//! with Howard Hinnant's `days_from_civil` algorithm. A time prints its
//! fraction of a second only if it has one, without its trailing zeros.

use crate::error::{Result, err};

/// Microseconds in a second, and in a day.
pub const MICROS_PER_SECOND: i64 = 1_000_000;
pub const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

/// The days of 0001-01-01 and 9999-12-31, the first and last dates.
pub const MIN_DATE: i32 = -719_162;
pub const MAX_DATE: i32 = 2_932_896;

/// A part of a date or time that EXTRACT takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    /// Whole seconds.
    Second,
}

impl Field {
    pub fn from_name(name: &str) -> Option<Field> {
        Some(match name.to_uppercase().as_str() {
            "YEAR" => Field::Year,
            "MONTH" => Field::Month,
            "DAY" => Field::Day,
            "HOUR" => Field::Hour,
            "MINUTE" => Field::Minute,
            "SECOND" => Field::Second,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Field::Year => "YEAR",
            Field::Month => "MONTH",
            Field::Day => "DAY",
            Field::Hour => "HOUR",
            Field::Minute => "MINUTE",
            Field::Second => "SECOND",
        }
    }

    /// Whether a date has this part (else only a time does).
    pub fn of_date(self) -> bool {
        matches!(self, Field::Year | Field::Month | Field::Day)
    }
}

/// The day of the epoch of a date, if it is one.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i32> {
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let m = month as i64;
    let day_of_year = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some((era * 146_097 + day_of_era - 719_468) as i32)
}

/// The year, month and day of a day of the epoch.
pub fn civil_from_days(days: i32) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = year_of_era + era * 400 + (month <= 2) as i64;
    (year, month, day)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A date in range, or an error.
pub fn date(days: i64) -> Result<i32> {
    match i32::try_from(days) {
        Ok(days) if (MIN_DATE..=MAX_DATE).contains(&days) => Ok(days),
        _ => err("DATE out of range: years 1 to 9999"),
    }
}

/// A timestamp in range, or an error.
pub fn timestamp(micros: i64) -> Result<i64> {
    let lowest = MIN_DATE as i64 * MICROS_PER_DAY;
    let highest = (MAX_DATE as i64 + 1) * MICROS_PER_DAY - 1;
    match (lowest..=highest).contains(&micros) {
        true => Ok(micros),
        false => err("TIMESTAMP out of range: years 1 to 9999"),
    }
}

/// Reads `YYYY-MM-DD`.
pub fn parse_date(text: &str) -> Result<i32> {
    let bad = || err(format!("invalid DATE: '{text}'"));
    let parts: Vec<&str> = text.trim().split('-').collect();
    let [year, month, day] = parts.as_slice() else {
        return bad();
    };
    let number = |s: &str, len: usize| {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<u32>().unwrap())
    };
    match (number(year, 4), number(month, 2), number(day, 2)) {
        (Some(y), Some(m), Some(d)) => days_from_civil(y as i64, m, d).map_or_else(bad, Ok),
        _ => bad(),
    }
}

/// Reads `HH:MM[:SS[.ffffff]]`.
pub fn parse_time(text: &str) -> Result<i64> {
    let bad = || err(format!("invalid TIME: '{text}'"));
    let s = text.trim();
    let (clock, fraction) = s.split_once('.').unwrap_or((s, ""));
    let parts: Vec<&str> = clock.split(':').collect();
    let two = |s: &str| {
        (s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<i64>().unwrap())
    };
    let (hour, minute, second) = match parts.as_slice() {
        [h, m] if fraction.is_empty() => (two(h), two(m), Some(0)),
        [h, m, sec] => (two(h), two(m), two(sec)),
        _ => return bad(),
    };
    let (Some(hour), Some(minute), Some(second)) = (hour, minute, second) else {
        return bad();
    };
    if hour > 23 || minute > 59 || second > 59 || fraction.len() > 6 {
        return bad();
    }
    if !fraction.bytes().all(|b| b.is_ascii_digit()) || (s.contains('.') && fraction.is_empty()) {
        return bad();
    }
    let micros: i64 = format!("{fraction:0<6}").parse().unwrap();
    Ok(((hour * 60 + minute) * 60 + second) * MICROS_PER_SECOND + micros)
}

/// Reads `YYYY-MM-DD HH:MM[:SS[.ffffff]]` (or with `T` between them), or a
/// date alone, at midnight.
pub fn parse_timestamp(text: &str) -> Result<i64> {
    let s = text.trim();
    let (day, time) = match s.split_once([' ', 'T']) {
        Some((day, time)) => (day, Some(time)),
        None => (s, None),
    };
    let bad = || crate::error::Error(format!("invalid TIMESTAMP: '{text}'"));
    let day = parse_date(day).map_err(|_| bad())?;
    let time = match time {
        Some(time) => parse_time(time).map_err(|_| bad())?,
        None => 0,
    };
    Ok(day as i64 * MICROS_PER_DAY + time)
}

pub fn format_date(days: i32) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn format_time(micros: i64) -> String {
    let seconds = micros / MICROS_PER_SECOND;
    let fraction = micros % MICROS_PER_SECOND;
    let clock = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    match fraction {
        0 => clock,
        f => format!("{clock}.{}", format!("{f:06}").trim_end_matches('0')),
    }
}

pub fn format_timestamp(micros: i64) -> String {
    let (day, time) = split_timestamp(micros);
    format!("{} {}", format_date(day), format_time(time))
}

/// A timestamp's date and time of day.
pub fn split_timestamp(micros: i64) -> (i32, i64) {
    (
        micros.div_euclid(MICROS_PER_DAY) as i32,
        micros.rem_euclid(MICROS_PER_DAY),
    )
}

/// A part of a date.
pub fn date_field(days: i32, field: Field) -> i64 {
    let (year, month, day) = civil_from_days(days);
    match field {
        Field::Year => year,
        Field::Month => month as i64,
        Field::Day => day as i64,
        _ => 0,
    }
}

/// A part of a time of day.
pub fn time_field(micros: i64, field: Field) -> i64 {
    let seconds = micros / MICROS_PER_SECOND;
    match field {
        Field::Hour => seconds / 3600,
        Field::Minute => seconds / 60 % 60,
        Field::Second => seconds % 60,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_convert_both_ways_over_every_year() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(1, 1, 1), Some(MIN_DATE));
        assert_eq!(days_from_civil(9999, 12, 31), Some(MAX_DATE));
        assert_eq!(days_from_civil(2000, 3, 1), Some(11_017));
        for days in (MIN_DATE..=MAX_DATE).step_by(97) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), Some(days), "{y}-{m}-{d}");
        }
        assert_eq!(days_from_civil(2023, 2, 29), None);
        assert_eq!(
            days_from_civil(2024, 2, 29).map(format_date),
            Some("2024-02-29".into())
        );
        assert_eq!(days_from_civil(1900, 2, 29), None);
        assert_eq!(
            days_from_civil(2000, 2, 29).map(format_date),
            Some("2000-02-29".into())
        );
        assert_eq!(days_from_civil(0, 1, 1), None);
        assert_eq!(days_from_civil(2024, 4, 31), None);
    }

    #[test]
    fn parses_and_prints() {
        assert_eq!(format_date(parse_date("2024-02-29").unwrap()), "2024-02-29");
        assert_eq!(format_date(parse_date("0001-01-01").unwrap()), "0001-01-01");
        for bad in [
            "2024-2-29",
            "2024-02-30",
            "24-02-01",
            "2024/02/01",
            "",
            "2024-02-01x",
            "10000-01-01",
        ] {
            assert!(parse_date(bad).is_err(), "{bad}");
        }
        assert_eq!(format_time(parse_time("13:45").unwrap()), "13:45:00");
        assert_eq!(
            format_time(parse_time("13:45:07.250").unwrap()),
            "13:45:07.25"
        );
        assert_eq!(
            format_time(parse_time("00:00:00.000001").unwrap()),
            "00:00:00.000001"
        );
        assert_eq!(
            format_time(parse_time("23:59:59.999999").unwrap()),
            "23:59:59.999999"
        );
        for bad in [
            "24:00",
            "12:60",
            "12:00:60",
            "1:00",
            "12:00:00.1234567",
            "12:00:00.",
            "12",
        ] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
        let at = parse_timestamp("2024-02-29 13:45:07.5").unwrap();
        assert_eq!(format_timestamp(at), "2024-02-29 13:45:07.5");
        assert_eq!(parse_timestamp("2024-02-29T13:45:07.5").unwrap(), at);
        assert_eq!(
            format_timestamp(parse_timestamp("1969-12-31").unwrap()),
            "1969-12-31 00:00:00"
        );
        assert_eq!(format_timestamp(-1), "1969-12-31 23:59:59.999999");
        assert!(parse_timestamp("2024-02-29 25:00").is_err());
    }

    #[test]
    fn fields_and_ranges() {
        let day = parse_date("2024-07-04").unwrap();
        assert_eq!(date_field(day, Field::Year), 2024);
        assert_eq!(date_field(day, Field::Month), 7);
        assert_eq!(date_field(day, Field::Day), 4);
        let time = parse_time("13:45:07.9").unwrap();
        assert_eq!(time_field(time, Field::Hour), 13);
        assert_eq!(time_field(time, Field::Minute), 45);
        assert_eq!(time_field(time, Field::Second), 7);
        assert!(date(MAX_DATE as i64 + 1).is_err());
        assert!(date(MIN_DATE as i64).is_ok());
        assert!(timestamp(MIN_DATE as i64 * MICROS_PER_DAY - 1).is_err());
        assert_eq!(Field::from_name("month"), Some(Field::Month));
    }
}
