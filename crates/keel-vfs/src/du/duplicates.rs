//! The **Duplicates** tab: files of a scan with the same bytes, found by size, then a hash
//! of the first 64 KiB, then a hash of the whole file (or, for files over 64 MiB with
//! sparse hashing on, of 16 samples spread over the file: "Probable duplicates").
//!
//! Nothing here changes the disk. Reading is careful: cloud placeholders (by the scan's
//! flag and by the live attributes) are never opened, files open without recall
//! (`FILE_FLAG_OPEN_NO_RECALL`) and without following a final link, and a file whose size
//! or modification time differs from the scan's is skipped ("rescan first"). Two names of
//! one file (hard links, or one file reached through a followed link) are never reported
//! as duplicates of each other. [`verify`] reads every file of a group whole again before
//! an action.

use super::analysis::{csv_cell, hex, FileRow};
use super::{attr, flag, NodeKind, ScanHandle, Tree};
use serde::Serialize;
use sha2::Digest as _;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Files above this are sampled, not read whole, when sparse hashing is on.
pub const SPARSE_THRESHOLD: u64 = 64 * 1024 * 1024;
/// The prefix hash and each sample.
const CHUNK: u64 = 64 * 1024;
/// Samples of a sparse hash (first and last chunk included).
const SAMPLES: u64 = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
pub enum Algorithm {
    #[default]
    Blake3,
    /// XXH3-128: fastest, not cryptographic.
    Xxh3,
    Sha256,
}

impl Algorithm {
    pub const ALL: [Algorithm; 3] = [Algorithm::Blake3, Algorithm::Xxh3, Algorithm::Sha256];

    pub fn label(self) -> &'static str {
        match self {
            Algorithm::Blake3 => "BLAKE3",
            Algorithm::Xxh3 => "xxHash3",
            Algorithm::Sha256 => "SHA-256",
        }
    }

    /// Digest bytes.
    fn len(self) -> usize {
        match self {
            Algorithm::Xxh3 => 16,
            Algorithm::Blake3 | Algorithm::Sha256 => 32,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub algorithm: Algorithm,
    /// Sample files over `sparse_above` instead of reading them whole.
    pub sparse: bool,
    /// [`SPARSE_THRESHOLD`] by default (at least 16 × 64 KiB).
    pub sparse_above: u64,
    /// Smaller files are left out (empty files are never duplicates worth listing).
    pub min_size: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            algorithm: Algorithm::Blake3,
            sparse: true,
            sparse_above: SPARSE_THRESHOLD,
            min_size: 1,
        }
    }
}

/// A file's BLAKE3 of its whole content from the library (its `cas_id`), used instead of
/// reading the file when the scan's size and modification time match it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedBlake3 {
    pub bytes: u64,
    pub modified_ns: i64,
    pub digest: [u8; 32],
}

pub type HashCache = HashMap<PathBuf, CachedBlake3>;

/// Files with the same content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Group {
    /// Sorted by path.
    pub files: Vec<FileRow>,
    pub algorithm: Algorithm,
    pub digest: String,
    /// Only samples were compared (a file over 64 MiB with sparse hashing on).
    pub probable: bool,
}

impl Group {
    /// Each file's size.
    pub fn bytes(&self) -> u64 {
        self.files.first().map_or(0, |f| f.bytes)
    }

    /// Bytes freed by keeping one.
    pub fn reclaimable(&self) -> u64 {
        (self.bytes()).saturating_mul(self.files.len().saturating_sub(1) as u64)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Most bytes to win back first.
    pub groups: Vec<Group>,
    /// Files that could not be read (or were left alone), with why.
    pub skipped: Vec<Skipped>,
    pub cancelled: bool,
    /// Files whose library hash was used instead of reading them.
    pub reused_hashes: u64,
}

impl Report {
    pub fn reclaimable(&self) -> u64 {
        self.groups.iter().map(Group::reclaimable).sum()
    }
}

type Key = [u8; 32];

/// Finds the duplicates among the files of `tree`. Progress goes to `handle` (files hashed
/// as `files`, bytes read as `bytes`); a cancel returns the groups found so far.
pub fn find(tree: &Tree, options: &Options, cache: &HashCache, handle: &ScanHandle) -> Report {
    handle.start();
    let mut out = Report::default();
    let mut by_size: BTreeMap<u64, Vec<FileRow>> = BTreeMap::new();
    let mut names = HashSet::new();
    for (id, n) in tree.nodes() {
        if n.kind != NodeKind::File || n.size < options.min_size.max(1) {
            continue;
        }
        // Placeholders, links followed to a file, and the further names of a hard-linked
        // file (the first name stands for it).
        if n.has(flag::CLOUD | flag::FOLLOWED | flag::HARDLINK_DUP)
            || n.attributes & attr::CLOUD != 0
            || tree.link_id(id).is_some_and(|k| !names.insert(k))
        {
            continue;
        }
        if let Some(row) = FileRow::from_tree(tree, id) {
            by_size.entry(n.size).or_default().push(row);
        }
    }
    'sizes: for files in by_size.into_values().rev().filter(|v| v.len() > 1) {
        let mut prefix: BTreeMap<Key, Vec<FileRow>> = BTreeMap::new();
        for file in files {
            if handle.is_cancelled() {
                break 'sizes;
            }
            match hash(&file, options.algorithm, Stage::Prefix, handle) {
                Ok(k) => prefix.entry(k).or_default().push(file),
                Err(e) => out.skip(file.path, e),
            }
        }
        for (k, files) in prefix.into_iter().filter(|(_, v)| v.len() > 1) {
            let size = files[0].bytes;
            if size <= CHUNK {
                // The prefix was the whole file.
                out.push(files, options.algorithm, k, false);
                continue;
            }
            let probable = options.sparse && size > options.sparse_above.max(SAMPLES * CHUNK);
            let stage = if probable { Stage::Sparse } else { Stage::Full };
            let mut full: BTreeMap<Key, Vec<FileRow>> = BTreeMap::new();
            for file in files {
                if handle.is_cancelled() {
                    break 'sizes;
                }
                let cached = (options.algorithm == Algorithm::Blake3 && !probable)
                    .then(|| cache.get(&file.path))
                    .flatten()
                    .filter(|c| c.bytes == file.bytes && Some(c.modified_ns) == file.modified_ns);
                let result = match cached {
                    // Still opened: the checks (placeholder, size, time) apply all the same.
                    Some(c) => open(&file).map(|_| {
                        out.reused_hashes += 1;
                        c.digest
                    }),
                    None => hash(&file, options.algorithm, stage, handle),
                };
                match result {
                    Ok(k) => full.entry(k).or_default().push(file),
                    Err(e) => out.skip(file.path, e),
                }
            }
            for (k, files) in full.into_iter().filter(|(_, v)| v.len() > 1) {
                out.push(files, options.algorithm, k, probable);
            }
        }
    }
    out.groups.sort_by(|a, b| {
        (b.reclaimable().cmp(&a.reclaimable())).then_with(|| a.files[0].path.cmp(&b.files[0].path))
    });
    out.cancelled = handle.is_cancelled();
    handle.finish();
    out
}

impl Report {
    fn skip(&mut self, path: PathBuf, e: anyhow::Error) {
        if e.is::<Cancelled>() {
            return;
        }
        self.skipped.push(Skipped {
            path,
            reason: format!("{e:#}"),
        });
    }

    /// Adds a group, leaving out further names of one file (same volume and file id).
    fn push(&mut self, files: Vec<FileRow>, algorithm: Algorithm, key: Key, probable: bool) {
        let mut ids = HashSet::new();
        let mut files: Vec<FileRow> = (files.into_iter())
            .filter(|f| identity(&f.path).is_none_or(|id| ids.insert(id)))
            .collect();
        if files.len() < 2 {
            return;
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        self.groups.push(Group {
            files,
            algorithm,
            digest: hex(&key[..algorithm.len()]),
            probable,
        });
    }
}

#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// A file's volume and file id.
fn identity(path: &Path) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        crate::ops_windows::file_id(path)
    }
    #[cfg(not(windows))]
    {
        crate::ops_unix::file_id(path)
    }
}

/// The file's own metadata (a final link is not followed), refusing what must not be read:
/// links and, on Windows, placeholders and other reparse points.
fn checked_metadata(path: &Path) -> anyhow::Result<Metadata> {
    let meta = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(!meta.file_type().is_symlink(), "a link: left alone");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        anyhow::ensure!(
            meta.file_attributes() & (attr::CLOUD | attr::REPARSE_POINT) == 0,
            "a cloud placeholder or reparse point: not read"
        );
    }
    anyhow::ensure!(meta.is_file(), "no longer a file");
    Ok(meta)
}

fn modified_ns(meta: &Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).ok(),
        Err(e) => i64::try_from(e.duration().as_nanos()).ok()?.checked_neg(),
    }
}

/// Opens a file of the scan for reading, unchanged since the scan.
fn open(row: &FileRow) -> anyhow::Result<File> {
    let unchanged = |m: &Metadata| m.len() == row.bytes && modified_ns(m) == row.modified_ns;
    let meta = checked_metadata(&row.path)?;
    anyhow::ensure!(
        row.modified_ns.is_some() && unchanged(&meta),
        "changed since the scan: rescan first"
    );
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_NO_RECALL | FILE_FLAG_OPEN_REPARSE_POINT; others may read but not
        // write while it is hashed.
        opts.custom_flags(0x0010_0000 | 0x0020_0000).share_mode(1);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts.open(&row.path)?;
    let opened = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        anyhow::ensure!(
            opened.file_attributes() & (attr::CLOUD | attr::REPARSE_POINT) == 0,
            "a cloud placeholder or reparse point: not read"
        );
    }
    anyhow::ensure!(opened.is_file() && unchanged(&opened), "changed while opening");
    Ok(file)
}

enum Hasher {
    Blake3(Box<blake3::Hasher>),
    Xxh3(Box<twox_hash::XxHash3_128>),
    Sha256(sha2::Sha256),
}

impl Hasher {
    fn new(a: Algorithm) -> Self {
        match a {
            Algorithm::Blake3 => Hasher::Blake3(Box::default()),
            Algorithm::Xxh3 => Hasher::Xxh3(Box::new(twox_hash::XxHash3_128::new())),
            Algorithm::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Blake3(h) => {
                h.update(data);
            }
            Hasher::Xxh3(h) => h.write(data),
            Hasher::Sha256(h) => h.update(data),
        }
    }

    fn finish(self) -> Key {
        let mut key = [0; 32];
        match self {
            Hasher::Blake3(h) => key = *h.finalize().as_bytes(),
            Hasher::Xxh3(h) => key[..16].copy_from_slice(&h.finish_128().to_be_bytes()),
            Hasher::Sha256(h) => key.copy_from_slice(&h.finalize()),
        }
        key
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// The first 64 KiB.
    Prefix,
    /// 16 samples of 64 KiB spread evenly, first and last included.
    Sparse,
    Full,
}

/// The byte ranges a stage reads of a file of `size` bytes.
fn ranges(stage: Stage, size: u64) -> Vec<(u64, u64)> {
    match stage {
        Stage::Prefix => vec![(0, size.min(CHUNK))],
        Stage::Full => vec![(0, size)],
        Stage::Sparse if size <= SAMPLES * CHUNK => vec![(0, size)],
        // Over 16 chunks: the samples are more than a chunk apart and never overlap.
        Stage::Sparse => {
            let last = u128::from(size - CHUNK);
            (0..SAMPLES)
                .map(|i| ((last * u128::from(i) / u128::from(SAMPLES - 1)) as u64, CHUNK))
                .collect()
        }
    }
}

fn hash(row: &FileRow, algorithm: Algorithm, stage: Stage, handle: &ScanHandle) -> anyhow::Result<Key> {
    if !handle.checkpoint() {
        return Err(Cancelled.into());
    }
    handle.at(&row.path);
    let mut file = open(row)?;
    let mut h = Hasher::new(algorithm);
    // The size is part of a sample hash, as the library's sampled hash does.
    if stage == Stage::Sparse {
        h.update(&row.bytes.to_le_bytes());
    }
    let mut buf = vec![0u8; CHUNK as usize];
    for (at, mut left) in ranges(stage, row.bytes) {
        file.seek(SeekFrom::Start(at))?;
        while left > 0 {
            if !handle.checkpoint() {
                return Err(Cancelled.into());
            }
            let n = left.min(CHUNK) as usize;
            file.read_exact(&mut buf[..n])?;
            h.update(&buf[..n]);
            handle.add(0, 0, n as u64);
            left -= n as u64;
        }
    }
    let after = file.metadata()?;
    anyhow::ensure!(
        after.len() == row.bytes && modified_ns(&after) == row.modified_ns,
        "changed while it was read"
    );
    handle.add(1, 0, 0);
    Ok(h.finish())
}

/// Before **Keep this one** / **Replace with hard links**: reads every file of `group`
/// whole again (sampled and library-hashed groups included) and checks they still hold
/// the same bytes, are distinct files, and (for `hard_links`) sit on the kept file's
/// volume. Returns the paths of the other files, for the caller's preview and
/// confirmation (nothing is changed here).
pub fn verify(
    group: &Group,
    keep: usize,
    hard_links: bool,
    handle: &ScanHandle,
) -> anyhow::Result<Vec<PathBuf>> {
    anyhow::ensure!(group.files.len() > 1, "the group has one file left");
    let kept = group.files.get(keep).ok_or_else(|| anyhow::anyhow!("choose a file to keep"))?;
    let kept_id = identity(&kept.path)
        .ok_or_else(|| anyhow::anyhow!("{}: cannot be identified", kept.path.display()))?;
    let digest = hash(kept, Algorithm::Blake3, Stage::Full, handle)?;
    let mut ids = HashSet::from([kept_id]);
    let mut others = Vec::new();
    for (i, f) in group.files.iter().enumerate() {
        if i == keep {
            continue;
        }
        let id = identity(&f.path)
            .ok_or_else(|| anyhow::anyhow!("{}: cannot be identified", f.path.display()))?;
        anyhow::ensure!(
            ids.insert(id),
            "{} is another name of a file already in the group",
            f.path.display()
        );
        anyhow::ensure!(
            f.bytes == kept.bytes && hash(f, Algorithm::Blake3, Stage::Full, handle)? == digest,
            "{} no longer has the same bytes",
            f.path.display()
        );
        if hard_links {
            anyhow::ensure!(
                id.0 == kept_id.0,
                "{} is on another volume: a hard link cannot reach it",
                f.path.display()
            );
        }
        others.push(f.path.clone());
    }
    Ok(others)
}

/// The report as CSV: one line per file.
pub fn write_csv(report: &Report, mut w: impl Write) -> std::io::Result<()> {
    write!(w, "group,status,algorithm,digest,path,extension,bytes,allocated\r\n")?;
    for (i, g) in report.groups.iter().enumerate() {
        let status = if g.probable {
            "Probable duplicates"
        } else {
            "Duplicates"
        };
        for f in &g.files {
            write!(
                w,
                "{},{},{},{},{},{},{},{}\r\n",
                i + 1,
                status,
                g.algorithm.label(),
                g.digest,
                csv_cell(&f.path.to_string_lossy()),
                csv_cell(&f.extension),
                f.bytes,
                f.allocated
            )?;
        }
    }
    Ok(())
}

/// The report as JSON.
pub fn write_json(report: &Report, w: impl Write) -> serde_json::Result<()> {
    serde_json::to_writer_pretty(w, report)
}

#[cfg(test)]
pub(crate) fn sparse_ranges(size: u64) -> Vec<(u64, u64)> {
    ranges(Stage::Sparse, size)
}
