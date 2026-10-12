//! exFAT: directory entry sets (file, stream extension, file names) whose in-use bits
//! are clear, found by walking every folder from the root, deleted folders included. A
//! set counts when its entries agree (all deleted) and its checksum (computed with the
//! in-use bits set again) matches. The data is contiguous for `NoFatChain` files and
//! follows the FAT otherwise; where the chain ends early the rest is taken as contiguous.
//! The allocation bitmap rates each file.

use super::alloc::Alloc;
use super::{
    civil, read_up_to, u16_at, u32_at, u64_at, utf16_units, Candidate, Confidence, Data, Extent,
    FsKind, List, RecoverError, Result, Scan, MAX_DEPTH, MAX_MAP_BYTES, ROOT_FOLDER,
};
use std::collections::HashSet;
use std::io::{Read, Seek};

/// Most entries read from one folder (exFAT allows 256 MiB of them).
const MAX_DIR_ENTRIES: u64 = 1 << 21;
/// Most fragments one file's data is followed through (more: left out as damaged).
const MAX_RUNS: usize = 1 << 16;

#[derive(Debug)]
pub(crate) struct Geo {
    pub cluster: u64,
    pub fat_off: u64,
    pub fat_len: u64,
    pub heap_off: u64,
    /// Clusters numbered `2..clusters + 2`.
    pub clusters: u64,
    pub root: u64,
    pub total: u64,
}

pub(crate) fn geometry(b: &[u8]) -> Option<Geo> {
    if b.len() < 512 || &b[3..11] != b"EXFAT   " || b[510] != 0x55 || b[511] != 0xAA {
        return None;
    }
    let (bps_shift, spc_shift, nfats) = (u32::from(b[108]), u32::from(b[109]), b[110]);
    if !(9..=12).contains(&bps_shift) || spc_shift > 25 - bps_shift || !(1..=2).contains(&nfats) {
        return None;
    }
    let bps = 1u64 << bps_shift;
    let clusters = u64::from(u32_at(b, 92));
    let root = u64::from(u32_at(b, 96));
    if clusters == 0 || !(2..clusters + 2).contains(&root) {
        return None;
    }
    Some(Geo {
        cluster: 1 << (bps_shift + spc_shift),
        fat_off: u64::from(u32_at(b, 80)) * bps,
        fat_len: u64::from(u32_at(b, 84)) * bps,
        heap_off: u64::from(u32_at(b, 88)) * bps,
        clusters,
        root,
        total: u64_at(b, 72).saturating_mul(bps),
    })
}

/// One directory entry set, decoded.
struct FileSet {
    in_use: bool,
    dir: bool,
    name: String,
    nofat: bool,
    first: u64,
    valid: u64,
    size: u64,
    created: Option<i64>,
    modified: Option<i64>,
    accessed: Option<i64>,
    local: bool,
}

struct Walk<'a, R> {
    r: &'a mut R,
    geo: Geo,
    fat: Vec<u8>,
    alloc: Option<Alloc>,
    seen: HashSet<u64>,
    /// FAT steps left for files' chains (bounds the work a looping FAT can cause).
    budget: u64,
    list: List,
    bad_sets: usize,
    no_data: usize,
}

/// A folder to read: first cluster, length (None: follow the chain), no FAT chain,
/// index in the folder table, depth, deleted.
type Item = (u64, Option<u64>, bool, u32, usize, bool);

pub(crate) fn scan<R: Read + Seek>(r: &mut R, limit: u64) -> Result<Scan> {
    let boot = read_up_to(r, 0, 512)?;
    let geo = geometry(&boot).ok_or(RecoverError::UnknownFs)?;
    let need = ((geo.clusters + 2) * 4).min(geo.fat_len).min(MAX_MAP_BYTES);
    let fat = read_up_to(r, geo.fat_off, need)?;
    let budget = fat.len() as u64 + (1 << 22);
    let mut walk = Walk {
        r,
        geo,
        fat,
        alloc: None,
        seen: HashSet::new(),
        budget,
        list: List::new(limit),
        bad_sets: 0,
        no_data: 0,
    };
    let root = walk.folder_entries(walk.geo.root, None, false);
    if root.is_empty() {
        return Err(RecoverError::Corrupt(
            "exFAT",
            "the root folder could not be read".into(),
        ));
    }
    walk.alloc = walk.bitmap(&root);
    // Live folders first: a deleted folder's clusters may now belong to a live one.
    let (mut live, mut deleted): (Vec<Item>, Vec<Item>) = (Vec::new(), Vec::new());
    walk.folder(&root, ROOT_FOLDER, 0, false, &mut live, &mut deleted);
    while let Some((first, len, nofat, folder, depth, in_deleted)) =
        live.pop().or_else(|| deleted.pop())
    {
        if walk.list.full() {
            break;
        }
        let entries = walk.folder_entries(first, len, nofat);
        walk.folder(&entries, folder, depth, in_deleted, &mut live, &mut deleted);
    }
    let mut notes: Vec<String> = Vec::new();
    if walk.alloc.is_none() {
        notes.push("the allocation bitmap could not be read: confidence unknown".into());
    }
    if walk.bad_sets > 0 {
        notes.push(format!(
            "{} deleted entry sets were partly overwritten and left out",
            walk.bad_sets
        ));
    }
    if walk.no_data > 0 {
        notes.push(format!(
            "{} deleted files point at no valid cluster and were left out",
            walk.no_data
        ));
    }
    let Walk { geo, list, .. } = walk;
    Ok(list.finish(FsKind::Exfat, geo.cluster, geo.total, notes))
}

impl<R: Read + Seek> Walk<'_, R> {
    fn is_cluster(&self, c: u64) -> bool {
        (2..self.geo.clusters + 2).contains(&c)
    }

    fn next(&self, c: u64) -> Option<u64> {
        let off = usize::try_from(c.checked_mul(4)?).ok()?;
        self.fat.get(off..off + 4)?;
        let v = u64::from(u32_at(&self.fat, off));
        self.is_cluster(v).then_some(v)
    }

    fn cluster_off(&self, c: u64) -> u64 {
        self.geo.heap_off + (c - 2) * self.geo.cluster
    }

    /// The cluster runs `(first, count)` holding `len` bytes from `first`: contiguous with
    /// `nofat`, else along the FAT, the rest contiguous where the chain stops early (or the
    /// scan's chain budget runs out). None when they would leave the volume.
    fn runs(&mut self, first: u64, len: u64, nofat: bool) -> Option<Vec<(u64, u64)>> {
        let n = len.div_ceil(self.geo.cluster);
        if n == 0 {
            return Some(Vec::new());
        }
        if !self.is_cluster(first) || n > self.geo.clusters {
            return None;
        }
        let mut runs: Vec<(u64, u64)> = Vec::new();
        let mut c = first;
        let mut have = 0u64;
        let mut visited = HashSet::new();
        loop {
            match runs.last_mut() {
                Some((s, k)) if *s + *k == c => *k += 1,
                _ => runs.push((c, 1)),
            }
            have += 1;
            if have == n {
                break;
            }
            if runs.len() > MAX_RUNS {
                return None;
            }
            let next = match nofat || self.budget == 0 {
                true => None,
                false => {
                    self.budget -= 1;
                    visited.insert(c);
                    self.next(c).filter(|n| !visited.contains(n))
                }
            };
            match next {
                Some(next) => c = next,
                None => {
                    // The rest, contiguous after this cluster.
                    let rest = n - have;
                    if c + 1 + rest > self.geo.clusters + 2 {
                        return None;
                    }
                    match runs.last_mut() {
                        Some((s, k)) if *s + *k == c + 1 => *k += rest,
                        _ => runs.push((c + 1, rest)),
                    }
                    break;
                }
            }
        }
        Some(runs)
    }

    /// The 32-byte entries of a folder with their offsets: `len` bytes (or the whole FAT
    /// chain without one), contiguous with `nofat`. Each cluster is read once over the
    /// whole scan: a loop, or a cluster already read for another folder, ends the folder.
    fn folder_entries(
        &mut self,
        first: u64,
        len: Option<u64>,
        nofat: bool,
    ) -> Vec<(u64, [u8; 32])> {
        let max = (MAX_DIR_ENTRIES * 32).div_ceil(self.geo.cluster);
        let n = len.map_or(max, |l| l.div_ceil(self.geo.cluster).min(max));
        let mut out = Vec::new();
        let mut c = Some(first).filter(|&c| self.is_cluster(c));
        let mut read = 0u64;
        while let Some(cl) = c {
            if read >= n || !self.seen.insert(cl) {
                break;
            }
            read += 1;
            let off = self.cluster_off(cl);
            let Ok(bytes) = read_up_to(self.r, off, self.geo.cluster) else {
                break;
            };
            for (i, e) in bytes.as_chunks::<32>().0.iter().enumerate() {
                out.push((off + i as u64 * 32, *e));
            }
            if (bytes.len() as u64) < self.geo.cluster {
                break;
            }
            c = match nofat {
                true => Some(cl + 1).filter(|&c| self.is_cluster(c)),
                false => self.next(cl),
            };
        }
        out
    }

    /// The allocation bitmap named in the root folder.
    fn bitmap(&mut self, root: &[(u64, [u8; 32])]) -> Option<Alloc> {
        let (_, e) = root.iter().find(|(_, e)| e[0] == 0x81)?;
        let first = u64::from(u32_at(e, 20));
        let len = u64_at(e, 24)
            .min(self.geo.clusters.div_ceil(8))
            .min(MAX_MAP_BYTES);
        let runs = self.runs(first, len, false)?;
        let mut bytes = Vec::new();
        for (s, k) in runs {
            let want = (len - bytes.len() as u64).min(k * self.geo.cluster);
            let chunk = read_up_to(self.r, self.cluster_off(s), want).ok()?;
            let short = (chunk.len() as u64) < want;
            bytes.extend_from_slice(&chunk);
            if short {
                break;
            }
        }
        (!bytes.is_empty()).then(|| Alloc::from_bitmap(&bytes, 2, self.geo.clusters))
    }

    fn folder(
        &mut self,
        entries: &[(u64, [u8; 32])],
        folder: u32,
        depth: usize,
        in_deleted: bool,
        live: &mut Vec<Item>,
        deleted: &mut Vec<Item>,
    ) {
        let mut i = 0;
        while i < entries.len() {
            let (off, e) = &entries[i];
            if e[0] == 0 {
                break;
            }
            if e[0] & 0x7F != 0x05 {
                i += 1;
                continue;
            }
            let count = usize::from(e[1]);
            let Some(set) = entries
                .get(i..=i + count)
                .filter(|_| (2..=18).contains(&count))
            else {
                i += 1;
                continue;
            };
            let Some(f) = file_set(set) else {
                if e[0] & 0x80 == 0 {
                    self.bad_sets += 1;
                }
                i += 1;
                continue;
            };
            i += count + 1;
            let gone = !f.in_use || in_deleted;
            if f.dir {
                // A cluster read already is not read again: no need to name it.
                if depth < MAX_DEPTH && self.is_cluster(f.first) && !self.seen.contains(&f.first) {
                    let Some(child) = self.list.folder(folder, f.name) else {
                        return;
                    };
                    let item = (f.first, Some(f.size), f.nofat, child, depth + 1, gone);
                    match gone {
                        true => deleted.push(item),
                        false => live.push(item),
                    }
                }
                continue;
            }
            if !gone {
                continue;
            }
            let Some(runs) = self.runs(f.first, f.size, f.nofat) else {
                self.no_data += 1;
                continue;
            };
            let confidence = match (&self.alloc, runs.is_empty()) {
                (_, true) => Confidence::Good,
                (Some(a), false) => a.confidence(&runs),
                (None, false) => Confidence::Unknown,
            };
            let extents = runs
                .iter()
                .map(|&(s, k)| Extent {
                    offset: Some(self.cluster_off(s)),
                    len: k * self.geo.cluster,
                })
                .collect();
            let added = self.list.push(Candidate {
                name: f.name,
                folder: Some(folder),
                first_char_unknown: false,
                size: f.size,
                created: f.created,
                modified: f.modified,
                accessed: f.accessed,
                local_times: f.local,
                confidence,
                data: Data::Extents {
                    extents,
                    initialized: f.valid.min(f.size),
                },
                location: *off,
            });
            if !added {
                return;
            }
        }
    }
}

/// The set's checksum, computed with every entry's in-use bit set (as when it was live).
pub(crate) fn set_checksum(set: &[(u64, [u8; 32])]) -> u16 {
    let mut sum: u16 = 0;
    for (n, (_, e)) in set.iter().enumerate() {
        for (i, &b) in e.iter().enumerate() {
            if n == 0 && (i == 2 || i == 3) {
                continue;
            }
            let b = if i == 0 { b | 0x80 } else { b };
            sum = sum.rotate_right(1).wrapping_add(u16::from(b));
        }
    }
    sum
}

/// A file entry set (primary first): every entry deleted or every one in use, a stream
/// extension, enough name entries, a matching checksum.
fn file_set(set: &[(u64, [u8; 32])]) -> Option<FileSet> {
    let (_, p) = &set[0];
    let in_use = p[0] & 0x80 != 0;
    if set.iter().any(|(_, e)| (e[0] & 0x80 != 0) != in_use) {
        return None;
    }
    let (_, s) = set.get(1)?;
    if s[0] & 0x7F != 0x40 {
        return None;
    }
    let name_len = usize::from(s[3]);
    let names = name_len.div_ceil(15);
    if name_len == 0 || set.len() < 2 + names {
        return None;
    }
    let mut units = Vec::with_capacity(names * 15);
    for (_, n) in &set[2..2 + names] {
        if n[0] & 0x7F != 0x41 {
            return None;
        }
        units.extend(utf16_units(&n[2..32]));
    }
    units.truncate(name_len);
    if set_checksum(set) != u16_at(p, 2) {
        return None;
    }
    let attrs = u16_at(p, 4);
    let (created, _) = ex_time(u32_at(p, 8), p[20], p[22]);
    let (modified, utc) = ex_time(u32_at(p, 12), p[21], p[23]);
    let (accessed, _) = ex_time(u32_at(p, 16), 0, p[24]);
    let possible = s[1] & 0x01 != 0;
    Some(FileSet {
        in_use,
        dir: attrs & 0x10 != 0,
        name: String::from_utf16_lossy(&units),
        nofat: s[1] & 0x02 != 0,
        first: u64::from(u32_at(s, 20)),
        valid: u64_at(s, 8),
        size: if possible { u64_at(s, 24) } else { 0 },
        created,
        modified,
        accessed,
        local: !utc,
    })
}

/// An exFAT timestamp: seconds since 1970 and whether it is UTC (a valid offset).
fn ex_time(ts: u32, ms10: u8, offset: u8) -> (Option<i64>, bool) {
    let t = civil(
        1980 + (ts >> 25) as i32,
        (ts >> 21) & 0xF,
        (ts >> 16) & 0x1F,
        (ts >> 11) & 0x1F,
        (ts >> 5) & 0x3F,
        (ts & 0x1F) * 2 + u32::from(ms10.min(199)) / 100,
    );
    if offset & 0x80 == 0 {
        return (t, false);
    }
    // A signed 7-bit count of 15 minutes east of UTC.
    let quarters = i64::from(((offset & 0x7F) as i8) << 1 >> 1);
    (t.map(|t| t - quarters * 15 * 60), true)
}

#[cfg(test)]
mod hostile_tests {
    use super::*;

    /// A walk over a FAT of `clusters` clusters whose entry `c` is `next(c)` (0: none).
    fn walk_over(
        clusters: u32,
        budget: u64,
        next: impl Fn(u32) -> u32,
    ) -> Walk<'static, std::io::Cursor<Vec<u8>>> {
        let mut fat = vec![0u8; (clusters as usize + 2) * 4];
        for c in 2..clusters + 2 {
            fat[c as usize * 4..c as usize * 4 + 4].copy_from_slice(&next(c).to_le_bytes());
        }
        // `runs` never reads the volume; a leaked empty cursor keeps the type simple.
        let reader = Box::leak(Box::new(std::io::Cursor::new(Vec::<u8>::new())));
        Walk {
            r: reader,
            geo: Geo {
                cluster: 512,
                fat_off: 0,
                fat_len: fat.len() as u64,
                heap_off: 0,
                clusters: u64::from(clusters),
                root: 2,
                total: u64::from(clusters) * 512,
            },
            fat,
            alloc: None,
            seen: HashSet::new(),
            budget,
            list: List::new(1 << 20),
            bad_sets: 0,
            no_data: 0,
        }
    }

    /// A contiguous FAT chain is followed one entry at a time and every step is paid from
    /// the scan's budget: once it is spent the rest is taken as contiguous, not walked.
    #[test]
    fn a_contiguous_chain_is_walked_within_the_step_budget() {
        let count = 100_000u32;
        let size = u64::from(count) * 512;
        let mut walk = walk_over(count, 1 << 24, |c| c + 1);
        let before = walk.budget;
        assert_eq!(walk.runs(2, size, false), Some(vec![(2, u64::from(count))]));
        assert_eq!(before - walk.budget, u64::from(count) - 1);
        // Out of budget: no more steps, the same contiguous answer.
        let mut walk = walk_over(count, 10, |c| c + 1);
        assert_eq!(walk.runs(2, size, false), Some(vec![(2, u64::from(count))]));
        assert_eq!(walk.budget, 0);
        // `NoFatChain` never reads the FAT.
        let mut walk = walk_over(count, 10, |c| c + 1);
        assert_eq!(walk.runs(2, size, true), Some(vec![(2, u64::from(count))]));
        assert_eq!(walk.budget, 10);
    }

    /// A chain of every second cluster is as many runs as clusters: past [`MAX_RUNS`] the
    /// file is left out instead of building a list the size of the chain.
    #[test]
    fn a_chain_of_endless_fragments_is_left_out() {
        let count = (MAX_RUNS + 10) as u32;
        let mut walk = walk_over(count * 2 + 2, 1 << 24, |c| match c % 2 {
            0 => c + 2,
            _ => 0,
        });
        assert_eq!(walk.runs(2, u64::from(count) * 512, false), None);
    }

    /// A chain that loops back ends where it repeats; the rest is contiguous.
    #[test]
    fn a_looping_chain_ends() {
        let mut walk = walk_over(1000, 1 << 24, |c| if c == 50 { 2 } else { c + 1 });
        let runs = walk.runs(2, 900 * 512, false).unwrap();
        assert_eq!(runs.iter().map(|r| r.1).sum::<u64>(), 900);
    }
}
