//! Staged writes: open -> write -> close (publish) or abort.
//!
//! A program writing through the mount writes into a spool, never into the target. For a
//! local target the spool is `<name>.keel-partial-<pid>-<n>` next to it (same volume, so
//! publishing is one rename); for a remote target it is a temp file that publishing uploads
//! to `<name>.keel-partial-<pid>-<n>` next to the target and then renames over it. Until
//! then other programs see the old file (or none); an aborted write leaves no file behind
//! (staging names are hidden by the mount and swept by `keel_vfs::ops` after a day).

use keel_vfs::{Provider, VPath};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub(crate) enum Spool {
    /// Next to a local target.
    Beside { file: File, path: PathBuf },
    /// For a remote target: uploaded on publish.
    Temp(tempfile::NamedTempFile),
}

impl Spool {
    fn file(&mut self) -> &mut File {
        match self {
            Spool::Beside { file, .. } => file,
            Spool::Temp(t) => t.as_file_mut(),
        }
    }
}

pub(crate) enum State {
    Open(Spool),
    Published,
    Aborted,
}

pub(crate) struct Staged {
    target: VPath,
    pub(crate) state: State,
}

fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "the staged write is already closed",
    )
}

/// An `io::Error` for a provider error, keeping the OS error or kind when there is one.
pub(crate) fn io_err(e: anyhow::Error) -> io::Error {
    match e.chain().find_map(|c| c.downcast_ref::<io::Error>()) {
        Some(inner) => match inner.raw_os_error() {
            Some(code) => io::Error::from_raw_os_error(code),
            None => io::Error::new(inner.kind(), format!("{e:#}")),
        },
        None => io::Error::other(format!("{e:#}")),
    }
}

impl Staged {
    /// Starts a write to `target`. `keep`: start from the target's current content (when
    /// it exists) rather than empty. Remote spools go to `spool_dir`.
    pub(crate) fn begin(
        provider: &dyn Provider,
        target: &VPath,
        keep: bool,
        spool_dir: &Path,
    ) -> io::Result<Staged> {
        let spool = match target.to_local_path() {
            Some(local) => {
                let name = local
                    .file_name()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
                let path =
                    local.with_file_name(keel_vfs::ops::partial_name(&name.to_string_lossy()));
                let exists = keep && fs::metadata(&local).is_ok_and(|m| m.is_file());
                if exists {
                    fs::copy(&local, &path)?;
                }
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(!exists)
                    .open(&path);
                match file {
                    Ok(file) => Spool::Beside { file, path },
                    Err(e) => {
                        if exists {
                            let _ = fs::remove_file(&path);
                        }
                        return Err(e);
                    }
                }
            }
            None => {
                fs::create_dir_all(spool_dir)?;
                let mut tmp = tempfile::Builder::new()
                    .prefix("keel-mount-")
                    .tempfile_in(spool_dir)?;
                // Only a missing target starts empty: a failed read must not truncate.
                let exists = keep
                    && match provider.stat(target) {
                        Ok(e) => e.kind == keel_vfs::Kind::File,
                        Err(e) => {
                            let e = io_err(e);
                            if e.kind() != io::ErrorKind::NotFound {
                                return Err(e);
                            }
                            false
                        }
                    };
                if exists {
                    let mut r = provider.read(target).map_err(io_err)?;
                    io::copy(&mut r, tmp.as_file_mut())?;
                }
                Spool::Temp(tmp)
            }
        };
        Ok(Staged {
            target: target.clone(),
            state: State::Open(spool),
        })
    }

    fn spool(&mut self) -> io::Result<&mut File> {
        match &mut self.state {
            State::Open(s) => Ok(s.file()),
            _ => Err(closed()),
        }
    }

    pub(crate) fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<usize> {
        let f = self.spool()?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
        Ok(data.len())
    }

    pub(crate) fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let f = self.spool()?;
        f.seek(SeekFrom::Start(offset))?;
        let mut n = 0;
        while n < buf.len() {
            match f.read(&mut buf[n..])? {
                0 => break,
                k => n += k,
            }
        }
        Ok(n)
    }

    pub(crate) fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.spool()?.set_len(len)
    }

    pub(crate) fn len(&mut self) -> io::Result<u64> {
        Ok(self.spool()?.metadata()?.len())
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(self.state, State::Open(_))
    }

    /// Places the spool at the target atomically (replacing it). On failure nothing visible
    /// changes and the written data is kept: a local spool stays as its `.keel-partial`
    /// file, a remote one is kept in the spool folder (both logged).
    pub(crate) fn publish(&mut self, provider: &dyn Provider) -> io::Result<()> {
        let spool = match std::mem::replace(&mut self.state, State::Aborted) {
            State::Open(spool) => spool,
            other => {
                self.state = other;
                return Err(closed());
            }
        };
        match spool {
            Spool::Beside { file, path } => {
                let result = file.sync_all().and_then(|()| {
                    drop(file);
                    let local = self.target.to_local_path().expect("local target");
                    fs::rename(&path, &local)
                });
                if let Err(e) = &result {
                    tracing::error!(
                        "mount: could not save {} ({e}); the data is in {}",
                        self.target.display(),
                        path.display()
                    );
                }
                result?;
            }
            Spool::Temp(mut tmp) => {
                let partial = self
                    .target
                    .parent()
                    .map(|dir| dir.join(&keel_vfs::ops::partial_name(self.target.name())));
                let result = partial
                    .as_ref()
                    .ok_or_else(|| io::Error::other("the target has no folder"))
                    .and_then(|partial| upload(provider, tmp.as_file_mut(), partial, &self.target));
                if let Err(e) = &result {
                    if let Some(p) = &partial {
                        let _ = provider.remove(p);
                    }
                    match tmp.keep() {
                        Ok((_, kept)) => tracing::error!(
                            "mount: could not save {} ({e}); the data is in {}",
                            self.target.display(),
                            kept.display()
                        ),
                        Err(k) => tracing::error!(
                            "mount: could not save {} ({e}); the data is lost ({k})",
                            self.target.display()
                        ),
                    }
                }
                result?;
            }
        }
        self.state = State::Published;
        Ok(())
    }

    /// Drops the written data; the target is untouched and no spool is left.
    pub(crate) fn abort(&mut self) {
        if let State::Open(Spool::Beside { path, .. }) =
            std::mem::replace(&mut self.state, State::Aborted)
        {
            let _ = fs::remove_file(path);
        }
        // A `Temp` spool deletes itself on drop.
    }
}

fn upload(
    provider: &dyn Provider,
    spool: &mut File,
    partial: &VPath,
    target: &VPath,
) -> io::Result<()> {
    spool.seek(SeekFrom::Start(0))?;
    let mut w = provider.create_new(partial).map_err(io_err)?;
    io::copy(spool, &mut w)?;
    w.flush()?;
    drop(w);
    provider.rename_replace(partial, target).map_err(io_err)
}

impl Drop for Staged {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn local_write_is_invisible_until_published() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("report.txt");
        let p = keel_vfs::LocalProvider;
        let mut s = Staged::begin(&p, &VPath::local(&target), true, dir.path()).unwrap();
        s.write_at(0, b"hello").unwrap();
        s.write_at(5, b" world").unwrap();
        assert!(!target.exists(), "nothing at the target before close");
        let listed = names(dir.path());
        assert_eq!(listed.len(), 1);
        assert!(keel_vfs::ops::is_partial(&listed[0]), "{listed:?}");
        let mut buf = [0; 5];
        assert_eq!(s.read_at(6, &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"world");
        s.publish(&p).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"hello world");
        assert_eq!(names(dir.path()), vec!["report.txt"]);
        assert!(s.write_at(0, b"x").is_err(), "no writes after close");
        assert!(s.publish(&p).is_err(), "published once");
    }

    #[test]
    fn local_edit_keeps_the_old_file_until_published_and_abort_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.bin");
        fs::write(&target, b"0123456789").unwrap();
        let p = keel_vfs::LocalProvider;
        let v = VPath::local(&target);
        let mut s = Staged::begin(&p, &v, true, dir.path()).unwrap();
        s.write_at(2, b"ab").unwrap();
        s.set_len(6).unwrap();
        assert_eq!(s.len().unwrap(), 6);
        assert_eq!(fs::read(&target).unwrap(), b"0123456789");
        s.publish(&p).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"01ab45");

        let mut s = Staged::begin(&p, &v, false, dir.path()).unwrap();
        assert_eq!(s.len().unwrap(), 0, "truncating open starts empty");
        s.write_at(0, b"half").unwrap();
        s.abort();
        assert!(!s.is_open());
        assert!(s.write_at(0, b"x").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"01ab45");
        assert_eq!(names(dir.path()), vec!["a.bin"]);

        // Dropped without a close (a crashed handle): the same as abort.
        let fresh = VPath::local(dir.path().join("new.txt"));
        let mut s = Staged::begin(&p, &fresh, true, dir.path()).unwrap();
        s.write_at(0, b"x").unwrap();
        drop(s);
        assert_eq!(names(dir.path()), vec!["a.bin"]);
    }

    fn memory_cloud() -> Arc<dyn Provider> {
        let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
        let account = keel_vfs::CloudAccount {
            id: "mem".into(),
            label: "Memory".into(),
            kind: keel_vfs::CloudKind::S3,
            root: None,
            client_id_override: None,
            s3: None,
            webdav: None,
        };
        Arc::new(
            keel_vfs::CloudProvider::with_operator(account, op, crossbeam_channel::unbounded().0)
                .unwrap(),
        )
    }

    fn get(p: &dyn Provider, v: &VPath) -> Vec<u8> {
        let mut out = Vec::new();
        p.read(v).unwrap().read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn remote_write_uploads_on_publish_only() {
        let spool = tempfile::tempdir().unwrap();
        let p = memory_cloud();
        let dir = VPath::parse("cloud://mem/docs").unwrap();
        p.mkdir(&dir).unwrap();
        let target = dir.join("notes.txt");
        let mut w = p.write(&target).unwrap();
        w.write_all(b"old notes").unwrap();
        w.flush().unwrap();
        drop(w);

        let mut s = Staged::begin(&*p, &target, true, spool.path()).unwrap();
        assert_eq!(s.len().unwrap(), 9, "starts from the remote content");
        s.write_at(0, b"new").unwrap();
        assert_eq!(get(&*p, &target), b"old notes");
        s.publish(&*p).unwrap();
        assert_eq!(get(&*p, &target), b"new notes");
        let listed: Vec<_> = p.list(&dir).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(listed, vec!["notes.txt"]);
        assert_eq!(
            fs::read_dir(spool.path()).unwrap().count(),
            0,
            "spool removed"
        );

        let fresh = dir.join("draft.txt");
        let mut s = Staged::begin(&*p, &fresh, true, spool.path()).unwrap();
        s.write_at(0, b"draft").unwrap();
        s.abort();
        assert!(p.stat(&fresh).is_err(), "an aborted write leaves no file");
        assert_eq!(fs::read_dir(spool.path()).unwrap().count(), 0);
    }
}
