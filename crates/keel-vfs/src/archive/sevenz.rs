use super::{ArchiveEntry, ArchiveReader};
use anyhow::Result;
use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

/// 7zAES coder id: entries in such a folder need a password.
const AES: [u8; 4] = [0x06, 0xf1, 0x07, 0x01];

pub(super) struct Reader {
    path: PathBuf,
    archive: sevenz_rust::SevenZReader<File>,
}
impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.into(),
            archive: sevenz_rust::SevenZReader::open(path, Default::default())?,
        })
    }
}
/// Calls `each` for every regular file `want` accepts, decoding the archive once.
fn visit(
    path: &Path,
    want: &dyn Fn(&str) -> bool,
    each: &mut dyn FnMut(&str, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    let mut archive = sevenz_rust::SevenZReader::open(path, Default::default())?;
    let mut failure = None;
    archive.for_each_entries(|entry, input| {
        if entry.is_directory() || entry.is_anti_item() || !want(&entry.name) {
            return Ok(true);
        }
        match each(&entry.name, input) {
            Ok(()) => Ok(true),
            Err(e) => {
                failure = Some(e);
                Ok(false)
            }
        }
    })?;
    failure.map_or(Ok(()), Err)
}
impl ArchiveReader for Reader {
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>> {
        let archive = self.archive.archive();
        Ok(archive
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| !file.is_anti_item())
            .map(|(i, file)| {
                let encrypted = archive
                    .stream_map
                    .file_folder_index
                    .get(i)
                    .copied()
                    .flatten()
                    .and_then(|folder| archive.folders.get(folder))
                    .is_some_and(|folder| {
                        folder
                            .coders
                            .iter()
                            .any(|c| c.decompression_method_id() == AES)
                    });
                ArchiveEntry {
                    inner: file.name.clone(),
                    is_dir: file.is_directory,
                    size: file.size,
                    modified: file
                        .has_last_modified_date
                        .then(|| file.last_modified_date.into()),
                    encrypted,
                }
            })
            .collect())
    }
    fn read(&mut self, inner: &str) -> Result<Box<dyn Read + Send>> {
        let path = self.path.clone();
        let inner = inner.to_owned();
        Ok(super::stream(move |out| {
            let found = std::cell::Cell::new(false);
            // ponytail: decodes earlier folders too; seek straight to the entry's folder
            // if previews of large non-solid 7z archives get slow.
            visit(
                &path,
                &|name| !found.get() && name == inner,
                &mut |_, input| {
                    io::copy(input, out)?;
                    found.set(true);
                    Ok(())
                },
            )?;
            anyhow::ensure!(found.get(), "archive entry not found: {inner}");
            Ok(())
        }))
    }
    fn visit(
        &mut self,
        want: &dyn Fn(&str) -> bool,
        each: &mut dyn FnMut(&str, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        visit(&self.path, want, each)
    }
}
