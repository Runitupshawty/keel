use super::{ArchiveEntry, ArchiveReader};
use anyhow::{Context, Result};
use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

pub(super) struct Reader {
    path: PathBuf,
    archive: ::zip::ZipArchive<BufReader<File>>,
}
impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.into(),
            archive: ::zip::ZipArchive::new(BufReader::new(File::open(path)?))?,
        })
    }
}
impl ArchiveReader for Reader {
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>> {
        let mut entries = Vec::with_capacity(self.archive.len());
        for i in 0..self.archive.len() {
            // `by_index_raw` decodes nothing (it only locates the local header).
            let file = self.archive.by_index_raw(i)?;
            if file.is_symlink() || !(file.is_dir() || file.is_file()) {
                continue;
            }
            entries.push(ArchiveEntry {
                inner: file.name().into(),
                is_dir: file.is_dir(),
                size: file.size(),
                modified: file
                    .last_modified()
                    .and_then(|t| time::OffsetDateTime::try_from(t).ok())
                    .map(SystemTime::from),
                encrypted: file.encrypted(),
            });
        }
        Ok(entries)
    }
    fn read(&mut self, inner: &str) -> Result<Box<dyn Read + Send>> {
        let index = self
            .archive
            .index_for_name(inner)
            .context("archive entry not found")?;
        {
            let file = self.archive.by_index_raw(index)?;
            anyhow::ensure!(file.enclosed_name().is_some(), "unsafe zip entry name");
            anyhow::ensure!(file.is_file() && !file.is_symlink(), "not a regular file");
            anyhow::ensure!(!file.encrypted(), "password-protected archive entry");
        }
        // `ZipFile` borrows its archive, so the body streams from a second handle.
        let path = self.path.clone();
        Ok(super::stream(move |out| {
            let mut archive = ::zip::ZipArchive::new(BufReader::new(File::open(path)?))?;
            io::copy(&mut archive.by_index(index)?, out)?;
            Ok(())
        }))
    }
}
