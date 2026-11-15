//! Small, dependency-free handling of FHIR `date`, `dateTime` and `instant`
//! strings: parse to seconds since the Unix epoch, add days, and write the same
//! instant with another UTC offset. Only the proleptic Gregorian calendar.

/// A parsed FHIR date or date-time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FhirTime {
    /// Seconds since 1970-01-01T00:00:00Z. Date-only values count from
    /// midnight UTC.
    pub epoch: i64,
    /// Fractional seconds as written, without the dot (may be empty).
    pub frac: String,
    /// True when a time part was present.
    pub has_time: bool,
    /// Offset in minutes east of UTC, when a time part was present.
    pub offset_minutes: i32,
    /// Precision of a date-only value: 1 year, 2 year-month, 3 full date.
    pub date_parts: u8,
}

fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Days from 1970-01-01 to the given civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for a day count since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse `YYYY`, `YYYY-MM`, `YYYY-MM-DD` or `YYYY-MM-DDThh:mm:ss[.f](Z|+hh:mm|-hh:mm)`.
pub fn parse(text: &str) -> Option<FhirTime> {
    let (date, time) = match text.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (text, None),
    };
    let parts: Vec<&str> = date.split('-').collect();
    if parts.is_empty() || parts.len() > 3 || parts[0].len() != 4 {
        return None;
    }
    let year = digits(parts[0])?;
    let month = match parts.get(1) {
        Some(m) if m.len() == 2 => digits(m)?,
        Some(_) => return None,
        None => 1,
    };
    let day = match parts.get(2) {
        Some(d) if d.len() == 2 => digits(d)?,
        Some(_) => return None,
        None => 1,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let Some(time) = time else {
        return Some(FhirTime {
            epoch: days * 86_400,
            frac: String::new(),
            has_time: false,
            offset_minutes: 0,
            date_parts: parts.len() as u8,
        });
    };
    if parts.len() != 3 {
        return None;
    }
    let (clock, offset_minutes) = if let Some(c) = time.strip_suffix('Z') {
        (c, 0)
    } else {
        let idx = time.rfind(['+', '-'])?;
        let (c, off) = time.split_at(idx);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off[1..].split_once(':')?;
        if oh.len() != 2 || om.len() != 2 {
            return None;
        }
        (c, sign * (digits(oh)? as i32 * 60 + digits(om)? as i32))
    };
    let (hms, frac) = match clock.split_once('.') {
        Some((h, f)) => (h, f.to_string()),
        None => (clock, String::new()),
    };
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hms: Vec<&str> = hms.split(':').collect();
    if hms.len() != 3 || hms.iter().any(|p| p.len() != 2) {
        return None;
    }
    let (h, m, s) = (digits(hms[0])?, digits(hms[1])?, digits(hms[2])?);
    if h > 23 || m > 59 || s > 60 {
        return None;
    }
    let local = days * 86_400 + h * 3_600 + m * 60 + s;
    Some(FhirTime {
        epoch: local - i64::from(offset_minutes) * 60,
        frac,
        has_time: true,
        offset_minutes,
        date_parts: 3,
    })
}

/// Write an instant as `YYYY-MM-DDThh:mm:ss[.f]` plus the given offset.
pub fn format_with_offset(epoch: i64, frac: &str, offset_minutes: i32) -> String {
    let local = epoch + i64::from(offset_minutes) * 60;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3_600, (secs % 3_600) / 60, secs % 60);
    let frac = if frac.is_empty() {
        String::new()
    } else {
        format!(".{frac}")
    };
    let offset = if offset_minutes == 0 {
        String::from("Z")
    } else {
        let sign = if offset_minutes < 0 { '-' } else { '+' };
        let abs = offset_minutes.abs();
        format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
    };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{frac}{offset}")
}

/// Write a date as `YYYY-MM-DD`.
pub fn format_date(epoch: i64) -> String {
    let (y, m, d) = civil_from_days(epoch.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Add whole days to a FHIR date or date-time, keeping its form and offset.
pub fn add_days(text: &str, days: i64) -> Option<String> {
    let t = parse(text)?;
    let epoch = t.epoch + days * 86_400;
    if t.has_time {
        Some(format_with_offset(epoch, &t.frac, t.offset_minutes))
    } else if t.date_parts == 3 {
        Some(format_date(epoch))
    } else {
        None
    }
}

/// Add seconds to a FHIR date-time with a time part, keeping its offset.
pub fn add_seconds(text: &str, secs: i64) -> Option<String> {
    let t = parse(text)?;
    if !t.has_time {
        return None;
    }
    Some(format_with_offset(
        t.epoch + secs,
        &t.frac,
        t.offset_minutes,
    ))
}

/// Parse an offset such as `+05:00`, `-03:30` or `Z` into minutes.
pub fn parse_offset(text: &str) -> Option<i32> {
    if text == "Z" {
        return Some(0);
    }
    let sign = match *text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (h, m) = text[1..].split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let minutes = digits(h)? as i32 * 60 + digits(m)? as i32;
    if minutes > 14 * 60 {
        return None;
    }
    Some(sign * minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2026, 1, 4)), (2026, 1, 4));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn parses_and_shifts_offsets() {
        let t = parse("2026-01-04T08:30:00Z").unwrap();
        assert_eq!(
            format_with_offset(t.epoch, &t.frac, 300),
            "2026-01-04T13:30:00+05:00"
        );
        let u = parse("2026-01-04T13:30:00+05:00").unwrap();
        assert_eq!(t.epoch, u.epoch);
        let v = parse("2026-01-04T03:30:00.250-05:00").unwrap();
        assert_eq!(v.epoch, t.epoch);
        assert_eq!(v.frac, "250");
    }

    #[test]
    fn adds_days_across_months() {
        assert_eq!(
            add_days("2026-01-30T09:12:00Z", 3).unwrap(),
            "2026-02-02T09:12:00Z"
        );
        assert_eq!(add_days("2026-01-04", -5).unwrap(), "2025-12-30");
    }

    #[test]
    fn rejects_bad_text() {
        assert!(parse("2026-13-01").is_none());
        assert!(parse("2026-01-04T08:30").is_none());
        assert!(parse("tomorrow").is_none());
        assert_eq!(parse_offset("+05:00"), Some(300));
        assert_eq!(parse_offset("-03:30"), Some(-210));
    }
}
