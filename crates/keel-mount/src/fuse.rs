//! FUSE backend (Linux, macOS with macFUSE; feature `fuse`): an empty folder served by
//! [`MountFs`] in this process. Inode numbers map to mount paths for as long as the mount
//! lives.

use crate::fs::{Attr, MountFs};
use crate::path::MountPath;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    KernelConfig, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const TTL: Duration = Duration::from_secs(1);
const ROOT: u64 = 1;

/// An errno for an `io::Error` (OS errors keep their number).
fn errno(e: &io::Error) -> Errno {
    use io::ErrorKind as K;
    if let Some(code) = e.raw_os_error() {
        return Errno::from_i32(code);
    }
    match e.kind() {
        K::NotFound => Errno::ENOENT,
        K::AlreadyExists => Errno::EEXIST,
        K::PermissionDenied => Errno::EACCES,
        K::DirectoryNotEmpty => Errno::ENOTEMPTY,
        K::IsADirectory => Errno::EISDIR,
        K::NotADirectory => Errno::ENOTDIR,
        K::InvalidInput => Errno::EINVAL,
        K::ResourceBusy => Errno::EBUSY,
        K::NotConnected => Errno::ENOTCONN,
        K::BrokenPipe => Errno::EBADF,
        _ => Errno::EIO,
    }
}

#[derive(Default)]
struct Inodes {
    paths: HashMap<u64, MountPath>,
    inos: HashMap<MountPath, u64>,
    next: u64,
}

impl Inodes {
    fn ino(&mut self, p: &MountPath) -> u64 {
        if p.is_root() {
            return ROOT;
        }
        if let Some(&ino) = self.inos.get(p) {
            return ino;
        }
        self.next = self.next.max(ROOT) + 1;
        self.paths.insert(self.next, p.clone());
        self.inos.insert(p.clone(), self.next);
        self.next
    }

    fn path(&self, ino: u64) -> Option<MountPath> {
        if ino == ROOT {
            return Some(MountPath::root());
        }
        self.paths.get(&ino).cloned()
    }

    /// Moves `from` (and everything below it) to `to`.
    fn rename(&mut self, from: &MountPath, to: &MountPath) {
        let moved: Vec<(u64, MountPath)> = self
            .paths
            .iter()
            .filter_map(|(&ino, p)| p.rebase(from, to).map(|n| (ino, n)))
            .collect();
        for (ino, new) in moved {
            if let Some(old) = self.paths.insert(ino, new.clone()) {
                self.inos.remove(&old);
            }
            if let Some(stale) = self.inos.insert(new, ino) {
                if stale != ino {
                    self.paths.remove(&stale);
                }
            }
        }
    }
}

pub(crate) struct Fuse {
    fs: Arc<MountFs>,
    inodes: Mutex<Inodes>,
    uid: u32,
    gid: u32,
}

impl Fuse {
    fn path(&self, ino: INodeNo) -> Result<MountPath, Errno> {
        self.inodes.lock().path(ino.0).ok_or(Errno::ENOENT)
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Result<MountPath, Errno> {
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        self.path(parent)?.join(name).map_err(|e| errno(&e))
    }

    fn attr(&self, p: &MountPath, a: &Attr) -> FileAttr {
        let ino = self.inodes.lock().ino(p);
        let t = a.modified.unwrap_or(SystemTime::UNIX_EPOCH);
        FileAttr {
            ino: INodeNo(ino),
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind: if a.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if a.is_dir { 0o755 } else { 0o644 },
            nlink: if a.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn entry(&self, p: MountPath, reply: ReplyEntry) {
        match self.fs.lookup(&p) {
            Ok((p, a)) => reply.entry(&TTL, &self.attr(&p, &a), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn done(reply: ReplyEmpty, r: io::Result<()>) {
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }
}

impl Filesystem for Fuse {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // O_TRUNC arrives with open (no separate truncate that would publish an empty file).
        let _ = config.add_capabilities(fuser::InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.child(parent, name) {
            Ok(p) => self.entry(p, reply),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let r = match fh {
            Some(fh) => self
                .fs
                .handle_path(fh.0)
                .and_then(|p| Ok((p, self.fs.handle_attr(fh.0)?))),
            None => self
                .path(ino)
                .map_err(|_| io::Error::from(io::ErrorKind::NotFound))
                .and_then(|p| self.fs.lookup(&p)),
        };
        match r {
            Ok((p, a)) => reply.attr(&TTL, &self.attr(&p, &a)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let path = match self.path(ino) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        if let Some(size) = size {
            // Without a handle (truncate(2)): a short-lived writer publishes at once.
            let r = match fh {
                Some(fh) => self.fs.set_len(fh.0, size),
                None => self.fs.open(&path, true, false).and_then(|(h, _)| {
                    let r = self.fs.set_len(h, size);
                    let closed = self.fs.release(h);
                    r.and(closed)
                }),
            };
            if let Err(e) = r {
                return reply.error(errno(&e));
            }
        }
        // Mode, owner and times are the source's own: accepted and ignored.
        match self.fs.lookup(&path) {
            Ok((p, a)) => reply.attr(&TTL, &self.attr(&p, &a)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        match self.child(parent, name) {
            Ok(p) => match self.fs.mkdir(&p) {
                Ok(()) => self.entry(p, reply),
                Err(e) => reply.error(errno(&e)),
            },
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.child(parent, name) {
            Ok(p) => Self::done(reply, self.fs.remove_file(&p)),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.child(parent, name) {
            Ok(p) => Self::done(reply, self.fs.remove_dir(&p)),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (from, to) = match (self.child(parent, name), self.child(newparent, newname)) {
            (Ok(f), Ok(t)) => (f, t),
            (Err(e), _) | (_, Err(e)) => return reply.error(e),
        };
        #[cfg(target_os = "linux")]
        let replace = !flags.contains(RenameFlags::RENAME_NOREPLACE);
        #[cfg(not(target_os = "linux"))]
        let replace = {
            let _ = flags;
            true
        };
        match self.fs.rename(&from, &to, replace) {
            Ok(()) => {
                self.inodes.lock().rename(&from, &to);
                reply.ok()
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let write = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let truncate = flags.0 & libc::O_TRUNC != 0;
        let r = self
            .path(ino)
            .map_err(|_| io::Error::from(io::ErrorKind::NotFound))
            .and_then(|p| self.fs.open(&p, write, write && truncate));
        match r {
            Ok((h, _)) => reply.opened(FileHandle(h), FopenFlags::empty()),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let mut buf = vec![0; size as usize];
        match self.fs.read(fh.0, offset, &mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.fs.write(fh.0, offset, data) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(errno(&e)),
        }
    }

    /// Every `close(2)`: the only writer publishes here, before `close` returns (release
    /// comes later, asynchronously).
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        Self::done(reply, self.fs.flush(fh.0));
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        Self::done(reply, self.fs.release(fh.0));
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // The staged copy is made durable when it is published (on flush / close).
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let path = match self.path(ino) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        let entries = match self.fs.list(&path) {
            Ok(e) => e,
            Err(e) => return reply.error(errno(&e)),
        };
        let parent = path.parent().unwrap_or_default();
        let mut all = vec![
            (ino.0, FileType::Directory, ".".to_owned()),
            (
                self.inodes.lock().ino(&parent),
                FileType::Directory,
                "..".to_owned(),
            ),
        ];
        for e in entries {
            let Ok(p) = path.join(&e.name) else { continue };
            let kind = if e.attr.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            all.push((self.inodes.lock().ino(&p), kind, e.name));
        }
        for (i, (ino, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let r = self
            .child(parent, name)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
            .and_then(|p| {
                let (h, a) = self.fs.create(&p)?;
                Ok((self.fs.handle_path(h)?, h, a))
            });
        match r {
            Ok((p, h, a)) => reply.created(
                &TTL,
                &self.attr(&p, &a),
                Generation(0),
                FileHandle(h),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(errno(&e)),
        }
    }
}

/// Unmounts (and joins the session thread) when dropped.
struct Session(Option<fuser::BackgroundSession>);

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            if let Err(e) = s.umount_and_join() {
                tracing::warn!("mount: unmount: {e}");
            }
        }
    }
}

pub(crate) fn mount(fs: Arc<MountFs>, target: &str) -> anyhow::Result<Box<dyn Send>> {
    let label = fs.label();
    // SAFETY: getuid/getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let fuse = Fuse {
        fs,
        inodes: Mutex::default(),
        uid,
        gid,
    };
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::FSName(format!("keel:{label}")),
        fuser::MountOption::Subtype("keel".into()),
        fuser::MountOption::NoDev,
        fuser::MountOption::NoSuid,
    ];
    let session = fuser::spawn_mount(fuse, target, &config)
        .map_err(|e| anyhow::anyhow!("mounting at {target}: {e} (is FUSE installed?)"))?;
    Ok(Box::new(Session(Some(session))))
}
