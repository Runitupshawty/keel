use super::{ArchiveEntry, ArchiveReader};
use anyhow::{Context, Result};
use std::{
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

#[derive(Clone, Copy)]
pub(super) enum Compression {
    Plain,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}
pub(super) struct Reader {
    path: PathBuf,
    compression: Compression,
}
impl Reader {
    pub fn new(path: &Path, compression: Compression) -> Self {
        Self {
            path: path.into(),
            compression,
        }
    }
    /// The decompressed tar stream (pure-Rust decoders).
    fn input(&self) -> Result<Box<dyn Read + Send>> {
        let file = BufReader::new(File::open(&self.path)?);
        Ok(match self.compression {
            Compression::Plain => Box::new(file),
            Compression::Gzip => Box::new(flate2::bufread::MultiGzDecoder::new(file)),
            Compression::Bzip2 => Box::new(bzip2::bufread::MultiBzDecoder::new(file)),
            // lzma-rs only decodes into a writer; `stream` turns that back into a reader.
            Compression::Xz => super::stream(move |mut out| {
                lzma_rs::xz_decompress(&mut { file }, &mut out)
                    .map_err(|e| anyhow::anyhow!("xz: {e:?}"))
            }),
            // ponytail: first zstd frame only; multi-frame (pzstd) tarballs end early.
            Compression::Zstd => Box::new(ruzstd::decoding::StreamingDecoder::new(file)?),
        })
    }
}
fn name(entry: &::tar::Entry<'_, impl Read>) -> String {
    String::from_utf8_lossy(&entry.path_bytes()).into_owned()
}
fn wanted(kind: ::tar::EntryType) -> bool {
    kind.is_file() || kind.is_dir()
}
fn metadata<R: Read>(entries: ::tar::Entries<'_, R>) -> Result<Vec<ArchiveEntry>> {
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let header = entry.header();
        if !wanted(header.entry_type()) {
            continue;
        }
        out.push(ArchiveEntry {
            inner: name(&entry),
            is_dir: header.entry_type().is_dir(),
            size: entry.size(),
            modified: header
                .mtime()
                .ok()
                .and_then(|t| UNIX_EPOCH.checked_add(Duration::from_secs(t))),
            encrypted: false,
        });
    }
    Ok(out)
}
/// The tar crate stops at the first zero block and, silently, at a clean end of input on a
/// block boundary; only the second zero block tells a whole archive from a truncated one.
fn end_marker(mut rest: impl Read) -> Result<()> {
    let mut block = [0; 512];
    anyhow::ensure!(
        rest.read_exact(&mut block).is_ok() && block.iter().all(|&b| b == 0),
        "truncated archive (no end marker)"
    );
    Ok(())
}
impl ArchiveReader for Reader {
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>> {
        if matches!(self.compression, Compression::Plain) {
            // Seeks past bodies instead of reading them.
            let mut archive = ::tar::Archive::new(File::open(&self.path)?);
            let out = metadata(archive.entries_with_seek()?)?;
            end_marker(archive.into_inner())?;
            return Ok(out);
        }
        // Compressed tar has no index: reaching each header decompresses the bytes before
        // it, but no body is kept.
        let mut archive = ::tar::Archive::new(self.input()?);
        let out = metadata(archive.entries()?)?;
        end_marker(archive.into_inner())?;
        Ok(out)
    }
    fn local(&self) -> &Path {
        &self.path
    }
    /// One sequential pass; the tar crate skips unread bodies itself.
    fn each_file(
        &mut self,
        want: &dyn Fn(&str) -> bool,
        each: &mut dyn FnMut(&str, u64, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        let mut archive = ::tar::Archive::new(self.input()?);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let name = name(&entry);
            if entry.header().entry_type().is_file() && want(&name) {
                each(&name, entry.size(), &mut entry)?;
            }
        }
        end_marker(archive.into_inner())
    }
}

/// Writes the old tar entries (minus replaced names) and then the new ones into
/// `job.out`, gzip-compressed when the old file was (or, for a new file, when `gzip`).
/// Only plain and gzip tars can be changed.
pub(crate) fn rewrite(job: crate::ops::Rewrite<'_>, gzip: bool) -> Result<()> {
    use std::io::Write;
    let mut gzip = gzip;
    if let Some(old) = job.old {
        let mut magic = [0; 6];
        let n = File::open(old)?.read(&mut magic)?;
        let magic = &magic[..n];
        anyhow::ensure!(
            !(magic.starts_with(b"BZh")
                || magic.starts_with(b"\xfd7zXZ\0")
                || magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])),
            "only plain and gzip tar archives can be changed"
        );
        gzip = magic.starts_with(&[0x1f, 0x8b]);
    }
    let out = std::io::BufWriter::new(&mut *job.out);
    let job = Job {
        old: job.old,
        files: job.files,
        state: job.state,
        progress: job.progress,
        cancel: job.cancel,
    };
    if gzip {
        let encoder = flate2::write::GzEncoder::new(out, flate2::Compression::default());
        build(encoder, job)?.finish()?.flush()?;
    } else {
        build(out, job)?.flush()?;
    }
    Ok(())
}

struct Job<'a> {
    old: Option<&'a Path>,
    files: crate::ops::AddFiles,
    state: &'a mut crate::Progress,
    progress: &'a dyn Fn(crate::Progress),
    cancel: &'a std::sync::atomic::AtomicBool,
}

fn build<W: std::io::Write>(sink: W, job: Job<'_>) -> Result<W> {
    use crate::ops::Feed;
    use std::io::BufReader;
    let Job {
        old,
        files,
        state,
        progress,
        cancel,
    } = job;
    let cancelled = || {
        anyhow::ensure!(
            !cancel.load(std::sync::atomic::Ordering::Relaxed),
            "operation cancelled"
        );
        Ok(())
    };
    let replaced: std::collections::HashSet<String> = files
        .keys()
        .map(|k| k.trim_end_matches('/').to_owned())
        .collect();
    let mut builder = ::tar::Builder::new(sink);
    if let Some(old) = old {
        let mut scratch = state.clone();
        let compression = {
            let mut magic = [0; 2];
            let n = File::open(old)?.read(&mut magic)?;
            if magic[..n] == [0x1f, 0x8b] {
                Compression::Gzip
            } else {
                Compression::Plain
            }
        };
        let mut archive = ::tar::Archive::new(Reader::new(old, compression).input()?);
        for entry in archive.entries()? {
            cancelled()?;
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let key = String::from_utf8_lossy(&entry.path_bytes())
                .trim_end_matches('/')
                .to_owned();
            if replaced.contains(&key) {
                continue;
            }
            let mut header = entry.header().clone();
            let kind = header.entry_type();
            if kind.is_file() || kind.is_dir() {
                let feed = Feed {
                    inner: &mut entry,
                    state: &mut scratch,
                    progress: &|_| {},
                    cancel,
                };
                builder.append_data(&mut header, &path, feed)?;
            } else if kind.is_symlink() || kind.is_hard_link() {
                let target = entry
                    .link_name()?
                    .context("link entry without a target")?
                    .into_owned();
                builder.append_link(&mut header, &path, &target)?;
            } else {
                anyhow::bail!("cannot rewrite a tar with a special entry: {key}");
            }
        }
        end_marker(archive.into_inner())?;
    }
    for (name, (path, meta)) in files {
        cancelled()?;
        state.current = name.clone();
        let mut header = ::tar::Header::new_gnu();
        header.set_metadata(&meta);
        if meta.is_dir() {
            builder.append_data(&mut header, &name, std::io::empty())?;
        } else {
            let mut file = File::open(&path)?;
            let feed = Feed {
                inner: &mut file,
                state: &mut *state,
                progress,
                cancel,
            };
            builder.append_data(&mut header, &name, BufReader::with_capacity(64 << 10, feed))?;
        }
        state.done_items += 1;
        progress(state.clone());
    }
    Ok(builder.into_inner()?)
}
