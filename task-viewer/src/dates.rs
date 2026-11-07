use chrono::{Local, NaiveDate};

pub(crate) fn today() -> NaiveDate {
    Local::now().date_naive()
}

pub(crate) fn parse_iso_date(raw: &str) -> Option<NaiveDate> {
    let trimmed = raw.trim().trim_matches(|ch| ch == '"' || ch == '\'');
    let date_part = trimmed
        .split_once(['T', ' '])
        .map(|(date, _)| date)
        .unwrap_or(trimmed);
    NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok()
}

pub(crate) fn format_date(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::parse_iso_date;
    use chrono::NaiveDate;

    #[test]
    fn dates_parse_frontmatter_and_commit_timestamps() {
        let created = parse_iso_date("2026-09-03T10:33:44").expect("created");
        assert_eq!(created, NaiveDate::from_ymd_opt(2026, 9, 3).expect("ymd"));
        let shipped = parse_iso_date("2026-07-17T12:00:00+00:00").expect("shipped");
        assert_eq!(shipped, NaiveDate::from_ymd_opt(2026, 7, 17).expect("ymd"));
    }

    #[test]
    fn dates_parse_a_space_separated_timestamp() {
        let day = NaiveDate::from_ymd_opt(2026, 5, 1).expect("ymd");
        assert_eq!(parse_iso_date("2026-05-01 10:00:00"), Some(day));
        assert_eq!(parse_iso_date("2026-05-01T10:00:00"), Some(day));
    }
}
