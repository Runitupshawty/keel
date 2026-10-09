//! `USN_RECORD_V2` / `USN_RECORD_V3` parsing (the output of `FSCTL_ENUM_USN_DATA` and
//! `FSCTL_READ_USN_JOURNAL`) and applying journal records to an [`Index`].

use std::collections::{HashMap, HashSet};

use super::index::Index;

pub(crate) const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub(crate) const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub(crate) const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// One MFT entry or journal record, reduced to what the index keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub frn: u64,
    pub parent: u64,
    pub usn: i64,
    pub reason: u32,
    pub is_dir: bool,
    pub name: String,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Parses one record (`rec` is exactly `RecordLength` bytes). V2 has 64-bit file
/// references, V3 128-bit ones (NTFS leaves the high half zero, so the low half is
/// kept). Other versions (V4 range records) yield `None`.
pub(crate) fn parse_record(rec: &[u8]) -> Option<Record> {
    // Offsets of: frn, parent, usn, reason, attributes, name length, name offset.
    let (frn, parent, usn, reason, attrs, name_len, name_off) = match u16_at(rec, 4)? {
        2 => (8, 16, 24, 40, 52, 56, 58),
        3 => (8, 24, 40, 56, 68, 72, 74),
        _ => return None,
    };
    let len = usize::from(u16_at(rec, name_len)?);
    let off = usize::from(u16_at(rec, name_off)?);
    let wide: Vec<u16> = rec
        .get(off..off + len)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Some(Record {
        frn: u64_at(rec, frn)?,
        parent: u64_at(rec, parent)?,
        usn: u64_at(rec, usn)? as i64,
        reason: u32_at(rec, reason)?,
        is_dir: u32_at(rec, attrs)? & FILE_ATTRIBUTE_DIRECTORY != 0,
        name: String::from_utf16_lossy(&wide),
    })
}

/// Splits an ioctl output buffer into its leading 8-byte value (the next start FRN
/// for `FSCTL_ENUM_USN_DATA`, the next USN for `FSCTL_READ_USN_JOURNAL`) and its
/// records. A truncated or too-short record ends the walk.
pub(crate) fn parse_buffer(buf: &[u8]) -> Option<(u64, Vec<Record>)> {
    let next = u64_at(buf, 0)?;
    let mut records = Vec::new();
    let mut at = 8;
    while let Some(len) = u32_at(buf, at) {
        let len = len as usize;
        if len < 8 || at + len > buf.len() {
            break;
        }
        records.extend(parse_record(&buf[at..at + len]));
        at += len;
    }
    Some((next, records))
}

/// FRNs whose rows must be rewritten (`upserts`) or deleted (`removes`) in the db.
#[derive(Default, Debug)]
pub(crate) struct Changes {
    pub upserts: HashSet<u64>,
    pub removes: HashSet<u64>,
}

/// The unprivileged journal read leaves names out. Fills them in: the name already
/// indexed, or `lookup` (open by file id) for entries that are new or renamed.
/// Records whose name stays unknown (gone again, or not visible to this user) are
/// dropped; deletes and old-name records need no name.
pub(crate) fn fill_names(
    records: &mut Vec<Record>,
    index: &Index,
    lookup: impl Fn(u64) -> Option<String>,
) {
    let mut looked_up: HashMap<u64, Option<String>> = HashMap::new();
    records.retain_mut(|r| {
        if !r.name.is_empty() || r.reason & (REASON_FILE_DELETE | REASON_RENAME_OLD_NAME) != 0 {
            return true;
        }
        let renamed = r.reason & REASON_RENAME_NEW_NAME != 0;
        let name = match looked_up.get(&r.frn) {
            Some(name) if !renamed => name.clone(),
            _ => match index.get(r.frn) {
                Some((_, name, _)) if !renamed => Some(name.to_owned()),
                _ => looked_up
                    .entry(r.frn)
                    .insert_entry(lookup(r.frn))
                    .get()
                    .clone(),
            },
        };
        match name {
            Some(name) => {
                r.name = name;
                true
            }
            None => false,
        }
    });
}

/// Applies journal records in order: a delete removes the entry, the old-name half
/// of a rename is skipped (its new-name record follows), anything else (create,
/// new name, data or attribute change) upserts the entry's current name and parent.
pub(crate) fn apply(index: &mut Index, records: &[Record], changes: &mut Changes) {
    for r in records {
        if r.reason & REASON_FILE_DELETE != 0 {
            if index.remove(r.frn) {
                changes.upserts.remove(&r.frn);
                changes.removes.insert(r.frn);
            }
        } else if r.reason & REASON_RENAME_OLD_NAME == 0
            && index.upsert(r.frn, r.parent, &r.name, r.is_dir)
        {
            changes.removes.remove(&r.frn);
            changes.upserts.insert(r.frn);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a record the way NTFS lays it out (`RecordLength` 8-byte aligned).
    pub(crate) fn record_bytes(
        version: u16,
        frn: u64,
        parent: u64,
        reason: u32,
        attrs: u32,
        name: &str,
    ) -> Vec<u8> {
        let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let header = if version == 2 { 60 } else { 76 };
        let len = (header + wide.len()).div_ceil(8) * 8;
        let mut b = vec![0u8; len];
        let mut put = |at: usize, bytes: &[u8]| b[at..at + bytes.len()].copy_from_slice(bytes);
        put(0, &(len as u32).to_le_bytes());
        put(4, &version.to_le_bytes());
        put(8, &frn.to_le_bytes()); // V3: the high half of FILE_ID_128 stays 0
        let (parent_at, usn_at, reason_at, attrs_at, len_at) = if version == 2 {
            (16, 24, 40, 52, 56)
        } else {
            (24, 40, 56, 68, 72)
        };
        put(parent_at, &parent.to_le_bytes());
        put(usn_at, &(frn as i64 * 10).to_le_bytes());
        put(reason_at, &reason.to_le_bytes());
        put(attrs_at, &attrs.to_le_bytes());
        put(len_at, &(wide.len() as u16).to_le_bytes());
        put(len_at + 2, &(header as u16).to_le_bytes());
        put(header, &wide);
        b
    }

    #[test]
    fn parses_v2_and_v3_records_in_one_buffer() {
        let mut buf = 0x1234_u64.to_le_bytes().to_vec();
        buf.extend(record_bytes(
            2,
            0x0005_0000_0000_1234,
            0x0005_0000_0000_0005,
            0x100,
            0x20,
            "Report.PDF",
        ));
        buf.extend(record_bytes(3, 42, 5, 0x2000, 0x10, "Ünïcødé dir"));
        let (next, records) = parse_buffer(&buf).unwrap();
        assert_eq!(next, 0x1234);
        assert_eq!(
            records,
            vec![
                Record {
                    frn: 0x0005_0000_0000_1234,
                    parent: 0x0005_0000_0000_0005,
                    usn: 0x0005_0000_0000_1234_i64 * 10,
                    reason: 0x100,
                    is_dir: false,
                    name: "Report.PDF".into()
                },
                Record {
                    frn: 42,
                    parent: 5,
                    usn: 420,
                    reason: 0x2000,
                    is_dir: true,
                    name: "Ünïcødé dir".into()
                },
            ]
        );
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn captured_v2_record_from_a_live_journal() {
        // FSCTL_READ_USN_JOURNAL on a real C:, 2026-10-09: the create
        // record of %TEMP%\keel-live-a.txt, as returned (96 bytes, 8-aligned).
        let bytes = hex(concat!(
            "60000000020000007cad190000001c004302000000000f00a0998f540f000000",
            "c8b85e7b0858dd01000100000000000000000000200000001e003c006b006500",
            "65006c002d006c006900760065002d0061002e00740078007400000000000000",
        ));
        let r = parse_record(&bytes).unwrap();
        assert_eq!(r.frn, 0x001c_0000_0019_ad7c);
        assert_eq!(r.parent, 0x000f_0000_0000_0243);
        assert_eq!(r.usn, 0x0f_548f_99a0);
        assert_eq!(r.reason, 0x100);
        assert!(!r.is_dir);
        assert_eq!(r.name, "keel-live-a.txt");
    }

    #[test]
    fn unprivileged_records_get_their_names_filled_in() {
        let mut index = Index::new("C:", 5);
        index.upsert(10, 5, "known.txt", false);
        index.upsert(11, 5, "before-rename.txt", false);
        let nameless = |frn, reason| Record {
            frn,
            parent: 5,
            usn: 0,
            reason,
            is_dir: false,
            name: String::new(),
        };
        let mut records = vec![
            nameless(10, 0x1),                    // data change: indexed name
            nameless(11, REASON_RENAME_OLD_NAME), // kept, needs no name
            nameless(11, REASON_RENAME_NEW_NAME), // looked up
            nameless(11, 0x8000_0000),            // close after rename: new name
            nameless(12, 0x100),                  // created: looked up
            nameless(13, 0x100),                  // created and gone: dropped
            nameless(13, REASON_FILE_DELETE),     // kept
        ];
        let calls = std::cell::Cell::new(0);
        fill_names(&mut records, &index, |frn| {
            calls.set(calls.get() + 1);
            match frn {
                11 => Some("after-rename.txt".into()),
                12 => Some("new.txt".into()),
                _ => None,
            }
        });
        let names: Vec<(u64, &str)> = records.iter().map(|r| (r.frn, r.name.as_str())).collect();
        assert_eq!(
            names,
            [
                (10, "known.txt"),
                (11, ""),
                (11, "after-rename.txt"),
                (11, "after-rename.txt"),
                (12, "new.txt"),
                (13, ""),
            ]
        );
        assert_eq!(calls.get(), 3);
        let mut changes = Changes::default();
        apply(&mut index, &records, &mut changes);
        assert_eq!(index.path(11).unwrap(), r"C:\after-rename.txt");
        assert_eq!(index.path(12).unwrap(), r"C:\new.txt");
    }

    #[test]
    fn truncated_and_unknown_records_are_skipped() {
        let mut buf = 9_u64.to_le_bytes().to_vec();
        let mut v4 = record_bytes(2, 1, 5, 0, 0, "x");
        v4[4] = 4;
        buf.extend(v4);
        buf.extend(record_bytes(2, 2, 5, 0, 0, "kept"));
        let mut cut = record_bytes(2, 3, 5, 0, 0, "cut");
        cut.truncate(40);
        buf.extend(cut);
        let (_, records) = parse_buffer(&buf).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "kept");
        assert!(parse_buffer(&[1, 2]).is_none());
    }
}
