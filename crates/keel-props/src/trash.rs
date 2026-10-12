//! Deleted items: when an item in a Windows Recycle Bin or a freedesktop.org Trash was
//! deleted, and where it was, from the bin's own records (`date-deleted:`).
//!
//! - **Windows** keeps a deleted item as `$Recycle.Bin\<SID>\$R<id><ext>` next to a small
//!   record `$I<id><ext>`: a version (1 before Windows 10, 2 since), the size, the deletion
//!   time (FILETIME, UTC) and the original path (UTF-16; 260 characters in version 1, a counted
//!   length in version 2). The record is read the same way on every platform (a Windows disk
//!   mounted elsewhere).
//! - **freedesktop.org** trash (Linux and BSD desktops) keeps `Trash/files/<name>` next to
//!   `Trash/info/<name>.trashinfo`, an INI file with `Path=` (percent-encoded) and
//!   `DeletionDate=` (local time, `YYYY-MM-DDThh:mm:ss`).
//!
//! An item inside a deleted folder has its folder's record. Both parsers are bounded and
//! fuzzed (`recycle_info`, `trash_info`).

use crate::value::filetime_to_system_time;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Largest record read: a version 2 `$I` file with a 32,767-character path is 65,562 bytes.
pub const MAX_INFO_BYTES: u64 = 72 * 1024;
/// Folders above an item looked at for a bin.
const MAX_DEPTH: usize = 64;
/// Lines of a `.trashinfo` file looked at.
const MAX_LINES: usize = 256;

/// What a bin's record says about a deleted item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    /// Where the item was before it was deleted.
    pub original_path: Option<String>,
    /// The item's size as the Recycle Bin recorded it (Windows only).
    pub size: Option<u64>,
    pub deleted: Option<SystemTime>,
}

/// Parses a Windows Recycle Bin `$I` record (version 1 or 2).
pub fn parse_recycle_info(b: &[u8]) -> Option<Deleted> {
    let u64_at = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
    };
    let version = u64_at(0)?;
    let size = u64_at(8)?;
    let filetime = u64_at(16)?;
    let path_bytes = match version {
        // 260 UTF-16 units, NUL-padded.
        1 => b.get(24..)?.get(..520).unwrap_or(&b[24..]),
        2 => {
            let units = u32::from_le_bytes(b.get(24..28)?.try_into().ok()?) as usize;
            let want = units.min(32_768) * 2;
            let rest = b.get(28..)?;
            &rest[..want.min(rest.len())]
        }
        _ => return None,
    };
    let units: Vec<u16> = path_bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    let path = String::from_utf16_lossy(&units);
    Some(Deleted {
        original_path: (!path.is_empty()).then_some(path),
        size: Some(size),
        deleted: if filetime == 0 {
            None
        } else {
            filetime_to_system_time(filetime)
        },
    })
}

/// Parses a freedesktop.org `.trashinfo` file. `None` without a `[Trash Info]` group.
pub fn parse_trash_info(b: &[u8]) -> Option<Deleted> {
    let text = String::from_utf8_lossy(b);
    let mut group = None::<bool>;
    let mut out = Deleted {
        original_path: None,
        size: None,
        deleted: None,
    };
    for line in text.lines().take(MAX_LINES) {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // Keys belong to the first group, which must be [Trash Info].
            if group.is_some() {
                break;
            }
            group = Some(name == "Trash Info");
            continue;
        }
        if group != Some(true) {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "Path" if out.original_path.is_none() => {
                    out.original_path = Some(percent_decode(v.trim()));
                }
                "DeletionDate" if out.deleted.is_none() => out.deleted = parse_local_date(v.trim()),
                _ => {}
            }
        }
    }
    (group == Some(true)).then_some(out)
}

/// `%XX` escapes decoded (invalid escapes stay as they are; bytes that are not UTF-8 become
/// U+FFFD).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' {
            if let (Some(h), Some(l)) = (
                b.get(i + 1).copied().and_then(hex),
                b.get(i + 2).copied().and_then(hex),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `YYYY-MM-DDThh:mm:ss` (optionally with fractional seconds) in local time, or with a
/// trailing `Z` or `+hh:mm`/`-hh:mm` offset.
fn parse_local_date(s: &str) -> Option<SystemTime> {
    use chrono::{FixedOffset, Local, NaiveDate, TimeZone};
    let num = |t: &str| -> Option<u32> {
        (!t.is_empty() && t.len() <= 4 && t.bytes().all(|c| c.is_ascii_digit()))
            .then(|| t.parse().ok())
            .flatten()
    };
    let (date, time) = s.split_once(['T', ' '])?;
    let mut d = date.split('-');
    let (y, m, dd) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
    if d.next().is_some() || date.len() != 10 {
        return None;
    }
    // The zone: Z, +hh:mm, -hh:mm or nothing (local time).
    let (clock, zone) = if let Some(c) = time.strip_suffix('Z') {
        (c, Some(0i32))
    } else if let Some(at) = time.rfind(['+', '-']) {
        let (c, z) = time.split_at(at);
        let sign = if z.starts_with('-') { -1 } else { 1 };
        let (zh, zm) = z[1..].split_once(':').unwrap_or((&z[1..], "0"));
        (
            c,
            Some(sign * (num(zh)? as i32 * 3600 + num(zm)? as i32 * 60)),
        )
    } else {
        (time, None)
    };
    let clock = clock.split('.').next()?;
    let mut t = clock.split(':');
    let (h, mi, sec) = (num(t.next()?)?, num(t.next()?)?, num(t.next()?)?);
    if t.next().is_some() {
        return None;
    }
    let naive = NaiveDate::from_ymd_opt(i32::try_from(y).ok()?, m, dd)?.and_hms_opt(h, mi, sec)?;
    match zone {
        Some(off) => {
            let tz = FixedOffset::east_opt(off)?;
            to_system_time(tz.from_local_datetime(&naive).single()?)
        }
        None => {
            // A time skipped by a daylight saving change is read an hour later.
            let dt = Local.from_local_datetime(&naive).earliest().or_else(|| {
                let later = naive.checked_add_signed(chrono::TimeDelta::hours(1))?;
                Local.from_local_datetime(&later).earliest()
            })?;
            to_system_time(dt)
        }
    }
}

/// Whole seconds of `dt`, or `None` when this platform's `SystemTime` cannot hold it.
fn to_system_time<Tz: chrono::TimeZone>(dt: chrono::DateTime<Tz>) -> Option<SystemTime> {
    let secs = dt.timestamp();
    match u64::try_from(secs) {
        Ok(s) => UNIX_EPOCH.checked_add(Duration::from_secs(s)),
        Err(_) => UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs())),
    }
}

/// Where the record of the deleted item at `path` is, if `path` (or a folder above it) is
/// in a Recycle Bin or a Trash.
pub fn record_path(path: &Path) -> Option<Record> {
    for item in path.ancestors().take(MAX_DEPTH) {
        let (Some(name), Some(parent)) = (item.file_name(), item.parent()) else {
            continue;
        };
        let name = name.to_string_lossy();
        let parent_name = parent.file_name().map(|n| n.to_string_lossy());
        // $Recycle.Bin\<SID>\$R... or the $I record itself.
        let in_bin = parent
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("$Recycle.Bin"));
        if in_bin && name.len() > 2 {
            let prefix = name.get(..2).unwrap_or("");
            if prefix.eq_ignore_ascii_case("$R") || prefix.eq_ignore_ascii_case("$I") {
                let rest = &name[2..];
                return Some(Record::RecycleBin(parent.join(format!("$I{rest}"))));
            }
        }
        // <trash>/files/<name>.
        if parent_name.as_deref() == Some("files") {
            if let Some(trash) = parent.parent() {
                if is_trash_dir(trash) {
                    return Some(Record::Trash(
                        trash.join("info").join(format!("{name}.trashinfo")),
                    ));
                }
            }
        }
    }
    None
}

/// A deleted item's record file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// A Windows `$I` file.
    RecycleBin(PathBuf),
    /// A freedesktop.org `.trashinfo` file.
    Trash(PathBuf),
}

/// `~/.local/share/Trash`, `$topdir/.Trash-<uid>` or `$topdir/.Trash/<uid>`.
fn is_trash_dir(p: &Path) -> bool {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
    match name.as_deref() {
        Some("Trash") => true,
        Some(n) if n.starts_with(".Trash-") => true,
        Some(_) => p
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == ".Trash"),
        None => false,
    }
}

/// The record of the deleted item at `path`: `Ok(None)` when it is not in a bin or its
/// record is missing or unreadable as one.
pub fn deleted(path: &Path) -> io::Result<Option<Deleted>> {
    let Some(record) = record_path(path) else {
        return Ok(None);
    };
    let (Record::RecycleBin(file) | Record::Trash(file)) = &record;
    let mut buf = Vec::new();
    match File::open(file) {
        Ok(f) => f.take(MAX_INFO_BYTES).read_to_end(&mut buf)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(match record {
        Record::RecycleBin(_) => parse_recycle_info(&buf),
        Record::Trash(_) => parse_trash_info(&buf),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures as fx;
    use crate::Prop;

    /// 2021-06-15T10:20:30Z as a FILETIME.
    const FT: u64 = 132_682_260_300_000_000;

    fn date(p: Option<SystemTime>) -> String {
        Prop::Date(p.unwrap()).to_string()
    }

    #[test]
    fn recycle_bin_records() {
        let v2 = fx::recycle_info(2, r"C:\Data\report.docx", 12_345, FT);
        let d = parse_recycle_info(&v2).unwrap();
        assert_eq!(d.original_path.as_deref(), Some(r"C:\Data\report.docx"));
        assert_eq!(d.size, Some(12_345));
        assert_eq!(date(d.deleted), "2021-06-15T10:20:30Z");
        let v1 = fx::recycle_info(1, r"D:\old\notes.txt", 7, FT);
        assert_eq!(v1.len(), 544);
        let d = parse_recycle_info(&v1).unwrap();
        assert_eq!(d.original_path.as_deref(), Some(r"D:\old\notes.txt"));
        assert_eq!(date(d.deleted), "2021-06-15T10:20:30Z");
        // Short, unknown and hostile records.
        assert!(parse_recycle_info(&v2[..20]).is_none());
        assert!(parse_recycle_info(&fx::recycle_info(3, "x", 1, FT)).is_none());
        let mut huge = v2[..24].to_vec();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        huge.extend_from_slice(&[0x41, 0]);
        assert_eq!(
            parse_recycle_info(&huge).unwrap().original_path.as_deref(),
            Some("A")
        );
        let mut v1_short = v1[..30].to_vec();
        v1_short.push(0x42);
        assert_eq!(
            parse_recycle_info(&v1_short)
                .unwrap()
                .original_path
                .as_deref(),
            Some(r"D:\")
        );
        let zero = fx::recycle_info(2, "x", 1, 0);
        assert_eq!(parse_recycle_info(&zero).unwrap().deleted, None);
    }

    #[test]
    fn trash_info_files() {
        let d = parse_trash_info(&fx::trash_info(
            "/home/u/My%20File%C3%A9.txt",
            "2021-06-15T10:20:30Z",
        ))
        .unwrap();
        assert_eq!(
            d.original_path.as_deref(),
            Some("/home/u/My File\u{e9}.txt")
        );
        assert_eq!(date(d.deleted), "2021-06-15T10:20:30Z");
        let d = parse_trash_info(b"[Trash Info]\nDeletionDate=2021-06-15T12:20:30.123+02:00\n")
            .unwrap();
        assert_eq!(date(d.deleted), "2021-06-15T10:20:30Z");
        assert_eq!(d.original_path, None);
        // Local time, as the specification writes it.
        let d = parse_trash_info(&fx::trash_info("/a", "2021-06-15T10:20:30")).unwrap();
        let local = chrono::NaiveDate::from_ymd_opt(2021, 6, 15)
            .unwrap()
            .and_hms_opt(10, 20, 30)
            .unwrap();
        use chrono::TimeZone;
        let want: SystemTime = chrono::Local
            .from_local_datetime(&local)
            .earliest()
            .unwrap()
            .into();
        assert_eq!(d.deleted, Some(want));
        // Not a trash info file, keys outside the group, bad dates.
        assert!(parse_trash_info(b"Path=/a\n").is_none());
        assert!(parse_trash_info(b"[Other]\nPath=/a\n").is_none());
        let d = parse_trash_info(b"[Trash Info]\nPath=/a\n[Other]\nPath=/b\n").unwrap();
        assert_eq!(d.original_path.as_deref(), Some("/a"));
        for bad in [
            "2021-13-01T00:00:00",
            "2021-06-15",
            "2021-06-15T25:00:00",
            "21-06-15T10:20:30",
            "2021-06-15T10:20:30+99:00",
            "2021-06-15T10:20",
            "2021-06-15T1:2:3:4",
        ] {
            let d = parse_trash_info(&fx::trash_info("/a", bad)).unwrap();
            assert_eq!(d.deleted, None, "{bad}");
        }
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%zz%41%"), "%zzA%");
    }

    #[test]
    fn records_of_items_in_bins() {
        let dir = tempfile::tempdir().unwrap();
        // A Windows Recycle Bin folder.
        let sid = dir.path().join("$Recycle.Bin").join("S-1-5-21-1-2-3-1001");
        std::fs::create_dir_all(sid.join("$RAB12CD").join("inner")).unwrap();
        std::fs::write(sid.join("$RXY34ZQ.txt"), b"deleted").unwrap();
        std::fs::write(
            sid.join("$IXY34ZQ.txt"),
            fx::recycle_info(2, r"C:\data\file.txt", 7, FT),
        )
        .unwrap();
        std::fs::write(
            sid.join("$IAB12CD"),
            fx::recycle_info(1, r"C:\data\folder", 0, FT),
        )
        .unwrap();
        let file = sid.join("$RXY34ZQ.txt");
        let d = deleted(&file).unwrap().unwrap();
        assert_eq!(d.original_path.as_deref(), Some(r"C:\data\file.txt"));
        assert_eq!(
            crate::get(&file, "date-deleted:").unwrap().to_string(),
            "2021-06-15T10:20:30Z"
        );
        let record = deleted(&sid.join("$IXY34ZQ.txt")).unwrap().unwrap();
        assert_eq!(record, d);
        // Inside a deleted folder: the folder's record.
        let inner = sid.join("$RAB12CD").join("inner");
        assert_eq!(
            deleted(&inner).unwrap().unwrap().original_path.as_deref(),
            Some(r"C:\data\folder")
        );
        // A $R item without its record, and files outside any bin.
        std::fs::write(sid.join("$RNOREC"), b"x").unwrap();
        assert_eq!(deleted(&sid.join("$RNOREC")).unwrap(), None);
        let outside = dir.path().join("$Rfile");
        std::fs::write(&outside, b"x").unwrap();
        assert_eq!(record_path(&outside), None);
        assert_eq!(crate::get(&outside, "date-deleted").unwrap(), Prop::Unknown);

        // freedesktop.org trash, in the home and on another volume.
        for trash in [
            dir.path().join("share").join("Trash"),
            dir.path().join("vol").join(".Trash-1000"),
            dir.path().join("vol2").join(".Trash").join("1000"),
        ] {
            std::fs::create_dir_all(trash.join("files").join("dir")).unwrap();
            std::fs::create_dir_all(trash.join("info")).unwrap();
            std::fs::write(trash.join("files").join("a b.txt"), b"x").unwrap();
            std::fs::write(
                trash.join("info").join("a b.txt.trashinfo"),
                fx::trash_info("/home/u/a%20b.txt", "2021-06-15T10:20:30Z"),
            )
            .unwrap();
            std::fs::write(
                trash.join("info").join("dir.trashinfo"),
                fx::trash_info("/home/u/dir", "2021-06-15T10:20:30Z"),
            )
            .unwrap();
            let f = trash.join("files").join("a b.txt");
            assert_eq!(
                deleted(&f).unwrap().unwrap().original_path.as_deref(),
                Some("/home/u/a b.txt")
            );
            assert_eq!(
                crate::get(&f, "date-deleted").unwrap().to_string(),
                "2021-06-15T10:20:30Z"
            );
            assert_eq!(
                deleted(&trash.join("files").join("dir"))
                    .unwrap()
                    .unwrap()
                    .original_path
                    .as_deref(),
                Some("/home/u/dir")
            );
        }
        let not_trash = dir.path().join("x").join("files");
        std::fs::create_dir_all(&not_trash).unwrap();
        assert_eq!(record_path(&not_trash.join("f")), None);
    }
}
