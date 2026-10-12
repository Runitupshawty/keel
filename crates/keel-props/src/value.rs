//! The value a property read returns.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One property value. `Unknown` means the property does not exist for this file on this
/// platform (Everything's `unknown`), which is not an error.
#[derive(Debug, Clone, PartialEq)]
pub enum Prop {
    /// No value: the file has no such property, or no reader knows it here.
    Unknown,
    /// Text (titles, names, versions, hashes, MIME types, paths).
    Str(String),
    /// A whole number (counts, track numbers, years, sample rates).
    Int(i64),
    /// A real number (frame rates, f-stops, coordinates).
    Float(f64),
    /// Yes or no (part of a compilation).
    Bool(bool),
    /// A point in time (UTC).
    Date(SystemTime),
    /// A length of time (audio length, total editing time).
    Duration(Duration),
    /// Several values (artists, keywords).
    List(Vec<Prop>),
}

impl Prop {
    /// True for [`Prop::Unknown`] and for an empty string or list.
    pub fn is_unknown(&self) -> bool {
        match self {
            Prop::Unknown => true,
            Prop::Str(s) => s.is_empty(),
            Prop::List(v) => v.is_empty(),
            _ => false,
        }
    }

    /// `Some(text)` for a non-empty string, else `None`.
    pub fn str(s: Option<String>) -> Prop {
        match s {
            Some(s) if !s.trim().is_empty() => Prop::Str(s.trim().to_owned()),
            _ => Prop::Unknown,
        }
    }

    /// The value as a string when it is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Prop::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The value as a whole number when it is an `Int` (or a whole `Float`).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Prop::Int(n) => Some(*n),
            Prop::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Some(*f as i64),
            _ => None,
        }
    }

    /// The value as a real number when it is numeric.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Prop::Int(n) => Some(*n as f64),
            Prop::Float(f) => Some(*f),
            _ => None,
        }
    }
}

impl From<Option<u64>> for Prop {
    fn from(v: Option<u64>) -> Prop {
        v.map_or(Prop::Unknown, |n| {
            Prop::Int(i64::try_from(n).unwrap_or(i64::MAX))
        })
    }
}

impl fmt::Display for Prop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Prop::Unknown => Ok(()),
            Prop::Str(s) => f.write_str(s),
            Prop::Int(n) => write!(f, "{n}"),
            Prop::Float(x) => write!(f, "{x}"),
            Prop::Bool(b) => f.write_str(if *b { "yes" } else { "no" }),
            Prop::Date(t) => write_date(f, *t),
            Prop::Duration(d) => {
                let s = d.as_secs();
                if s >= 3600 {
                    write!(f, "{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
                } else {
                    write!(f, "{}:{:02}", s / 60, s % 60)
                }
            }
            Prop::List(v) => {
                for (i, p) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str("; ")?;
                    }
                    write!(f, "{p}")?;
                }
                Ok(())
            }
        }
    }
}

/// ISO 8601 in UTC, to the second (`2024-05-01T12:30:00Z`).
fn write_date(f: &mut fmt::Formatter<'_>, t: SystemTime) -> fmt::Result {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    write!(
        f,
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// A Windows FILETIME (100 ns ticks since 1601-01-01 UTC) as a `SystemTime`.
pub fn filetime_to_system_time(ticks: u64) -> SystemTime {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let secs = ticks / 10_000_000;
    let nanos = (ticks % 10_000_000) as u32 * 100;
    if secs >= EPOCH_DIFF_SECS {
        UNIX_EPOCH + Duration::new(secs - EPOCH_DIFF_SECS, nanos)
    } else {
        UNIX_EPOCH - Duration::new(EPOCH_DIFF_SECS - secs, 0) + Duration::new(0, nanos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display() {
        assert_eq!(Prop::Unknown.to_string(), "");
        assert_eq!(Prop::Int(42).to_string(), "42");
        assert_eq!(Prop::Bool(true).to_string(), "yes");
        assert_eq!(
            Prop::Duration(Duration::from_secs(3725)).to_string(),
            "1:02:05"
        );
        assert_eq!(Prop::Duration(Duration::from_secs(65)).to_string(), "1:05");
        let list = Prop::List(vec![Prop::Str("a".into()), Prop::Str("b".into())]);
        assert_eq!(list.to_string(), "a; b");
        let t = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(Prop::Date(t).to_string(), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn filetime() {
        // 2001-01-01T00:00:00Z
        let t = filetime_to_system_time(126_227_808_000_000_000);
        assert_eq!(Prop::Date(t).to_string(), "2001-01-01T00:00:00Z");
        let t = filetime_to_system_time(0);
        assert_eq!(Prop::Date(t).to_string(), "1601-01-01T00:00:00Z");
    }

    #[test]
    fn unknown() {
        assert!(Prop::Unknown.is_unknown());
        assert!(Prop::Str(String::new()).is_unknown());
        assert!(Prop::List(vec![]).is_unknown());
        assert!(!Prop::Int(0).is_unknown());
        assert_eq!(Prop::str(Some("  ".into())), Prop::Unknown);
        assert_eq!(Prop::str(Some(" x ".into())), Prop::Str("x".into()));
    }
}
