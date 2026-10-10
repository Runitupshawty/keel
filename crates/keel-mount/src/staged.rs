//! Staged writes: open -> write -> close (publish) or abort.
//!
//! A program writing through the mount writes into a spool, never into the target. For a
//! local target the spool is `<name>.keel-partial-<pid>-<n>` next to it (same volume, so
//! publishing is one rename); for a remote target it is a temp file that publishing uploads
//! to `<name>.keel-partial-<pid>-<n>` next to the target and then renames over it. Until
//! then other programs see the old file (or none); an aborted write leaves no file behind
//! (staging names are hidden by the mount and swept by `keel_vfs::ops` after a day). A
//! write that cannot be published is kept under `<name> (unsaved <date>).<ext>` (beside a
//! local target, in the spool folder for a remote one), a name nothing sweeps.

use keel_vfs::{Provider, VPath};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Data was written since it was staged (truncating alone does not count).
    pub(crate) written: bool,
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
            written: false,
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
        self.written = true;
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

    /// When the staged copy was last written.
    pub(crate) fn modified(&mut self) -> io::Result<SystemTime> {
        self.spool()?.metadata()?.modified()
    }

    /// The write now lands at `target`: the file, or a folder above it, was renamed. A
    /// local spool moves to stay beside its target (a hidden staging name in the target's
    /// folder); it is found where it was or, when its folder was the one renamed, under
    /// its name in the new folder. A spool that cannot move stays put and is renamed
    /// across folders when the write is published.
    pub(crate) fn retarget(&mut self, target: VPath) {
        if let State::Open(Spool::Beside { path, .. }) = &mut self.state {
            if let (Some(new), Some(name)) = (target.to_local_path(), path.file_name()) {
                let moved = new.with_file_name(name);
                let now = if fs::symlink_metadata(&*path).is_ok() {
                    path.clone()
                } else {
                    moved
                };
                let want = new.with_file_name(keel_vfs::ops::partial_name(target.name()));
                *path = match fs::rename(&now, &want) {
                    Ok(()) => want,
                    Err(_) => now,
                };
            }
        }
        self.target = target;
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(self.state, State::Open(_))
    }

    pub(crate) fn is_published(&self) -> bool {
        matches!(self.state, State::Published)
    }

    /// Places the spool at the target atomically (replacing it). On failure nothing visible
    /// changes, the error is returned and the written data is kept (see [`Staged::keep`]).
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
                    self.keep(&path, e);
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
                    self.keep_temp(tmp, e);
                }
                result?;
            }
        }
        self.state = State::Published;
        Ok(())
    }

    /// Closes the write without publishing it (the source cannot be reached) and keeps the
    /// data like a failed [`Staged::publish`].
    pub(crate) fn keep_unsaved(&mut self, why: &io::Error) {
        match std::mem::replace(&mut self.state, State::Aborted) {
            State::Open(Spool::Beside { file, path }) => {
                drop(file);
                self.keep(&path, why);
            }
            State::Open(Spool::Temp(tmp)) => self.keep_temp(tmp, why),
            other => self.state = other,
        }
    }

    fn keep_temp(&self, tmp: tempfile::NamedTempFile, why: &io::Error) {
        match tmp.keep() {
            Ok((file, kept)) => {
                drop(file);
                self.keep(&kept, why);
            }
            Err(k) => tracing::error!(
                "mount: could not save {} ({why}); the data is lost ({k})",
                self.target.display()
            ),
        }
    }

    /// Renames the spool at `path` to `<name> (unsaved <date>).<ext>` in its folder, a name
    /// nothing sweeps, and logs where the data is.
    fn keep(&self, path: &Path, why: &io::Error) {
        let kept = (1..100)
            .map(|n| path.with_file_name(unsaved_name(self.target.name(), n)))
            .find(|p| fs::symlink_metadata(p).is_err())
            .ok_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists))
            .and_then(|p| fs::rename(path, &p).map(|()| p));
        match kept {
            Ok(p) => tracing::error!(
                "mount: could not save {} ({why}); the data is in {}",
                self.target.display(),
                p.display()
            ),
            Err(e) => tracing::error!(
                "mount: could not save {} ({why}); the data is in {} (not renamed: {e})",
                self.target.display(),
                path.display()
            ),
        }
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

/// `report.txt` -> `report (unsaved 2026-10-10 153000).txt` (UTC; `n` > 1 adds ` n`).
fn unsaved_name(name: &str, n: u32) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Days to a civil date (Howard Hinnant's civil_from_days).
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let t = secs % 86_400;
    let more = if n > 1 {
        format!(" {n}")
    } else {
        String::new()
    };
    format!(
        "{stem} (unsaved {y:04}-{m:02}-{d:02} {:02}{:02}{:02}{more}){ext}",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
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

    #[test]
    fn a_retargeted_write_moves_its_spool_and_lands_under_the_new_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::create_dir(dir.path().join("b")).unwrap();
        let p = keel_vfs::LocalProvider;
        let mut s = Staged::begin(
            &p,
            &VPath::local(dir.path().join("a/x.txt")),
            true,
            dir.path(),
        )
        .unwrap();
        s.write_at(0, b"one ").unwrap();
        // Renamed in its folder, then moved to another one: the spool follows.
        s.retarget(VPath::local(dir.path().join("a/y.txt")));
        s.retarget(VPath::local(dir.path().join("b/z.txt")));
        assert!(names(&dir.path().join("a")).is_empty());
        let spool = names(&dir.path().join("b"));
        assert_eq!(spool.len(), 1);
        assert!(keel_vfs::ops::is_partial(&spool[0]), "{spool:?}");
        assert!(spool[0].starts_with("z.txt"), "{spool:?}");
        s.write_at(4, b"two").unwrap();
        // Its folder renamed under it (the spool moved with the folder). Windows refuses
        // to rename a folder holding an open file, like any other program's.
        let last = match fs::rename(dir.path().join("b"), dir.path().join("c")) {
            Ok(()) => dir.path().join("c"),
            Err(e) => {
                if !cfg!(windows) {
                    panic!("{e}");
                }
                dir.path().join("b")
            }
        };
        s.retarget(VPath::local(last.join("z.txt")));
        s.publish(&p).unwrap();
        assert_eq!(fs::read(last.join("z.txt")).unwrap(), b"one two");
        assert_eq!(names(&last), vec!["z.txt"]);
        assert!(s.modified().is_err(), "closed");
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
