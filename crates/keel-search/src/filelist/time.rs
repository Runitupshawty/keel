//! File list dates: Windows FILETIME (100 ns ticks since 1601-01-01 UTC), the form EFU
//! files store, and ISO 8601 text.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds from 1601-01-01 to 1970-01-01.
const EPOCH_DIFF: i64 = 11_644_473_600;
const TICKS: i64 = 10_000_000;
/// FILETIME's last representable year (Windows caps SYSTEMTIME at 30827).
const MAX_YEAR: i64 = 30_827;

/// A date column's value: a FILETIME number, or an ISO 8601 date (`2024-03-05`,
/// `2024-03-05T14:07:09`, with `.fraction`, `Z` or `±hh:mm`; no zone means UTC). Empty,
/// zero, before 1601 or unreadable: None.
pub fn parse_date(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        return text.parse::<u64>().ok().filter(|&t| t != 0);
    }
    parse_iso(text)
}

fn digits(s: &[u8], n: usize) -> Option<(i64, &[u8])> {
    if s.len() < n || !s[..n].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let v = s[..n]
        .iter()
        .fold(0i64, |v, &b| v * 10 + i64::from(b - b'0'));
    Some((v, &s[n..]))
}

fn parse_iso(text: &str) -> Option<u64> {
    let s = text.as_bytes();
    let (year, s) = digits(s, 4)?;
    let s = s.strip_prefix(b"-")?;
    let (month, s) = digits(s, 2)?;
    let s = s.strip_prefix(b"-")?;
    let (day, mut s) = digits(s, 2)?;
    let (mut hour, mut minute, mut second, mut frac) = (0, 0, 0, 0);
    if let Some(rest) = s.strip_prefix(b"T").or_else(|| s.strip_prefix(b" ")) {
        let (h, rest) = digits(rest, 2)?;
        let rest = rest.strip_prefix(b":")?;
        let (m, mut rest) = digits(rest, 2)?;
        (hour, minute) = (h, m);
        if let Some(r) = rest.strip_prefix(b":") {
            let (sec, r) = digits(r, 2)?;
            second = sec;
            rest = r;
            if let Some(r) = rest.strip_prefix(b".").or_else(|| rest.strip_prefix(b",")) {
                let n = r.iter().take_while(|b| b.is_ascii_digit()).count();
                if n == 0 {
                    return None;
                }
                // 100 ns ticks: the first 7 digits count, the rest are dropped.
                let mut ticks = 0;
                for i in 0..7 {
                    ticks = ticks * 10
                        + r.get(i)
                            .filter(|_| i < n)
                            .map_or(0, |b| i64::from(b - b'0'));
                }
                frac = ticks;
                rest = &r[n..];
            }
        }
        s = rest;
    }
    let offset = match s {
        [] | b"Z" | b"z" => 0,
        [sign @ (b'+' | b'-'), rest @ ..] => {
            let (h, rest) = digits(rest, 2)?;
            let rest = rest.strip_prefix(b":").unwrap_or(rest);
            let m = match rest {
                [] => 0,
                _ => match digits(rest, 2)? {
                    (m, []) => m,
                    _ => return None,
                },
            };
            if h > 23 || m > 59 {
                return None;
            }
            let secs = h * 3600 + m * 60;
            if *sign == b'+' {
                secs
            } else {
                -secs
            }
        }
        _ => return None,
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&month)
        || day < 1
        || day > month_days[month as usize - 1]
        || hour > 23
        || minute > 59
        || second > 60
        || !(1601..=MAX_YEAR).contains(&year)
    {
        return None;
    }
    let unix =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    let ticks = (unix + EPOCH_DIFF).checked_mul(TICKS)?.checked_add(frac)?;
    u64::try_from(ticks).ok().filter(|&t| t != 0)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of day `z` since 1970-01-01: (year, month, day).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A FILETIME as ISO 8601 UTC: `2024-03-05T14:07:09Z`, with the 100 ns fraction when it is
/// not zero (`…:09.25Z`), so [`parse_date`] reads back the same value.
pub fn format_iso(filetime: u64) -> String {
    let ticks = filetime as i128;
    let secs = (ticks / TICKS as i128) as i64 - EPOCH_DIFF;
    let frac = (ticks % TICKS as i128) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let mut out = format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    );
    if frac != 0 {
        let digits = format!("{frac:07}");
        out.push('.');
        out.push_str(digits.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

/// A FILETIME as a `SystemTime`.
pub fn system_from_filetime(filetime: u64) -> Option<SystemTime> {
    let unix_ticks = filetime as i128 - EPOCH_DIFF as i128 * TICKS as i128;
    let nanos = unix_ticks.unsigned_abs() * 100;
    let d = Duration::new(
        u64::try_from(nanos / 1_000_000_000).ok()?,
        (nanos % 1_000_000_000) as u32,
    );
    if unix_ticks >= 0 {
        UNIX_EPOCH.checked_add(d)
    } else {
        UNIX_EPOCH.checked_sub(d)
    }
}

/// A `SystemTime` as a FILETIME; None before 1601.
pub fn filetime_from_system(time: SystemTime) -> Option<u64> {
    let unix_ticks: i128 = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_nanos() / 100) as i128,
        Err(e) => -((e.duration().as_nanos() / 100) as i128),
    };
    u64::try_from(unix_ticks + EPOCH_DIFF as i128 * TICKS as i128)
        .ok()
        .filter(|&t| t != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-03-05T14:07:09Z as a FILETIME.
    const T: u64 = 133_541_212_290_000_000;

    #[test]
    fn filetime_and_iso_agree() {
        assert_eq!(parse_date("133541212290000000"), Some(T));
        assert_eq!(parse_date("2024-03-05T14:07:09Z"), Some(T));
        assert_eq!(parse_date("2024-03-05T14:07:09"), Some(T), "no zone: UTC");
        assert_eq!(parse_date("2024-03-05 14:07:09"), Some(T));
        assert_eq!(parse_date("2024-03-05T15:07:09+01:00"), Some(T));
        assert_eq!(parse_date("2024-03-05T13:07:09-0100"), Some(T));
        assert_eq!(parse_date("2024-03-05T14:07"), Some(T - 9 * 10_000_000));
        assert_eq!(
            parse_date("2024-03-05"),
            Some(T - (14 * 3600 + 7 * 60 + 9) * 10_000_000)
        );
        assert_eq!(parse_date("2024-03-05T14:07:09.25Z"), Some(T + 2_500_000));
        assert_eq!(
            parse_date("2024-03-05T14:07:09.123456789Z"),
            Some(T + 1_234_567)
        );
        assert_eq!(format_iso(T), "2024-03-05T14:07:09Z");
        assert_eq!(format_iso(T + 2_500_000), "2024-03-05T14:07:09.25Z");
        assert_eq!(format_iso(T + 1), "2024-03-05T14:07:09.0000001Z");
        for t in [
            T,
            T + 1,
            T + 1_234_567,
            1,
            116_444_736_000_000_000,
            2_650_467_743_999_999_999,
        ] {
            assert_eq!(parse_date(&format_iso(t)), Some(t), "{t}");
        }
        assert_eq!(format_iso(116_444_736_000_000_000), "1970-01-01T00:00:00Z");
        assert_eq!(format_iso(1), "1601-01-01T00:00:00.0000001Z");
    }

    #[test]
    fn bad_dates_are_none() {
        for bad in [
            "",
            "0",
            "  ",
            "2024-13-01",
            "2024-02-30",
            "2023-02-29",
            "1600-12-31",
            "2024-03-05T24:00",
            "2024-03-05T14:07:09+25:00",
            "2024-03-05T14:07:09Q",
            "2024-03-05T14:07:09.",
            "2024-3-5",
            "yesterday",
            "99999999999999999999999",
            "-5",
        ] {
            assert_eq!(parse_date(bad), None, "{bad:?}");
        }
        assert_eq!(parse_date("2024-02-29"), parse_date("2024-02-29T00:00:00Z"));
        assert!(parse_date("2024-02-29").is_some(), "leap day");
    }

    #[test]
    fn system_time_round_trip() {
        let t = system_from_filetime(T).unwrap();
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_709_647_629
        );
        assert_eq!(filetime_from_system(t), Some(T));
        let early = system_from_filetime(1_000).unwrap();
        assert!(early < UNIX_EPOCH);
        assert_eq!(filetime_from_system(early), Some(1_000));
    }
}
