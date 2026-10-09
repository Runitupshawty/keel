use super::{ArchiveEntry, ArchiveReader};
use anyhow::Result;
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
impl ArchiveReader for Reader {
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>> {
        if matches!(self.compression, Compression::Plain) {
            // Seeks past bodies instead of reading them.
            return metadata(::tar::Archive::new(File::open(&self.path)?).entries_with_seek()?);
        }
        // Compressed tar has no index: reaching each header decompresses the bytes before
        // it, but no body is kept.
        metadata(::tar::Archive::new(self.input()?).entries()?)
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
        for entry in ::tar::Archive::new(self.input()?).entries()? {
            let mut entry = entry?;
            let name = name(&entry);
            if entry.header().entry_type().is_file() && want(&name) {
                each(&name, entry.size(), &mut entry)?;
            }
        }
        Ok(())
    }
}
