use super::{ArchiveEntry, ArchiveReader};
use anyhow::{bail, Result};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

/// libunrar only extracts to files, so each body goes through a private temp file.
pub(super) struct Reader {
    path: PathBuf,
}
impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        unrar::Archive::new(path).open_for_listing()?;
        Ok(Self { path: path.into() })
    }
}
fn name(header: &unrar::FileHeader) -> String {
    header.filename.to_string_lossy().replace('\\', "/")
}
impl ArchiveReader for Reader {
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>> {
        let mut out = Vec::new();
        for header in unrar::Archive::new(&self.path).open_for_listing()? {
            let header = header?;
            out.push(ArchiveEntry {
                inner: name(&header),
                is_dir: header.is_directory(),
                size: header.unpacked_size,
                modified: None,
                encrypted: header.is_encrypted(),
            });
        }
        Ok(out)
    }
    fn read(&mut self, inner: &str) -> Result<Box<dyn Read + Send>> {
        let mut archive = unrar::Archive::new(&self.path).open_for_processing()?;
        while let Some(header) = archive.read_header()? {
            if header.entry().is_file() && name(header.entry()) == inner {
                anyhow::ensure!(
                    !header.entry().is_encrypted(),
                    "password-protected archive entry"
                );
                let temp = tempfile::tempdir()?;
                let path = temp.path().join("entry");
                header.extract_to(&path)?;
                return Ok(Box::new(TempReader {
                    file: File::open(path)?,
                    _temp: temp,
                }));
            }
            archive = header.skip()?;
        }
        bail!("archive entry not found: {inner}")
    }
    fn visit(
        &mut self,
        want: &dyn Fn(&str) -> bool,
        each: &mut dyn FnMut(&str, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        let mut archive = unrar::Archive::new(&self.path).open_for_processing()?;
        while let Some(header) = archive.read_header()? {
            let name = name(header.entry());
            archive = if header.entry().is_file() && want(&name) {
                let temp = tempfile::tempdir()?;
                let path = temp.path().join("entry");
                let next = header.extract_to(&path)?;
                each(&name, &mut File::open(&path)?)?;
                next
            } else {
                header.skip()?
            };
        }
        Ok(())
    }
}
struct TempReader {
    file: File,
    _temp: tempfile::TempDir,
}
impl Read for TempReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(bytes)
    }
}
