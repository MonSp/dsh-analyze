//! UTC calendar parsing / bucketing for time-period groupBy.

/// Parsed civil date+time in UTC (offset already applied).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcParts {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Period {
    Day,
    Week,
    Month,
}

impl Period {
    pub fn parse(s: &str) -> Option<Period> {
        match s {
            "day" => Some(Period::Day),
            "week" => Some(Period::Week),
            "month" => Some(Period::Month),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Period::Day => "day",
            Period::Week => "week",
            Period::Month => "month",
        }
    }
}

/// Parse `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`.
/// Returns UTC calendar date after applying offset when present.
pub fn parse_timestamp_utc_date(s: &str) -> Option<UtcParts> {
    let t = s.trim();
    if t.len() < 10 {
        return None;
    }
    let bytes = t.as_bytes();
    // date part
    let year: i32 = t.get(0..4)?.parse().ok()?;
    if bytes[4] != b'-' {
        return None;
    }
    let month: u32 = t.get(5..7)?.parse().ok()?;
    if bytes[7] != b'-' {
        return None;
    }
    let day: u32 = t.get(8..10)?.parse().ok()?;
    if month == 0 || month > 12 || day == 0 || day > 31 {
        return None;
    }
    // optional time
    let rest = t.get(10..)?;
    if rest.is_empty() {
        return Some(UtcParts { year, month, day });
    }
    if !rest.starts_with('T') && !rest.starts_with('t') && !rest.starts_with(' ') {
        return None;
    }
    let rest = &rest[1..];
    let (hms, offset) = split_offset(rest);
    let hhmmss: Vec<&str> = hms.split(':').collect();
    if hhmmss.len() < 2 {
        return None;
    }
    let hour: i64 = hhmmss[0].parse().ok()?;
    let minute: i64 = hhmmss[1].parse().ok()?;
    if hour < 0 || hour > 23 || minute < 0 || minute > 59 {
        return None;
    }
    // seconds optional; ignore fractional part
    if hhmmss.len() >= 3 {
        let sec_part = hhmmss[2];
        let sec_str = sec_part.split('.').next().unwrap_or("0");
        let _sec: i64 = sec_str.parse().ok()?;
    }
    // local = UTC + offset  ⇒  UTC minutes from midnight = local - offset
    let total_min = hour * 60 + minute - offset;
    // shift date by offset-adjusted day delta (already relative to UTC midnight)
    let mut y = year;
    let mut m = month as i32;
    let mut d = day as i32;
    let mut day_delta = total_min.div_euclid(24 * 60);
    let _ = hour;
    let _ = minute;
    while day_delta != 0 {
        if day_delta > 0 {
            d += 1;
            day_delta -= 1;
            if d > days_in_month(y, m) {
                d = 1;
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            }
        } else {
            d -= 1;
            day_delta += 1;
            if d < 1 {
                m -= 1;
                if m < 1 {
                    m = 12;
                    y -= 1;
                }
                d = days_in_month(y, m);
            }
        }
    }
    Some(UtcParts {
        year: y,
        month: m as u32,
        day: d as u32,
    })
}

/// Split trailing `Z` / `±HH:MM` / `±HHMM` off the time string.
/// Returns (time_without_offset, offset_minutes).
fn split_offset(rest: &str) -> (&str, i64) {
    if let Some(stripped) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return (stripped, 0);
    }
    // find last + or - after position 0 (not year sign)
    if let Some(pos) = rest.rfind(['+', '-']) {
        if pos > 0 {
            let (time_part, off) = rest.split_at(pos);
            let sign = if off.starts_with('-') { -1 } else { 1 };
            let off_body = &off[1..];
            // HH:MM or HHMM
            let (h_str, m_str) = match off_body.split_once(':') {
                Some((h, m)) => (h.to_string(), m.chars().take(2).collect::<String>()),
                None => {
                    let hb: String = off_body.chars().take(2).collect();
                    let mb: String = off_body.chars().skip(2).take(2).collect();
                    (hb, mb)
                }
            };
            let h: i64 = h_str.parse().unwrap_or(0);
            let m: i64 = m_str.parse().unwrap_or(0);
            return (time_part, sign * (h * 60 + m));
        }
    }
    (rest, 0)
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: i32) -> i32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 30,
    }
}

/// Days from civil epoch (1970-01-03 = 0-aligned Howard Hinnant algorithm).
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let y = i32::try_from(y).unwrap_or(0);
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// ISO week: Monday of the week containing this date.
fn monday_of_week(p: UtcParts) -> UtcParts {
    let days = days_from_civil(p.year, p.month, p.day);
    // 1970-01-01 was Thursday; weekday 0=Mon..6=Sun for days_from_civil epoch 1970-03-01 style.
    // Hinnant: day 0 = 1970-01-00 = 1969-12-31 which was Thursday.
    // days_from_civil(1970,1,1) = 0 → Thursday. We want Monday-based:
    // (weekday_from_epoch + 3) % 7 gives Mon=0 for Thursday=3.
    let wd = ((days % 7) + 7 + 3) % 7; // Mon=0
    let back = wd; // days back to Monday
    let (y, m, d) = civil_from_days(days - back);
    UtcParts {
        year: y,
        month: m,
        day: d,
    }
}

pub fn bucket_key(p: UtcParts, period: &Period) -> String {
    match period {
        Period::Day => format!("{:04}-{:02}-{:02}", p.year, p.month, p.day),
        Period::Month => format!("{:04}-{:02}", p.year, p.month),
        Period::Week => {
            let mon = monday_of_week(p);
            format!("{:04}-{:02}-{:02}", mon.year, mon.month, mon.day)
        }
    }
}

/// Classify a cell string into a bucket (null / key / parse-fail).
#[allow(dead_code)]
pub fn classify_str(s: Option<&str>, period: &Period) -> Classify {
    match s.map(str::trim) {
        None | Some("") => Classify::Null,
        Some(t) => match parse_timestamp_utc_date(t) {
            Some(p) => Classify::Key(bucket_key(p, period)),
            None => Classify::ParseFail,
        },
    }
}

#[allow(dead_code)]
pub enum Classify {
    Key(String),
    Null,
    ParseFail,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_date_only() {
        let p = parse_timestamp_utc_date("2024-01-31").unwrap();
        assert_eq!(p, UtcParts { year: 2024, month: 1, day: 31 });
    }

    #[test]
    fn parses_datetime_z() {
        let p = parse_timestamp_utc_date("2024-01-31T15:04:05Z").unwrap();
        assert_eq!(p.day, 31);
    }

    #[test]
    fn positive_offset_crosses_day() {
        // 2024-01-31 23:04 +08:00 → UTC 15:04 same day
        let p = parse_timestamp_utc_date("2024-01-31T23:04:05+08:00").unwrap();
        assert_eq!(p.day, 31);
        // 2024-02-01 01:04 +08:00 → UTC 2024-01-31 17:04
        let p2 = parse_timestamp_utc_date("2024-02-01T01:04:05+08:00").unwrap();
        assert_eq!(p2, UtcParts { year: 2024, month: 1, day: 31 });
    }

    #[test]
    fn week_bucket_monday() {
        // 2024-01-31 is Wednesday; Monday is 2024-01-29
        let p = parse_timestamp_utc_date("2024-01-31").unwrap();
        assert_eq!(bucket_key(p, &Period::Week), "2024-01-29");
    }

    #[test]
    fn month_bucket() {
        let p = parse_timestamp_utc_date("2024-01-31").unwrap();
        assert_eq!(bucket_key(p, &Period::Month), "2024-01");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_timestamp_utc_date("not-a-date").is_none());
        assert!(parse_timestamp_utc_date("2024-13-01").is_none());
    }
}
