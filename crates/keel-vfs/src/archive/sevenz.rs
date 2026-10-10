use super::{ArchiveEntry, ArchiveReader};
use anyhow::Result;
use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

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
                    // 7zAES: entries in such a block need a password.
                    .is_some_and(|block| {
                        block.coders.iter().any(|c| {
                            c.encoder_method_id() == sevenz_rust2::EncoderMethod::ID_AES256_SHA256
                        })
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
        let mut failure = None;
        let password = sevenz_rust2::Password::empty();
        for index in 0..self.archive.blocks.len() {
            let block =
                sevenz_rust2::BlockDecoder::new(1, index, &self.archive, &password, &mut source);
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
                io::copy(input, &mut io::sink())?;
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
