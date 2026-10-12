//! Minimal FAT12/16/32, exFAT and NTFS volume image builders for the recovery tests and
//! the fuzz seeds: files written, folders made, entries deleted the way Windows does it.
//! Images grow only as far as something is written (the scanners treat the rest as the
//! end of the volume), so they stay small.

use super::exfat::set_checksum;
use super::fat::lfn_checksum;
use std::collections::HashMap;

fn put(img: &mut Vec<u8>, off: usize, bytes: &[u8]) {
    if img.len() < off + bytes.len() {
        img.resize(off + bytes.len(), 0);
    }
    img[off..off + bytes.len()].copy_from_slice(bytes);
}

fn put16(img: &mut Vec<u8>, off: usize, v: u16) {
    put(img, off, &v.to_le_bytes());
}

fn put32(img: &mut Vec<u8>, off: usize, v: u32) {
    put(img, off, &v.to_le_bytes());
}

fn put64(img: &mut Vec<u8>, off: usize, v: u64) {
    put(img, off, &v.to_le_bytes());
}

fn get16(img: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([img[off], img[off + 1]])
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// 2026-10-12 12:34:56 as a FAT date and time.
pub const FAT_DATE: u16 = ((2026 - 1980) << 9) | (10 << 5) | 12;
pub const FAT_TIME: u16 = (12 << 11) | (34 << 5) | (56 / 2);
/// The same moment, seconds since 1970.
pub const WHEN: i64 = 1_791_808_496;

// --- FAT ---

pub struct FatImg {
    pub img: Vec<u8>,
    pub kind: u8,
    pub cluster: usize,
    fat_off: usize,
    fat_bytes: usize,
    nfats: usize,
    root_off: usize,
    root_ent: usize,
    data_off: usize,
    next: u32,
    fill: HashMap<u32, usize>,
}

/// A folder: its first cluster, 0 for the FAT12/16 fixed root.
#[derive(Clone, Copy, Debug)]
pub struct FatDir(pub u32);

#[derive(Clone, Debug)]
pub struct FatFile {
    /// Byte offsets of its LFN entries and (last) its short entry.
    pub entries: Vec<usize>,
    pub clusters: Vec<u32>,
}

impl FatImg {
    /// A FAT12 (1 MiB), FAT16 (4 MiB) or FAT32 (35 MiB) volume with 512-byte clusters.
    pub fn new(kind: u8) -> Self {
        Self::build(kind, false)
    }

    /// The same with one FAT, and for FAT32 a FAT of 8 sectors only (1,024 entries): what
    /// the fuzz seeds use to stay small.
    pub fn compact(kind: u8) -> Self {
        Self::build(kind, true)
    }

    fn build(kind: u8, compact: bool) -> Self {
        let bps = 512usize;
        let (total, root_ent, fatsz, reserved) = match kind {
            12 => (2048u32, 64usize, 6usize, 1usize),
            16 => (8192, 64, 32, 1),
            _ => (70_000, 0, if compact { 8 } else { 547 }, 32),
        };
        let nfats = if compact { 1 } else { 2 };
        let fat_off = reserved * bps;
        let root_off = fat_off + nfats * fatsz * bps;
        let data_off = root_off + root_ent * 32;
        let mut img = Vec::new();
        put(&mut img, 0, &[0xEB, 0x3C, 0x90]);
        put(&mut img, 3, b"MSWIN4.1");
        put16(&mut img, 11, bps as u16);
        put(&mut img, 13, &[1]);
        put16(&mut img, 14, reserved as u16);
        put(&mut img, 16, &[nfats as u8]);
        put16(&mut img, 17, root_ent as u16);
        put(&mut img, 21, &[0xF8]);
        if kind == 32 {
            put32(&mut img, 32, total);
            put32(&mut img, 36, fatsz as u32);
            put32(&mut img, 44, 2);
        } else {
            put16(&mut img, 19, total as u16);
            put16(&mut img, 22, fatsz as u16);
        }
        put(&mut img, 510, &[0x55, 0xAA]);
        img.resize(data_off, 0);
        let mut fat = Self {
            img,
            kind,
            cluster: bps,
            fat_off,
            fat_bytes: fatsz * bps,
            nfats,
            root_off,
            root_ent,
            data_off,
            next: 2,
            fill: HashMap::new(),
        };
        fat.set_fat(0, 0x0FFF_FFF8);
        fat.set_fat(1, 0x0FFF_FFFF);
        if kind == 32 {
            let root = fat.alloc(1);
            assert_eq!(root, [2]);
        }
        fat
    }

    pub fn root(&self) -> FatDir {
        FatDir(if self.kind == 32 { 2 } else { 0 })
    }

    fn eoc(&self) -> u32 {
        match self.kind {
            12 => 0xFFF,
            16 => 0xFFFF,
            _ => 0x0FFF_FFFF,
        }
    }

    pub fn set_fat(&mut self, n: u32, v: u32) {
        let n = n as usize;
        for f in 0..self.nfats {
            let base = self.fat_off + f * self.fat_bytes;
            match self.kind {
                12 => {
                    let off = base + n + n / 2;
                    let cur = get16(&self.img, off);
                    let v = (v & 0xFFF) as u16;
                    let new = if n & 1 == 1 {
                        (cur & 0x000F) | (v << 4)
                    } else {
                        (cur & 0xF000) | v
                    };
                    put16(&mut self.img, off, new);
                }
                16 => put16(&mut self.img, base + 2 * n, v as u16),
                _ => put32(&mut self.img, base + 4 * n, v & 0x0FFF_FFFF),
            }
        }
    }

    pub fn cluster_off(&self, c: u32) -> usize {
        self.data_off + (c as usize - 2) * self.cluster
    }

    /// `n` contiguous free clusters, chained.
    pub fn alloc(&mut self, n: usize) -> Vec<u32> {
        let first = self.next;
        self.next += n as u32;
        let clusters: Vec<u32> = (first..first + n as u32).collect();
        self.chain(&clusters);
        clusters
    }

    /// Chains `clusters` in the FAT (reusing them if they were freed).
    pub fn chain(&mut self, clusters: &[u32]) {
        for (i, &c) in clusters.iter().enumerate() {
            let next = clusters.get(i + 1).copied().unwrap_or(self.eoc());
            self.set_fat(c, next);
        }
    }

    fn entry_off(&mut self, dir: FatDir, count: usize) -> Vec<usize> {
        let at = self.fill.entry(dir.0).or_insert(0);
        let first = *at;
        *at += count;
        let (base, cap) = match dir.0 {
            0 => (self.root_off, self.root_ent),
            c => (self.cluster_off(c), self.cluster / 32),
        };
        assert!(first + count <= cap, "folder full");
        (first..first + count).map(|i| base + i * 32).collect()
    }

    fn short_entry(&mut self, off: usize, short: &[u8; 11], attr: u8, first: u32, size: u32) {
        put(&mut self.img, off, &[0; 32]);
        put(&mut self.img, off, short);
        self.img[off + 11] = attr;
        put16(&mut self.img, off + 14, FAT_TIME);
        put16(&mut self.img, off + 16, FAT_DATE);
        put16(&mut self.img, off + 18, FAT_DATE);
        put16(&mut self.img, off + 20, (first >> 16) as u16);
        put16(&mut self.img, off + 22, FAT_TIME);
        put16(&mut self.img, off + 24, FAT_DATE);
        put16(&mut self.img, off + 26, first as u16);
        put32(&mut self.img, off + 28, size);
    }

    /// Writes the LFN entries (when `long`) and the short entry; returns their offsets.
    fn entries(
        &mut self,
        dir: FatDir,
        long: Option<&str>,
        short: &[u8; 11],
        attr: u8,
        first: u32,
        size: u32,
    ) -> Vec<usize> {
        let mut units = long.map(utf16).unwrap_or_default();
        let parts = units.len().div_ceil(13);
        if units.len() % 13 != 0 {
            units.push(0);
        }
        units.resize(parts * 13, 0xFFFF);
        let offs = self.entry_off(dir, parts + 1);
        let chk = lfn_checksum(short);
        for (i, &off) in offs[..parts].iter().enumerate() {
            let ord = (parts - i) as u8;
            let part = &units[(ord as usize - 1) * 13..ord as usize * 13];
            let mut e = [0u8; 32];
            e[0] = ord | if i == 0 { 0x40 } else { 0 };
            e[11] = 0x0F;
            e[13] = chk;
            for (k, &u) in part.iter().enumerate() {
                let at = match k {
                    0..5 => 1 + 2 * k,
                    5..11 => 14 + 2 * (k - 5),
                    _ => 28 + 2 * (k - 11),
                };
                e[at..at + 2].copy_from_slice(&u.to_le_bytes());
            }
            put(&mut self.img, off, &e);
        }
        self.short_entry(offs[parts], short, attr, first, size);
        offs
    }

    pub fn add_file(
        &mut self,
        dir: FatDir,
        long: Option<&str>,
        short: &[u8; 11],
        content: &[u8],
    ) -> FatFile {
        let clusters = self.alloc(content.len().div_ceil(self.cluster));
        for (i, chunk) in content.chunks(self.cluster).enumerate() {
            let off = self.cluster_off(clusters[i]);
            put(&mut self.img, off, chunk);
        }
        let first = clusters.first().copied().unwrap_or(0);
        let entries = self.entries(dir, long, short, 0x20, first, content.len() as u32);
        FatFile { entries, clusters }
    }

    pub fn add_dir(&mut self, parent: FatDir, long: Option<&str>, short: &[u8; 11]) -> (FatDir, FatFile) {
        let clusters = self.alloc(1);
        let c = clusters[0];
        let dir = FatDir(c);
        let dot = self.entry_off(dir, 2);
        self.short_entry(dot[0], b".          ", 0x10, c, 0);
        self.short_entry(dot[1], b"..         ", 0x10, parent.0, 0);
        let entries = self.entries(parent, long, short, 0x10, c, 0);
        (dir, FatFile { entries, clusters })
    }

    /// As Windows deletes: `0xE5` on every entry, the chain freed, and on FAT32 the high
    /// half of the first cluster cleared.
    pub fn delete(&mut self, f: &FatFile) {
        for &off in &f.entries {
            self.img[off] = 0xE5;
        }
        if self.kind == 32 {
            let short = *f.entries.last().unwrap();
            put16(&mut self.img, short + 20, 0);
        }
        for &c in &f.clusters {
            self.set_fat(c, 0);
        }
    }

    /// Writes `bytes` over clusters from `first` and marks them used (a new file reusing
    /// a deleted one's space).
    pub fn reuse(&mut self, first: u32, bytes: &[u8]) {
        let n = bytes.len().div_ceil(self.cluster);
        let clusters: Vec<u32> = (first..first + n as u32).collect();
        self.chain(&clusters);
        let off = self.cluster_off(first);
        put(&mut self.img, off, bytes);
    }
}

// --- exFAT ---

/// 4 KiB clusters, 1,000 of them; bitmap in cluster 2, up-case table in 3, root in 4.
pub struct ExImg {
    pub img: Vec<u8>,
    pub cluster: usize,
    bitmap: Vec<u8>,
    next: u32,
    fill: HashMap<u32, usize>,
}

#[derive(Clone, Copy, Debug)]
pub struct ExDir(pub u32);

#[derive(Clone, Debug)]
pub struct ExFile {
    pub set_off: usize,
    pub set_len: usize,
    pub clusters: Vec<u32>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Layout {
    /// One contiguous run, `NoFatChain`.
    Contiguous,
    /// Contiguous but FAT-chained.
    Chained,
    /// FAT-chained in two pieces with a used cluster between them.
    Fragmented,
}

const EX_FAT_OFF: usize = 24 * 512;
const EX_HEAP_OFF: usize = 32 * 512;
const EX_CLUSTERS: u32 = 1000;
/// 2026-10-12 12:34:56 as an exFAT timestamp.
pub const EX_TS: u32 = ((2026 - 1980) << 25) | (10 << 21) | (12 << 16) | (12 << 11) | (34 << 5) | 28;

#[allow(clippy::new_without_default)]
impl ExImg {
    pub fn new() -> Self {
        let mut img = Vec::new();
        put(&mut img, 0, &[0xEB, 0x76, 0x90]);
        put(&mut img, 3, b"EXFAT   ");
        put64(&mut img, 72, 32 + u64::from(EX_CLUSTERS) * 8);
        put32(&mut img, 80, 24);
        put32(&mut img, 84, 8);
        put32(&mut img, 88, 32);
        put32(&mut img, 92, EX_CLUSTERS);
        put32(&mut img, 96, 4);
        put32(&mut img, 100, 0x1234_5678);
        put16(&mut img, 104, 0x0100);
        put(&mut img, 108, &[9]);
        put(&mut img, 109, &[3]);
        put(&mut img, 110, &[1]);
        put(&mut img, 111, &[0x80]);
        put(&mut img, 510, &[0x55, 0xAA]);
        img.resize(EX_HEAP_OFF, 0);
        let mut ex = Self {
            img,
            cluster: 4096,
            bitmap: vec![0; (EX_CLUSTERS as usize).div_ceil(8)],
            next: 5,
            fill: HashMap::new(),
        };
        ex.set_fat(0, 0xFFFF_FFF8);
        ex.set_fat(1, 0xFFFF_FFFF);
        for c in 2..=4 {
            ex.set_fat(c, 0xFFFF_FFFF);
            ex.mark(c, true);
        }
        let root = ex.slots(ExDir(4), 2);
        let mut e = [0u8; 32];
        e[0] = 0x81;
        e[20..24].copy_from_slice(&2u32.to_le_bytes());
        e[24..32].copy_from_slice(&(ex.bitmap.len() as u64).to_le_bytes());
        put(&mut ex.img, root[0], &e);
        let mut e = [0u8; 32];
        e[0] = 0x82;
        e[20..24].copy_from_slice(&3u32.to_le_bytes());
        e[24..32].copy_from_slice(&128u64.to_le_bytes());
        put(&mut ex.img, root[1], &e);
        let off = ex.cluster_off(4) + ex.cluster - 1;
        put(&mut ex.img, off, &[0]);
        ex
    }

    pub fn root(&self) -> ExDir {
        ExDir(4)
    }

    fn set_fat(&mut self, n: u32, v: u32) {
        put32(&mut self.img, EX_FAT_OFF + 4 * n as usize, v);
    }

    fn mark(&mut self, c: u32, used: bool) {
        let i = (c - 2) as usize;
        if used {
            self.bitmap[i / 8] |= 1 << (i % 8);
        } else {
            self.bitmap[i / 8] &= !(1 << (i % 8));
        }
    }

    pub fn cluster_off(&self, c: u32) -> usize {
        EX_HEAP_OFF + (c as usize - 2) * self.cluster
    }

    fn slots(&mut self, dir: ExDir, n: usize) -> Vec<usize> {
        let at = self.fill.entry(dir.0).or_insert(0);
        let first = *at;
        *at += n;
        assert!(first + n <= self.cluster / 32, "folder full");
        let base = self.cluster_off(dir.0);
        (first..first + n).map(|i| base + i * 32).collect()
    }

    fn alloc(&mut self, n: usize, layout: Layout) -> Vec<u32> {
        let mut out = Vec::new();
        for i in 0..n {
            if layout == Layout::Fragmented && i == n / 2 && i > 0 {
                // A cluster of some other file between the two pieces.
                self.mark(self.next, true);
                self.set_fat(self.next, 0xFFFF_FFFF);
                self.next += 1;
            }
            out.push(self.next);
            self.next += 1;
        }
        for &c in &out {
            self.mark(c, true);
        }
        if layout != Layout::Contiguous {
            for (i, &c) in out.iter().enumerate() {
                let next = out.get(i + 1).copied().unwrap_or(0xFFFF_FFFF);
                self.set_fat(c, next);
            }
        }
        out
    }

    fn set(&mut self, dir: ExDir, name: &str, attrs: u16, first: u32, len: u64, nofat: bool) -> (usize, usize) {
        let units = utf16(name);
        let names = units.len().div_ceil(15);
        let mut set = vec![[0u8; 32]; 2 + names];
        set[0][0] = 0x85;
        set[0][1] = (1 + names) as u8;
        set[0][4..6].copy_from_slice(&attrs.to_le_bytes());
        for at in [8, 12, 16] {
            set[0][at..at + 4].copy_from_slice(&EX_TS.to_le_bytes());
        }
        // UTC offsets valid, +0.
        set[0][22] = 0x80;
        set[0][23] = 0x80;
        set[0][24] = 0x80;
        set[1][0] = 0xC0;
        set[1][1] = 0x01 | if nofat { 0x02 } else { 0 };
        set[1][3] = units.len() as u8;
        set[1][8..16].copy_from_slice(&len.to_le_bytes());
        set[1][20..24].copy_from_slice(&first.to_le_bytes());
        set[1][24..32].copy_from_slice(&len.to_le_bytes());
        for (i, part) in units.chunks(15).enumerate() {
            let e = &mut set[2 + i];
            e[0] = 0xC1;
            for (k, u) in part.iter().enumerate() {
                e[2 + 2 * k..4 + 2 * k].copy_from_slice(&u.to_le_bytes());
            }
        }
        let with_offs: Vec<(u64, [u8; 32])> = set.iter().map(|e| (0, *e)).collect();
        let chk = set_checksum(&with_offs);
        set[0][2..4].copy_from_slice(&chk.to_le_bytes());
        let offs = self.slots(dir, set.len());
        for (e, off) in set.iter().zip(&offs) {
            put(&mut self.img, *off, e);
        }
        (offs[0], set.len())
    }

    pub fn add_file(&mut self, dir: ExDir, name: &str, content: &[u8], layout: Layout) -> ExFile {
        let clusters = self.alloc(content.len().div_ceil(self.cluster), layout);
        for (i, chunk) in content.chunks(self.cluster).enumerate() {
            let off = self.cluster_off(clusters[i]);
            put(&mut self.img, off, chunk);
        }
        let first = clusters.first().copied().unwrap_or(0);
        let nofat = layout == Layout::Contiguous;
        let (set_off, set_len) = self.set(dir, name, 0x20, first, content.len() as u64, nofat);
        ExFile {
            set_off,
            set_len,
            clusters,
        }
    }

    pub fn add_dir(&mut self, parent: ExDir, name: &str) -> (ExDir, ExFile) {
        let clusters = self.alloc(1, Layout::Chained);
        let c = clusters[0];
        let off = self.cluster_off(c) + self.cluster - 1;
        put(&mut self.img, off, &[0]);
        let (set_off, set_len) = self.set(parent, name, 0x10, c, self.cluster as u64, false);
        (
            ExDir(c),
            ExFile {
                set_off,
                set_len,
                clusters,
            },
        )
    }

    /// As Windows deletes: the in-use bit cleared on every entry of the set and the
    /// clusters freed in the bitmap (the FAT is left as it was).
    pub fn delete(&mut self, f: &ExFile) {
        for i in 0..f.set_len {
            self.img[f.set_off + i * 32] &= 0x7F;
        }
        for &c in &f.clusters {
            self.mark(c, false);
        }
    }

    pub fn reuse(&mut self, c: u32, bytes: &[u8]) {
        self.mark(c, true);
        let off = self.cluster_off(c);
        put(&mut self.img, off, bytes);
    }

    pub fn image(&self) -> Vec<u8> {
        let mut img = self.img.clone();
        let off = self.cluster_off(2);
        put(&mut img, off, &self.bitmap);
        img
    }
}

// --- NTFS ---

/// 1 KiB clusters (one MFT record each), 8,192 of them; `$Bitmap` at cluster 2, a
/// 32-record `$MFT` at clusters 4..36, data from cluster 40.
pub struct NtfsImg {
    records: Vec<Option<NtRecord>>,
    bitmap: Vec<u8>,
    data: Vec<(usize, Vec<u8>)>,
    next: u64,
}

struct NtRecord {
    seq: u16,
    flags: u16,
    attrs: Vec<Vec<u8>>,
    /// Clusters its data holds.
    clusters: Vec<(u64, u64)>,
}

pub const NT_CLUSTER: usize = 1024;
const NT_MFT_LCN: u64 = 4;
const NT_RECORDS: u64 = 32;
const NT_CLUSTERS: u64 = 8192;
/// 2026-10-12 12:34:56 UTC as a FILETIME.
pub const NT_TIME: u64 = (WHEN as u64 + 11_644_473_600) * 10_000_000;

fn align8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

fn resident_attr(ty: u32, value: &[u8]) -> Vec<u8> {
    let mut a = vec![0u8; align8(24 + value.len())];
    let len = a.len();
    a[0..4].copy_from_slice(&ty.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[10..12].copy_from_slice(&0x18u16.to_le_bytes());
    a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
    a[20..22].copy_from_slice(&0x18u16.to_le_bytes());
    a[24..24 + value.len()].copy_from_slice(value);
    a
}

/// Mapping pairs for `(lcn, clusters)` runs.
pub fn encode_runs(runs: &[(u64, u64)]) -> Vec<u8> {
    fn min_unsigned(v: u64) -> usize {
        (1..=8).find(|&n| n == 8 || v >> (8 * n) == 0).unwrap()
    }
    fn min_signed(v: i64) -> usize {
        (1..=8)
            .find(|&n| n == 8 || (v << (64 - 8 * n)) >> (64 - 8 * n) == v)
            .unwrap()
    }
    let mut out = Vec::new();
    let mut prev = 0i64;
    for &(lcn, len) in runs {
        let delta = lcn as i64 - prev;
        prev = lcn as i64;
        let (ls, os) = (min_unsigned(len), min_signed(delta));
        out.push(((os << 4) | ls) as u8);
        out.extend_from_slice(&len.to_le_bytes()[..ls]);
        out.extend_from_slice(&delta.to_le_bytes()[..os]);
    }
    out.push(0);
    out
}

fn nonresident_attr(ty: u32, runs: &[(u64, u64)], size: u64) -> Vec<u8> {
    let mapping = encode_runs(runs);
    let clusters: u64 = runs.iter().map(|r| r.1).sum();
    let mut a = vec![0u8; align8(64 + mapping.len())];
    let len = a.len();
    a[0..4].copy_from_slice(&ty.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[8] = 1;
    a[10..12].copy_from_slice(&0x40u16.to_le_bytes());
    a[24..32].copy_from_slice(&clusters.saturating_sub(1).to_le_bytes());
    a[32..34].copy_from_slice(&0x40u16.to_le_bytes());
    a[40..48].copy_from_slice(&(clusters * NT_CLUSTER as u64).to_le_bytes());
    a[48..56].copy_from_slice(&size.to_le_bytes());
    a[56..64].copy_from_slice(&size.to_le_bytes());
    a[64..64 + mapping.len()].copy_from_slice(&mapping);
    a
}

fn std_info() -> Vec<u8> {
    let mut v = vec![0u8; 48];
    for at in [0, 8, 16, 24] {
        v[at..at + 8].copy_from_slice(&NT_TIME.to_le_bytes());
    }
    resident_attr(0x10, &v)
}

fn file_name_attr(parent: (u64, u16), name: &str, namespace: u8) -> Vec<u8> {
    let units = utf16(name);
    let mut v = vec![0u8; 66 + 2 * units.len()];
    let r = parent.0 | (u64::from(parent.1) << 48);
    v[0..8].copy_from_slice(&r.to_le_bytes());
    for at in [8, 16, 24, 32] {
        v[at..at + 8].copy_from_slice(&NT_TIME.to_le_bytes());
    }
    v[64] = units.len() as u8;
    v[65] = namespace;
    for (i, u) in units.iter().enumerate() {
        v[66 + 2 * i..68 + 2 * i].copy_from_slice(&u.to_le_bytes());
    }
    resident_attr(0x30, &v)
}

/// A file or folder record: (record number, sequence number).
pub type NtRef = (u64, u16);

pub const NT_ROOT: NtRef = (5, 5);

#[allow(clippy::new_without_default)]
impl NtfsImg {
    pub fn new() -> Self {
        let mut nt = Self {
            records: (0..NT_RECORDS).map(|_| None).collect(),
            bitmap: vec![0; (NT_CLUSTERS / 8) as usize],
            data: Vec::new(),
            next: 40,
        };
        for c in 0..40 {
            nt.mark(c, true);
        }
        nt.records[0] = Some(NtRecord {
            seq: 1,
            flags: 1,
            attrs: vec![
                std_info(),
                file_name_attr(NT_ROOT, "$MFT", 3),
                nonresident_attr(0x80, &[(NT_MFT_LCN, NT_RECORDS)], NT_RECORDS * 1024),
            ],
            clusters: Vec::new(),
        });
        nt.records[5] = Some(NtRecord {
            seq: 5,
            flags: 3,
            attrs: vec![std_info(), file_name_attr(NT_ROOT, ".", 3)],
            clusters: Vec::new(),
        });
        nt.records[6] = Some(NtRecord {
            seq: 6,
            flags: 1,
            attrs: vec![
                std_info(),
                file_name_attr(NT_ROOT, "$Bitmap", 3),
                nonresident_attr(0x80, &[(2, 1)], NT_CLUSTERS / 8),
            ],
            clusters: Vec::new(),
        });
        nt
    }

    fn mark(&mut self, c: u64, used: bool) {
        let i = c as usize;
        if used {
            self.bitmap[i / 8] |= 1 << (i % 8);
        } else {
            self.bitmap[i / 8] &= !(1 << (i % 8));
        }
    }

    fn free_record(&self) -> u64 {
        (16..NT_RECORDS)
            .find(|&n| self.records[n as usize].is_none())
            .expect("MFT full")
    }

    /// A file: resident when `content` is under 600 bytes, else in clusters (in two runs
    /// with a used cluster between them when `fragmented`). Also a DOS name when given.
    pub fn add_file(
        &mut self,
        parent: NtRef,
        name: &str,
        dos: Option<&str>,
        content: &[u8],
        fragmented: bool,
    ) -> NtRef {
        let no = self.free_record();
        let mut attrs = vec![std_info()];
        if let Some(d) = dos {
            attrs.push(file_name_attr(parent, d, 2));
        }
        attrs.push(file_name_attr(parent, name, 1));
        let mut held = Vec::new();
        if content.len() < 600 {
            attrs.push(resident_attr(0x80, content));
        } else {
            let n = content.len().div_ceil(NT_CLUSTER) as u64;
            let mut runs = Vec::new();
            if fragmented && n > 1 {
                runs.push((self.next, n / 2));
                self.mark(self.next + n / 2, true);
                runs.push((self.next + n / 2 + 1, n - n / 2));
                self.next += n + 1;
            } else {
                runs.push((self.next, n));
                self.next += n;
            }
            let mut written = 0;
            for &(lcn, len) in &runs {
                for c in lcn..lcn + len {
                    self.mark(c, true);
                }
                let take = (len as usize * NT_CLUSTER).min(content.len() - written);
                self.data
                    .push((lcn as usize * NT_CLUSTER, content[written..written + take].to_vec()));
                written += take;
            }
            attrs.push(nonresident_attr(0x80, &runs, content.len() as u64));
            held = runs;
        }
        self.records[no as usize] = Some(NtRecord {
            seq: 1,
            flags: 1,
            attrs,
            clusters: held,
        });
        (no, 1)
    }

    pub fn add_dir(&mut self, parent: NtRef, name: &str) -> NtRef {
        let no = self.free_record();
        self.records[no as usize] = Some(NtRecord {
            seq: 1,
            flags: 3,
            attrs: vec![std_info(), file_name_attr(parent, name, 1)],
            clusters: Vec::new(),
        });
        (no, 1)
    }

    /// As NTFS deletes: the in-use flag cleared, the sequence number bumped, the clusters
    /// freed in `$Bitmap`.
    pub fn delete(&mut self, f: NtRef) {
        let rec = self.records[f.0 as usize].as_mut().unwrap();
        rec.flags &= !1;
        rec.seq += 1;
        let clusters = rec.clusters.clone();
        for (lcn, len) in clusters {
            for c in lcn..lcn + len {
                self.mark(c, false);
            }
        }
    }

    /// Writes `bytes` from cluster `lcn` and marks those clusters used.
    pub fn reuse(&mut self, lcn: u64, bytes: &[u8]) {
        for c in lcn..lcn + bytes.len().div_ceil(NT_CLUSTER) as u64 {
            self.mark(c, true);
        }
        self.data.push((lcn as usize * NT_CLUSTER, bytes.to_vec()));
    }

    /// The first cluster of a file's data.
    pub fn first_cluster(&self, f: NtRef) -> u64 {
        self.records[f.0 as usize].as_ref().unwrap().clusters[0].0
    }

    pub fn image(&self) -> Vec<u8> {
        let mut img = Vec::new();
        put(&mut img, 0, &[0xEB, 0x52, 0x90]);
        put(&mut img, 3, b"NTFS    ");
        put16(&mut img, 11, 512);
        put(&mut img, 13, &[2]);
        put(&mut img, 21, &[0xF8]);
        put64(&mut img, 40, NT_CLUSTERS * 2);
        put64(&mut img, 48, NT_MFT_LCN);
        put64(&mut img, 56, NT_MFT_LCN + NT_RECORDS);
        put(&mut img, 64, &[1]);
        put(&mut img, 68, &[1]);
        put64(&mut img, 72, 0x1122_3344_5566_7788);
        put(&mut img, 510, &[0x55, 0xAA]);
        put(&mut img, 2 * NT_CLUSTER, &self.bitmap);
        let mft = NT_MFT_LCN as usize * NT_CLUSTER;
        img.resize(mft + NT_RECORDS as usize * 1024, 0);
        for (no, rec) in self.records.iter().enumerate() {
            if let Some(rec) = rec {
                put(&mut img, mft + no * 1024, &record_bytes(no as u32, rec));
            }
        }
        for (off, bytes) in &self.data {
            put(&mut img, *off, bytes);
        }
        img
    }

    /// The image with record `no` torn: one stride's update sequence number wrong.
    pub fn image_torn(&self, no: u64) -> Vec<u8> {
        let mut img = self.image();
        let at = NT_MFT_LCN as usize * NT_CLUSTER + no as usize * 1024 + 510;
        img[at] ^= 0xFF;
        img
    }
}

fn record_bytes(no: u32, rec: &NtRecord) -> Vec<u8> {
    let mut r = vec![0u8; 1024];
    r[0..4].copy_from_slice(b"FILE");
    r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
    r[6..8].copy_from_slice(&3u16.to_le_bytes());
    r[0x10..0x12].copy_from_slice(&rec.seq.to_le_bytes());
    r[0x12..0x14].copy_from_slice(&1u16.to_le_bytes());
    r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
    r[0x16..0x18].copy_from_slice(&rec.flags.to_le_bytes());
    r[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
    r[0x2C..0x30].copy_from_slice(&no.to_le_bytes());
    let mut off = 0x38;
    for a in &rec.attrs {
        r[off..off + a.len()].copy_from_slice(a);
        off += a.len();
    }
    r[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    let used = (off + 8) as u32;
    r[0x18..0x1C].copy_from_slice(&used.to_le_bytes());
    // Update sequence: number 1; the strides' last two bytes saved in the array.
    let usn = 1u16.to_le_bytes();
    r[0x30..0x32].copy_from_slice(&usn);
    for i in 1..3 {
        let end = i * 512;
        let saved = [r[end - 2], r[end - 1]];
        r[0x30 + 2 * i..0x32 + 2 * i].copy_from_slice(&saved);
        r[end - 2..end].copy_from_slice(&usn);
    }
    r
}

// --- Recycle Bin ---

/// A version 2 `$I` file (Windows 10 and later).
pub fn info_v2(path: &str, size: u64) -> Vec<u8> {
    let units = utf16(path);
    let mut b = Vec::new();
    b.extend_from_slice(&2u64.to_le_bytes());
    b.extend_from_slice(&size.to_le_bytes());
    b.extend_from_slice(&NT_TIME.to_le_bytes());
    b.extend_from_slice(&(units.len() as u32 + 1).to_le_bytes());
    for u in units.iter().chain([&0u16]) {
        b.extend_from_slice(&u.to_le_bytes());
    }
    b
}

/// A version 1 `$I` file (Vista to 8.1).
pub fn info_v1(path: &str, size: u64) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&1u64.to_le_bytes());
    b.extend_from_slice(&size.to_le_bytes());
    b.extend_from_slice(&NT_TIME.to_le_bytes());
    let mut units = utf16(path);
    units.resize(260, 0);
    for u in units {
        b.extend_from_slice(&u.to_le_bytes());
    }
    b
}

// --- fuzz seeds ---

/// (target, file name, bytes) for `fuzz/corpus/`: one small image per file system with
/// live and deleted files, deleted folders and long names, and `$I` files of each version.
pub fn seeds() -> Vec<(&'static str, String, Vec<u8>)> {
    let mut out = Vec::new();
    for kind in [12u8, 16, 32] {
        let mut fat = FatImg::compact(kind);
        let root = fat.root();
        fat.add_file(root, Some("live file.txt"), b"LIVEFI~1TXT", b"live");
        let a = fat.add_file(root, Some("deleted long name.bin"), b"DELETE~1BIN", &[7; 1100]);
        let b = fat.add_file(root, None, b"SHORT   TXT", b"short");
        let (dir, dir_entry) = fat.add_dir(root, Some("Folder"), b"FOLDER     ");
        let c = fat.add_file(dir, Some("inside.txt"), b"INSIDE  TXT", b"inside");
        for f in [&a, &b, &c, &dir_entry] {
            fat.delete(f);
        }
        out.push(("recover_fat", format!("fat{kind}.img"), fat.img));
    }
    let mut ex = ExImg::new();
    let root = ex.root();
    ex.add_file(root, "live.txt", b"live", Layout::Contiguous);
    let a = ex.add_file(root, "contiguous.txt", &[1; 100], Layout::Contiguous);
    let b = ex.add_file(root, "chained.bin", &[2; 4000], Layout::Chained);
    let c = ex.add_file(root, "fragmented.dat", &[3; 8200], Layout::Fragmented);
    let (dir, dir_set) = ex.add_dir(root, "Folder");
    let d = ex.add_file(dir, "a name of more than fifteen characters.txt", &[4; 50], Layout::Contiguous);
    for f in [&a, &b, &c, &d, &dir_set] {
        ex.delete(f);
    }
    out.push(("recover_exfat", "exfat.img".into(), ex.image()));
    let mut nt = NtfsImg::new();
    nt.add_file(NT_ROOT, "live.txt", None, b"live", false);
    let small = nt.add_file(NT_ROOT, "small.txt", Some("SMALL~1.TXT"), b"resident", false);
    let big = nt.add_file(NT_ROOT, "big.bin", None, &[5; 2100], true);
    let dir = nt.add_dir(NT_ROOT, "Folder");
    let inner = nt.add_file(dir, "inner.txt", None, &[6; 700], false);
    for f in [small, big, inner, dir] {
        nt.delete(f);
    }
    out.push(("recover_ntfs", "ntfs.img".into(), nt.image()));
    out.push(("recycle_bin", "v1".into(), info_v1(r"C:\Data\old report.doc", 4096)));
    out.push(("recycle_bin", "v2".into(), info_v2(r"C:\Data\Pictures\photo.jpg", 123_456)));
    out
}
