//! Date values: constants (`today`, `lastweek`, `past3days`, `mtd`, month and weekday
//! names), ISO 8601 (extended, basic, with `*` wildcards), local `/` and `.` formats,
//! `YYMMDD`, Excel serial days, FILETIME, and times of day. A date is a span of local time
//! turned into UTC milliseconds; a wildcard date is a pattern over the local fields.

use chrono::{
    DateTime, Datelike, Days, Months, NaiveDate, NaiveDateTime, TimeDelta, Timelike, Weekday,
};

use crate::values::NumVal;

/// How `a/b/c` dates are read when the year is not first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DateOrder {
    /// `MM/DD/YYYY` (United States).
    #[default]
    Mdy,
    /// `DD/MM/YYYY`.
    Dmy,
    /// `YY/MM/DD`.
    Ymd,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DateCtx {
    /// Now, UTC milliseconds.
    pub now: i64,
    /// Local time minus UTC, milliseconds.
    pub offset_ms: i64,
    pub week_start: Weekday,
    pub order: DateOrder,
}

/// A parsed date value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DateVal {
    /// UTC milliseconds.
    Span(NumVal),
    /// Local fields (year, month, day, hour, minute, second, millisecond); `None` past
    /// the precision or for `*`.
    Pattern([Option<i64>; 7], usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Year,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
}

fn unit_of(s: &str) -> Option<Unit> {
    Some(match s {
        "year" | "years" | "yr" | "yrs" => Unit::Year,
        "month" | "months" => Unit::Month,
        "week" | "weeks" | "wk" | "wks" => Unit::Week,
        "day" | "days" => Unit::Day,
        "hour" | "hours" | "hr" | "hrs" => Unit::Hour,
        "minute" | "minutes" | "min" | "mins" => Unit::Minute,
        "second" | "seconds" | "sec" | "secs" => Unit::Second,
        _ => return None,
    })
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const WEEKDAYS: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

/// `jan`/`january`/`sept` → 1..12.
pub(crate) fn month_name(s: &str) -> Option<u32> {
    if s == "sept" {
        return Some(9);
    }
    MONTHS
        .iter()
        .position(|m| s.len() >= 3 && m.starts_with(s))
        .map(|i| i as u32 + 1)
}

/// `sun`/`sunday`/`tues`/`thurs` → 0 (Sunday)..6.
pub(crate) fn weekday_name(s: &str) -> Option<u32> {
    WEEKDAYS
        .iter()
        .position(|d| s.len() >= 3 && d.starts_with(s))
        .map(|i| i as u32)
}

fn add(t: NaiveDateTime, unit: Unit, n: i64) -> Option<NaiveDateTime> {
    let months = |m: i64| -> Option<NaiveDateTime> {
        let k = Months::new(u32::try_from(m.unsigned_abs()).ok()?);
        if m >= 0 {
            t.checked_add_months(k)
        } else {
            t.checked_sub_months(k)
        }
    };
    match unit {
        Unit::Year => months(n.checked_mul(12)?),
        Unit::Month => months(n),
        Unit::Week => t.checked_add_signed(TimeDelta::try_weeks(n)?),
        Unit::Day => t.checked_add_signed(TimeDelta::try_days(n)?),
        Unit::Hour => t.checked_add_signed(TimeDelta::try_hours(n)?),
        Unit::Minute => t.checked_add_signed(TimeDelta::try_minutes(n)?),
        Unit::Second => t.checked_add_signed(TimeDelta::try_seconds(n)?),
    }
}

impl DateCtx {
    /// A UTC timestamp as local time.
    pub fn local(&self, ts: i64) -> Option<NaiveDateTime> {
        DateTime::from_timestamp_millis(ts.checked_add(self.offset_ms)?).map(|d| d.naive_utc())
    }

    /// Local day number (days since 1970-01-01 local).
    pub fn local_day(&self, ts: i64) -> i64 {
        ts.saturating_add(self.offset_ms).div_euclid(86_400_000)
    }

    fn now_local(&self) -> NaiveDateTime {
        self.local(self.now).unwrap_or_default()
    }

    fn utc(&self, t: NaiveDateTime) -> f64 {
        (t.and_utc().timestamp_millis() - self.offset_ms) as f64
    }

    /// `[a, b)` in local time.
    fn span(&self, a: NaiveDateTime, b: NaiveDateTime) -> NumVal {
        NumVal::span(self.utc(a), self.utc(b) - 1.0)
    }

    fn floor(&self, t: NaiveDateTime, unit: Unit) -> NaiveDateTime {
        let d = t.date();
        let midnight = d.and_hms_opt(0, 0, 0).unwrap_or(t);
        match unit {
            Unit::Year => NaiveDate::from_ymd_opt(d.year(), 1, 1)
                .map_or(t, |d| d.and_time(Default::default())),
            Unit::Month => NaiveDate::from_ymd_opt(d.year(), d.month(), 1)
                .map_or(t, |d| d.and_time(Default::default())),
            Unit::Week => {
                let back = (7 + d.weekday().num_days_from_monday()
                    - self.week_start.num_days_from_monday())
                    % 7;
                midnight
                    .checked_sub_days(Days::new(back.into()))
                    .unwrap_or(midnight)
            }
            Unit::Day => midnight,
            Unit::Hour => midnight + TimeDelta::hours(t.hour().into()),
            Unit::Minute => midnight + TimeDelta::minutes((t.hour() * 60 + t.minute()).into()),
            Unit::Second => t.with_nanosecond(0).unwrap_or(t),
        }
    }

    /// The calendar `unit` `k` units from the current one.
    fn calendar(&self, unit: Unit, k: i64) -> Option<NumVal> {
        let start = add(self.floor(self.now_local(), unit), unit, k)?;
        Some(self.span(start, add(start, unit, 1)?))
    }

    /// The last `n` units up to now (`n` > 0) or the next `-n` units from now.
    fn rolling(&self, unit: Unit, n: i64) -> Option<NumVal> {
        let now = self.now_local();
        let other = add(now, unit, -n)?;
        Some(if n >= 0 {
            NumVal::span(self.utc(other), self.utc(now))
        } else {
            NumVal::span(self.utc(now), self.utc(other))
        })
    }

    fn constant(&self, t: &str) -> Option<Result<NumVal, String>> {
        let range = || "date out of range".to_string();
        let v = match t {
            "today" => self.calendar(Unit::Day, 0),
            "yesterday" => self.calendar(Unit::Day, -1),
            "tomorrow" => self.calendar(Unit::Day, 1),
            "mtd" | "ytd" | "qtd" => {
                let now = self.now_local();
                let start = match t {
                    "mtd" => self.floor(now, Unit::Month),
                    "ytd" => self.floor(now, Unit::Year),
                    _ => {
                        let m = (now.month() - 1) / 3 * 3 + 1;
                        NaiveDate::from_ymd_opt(now.year(), m, 1)?.and_time(Default::default())
                    }
                };
                Some(NumVal::span(self.utc(start), self.utc(now)))
            }
            _ => {
                if let Some(m) = month_name(t) {
                    let y = self.now_local().year();
                    let a = NaiveDate::from_ymd_opt(y, m, 1)?.and_time(Default::default());
                    return Some(
                        add(a, Unit::Month, 1)
                            .map(|b| self.span(a, b))
                            .ok_or_else(range),
                    );
                }
                if let Some(w) = weekday_name(t) {
                    let today = self.floor(self.now_local(), Unit::Day);
                    let back = (7 + today.weekday().num_days_from_sunday() - w) % 7;
                    let a = add(today, Unit::Day, -i64::from(back))?;
                    return Some(
                        add(a, Unit::Day, 1)
                            .map(|b| self.span(a, b))
                            .ok_or_else(range),
                    );
                }
                return self.relative(t).map(|v| v.ok_or_else(range));
            }
        };
        Some(v.ok_or_else(range))
    }

    /// `thisweek`, `lastmonth`, `nextyear`, `pastday`, `past3days`, `last2weeks`,
    /// `next4hours`, `7days`.
    fn relative(&self, t: &str) -> Option<Option<NumVal>> {
        const WORDS: [&str; 8] = [
            "previous", "current", "coming", "this", "last", "prev", "past", "next",
        ];
        let (word, rest) = WORDS
            .iter()
            .find_map(|w| t.strip_prefix(w).map(|r| (*w, r)))
            .unwrap_or(("", t));
        if let Some(unit) = unit_of(rest) {
            return match word {
                "this" | "current" => Some(self.calendar(unit, 0)),
                "last" | "prev" | "previous" => Some(self.calendar(unit, -1)),
                "next" | "coming" => Some(self.calendar(unit, 1)),
                "past" => Some(self.rolling(unit, 1)),
                _ => None,
            };
        }
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return None;
        }
        let unit = unit_of(&rest[digits..])?;
        let n: i64 = rest[..digits].parse().ok()?;
        match word {
            "" | "last" | "prev" | "previous" | "past" => Some(self.rolling(unit, n)),
            "next" | "coming" => Some(self.rolling(unit, n.checked_neg()?)),
            _ => None,
        }
    }

    fn century(&self) -> i64 {
        i64::from(self.now_local().year()).div_euclid(100) * 100
    }

    /// A date value (lower case, trimmed by the caller).
    pub fn parse(&self, s: &str) -> Result<DateVal, String> {
        let t = s.trim().to_ascii_lowercase();
        let bad = || format!("`{s}` is not a date (try today, lastweek, 2024, 2024-05-31 or 10am)");
        if t.is_empty() {
            return Err(bad());
        }
        if let Some(v) = self.constant(&t) {
            return v.map(DateVal::Span);
        }
        let fields = if t.bytes().all(|b| b.is_ascii_digit()) {
            if t.len() >= 9 {
                return filetime(t.parse::<u64>().map_err(|_| bad())?).ok_or_else(bad);
            }
            self.digits(&t)
        } else if let Some(h) = t.strip_prefix("0x") {
            return u64::from_str_radix(h, 16)
                .ok()
                .and_then(filetime)
                .ok_or_else(bad);
        } else if let Some(f) = basic(&t) {
            Some(f)
        } else if let Some(f) = extended(&t) {
            Some(f)
        } else if let Some(f) = self.local_format(&t) {
            Some(f)
        } else if let Some((secs, gran)) = time_of_day(&t, false) {
            let today = self.floor(self.now_local(), Unit::Day);
            let a = today + TimeDelta::seconds(secs);
            return Ok(DateVal::Span(self.span(a, a + TimeDelta::seconds(gran))));
        } else {
            None
        };
        let (f, prec) = fields.ok_or_else(bad)?;
        if f[..prec].iter().any(Option::is_none) {
            return Ok(DateVal::Pattern(f, prec));
        }
        self.fields_span(&f, prec)
            .map(DateVal::Span)
            .ok_or_else(bad)
    }

    /// `YY`, `YYYY`, Excel serial days (5 digits), `YYMMDD`, `YYYYMMDD`.
    fn digits(&self, t: &str) -> Option<([Option<i64>; 7], usize)> {
        match t.len() {
            2 => Some(fields(&[self.century() + num(t)?])),
            4 => Some(fields(&[num(t)?])),
            5 => {
                let base = NaiveDate::from_ymd_opt(1899, 12, 30)?;
                let d = base.checked_add_days(Days::new(num(t)?.unsigned_abs()))?;
                Some(fields(&[d.year().into(), d.month().into(), d.day().into()]))
            }
            6 => Some(fields(&[
                self.century() + num(&t[..2])?,
                num(&t[2..4])?,
                num(&t[4..])?,
            ])),
            8 => Some(fields(&[num(&t[..4])?, num(&t[4..6])?, num(&t[6..])?])),
            _ => None,
        }
    }

    fn fields_span(&self, f: &[Option<i64>; 7], prec: usize) -> Option<NumVal> {
        let get = |i: usize, d: i64| f[i].unwrap_or(d);
        let date = NaiveDate::from_ymd_opt(
            i32::try_from(get(0, 1970)).ok()?,
            u32::try_from(get(1, 1)).ok()?,
            u32::try_from(get(2, 1)).ok()?,
        )?;
        let start = date.and_hms_milli_opt(
            u32::try_from(get(3, 0)).ok()?,
            u32::try_from(get(4, 0)).ok()?,
            u32::try_from(get(5, 0)).ok()?,
            u32::try_from(get(6, 0)).ok()?,
        )?;
        let end = match prec {
            1 => add(start, Unit::Year, 1)?,
            2 => add(start, Unit::Month, 1)?,
            3 => add(start, Unit::Day, 1)?,
            4 => add(start, Unit::Hour, 1)?,
            5 => add(start, Unit::Minute, 1)?,
            6 => add(start, Unit::Second, 1)?,
            _ => start.checked_add_signed(TimeDelta::milliseconds(1))?,
        };
        Some(self.span(start, end))
    }

    /// `a/b/c`, `a.b.c`, `MM/YYYY`, `YYYY/MM` by [`DateOrder`].
    fn local_format(&self, t: &str) -> Option<([Option<i64>; 7], usize)> {
        let sep = if t.contains('/') {
            '/'
        } else if t.contains('.') {
            '.'
        } else {
            return None;
        };
        let parts: Vec<&str> = t.split(sep).collect();
        if parts
            .iter()
            .any(|p| p.is_empty() || p.len() > 4 || !p.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        let year = |p: &str| -> Option<i64> {
            match p.len() {
                4 => num(p),
                1 | 2 => Some(self.century() + num(p)?),
                _ => None,
            }
        };
        match parts.as_slice() {
            [a, b] if a.len() == 4 => Some(fields(&[num(a)?, num(b)?])),
            [a, b] if b.len() == 4 => Some(fields(&[num(b)?, num(a)?])),
            [a, b, c] if a.len() == 4 => Some(fields(&[num(a)?, num(b)?, num(c)?])),
            [a, b, c] => match self.order {
                DateOrder::Mdy => Some(fields(&[year(c)?, num(a)?, num(b)?])),
                DateOrder::Dmy => Some(fields(&[year(c)?, num(b)?, num(a)?])),
                DateOrder::Ymd => Some(fields(&[year(a)?, num(b)?, num(c)?])),
            },
            _ => None,
        }
    }
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || s.len() > 9 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn fields(v: &[i64]) -> ([Option<i64>; 7], usize) {
    let mut f = [None; 7];
    for (slot, x) in f.iter_mut().zip(v) {
        *slot = Some(*x);
    }
    (f, v.len())
}

/// 100 ns ticks since 1601 → a one-millisecond span.
fn filetime(ft: u64) -> Option<DateVal> {
    let ms = i64::try_from(ft / 10_000).ok()? - 11_644_473_600_000;
    Some(DateVal::Span(NumVal::point(ms as f64)))
}

/// `YYYYMMDDThh[mm[ss]]`.
fn basic(t: &str) -> Option<([Option<i64>; 7], usize)> {
    let (d, tm) = t.split_once('t')?;
    if d.len() != 8 || !matches!(tm.len(), 2 | 4 | 6) {
        return None;
    }
    let mut v = vec![num(&d[..4])?, num(&d[4..6])?, num(&d[6..])?];
    for i in (0..tm.len()).step_by(2) {
        v.push(num(&tm[i..i + 2])?);
    }
    Some(fields(&v))
}

/// `YYYY[-MM[-DD[Thh[:mm[:ss[.sss]]]]]]`, any field may be `*`.
fn extended(t: &str) -> Option<([Option<i64>; 7], usize)> {
    let (d, tm) = match t.split_once(['t', ' ']) {
        Some((d, tm)) => (d, Some(tm)),
        None => (t, None),
    };
    let field = |p: &str, max_len: usize| -> Option<Option<i64>> {
        if p == "*" {
            Some(None)
        } else if !p.is_empty() && p.len() <= max_len {
            num(p).map(Some)
        } else {
            None
        }
    };
    let dparts: Vec<&str> = d.split('-').collect();
    if dparts.len() > 3 || (dparts.len() < 2 && tm.is_none() && !d.contains('*')) {
        return None;
    }
    let mut f = [None; 7];
    let mut prec = 0;
    for (i, p) in dparts.iter().enumerate() {
        f[i] = field(p, if i == 0 { 4 } else { 2 })?;
        prec = i + 1;
    }
    if dparts[0] != "*" && dparts[0].len() != 4 {
        return None;
    }
    if let Some(tm) = tm {
        if dparts.len() != 3 {
            return None;
        }
        let (hms, frac) = match tm.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (tm, None),
        };
        let tparts: Vec<&str> = hms.split(':').collect();
        if tparts.len() > 3 {
            return None;
        }
        for (i, p) in tparts.iter().enumerate() {
            f[3 + i] = field(p, 2)?;
            prec = 4 + i;
        }
        if let Some(frac) = frac {
            if tparts.len() != 3 || frac.is_empty() || frac.len() > 3 {
                return None;
            }
            let ms = num(frac)? * 10i64.pow(3 - frac.len() as u32);
            f[6] = Some(ms);
            prec = 7;
        }
    }
    Some((f, prec))
}

/// `10am`, `2:30pm`, `17:30`, `17:30:15` (and a bare hour when `bare_hour`) → seconds
/// since midnight and the granularity in seconds.
pub(crate) fn time_of_day(t: &str, bare_hour: bool) -> Option<(i64, i64)> {
    let (body, pm) = if let Some(b) = t.strip_suffix("am") {
        (b.trim_end(), Some(false))
    } else if let Some(b) = t.strip_suffix("pm") {
        (b.trim_end(), Some(true))
    } else {
        (t, None)
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() > 3 || parts.iter().any(|p| p.is_empty() || p.len() > 2) {
        return None;
    }
    if parts.len() == 1 && pm.is_none() && !bare_hour {
        return None;
    }
    let mut h = num(parts[0])?;
    let m = parts.get(1).map_or(Some(0), |p| num(p))?;
    let s = parts.get(2).map_or(Some(0), |p| num(p))?;
    if m > 59 || s > 59 {
        return None;
    }
    match pm {
        Some(pm) => {
            if !(1..=12).contains(&h) {
                return None;
            }
            h = h % 12 + if pm { 12 } else { 0 };
        }
        None if h > 23 => return None,
        None => {}
    }
    Some((h * 3600 + m * 60 + s, [3600, 60, 1][parts.len() - 1]))
}

/// The local fields of a timestamp, for wildcard patterns.
pub(crate) fn local_fields(t: NaiveDateTime) -> [i64; 7] {
    [
        t.year().into(),
        t.month().into(),
        t.day().into(),
        t.hour().into(),
        t.minute().into(),
        t.second().into(),
        (t.nanosecond() / 1_000_000).into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-12 12:00 UTC, a Monday.
    pub(crate) fn ctx() -> DateCtx {
        DateCtx {
            now: 1_791_806_400_000,
            offset_ms: 0,
            week_start: Weekday::Mon,
            order: DateOrder::Mdy,
        }
    }

    fn ms(s: &str) -> f64 {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .unwrap()
            .and_utc()
            .timestamp_millis() as f64
    }

    fn span(s: &str) -> (f64, f64) {
        match ctx().parse(s).unwrap() {
            DateVal::Span(v) => v.eq,
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn now_is_monday_noon() {
        assert_eq!(ms("2026-10-12 12:00:00"), ctx().now as f64);
    }

    #[test]
    fn constants() {
        assert_eq!(
            span("today"),
            (ms("2026-10-12 00:00:00"), ms("2026-10-13 00:00:00") - 1.0)
        );
        assert_eq!(span("yesterday").0, ms("2026-10-11 00:00:00"));
        assert_eq!(span("thisweek").0, ms("2026-10-12 00:00:00"));
        assert_eq!(
            span("lastweek"),
            (ms("2026-10-05 00:00:00"), ms("2026-10-12 00:00:00") - 1.0)
        );
        assert_eq!(span("lastmonth").0, ms("2026-09-01 00:00:00"));
        assert_eq!(span("nextyear").0, ms("2027-01-01 00:00:00"));
        assert_eq!(
            span("pastweek"),
            (ms("2026-10-05 12:00:00"), ms("2026-10-12 12:00:00"))
        );
        assert_eq!(span("past3days").0, ms("2026-10-09 12:00:00"));
        assert_eq!(span("7days").0, ms("2026-10-05 12:00:00"));
        assert_eq!(
            span("next2hours"),
            (ms("2026-10-12 12:00:00"), ms("2026-10-12 14:00:00"))
        );
        assert_eq!(span("feb").0, ms("2026-02-01 00:00:00"));
        assert_eq!(span("friday").0, ms("2026-10-09 00:00:00"));
        assert_eq!(span("monday").0, ms("2026-10-12 00:00:00"));
        assert_eq!(span("qtd").0, ms("2026-10-01 00:00:00"));
        assert_eq!(span("ytd").0, ms("2026-01-01 00:00:00"));
    }

    #[test]
    fn formats() {
        assert_eq!(
            span("2024"),
            (ms("2024-01-01 00:00:00"), ms("2025-01-01 00:00:00") - 1.0)
        );
        assert_eq!(span("2024-02").1, ms("2024-03-01 00:00:00") - 1.0);
        assert_eq!(span("2024-02-29T13").0, ms("2024-02-29 13:00:00"));
        assert_eq!(
            span("2024-02-29T13:05:09.5").0,
            ms("2024-02-29 13:05:09") + 500.0
        );
        assert_eq!(span("20240229").0, ms("2024-02-29 00:00:00"));
        assert_eq!(span("20240229T1305").0, ms("2024-02-29 13:05:00"));
        assert_eq!(span("240229").0, ms("2024-02-29 00:00:00"));
        assert_eq!(span("24").0, ms("2024-01-01 00:00:00"));
        assert_eq!(span("02/29/2024").0, ms("2024-02-29 00:00:00"));
        assert_eq!(span("2024/02/29").0, ms("2024-02-29 00:00:00"));
        assert_eq!(span("02/2024").0, ms("2024-02-01 00:00:00"));
        assert_eq!(span("45000").0, ms("2023-03-15 00:00:00"));
        assert_eq!(span("133000000000000000").0, 1_655_526_400_000.0);
        assert_eq!(
            span("10am"),
            (ms("2026-10-12 10:00:00"), ms("2026-10-12 11:00:00") - 1.0)
        );
        assert_eq!(span("17:30").0, ms("2026-10-12 17:30:00"));
        assert!(ctx().parse("2024-13").is_err());
        assert!(ctx().parse("2024-02-30").is_err());
        assert!(ctx().parse("soon").is_err());
        assert!(ctx().parse("999").is_err());
        let dmy = DateCtx {
            order: DateOrder::Dmy,
            ..ctx()
        };
        assert!(matches!(dmy.parse("29/02/2024"), Ok(DateVal::Span(_))));
    }

    #[test]
    fn wildcards() {
        match ctx().parse("*-*-*T9:00").unwrap() {
            DateVal::Pattern(f, prec) => {
                assert_eq!(prec, 5);
                assert_eq!(f, [None, None, None, Some(9), Some(0), None, None]);
            }
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn times() {
        assert_eq!(time_of_day("12am", false), Some((0, 3600)));
        assert_eq!(time_of_day("12pm", false), Some((12 * 3600, 3600)));
        assert_eq!(time_of_day("2:30pm", false), Some((14 * 3600 + 1800, 60)));
        assert_eq!(time_of_day("17", false), None);
        assert_eq!(time_of_day("17", true), Some((17 * 3600, 3600)));
        assert_eq!(time_of_day("25:00", false), None);
    }
}
