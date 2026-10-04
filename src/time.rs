//! The times these logs write, read strictly: a date that doesn't exist is
//! no date.

use common::time::{civil_from_days, days_from_civil, Precision, Ts, TICKS_PER_SECOND};

use crate::{Context, Entry};

const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_DAY: i64 = 86_400;
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `YYYY-MM-DD`: days since 1970-01-01.
pub(crate) fn date(text: &str) -> Option<i64> {
    let mut parts = text.split('-');
    let year = number(parts.next()?, 4)?;
    let month = u32::try_from(number(parts.next()?, 2)?).ok()?;
    let day = u32::try_from(number(parts.next()?, 2)?).ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    (civil_from_days(days) == (year, month, day)).then_some(days)
}

/// `HH:MM:SS`: seconds since midnight.
pub(crate) fn clock(text: &str) -> Option<i64> {
    let mut parts = text.split(':');
    let hour = number(parts.next()?, 2)?;
    let minute = number(parts.next()?, 2)?;
    let second = number(parts.next()?, 2)?;
    let valid = parts.next().is_none() && hour < 24 && minute < 60 && second <= 60;
    valid.then_some(hour * SECONDS_PER_HOUR + minute * SECONDS_PER_MINUTE + second)
}

/// `digits` ASCII digits.
fn number(text: &str, digits: usize) -> Option<i64> {
    let valid = text.len() == digits && text.bytes().all(|b| b.is_ascii_digit());
    valid.then(|| text.parse().ok())?
}

/// A wall-clock time in the host's unknown zone.
pub(crate) fn local(days: i64, seconds: i64) -> Ts {
    Ts::from_local_ticks(
        (days * SECONDS_PER_DAY + seconds) * TICKS_PER_SECOND,
        Precision::Second,
    )
}

/// `YYYY-MM-DD HH:MM:SS`, with any run of spaces between (apt writes two):
/// a wall-clock time.
pub(crate) fn local_date_time(text: &str) -> Option<Ts> {
    let (day, time) = text.trim().split_once(' ')?;
    Some(local(date(day)?, clock(time.trim_start())?))
}

/// `YYYY-MM-DDTHH:MM:SS` then `Z`, `+HHMM` or `+HH:MM` (dnf): UTC.
pub(crate) fn offset_date_time(text: &str) -> Option<Ts> {
    let (day, rest) = text.split_once('T')?;
    let days = date(day)?;
    let seconds = clock(rest.get(..8)?)?;
    let offset = utc_offset(rest.get(8..)?)?;
    let utc = days * SECONDS_PER_DAY + seconds - offset;
    Some(Ts::from_ticks(utc * TICKS_PER_SECOND, Precision::Second))
}

/// `Z`, `+HHMM` or `+HH:MM`: seconds east of UTC.
fn utc_offset(text: &str) -> Option<i64> {
    if text == "Z" {
        return Some(0);
    }
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = text[1..].replacen(':', "", 1);
    let hours = number(digits.get(..2)?, 2)?;
    let minutes = number(digits.get(2..)?, 2)?;
    let valid = hours < 24 && minutes < 60;
    valid.then_some(sign * (hours * SECONDS_PER_HOUR + minutes * SECONDS_PER_MINUTE))
}

/// A yum time before its year is known: `Oct 04 14:39:17`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Undated {
    month: u32,
    day: u32,
    seconds: i64,
}

impl Undated {
    /// `Mmm dd HH:MM:SS` (the day zero- or space-padded), and the rest of
    /// the line.
    pub(crate) fn read(line: &str) -> Option<(Self, &str, &str)> {
        let month = MONTHS.iter().position(|m| line.starts_with(m))? as u32 + 1;
        let rest = line.get(3..)?.strip_prefix(' ')?.trim_start_matches(' ');
        let (day, rest) = rest.split_once(' ')?;
        let day: u32 = day.parse().ok().filter(|d| (1..=31).contains(d))?;
        let seconds = clock(rest.get(..8)?)?;
        let after = &rest[8..];
        let time_text = &line[..line.len() - after.len()];
        let undated = Self {
            month,
            day,
            seconds,
        };
        Some((undated, time_text, after))
    }
}

/// Give yum entries their year: lines are in order, so it goes up when the
/// month goes back, and the last one is in the year the file was last
/// modified (or the year before, if its month is later than that).
/// Without a modification time, times stay unset.
pub(crate) fn infer_years(
    entries: &mut [Entry],
    undated: &[Undated],
    context: Context,
    problems: &mut Vec<String>,
) {
    let Some((reference_year, reference_month)) = context.modified.and_then(year_and_month) else {
        return;
    };
    let relative = relative_years(undated);
    let (Some(&last_relative), Some(last)) = (relative.last(), undated.last()) else {
        return;
    };
    let last_year = if last.month <= reference_month {
        reference_year
    } else {
        reference_year - 1
    };
    for ((entry, clock), relative) in entries.iter_mut().zip(undated).zip(relative) {
        let year = last_year - (last_relative - relative);
        let days = days_from_civil(year, clock.month, clock.day);
        if civil_from_days(days) == (year, clock.month, clock.day) {
            entry.time = Some(local(days, clock.seconds));
        } else {
            problems.push(format!(
                "line {}: {} isn't a date in {year} (the inferred year)",
                entry.line, entry.time_text
            ));
        }
    }
}

/// Years counted from the first line's: one more each time the month goes
/// back.
fn relative_years(undated: &[Undated]) -> Vec<i64> {
    let mut year = 0;
    let mut previous = None;
    undated
        .iter()
        .map(|clock| {
            if previous.is_some_and(|p| clock.month < p) {
                year += 1;
            }
            previous = Some(clock.month);
            year
        })
        .collect()
}

fn year_and_month(time: Ts) -> Option<(i64, u32)> {
    let days = time.ticks()?.div_euclid(SECONDS_PER_DAY * TICKS_PER_SECOND);
    let (year, month, _) = civil_from_days(days);
    Some((year, month))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso(time: Option<Ts>) -> String {
        time.and_then(|t| t.to_iso8601()).unwrap_or_default()
    }

    #[test]
    fn dates_that_dont_exist_are_rejected() {
        assert!(date("2024-02-29").is_some());
        assert!(date("2026-02-29").is_none());
        assert!(date("2026-13-01").is_none());
        assert!(date("26-01-01").is_none());
        assert!(clock("24:00:00").is_none());
    }

    #[test]
    fn offsets_are_taken_off() {
        assert_eq!(
            iso(offset_date_time("2026-10-04T20:09:06+0530")),
            "2026-10-04T14:39:06.0000000Z"
        );
        assert_eq!(
            iso(offset_date_time("2026-10-04T01:00:00-05:00")),
            "2026-10-04T06:00:00.0000000Z"
        );
        assert_eq!(
            iso(offset_date_time("2019-11-28T09:52:16Z")),
            "2019-11-28T09:52:16.0000000Z"
        );
        assert!(offset_date_time("2026-10-04T20:09:06+05").is_none());
    }

    #[test]
    fn wall_clock_times_have_no_zone() {
        assert_eq!(
            iso(local_date_time("2026-10-04  16:38:41")),
            "2026-10-04T16:38:41.0000000"
        );
    }
}
