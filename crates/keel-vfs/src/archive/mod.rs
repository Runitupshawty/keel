//! Archives as read-only folders (`x.zip!/dir/a.txt`). Everything here blocks: call it off
//! the UI thread. Links and special files inside archives are not listed or extracted.
pub mod cache;
#[cfg(feature = "rar")]
mod rar;
#[cfg(feature = "sevenz")]
mod sevenz;
#[cfg(feature = "tar")]
mod tar;
#[cfg(feature = "zip")]
mod zip;

use crate::{Caps, Entry, Kind, Provider, VPath};
use anyhow::{bail, Context, Result};
use cache::{CacheKey, MaterialiseCache, Pinned};
use parking_lot::Mutex;
use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, mpsc, Arc, Weak},
    time::SystemTime,
};

/// Receives download progress for archives fetched from another provider.
pub type ProgressSink = Arc<dyn Fn(crate::Progress) + Send + Sync>;

/// Entries above this are never materialised (previews show `TooLarge`).
pub const MATERIALISE_LIMIT: u64 = 1 << 30;

pub trait ArchiveReader: Send {
    /// Metadata only (regular files and folders, raw archive names); never entry bodies.
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>>;
    /// The archive file being read.
    fn local(&self) -> &Path;
    /// One pass in archive order: `each(raw name, declared size, body)` for every regular
    /// file `want` accepts. Everything else is skipped (and decoded past where the format
    /// needs it). Bodies are raw: use `visit`, which holds them to their declared size.
    fn each_file(
        &mut self,
        want: &dyn Fn(&str) -> bool,
        each: &mut dyn FnMut(&str, u64, &mut dyn Read) -> Result<()>,
    ) -> Result<()>;
    /// `each_file` where a body that ends before, or runs past, the size its header declares
    /// is an IO error rather than silently short or unbounded data.
    fn visit(
        &mut self,
        want: &dyn Fn(&str) -> bool,
        each: &mut dyn FnMut(&str, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        self.each_file(want, &mut |name, size, body| {
            each(name, &mut Exact { body, left: size })
        })
    }
    /// One entry body by its raw name, streaming from a decoder thread.
    fn read(&mut self, inner: &str) -> Result<Box<dyn Read + Send>> {
        anyhow::ensure!(
            self.entries()?
                .iter()
                .any(|e| !e.is_dir && e.inner == inner),
            "archive entry not found: {inner}"
        );
        Ok(read_entry(self.local().into(), inner.into(), None))
    }
}

#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    /// The raw name as stored; `safe_name` normalises it.
    pub inner: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub encrypted: bool,
}

/// Holds an entry body to exactly `left` more bytes.
struct Exact<'a> {
    body: &'a mut dyn Read,
    left: u64,
}
impl Read for Exact<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            // Probing one byte also lets checksumming readers verify at their end.
            return match self.body.read(&mut [0])? {
                0 => Ok(0),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "archive entry is larger than its header says",
                )),
            };
        }
        let max = bytes
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.body.read(&mut bytes[..max])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "archive entry is truncated",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Ends a visit early once the wanted entry is done.
#[derive(Debug)]
struct Found;
impl std::fmt::Display for Found {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("found")
    }
}
impl std::error::Error for Found {}

/// Runs `body` on the first regular file named `raw` (size-checked), decoding no further.
fn with_entry(
    local: &Path,
    raw: &str,
    body: impl FnOnce(&mut dyn Read) -> Result<()>,
) -> Result<()> {
    let mut body = Some(body);
    let found = Cell::new(false);
    let result =
        open_archive(local)?.visit(&|name| !found.get() && name == raw, &mut |_, input| {
            found.set(true);
            (body.take().context("entry visited twice")?)(input)?;
            Err(Found.into())
        });
    match result {
        Err(e) if e.is::<Found>() => Ok(()),
        Err(e) => Err(e),
        Ok(()) if found.get() => Ok(()),
        Ok(()) => bail!("archive entry not found: {raw}"),
    }
}

/// Streams one entry from a decoder thread; `pin` keeps a materialised archive alive.
fn read_entry(local: PathBuf, raw: String, pin: Option<Pinned>) -> Box<dyn Read + Send> {
    stream(move |out| {
        let _pin = pin;
        with_entry(&local, &raw, |input| {
            io::copy(input, out)?;
            Ok(())
        })
    })
}

/// Dispatch by magic bytes, then by extension.
pub fn open_archive(local: &Path) -> Result<Box<dyn ArchiveReader>> {
    let mut magic = [0; 512];
    let mut file = File::open(local).with_context(|| format!("open {}", local.display()))?;
    let n = read_up_to(&mut file, &mut magic)?;
    let magic = &magic[..n];
    let name = local.to_string_lossy().to_ascii_lowercase();
    #[cfg(feature = "zip")]
    if magic.starts_with(b"PK\x03\x04") || magic.starts_with(b"PK\x05\x06") {
        return Ok(Box::new(zip::Reader::open(local)?));
    }
    #[cfg(feature = "sevenz")]
    if magic.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Ok(Box::new(sevenz::Reader::open(local)?));
    }
    #[cfg(feature = "rar")]
    if magic.starts_with(b"Rar!\x1a\x07") {
        return Ok(Box::new(rar::Reader::open(local)?));
    }
    #[cfg(feature = "tar")]
    {
        use tar::Compression::*;
        let compression = if magic.starts_with(&[0x1f, 0x8b]) {
            Some(Gzip)
        } else if magic.starts_with(b"BZh") {
            Some(Bzip2)
        } else if magic.starts_with(b"\xfd7zXZ\0") {
            Some(Xz)
        } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Some(Zstd)
        } else if magic.get(257..262) == Some(b"ustar") {
            Some(Plain)
        } else {
            None
        };
        if let Some(compression) = compression {
            return Ok(Box::new(tar::Reader::new(local, compression)));
        }
    }
    // No known magic (e.g. an old v7 tar, or a truncated file): fall back to the extension.
    #[cfg(feature = "zip")]
    if name.ends_with(".zip") || name.ends_with(".jar") {
        return Ok(Box::new(zip::Reader::open(local)?));
    }
    #[cfg(feature = "sevenz")]
    if name.ends_with(".7z") {
        return Ok(Box::new(sevenz::Reader::open(local)?));
    }
    #[cfg(feature = "rar")]
    if name.ends_with(".rar") {
        return Ok(Box::new(rar::Reader::open(local)?));
    }
    #[cfg(feature = "tar")]
    if name.ends_with(".tar") {
        return Ok(Box::new(tar::Reader::new(local, tar::Compression::Plain)));
    }
    let _ = (magic, name);
    bail!("unsupported or corrupt archive: {}", local.display())
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Parsed entry lists kept between calls: `(safe name or None if unsafe, entry)`.
type Listing = Arc<Vec<(Option<String>, ArchiveEntry)>>;
/// How many archives' entry lists stay parsed.
const LISTINGS: usize = 256;
#[derive(Default)]
struct Listings {
    map: HashMap<CacheKey, (Listing, u64)>,
    clock: u64,
}

/// One archive opened for one call.
struct Opened {
    /// The archive file on disk (a cache file for nested archives, kept by `pin`).
    local: PathBuf,
    pin: Option<Pinned>,
    entries: Listing,
    /// The archive's own path and the normalised name asked for inside it.
    outer: VPath,
    inner: String,
}
impl Opened {
    /// The entry whose normalised name is `self.inner`.
    fn find(&self) -> Option<&ArchiveEntry> {
        self.entries
            .iter()
            .find(|(name, _)| name.as_deref() == Some(self.inner.as_str()))
            .map(|(_, e)| e)
    }
}

/// Serves `file://…/x.zip!/…` (and nested chains). The outer path is resolved through the
/// router, so an archive on any provider works once that provider has `local_copy`.
pub struct ArchiveProvider {
    cache: Arc<MaterialiseCache>,
    providers: Weak<crate::router::Registry>,
    listings: Mutex<Listings>,
    /// Stops a download of a remote archive (`local_copy_cancellable`); never set by default.
    cancel: Arc<AtomicBool>,
    progress: Option<ProgressSink>,
}
impl ArchiveProvider {
    pub(crate) fn new(
        cache: Arc<MaterialiseCache>,
        providers: Weak<crate::router::Registry>,
    ) -> Self {
        Self {
            cache,
            providers,
            listings: Mutex::default(),
            cancel: Arc::default(),
            progress: None,
        }
    }
    /// Lets the app cancel (and watch) the download of an archive that lives on another
    /// provider, e.g. a zip on an SFTP host being opened as a folder.
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>, progress: Option<ProgressSink>) -> Self {
        self.cancel = cancel;
        self.progress = progress;
        self
    }
    /// The archive file at `outer` (on another provider) as a local file.
    fn fetch(&self, outer: &VPath) -> Result<PathBuf> {
        let progress = |p: crate::Progress| {
            if let Some(sink) = &self.progress {
                sink(p);
            }
        };
        self.provider(outer)?
            .local_copy_cancellable(outer, &progress, &self.cancel)
    }
    fn provider(&self, path: &VPath) -> Result<Arc<dyn Provider>> {
        let registry = self.providers.upgrade().context("router was dropped")?;
        crate::router::find(&registry, path)
            .with_context(|| format!("no provider for {}", path.display()))
    }
    /// The archive holding `path`, its (cached) entry list and `path`'s normalised name.
    fn open(&self, path: &VPath) -> Result<Opened> {
        let (outer, inner) = path.split_archive().context("not an archive path")?;
        let inner = if inner.trim_matches('/').is_empty() {
            String::new()
        } else {
            safe_name(&inner)?
        };
        // An archive inside an archive is materialised and pinned for this call.
        let (local, pin) = match outer.split_archive() {
            Some((_, name)) if !name.trim_matches('/').is_empty() => {
                let pinned = self.materialise(&outer)?;
                (pinned.path().to_path_buf(), Some(pinned))
            }
            _ => (self.fetch(&outer)?, None),
        };
        let entries = self.listing(&self.key(&outer)?, &local)?;
        Ok(Opened {
            local,
            pin,
            entries,
            outer,
            inner,
        })
    }
    /// The entry list of the archive at `local`, parsed once per version of the archive.
    fn listing(&self, key: &CacheKey, local: &Path) -> Result<Listing> {
        {
            let mut listings = self.listings.lock();
            listings.clock += 1;
            let now = listings.clock;
            if let Some((listing, used)) = listings.map.get_mut(key) {
                *used = now;
                return Ok(listing.clone());
            }
        }
        let listing: Listing = Arc::new(
            open_archive(local)?
                .entries()?
                .into_iter()
                .map(|e| (safe_name(&e.inner).ok(), e))
                .collect(),
        );
        let mut listings = self.listings.lock();
        if listings.map.len() >= LISTINGS {
            let oldest = listings
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                listings.map.remove(&oldest);
            }
        }
        listings.clock += 1;
        let now = listings.clock;
        listings.map.insert(key.clone(), (listing.clone(), now));
        Ok(listing)
    }
    /// Identifies `path` (an archive or an entry) by the outermost archive as it is now.
    fn key(&self, path: &VPath) -> Result<CacheKey> {
        let mut outer = path.clone();
        while let Some((parent, _)) = outer.split_archive() {
            outer = parent;
        }
        let meta = self.provider(&outer)?.stat(&outer)?;
        let inner = path
            .path
            .get(outer.path.len() + 2..)
            .unwrap_or("")
            .to_owned();
        Ok(CacheKey {
            outer,
            modified: meta.modified,
            size: meta.size,
            inner,
        })
    }
    /// Copies one entry into the cache (nested archives recurse) and pins it.
    fn materialise(&self, path: &VPath) -> Result<Pinned> {
        let key = self.key(path)?;
        self.cache.get_or_extract(&key, |destination| {
            let opened = self.open(path)?;
            let item = opened
                .find()
                .with_context(|| format!("archive entry not found: {}", path.display()))?;
            anyhow::ensure!(!item.is_dir, "cannot materialise a folder");
            anyhow::ensure!(!item.encrypted, "password-protected archive entry");
            anyhow::ensure!(
                item.size <= MATERIALISE_LIMIT,
                "TooLarge: archive entries over 1 GiB are not materialised"
            );
            let mut output = File::create(destination)?;
            with_entry(&opened.local, &item.inner, |input| {
                io::copy(input, &mut output)?;
                Ok(())
            })
        })
    }
}
fn entry(
    outer: &VPath,
    inner: &str,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
    encrypted: bool,
) -> Entry {
    let path = VPath::join_archive(outer, inner);
    let name = path.name().to_owned();
    let ext = Path::new(&name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    Entry {
        path,
        hidden: name.starts_with('.'),
        name,
        kind: if is_dir { Kind::Dir } else { Kind::File },
        size,
        modified,
        is_link: false,
        encrypted,
        ext,
    }
}
const READ_ONLY: &str = "archives are read-only in this version";
impl Provider for ArchiveProvider {
    fn scheme(&self) -> &'static str {
        "archive"
    }
    fn caps(&self) -> Caps {
        Caps::default()
    }
    /// Unsafe names (`..`, absolute, drive letters) are left out; folders that exist only
    /// as a prefix of deeper entries are synthesised.
    fn list(&self, path: &VPath) -> Result<Vec<Entry>> {
        let opened = self.open(path)?;
        let prefix = if opened.inner.is_empty() {
            String::new()
        } else {
            format!("{}/", opened.inner)
        };
        let mut children = BTreeMap::new();
        for (name, item) in opened.entries.iter() {
            let Some(rest) = name.as_deref().and_then(|n| n.strip_prefix(&prefix)) else {
                continue;
            };
            let (child, deeper) = rest
                .split_once('/')
                .map_or((rest, false), |(c, _)| (c, true));
            let is_dir = deeper || item.is_dir;
            let value = entry(
                &opened.outer,
                &format!("{prefix}{child}"),
                is_dir,
                if is_dir { 0 } else { item.size },
                if deeper { None } else { item.modified },
                !is_dir && item.encrypted,
            );
            children
                .entry(child.to_owned())
                .and_modify(|seen: &mut Entry| {
                    // An explicit folder entry carries the real mtime.
                    if !deeper && is_dir {
                        *seen = value.clone();
                    }
                })
                .or_insert(value);
        }
        Ok(children.into_values().collect())
    }
    fn stat(&self, path: &VPath) -> Result<Entry> {
        let opened = self.open(path)?;
        let (outer, inner) = (&opened.outer, &opened.inner);
        if inner.is_empty() {
            return Ok(entry(outer, "", true, 0, None, false));
        }
        if let Some(item) = opened.find() {
            return Ok(entry(
                outer,
                inner,
                item.is_dir,
                item.size,
                item.modified,
                item.encrypted,
            ));
        }
        let folder = format!("{inner}/");
        if opened
            .entries
            .iter()
            .any(|(name, _)| name.as_deref().is_some_and(|n| n.starts_with(&folder)))
        {
            return Ok(entry(outer, inner, true, 0, None, false));
        }
        bail!("archive entry not found: {}", path.display())
    }
    fn read(&self, path: &VPath) -> Result<Box<dyn Read + Send>> {
        let opened = self.open(path)?;
        let item = opened
            .find()
            .filter(|e| !e.is_dir)
            .with_context(|| format!("archive file not found: {}", path.display()))?;
        anyhow::ensure!(!item.encrypted, "password-protected archive entry");
        let raw = item.inner.clone();
        Ok(read_entry(opened.local, raw, opened.pin))
    }
    /// Materialises one entry into the cache. The path stays valid until evicted: open it
    /// promptly, never persist it.
    fn local_copy(&self, path: &VPath) -> Result<PathBuf> {
        let (outer, inner) = path.split_archive().context("not an archive path")?;
        if inner.trim_matches('/').is_empty() {
            return self.fetch(&outer);
        }
        Ok(self.materialise(path)?.path().to_path_buf())
    }
    fn write(&self, _: &VPath) -> Result<Box<dyn Write + Send>> {
        bail!(READ_ONLY)
    }
    fn mkdir(&self, _: &VPath) -> Result<()> {
        bail!(READ_ONLY)
    }
    fn rename(&self, _: &VPath, _: &VPath) -> Result<()> {
        bail!(READ_ONLY)
    }
    fn remove(&self, _: &VPath) -> Result<()> {
        bail!(READ_ONLY)
    }
    /// The whole entry table is read; nothing is cut off.
    fn list_complete(&self, dir: &VPath) -> anyhow::Result<Vec<Entry>> {
        self.list(dir)
    }
    fn remove_kind(&self) -> crate::RemoveKind {
        crate::RemoveKind::Permanent
    }
}

/// Runs `work` on a thread and reads what it writes. At most two 64 KiB chunks are in
/// flight; dropping the reader stops the producer on its next write; errors arrive as IO
/// errors, and so does a producer that ends without saying it is done (a panic).
fn stream(
    work: impl FnOnce(&mut dyn Write) -> Result<()> + Send + 'static,
) -> Box<dyn Read + Send> {
    let (tx, rx) = mpsc::sync_channel(2);
    std::thread::spawn(move || {
        let mut output = StreamWriter(tx.clone());
        let _ = tx.send(match work(&mut output) {
            Ok(()) => Ok(None),
            Err(error) => Err(io::Error::other(format!("{error:#}"))),
        });
    });
    Box::new(StreamReader {
        rx,
        pending: io::Cursor::new(Vec::new()),
        done: false,
    })
}
/// `Some(chunk)` carries data, `None` says the producer finished cleanly.
type Chunk = io::Result<Option<Vec<u8>>>;
struct StreamWriter(mpsc::SyncSender<Chunk>);
impl Write for StreamWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = bytes.len().min(65536);
        if n > 0 {
            self.0
                .send(Ok(Some(bytes[..n].to_vec())))
                .map_err(|_| io::ErrorKind::BrokenPipe)?;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct StreamReader {
    rx: mpsc::Receiver<Chunk>,
    pending: io::Cursor<Vec<u8>>,
    done: bool,
}
impl Read for StreamReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.pending.read(bytes)?;
            if n > 0 || self.done {
                return Ok(n);
            }
            match self.rx.recv() {
                Ok(Ok(Some(chunk))) => self.pending = io::Cursor::new(chunk),
                Ok(Ok(None)) => self.done = true,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(io::Error::other("archive decoder stopped unexpectedly"));
                }
            }
        }
    }
}

/// Normalises an archive name to `a/b/c` (`\` and `/` both separate, `.` and empty parts
/// dropped) and refuses anything that could leave an extraction folder or misbehave on
/// Windows: `..`, absolute paths, drive letters and ADS (`:`), NUL and other control
/// characters, `<>"|?*`, trailing dots/spaces and device names (CON, `con .txt`, COM¹,
/// CONIN$…). A deliberately portable subset, refused on every OS, so an archive extracted
/// anywhere stays safe to copy to Windows.
pub fn safe_name(name: &str) -> Result<String> {
    let name = name.replace('\\', "/");
    anyhow::ensure!(
        !name.starts_with('/')
            && !name.contains([':', '<', '>', '"', '|', '?', '*'])
            && !name.chars().any(char::is_control),
        "unsafe archive path: {name}"
    );
    let parts: Vec<&str> = name
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    anyhow::ensure!(!parts.is_empty(), "empty archive path");
    for part in &parts {
        // Windows ignores everything from the first dot and trailing spaces before it.
        let stem = part
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end_matches(' ')
            .to_uppercase();
        let numbered = |prefix: &str| {
            stem.strip_prefix(prefix).is_some_and(|n| {
                let mut chars = n.chars();
                matches!(
                    (chars.next(), chars.next()),
                    (Some('0'..='9' | '¹' | '²' | '³'), None)
                )
            })
        };
        let device = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || numbered("COM")
            || numbered("LPT");
        anyhow::ensure!(
            *part != ".." && !part.ends_with(['.', ' ']) && !device,
            "unsafe archive path: {name}"
        );
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::{safe_name, stream};
    use std::io::Read;
    #[test]
    fn safe_names() {
        assert_eq!(safe_name("./a//b/").unwrap(), "a/b");
        assert_eq!(safe_name("dir\\x.txt").unwrap(), "dir/x.txt");
        assert_eq!(safe_name("console.txt").unwrap(), "console.txt");
        assert_eq!(safe_name("com10").unwrap(), "com10");
        for bad in [
            "../x",
            "a/../../x",
            "/etc/x",
            "C:/x",
            "C:x",
            "a/b:s",
            "\\\\srv\\x",
            "a/CON.txt",
            "a/b.",
            "a/b ",
            ".",
            "",
            "lpt1",
            "con .txt",
            "a/aux  .tar.gz",
            "COM¹",
            "lpt³.log",
            "CONIN$",
            "conout$.txt",
            "a<b",
            "a>b",
            "a|b",
            "a?b",
            "a*b",
            "a\"b",
            "a\tb",
            "a\nb",
        ] {
            assert!(safe_name(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn a_panicking_decoder_is_an_error_not_eof() {
        let mut body = Vec::new();
        let error = stream(|out| {
            out.write_all(b"partial")?;
            panic!("decoder bug")
        })
        .read_to_end(&mut body)
        .unwrap_err();
        assert!(error.to_string().contains("unexpectedly"), "{error}");
        let mut body = Vec::new();
        stream(|out| Ok(out.write_all(b"whole")?))
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(body, b"whole");
    }
}
