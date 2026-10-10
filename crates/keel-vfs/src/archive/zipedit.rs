//! Changing a zip (or jar): entries deleted, renamed, copied or added, by writing a new
//! archive beside the old one (`<name>.keel-partial-<pid>-<n>`), fsyncing it and renaming it
//! over the old one, so a cancel, an error or a crash leaves the original as it was.
//!
//! Entries that stay are copied byte for byte (local header, data and data descriptor, never
//! decompressed): stored entries stay stored, encrypted ones are never decrypted, extra
//! fields and comments survive. Only a renamed entry's name and every entry's offset in the
//! central directory change. Zip64 records are kept, and written wherever a size, an offset
//! or the entry count needs them. New files are deflated.

use super::safe_name;
use crate::ops::{partial_name, Conflict, Journal, Placed, Progress, Stamp};
use crate::VPath;
use anyhow::{bail, ensure, Context, Result};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::SystemTime,
};

/// Opens a new file's bytes when they are written.
pub type Open<'a> = Box<dyn FnOnce() -> Result<Box<dyn Read + Send>> + 'a>;

/// One change, by normalised entry name (`safe_name`). Delete, Rename and Copy apply to
/// the entry of that name and everything under it (a folder); the most specific name wins.
pub enum Edit<'a> {
    Delete(String),
    Rename {
        from: String,
        to: String,
    },
    /// A raw copy (no recompression) of `from` named `to`; `from` stays.
    Copy {
        from: String,
        to: String,
    },
    /// Drops only the entry named exactly so (a folder entry whose files move elsewhere).
    DropExact(String),
    /// A folder entry `name/`, unless that folder exists (explicitly or by its entries).
    Mkdir(String),
    /// A new file, replacing any entry of that name.
    Add {
        name: String,
        size: u64,
        modified: Option<SystemTime>,
        open: Open<'a>,
    },
}

const MAX32: u64 = 0xFFFF_FFFF;
const LOCAL_SIG: u32 = 0x0403_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const END_SIG: u32 = 0x0605_4b50;
const END64_SIG: u32 = 0x0606_4b50;
const LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_FIELD: u16 = 0x0001;
/// Info-ZIP Unicode Path: overrides the name on read, so it goes when a name changes.
const UNICODE_PATH: u16 = 0x7075;
/// New files from this size on get zip64 sizes (deflate may grow incompressible data).
const ZIP64_FROM: u64 = 0xF000_0000;

/// Why `path` (an archive entry or folder) cannot be changed, or its archive file and its
/// normalised name inside it (empty for the root). Decided by name: the bytes are checked
/// when the archive is rewritten.
pub fn editable(path: &VPath) -> Result<(PathBuf, String)> {
    let (outer, inner) = path.split_archive().context("not inside an archive")?;
    let name = outer.name().to_owned();
    if outer.split_archive().is_some() {
        bail!("{name} is inside another archive: archives inside archives are read-only (extract it to change it)");
    }
    let lower = name.to_ascii_lowercase();
    let format = if lower.ends_with(".zip") || lower.ends_with(".jar") {
        None
    } else if lower.ends_with(".7z") {
        Some("7z")
    } else if lower.ends_with(".rar") {
        Some("RAR")
    } else {
        Some("tar")
    };
    if let Some(format) = format {
        bail!("{format} archives are read-only: only entries of zip and jar archives can be changed ({name})");
    }
    let local = outer.to_local_path().with_context(|| {
        format!("{name} is not on this computer: only local zip archives can be changed (copy it here first)")
    })?;
    let inner = if inner.trim_matches('/').is_empty() {
        String::new()
    } else {
        safe_name(&inner)?
    };
    Ok((local, inner))
}

/// `name` is `root` or inside it.
pub(crate) fn under(name: &str, root: &str) -> bool {
    root.is_empty()
        || name
            .strip_prefix(root)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `a/b` under `dir` (`""`: the root).
pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() || name.is_empty() {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The folder holding `name` (`""` at the root).
pub(crate) fn parent(name: &str) -> &str {
    name.rsplit_once('/').map_or("", |(p, _)| p)
}

/// The normalised names in a zip: files, and folders (explicit or implied by deeper names).
#[derive(Default)]
pub struct Names {
    pub files: HashSet<String>,
    pub dirs: HashSet<String>,
    /// Every normalised entry name (files and explicit folders) with whether it is a folder.
    pub entries: Vec<(String, bool)>,
}
impl Names {
    pub fn read(zip: &Path) -> Result<Self> {
        let mut archive = ::zip::ZipArchive::new(io::BufReader::new(File::open(zip)?))?;
        let mut names = Names::default();
        for i in 0..archive.len() {
            let file = archive.by_index_raw(i)?;
            let Ok(name) = safe_name(file.name()) else {
                continue;
            };
            names.add(&name, file.is_dir());
        }
        Ok(names)
    }
    fn add(&mut self, name: &str, is_dir: bool) {
        let mut up = parent(name);
        while !up.is_empty() && self.dirs.insert(up.to_owned()) {
            up = parent(up);
        }
        if is_dir {
            self.dirs.insert(name.to_owned());
        } else {
            self.files.insert(name.to_owned());
        }
        self.entries.push((name.to_owned(), is_dir));
    }
    pub fn exists(&self, name: &str) -> bool {
        name.is_empty() || self.files.contains(name) || self.dirs.contains(name)
    }
    /// Entry names under `root` (itself included).
    pub fn under<'a>(&'a self, root: &'a str) -> impl Iterator<Item = &'a (String, bool)> + 'a {
        self.entries.iter().filter(move |(n, _)| under(n, root))
    }
}

/// A rewritten archive waiting beside the original; dropped without `commit`, it is removed.
pub struct Staged {
    partial: Option<PathBuf>,
    target: PathBuf,
}
impl Staged {
    /// Puts the new archive in place of the old one (one rename).
    pub fn commit(mut self) -> Result<()> {
        let partial = self.partial.take().context("already committed")?;
        if let Err(e) = fs::rename(&partial, &self.target) {
            let _ = fs::remove_file(&partial);
            return Err(e).with_context(|| format!("replace {}", self.target.display()));
        }
        Ok(())
    }
    /// The staged file (tests simulate a crash before the rename with it).
    pub fn path(&self) -> Option<&Path> {
        self.partial.as_deref()
    }
}
impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(p) = self.partial.take() {
            let _ = fs::remove_file(p);
        }
    }
}

/// `stage` then `commit`.
pub fn edit(
    zip: &Path,
    edits: Vec<Edit<'_>>,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<()> {
    stage(zip, edits, progress, cancel)?.commit()
}

/// One entry of the old archive.
struct Old {
    name: Option<String>,
    is_dir: bool,
    header: u64,
    central: u64,
    size: u64,
    csize: u64,
}

enum Out<'a> {
    Keep {
        old: usize,
        /// The new normalised name; None keeps the raw name.
        name: Option<String>,
    },
    Dir(String),
    File {
        name: String,
        size: u64,
        modified: Option<SystemTime>,
        open: Open<'a>,
    },
}
impl Out<'_> {
    /// The normalised name, None for an unsafe name kept as it is.
    fn name<'s>(&'s self, olds: &'s [Old]) -> Option<&'s str> {
        match self {
            Out::Keep { name: Some(n), .. } | Out::Dir(n) | Out::File { name: n, .. } => Some(n),
            Out::Keep { old, name: None } => olds[*old].name.as_deref(),
        }
    }
}

enum Act {
    Delete,
    Rename(String),
    Copy(String),
    DropExact,
}

/// Writes the changed archive beside `zip` and returns it, not yet in place. Every Delete,
/// Rename, Copy and DropExact must find its entry ("not in the archive" otherwise), and no
/// two entries may end up with one name.
pub fn stage(
    zip: &Path,
    edits: Vec<Edit<'_>>,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<Staged> {
    check(cancel)?;
    let zip = crate::long(zip)?;
    let meta = fs::metadata(&zip).with_context(|| format!("open {}", zip.display()))?;
    ensure!(meta.is_file(), "not an archive file: {}", zip.display());
    let mut source = File::open(&zip)?;
    let mut magic = [0u8; 4];
    let n = source.read(&mut magic)?;
    ensure!(
        n == 4 && (magic == *b"PK\x03\x04" || magic == *b"PK\x05\x06"),
        "{} is not a zip archive",
        zip.display()
    );
    let (olds, comment, wide_end) = {
        let mut archive = ::zip::ZipArchive::new(io::BufReader::new(File::open(&zip)?))
            .with_context(|| format!("read {}", zip.display()))?;
        let mut olds = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let file = archive.by_index_raw(i)?;
            olds.push(Old {
                name: safe_name(file.name()).ok(),
                is_dir: file.is_dir(),
                header: file.header_start(),
                central: file.central_header_start(),
                size: file.size(),
                csize: file.compressed_size(),
            });
        }
        let comment = archive.comment().to_vec();
        (olds, comment, has_zip64_end(&mut source)?)
    };

    // Keys are matched against every name and its parents: the longest match wins.
    let mut acts: HashMap<String, Vec<Act>> = HashMap::new();
    let mut adds = Vec::new();
    for edit in edits {
        let (key, act) = match edit {
            Edit::Delete(n) => (n, Act::Delete),
            Edit::Rename { from, to } => (from, Act::Rename(to)),
            Edit::Copy { from, to } => (from, Act::Copy(to)),
            Edit::DropExact(n) => (n, Act::DropExact),
            other => {
                adds.push(other);
                continue;
            }
        };
        ensure!(
            !key.is_empty(),
            "the archive root cannot be changed this way"
        );
        acts.entry(key).or_default().push(act);
    }
    let mut used: HashSet<&str> = HashSet::new();
    let mut outs: Vec<Out> = Vec::with_capacity(olds.len() + adds.len());
    for (i, old) in olds.iter().enumerate() {
        let Some(name) = old.name.as_deref() else {
            outs.push(Out::Keep { old: i, name: None });
            continue;
        };
        let mut key = name;
        let found = loop {
            if let Some((k, list)) = acts.get_key_value(key) {
                break Some((k.as_str(), list));
            }
            if key.is_empty() {
                break None;
            }
            key = parent(key);
        };
        let Some((key, list)) = found else {
            outs.push(Out::Keep { old: i, name: None });
            continue;
        };
        let rest = &name[key.len()..];
        let exact = rest.is_empty();
        let mut kept = true;
        let mut renamed = None;
        for act in list {
            match act {
                Act::Delete => kept = false,
                Act::DropExact if exact => kept = false,
                Act::DropExact => continue,
                Act::Rename(to) => renamed = Some(format!("{to}{rest}")),
                Act::Copy(to) => outs.push(Out::Keep {
                    old: i,
                    name: Some(format!("{to}{rest}")),
                }),
            }
            used.insert(key);
        }
        if kept {
            outs.push(Out::Keep {
                old: i,
                name: renamed,
            });
        }
    }
    if let Some(missing) = acts.keys().find(|k| !used.contains(k.as_str())) {
        bail!("not in the archive: {missing}");
    }
    for add in adds {
        match add {
            Edit::Mkdir(name) => outs.push(Out::Dir(name)),
            Edit::Add {
                name,
                size,
                modified,
                open,
            } => {
                // Replaces a file of that name (never a folder: that clash is refused below).
                outs.retain(|o| {
                    o.name(&olds) != Some(name.as_str())
                        || matches!(o, Out::Keep { old, .. } if olds[*old].is_dir)
                });
                outs.push(Out::File {
                    name,
                    size,
                    modified,
                    open,
                });
            }
            _ => unreachable!(),
        }
    }
    // A folder entry for a folder that exists anyway is left out.
    let mut present = HashSet::new();
    for o in &outs {
        if let (Some(n), false) = (o.name(&olds), matches!(o, Out::Dir(_))) {
            let mut up = n;
            while !up.is_empty() && present.insert(up.to_owned()) {
                up = parent(up);
            }
        }
    }
    let mut made = HashSet::new();
    outs.retain(|o| match o {
        Out::Dir(n) => !present.contains(n) && made.insert(n.clone()),
        _ => true,
    });
    // No two files with one name, and no file where a folder is, among what changed (an
    // archive that already holds such names is left as it is).
    let mut files: HashMap<&str, (u32, bool)> = HashMap::new();
    let (mut folders, mut new_folders) = (HashSet::new(), HashSet::new());
    for o in &outs {
        let Some(n) = o.name(&olds) else { continue };
        let changed = !matches!(o, Out::Keep { name: None, .. });
        let is_dir = match o {
            Out::Keep { old, .. } => olds[*old].is_dir,
            Out::Dir(_) => true,
            Out::File { .. } => false,
        };
        if is_dir {
            folders.insert(n);
            if changed {
                new_folders.insert(n);
            }
        } else {
            let seen = files.entry(n).or_default();
            seen.0 += 1;
            seen.1 |= changed;
        }
        let mut up = parent(n);
        while !up.is_empty() {
            folders.insert(up);
            if changed {
                new_folders.insert(up);
            }
            up = parent(up);
        }
    }
    for (name, (count, changed)) in &files {
        ensure!(
            !(*changed && *count > 1),
            "the archive would hold two entries named {name}"
        );
        ensure!(
            !(*changed && folders.contains(name) || new_folders.contains(name)),
            "the archive would hold a file and a folder named {name}"
        );
    }

    // Each kept entry's bytes run from its local header to the next entry (or the central
    // directory): that covers its data descriptor too.
    let mut starts: Vec<u64> = olds.iter().map(|o| o.header).collect();
    starts.sort_unstable();
    let directory = olds.iter().map(|o| o.central).min().unwrap_or(0);
    let file_len = meta.len();
    let region_end = |header: u64| -> u64 {
        let next = starts.partition_point(|s| *s <= header);
        starts.get(next).copied().unwrap_or(directory)
    };
    let mut total = starts.first().copied().unwrap_or(0);
    for o in &outs {
        total += match o {
            Out::Keep { old, .. } => {
                region_end(olds[*old].header).saturating_sub(olds[*old].header)
            }
            Out::File { size, .. } => *size,
            Out::Dir(_) => 0,
        };
    }
    let mut state = Progress {
        total_bytes: total,
        total_items: outs.len(),
        ..Progress::default()
    };
    progress(state.clone());

    let folder = zip.parent().context("archive has no parent folder")?;
    crate::ops::sweep_local(folder);
    let leaf = zip.file_name().context("archive has no name")?;
    let staged_path = folder.join(partial_name(&leaf.to_string_lossy()));
    let file = fs::OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .open(&staged_path)
        .with_context(|| format!("create {}", staged_path.display()))?;
    let staged = Staged {
        partial: Some(staged_path),
        target: zip.clone(),
    };
    // Keeps the old archive's mode bits.
    file.set_permissions(meta.permissions())?;
    let mut out = Writer {
        inner: BufWriter::with_capacity(1 << 20, file),
        pos: 0,
    };
    let mut buffer = vec![0u8; 1 << 20];
    let mut copy = |out: &mut Writer, from: u64, len: u64, state: &mut Progress| -> Result<()> {
        ensure!(from + len <= file_len, "the archive is truncated");
        source.seek(SeekFrom::Start(from))?;
        let mut left = len;
        while left > 0 {
            check(cancel)?;
            let n = buffer
                .len()
                .min(usize::try_from(left).unwrap_or(usize::MAX));
            source.read_exact(&mut buffer[..n])?;
            out.write_all(&buffer[..n])?;
            left -= n as u64;
            state.done_bytes += n as u64;
            progress(state.clone());
        }
        Ok(())
    };
    // Whatever precedes the first entry (a self-extractor's stub) stays.
    if let Some(first) = starts.first() {
        copy(&mut out, 0, *first, &mut state)?;
    }
    let mut directory_records = Vec::with_capacity(outs.len());
    let mut reader = File::open(&zip)?;
    for o in outs {
        check(cancel)?;
        let offset = out.pos;
        match o {
            Out::Keep { old, name } => {
                let old = &olds[old];
                let end = region_end(old.header);
                let head = read_at(&mut reader, old.header, 30)?;
                ensure!(
                    le32(&head, 0) == LOCAL_SIG,
                    "unsupported zip layout (no local header at {})",
                    old.header
                );
                let (n, m) = (le16(&head, 26) as u64, le16(&head, 28) as u64);
                let data = old.header + 30 + n + m;
                ensure!(
                    data + old.csize <= end,
                    "unsupported zip layout (overlapping entries)"
                );
                let central = central_at(&mut reader, old.central)?;
                let raw_name = match &name {
                    Some(new) => {
                        let wanted = if old.is_dir {
                            format!("{new}/")
                        } else {
                            new.clone()
                        };
                        let extra = read_at(&mut reader, old.header + 30 + n, m as usize)?;
                        out.write_all(&renamed_local(&head, &extra, &wanted)?)?;
                        state.done_bytes += 30 + n + m;
                        copy(&mut out, data, end - data, &mut state)?;
                        Some(wanted)
                    }
                    None => {
                        copy(&mut out, old.header, end - old.header, &mut state)?;
                        None
                    }
                };
                let mut record = Central::parse(&central)?;
                if let Some(raw) = raw_name {
                    record.rename(&raw)?;
                }
                record.size = old.size;
                record.csize = old.csize;
                record.offset = offset;
                directory_records.push(record.bytes()?);
            }
            Out::Dir(name) => {
                state.current = name.clone();
                directory_records.push(write_new(
                    &mut out,
                    &format!("{name}/"),
                    0,
                    None,
                    None,
                    &mut state,
                    progress,
                    cancel,
                )?);
            }
            Out::File {
                name,
                size,
                modified,
                open,
            } => {
                state.current = name.clone();
                let mut input = open()?;
                directory_records.push(write_new(
                    &mut out,
                    &name,
                    size,
                    modified,
                    Some(&mut *input),
                    &mut state,
                    progress,
                    cancel,
                )?);
            }
        }
        state.done_items += 1;
        progress(state.clone());
    }
    let start = out.pos;
    let count = directory_records.len() as u64;
    for record in &directory_records {
        out.write_all(record)?;
    }
    let size = out.pos - start;
    ensure!(comment.len() <= 0xFFFF, "archive comment too long");
    if wide_end || count >= 0xFFFF || start >= MAX32 || size >= MAX32 {
        let end64 = out.pos;
        let mut rec = Vec::with_capacity(76);
        put32(&mut rec, END64_SIG);
        put64(&mut rec, 44);
        put16(&mut rec, 45);
        put16(&mut rec, 45);
        put32(&mut rec, 0);
        put32(&mut rec, 0);
        put64(&mut rec, count);
        put64(&mut rec, count);
        put64(&mut rec, size);
        put64(&mut rec, start);
        put32(&mut rec, LOCATOR_SIG);
        put32(&mut rec, 0);
        put64(&mut rec, end64);
        put32(&mut rec, 1);
        out.write_all(&rec)?;
    }
    let mut end = Vec::with_capacity(22 + comment.len());
    put32(&mut end, END_SIG);
    put16(&mut end, 0);
    put16(&mut end, 0);
    put16(&mut end, count.min(0xFFFF) as u16);
    put16(&mut end, count.min(0xFFFF) as u16);
    put32(&mut end, size.min(MAX32) as u32);
    put32(&mut end, start.min(MAX32) as u32);
    put16(&mut end, comment.len() as u16);
    end.extend_from_slice(&comment);
    out.write_all(&end)?;
    check(cancel)?;
    let file = out.inner.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    drop(file);
    progress(state);
    Ok(staged)
}

fn check(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
    Ok(())
}

/// Counts what it writes.
struct Writer {
    inner: BufWriter<File>,
    pos: u64,
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}
fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn read_at(file: &mut File, at: u64, len: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(at))?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)
        .with_context(|| format!("the archive is truncated at {at}"))?;
    Ok(bytes)
}

/// The whole central directory record at `at`.
fn central_at(file: &mut File, at: u64) -> Result<Vec<u8>> {
    let mut record = read_at(file, at, 46)?;
    ensure!(
        le32(&record, 0) == CENTRAL_SIG,
        "unsupported zip layout (no central record at {at})"
    );
    let rest = le16(&record, 28) as usize + le16(&record, 30) as usize + le16(&record, 32) as usize;
    record.extend(read_at(file, at + 46, rest)?);
    Ok(record)
}

/// Whether the archive ends with a zip64 end of central directory (kept when rewritten).
fn has_zip64_end(file: &mut File) -> Result<bool> {
    let len = file.seek(SeekFrom::End(0))?;
    let tail = len.min(22 + 0xFFFF + 20);
    let bytes = read_at(file, len - tail, tail as usize)?;
    let Some(end) = (0..=bytes.len().saturating_sub(22))
        .rev()
        .find(|&i| le32(&bytes, i) == END_SIG)
    else {
        return Ok(false);
    };
    Ok(end >= 20 && le32(&bytes, end - 20) == LOCATOR_SIG)
}

/// Extra fields as `(id, data)`; refuses a malformed block.
fn fields(mut extra: &[u8]) -> Result<Vec<(u16, Vec<u8>)>> {
    let mut out = Vec::new();
    while !extra.is_empty() {
        ensure!(extra.len() >= 4, "malformed extra field in the archive");
        let (id, len) = (le16(extra, 0), le16(extra, 2) as usize);
        ensure!(
            extra.len() >= 4 + len,
            "malformed extra field in the archive"
        );
        out.push((id, extra[4..4 + len].to_vec()));
        extra = &extra[4 + len..];
    }
    Ok(out)
}
fn join_fields(fields: &[(u16, Vec<u8>)]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (id, data) in fields {
        put16(&mut out, *id);
        put16(
            &mut out,
            u16::try_from(data.len()).context("extra field too long")?,
        );
        out.extend_from_slice(data);
    }
    ensure!(out.len() <= 0xFFFF, "extra fields too long");
    Ok(out)
}

/// Bit 11: the name is UTF-8.
fn utf8_flag(flags: u16, name: &str) -> u16 {
    if name.is_ascii() {
        flags
    } else {
        flags | 0x0800
    }
}

/// A local header (fixed part `head`, old `extra`) carrying `name` instead.
fn renamed_local(head: &[u8], extra: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut fixed = head[..30].to_vec();
    let extra: Vec<_> = fields(extra)?
        .into_iter()
        .filter(|(id, _)| *id != UNICODE_PATH)
        .collect();
    let extra = join_fields(&extra)?;
    let flags = utf8_flag(le16(&fixed, 6), name);
    fixed[6..8].copy_from_slice(&flags.to_le_bytes());
    let n = u16::try_from(name.len()).context("entry name too long")?;
    fixed[26..28].copy_from_slice(&n.to_le_bytes());
    fixed[28..30].copy_from_slice(&(extra.len() as u16).to_le_bytes());
    fixed.extend_from_slice(name.as_bytes());
    fixed.extend_from_slice(&extra);
    Ok(fixed)
}

/// A central directory record, with the values its zip64 field may carry kept apart.
struct Central {
    head: Vec<u8>,
    size: u64,
    csize: u64,
    offset: u64,
    /// The start disk, when it was in the zip64 field.
    disk: Option<u32>,
    /// Size, compressed size and offset were in the zip64 field (they stay there).
    wide: [bool; 3],
    name: Vec<u8>,
    extra: Vec<(u16, Vec<u8>)>,
    comment: Vec<u8>,
}
impl Central {
    fn parse(raw: &[u8]) -> Result<Self> {
        let (n, m) = (le16(raw, 28) as usize, le16(raw, 30) as usize);
        let name = raw[46..46 + n].to_vec();
        let mut extra = fields(&raw[46 + n..46 + n + m])?;
        let comment = raw[46 + n + m..].to_vec();
        let wide = [
            le32(raw, 24) as u64 == MAX32,
            le32(raw, 20) as u64 == MAX32,
            le32(raw, 42) as u64 == MAX32,
        ];
        let mut disk = None;
        if let Some(i) = extra.iter().position(|(id, _)| *id == ZIP64_FIELD) {
            let (_, data) = extra.remove(i);
            let at = 8 * wide.iter().filter(|w| **w).count();
            if le16(raw, 34) == 0xFFFF {
                ensure!(data.len() >= at + 4, "malformed zip64 field in the archive");
                disk = Some(le32(&data, at));
            }
        }
        Ok(Self {
            head: raw[..46].to_vec(),
            size: 0,
            csize: 0,
            offset: 0,
            disk,
            wide,
            name,
            extra,
            comment,
        })
    }
    fn rename(&mut self, name: &str) -> Result<()> {
        self.extra.retain(|(id, _)| *id != UNICODE_PATH);
        let flags = utf8_flag(le16(&self.head, 8), name);
        self.head[8..10].copy_from_slice(&flags.to_le_bytes());
        self.name = name.as_bytes().to_vec();
        Ok(())
    }
    fn bytes(&self) -> Result<Vec<u8>> {
        let mut head = self.head.clone();
        let mut wide = Vec::new();
        for (at, value, keep) in [
            (24, self.size, self.wide[0]),
            (20, self.csize, self.wide[1]),
            (42, self.offset, self.wide[2]),
        ] {
            if keep || value >= MAX32 {
                put64(&mut wide, value);
                head[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            } else {
                head[at..at + 4].copy_from_slice(&(value as u32).to_le_bytes());
            }
        }
        if let Some(disk) = self.disk {
            put32(&mut wide, disk);
        }
        let mut extra = self.extra.clone();
        if !wide.is_empty() {
            extra.insert(0, (ZIP64_FIELD, wide));
            if le16(&head, 6) & 0xFF < 45 {
                let needed = (le16(&head, 6) & 0xFF00) | 45;
                head[6..8].copy_from_slice(&needed.to_le_bytes());
            }
        }
        let extra = join_fields(&extra)?;
        let n = u16::try_from(self.name.len()).context("entry name too long")?;
        head[28..30].copy_from_slice(&n.to_le_bytes());
        head[30..32].copy_from_slice(&(extra.len() as u16).to_le_bytes());
        ensure!(
            self.name.len() + extra.len() + self.comment.len() <= 0xFFFF,
            "central directory record too long"
        );
        let mut out = head;
        out.extend_from_slice(&self.name);
        out.extend_from_slice(&extra);
        out.extend_from_slice(&self.comment);
        Ok(out)
    }
}

/// DOS date and time (local wall clock, two-second steps, 1980 onwards).
fn dos_time(time: Option<SystemTime>) -> (u16, u16) {
    use chrono::{Datelike, Timelike};
    let local = chrono::DateTime::<chrono::Local>::from(time.unwrap_or_else(SystemTime::now));
    if local.year() < 1980 || local.year() > 2107 {
        return (0, (1 << 5) | 1);
    }
    let t = ((local.hour() as u16) << 11)
        | ((local.minute() as u16) << 5)
        | (local.second() as u16 / 2);
    let d =
        (((local.year() - 1980) as u16) << 9) | ((local.month() as u16) << 5) | local.day() as u16;
    (t, d)
}

/// Writes a new entry (a folder when `input` is None) and returns its central record.
#[allow(clippy::too_many_arguments)]
fn write_new(
    out: &mut Writer,
    name: &str,
    size: u64,
    modified: Option<SystemTime>,
    input: Option<&mut dyn Read>,
    state: &mut Progress,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let offset = out.pos;
    let deflate = input.is_some() && size > 0;
    let wide = size >= ZIP64_FROM;
    let needed: u16 = if wide { 45 } else { 20 };
    let flags = utf8_flag(0, name);
    let method: u16 = if deflate { 8 } else { 0 };
    let (time, date) = dos_time(modified);
    let n = u16::try_from(name.len()).context("entry name too long")?;
    let mut head = Vec::with_capacity(30 + name.len() + 20);
    put32(&mut head, LOCAL_SIG);
    put16(&mut head, needed);
    put16(&mut head, flags);
    put16(&mut head, method);
    put16(&mut head, time);
    put16(&mut head, date);
    put32(&mut head, 0);
    put32(&mut head, if wide { u32::MAX } else { 0 });
    put32(&mut head, if wide { u32::MAX } else { 0 });
    put16(&mut head, n);
    put16(&mut head, if wide { 20 } else { 0 });
    head.extend_from_slice(name.as_bytes());
    if wide {
        put16(&mut head, ZIP64_FIELD);
        put16(&mut head, 16);
        put64(&mut head, 0);
        put64(&mut head, 0);
    }
    out.write_all(&head)?;
    let data = out.pos;
    let mut crc = flate2::Crc::new();
    let mut copied = 0u64;
    if let Some(input) = input {
        let mut pump = |sink: &mut dyn Write| -> Result<()> {
            let mut buffer = vec![0u8; 256 << 10];
            loop {
                check(cancel)?;
                let n = match input.read(&mut buffer) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    r => r?,
                };
                if n == 0 {
                    return Ok(());
                }
                crc.update(&buffer[..n]);
                sink.write_all(&buffer[..n])?;
                copied += n as u64;
                state.done_bytes = state.done_bytes.saturating_add(n as u64);
                progress(state.clone());
            }
        };
        if deflate {
            let mut encoder =
                flate2::write::DeflateEncoder::new(&mut *out, flate2::Compression::default());
            pump(&mut encoder)?;
            encoder.finish()?;
        } else {
            pump(&mut *out)?;
        }
        ensure!(
            copied == size,
            "{name} changed while it was added ({copied} bytes instead of {size})"
        );
    }
    let csize = out.pos - data;
    ensure!(
        wide || csize < MAX32,
        "{name} grew past 4 GiB when compressed"
    );
    // The sizes and checksum go into the header now that they are known.
    let end = out.pos;
    out.inner.seek(SeekFrom::Start(offset + 14))?;
    out.inner.write_all(&crc.sum().to_le_bytes())?;
    if wide {
        out.inner
            .seek(SeekFrom::Start(offset + 30 + u64::from(n) + 4))?;
        out.inner.write_all(&copied.to_le_bytes())?;
        out.inner.write_all(&csize.to_le_bytes())?;
    } else {
        out.inner.write_all(&(csize as u32).to_le_bytes())?;
        out.inner.write_all(&(copied as u32).to_le_bytes())?;
    }
    out.inner.seek(SeekFrom::Start(end))?;
    let is_dir = name.ends_with('/');
    let mode: u32 = if is_dir { 0o040755 } else { 0o100644 };
    let mut central = Vec::with_capacity(46);
    put32(&mut central, CENTRAL_SIG);
    put16(&mut central, (3 << 8) | 45);
    put16(&mut central, needed);
    put16(&mut central, flags);
    put16(&mut central, method);
    put16(&mut central, time);
    put16(&mut central, date);
    put32(&mut central, crc.sum());
    put32(&mut central, 0);
    put32(&mut central, 0);
    put16(&mut central, n);
    put16(&mut central, 0);
    put16(&mut central, 0);
    put16(&mut central, 0);
    put16(&mut central, 0);
    put32(&mut central, (mode << 16) | if is_dir { 0x10 } else { 0 });
    put32(&mut central, 0);
    Central {
        head: central,
        size: copied,
        csize,
        offset,
        disk: None,
        wide: [wide, wide, false],
        name: name.as_bytes().to_vec(),
        extra: Vec::new(),
        comment: Vec::new(),
    }
    .bytes()
}

/// Deletes `paths` (entries or folders inside zips on this computer) with one rewrite per
/// archive. The folders that held them stay, even when they end up empty.
pub fn remove_entries(
    paths: &[VPath],
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<()> {
    let mut by_zip: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for p in paths {
        let (zip, inner) = editable(p)?;
        ensure!(
            !inner.is_empty(),
            "the archive itself cannot be deleted from inside it: {}",
            p.display()
        );
        match by_zip.iter_mut().find(|(z, _)| *z == zip) {
            Some((_, names)) => names.push(inner),
            None => by_zip.push((zip, vec![inner])),
        }
    }
    for (zip, names) in by_zip {
        let existing = Names::read(&zip)?;
        let mut edits = Vec::new();
        for name in &names {
            ensure!(existing.exists(name), "not in the archive: {name}");
            edits.push(Edit::Delete(name.clone()));
        }
        for name in &names {
            let up = parent(name);
            if !up.is_empty() && !names.iter().any(|d| under(up, d)) {
                edits.push(Edit::Mkdir(up.to_owned()));
            }
        }
        edit(&zip, edits, progress, cancel)?;
    }
    Ok(())
}

/// Renames or moves an entry or folder inside one zip (never onto an existing name).
pub fn rename_entry(from: &VPath, to: &VPath) -> Result<()> {
    let (zip, a) = editable(from)?;
    let (other, b) = editable(to)?;
    ensure!(zip == other, "entries cannot be moved between archives");
    ensure!(
        !a.is_empty() && !b.is_empty(),
        "the archive root cannot be renamed from inside it"
    );
    if a == b {
        return Ok(());
    }
    ensure!(!under(&b, &a), "cannot move a folder into itself: {a}");
    let names = Names::read(&zip)?;
    ensure!(names.exists(&a), "not in the archive: {a}");
    ensure!(!names.exists(&b), "{} already exists", to.display());
    let mut edits = vec![Edit::Rename {
        from: a.clone(),
        to: b,
    }];
    if !parent(&a).is_empty() {
        edits.push(Edit::Mkdir(parent(&a).to_owned()));
    }
    edit(&zip, edits, &|_| {}, &AtomicBool::new(false))
}

/// A new folder entry; fails when the name exists.
pub fn mkdir_entry(path: &VPath) -> Result<()> {
    let (zip, name) = editable(path)?;
    ensure!(!name.is_empty(), "the archive root exists");
    ensure!(
        !Names::read(&zip)?.exists(&name),
        "{} already exists",
        path.display()
    );
    edit(
        &zip,
        vec![Edit::Mkdir(name)],
        &|_| {},
        &AtomicBool::new(false),
    )
}

/// A writer for one new file inside a zip: the bytes are spooled to a temp file and the
/// archive is rewritten on `flush()` (once). Dropped without a flush, nothing changes.
pub fn spool(path: &VPath, replace: bool) -> Result<Box<dyn Write + Send>> {
    let (zip, name) = editable(path)?;
    ensure!(!name.is_empty(), "the archive root is not a file");
    let names = Names::read(&zip)?;
    ensure!(
        !names.dirs.contains(&name),
        "{} is a folder",
        path.display()
    );
    ensure!(
        replace || !names.files.contains(&name),
        "{} already exists",
        path.display()
    );
    let dir = crate::cache_dir();
    fs::create_dir_all(&dir)?;
    Ok(Box::new(Spool {
        zip,
        name,
        replace,
        temp: Some(tempfile::NamedTempFile::new_in(dir)?),
    }))
}

struct Spool {
    zip: PathBuf,
    name: String,
    replace: bool,
    temp: Option<tempfile::NamedTempFile>,
}
impl Write for Spool {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match &mut self.temp {
            Some(temp) => temp.write(bytes),
            None => Err(io::Error::other("the archive entry was already written")),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        let Some(mut temp) = self.temp.take() else {
            return Ok(());
        };
        let mut commit = || -> Result<()> {
            temp.flush()?;
            if !self.replace {
                ensure!(
                    !Names::read(&self.zip)?.exists(&self.name),
                    "{} already exists",
                    self.name
                );
            }
            let size = temp.as_file().metadata()?.len();
            let path = temp.path().to_path_buf();
            let open: Open = Box::new(move || Ok(Box::new(File::open(&path)?)));
            edit(
                &self.zip,
                vec![Edit::Add {
                    name: self.name.clone(),
                    size,
                    modified: None,
                    open,
                }],
                &|_| {},
                &AtomicBool::new(false),
            )
        };
        commit().map_err(|e| io::Error::other(format!("{e:#}")))
    }
}

/// A source item's file or folder, `rel` below the item (`""`: the item itself).
struct Node {
    rel: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
    path: VPath,
}

/// Every file and folder of `path` on `provider`, refusing links.
fn walk(
    provider: &dyn crate::Provider,
    path: &VPath,
    rel: String,
    nodes: &mut Vec<Node>,
    cancel: &AtomicBool,
    depth: usize,
) -> Result<()> {
    check(cancel)?;
    ensure!(depth < 256, "directory nesting limit: {}", path.display());
    let e = provider.stat(path)?;
    ensure!(
        !e.is_link && e.kind != crate::Kind::Symlink,
        "copy/move of links is unsupported: {}",
        path.display()
    );
    let is_dir = e.kind == crate::Kind::Dir;
    nodes.push(Node {
        rel: rel.clone(),
        is_dir,
        size: if is_dir { 0 } else { e.size },
        modified: e.modified,
        path: path.clone(),
    });
    if is_dir {
        for child in provider.list(path)? {
            walk(
                provider,
                &child.path,
                join(&rel, &child.name),
                nodes,
                cancel,
                depth + 1,
            )?;
        }
    }
    Ok(())
}

/// Names in the archive, growing as a transfer adds its items.
struct Taken {
    files: HashSet<String>,
    dirs: HashSet<String>,
}
impl Taken {
    fn exists(&self, name: &str) -> bool {
        self.files.contains(name) || self.dirs.contains(name)
    }
    fn add(&mut self, name: &str, is_dir: bool) {
        let mut up = parent(name);
        while !up.is_empty() && self.dirs.insert(up.to_owned()) {
            up = parent(up);
        }
        if is_dir {
            self.dirs.insert(name.to_owned());
        } else {
            self.files.insert(name.to_owned());
        }
    }
    /// `name (2).ext`, `name (3).ext`… for a file, `name (2)`… for a folder.
    fn free(&self, name: &str, is_dir: bool) -> String {
        let dir = parent(name);
        let leaf = &name[name.rfind('/').map_or(0, |i| i + 1)..];
        let (stem, ext) = match leaf.rsplit_once('.') {
            Some((s, e)) if !is_dir && !s.is_empty() => (s, Some(e)),
            _ => (leaf, None),
        };
        (2u64..)
            .map(|n| match ext {
                Some(e) => join(dir, &format!("{stem} ({n}).{e}")),
                None => join(dir, &format!("{stem} ({n})")),
            })
            .find(|c| !self.exists(c))
            .expect("a free name")
    }
}

/// How one top-level item lands.
enum Landing {
    /// Under this name, replacing a file there when `replace`.
    Whole(String, bool),
    /// Into the folder of the same name, file by file.
    Merge,
    Skip,
}

fn landing(taken: &Taken, top: &str, is_dir: bool, conflict: Conflict) -> Result<Landing> {
    if !taken.exists(top) {
        return Ok(Landing::Whole(top.to_owned(), false));
    }
    let folder = taken.dirs.contains(top);
    Ok(match conflict {
        Conflict::RenameNew => Landing::Whole(taken.free(top, is_dir), false),
        _ if is_dir && folder => Landing::Merge,
        Conflict::Skip => Landing::Skip,
        Conflict::Overwrite if !is_dir && !folder => Landing::Whole(top.to_owned(), true),
        Conflict::Overwrite => bail!("unsafe destination: {top}"),
    })
}

/// For a merge: whether the file `target` lands (`Some(replaces a file)`) or is skipped.
fn merge_file(taken: &Taken, target: &str, conflict: Conflict) -> Result<Option<bool>> {
    let mut up = parent(target);
    while !up.is_empty() {
        if taken.files.contains(up) {
            ensure!(conflict == Conflict::Skip, "unsafe destination: {up}");
            return Ok(None);
        }
        up = parent(up);
    }
    if taken.dirs.contains(target) {
        ensure!(conflict == Conflict::Skip, "unsafe destination: {target}");
        return Ok(None);
    }
    Ok(match (taken.files.contains(target), conflict) {
        (false, _) => Some(false),
        (true, Conflict::Overwrite) => Some(true),
        (true, _) => None,
    })
}

/// A run before this one (`journal`) recorded what its rewrite of `zip` placed, with the
/// archive as it was then (`Placed::archive`): where the archive changed since and holds
/// the entry, the rewrite went through. Moved files and folders still at their source
/// (the run stopped before deleting them) are deleted now (folders only when empty), and
/// the recorded sources are returned: they are done.
fn rewritten(
    journal: &Journal,
    zip: &Path,
    now: Stamp,
    mv: bool,
    router: &crate::Router,
) -> Result<HashSet<VPath>> {
    let recorded: Vec<(&VPath, &Placed)> = (journal.placed.iter())
        .filter(|(_, p)| p.archive.is_some_and(|was| was != now))
        .collect();
    if recorded.is_empty() {
        return Ok(HashSet::new());
    }
    let names = Names::read(zip)?;
    let held = |p: &Placed| {
        (p.target.split_archive())
            .and_then(|(_, inner)| safe_name(&inner).ok())
            .is_some_and(|inner| names.exists(&inner))
    };
    let mut done = HashSet::new();
    let mut dirs = Vec::new();
    for (source, placed) in recorded {
        if !held(placed) {
            continue;
        }
        done.insert(source.clone());
        if !mv {
            continue;
        }
        if placed.file.is_none() {
            dirs.push(source);
            continue;
        }
        let gone = match source.to_local_path() {
            Some(local) => match fs::remove_file(&local) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            },
            None => {
                let provider = (router.provider_for(source))
                    .with_context(|| format!("no provider: {}", source.display()))?;
                match provider.stat(source) {
                    Ok(_) => provider.remove(source),
                    Err(_) => Ok(()),
                }
            }
        };
        gone.with_context(|| format!("remove moved file {}", source.display()))?;
    }
    // Deepest first; a folder holding anything else stays.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.path.len()));
    for dir in dirs {
        let _ = match dir.to_local_path() {
            Some(local) => fs::remove_dir(&local).map_err(anyhow::Error::from),
            None => (router.provider_for(dir))
                .context("no provider")
                .and_then(|p| p.remove_empty_dir(dir)),
        };
    }
    Ok(done)
}

/// Copies or moves `src` (from anywhere, or from inside this same zip) into the folder
/// `dst_dir` inside a zip on this computer: one rewrite of the archive. Clashes follow
/// `conflict` as in `transfer` (folders merge; Keep both renames the top-level item).
/// Moved sources from elsewhere are deleted once the new archive is in place. With a
/// `journal`, what the rewrite places is recorded before it runs (a copy's top-level
/// items, a move's files and folders from elsewhere); run again after a stop, sources
/// the earlier rewrite placed are done, and a move deletes what of them is left.
#[allow(clippy::too_many_arguments)]
pub(crate) fn transfer_into(
    src: &[VPath],
    dst_dir: &VPath,
    mv: bool,
    conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    router: &crate::Router,
    journal: Option<&mut Journal>,
) -> Result<()> {
    check(cancel)?;
    let (zip, base) = editable(dst_dir)?;
    let outer = dst_dir.split_archive().context("not inside an archive")?.0;
    let before = Stamp::of(&fs::metadata(&zip)?);
    let done = match journal.as_deref() {
        Some(j) => rewritten(j, &zip, before, mv, router)?,
        None => HashSet::new(),
    };
    let src: Vec<&VPath> = src.iter().filter(|s| !done.contains(*s)).collect();
    // Recorded before the rewrite (see `Placed::archive`).
    let record = |inner: &str, file: Option<Stamp>| Placed {
        target: VPath::join_archive(&outer, inner),
        file,
        archive: Some(before),
    };
    let mut records: Vec<(VPath, Placed)> = Vec::new();
    let names = Names::read(&zip)?;
    ensure!(
        base.is_empty() || names.dirs.contains(&base),
        "destination must be a folder: {}",
        dst_dir.display()
    );
    let mut taken = Taken {
        files: names.files.clone(),
        dirs: names.dirs.clone(),
    };
    let mut edits: Vec<Edit> = Vec::new();
    let mut skipped = 0usize;
    type Moved = Vec<(std::sync::Arc<dyn crate::Provider>, VPath, Placed)>;
    // Moved sources from elsewhere, deleted after the commit: files, then folders deepest
    // first (only folders whose every file moved).
    let (mut moved_files, mut moved_dirs): (Moved, Moved) = (Vec::new(), Vec::new());
    for s in src {
        check(cancel)?;
        ensure!(!s.name().is_empty(), "unsupported source: {}", s.display());
        let top = join(&base, s.name());
        if let Some((_, inner)) = s.split_archive().filter(|(o, _)| *o == outer) {
            // Inside this same zip: raw renames or copies.
            let inner = safe_name(&inner)?;
            ensure!(
                !under(&base, &inner),
                "cannot transfer a folder into itself: {}",
                s.display()
            );
            ensure!(names.exists(&inner), "not in the archive: {inner}");
            if mv && parent(&inner) == base {
                continue;
            }
            let is_dir = names.dirs.contains(&inner);
            let to_entry = |from: &str, to: &str| {
                let (from, to) = (from.to_owned(), to.to_owned());
                if mv {
                    Edit::Rename { from, to }
                } else {
                    Edit::Copy { from, to }
                }
            };
            let landed: Vec<(String, bool)> = match landing(&taken, &top, is_dir, conflict)? {
                Landing::Skip => {
                    skipped += 1;
                    continue;
                }
                Landing::Whole(to, replace) => {
                    if replace {
                        edits.push(Edit::Delete(to.clone()));
                    }
                    edits.push(to_entry(&inner, &to));
                    if !mv {
                        records.push((s.clone(), record(&to, None)));
                    }
                    names
                        .under(&inner)
                        .map(|(n, d)| (format!("{to}{}", &n[inner.len()..]), *d))
                        .chain([(to.clone(), is_dir)])
                        .collect()
                }
                Landing::Merge => {
                    if !mv {
                        records.push((s.clone(), record(&top, None)));
                    }
                    let mut landed = Vec::new();
                    for (name, node_dir) in names.under(&inner) {
                        let target = format!("{top}{}", &name[inner.len()..]);
                        if *node_dir {
                            // The folder entry itself: its files go one by one.
                            if mv {
                                edits.push(Edit::DropExact(name.clone()));
                            }
                            edits.push(Edit::Mkdir(target.clone()));
                            landed.push((target, true));
                            continue;
                        }
                        match merge_file(&taken, &target, conflict)? {
                            None => skipped += 1,
                            Some(replace) => {
                                if replace {
                                    edits.push(Edit::Delete(target.clone()));
                                }
                                edits.push(to_entry(name, &target));
                                landed.push((target, false));
                            }
                        }
                    }
                    landed
                }
            };
            for (name, is_dir) in landed {
                taken.add(&name, is_dir);
            }
            if mv && !parent(&inner).is_empty() {
                edits.push(Edit::Mkdir(parent(&inner).to_owned()));
            }
            continue;
        }
        // From another location (or another archive): streamed in.
        let provider = router
            .provider_for(s)
            .with_context(|| format!("no provider: {}", s.display()))?;
        if mv {
            ensure!(
                provider.caps().delete,
                "cannot move out of a read-only location (copy instead): {}",
                s.display()
            );
        }
        let mut nodes = Vec::new();
        walk(&*provider, s, String::new(), &mut nodes, cancel, 0)?;
        for node in nodes.iter().filter(|n| !n.is_dir) {
            if let Some(local) = node.path.to_local_path() {
                ensure!(
                    !same_file::is_same_file(&local, &zip).unwrap_or(false),
                    "cannot add an archive to itself"
                );
            }
        }
        let is_dir = nodes.first().is_some_and(|n| n.is_dir);
        let (top, merge) = match landing(&taken, &top, is_dir, conflict)? {
            Landing::Skip => {
                skipped += 1;
                continue;
            }
            Landing::Whole(to, _) => (to, false),
            Landing::Merge => (top, true),
        };
        if !mv {
            records.push(((*s).clone(), record(&top, None)));
        }
        let mut left_behind: Vec<VPath> = Vec::new();
        let first_dir = moved_dirs.len();
        for node in nodes {
            let target = join(&top, &node.rel);
            if node.is_dir {
                edits.push(Edit::Mkdir(target.clone()));
                taken.add(&target, true);
                if mv {
                    moved_dirs.push((provider.clone(), node.path, record(&target, None)));
                }
                continue;
            }
            if merge && merge_file(&taken, &target, conflict)?.is_none() {
                skipped += 1;
                left_behind.push(node.path);
                continue;
            }
            taken.add(&target, false);
            let stamp = Stamp::new(node.size, node.modified);
            let placed = record(&target, Some(stamp));
            let (reader, path) = (provider.clone(), node.path.clone());
            edits.push(Edit::Add {
                name: target,
                size: node.size,
                modified: node.modified,
                open: Box::new(move || reader.read(&path)),
            });
            if mv {
                moved_files.push((provider.clone(), node.path, placed));
            }
        }
        // Folders still holding skipped files stay.
        let mut i = first_dir;
        while i < moved_dirs.len() {
            let dir = format!("{}/", moved_dirs[i].1.path.trim_end_matches('/'));
            if left_behind.iter().any(|f| f.path.starts_with(&dir)) {
                moved_dirs.remove(i);
            } else {
                i += 1;
            }
        }
    }
    let report = |mut p: Progress| {
        p.skipped = skipped;
        progress(p);
    };
    if edits.is_empty() {
        report(Progress::default());
        return Ok(());
    }
    if let Some(j) = journal {
        let moved = moved_files.iter().chain(&moved_dirs);
        for (source, placed) in records
            .into_iter()
            .chain(moved.map(|(_, path, placed)| (path.clone(), placed.clone())))
        {
            j.placed(source, placed, false)?;
        }
        j.flush()?;
    }
    edit(&zip, edits, &report, cancel)?;
    for (provider, file, _) in moved_files {
        match file.to_local_path() {
            // In place in the archive: the source goes for good, as for any move.
            Some(local) => fs::remove_file(&local)
                .with_context(|| format!("remove moved file {}", local.display()))?,
            None => provider.remove(&file)?,
        }
    }
    for (provider, dir, _) in moved_dirs.into_iter().rev() {
        match dir.to_local_path() {
            Some(local) => fs::remove_dir(&local)
                .with_context(|| format!("remove moved folder {}", local.display()))?,
            None => provider.remove_empty_dir(&dir)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn central_records_move_offsets_into_zip64_and_keep_wide_fields() {
        let mut raw = Vec::new();
        put32(&mut raw, CENTRAL_SIG);
        raw.extend_from_slice(&[0; 42]);
        raw[28..30].copy_from_slice(&1u16.to_le_bytes());
        raw.push(b'a');
        let mut record = Central::parse(&raw).unwrap();
        record.size = 5;
        record.csize = 7;
        record.offset = 5 << 30;
        let bytes = record.bytes().unwrap();
        assert_eq!(le32(&bytes, 42), u32::MAX, "offset in the zip64 field");
        assert_eq!((le32(&bytes, 24), le32(&bytes, 20)), (5, 7));
        assert_eq!(le16(&bytes, 6) & 0xFF, 45);
        let extra = fields(&bytes[47..]).unwrap();
        assert_eq!(
            extra,
            vec![(ZIP64_FIELD, (5u64 << 30).to_le_bytes().to_vec())]
        );
        // Parsed back: the offset stays wide even when it fits again.
        let mut again = Central::parse(&bytes).unwrap();
        assert_eq!(again.wide, [false, false, true]);
        again.offset = 10;
        again.size = 5;
        again.csize = 7;
        let small = again.bytes().unwrap();
        assert_eq!(le32(&small, 42), u32::MAX);
        assert_eq!(
            fields(&small[47..]).unwrap(),
            vec![(ZIP64_FIELD, 10u64.to_le_bytes().to_vec())]
        );
    }

    #[test]
    fn names_under_and_parents() {
        assert!(under("a/b", "a") && under("a", "a") && under("x", ""));
        assert!(!under("ab", "a"));
        assert_eq!(parent("a/b/c"), "a/b");
        assert_eq!(parent("a"), "");
        assert_eq!(join("", "x"), "x");
        assert_eq!(join("d", "x"), "d/x");
        assert_eq!(join("d", ""), "d");
    }
}
