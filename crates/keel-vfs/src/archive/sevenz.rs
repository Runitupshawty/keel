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
    archive: sevenz_rust2::Archive,
}
impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.into(),
            archive: sevenz_rust2::Archive::open(path)?,
        })
    }
}
impl ArchiveReader for Reader {
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>> {
        let archive = &self.archive;
        Ok(archive
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| !file.is_anti_item())
            .map(|(i, file)| {
                let encrypted = archive
                    .stream_map
                    .file_block_index
                    .get(i)
                    .copied()
                    .flatten()
                    .and_then(|block| archive.blocks.get(block))
                    .is_some_and(|block| block.coders.iter().any(|c| c.encoder_method_id() == AES));
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
    fn local(&self) -> &Path {
        &self.path
    }
    /// Decodes only the folders (blocks) holding a wanted file. Inside a block every file
    /// is read to its end, wanted or not, because the next one starts where it stops: in a
    /// solid archive (7-Zip's default) one block holds many files.
    fn each_file(
        &mut self,
        want: &dyn Fn(&str) -> bool,
        each: &mut dyn FnMut(&str, u64, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        let wanted = |file: &sevenz_rust2::ArchiveEntry| {
            !file.is_directory() && !file.is_anti_item() && want(&file.name)
        };
        let mut source = File::open(&self.path)?;
        let password = sevenz_rust2::Password::empty();
        let mut failure = None;
        for folder in 0..self.archive.blocks.len() {
            let block =
                sevenz_rust2::BlockDecoder::new(1, folder, &self.archive, &password, &mut source);
            if !block.entries().iter().any(wanted) {
                continue;
            }
            let finished = block.for_each_entries(&mut |file, input| {
                if wanted(file) {
                    if let Err(e) = each(&file.name, file.size, input) {
                        failure = Some(e);
                        return Ok(false);
                    }
                }
                io::copy(input, &mut io::sink())
                    .map_err(|e| sevenz_rust2::Error::Io(e, "skip entry".into()))?;
                Ok(true)
            })?;
            if !finished {
                break;
            }
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        // Empty files belong to no block.
        for (i, file) in self.archive.files.iter().enumerate() {
            if self.archive.stream_map.file_block_index[i].is_none() && wanted(file) {
                each(&file.name, 0, &mut io::empty())?;
            }
        }
        Ok(())
    }
}

/// Writes the old archive entries (minus replaced names) and then the new ones into
/// `job.out`. Old entries are decoded and re-encoded (LZMA2, one block per file, so no
/// solid block ever has to be rewritten); encrypted archives are refused.
pub(crate) fn rewrite(job: crate::ops::Rewrite<'_>) -> Result<()> {
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
    use std::{collections::HashMap, io::Write, sync::atomic::Ordering, time::SystemTime};
    let crate::ops::Rewrite {
        old,
        files,
        out,
        state,
        progress,
        cancel,
    } = job;
    let cancelled = || {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
        Ok(())
    };
    let replaced: std::collections::HashSet<String> = files
        .keys()
        .map(|k| k.trim_end_matches('/').to_owned())
        .collect();
    let entry = |name: &str, dir: bool, modified: Option<SystemTime>| {
        let mut e = ArchiveEntry::new();
        e.name = name.to_owned();
        e.is_directory = dir;
        e.has_stream = !dir;
        if let Some(Ok(date)) = modified.map(TryInto::try_into) {
            e.last_modified_date = date;
            e.has_last_modified_date = true;
        }
        e
    };
    let mut writer = ArchiveWriter::new(io::BufWriter::new(&mut *out))?;
    writer.set_encrypt_header(false);
    if let Some(old) = old {
        let mut reader = Reader::open(old)?;
        let entries = reader.entries()?;
        anyhow::ensure!(
            !entries.iter().any(|e| e.encrypted),
            "cannot add to an encrypted 7z archive"
        );
        let modified: HashMap<&str, Option<SystemTime>> = entries
            .iter()
            .map(|e| (e.inner.as_str(), e.modified))
            .collect();
        for e in entries.iter().filter(|e| e.is_dir) {
            cancelled()?;
            if !replaced.contains(e.inner.trim_end_matches('/')) {
                writer.push_archive_entry::<&[u8]>(entry(&e.inner, true, e.modified), None)?;
            }
        }
        let mut scratch = state.clone();
        reader.visit(
            &|raw| !replaced.contains(raw.trim_end_matches('/')),
            &mut |raw, body| {
                cancelled()?;
                let e = entry(raw, false, modified.get(raw).copied().flatten());
                let feed = crate::ops::Feed {
                    inner: body,
                    state: &mut scratch,
                    progress: &|_| {},
                    cancel,
                };
                writer.push_archive_entry(e, Some(io::BufReader::with_capacity(64 << 10, feed)))?;
                Ok(())
            },
        )?;
    }
    for (name, (path, meta)) in files {
        cancelled()?;
        state.current = name.clone();
        let e = ArchiveEntry::from_path(&path, name.trim_end_matches('/').to_owned());
        if meta.is_dir() {
            writer.push_archive_entry::<&[u8]>(e, None)?;
        } else {
            let mut file = File::open(&path)?;
            let feed = crate::ops::Feed {
                inner: &mut file,
                state: &mut *state,
                progress,
                cancel,
            };
            writer.push_archive_entry(e, Some(io::BufReader::with_capacity(64 << 10, feed)))?;
        }
        state.done_items += 1;
        progress(state.clone());
    }
    writer.finish()?.flush()?;
    Ok(())
}
