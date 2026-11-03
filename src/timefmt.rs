//! Calendar arithmetic and git-style date parsing/formatting (no libc).

use std::time::{SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: `(year, month, day)`.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Current Unix time in seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse a `+hhmm` / `-hhmm` offset into seconds east of UTC.
pub fn parse_tz(tz: &str) -> Option<i64> {
    let sign = match tz.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = &tz[1..];
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: i64 = digits[..2].parse().ok()?;
    let m: i64 = digits[2..].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

/// Format like `git log` does: `Tue Nov 3 09:07:08 2026 -0800`.
pub fn format_git_date(time: i64, tz: &str) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let local = time + parse_tz(tz).unwrap_or(0);
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let wd = (days + 4).rem_euclid(7) as usize;
    format!(
        "{} {} {} {:02}:{:02}:{:02} {} {}",
        DAYS[wd],
        MONTHS[(m - 1) as usize],
        d,
        secs / 3600,
        secs % 3600 / 60,
        secs % 60,
        y,
        tz
    )
}

/// Parse the date formats accepted in `GIT_AUTHOR_DATE` and friends:
/// `<unix> [+hhmm]`, `@<unix> [+hhmm]` and `YYYY-MM-DD[T ]HH:MM:SS [+hhmm|Z]`.
/// Returns the timestamp and the zone string.
pub fn parse_date(s: &str) -> Option<(i64, String)> {
    let s = s.trim();
    let s = s.strip_prefix('@').unwrap_or(s);
    let mut parts = s.split_whitespace();
    let first = parts.next()?;
    if let Ok(t) = first.parse::<i64>() {
        let tz = parts.next().unwrap_or("+0000");
        parse_tz(tz)?;
        return Some((t, tz.to_string()));
    }
    // ISO form; the time may be glued with 'T' or be the second token.
    let (date, mut time, mut tz) = match first.split_once('T') {
        Some((d, t)) => (d.to_string(), Some(t.to_string()), None),
        None => (first.to_string(), parts.next().map(str::to_string), None),
    };
    if let Some(t) = &time {
        if let Some(stripped) = t.strip_suffix('Z') {
            tz = Some("+0000".to_string());
            time = Some(stripped.to_string());
        }
    }
    if tz.is_none() {
        tz = parts.next().map(str::to_string);
    }
    let tz = tz.unwrap_or_else(|| "+0000".to_string());
    let offset = parse_tz(&tz)?;
    let mut d = date.split('-');
    let (y, mo, da): (i64, i64, i64) =
        (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || d.next().is_some() {
        return None;
    }
    let time = time.unwrap_or_else(|| "00:00:00".to_string());
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next().map_or(Some(0), |x| x.parse().ok())?;
    let se: i64 = t.next().map_or(Some(0), |x| x.parse().ok())?;
    if h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let unix = days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + se - offset;
    Some((unix, tz))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for days in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn formats_like_git() {
        assert_eq!(format_git_date(0, "+0000"), "Thu Jan 1 00:00:00 1970 +0000");
        assert_eq!(format_git_date(1_700_000_000, "+0000"), "Tue Nov 14 22:13:20 2023 +0000");
        assert_eq!(format_git_date(1_700_000_000, "-0800"), "Tue Nov 14 14:13:20 2023 -0800");
        assert_eq!(format_git_date(1_700_000_000, "+0530"), "Wed Nov 15 03:43:20 2023 +0530");
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_date("1700000000 +0100"), Some((1_700_000_000, "+0100".into())));
        assert_eq!(parse_date("@1700000000"), Some((1_700_000_000, "+0000".into())));
        assert_eq!(parse_date("2023-11-14T22:13:20Z"), Some((1_700_000_000, "+0000".into())));
        assert_eq!(parse_date("2023-11-14 14:13:20 -0800"), Some((1_700_000_000, "-0800".into())));
        assert_eq!(parse_date("2023-11-14T22:13:20 +0000"), Some((1_700_000_000, "+0000".into())));
        assert_eq!(parse_date("garbage"), None);
        assert_eq!(parse_date("2023-13-01 00:00:00"), None);
        assert_eq!(parse_tz("+0530"), Some(19_800));
        assert_eq!(parse_tz("0530"), None);
    }
}
