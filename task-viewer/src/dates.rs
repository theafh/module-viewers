use chrono::{
    DateTime, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone,
    Utc,
};

pub(crate) fn today() -> NaiveDate {
    Utc::now().date_naive()
}

/// Read a task timestamp as the UTC calendar day it names.
///
/// A bare `YYYY-MM-DD` value keeps the day it states. A value with a time
/// converts its instant to UTC: an explicit `Z` or `±HH:MM` / `±HHMM` offset
/// states its own instant, while a value with no offset is a `Europe/Berlin`
/// wall time, the zone the legacy task system wrote. Seconds and fractional
/// seconds are optional. A value whose date prefix parses but whose time or
/// offset does not keeps that date prefix, so an unreadable time changes no
/// harvest skip, warning, or fallback.
pub(crate) fn parse_iso_date(raw: &str) -> Option<NaiveDate> {
    let trimmed = strip_one_quote_pair(raw.trim());
    let Some((date_part, time_part)) = trimmed.split_once(['T', ' ']) else {
        return NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").ok();
    };
    let Ok(date) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") else {
        return None;
    };
    Some(timestamp_day(date, time_part).unwrap_or(date))
}

/// Read a `Europe/Berlin` wall time as a UTC instant. An ambiguous wall time
/// resolves to the earlier instant, and a spring-forward wall time that does
/// not exist resolves with the offset in force before the gap.
pub(crate) fn berlin_instant(wall: NaiveDateTime) -> Option<DateTime<Utc>> {
    let zone = chrono_tz::Europe::Berlin;
    match zone.from_local_datetime(&wall) {
        LocalResult::Single(instant) => Some(instant.with_timezone(&Utc)),
        LocalResult::Ambiguous(earlier, _later) => Some(earlier.with_timezone(&Utc)),
        LocalResult::None => {
            let before = zone
                .from_local_datetime(&(wall - Duration::hours(1)))
                .earliest()?
                .with_timezone(&Utc);
            Some(before + Duration::hours(1))
        }
    }
}

fn timestamp_day(date: NaiveDate, time_part: &str) -> Option<NaiveDate> {
    let (time_text, offset) = split_offset(time_part);
    let time = parse_wall_time(time_text)?;
    let wall = date.and_time(time);
    let instant = match offset {
        Some("Z" | "z") => wall.and_utc(),
        Some(offset) => {
            let zone = FixedOffset::east_opt(parse_offset_seconds(offset)?)?;
            zone.from_local_datetime(&wall)
                .single()?
                .with_timezone(&Utc)
        }
        None => berlin_instant(wall)?,
    };
    Some(instant.date_naive())
}

/// Split a time from its `Z` or numeric offset suffix.
fn split_offset(time_part: &str) -> (&str, Option<&str>) {
    if let Some(rest) = time_part.strip_suffix(['Z', 'z']) {
        return (rest, Some("Z"));
    }
    match time_part.rfind(['+', '-']) {
        Some(index) => (&time_part[..index], Some(&time_part[index..])),
        None => (time_part, None),
    }
}

fn parse_wall_time(time_text: &str) -> Option<NaiveTime> {
    for format in ["%H:%M:%S%.f", "%H:%M:%S", "%H:%M"] {
        if let Ok(time) = NaiveTime::parse_from_str(time_text, format) {
            return Some(time);
        }
    }
    None
}

/// The signed seconds east of UTC an offset such as `+01:00`, `-0500`, or
/// `+00:00` names.
fn parse_offset_seconds(offset: &str) -> Option<i32> {
    let (sign, rest) = match offset.as_bytes().first()? {
        b'+' => (1, &offset[1..]),
        b'-' => (-1, &offset[1..]),
        _ => return None,
    };
    let (hours, minutes) = match rest.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if rest.len() == 4 => rest.split_at(2),
        None => return None,
    };
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Strip one matching pair of single or double quotes around a value, so a
/// quoted frontmatter date reads like its bare spelling.
fn strip_one_quote_pair(value: &str) -> &str {
    if let Some(inner) = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return inner;
    }
    if let Some(inner) = value
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return inner;
    }
    value
}

/// The earliest day a chart series draws. A frontmatter date before this floor
/// folds into the first charted day, so one mistyped year cannot stretch a walk
/// to hundreds of thousands of points.
pub(crate) const SERIES_FLOOR: NaiveDate =
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 is a valid date");

pub(crate) fn format_date(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::parse_iso_date;
    use chrono::NaiveDate;

    fn day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("ymd")
    }

    #[test]
    fn dates_parse_frontmatter_and_commit_timestamps() {
        let created = parse_iso_date("2026-09-03T10:33:44").expect("created");
        assert_eq!(created, day(2026, 9, 3));
        let shipped = parse_iso_date("2026-07-17T12:00:00+00:00").expect("shipped");
        assert_eq!(shipped, day(2026, 7, 17));
    }

    #[test]
    fn dates_parse_a_space_separated_timestamp() {
        assert_eq!(parse_iso_date("2026-05-01 10:00:00"), Some(day(2026, 5, 1)));
        assert_eq!(parse_iso_date("2026-05-01T10:00:00"), Some(day(2026, 5, 1)));
    }

    #[test]
    fn dates_convert_offsets_and_zones_to_utc_days() {
        // A Berlin wall time just past midnight belongs to the previous UTC day.
        assert_eq!(
            parse_iso_date("2026-11-09T00:30:00+01:00"),
            Some(day(2026, 11, 8))
        );
        assert_eq!(
            parse_iso_date("2026-11-09T00:30:00+0100"),
            Some(day(2026, 11, 8))
        );
        assert_eq!(
            parse_iso_date("2026-11-09T00:30:00.500+01:00"),
            Some(day(2026, 11, 8))
        );
        assert_eq!(
            parse_iso_date("2026-11-09T00:30:00Z"),
            Some(day(2026, 11, 9))
        );
        assert_eq!(
            parse_iso_date("2026-11-09 00:30:00"),
            Some(day(2026, 11, 8))
        );
        assert_eq!(
            parse_iso_date("2026-07-01T00:30:00"),
            Some(day(2026, 6, 30))
        );
        assert_eq!(parse_iso_date("2026-11-08"), Some(day(2026, 11, 8)));
        // Days that stay on their own UTC day keep it.
        assert_eq!(parse_iso_date("2026-05-01 10:00:00"), Some(day(2026, 5, 1)));
        assert_eq!(
            parse_iso_date("2026-07-17T12:00:00+00:00"),
            Some(day(2026, 7, 17))
        );
    }

    #[test]
    fn dates_convert_minute_precision_and_keep_an_unreadable_time_prefix() {
        assert_eq!(
            parse_iso_date("2026-11-09T00:30"),
            Some(day(2026, 11, 8)),
            "a minute-precision Berlin wall time converts across midnight"
        );
        assert_eq!(
            parse_iso_date("2026-11-09T10:00"),
            Some(day(2026, 11, 9)),
            "a minute-precision wall time on its own UTC day keeps it"
        );
        assert_eq!(
            parse_iso_date("2026-11-09Tgarbage"),
            Some(day(2026, 11, 9)),
            "a time that does not parse keeps the date prefix"
        );
        assert_eq!(
            parse_iso_date("2026-11-09T"),
            Some(day(2026, 11, 9)),
            "an empty time keeps the date prefix"
        );
    }

    #[test]
    fn dates_resolve_both_berlin_dst_boundaries_to_a_day() {
        // Spring forward skips 02:00-03:00; fall back repeats 02:00-03:00.
        assert_eq!(
            parse_iso_date("2026-03-29T02:30:00"),
            Some(day(2026, 3, 29))
        );
        assert_eq!(
            parse_iso_date("2026-10-25T02:30:00"),
            Some(day(2026, 10, 25))
        );
    }

    #[test]
    fn dates_clock_and_stamp_run_on_utc() {
        let dates_source = include_str!("dates.rs");
        let today_body = function_body(dates_source, "fn today()");
        assert!(
            today_body.contains("Utc::now().date_naive()"),
            "the chart clock reads the UTC day: {today_body}"
        );
        assert!(
            !today_body.contains("Local::now()"),
            "the chart clock must not read local time: {today_body}"
        );

        let db_source = include_str!("db.rs");
        let stamp_body = function_body(db_source, "fn now_stamp()");
        assert!(
            stamp_body.contains("Utc::now()"),
            "the repository stamp reads the UTC clock: {stamp_body}"
        );
        assert!(
            stamp_body.contains("%Y-%m-%dT%H:%M:%SZ"),
            "the repository stamp carries its zone designator: {stamp_body}"
        );
        assert!(
            !stamp_body.contains("Local::now()"),
            "the repository stamp must not read local time: {stamp_body}"
        );
    }

    fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} must exist in the source"));
        let rest = &source[start..];
        let open = rest
            .find('{')
            .unwrap_or_else(|| panic!("{signature} must open a body"));
        let close = rest
            .find("\n}")
            .unwrap_or_else(|| panic!("{signature} must close its body"));
        &rest[open..close]
    }
}
