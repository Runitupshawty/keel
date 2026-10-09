use super::{ArchiveEntry, ArchiveReader};
use anyhow::Result;
use std::{
    fs::File,
    io::{BufReader, Read},
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
fn regular(file: &::zip::read::ZipFile<'_>) -> bool {
    !file.is_symlink() && (file.is_dir() || file.is_file())
}
/// Zip stores local wall-clock time without a zone.
fn local_time(stamp: ::zip::DateTime) -> Option<SystemTime> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDate::from_ymd_opt(
        stamp.year().into(),
        stamp.month().into(),
        stamp.day().into(),
    )?
    .and_hms_opt(
        stamp.hour().into(),
        stamp.minute().into(),
        stamp.second().into(),
    )?;
    Some(chrono::Local.from_local_datetime(&naive).earliest()?.into())
}
impl ArchiveReader for Reader {
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>> {
        let mut entries = Vec::with_capacity(self.archive.len());
        for i in 0..self.archive.len() {
            // `by_index_raw` decodes nothing (it only locates the local header).
            let file = self.archive.by_index_raw(i)?;
            if !regular(&file) {
                continue;
            }
            entries.push(ArchiveEntry {
                inner: file.name().into(),
                is_dir: file.is_dir(),
                size: file.size(),
                modified: file.last_modified().and_then(local_time),
                encrypted: file.encrypted(),
            });
        }
        Ok(entries)
    }
    fn local(&self) -> &Path {
        &self.path
    }
    /// One open archive, entries read by index in order.
    fn each_file(
        &mut self,
        want: &dyn Fn(&str) -> bool,
        each: &mut dyn FnMut(&str, u64, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        for i in 0..self.archive.len() {
            let (name, size) = {
                let file = self.archive.by_index_raw(i)?;
                if !regular(&file) || file.is_dir() || !want(file.name()) {
                    continue;
                }
                anyhow::ensure!(!file.encrypted(), "password-protected archive entry");
                (file.name().to_owned(), file.size())
            };
            each(&name, size, &mut self.archive.by_index(i)?)?;
        }
        Ok(())
    }
}
