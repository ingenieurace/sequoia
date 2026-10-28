//! DATE, TIME and TIMESTAMP: dates of the proleptic Gregorian calendar from
//! year 1 to 9999, times of day to the microsecond, and both together, with
//! no time zone (as PostgreSQL's `timestamp without time zone`); TIMESTAMP
//! WITH TIME ZONE, an instant; and INTERVAL, a span of time.
//!
//! ```text
//! DATE         days since 1970-01-01, an i32       DATE '2024-02-29'
//! TIME         microseconds since midnight, an i64  TIME '13:45:00.25'
//! TIMESTAMP    microseconds since 1970-01-01 00:00  TIMESTAMP '2024-02-29 13:45:00'
//! TIMESTAMPTZ  microseconds since 1970-01-01 00:00 UTC, an i64
//!                                                  TIMESTAMPTZ '2024-02-29 13:45:00+02'
//! INTERVAL     months, days, microseconds (i32, i32, i64)
//!                                                  INTERVAL '1 year 2 mons 3 days 04:05:06'
//! ```
//!
//! **Time zones.** A TIMESTAMP WITH TIME ZONE is an instant, kept in UTC: a
//! zone written with it says how to read the wall time, and is not kept. The
//! session's time zone is always UTC, so it prints in UTC and a timestamp
//! without zone converts to one as UTC; `AT TIME ZONE` converts to and from
//! other zones. A zone is UTC or an offset from it: with no time zone
//! database (and no dependencies), there are no zones by name, and so no
//! daylight saving time.
//!
//! Each counts up as time goes on, so the numbers compare as the values do,
//! and a date's day of the epoch converts to and from year, month and day
//! with Howard Hinnant's `days_from_civil` algorithm. A time prints its
//! fraction of a second only if it has one, without its trailing zeros.

use std::cmp::Ordering;

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
    /// The quarter of the year, from 1.
    Quarter,
    /// The day of the week, from 0 for Sunday.
    Dow,
    /// The day of the year, from 1.
    Doy,
    /// Seconds since 1970-01-01 00:00 (UTC), as a REAL; of an interval,
    /// its length in seconds, a year as 365.25 days and a month as 30.
    Epoch,
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
            "QUARTER" => Field::Quarter,
            "DOW" => Field::Dow,
            "DOY" => Field::Doy,
            "EPOCH" => Field::Epoch,
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
            Field::Quarter => "QUARTER",
            Field::Dow => "DOW",
            Field::Doy => "DOY",
            Field::Epoch => "EPOCH",
        }
    }

    /// Whether a date has this part (else only a time does, but for EPOCH,
    /// which both have).
    pub fn of_date(self) -> bool {
        matches!(
            self,
            Field::Year | Field::Month | Field::Day | Field::Quarter | Field::Dow | Field::Doy
        )
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

// ---- INTERVAL ----

/// An INTERVAL: months, days and microseconds, each with a sign of its own,
/// as PostgreSQL keeps them: a month is not always as many days, so adding
/// `1 mon` to a date moves it to the same day of the next month, and adding
/// `30 days` moves it 30 days.
///
/// Intervals order by their length, a month as 30 days and a day as 24
/// hours, then by their months, days and microseconds; so two are equal
/// only if all three are (where PostgreSQL has `1 mon` equal `30 days`),
/// which keeps them equal exactly when their sort keys are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    /// Its length in microseconds, a month as 30 days and a day as 24 hours.
    pub fn length(self) -> i128 {
        (self.months as i128 * 30 + self.days as i128) * MICROS_PER_DAY as i128
            + self.micros as i128
    }

    pub fn compare(self, other: Interval) -> Ordering {
        (self.length(), self.months, self.days, self.micros).cmp(&(
            other.length(),
            other.months,
            other.days,
            other.micros,
        ))
    }

    pub fn checked_add(self, other: Interval) -> Result<Interval> {
        Ok(Interval {
            months: self.months.checked_add(other.months).ok_or_else(too_long)?,
            days: self.days.checked_add(other.days).ok_or_else(too_long)?,
            micros: self.micros.checked_add(other.micros).ok_or_else(too_long)?,
        })
    }

    pub fn checked_neg(self) -> Result<Interval> {
        Interval::default().checked_sub(self)
    }

    pub fn checked_sub(self, other: Interval) -> Result<Interval> {
        Ok(Interval {
            months: self.months.checked_sub(other.months).ok_or_else(too_long)?,
            days: self.days.checked_sub(other.days).ok_or_else(too_long)?,
            micros: self.micros.checked_sub(other.micros).ok_or_else(too_long)?,
        })
    }

    /// The interval times `factor`, as PostgreSQL computes it: the fraction
    /// of a month that comes of it goes to the days (30 a month), and the
    /// fraction of a day to the microseconds.
    pub fn scale(self, factor: f64) -> Result<Interval> {
        Interval::from_parts(
            self.months as f64 * factor,
            self.days as f64 * factor,
            self.micros as f64 * factor,
        )
    }

    /// An interval of these months, days and microseconds, which may have
    /// fractions: a month's go to the days, and a day's to the microseconds.
    fn from_parts(months: f64, days: f64, micros: f64) -> Result<Interval> {
        let whole_months = months.trunc();
        let days = days + (months - whole_months) * 30.0;
        let whole_days = days.trunc();
        let micros = (micros + (days - whole_days) * MICROS_PER_DAY as f64).round();
        let fits = |x: f64, max: f64| x.is_finite() && x.abs() <= max;
        if !fits(whole_months, i32::MAX as f64)
            || !fits(whole_days, i32::MAX as f64)
            || !fits(micros, 9.2e18)
        {
            return Err(too_long());
        }
        Ok(Interval {
            months: whole_months as i32,
            days: whole_days as i32,
            micros: micros as i64,
        })
    }

    /// Reads an interval as PostgreSQL writes them: quantities with units
    /// (`1 year 2 mons 3 days`, `90 minutes`, `1.5 hours`, `-2 weeks`), a
    /// time of day's form for hours, minutes and seconds (`04:05:06.5`, which
    /// may pass 24 hours), a number alone for seconds, and `ago` at the end
    /// to negate it all. Units may be singular or plural, or short: `y`,
    /// `mon`, `w`, `d`, `h`, `min`, `s`, `ms`, `us`, and the decades,
    /// centuries and millennia.
    pub fn parse(text: &str) -> Result<Interval> {
        let bad = || err(format!("invalid INTERVAL: '{text}'"));
        let lower = text.trim().to_lowercase();
        let mut words: Vec<String> = Vec::new();
        // A number and its unit may be written together: `3days`.
        for word in lower.trim_start_matches('@').split_whitespace() {
            let numeric = word.starts_with(|c: char| c.is_ascii_digit() || "+-.".contains(c));
            let split = word
                .char_indices()
                .find(|&(i, c)| i > 0 && c.is_alphabetic())
                .map(|(i, _)| i)
                .filter(|&i| numeric && !word[..i].contains(':'));
            match split {
                Some(i) => words.extend([word[..i].to_string(), word[i..].to_string()]),
                None => words.push(word.to_string()),
            }
        }
        let ago = words.last().is_some_and(|w| w == "ago");
        if ago {
            words.pop();
        }
        if words.is_empty() {
            return bad();
        }
        let (mut months, mut days, mut micros) = (0f64, 0f64, 0f64);
        let mut i = 0;
        while i < words.len() {
            let word = &words[i];
            if word.contains(':') {
                micros += clock(word).ok_or_else(|| bad().unwrap_err())?;
                i += 1;
                continue;
            }
            let Ok(quantity) = word.parse::<f64>() else {
                return bad();
            };
            if !quantity.is_finite() {
                return bad();
            }
            let Some(unit) = words.get(i + 1) else {
                // A number alone counts seconds.
                micros += quantity * MICROS_PER_SECOND as f64;
                break;
            };
            let Some((m, d, us)) = unit_of(unit) else {
                return bad();
            };
            months += quantity * m;
            days += quantity * d;
            micros += quantity * us;
            i += 2;
        }
        let sign = if ago { -1.0 } else { 1.0 };
        Interval::from_parts(months * sign, days * sign, micros * sign).or_else(|_| bad())
    }
}

/// How many months, days and microseconds a unit of an interval is.
fn unit_of(unit: &str) -> Option<(f64, f64, f64)> {
    let micros = |n: i64| Some((0.0, 0.0, n as f64));
    match unit {
        "millennium" | "millennia" | "millenniums" | "mil" | "mils" => Some((12000.0, 0.0, 0.0)),
        "century" | "centuries" | "c" => Some((1200.0, 0.0, 0.0)),
        "decade" | "decades" | "dec" | "decs" => Some((120.0, 0.0, 0.0)),
        "year" | "years" | "y" | "yr" | "yrs" => Some((12.0, 0.0, 0.0)),
        "month" | "months" | "mon" | "mons" => Some((1.0, 0.0, 0.0)),
        "week" | "weeks" | "w" => Some((0.0, 7.0, 0.0)),
        "day" | "days" | "d" => Some((0.0, 1.0, 0.0)),
        "hour" | "hours" | "h" | "hr" | "hrs" => micros(3600 * MICROS_PER_SECOND),
        "minute" | "minutes" | "m" | "min" | "mins" => micros(60 * MICROS_PER_SECOND),
        "second" | "seconds" | "s" | "sec" | "secs" => micros(MICROS_PER_SECOND),
        "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" => micros(1000),
        "microsecond" | "microseconds" | "us" | "usec" | "usecs" => micros(1),
        _ => None,
    }
}

/// `[+-]hh:mm[:ss[.ffffff]]`, in microseconds; the hours may pass 24.
fn clock(word: &str) -> Option<f64> {
    let (sign, rest) = match word.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, word.strip_prefix('+').unwrap_or(word)),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (hours, minutes, seconds) = match parts.as_slice() {
        [h, m] => (*h, *m, "0"),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, "0"));
    if !digits(hours) || !digits(minutes) || !digits(whole) || !digits(fraction) {
        return None;
    }
    let (minutes, seconds): (f64, f64) = (minutes.parse().ok()?, seconds.parse().ok()?);
    if minutes >= 60.0 || seconds >= 60.0 {
        return None;
    }
    let hours: f64 = hours.parse().ok()?;
    Some(sign * ((hours * 60.0 + minutes) * 60.0 + seconds) * MICROS_PER_SECOND as f64)
}

fn too_long() -> crate::error::Error {
    crate::error::Error("INTERVAL out of range".into())
}

impl std::fmt::Display for Interval {
    /// As PostgreSQL writes it: `1 year 2 mons 3 days 04:05:06.5`, with
    /// `00:00:00` for nothing at all; which reads back as the same interval.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let mut parts = Vec::new();
        let count = |n: i64, one: &str, many: &str| match n.abs() {
            1 => format!("{n} {one}"),
            _ => format!("{n} {many}"),
        };
        let (years, months) = (self.months / 12, self.months % 12);
        if years != 0 {
            parts.push(count(years as i64, "year", "years"));
        }
        if months != 0 {
            parts.push(count(months as i64, "mon", "mons"));
        }
        if self.days != 0 {
            parts.push(count(self.days as i64, "day", "days"));
        }
        if self.micros != 0 || parts.is_empty() {
            let sign = if self.micros < 0 { "-" } else { "" };
            let micros = self.micros.unsigned_abs();
            let seconds = micros / MICROS_PER_SECOND as u64;
            let fraction = micros % MICROS_PER_SECOND as u64;
            let mut clock = format!(
                "{sign}{:02}:{:02}:{:02}",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            );
            if fraction > 0 {
                clock += &format!(".{}", format!("{fraction:06}").trim_end_matches('0'));
            }
            parts.push(clock);
        }
        f.write_str(&parts.join(" "))
    }
}

/// The day `months` months after (or before) this one, on the same day of
/// the month, or that month's last day if it has fewer.
fn add_months(days: i32, months: i32) -> Result<i32> {
    let (year, month, day) = civil_from_days(days);
    let total = year * 12 + month as i64 - 1 + months as i64;
    let (year, month) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    if !(1..=9999).contains(&year) {
        return err("TIMESTAMP out of range: years 1 to 9999");
    }
    let day = day.min(days_in_month(year, month));
    Ok(days_from_civil(year, month, day).expect("a day of a month in range"))
}

/// A timestamp moved by an interval: by its months on the calendar, then
/// by its days and microseconds.
pub fn add_interval(micros: i64, interval: Interval) -> Result<i64> {
    let (day, time) = split_timestamp(micros);
    let day = match interval.months {
        0 => day,
        months => add_months(day, months)?,
    };
    let moved = (day as i64 + interval.days as i64)
        .checked_mul(MICROS_PER_DAY)
        .and_then(|m| m.checked_add(time))
        .and_then(|m| m.checked_add(interval.micros));
    timestamp(moved.ok_or_else(|| crate::error::Error("TIMESTAMP out of range".into()))?)
}

/// The interval from timestamp `b` to timestamp `a`: whole days, then the
/// rest, as PostgreSQL gives it (`1 day 02:00:00` for 26 hours).
pub fn between(a: i64, b: i64) -> Interval {
    let micros = a - b; // both in range, so this cannot overflow
    Interval {
        months: 0,
        days: (micros / MICROS_PER_DAY) as i32,
        micros: micros % MICROS_PER_DAY,
    }
}

/// A time of day moved by an interval's microseconds, around midnight.
pub fn add_to_time(micros: i64, interval: Interval) -> i64 {
    (micros + interval.micros.rem_euclid(MICROS_PER_DAY)).rem_euclid(MICROS_PER_DAY)
}

// ---- time zones ----

/// The offset of a time zone east of UTC, in microseconds: `UTC`, `GMT`
/// and `Z` are none, and `+hh`, `+hh:mm` or `+hhmm` (or with `-`) up to 15
/// hours 59. There are no zones by name, as there is no time zone database.
pub fn parse_zone(text: &str) -> Result<i64> {
    let bad = || {
        err(format!(
            "unknown time zone: '{text}' (only UTC and offsets like +02:00 are known)"
        ))
    };
    let s = text.trim();
    if ["utc", "gmt", "z", "zulu"].contains(&s.to_lowercase().as_str()) {
        return Ok(0);
    }
    let (sign, rest) = match (s.strip_prefix('+'), s.strip_prefix('-')) {
        (Some(rest), _) => (1, rest),
        (_, Some(rest)) => (-1, rest),
        _ => return bad(),
    };
    let digits = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
    let (hours, minutes) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None if rest.len() == 4 => rest.split_at(2),
        None => (rest, "00"),
    };
    if !digits(hours) || !digits(minutes) {
        return bad();
    }
    let (hours, minutes): (i64, i64) = (hours.parse().unwrap(), minutes.parse().unwrap());
    if hours > 15 || minutes > 59 {
        return bad();
    }
    Ok(sign * (hours * 60 + minutes) * 60 * MICROS_PER_SECOND)
}

/// Reads a TIMESTAMP WITH TIME ZONE: a timestamp, then maybe its zone
/// (`Z`, `UTC`, `+02`, `-05:30`, after a space or not); without one, UTC.
/// Returns its instant, in microseconds of UTC.
pub fn parse_timestamptz(text: &str) -> Result<i64> {
    let bad = || crate::error::Error(format!("invalid TIMESTAMP WITH TIME ZONE: '{text}'"));
    let s = text.trim();
    // The zone, if any, is after the date, which has its own `-`s.
    let after_date = s.char_indices().nth(10).map_or(s.len(), |(i, _)| i);
    let (stamp, zone) = match s[after_date..].rfind(['+', '-']) {
        Some(at) => s.split_at(after_date + at),
        None => match s.rsplit_once(' ') {
            Some((stamp, zone)) if zone.chars().all(char::is_alphabetic) => (stamp, zone),
            _ if s.ends_with(['Z', 'z']) => (&s[..s.len() - 1], "Z"),
            _ => (s, "UTC"),
        },
    };
    let local = parse_timestamp(stamp).map_err(|_| bad())?;
    let offset = parse_zone(zone).map_err(|_| bad())?;
    timestamp(local - offset).map_err(|_| bad())
}

/// A TIMESTAMP WITH TIME ZONE as UTC shows it: `2024-02-29 13:45:00+00`.
pub fn format_timestamptz(micros: i64) -> String {
    format!("{}+00", format_timestamp(micros))
}

/// The time now, in microseconds of UTC.
pub fn now() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    since.as_micros() as i64
}

/// `DATE_TRUNC(unit, timestamp)`: the timestamp with the parts below
/// `unit` set to their least (`week` from Monday, `quarter` from its
/// first month).
pub fn truncate(micros: i64, unit: &str) -> Result<i64> {
    let (day, time) = split_timestamp(micros);
    let (year, month, _) = civil_from_days(day);
    let at = |year: i64, month: u32| days_from_civil(year, month, 1).expect("a month in range") as i64 * MICROS_PER_DAY;
    let of_time = |unit: i64| day as i64 * MICROS_PER_DAY + time / unit * unit;
    Ok(match unit.to_lowercase().as_str() {
        "microsecond" | "microseconds" => micros,
        "millisecond" | "milliseconds" => of_time(1000),
        "second" | "seconds" => of_time(MICROS_PER_SECOND),
        "minute" | "minutes" => of_time(60 * MICROS_PER_SECOND),
        "hour" | "hours" => of_time(3600 * MICROS_PER_SECOND),
        "day" | "days" => day as i64 * MICROS_PER_DAY,
        // 1970-01-01 was a Thursday, 3 days after a Monday.
        "week" | "weeks" => (day as i64 - (day as i64 + 3).rem_euclid(7)) * MICROS_PER_DAY,
        "month" | "months" => at(year, month),
        "quarter" | "quarters" => at(year, (month - 1) / 3 * 3 + 1),
        "year" | "years" => at(year, 1),
        "decade" | "decades" => at((year / 10 * 10).max(1), 1),
        "century" | "centuries" => at(((year - 1) / 100 * 100 + 1).max(1), 1),
        "millennium" | "millennia" => at(((year - 1) / 1000 * 1000 + 1).max(1), 1),
        other => return err(format!("unknown unit for DATE_TRUNC: '{other}'")),
    })
}

/// The day of the week (0 for Sunday) and of the year (from 1) of a date,
/// and its quarter.
pub fn calendar_field(days: i32, field: Field) -> i64 {
    let (year, month, _) = civil_from_days(days);
    match field {
        // 1970-01-01 was a Thursday.
        Field::Dow => (days as i64 + 4).rem_euclid(7),
        Field::Doy => {
            days as i64 - days_from_civil(year, 1, 1).expect("a year in range") as i64 + 1
        }
        Field::Quarter => (month as i64 - 1) / 3 + 1,
        _ => date_field(days, field),
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
