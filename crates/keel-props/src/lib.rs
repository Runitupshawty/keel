//! File properties beyond the index, for Everything's property search functions
//! (`perceived-type:`, `shortcut-target:`, `sha256:`, `artist:`, `width:` ...).
//!
//! Keel's own readers work on every platform and read only what they need: file signatures,
//! Windows shortcuts and symlinks, PE version resources, text statistics, streamed hashes and
//! audio tags. On Windows the Windows Property System (`IPropertyStore`) answers the rest,
//! with any canonical property name. A property a platform cannot read is
//! [`Prop::Unknown`], which is not an error.
//!
//! Cloud placeholders (files whose data is not on this device) are never downloaded: their
//! content is not read and the property system is asked only for properties it already has,
//! unless [`Options::allow_recall`] is set.
//!
//! ```no_run
//! let artist = keel_props::get(std::path::Path::new("song.mp3"), "artist")?;
//! # Ok::<(), keel_props::Error>(())
//! ```

pub mod audio;
/// Generated test files (tests and fuzz seeds).
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fixtures;
pub mod functions;
/// Fuzz entry points (`fuzz/` and the in-tree runs; docs/fuzzing.md).
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzz;
pub mod hash;
pub mod lnk;
pub mod pe;
pub mod signature;
pub mod text;
mod value;
#[cfg(windows)]
pub mod winprops;

pub use functions::{lookup, Function, FUNCTIONS};
pub use hash::HashAlgo;
pub use value::{filetime_to_system_time, Prop};

use functions::{AudioField, Conv, PeField, Reader, TextField};
use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Why a property could not be read.
#[derive(Debug)]
pub enum Error {
    /// The file could not be opened or read (missing, no access).
    Io(io::Error),
    /// Not a property function name (nor, on Windows, a canonical property name).
    UnknownFunction(String),
    /// A canonical property name the Windows Property System does not know.
    UnknownProperty(String),
    /// The read was cancelled through [`Options::cancel`].
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::UnknownFunction(n) => write!(f, "unknown property function: {n}"),
            Error::UnknownProperty(n) => write!(f, "unknown property: {n}"),
            Error::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// A cancel flag shared with whoever may stop a long read (a hash of a large file).
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Limits and switches for property reads.
#[derive(Debug, Clone)]
pub struct Options {
    /// Stops hashing and text counting between chunks.
    pub cancel: Cancel,
    /// Files longer than this get no `line-count:` or `word-count:` (unknown); the encoding
    /// comes from their first 64 KiB. Default 64 MiB.
    pub max_text_bytes: u64,
    /// Files longer than this are not hashed (unknown). Default: no limit.
    pub max_hash_bytes: u64,
    /// Read the content of cloud placeholders, downloading them. Default false.
    pub allow_recall: bool,
    /// Ask the Windows Property System (Windows only). Default true.
    pub system: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            cancel: Cancel::default(),
            max_text_bytes: 64 << 20,
            max_hash_bytes: u64::MAX,
            allow_recall: false,
            system: true,
        }
    }
}

/// The property `name` of the file at `path`: an Everything function name (`artist`,
/// `sha256:`, `width`; see [`FUNCTIONS`]) or, on Windows, a canonical property name
/// (`System.Photo.FNumber`).
pub fn get(path: &Path, name: &str) -> Result<Prop, Error> {
    get_with(path, name, &Options::default())
}

/// [`get`] with limits and a cancel flag.
pub fn get_with(path: &Path, name: &str, opts: &Options) -> Result<Prop, Error> {
    FileProps::new(path, opts).get(name)
}

/// Several properties of one file: what each reader parses is kept for the next property.
pub struct FileProps<'a> {
    path: PathBuf,
    opts: &'a Options,
    meta: Option<Metadata>,
    head: Option<Vec<u8>>,
    audio: Option<Option<audio::AudioInfo>>,
    pe: Option<Option<pe::VersionInfo>>,
    text: Option<Option<text::TextStats>>,
    hashes: HashMap<HashAlgo, String>,
}

/// Bytes read for `.url` files.
const MAX_URL_FILE: u64 = 64 * 1024;

impl<'a> FileProps<'a> {
    pub fn new(path: &Path, opts: &'a Options) -> Self {
        FileProps {
            path: path.to_owned(),
            opts,
            meta: None,
            head: None,
            audio: None,
            pe: None,
            text: None,
            hashes: HashMap::new(),
        }
    }

    /// See [`get`].
    pub fn get(&mut self, name: &str) -> Result<Prop, Error> {
        if let Some(f) = functions::lookup(name) {
            // A broken symlink still has a target: only the shortcut reader may skip this.
            if f.reader != Reader::ShortcutTarget {
                self.meta()?;
            }
            let own = self.own(f)?;
            if !own.is_unknown() {
                return Ok(own);
            }
            return match f.windows {
                Some(canonical) => match self.system(canonical, f.conv) {
                    // An older Windows without this property: unknown, like other platforms.
                    Err(Error::UnknownProperty(_)) => Ok(Prop::Unknown),
                    other => other,
                },
                None => Ok(Prop::Unknown),
            };
        }
        if functions::is_canonical_name(name) {
            self.meta()?;
            return self.system(name, Conv::Same);
        }
        Err(Error::UnknownFunction(name.to_owned()))
    }

    fn meta(&mut self) -> Result<&Metadata, Error> {
        if self.meta.is_none() {
            self.meta = Some(fs::metadata(&self.path)?);
        }
        Ok(self.meta.as_ref().expect("set above"))
    }

    /// True when the file's content may be read: a regular file that is not a cloud
    /// placeholder (or recall is allowed).
    fn readable(&mut self) -> Result<bool, Error> {
        let allow = self.opts.allow_recall;
        let m = self.meta()?;
        Ok(m.is_file() && (allow || !is_placeholder(m)))
    }

    fn ext(&self) -> String {
        self.path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    }

    fn head(&mut self) -> Result<&[u8], Error> {
        if self.head.is_none() {
            let mut buf = Vec::with_capacity(signature::HEAD_LEN);
            File::open(&self.path)?
                .take(signature::HEAD_LEN as u64)
                .read_to_end(&mut buf)?;
            self.head = Some(buf);
        }
        Ok(self.head.as_deref().unwrap_or(&[]))
    }

    fn signature(&mut self) -> Result<Option<signature::Signature>, Error> {
        if !self.readable()? {
            return Ok(None);
        }
        Ok(signature::detect(self.head()?))
    }

    fn own(&mut self, f: &Function) -> Result<Prop, Error> {
        Ok(match f.reader {
            Reader::None => Prop::Unknown,
            Reader::SignatureId => self
                .signature()?
                .map_or(Prop::Unknown, |s| Prop::Str(s.id.to_owned())),
            Reader::ContentType | Reader::PerceivedType => {
                let want_mime = f.reader == Reader::ContentType;
                match self.signature()? {
                    Some(s) if want_mime => Prop::Str(s.content_type.to_owned()),
                    Some(s) => Prop::Str(s.perceived.to_owned()),
                    None if self.readable()? && signature::looks_like_text(self.head()?) => {
                        Prop::Str(if want_mime { "text/plain" } else { "text" }.to_owned())
                    }
                    None => Prop::Unknown,
                }
            }
            Reader::ShortcutTarget => self.shortcut_target()?,
            Reader::Url => self.url()?,
            Reader::Pe(field) => self.pe_field(field)?,
            Reader::Text(field) => self.text_field(field)?,
            Reader::Hash(algo) => self.hash(algo)?,
            Reader::Audio(field) => self.audio_field(field)?,
        })
    }

    fn shortcut_target(&mut self) -> Result<Prop, Error> {
        let lmeta = fs::symlink_metadata(&self.path)?;
        if lmeta.file_type().is_symlink() {
            let target = fs::read_link(&self.path)?;
            return Ok(Prop::Str(display_path(&target)));
        }
        if !self.readable()? || !self.head()?.starts_with(b"\x4c\0\0\0\x01\x14\x02\0") {
            return Ok(Prop::Unknown);
        }
        let mut buf = Vec::new();
        File::open(&self.path)?
            .take(lnk::MAX_LNK_BYTES)
            .read_to_end(&mut buf)?;
        Ok(Prop::str(lnk::parse(&buf).ok().and_then(|s| s.target())))
    }

    fn url(&mut self) -> Result<Prop, Error> {
        if !self.readable()? || self.ext() != "url" {
            return Ok(Prop::Unknown);
        }
        let mut buf = Vec::new();
        File::open(&self.path)?
            .take(MAX_URL_FILE)
            .read_to_end(&mut buf)?;
        Ok(Prop::str(parse_url_file(&buf)))
    }

    fn version_info(&mut self) -> Result<Option<&pe::VersionInfo>, Error> {
        if self.pe.is_none() {
            let v = if self.readable()? && self.head()?.starts_with(b"MZ") {
                let mut f = File::open(&self.path)?;
                let len = f.metadata()?.len();
                match pe::version_info(&mut f, len) {
                    Ok(v) => Some(v),
                    Err(pe::PeError::Io(e)) => return Err(e.into()),
                    Err(_) => None,
                }
            } else {
                None
            };
            self.pe = Some(v);
        }
        Ok(self.pe.as_ref().and_then(Option::as_ref))
    }

    fn pe_field(&mut self, field: PeField) -> Result<Prop, Error> {
        let Some(v) = self.version_info()? else {
            return Ok(Prop::Unknown);
        };
        let s = match field {
            PeField::FileVersion => v.file_version(),
            PeField::ProductVersion => v.product_version(),
            PeField::ProductName => v.string("ProductName").map(str::to_owned),
            PeField::Company => v.string("CompanyName").map(str::to_owned),
            PeField::Copyright => v.string("LegalCopyright").map(str::to_owned),
            PeField::Trademarks => v.string("LegalTrademarks").map(str::to_owned),
            PeField::Description => v.string("FileDescription").map(str::to_owned),
        };
        Ok(Prop::str(s))
    }

    fn text_stats(&mut self) -> Result<Option<text::TextStats>, Error> {
        if self.text.is_none() {
            let stats = if self.readable()? {
                let len = self.meta()?.len();
                let f = File::open(&self.path)?;
                if len > self.opts.max_text_bytes {
                    let mut head = Vec::new();
                    f.take(text::SNIFF_LEN as u64).read_to_end(&mut head)?;
                    Some(text::TextStats {
                        encoding: text::sniff(&head),
                        lines: None,
                        words: None,
                        truncated: true,
                    })
                } else {
                    Some(text::stats(
                        io::BufReader::new(f),
                        self.opts.max_text_bytes,
                        &self.opts.cancel,
                    )?)
                }
            } else {
                None
            };
            self.text = Some(stats);
        }
        Ok(self.text.flatten())
    }

    fn text_field(&mut self, field: TextField) -> Result<Prop, Error> {
        let Some(s) = self.text_stats()? else {
            return Ok(Prop::Unknown);
        };
        Ok(match field {
            TextField::Encoding => Prop::Str(s.encoding.name().to_owned()),
            TextField::Lines => s.lines.into(),
            TextField::Words => s.words.into(),
        })
    }

    /// Hashes the file with every algorithm in `algos` not yet computed, in one pass.
    /// Returns `Prop::Unknown` entries for files that are not hashed (folders, placeholders,
    /// files over [`Options::max_hash_bytes`]).
    pub fn hashes(&mut self, algos: &[HashAlgo]) -> Result<Vec<Prop>, Error> {
        let todo: Vec<HashAlgo> = algos
            .iter()
            .copied()
            .filter(|a| !self.hashes.contains_key(a))
            .collect();
        if !todo.is_empty() {
            if !self.readable()? || self.meta()?.len() > self.opts.max_hash_bytes {
                return Ok(vec![Prop::Unknown; algos.len()]);
            }
            let f = File::open(&self.path)?;
            let digests = hash::hash_reader(f, &todo, &self.opts.cancel)?;
            self.hashes.extend(todo.into_iter().zip(digests));
        }
        Ok(algos
            .iter()
            .map(|a| {
                self.hashes
                    .get(a)
                    .map_or(Prop::Unknown, |h| Prop::Str(h.clone()))
            })
            .collect())
    }

    fn hash(&mut self, algo: HashAlgo) -> Result<Prop, Error> {
        Ok(self.hashes(&[algo])?.pop().unwrap_or(Prop::Unknown))
    }

    fn audio_info(&mut self) -> Result<Option<&audio::AudioInfo>, Error> {
        if self.audio.is_none() {
            let is_audio = self.readable()?
                && (matches!(
                    self.signature()?.map(|s| s.id),
                    Some("mp3" | "flac" | "wav")
                ) || matches!(self.ext().as_str(), "mp3" | "mp2" | "flac" | "wav"));
            let info = if is_audio {
                let mut f = File::open(&self.path)?;
                let len = f.metadata()?.len();
                Some(audio::read(&mut f, len)?)
            } else {
                None
            };
            self.audio = Some(info);
        }
        Ok(self.audio.as_ref().and_then(Option::as_ref))
    }

    fn audio_field(&mut self, field: AudioField) -> Result<Prop, Error> {
        let Some(a) = self.audio_info()? else {
            return Ok(Prop::Unknown);
        };
        let num = |n: Option<u32>| -> Prop { n.map(u64::from).into() };
        Ok(match field {
            AudioField::Title => Prop::str(a.title.clone()),
            AudioField::Artist => Prop::str(a.artist.clone()),
            AudioField::Album => Prop::str(a.album.clone()),
            AudioField::AlbumArtist => Prop::str(a.album_artist.clone()),
            AudioField::Genre => Prop::str(a.genre.clone()),
            AudioField::Comment => Prop::str(a.comment.clone()),
            AudioField::Composer => Prop::str(a.composer.clone()),
            AudioField::Conductor => Prop::str(a.conductor.clone()),
            AudioField::Publisher => Prop::str(a.publisher.clone()),
            AudioField::EncodedBy => Prop::str(a.encoded_by.clone()),
            AudioField::Subtitle => Prop::str(a.subtitle.clone()),
            AudioField::Language => Prop::str(a.language.clone()),
            AudioField::Mood => Prop::str(a.mood.clone()),
            AudioField::InitialKey => Prop::str(a.initial_key.clone()),
            AudioField::Writer => Prop::str(a.writer.clone()),
            AudioField::PartOfSet => Prop::str(a.part_of_set.clone()),
            AudioField::Format => Prop::str(a.format.map(str::to_owned)),
            AudioField::Track => num(a.track),
            AudioField::Year => a.year.map_or(Prop::Unknown, |y| Prop::Int(i64::from(y))),
            AudioField::Bpm => num(a.bpm),
            AudioField::Compilation => a.compilation.map_or(Prop::Unknown, Prop::Bool),
            AudioField::Length => a.length.map_or(Prop::Unknown, Prop::Duration),
            AudioField::Bitrate => num(a.bitrate),
            AudioField::SampleRate => num(a.sample_rate),
            AudioField::Channels => num(a.channels.map(u32::from)),
            AudioField::BitsPerSample => num(a.bits_per_sample.map(u32::from)),
        })
    }

    #[cfg(windows)]
    fn system(&mut self, canonical: &str, conv: Conv) -> Result<Prop, Error> {
        if !self.opts.system {
            return Ok(Prop::Unknown);
        }
        let fast_only = !self.opts.allow_recall && is_placeholder(self.meta()?);
        Ok(convert(
            winprops::get(&self.path, canonical, fast_only)?,
            conv,
        ))
    }

    #[cfg(not(windows))]
    fn system(&mut self, _canonical: &str, _conv: Conv) -> Result<Prop, Error> {
        Ok(Prop::Unknown)
    }
}

/// Applies a function's unit conversion to a Windows property value.
#[cfg_attr(not(windows), allow(dead_code))]
fn convert(p: Prop, conv: Conv) -> Prop {
    match (conv, p) {
        (Conv::Kbps, Prop::Int(n)) => Prop::Int(n / 1000),
        (Conv::Ticks, Prop::Int(n)) => Prop::Duration(Duration::from_nanos(
            u64::try_from(n).unwrap_or(0).saturating_mul(100),
        )),
        (Conv::PerThousand, Prop::Int(n)) => Prop::Float(n as f64 / 1000.0),
        (Conv::Perceived, Prop::Int(n)) => match n {
            -1 => Prop::Str("folder".into()),
            1 => Prop::Str("text".into()),
            2 => Prop::Str("image".into()),
            3 => Prop::Str("audio".into()),
            4 => Prop::Str("video".into()),
            5 => Prop::Str("compressed".into()),
            6 => Prop::Str("document".into()),
            7 => Prop::Str("system".into()),
            8 => Prop::Str("application".into()),
            9 => Prop::Str("gamemedia".into()),
            10 => Prop::Str("contacts".into()),
            _ => Prop::Unknown,
        },
        (_, p) => p,
    }
}

/// A cloud placeholder whose data is not on this device (reading it would download it):
/// on Windows the recall-on-open, recall-on-data-access or offline attribute, on macOS a
/// dataless file.
pub fn is_placeholder(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const OFFLINE: u32 = 0x1000;
        const RECALL_ON_OPEN: u32 = 0x0004_0000;
        const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        meta.file_attributes() & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        const SF_DATALESS: u32 = 0x4000_0000;
        meta.st_flags() & SF_DATALESS != 0
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = meta;
        false
    }
}

/// A symlink target for display: Windows' `\\?\` prefix dropped from drive paths.
fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        Some(rest) if rest.starts_with("UNC\\") => format!(r"\\{}", &rest[4..]),
        _ => s.into_owned(),
    }
}

/// An Internet shortcut's (`.url`, INI) `URL=` value in its `[InternetShortcut]` section.
pub fn parse_url_file(b: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(b);
    let mut in_section = false;
    for line in text.lines().take(4096) {
        let line = line.trim().trim_start_matches('\u{feff}');
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = section.trim().eq_ignore_ascii_case("InternetShortcut");
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case("URL") {
                let v = v.trim();
                return (!v.is_empty()).then(|| v.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
