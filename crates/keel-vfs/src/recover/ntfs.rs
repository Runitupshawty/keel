//! NTFS: every `$MFT` record (read through the `$MFT`'s own data runs) whose in-use flag
//! is clear and that still has a `$FILE_NAME` and an unnamed `$DATA`: resident data comes
//! from the record itself, non-resident data from its runs, rated against `$Bitmap`. The
//! folder path is rebuilt from the parent references of the folder records (live or
//! deleted, the sequence number checked). Compressed and encrypted files, and files whose
//! runs continue in other records (`$ATTRIBUTE_LIST`), are counted but not listed.

use super::alloc::Alloc;
use super::{
    filetime, read_at, read_up_to, u16_at, u32_at, u64_at, Candidate,
    Confidence, Data, Extent, FsKind, RecoverError, Result, Scan, MAX_CANDIDATES, MAX_DEPTH,
};
use std::collections::HashMap;
use std::io::{Read, Seek};

/// The root folder's record.
const ROOT: u64 = 5;
/// `$Bitmap`'s record.
const BITMAP: u64 = 6;

#[derive(Debug)]
pub(crate) struct Geo {
    pub cluster: u64,
    pub record: u64,
    pub mft_lcn: u64,
    pub clusters: u64,
    pub total: u64,
}

pub(crate) fn geometry(b: &[u8]) -> Option<Geo> {
    if b.len() < 512 || &b[3..11] != b"NTFS    " || b[510] != 0x55 || b[511] != 0xAA {
        return None;
    }
    let bps = u64::from(u16_at(b, 11));
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return None;
    }
    let spc = match b[13] {
        0 => return None,
        n @ 1..=0x80 if n.is_power_of_two() => u64::from(n),
        n @ 0x81.. if 256 - u32::from(n) <= 12 => 1 << (256 - u32::from(n)),
        _ => return None,
    };
    let cluster = bps * spc;
    let record = match b[64] as i8 {
        n @ 1.. => u64::from(n as u8).checked_mul(cluster)?,
        n @ -16..=-8 => 1 << -i32::from(n),
        _ => return None,
    };
    if !record.is_power_of_two() || !(256..=65536).contains(&record) || cluster > 2 << 20 {
        return None;
    }
    let sectors = u64_at(b, 40);
    let clusters = sectors / spc;
    let mft_lcn = u64_at(b, 48);
    if mft_lcn >= clusters {
        return None;
    }
    Some(Geo {
        cluster,
        record,
        mft_lcn,
        clusters,
        total: sectors.saturating_mul(bps),
    })
}

/// Applies the update sequence array: the last two bytes of each 512-byte stride must hold
/// the sequence number and get their saved bytes back. False for a torn or bad record.
pub(crate) fn fixup(rec: &mut [u8]) -> bool {
    let usa = usize::from(u16_at(rec, 4));
    let count = usize::from(u16_at(rec, 6));
    if count < 2 || usa + count * 2 > rec.len() || (count - 1) * 512 > rec.len() {
        return false;
    }
    let usn = [rec[usa], rec[usa + 1]];
    for i in 1..count {
        let end = i * 512;
        if rec[end - 2..end] != usn {
            return false;
        }
        rec[end - 2] = rec[usa + 2 * i];
        rec[end - 1] = rec[usa + 2 * i + 1];
    }
    true
}

#[derive(Clone, Debug)]
enum DataAttr {
    Resident(Vec<u8>),
    NonResident {
        runs: Vec<(Option<u64>, u64)>,
        size: u64,
        initialized: u64,
        flags: u16,
    },
}

struct FileName {
    parent: u64,
    parent_seq: u16,
    name: String,
    namespace: u8,
    times: [Option<i64>; 3],
}

#[derive(Default)]
struct Record {
    in_use: bool,
    dir: bool,
    seq: u16,
    base: u64,
    names: Vec<FileName>,
    /// `$STANDARD_INFORMATION`: created, modified, accessed.
    times: Option<[Option<i64>; 3]>,
    data: Option<DataAttr>,
    attr_list: bool,
}

impl Record {
    /// The Win32 or POSIX name over the DOS 8.3 one.
    fn name(&self) -> Option<&FileName> {
        self.names
            .iter()
            .find(|n| n.namespace != 2)
            .or(self.names.first())
    }
}

fn resident(a: &[u8]) -> Option<&[u8]> {
    let len = u32_at(a, 16) as usize;
    let off = usize::from(u16_at(a, 20));
    a.get(off..off.checked_add(len)?)
}

fn parse_record(rec: &[u8]) -> Option<Record> {
    if rec.get(..4)? != b"FILE" {
        return None;
    }
    let flags = u16_at(rec, 0x16);
    let mut out = Record {
        in_use: flags & 1 != 0,
        dir: flags & 2 != 0,
        seq: u16_at(rec, 0x10),
        base: u64_at(rec, 0x20) & 0xFFFF_FFFF_FFFF,
        ..Record::default()
    };
    let used = (u32_at(rec, 0x18) as usize).min(rec.len());
    let mut off = usize::from(u16_at(rec, 0x14));
    while off + 16 <= used {
        let a = &rec[off..used];
        let ty = u32_at(a, 0);
        let len = u32_at(a, 4) as usize;
        if ty == 0xFFFF_FFFF || len < 16 || len > a.len() {
            break;
        }
        let a = &a[..len];
        let nonresident = a[8] != 0;
        match ty {
            0x10 if !nonresident => {
                if let Some(v) = resident(a).filter(|v| v.len() >= 32) {
                    out.times = Some([
                        filetime(u64_at(v, 0)),
                        filetime(u64_at(v, 8)),
                        filetime(u64_at(v, 24)),
                    ]);
                }
            }
            0x20 => out.attr_list = true,
            0x30 if !nonresident => {
                if let Some(n) = resident(a).and_then(file_name) {
                    out.names.push(n);
                }
            }
            0x80 if a[9] == 0 && out.data.is_none() => out.data = data_attr(a, nonresident),
            _ => {}
        }
        off += len;
    }
    Some(out)
}

fn file_name(v: &[u8]) -> Option<FileName> {
    let n = usize::from(*v.get(64)?);
    let raw = v.get(66..66 + 2 * n)?;
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let name = String::from_utf16_lossy(&units);
    if name.is_empty() {
        return None;
    }
    let parent = u64_at(v, 0);
    Some(FileName {
        parent: parent & 0xFFFF_FFFF_FFFF,
        parent_seq: (parent >> 48) as u16,
        name,
        namespace: v[65],
        times: [
            filetime(u64_at(v, 8)),
            filetime(u64_at(v, 16)),
            filetime(u64_at(v, 32)),
        ],
    })
}

fn data_attr(a: &[u8], nonresident: bool) -> Option<DataAttr> {
    if !nonresident {
        return resident(a).map(|v| DataAttr::Resident(v.to_vec()));
    }
    // A piece starting past VCN 0 continues data held elsewhere.
    if a.len() < 64 || u64_at(a, 16) != 0 {
        return None;
    }
    let runs = decode_runs(a.get(usize::from(u16_at(a, 32))..)?)?;
    Some(DataAttr::NonResident {
        runs,
        size: u64_at(a, 48),
        initialized: u64_at(a, 56),
        flags: u16_at(a, 12),
    })
}

/// A mapping pairs array: (LCN, None for sparse; length in clusters).
pub(crate) fn decode_runs(b: &[u8]) -> Option<Vec<(Option<u64>, u64)>> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut lcn: i64 = 0;
    while let Some(&h) = b.get(pos) {
        if h == 0 {
            break;
        }
        let (ls, os) = (usize::from(h & 0xF), usize::from(h >> 4));
        if ls == 0 || ls > 8 || os > 8 {
            return None;
        }
        let len = le(b.get(pos + 1..pos + 1 + ls)?);
        if len == 0 {
            return None;
        }
        if os == 0 {
            out.push((None, len));
        } else {
            let raw = le(b.get(pos + 1 + ls..pos + 1 + ls + os)?);
            let shift = 64 - 8 * os as u32;
            let delta = ((raw << shift) as i64) >> shift;
            lcn = lcn.checked_add(delta)?;
            out.push((Some(u64::try_from(lcn).ok()?), len));
        }
        pos += 1 + ls + os;
    }
    Some(out)
}

fn le(b: &[u8]) -> u64 {
    b.iter().rev().fold(0, |v, &x| (v << 8) | u64::from(x))
}

/// Whether two allocated runs share a cluster.
fn overlap(runs: &[(Option<u64>, u64)]) -> bool {
    let mut real: Vec<(u64, u64)> = runs
        .iter()
        .filter_map(|&(lcn, len)| lcn.map(|l| (l, len)))
        .collect();
    real.sort_unstable();
    real.windows(2)
        .any(|w| w[0].0.saturating_add(w[0].1) > w[1].0)
}

struct Dir {
    name: String,
    parent: u64,
    parent_seq: u16,
    seq: u16,
    in_use: bool,
}

struct Found {
    candidate: Candidate,
    parent: u64,
    parent_seq: u16,
    /// Allocated cluster runs holding the data (for the confidence).
    clusters: Vec<(u64, u64)>,
}

#[derive(Default)]
struct Counts {
    compressed: usize,
    elsewhere: usize,
    no_data: usize,
}

pub(crate) fn scan<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    let boot = read_up_to(r, 0, 512)?;
    let geo = geometry(&boot).ok_or(RecoverError::UnknownFs)?;
    let corrupt = |why: &str| RecoverError::Corrupt("NTFS", why.into());
    let mut rec = vec![0u8; geo.record as usize];
    let mft_off = geo
        .mft_lcn
        .checked_mul(geo.cluster)
        .ok_or_else(|| corrupt("bad $MFT location"))?;
    read_at(r, mft_off, &mut rec).map_err(|_| corrupt("the $MFT could not be read"))?;
    let mft = match fixup(&mut rec).then(|| parse_record(&rec)).flatten() {
        Some(Record {
            data: Some(DataAttr::NonResident { runs, .. }),
            ..
        }) if !overlap(&runs) => runs,
        _ => return Err(corrupt("the $MFT record is damaged")),
    };
    let mut dirs: HashMap<u64, Dir> = HashMap::new();
    let mut found: Vec<Found> = Vec::new();
    let mut bitmap: Option<DataAttr> = None;
    let mut counts = Counts::default();
    let mut recno = 0u64;
    for (lcn, len) in mft {
        let bytes = len.saturating_mul(geo.cluster);
        let in_run = bytes / geo.record;
        let Some(start) = lcn.and_then(|l| l.checked_mul(geo.cluster)) else {
            recno = recno.saturating_add(in_run);
            continue;
        };
        for i in 0..in_run {
            if found.len() >= MAX_CANDIDATES {
                break;
            }
            let Some(off) = start.checked_add(i * geo.record) else {
                break;
            };
            // The end of the volume (or a bad stretch) ends this run.
            if read_at(r, off, &mut rec).is_err() {
                recno = recno.saturating_add(in_run - i);
                break;
            }
            let no = recno;
            recno += 1;
            if !fixup(&mut rec) {
                continue;
            }
            let Some(record) = parse_record(&rec) else {
                continue;
            };
            if no == BITMAP && record.in_use {
                bitmap = record.data.clone();
            }
            if record.base != 0 {
                continue;
            }
            if record.dir {
                if let Some(n) = record.name() {
                    dirs.insert(
                        no,
                        Dir {
                            name: n.name.clone(),
                            parent: n.parent,
                            parent_seq: n.parent_seq,
                            seq: record.seq,
                            in_use: record.in_use,
                        },
                    );
                }
                continue;
            }
            if record.in_use || record.names.is_empty() {
                continue;
            }
            match deleted_file(&geo, no, record, &mut counts) {
                Some(f) => found.push(f),
                None => continue,
            }
        }
    }
    let alloc = bitmap.and_then(|b| read_bitmap(r, &geo, &b));
    let candidates = found
        .into_iter()
        .map(|mut f| {
            f.candidate.folder = folder_path(&dirs, f.parent, f.parent_seq);
            if !f.clusters.is_empty() {
                f.candidate.confidence = match &alloc {
                    Some(a) => a.confidence(&f.clusters),
                    None => Confidence::Unknown,
                };
            }
            f.candidate
        })
        .collect::<Vec<_>>();
    let mut notes: Vec<String> = Vec::new();
    if alloc.is_none() {
        notes.push("$Bitmap could not be read: confidence unknown".into());
    }
    if counts.compressed > 0 {
        notes.push(format!(
            "{} deleted compressed or encrypted files cannot be recovered and were left out",
            counts.compressed
        ));
    }
    if counts.elsewhere > 0 {
        notes.push(format!(
            "{} deleted files keep their data runs in other records and were left out",
            counts.elsewhere
        ));
    }
    if counts.no_data > 0 {
        notes.push(format!(
            "{} deleted records have damaged data runs and were left out",
            counts.no_data
        ));
    }
    if candidates.len() >= MAX_CANDIDATES {
        notes.push(format!("the list stops at {MAX_CANDIDATES} files"));
    }
    Ok(Scan {
        fs: FsKind::Ntfs,
        cluster_size: geo.cluster,
        volume_size: geo.total,
        candidates,
        notes,
    })
}

fn deleted_file(geo: &Geo, no: u64, record: Record, counts: &mut Counts) -> Option<Found> {
    let name = record.name()?;
    let (parent, parent_seq) = (name.parent, name.parent_seq);
    let [created, modified, accessed] = record.times.unwrap_or(name.times);
    let name = name.name.clone();
    let Some(data) = record.data else {
        if record.attr_list {
            counts.elsewhere += 1;
        }
        return None;
    };
    let (size, data, clusters) = match data {
        DataAttr::Resident(bytes) => (bytes.len() as u64, Data::Resident(bytes), Vec::new()),
        DataAttr::NonResident {
            runs,
            size,
            initialized,
            flags,
        } => {
            if flags & 0x4001 != 0 {
                counts.compressed += 1;
                return None;
            }
            match extents(geo, &runs, size) {
                Some((extents, clusters)) => (
                    size,
                    Data::Extents {
                        extents,
                        initialized: initialized.min(size),
                    },
                    clusters,
                ),
                None => {
                    counts.no_data += 1;
                    return None;
                }
            }
        }
    };
    Some(Found {
        candidate: Candidate {
            name,
            folder: None,
            first_char_unknown: false,
            size,
            created,
            modified,
            accessed,
            local_times: false,
            confidence: Confidence::Good,
            data,
            location: no,
        },
        parent,
        parent_seq,
        clusters,
    })
}

/// The byte extents covering `size` and the allocated cluster runs among them; None when
/// a run leaves the volume or the runs are shorter than the size.
fn extents(
    geo: &Geo,
    runs: &[(Option<u64>, u64)],
    size: u64,
) -> Option<(Vec<Extent>, Vec<(u64, u64)>)> {
    let mut need = size.div_ceil(geo.cluster);
    let mut extents = Vec::new();
    let mut clusters = Vec::new();
    for &(lcn, len) in runs {
        if need == 0 {
            break;
        }
        let take = len.min(need);
        need -= take;
        let bytes = take.checked_mul(geo.cluster)?;
        match lcn {
            Some(l) => {
                if l.checked_add(take)? > geo.clusters {
                    return None;
                }
                extents.push(Extent {
                    offset: Some(l * geo.cluster),
                    len: bytes,
                });
                clusters.push((l, take));
            }
            None => extents.push(Extent {
                offset: None,
                len: bytes,
            }),
        }
    }
    (need == 0).then_some((extents, clusters))
}

/// `$Bitmap`'s content, at most one bit per cluster.
fn read_bitmap<R: Read + Seek>(r: &mut R, geo: &Geo, attr: &DataAttr) -> Option<Alloc> {
    let want = geo.clusters.div_ceil(8);
    let bytes = match attr {
        DataAttr::Resident(b) => b.clone(),
        DataAttr::NonResident { runs, .. } => {
            if overlap(runs) {
                return None;
            }
            let mut bytes = Vec::new();
            for &(lcn, len) in runs {
                let left = want - bytes.len() as u64;
                if left == 0 {
                    break;
                }
                let n = len.saturating_mul(geo.cluster).min(left);
                let chunk = read_up_to(r, lcn?.checked_mul(geo.cluster)?, n).ok()?;
                let short = (chunk.len() as u64) < n;
                bytes.extend_from_slice(&chunk);
                if short {
                    break;
                }
            }
            bytes
        }
    };
    (!bytes.is_empty()).then(|| Alloc::from_bitmap(&bytes, 0, geo.clusters))
}

/// The folder path of a parent reference: names up to the root, each folder record's
/// sequence number matching the reference (one more for a deleted folder, which NTFS
/// bumps on delete).
fn folder_path(dirs: &HashMap<u64, Dir>, parent: u64, seq: u16) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let (mut cur, mut want) = (parent, seq);
    for _ in 0..MAX_DEPTH {
        if cur == ROOT {
            parts.reverse();
            return Some(parts.join("/"));
        }
        let d = dirs.get(&cur)?;
        let same = want == 0 || d.seq == want || (!d.in_use && d.seq == want.wrapping_add(1));
        if !same {
            return None;
        }
        parts.push(&d.name);
        cur = d.parent;
        want = d.parent_seq;
    }
    None
}
