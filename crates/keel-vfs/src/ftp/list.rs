//! FTP directory listings: `MLSD` facts (RFC 3659) and `LIST` lines in the UNIX `ls -l`
//! and the DOS (IIS) formats. Both read bytes from the server: lines and entries are
//! bounded, and a line that cannot be parsed is skipped, never trusted.

use chrono::{Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};
use std::io::{self, BufRead};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A listing line longer than this is an error (names are at most a few hundred bytes).
pub const MAX_LINE: usize = 8 * 1024;
/// A folder with more entries than this is refused rather than held in memory.
pub const MAX_ENTRIES: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    File,
    Dir,
    /// A symbolic link (its target is unknown here).
    Link,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub kind: Type,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// The lines of a listing (CR LF or LF, empty lines dropped), decoded lossily: a name
/// that is not UTF-8 keeps U+FFFD and is refused when used.
pub fn lines(mut r: impl BufRead) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = io::Read::take(r.by_ref(), MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        if buf.last() != Some(&b'\n') && buf.len() > MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FTP listing line too long",
            ));
        }
        while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            buf.pop();
        }
        if buf.is_empty() {
            continue;
        }
        if out.len() >= MAX_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the folder has more than {MAX_ENTRIES} entries"),
            ));
        }
        out.push(String::from_utf8_lossy(&buf).into_owned());
    }
}

/// One `MLSD` line: `fact=value;...; name`. None for the folder itself, its parent, and
/// lines without a usable name or type.
pub fn mlsd(line: &str) -> Option<Item> {
    facts(line, false).filter(|item| usable(&item.name))
}

/// The `MLST` answer for one path (its name is the path, and a folder may say `cdir`).
pub fn mlst(line: &str) -> Option<Item> {
    facts(line, true)
}

fn facts(line: &str, this: bool) -> Option<Item> {
    let (facts, name) = line.trim_start().split_once(' ')?;
    let mut item = Item {
        name: name.to_owned(),
        kind: Type::File,
        size: 0,
        modified: None,
    };
    let mut typed = false;
    for fact in facts.split(';') {
        let Some((key, value)) = fact.split_once('=') else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "type" => {
                let value = value.to_ascii_lowercase();
                item.kind = match value.as_str() {
                    "file" => Type::File,
                    "dir" => Type::Dir,
                    "cdir" if this => Type::Dir,
                    "cdir" | "pdir" => return None,
                    v if v.starts_with("os.unix=slink") || v.starts_with("os.unix=symlink") => {
                        Type::Link
                    }
                    _ => return None,
                };
                typed = true;
            }
            "size" => item.size = value.parse().unwrap_or(0),
            "modify" => item.modified = mlsd_time(value),
            _ => {}
        }
    }
    typed.then_some(item)
}

/// `YYYYMMDDHHMMSS[.sss]`, UTC.
fn mlsd_time(v: &str) -> Option<SystemTime> {
    let digits = v.split('.').next()?;
    let t = NaiveDateTime::parse_from_str(digits, "%Y%m%d%H%M%S").ok()?;
    system_time(t)
}

/// `t` (UTC) as a `SystemTime`; None outside what the platform can hold (Windows: before
/// 1601), where chrono's conversion would panic.
fn system_time(t: NaiveDateTime) -> Option<SystemTime> {
    let secs = Utc.from_utc_datetime(&t).timestamp();
    match u64::try_from(secs) {
        Ok(after) => UNIX_EPOCH.checked_add(Duration::from_secs(after)),
        Err(_) => UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs())),
    }
}

fn usable(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// One `LIST` line, UNIX or DOS format; `now` places dates shown without a year. None for
/// `total N`, `.`, `..` and lines in neither format.
pub fn list(line: &str, now: SystemTime) -> Option<Item> {
    unix(line, now).or_else(|| dos(line))
}

/// Whitespace-separated words with the byte offset where each ends.
fn words(line: &str) -> impl Iterator<Item = (&str, usize)> {
    line.split_ascii_whitespace().map(move |w| {
        let start = w.as_ptr() as usize - line.as_ptr() as usize;
        (w, start + w.len())
    })
}

/// `drwxr-xr-x 2 owner group 4096 Jan  1 12:00 name` (owner, group and link count vary:
/// the date is found by its month name, the size is the word before it).
fn unix(line: &str, now: SystemTime) -> Option<Item> {
    let perms = line.split_ascii_whitespace().next()?;
    let kind = match perms.as_bytes().first()? {
        b'-' => Type::File,
        b'd' => Type::Dir,
        b'l' => Type::Link,
        // Devices, pipes, sockets: listed as files (reading them fails on the server).
        b'b' | b'c' | b'p' | b's' => Type::File,
        _ => return None,
    };
    if perms.len() < 10 {
        return None;
    }
    let w: Vec<(&str, usize)> = words(line).take(12).collect();
    // The first `size month day time-or-year` run after the permissions and link count
    // (an owner named like a month does not match: the words after it are no date).
    let (m, modified) = (3..w.len().saturating_sub(2)).find_map(|i| {
        let t = unix_time(month(w[i].0)?, w[i + 1].0, w[i + 2].0, now)?;
        w[i - 1].0.parse::<u64>().ok()?;
        Some((i, t))
    })?;
    let size = w[m - 1].0.parse().ok()?;
    let name = line[w[m + 2].1..].strip_prefix(' ')?;
    let name = match kind {
        Type::Link => name.split_once(" -> ").map_or(name, |(n, _)| n),
        _ => name,
    };
    let modified = Some(modified);
    let item = Item {
        name: name.to_owned(),
        kind,
        size,
        modified,
    };
    usable(&item.name).then_some(item)
}

fn month(w: &str) -> Option<u32> {
    let w = w.to_ascii_lowercase();
    MONTHS.iter().position(|m| *m == w).map(|i| i as u32 + 1)
}

/// `Jan 1 12:00` (within the last year: the year is now's, or the one before when that
/// date would be more than a day ahead) or `Jan 1 2020`; server time taken as UTC.
fn unix_time(month: u32, day: &str, at: &str, now: SystemTime) -> Option<SystemTime> {
    let day: u32 = day.parse().ok()?;
    let t = match at.split_once(':') {
        Some((h, m)) => {
            let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
            let now = chrono::DateTime::<Utc>::from(now);
            let mut year = now.year();
            let date = |y| NaiveDate::from_ymd_opt(y, month, day)?.and_hms_opt(h, m, 0);
            if date(year)? > (now + chrono::Duration::days(1)).naive_utc() {
                year -= 1;
            }
            date(year)?
        }
        None => NaiveDate::from_ymd_opt(at.parse().ok()?, month, day)?.and_hms_opt(0, 0, 0)?,
    };
    system_time(t)
}

/// `01-15-24  03:04PM  <DIR>  name` or `01-15-2024  15:04  1234 name` (IIS).
fn dos(line: &str) -> Option<Item> {
    let w: Vec<(&str, usize)> = words(line).take(3).collect();
    let [(date, _), (time, _), (third, end)] = w[..] else {
        return None;
    };
    let mut d = date.split('-');
    let (mo, dd, yy) = (d.next()?, d.next()?, d.next()?);
    let (mo, dd, mut yy): (u32, u32, i32) = (mo.parse().ok()?, dd.parse().ok()?, yy.parse().ok()?);
    if yy < 100 {
        yy += if yy < 70 { 2000 } else { 1900 };
    }
    let upper = time.to_ascii_uppercase();
    let (clock, pm) = match (upper.strip_suffix("PM"), upper.strip_suffix("AM")) {
        (Some(c), _) => (c, Some(true)),
        (_, Some(c)) => (c, Some(false)),
        _ => (upper.as_str(), None),
    };
    let (h, m) = clock.split_once(':')?;
    let (mut h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    match pm {
        Some(true) if h < 12 => h += 12,
        Some(false) if h == 12 => h = 0,
        _ => {}
    }
    let t = NaiveDate::from_ymd_opt(yy, mo, dd)?.and_hms_opt(h, m, 0)?;
    let (kind, size) = match third {
        "<DIR>" => (Type::Dir, 0),
        n => (Type::File, n.parse().ok()?),
    };
    let item = Item {
        name: line[end..].trim_start().to_owned(),
        kind,
        size,
        modified: Some(system_time(t)?),
    };
    usable(&item.name).then_some(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> SystemTime {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().into()
    }

    #[test]
    fn unix_lines() {
        let now = at(2026, 3, 1, 12, 0);
        let f = list(
            "-rw-r--r--    1 1000     1000         1234 Feb 27 09:05 a file.txt",
            now,
        )
        .unwrap();
        assert_eq!(f.name, "a file.txt");
        assert_eq!((f.kind, f.size), (Type::File, 1234));
        assert_eq!(f.modified, Some(at(2026, 2, 27, 9, 5)));
        // A date shown with a time but after now belongs to last year.
        let old = list("drwxr-xr-x 2 u g 4096 Dec 24 18:00 Ordner ü", now).unwrap();
        assert_eq!((old.kind, old.name.as_str()), (Type::Dir, "Ordner ü"));
        assert_eq!(old.modified, Some(at(2025, 12, 24, 18, 0)));
        let year = list("-rw-r--r-- 1 owner 7 Jan  5  2019 no group", now).unwrap();
        assert_eq!((year.size, year.name.as_str()), (7, "no group"));
        let may = list(
            "-rw-r--r-- 1 may staff 12 Jan 1 2020 owner like a month",
            now,
        )
        .unwrap();
        assert_eq!((may.size, may.name.as_str()), (12, "owner like a month"));
        assert_eq!(year.modified, Some(at(2019, 1, 5, 0, 0)));
        let link = list("lrwxrwxrwx 1 u g 4 Jan 1 2020 latest -> v2", now).unwrap();
        assert_eq!((link.kind, link.name.as_str()), (Type::Link, "latest"));
        // Leading spaces in a name survive (one space separates it from the date).
        let spaced = list("-rw-r--r-- 1 u g 1 Jan 1 2020   lead", now).unwrap();
        assert_eq!(spaced.name, "  lead");
        // Before 1601 (Windows' SystemTime) or past chrono's range: skipped, no panic.
        assert_eq!(list("-rw-r--r-- 1 u g 1 Jan 1 1000 old", now), None);
        assert_eq!(list("01-01-1000  01:00AM  1 old", now), None);
        for skipped in [
            "total 12",
            "drwxr-xr-x 2 u g 4096 Jan 1 2020 .",
            "drwxr-xr-x 2 u g 4096 Jan 1 2020 ..",
            "-rw-r--r-- 1 u g x Jan 1 2020 bad size",
            "-rw-r--r-- 1 u g 1 Foo 1 2020 bad month",
            "-rw-r--r-- 1 u g 1 Jan 1 2020",
            "-rw-r--r-- 1 u g 1 Jan 1 2020 a/b",
            "",
        ] {
            assert_eq!(list(skipped, now), None, "{skipped:?}");
        }
    }

    #[test]
    fn dos_lines() {
        let now = UNIX_EPOCH;
        let d = list("01-15-24  03:04PM       <DIR>          My Folder", now).unwrap();
        assert_eq!((d.kind, d.name.as_str()), (Type::Dir, "My Folder"));
        assert_eq!(d.modified, Some(at(2024, 1, 15, 15, 4)));
        let f = list("12-31-1999  12:30AM              1234 old.txt", now).unwrap();
        assert_eq!((f.kind, f.size), (Type::File, 1234));
        assert_eq!(f.modified, Some(at(1999, 12, 31, 0, 30)));
        let h24 = list("02-01-2025  23:59  5 x", now).unwrap();
        assert_eq!(h24.modified, Some(at(2025, 2, 1, 23, 59)));
        assert_eq!(list("13-45-24  03:04PM  1 bad date", now), None);
    }

    #[test]
    fn mlsd_lines() {
        let f = mlsd("type=file;size=42;modify=20240115150405.123;perm=r; a; b.txt").unwrap();
        assert_eq!(
            (f.kind, f.size, f.name.as_str()),
            (Type::File, 42, "a; b.txt")
        );
        assert_eq!(
            f.modified,
            Some(at(2024, 1, 15, 15, 4) + Duration::from_secs(5))
        );
        let d = mlsd("Type=dir;Modify=20240115150405; Ordner ü").unwrap();
        assert_eq!(d.kind, Type::Dir);
        let l = mlsd("type=OS.unix=slink:/x;size=1; ln").unwrap();
        assert_eq!(l.kind, Type::Link);
        assert_eq!(
            mlst(" type=cdir;modify=20240101000000; /a b").unwrap().kind,
            Type::Dir
        );
        assert_eq!(mlst("type=file;size=3; /x/y").unwrap().size, 3);
        for skipped in [
            "type=cdir; .",
            "type=pdir; ..",
            "type=dir; ..",
            "size=1; untyped",
            "type=file;size=1;",
            "type=weird; x",
        ] {
            assert_eq!(mlsd(skipped), None, "{skipped:?}");
        }
    }

    #[test]
    fn lines_are_bounded() {
        let ok = lines(&b"a\r\n\r\nb\nc"[..]).unwrap();
        assert_eq!(ok, ["a", "b", "c"]);
        let long = vec![b'x'; MAX_LINE + 1];
        assert!(lines(&long[..]).is_err());
        let fits = vec![b'x'; MAX_LINE];
        assert_eq!(lines(&fits[..]).unwrap().len(), 1);
        let bad = lines(&b"\xff\xfe name\n"[..]).unwrap();
        assert!(bad[0].contains('\u{FFFD}'));
    }
}
