//! FAT12/16/32: deleted directory entries (first byte `0xE5`) found by walking every
//! folder from the root, deleted folders included (their first cluster). A long name
//! (LFN entries just before the short entry, checksum consistent) gives the full name;
//! without one the short name's first character is unknown and shown as `_`. Deleting a
//! file frees its cluster chain, so the data is taken to be the clusters following its
//! first one (the contiguous heuristic) and rated by how many of them the FAT now marks
//! used. Windows clears the high 16 bits of a FAT32 file's first cluster on delete, so
//! files past cluster 65,535 there point at the wrong place (rated by the clusters found).

use super::alloc::Alloc;
use super::{
    civil, read_up_to, u16_at, u32_at, Candidate, Confidence, Data, Extent, FsKind, RecoverError,
    Result, Scan, MAX_CANDIDATES, MAX_DEPTH,
};
use std::collections::HashSet;
use std::io::{Read, Seek};

/// Most entries read from one folder (FAT allows 65,536).
const MAX_DIR_ENTRIES: u64 = 1 << 20;

#[derive(Debug)]
pub(crate) struct Geo {
    pub kind: FsKind,
    pub cluster: u64,
    pub fat_off: u64,
    pub fat_len: u64,
    pub root_off: u64,
    pub root_len: u64,
    pub data_off: u64,
    /// Data clusters, numbered `2..clusters + 2`.
    pub clusters: u64,
    pub root_cluster: u64,
    pub total: u64,
}

/// The BIOS parameter block of a FAT boot sector, checked.
pub(crate) fn geometry(b: &[u8]) -> Option<Geo> {
    if b.len() < 512 || b[510] != 0x55 || b[511] != 0xAA || !matches!(b[0], 0xEB | 0xE9) {
        return None;
    }
    let bps = u64::from(u16_at(b, 11));
    let spc = u64::from(b[13]);
    let reserved = u64::from(u16_at(b, 14));
    let nfats = u64::from(b[16]);
    let root_ent = u64::from(u16_at(b, 17));
    let total16 = u64::from(u16_at(b, 19));
    let fatsz16 = u64::from(u16_at(b, 22));
    let total32 = u64::from(u32_at(b, 32));
    let fatsz32 = u64::from(u32_at(b, 36));
    if !matches!(bps, 512 | 1024 | 2048 | 4096)
        || spc == 0
        || !spc.is_power_of_two()
        || spc * bps > 256 << 10
        || reserved == 0
        || !(1..=4).contains(&nfats)
    {
        return None;
    }
    let fatsz = if fatsz16 != 0 { fatsz16 } else { fatsz32 };
    let total = if total16 != 0 { total16 } else { total32 };
    let root_secs = (root_ent * 32).div_ceil(bps);
    let first_data = reserved + nfats * fatsz + root_secs;
    if fatsz == 0 || total <= first_data {
        return None;
    }
    let clusters = (total - first_data) / spc;
    let kind = match clusters {
        0 => return None,
        1..4085 => FsKind::Fat12,
        4085..65525 => FsKind::Fat16,
        _ => FsKind::Fat32,
    };
    match kind {
        FsKind::Fat32 if root_ent != 0 || fatsz16 != 0 => return None,
        FsKind::Fat12 | FsKind::Fat16 if root_ent == 0 => return None,
        _ => {}
    }
    Some(Geo {
        kind,
        cluster: spc * bps,
        fat_off: reserved * bps,
        fat_len: fatsz * bps,
        root_off: (reserved + nfats * fatsz) * bps,
        root_len: root_ent * 32,
        data_off: first_data * bps,
        clusters,
        root_cluster: match kind {
            FsKind::Fat32 => u64::from(u32_at(b, 44)),
            _ => 0,
        },
        total: total * bps,
    })
}

/// The first FAT, as far as the volume holds it.
struct Table {
    kind: FsKind,
    bytes: Vec<u8>,
    clusters: u64,
}

impl Table {
    fn get(&self, n: u64) -> Option<u32> {
        let n = usize::try_from(n).ok()?;
        match self.kind {
            FsKind::Fat12 => {
                let off = n.checked_add(n / 2)?;
                self.bytes.get(off..off.checked_add(2)?)?;
                let v = u32::from(u16_at(&self.bytes, off));
                Some(if n & 1 == 1 { v >> 4 } else { v & 0xFFF })
            }
            FsKind::Fat16 => {
                let off = n.checked_mul(2)?;
                self.bytes.get(off..off.checked_add(2)?)?;
                Some(u32::from(u16_at(&self.bytes, off)))
            }
            _ => {
                let off = n.checked_mul(4)?;
                self.bytes.get(off..off.checked_add(4)?)?;
                Some(u32_at(&self.bytes, off) & 0x0FFF_FFFF)
            }
        }
    }

    /// Entries fully read.
    fn entries(&self) -> u64 {
        let len = self.bytes.len() as u64;
        match self.kind {
            FsKind::Fat12 => len * 2 / 3,
            FsKind::Fat16 => len / 2,
            _ => len / 4,
        }
    }

    fn is_cluster(&self, c: u64) -> bool {
        (2..self.clusters + 2).contains(&c)
    }

    /// The chain from `start`, stopping at the end mark, a free or bad entry, `max`
    /// clusters or a cluster in `seen` (which it adds to).
    fn chain(&self, start: u64, max: u64, seen: &mut HashSet<u64>) -> Vec<u64> {
        let mut out = Vec::new();
        let mut c = start;
        while self.is_cluster(c) && (out.len() as u64) < max && seen.insert(c) {
            out.push(c);
            match self.get(c) {
                Some(v) => c = u64::from(v),
                None => break,
            }
        }
        out
    }
}

enum DirLoc {
    /// FAT12/16's fixed root folder.
    Root,
    /// A live folder's cluster chain.
    Chain(u64),
    /// A deleted folder's first cluster (its chain is gone).
    First(u64),
}

struct Walk<'a, R> {
    r: &'a mut R,
    geo: Geo,
    fat: Table,
    alloc: Alloc,
    /// Folder clusters read (each once).
    seen: HashSet<u64>,
    out: Vec<Candidate>,
    no_data: usize,
}

pub(crate) fn scan<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    let boot = read_up_to(r, 0, 512)?;
    let geo = geometry(&boot).ok_or(RecoverError::UnknownFs)?;
    let need = match geo.kind {
        FsKind::Fat12 => (geo.clusters + 2) * 3 / 2 + 1,
        FsKind::Fat16 => (geo.clusters + 2) * 2,
        _ => (geo.clusters + 2) * 4,
    }
    .min(geo.fat_len);
    let fat = Table {
        kind: geo.kind,
        bytes: read_up_to(r, geo.fat_off, need)?,
        clusters: geo.clusters,
    };
    if fat.entries() < 3 {
        return Err(RecoverError::Corrupt(
            geo.kind.label(),
            "the FAT could not be read".into(),
        ));
    }
    let bits = geo.clusters.min(fat.entries().saturating_sub(2));
    let alloc = Alloc::from_fn(bits, 2, |i| fat.get(i + 2).is_some_and(|v| v != 0));
    let root = match geo.kind {
        FsKind::Fat32 => DirLoc::Chain(geo.root_cluster),
        _ => DirLoc::Root,
    };
    let mut walk = Walk {
        r,
        geo,
        fat,
        alloc,
        seen: HashSet::new(),
        out: Vec::new(),
        no_data: 0,
    };
    // Live folders first: a deleted folder's cluster may now belong to a live one.
    let mut stack: Stack = (vec![(root, String::new(), 0, false)], Vec::new());
    while let Some((loc, path, depth, in_deleted)) = stack.0.pop().or_else(|| stack.1.pop()) {
        if walk.out.len() >= MAX_CANDIDATES {
            break;
        }
        let entries = walk.entries(&loc);
        if let DirLoc::First(_) = loc {
            // A deleted folder's cluster starts with its "." entry, unless reused.
            if entries
                .first()
                .is_none_or(|(_, e)| &e[..11] != b".          ")
            {
                continue;
            }
        }
        walk.folder(&entries, &path, depth, in_deleted, &mut stack);
    }
    let mut notes: Vec<String> = Vec::new();
    if walk.no_data > 0 {
        notes.push(format!(
            "{} deleted entries point at no valid cluster and were left out",
            walk.no_data
        ));
    }
    if walk.out.len() >= MAX_CANDIDATES {
        notes.push(format!("the list stops at {MAX_CANDIDATES} files"));
    }
    Ok(Scan {
        fs: walk.geo.kind,
        cluster_size: walk.geo.cluster,
        volume_size: walk.geo.total,
        candidates: walk.out,
        notes,
    })
}

/// Folders to read (location, path, depth, inside a deleted folder): live, deleted.
type Stack = (Vec<Folder>, Vec<Folder>);
type Folder = (DirLoc, String, usize, bool);

impl<R: Read + Seek> Walk<'_, R> {
    fn cluster_off(&self, c: u64) -> u64 {
        self.geo.data_off + (c - 2) * self.geo.cluster
    }

    /// The folder's 32-byte entries with their byte offsets on the volume.
    fn entries(&mut self, loc: &DirLoc) -> Vec<(u64, [u8; 32])> {
        let max_clusters = (MAX_DIR_ENTRIES * 32).div_ceil(self.geo.cluster);
        let chunks: Vec<(u64, u64)> = match *loc {
            DirLoc::Root => vec![(self.geo.root_off, self.geo.root_len)],
            DirLoc::Chain(c) => self
                .fat
                .chain(c, max_clusters, &mut self.seen)
                .into_iter()
                .map(|c| (self.cluster_off(c), self.geo.cluster))
                .collect(),
            DirLoc::First(c) => match self.seen.insert(c) {
                true => vec![(self.cluster_off(c), self.geo.cluster)],
                false => Vec::new(),
            },
        };
        let mut out = Vec::new();
        for (off, len) in chunks {
            let Ok(bytes) = read_up_to(self.r, off, len) else {
                break;
            };
            for (i, e) in bytes.chunks_exact(32).enumerate() {
                if out.len() as u64 >= MAX_DIR_ENTRIES {
                    return out;
                }
                let mut entry = [0u8; 32];
                entry.copy_from_slice(e);
                out.push((off + i as u64 * 32, entry));
            }
            if (bytes.len() as u64) < len {
                break;
            }
        }
        out
    }

    fn folder(
        &mut self,
        entries: &[(u64, [u8; 32])],
        path: &str,
        depth: usize,
        in_deleted: bool,
        stack: &mut Stack,
    ) {
        let mut lfn: Vec<[u8; 32]> = Vec::new();
        for (off, e) in entries {
            if e[0] == 0 {
                break;
            }
            let attr = e[11];
            if attr & 0x3F == 0x0F {
                // At most 20 LFN entries make a name (255 characters).
                if lfn.len() >= 20 {
                    lfn.clear();
                }
                lfn.push(*e);
                continue;
            }
            let lfns = std::mem::take(&mut lfn);
            if attr & 0x08 != 0 || is_dot(e) {
                continue;
            }
            let erased = e[0] == 0xE5;
            let deleted = erased || in_deleted;
            let (name, unknown) = name(e, &lfns, erased);
            let lo = u64::from(u16_at(e, 26));
            let hi = match self.geo.kind {
                FsKind::Fat32 => u64::from(u16_at(e, 20)),
                _ => 0,
            };
            let start = (hi << 16) | lo;
            if attr & 0x10 != 0 {
                if self.fat.is_cluster(start) && depth < MAX_DEPTH {
                    let child = join(path, &name);
                    match deleted {
                        true => stack.1.push((DirLoc::First(start), child, depth + 1, true)),
                        false => stack
                            .0
                            .push((DirLoc::Chain(start), child, depth + 1, false)),
                    }
                }
                continue;
            }
            if !deleted {
                continue;
            }
            if self.out.len() >= MAX_CANDIDATES {
                return;
            }
            let size = u64::from(u32_at(e, 28));
            let n = size.div_ceil(self.geo.cluster);
            if n > 0 && (!self.fat.is_cluster(start) || start + n > self.geo.clusters + 2) {
                self.no_data += 1;
                continue;
            }
            let extents = match n {
                0 => Vec::new(),
                n => vec![Extent {
                    offset: Some(self.cluster_off(start)),
                    len: n * self.geo.cluster,
                }],
            };
            let confidence = match n {
                0 => Confidence::Good,
                n => self.alloc.confidence(&[(start, n)]),
            };
            self.out.push(Candidate {
                name,
                folder: Some(path.to_owned()),
                first_char_unknown: unknown,
                size,
                created: fat_time(u16_at(e, 16), u16_at(e, 14)),
                modified: fat_time(u16_at(e, 24), u16_at(e, 22)),
                accessed: fat_time(u16_at(e, 18), 0),
                local_times: true,
                confidence,
                data: Data::Extents {
                    extents,
                    initialized: size,
                },
                location: *off,
            });
        }
    }
}

fn join(path: &str, name: &str) -> String {
    match path {
        "" => name.to_owned(),
        p => format!("{p}/{name}"),
    }
}

fn is_dot(e: &[u8; 32]) -> bool {
    &e[..11] == b".          " || &e[..11] == b"..         "
}

/// The checksum of a short name that its LFN entries carry.
pub(crate) fn lfn_checksum(name11: &[u8]) -> u8 {
    name11.iter().fold(0u8, |s, &b| {
        ((s & 1) << 7).wrapping_add(s >> 1).wrapping_add(b)
    })
}

/// A character Windows puts in a short name.
fn short_char(b: u8) -> bool {
    b >= 0x80 || (b > 0x20 && !b"\"*+,./:;<=>?[\\]|".contains(&b) && !b.is_ascii_lowercase())
}

/// The long name of `lfns` (physical order, last part first) if it belongs to the short
/// entry `e`: same checksum in every part, matching the short name (for an erased one, a
/// first character that makes it match must be a valid short name character).
fn long_name(e: &[u8; 32], lfns: &[[u8; 32]], erased: bool) -> Option<String> {
    let chk = lfns.first()?[13];
    if lfns.iter().any(|l| l[13] != chk) {
        return None;
    }
    if erased {
        let mut n = [0u8; 11];
        n.copy_from_slice(&e[..11]);
        let first = (0..=255u8).find(|&b| {
            n[0] = b;
            lfn_checksum(&n) == chk
        })?;
        if !short_char(first) || first == 0xE5 {
            return None;
        }
    } else {
        let count = lfns.len() as u8;
        let ordered = lfns[0][0] & 0x40 != 0
            && lfns
                .iter()
                .enumerate()
                .all(|(i, l)| l[0] & 0x1F == count - i as u8);
        if !ordered || lfn_checksum(&e[..11]) != chk {
            return None;
        }
    }
    let mut units = Vec::with_capacity(lfns.len() * 13);
    for l in lfns.iter().rev() {
        for range in [1..11, 14..26, 28..32] {
            units.extend(
                l[range]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]])),
            );
        }
    }
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    units.truncate(end);
    while units.last() == Some(&0xFFFF) {
        units.pop();
    }
    let name = String::from_utf16_lossy(&units);
    (!name.is_empty()).then_some(name)
}

/// The entry's name and whether its first character is unknown.
fn name(e: &[u8; 32], lfns: &[[u8; 32]], erased: bool) -> (String, bool) {
    if let Some(long) = long_name(e, lfns, erased) {
        return (long, false);
    }
    let latin1 = |b: &[u8]| -> String { b.iter().map(|&c| char::from(c)).collect() };
    let mut base = e[..8].to_vec();
    if erased {
        base[0] = b'_';
    } else if base[0] == 0x05 {
        base[0] = 0xE5;
    }
    let mut base = latin1(base.trim_ascii_end());
    let mut ext = latin1(e[8..11].trim_ascii_end());
    if e[12] & 0x08 != 0 {
        base = base.to_lowercase();
    }
    if e[12] & 0x10 != 0 {
        ext = ext.to_lowercase();
    }
    let name = match ext.is_empty() {
        true => base,
        false => format!("{base}.{ext}"),
    };
    (name, erased)
}

/// A FAT date and time (local wall clock), or None for no date.
fn fat_time(date: u16, time: u16) -> Option<i64> {
    if date == 0 {
        return None;
    }
    civil(
        1980 + i32::from(date >> 9),
        u32::from((date >> 5) & 0xF),
        u32::from(date & 0x1F),
        u32::from(time >> 11),
        u32::from((time >> 5) & 0x3F),
        u32::from(time & 0x1F) * 2,
    )
}
