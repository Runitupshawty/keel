//! NTFS compression of files and folders (Windows): LZNT1 through `FSCTL_SET_COMPRESSION`
//! (the folder's "Compress contents" attribute, new files inherit it) and the Windows
//! Overlay Filter's XPRESS4K / XPRESS8K / XPRESS16K / LZX through
//! `FSCTL_SET_EXTERNAL_BACKING` (what `compact /exe` does; per file, read-mostly files).
//! Uncompressing removes either. The bytes saved are measured with
//! `GetCompressedFileSizeW` before and after each file.
//!
//! [`count`] walks what a run would touch (the preview) on every system; [`run`] and
//! [`supported`] work on Windows only (elsewhere nothing is supported and `run` refuses).
//! Links (symbolic links, junctions) are never followed and never compressed.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// A compression algorithm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    /// Classic NTFS compression (`compact /c`): any file, read and written transparently;
    /// set on a folder, new files in it are compressed too.
    #[default]
    Lznt1,
    /// WOF (`compact /exe`): fastest, least saving. Files only; a file written to later is
    /// stored uncompressed again.
    Xpress4k,
    Xpress8k,
    Xpress16k,
    /// WOF: slowest, most saving; for files that are read, never written (programs).
    Lzx,
}

impl Algorithm {
    pub const ALL: [Algorithm; 5] = [
        Algorithm::Lznt1,
        Algorithm::Xpress4k,
        Algorithm::Xpress8k,
        Algorithm::Xpress16k,
        Algorithm::Lzx,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Algorithm::Lznt1 => "LZNT1 (NTFS)",
            Algorithm::Xpress4k => "XPRESS 4K",
            Algorithm::Xpress8k => "XPRESS 8K",
            Algorithm::Xpress16k => "XPRESS 16K",
            Algorithm::Lzx => "LZX",
        }
    }

    /// Through the Windows Overlay Filter (files only, written files decompress).
    pub fn wof(self) -> bool {
        self != Algorithm::Lznt1
    }

    /// `FILE_PROVIDER_COMPRESSION_*`.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn wof_code(self) -> u32 {
        match self {
            Algorithm::Xpress4k => 0,
            Algorithm::Lzx => 1,
            Algorithm::Xpress8k => 2,
            Algorithm::Xpress16k => 3,
            Algorithm::Lznt1 => u32::MAX,
        }
    }
}

/// Compress with an algorithm, or uncompress (either kind).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Compress(Algorithm),
    Uncompress,
}

/// What a run would touch: files (and their bytes) and folders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

/// What a run did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Files changed.
    pub files: u64,
    /// Folders whose compression attribute changed (LZNT1 and uncompress).
    pub folders: u64,
    /// Files left as they were: compressing would not save space, or the volume does not
    /// support the algorithm.
    pub skipped: u64,
    /// Files that failed, with why (at most [`Report::FAILED_KEPT`] kept).
    pub failed: Vec<(PathBuf, String)>,
    pub failed_count: u64,
    /// Sizes of the files changed or skipped: logical, and on disk before and after.
    pub logical: u64,
    pub before: u64,
    pub after: u64,
}

impl Report {
    pub const FAILED_KEPT: usize = 50;

    /// Bytes on disk saved (negative when uncompressing grew the files).
    pub fn saved(&self) -> i64 {
        self.before as i64 - self.after as i64
    }

    fn fail(&mut self, path: &Path, why: String) {
        self.failed_count += 1;
        if self.failed.len() < Self::FAILED_KEPT {
            self.failed.push((path.to_owned(), why));
        }
    }
}

/// Progress of a run: files done of the total the preview counted.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub current: PathBuf,
}

/// Stopped by the cancel flag.
#[derive(Debug, thiserror::Error)]
#[error("compression cancelled")]
pub struct Cancelled;

/// One thing a run touches.
enum Item {
    File(PathBuf, u64),
    Folder(PathBuf),
}

/// Every file and folder under `paths` (the paths themselves included; with `recurse`
/// off a folder's own files only), links skipped, in walk order.
fn walk(
    paths: &[PathBuf],
    recurse: bool,
    cancel: &AtomicBool,
    mut each: impl FnMut(Item) -> Result<()>,
) -> Result<()> {
    for root in paths {
        let depth = if recurse { usize::MAX } else { 1 };
        for e in walkdir::WalkDir::new(root)
            .follow_links(false)
            .max_depth(depth)
        {
            if cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            let e = match e {
                Ok(e) => e,
                // An unreadable folder: what is in it is not counted.
                Err(_) => continue,
            };
            let t = e.file_type();
            if t.is_symlink() {
                continue;
            }
            let path = e.into_path();
            if t.is_dir() {
                each(Item::Folder(path))?;
            } else if t.is_file() {
                let len = std::fs::symlink_metadata(&path).map_or(0, |m| m.len());
                each(Item::File(path, len))?;
            }
        }
    }
    Ok(())
}

/// The preview: what [`run`] on the same arguments would touch.
pub fn count(paths: &[PathBuf], recurse: bool, cancel: &AtomicBool) -> Result<Count> {
    let mut c = Count::default();
    walk(paths, recurse, cancel, |item| {
        match item {
            Item::File(_, len) => {
                c.files += 1;
                c.bytes += len;
            }
            Item::Folder(_) => c.folders += 1,
        }
        Ok(())
    })?;
    Ok(c)
}

/// Compresses or uncompresses `paths` (files and folders; with `recurse`, everything
/// under the folders), one file at a time; `total` is the preview's file count. Per-file
/// failures are collected in the report and the run goes on; the cancel flag stops it
/// between files (what was done stays done).
pub fn run(
    paths: &[PathBuf],
    action: Action,
    recurse: bool,
    total: u64,
    progress: &dyn Fn(&Progress),
    cancel: &AtomicBool,
) -> Result<Report> {
    anyhow::ensure!(cfg!(windows), "NTFS compression is a Windows feature");
    if let Some(p) = paths.iter().find(|p| !supported(p)) {
        anyhow::bail!("{} is not on a drive with NTFS compression", p.display());
    }
    let mut report = Report::default();
    let mut p = Progress {
        total,
        ..Progress::default()
    };
    walk(paths, recurse, cancel, |item| {
        match item {
            Item::Folder(dir) => {
                // WOF compresses files only; the folder attribute is LZNT1's.
                let off = match action {
                    Action::Compress(Algorithm::Lznt1) => false,
                    Action::Compress(_) => return Ok(()),
                    Action::Uncompress if is_compressed(&dir) => true,
                    Action::Uncompress => return Ok(()),
                };
                match sys::set_folder(&dir, off) {
                    Ok(()) => report.folders += 1,
                    Err(e) => report.fail(&dir, format!("{e:#}")),
                }
            }
            Item::File(file, len) => {
                p.current = file.clone();
                progress(&p);
                let before = on_disk(&file).unwrap_or(len);
                match sys::file(&file, action) {
                    Ok(done) => {
                        if done {
                            report.files += 1;
                        } else {
                            report.skipped += 1;
                        }
                        report.logical += len;
                        report.before += before;
                        report.after += on_disk(&file).unwrap_or(before);
                    }
                    Err(e) => report.fail(&file, format!("{e:#}")),
                }
                p.done += 1;
            }
        }
        Ok(())
    })?;
    progress(&p);
    Ok(report)
}

/// Whether the drive holding `path` supports NTFS compression (Windows only).
pub fn supported(path: &Path) -> bool {
    sys::supported(path)
}

/// Bytes `path` takes on disk: the compressed size of a compressed file, the allocated
/// size of a sparse one (`GetCompressedFileSizeW`); elsewhere its length.
pub fn on_disk(path: &Path) -> std::io::Result<u64> {
    sys::on_disk(path)
}

/// Whether `path` is compressed: LZNT1 (the compressed attribute) or WOF.
pub fn is_compressed(path: &Path) -> bool {
    sys::is_compressed(path)
}

#[cfg(not(windows))]
mod sys {
    use super::Action;
    use std::path::Path;

    pub fn supported(_: &Path) -> bool {
        false
    }
    pub fn on_disk(path: &Path) -> std::io::Result<u64> {
        Ok(std::fs::symlink_metadata(path)?.len())
    }
    pub fn is_compressed(_: &Path) -> bool {
        false
    }
    pub fn set_folder(_: &Path, _: bool) -> anyhow::Result<()> {
        anyhow::bail!("NTFS compression is a Windows feature")
    }
    pub fn file(_: &Path, _: Action) -> anyhow::Result<bool> {
        anyhow::bail!("NTFS compression is a Windows feature")
    }
}

#[cfg(windows)]
mod sys {
    use super::{Action, Algorithm};
    use anyhow::{Context, Result};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{GetCompressedFileSizeW, GetVolumeInformationW};
    use windows::Win32::System::IO::DeviceIoControl;

    const FSCTL_SET_COMPRESSION: u32 = 0x0009_C040;
    const FSCTL_SET_EXTERNAL_BACKING: u32 = 0x0009_030C;
    const FSCTL_GET_EXTERNAL_BACKING: u32 = 0x0009_0310;
    const FSCTL_DELETE_EXTERNAL_BACKING: u32 = 0x0009_0314;
    const COMPRESSION_FORMAT_NONE: u16 = 0;
    const COMPRESSION_FORMAT_LZNT1: u16 = 2;
    const WOF_CURRENT_VERSION: u32 = 1;
    const WOF_PROVIDER_FILE: u32 = 2;
    const FILE_PROVIDER_CURRENT_VERSION: u32 = 1;
    const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
    const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
    const FILE_FILE_COMPRESSION: u32 = 0x10;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    // FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES |
    // SYNCHRONIZE: what FSCTL_SET_COMPRESSION and the WOF controls need.
    const ACCESS: u32 = 0x1 | 0x2 | 0x80 | 0x100 | 0x0010_0000;
    // FILE_SHARE_READ | FILE_SHARE_DELETE: nobody writes while it changes.
    const SHARE: u32 = 0x1 | 0x4;
    const ERROR_INVALID_FUNCTION: u32 = 1;
    const ERROR_NOT_SUPPORTED: u32 = 50;
    const ERROR_OBJECT_NOT_EXTERNALLY_BACKED: u32 = 342;
    const ERROR_COMPRESSION_NOT_BENEFICIAL: u32 = 344;

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain([0]).collect()
    }

    /// The Win32 error code inside a `windows` error (0 when it is not one).
    fn code(e: &windows::core::Error) -> u32 {
        let h = e.code().0 as u32;
        if h & 0xFFFF_0000 == 0x8007_0000 {
            h & 0xFFFF
        } else {
            0
        }
    }

    fn ioctl(file: &std::fs::File, control: u32, input: &[u8]) -> windows::core::Result<()> {
        let mut got = 0u32;
        // SAFETY: `input` outlives the call and its length is passed; no output buffer.
        unsafe {
            DeviceIoControl(
                HANDLE(file.as_raw_handle()),
                control,
                (!input.is_empty()).then_some(input.as_ptr().cast()),
                input.len() as u32,
                None,
                0,
                Some(&mut got),
                None,
            )
        }
    }

    fn open(path: &Path, dir: bool) -> std::io::Result<std::fs::File> {
        let mut o = std::fs::OpenOptions::new();
        o.access_mode(ACCESS).share_mode(SHARE);
        if dir {
            o.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        }
        o.open(path)
    }

    /// Runs `f` on `path` opened for the compression controls; a read-only file has its
    /// read-only attribute lifted for the time of it.
    fn with_open<T>(
        path: &Path,
        dir: bool,
        f: impl FnOnce(&std::fs::File) -> Result<T>,
    ) -> Result<T> {
        let long = crate::local::long(path)?;
        match open(&long, dir) {
            Ok(file) => f(&file),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && !dir => {
                let meta = std::fs::metadata(&long)?;
                if meta.file_attributes() & FILE_ATTRIBUTE_READONLY == 0 {
                    return Err(e).context("open");
                }
                let mut perms = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                std::fs::set_permissions(&long, perms.clone()).context("lift read-only")?;
                let out = open(&long, dir).context("open").and_then(|file| f(&file));
                perms.set_readonly(true);
                let restored = std::fs::set_permissions(&long, perms);
                let out = out?;
                restored.context("set read-only again")?;
                Ok(out)
            }
            Err(e) => Err(e).context("open"),
        }
    }

    fn set_lznt1(file: &std::fs::File, on: bool) -> windows::core::Result<()> {
        let format = if on {
            COMPRESSION_FORMAT_LZNT1
        } else {
            COMPRESSION_FORMAT_NONE
        };
        ioctl(file, FSCTL_SET_COMPRESSION, &format.to_le_bytes())
    }

    /// WOF_EXTERNAL_INFO then FILE_PROVIDER_EXTERNAL_INFO_V1.
    fn wof_input(alg: Algorithm) -> Vec<u8> {
        [
            WOF_CURRENT_VERSION,
            WOF_PROVIDER_FILE,
            FILE_PROVIDER_CURRENT_VERSION,
            alg.wof_code(),
            0,
        ]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect()
    }

    fn wof_backed(file: &std::fs::File) -> bool {
        let mut out = [0u8; 64];
        let mut got = 0u32;
        // SAFETY: the output buffer is valid for its length; no input.
        unsafe {
            DeviceIoControl(
                HANDLE(file.as_raw_handle()),
                FSCTL_GET_EXTERNAL_BACKING,
                None,
                0,
                Some(out.as_mut_ptr().cast()),
                out.len() as u32,
                Some(&mut got),
                None,
            )
        }
        .is_ok()
    }

    fn drop_wof(file: &std::fs::File) -> Result<()> {
        match ioctl(file, FSCTL_DELETE_EXTERNAL_BACKING, &[]) {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    code(&e),
                    ERROR_OBJECT_NOT_EXTERNALLY_BACKED
                        | ERROR_INVALID_FUNCTION
                        | ERROR_NOT_SUPPORTED
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!(e.message())).context("remove WOF compression"),
        }
    }

    pub fn set_folder(dir: &Path, off: bool) -> Result<()> {
        with_open(dir, true, |f| {
            set_lznt1(f, !off).map_err(|e| anyhow::anyhow!(e.message()))
        })
    }

    /// True when the file changed; false when it was left as it was (not beneficial, or
    /// the algorithm is not available on this volume).
    pub fn file(path: &Path, action: Action) -> Result<bool> {
        let lznt1 = std::fs::metadata(path)
            .map(|m| m.file_attributes() & FILE_ATTRIBUTE_COMPRESSED != 0)
            .unwrap_or(false);
        // Read before the file is opened for writing, which ends WOF compression itself.
        let wof = action == Action::Uncompress && is_wof(path);
        with_open(path, false, |f| match action {
            Action::Uncompress => {
                drop_wof(f)?;
                if lznt1 {
                    set_lznt1(f, false).map_err(|e| anyhow::anyhow!(e.message()))?;
                }
                Ok(wof || lznt1)
            }
            Action::Compress(Algorithm::Lznt1) => {
                drop_wof(f)?;
                set_lznt1(f, true).map_err(|e| anyhow::anyhow!(e.message()))?;
                Ok(true)
            }
            Action::Compress(alg) => {
                if lznt1 {
                    set_lznt1(f, false).map_err(|e| anyhow::anyhow!(e.message()))?;
                }
                match ioctl(f, FSCTL_SET_EXTERNAL_BACKING, &wof_input(alg)) {
                    Ok(()) => Ok(true),
                    Err(e)
                        if matches!(
                            code(&e),
                            ERROR_COMPRESSION_NOT_BENEFICIAL
                                | ERROR_INVALID_FUNCTION
                                | ERROR_NOT_SUPPORTED
                        ) =>
                    {
                        Ok(false)
                    }
                    Err(e) => Err(anyhow::anyhow!(e.message())).context("WOF compression"),
                }
            }
        })
    }

    pub fn supported(path: &Path) -> bool {
        let Some(id) = crate::desktop::volume_id(path) else {
            return false;
        };
        let root: Vec<u16> = id.encode_utf16().chain([0]).collect();
        let mut flags = 0u32;
        // SAFETY: `root` is NUL-terminated; the flags output is a valid u32.
        let ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(root.as_ptr()),
                None,
                None,
                None,
                Some(&mut flags),
                None,
            )
        };
        ok.is_ok() && flags & FILE_FILE_COMPRESSION != 0
    }

    pub fn on_disk(path: &Path) -> std::io::Result<u64> {
        let w = wide(&crate::local::long(path).map_err(std::io::Error::other)?);
        let mut high = 0u32;
        // SAFETY: `w` is NUL-terminated; `high` is a valid u32.
        let low = unsafe { GetCompressedFileSizeW(PCWSTR(w.as_ptr()), Some(&mut high)) };
        if low == u32::MAX {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error().unwrap_or(0) != 0 {
                return Err(e);
            }
        }
        Ok((u64::from(high) << 32) | u64::from(low))
    }

    pub fn is_compressed(path: &Path) -> bool {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return false;
        };
        if meta.file_attributes() & FILE_ATTRIBUTE_COMPRESSED != 0 {
            return true;
        }
        meta.is_file() && is_wof(path)
    }

    fn is_wof(path: &Path) -> bool {
        crate::local::long(path)
            .ok()
            .and_then(|l| open_read(&l).ok())
            .is_some_and(|f| wof_backed(&f))
    }

    fn open_read(path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .access_mode(0x80 | 0x0010_0000)
            .share_mode(0x1 | 0x2 | 0x4)
            .open(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let text = "keel compresses this line again and again\n".repeat(8 * 1024);
        for f in ["a.txt", "sub dir/b.txt", "sub dir/deep/ünï cödé.txt"] {
            let p = t.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, &text).unwrap();
        }
        t
    }

    #[test]
    fn the_preview_counts_files_and_folders() {
        let t = tree();
        let no = AtomicBool::new(false);
        let all = count(&[t.path().to_owned()], true, &no).unwrap();
        assert_eq!((all.files, all.folders), (3, 3));
        assert_eq!(all.bytes, 3 * 8 * 1024 * 42);
        let top = count(&[t.path().to_owned()], false, &no).unwrap();
        assert_eq!((top.files, top.folders), (1, 2));
        let one = count(&[t.path().join("a.txt")], true, &no).unwrap();
        assert_eq!((one.files, one.folders), (1, 0));
        let stop = AtomicBool::new(true);
        assert!(count(&[t.path().to_owned()], true, &stop)
            .unwrap_err()
            .is::<Cancelled>());
    }

    #[cfg(not(windows))]
    #[test]
    fn elsewhere_nothing_is_supported() {
        let t = tree();
        assert!(!supported(t.path()));
        let no = AtomicBool::new(false);
        let r = run(
            &[t.path().to_owned()],
            Action::Compress(Algorithm::Lznt1),
            true,
            3,
            &|_| {},
            &no,
        );
        assert!(r.is_err());
    }

    #[cfg(windows)]
    fn read_all(t: &tempfile::TempDir) -> Vec<String> {
        ["a.txt", "sub dir/b.txt", "sub dir/deep/ünï cödé.txt"]
            .iter()
            .map(|f| std::fs::read_to_string(t.path().join(f)).unwrap())
            .collect()
    }

    /// Each algorithm compresses the tree (the bytes on disk shrink, the content stays),
    /// and uncompress brings it back; skipped where the drive has no NTFS compression.
    #[cfg(windows)]
    #[test]
    fn compress_and_uncompress_every_algorithm() {
        let t = tree();
        if !supported(t.path()) {
            eprintln!("skipped: the temp folder's drive has no NTFS compression");
            return;
        }
        let content = read_all(&t);
        let no = AtomicBool::new(false);
        let roots = [t.path().to_owned()];
        for alg in Algorithm::ALL {
            let r = run(&roots, Action::Compress(alg), true, 3, &|_| {}, &no).unwrap();
            assert!(r.failed.is_empty(), "{alg:?}: {:?}", r.failed);
            if alg.wof() && r.files == 0 {
                eprintln!("skipped {alg:?}: WOF is not available here");
                continue;
            }
            assert_eq!(r.files, 3, "{alg:?}");
            assert!(r.after < r.logical, "{alg:?}: {r:?}");
            assert!(is_compressed(&t.path().join("sub dir/b.txt")), "{alg:?}");
            assert_eq!(read_all(&t), content, "{alg:?}");
            if alg == Algorithm::Lznt1 {
                assert_eq!(r.folders, 3);
                assert!(is_compressed(&t.path().join("sub dir")));
            }
            let back = run(&roots, Action::Uncompress, true, 3, &|_| {}, &no).unwrap();
            assert!(back.failed.is_empty(), "{alg:?}: {:?}", back.failed);
            assert_eq!(back.files, 3, "{alg:?}");
            assert!(back.after >= back.logical, "{alg:?}: {back:?}");
            assert!(!is_compressed(&t.path().join("a.txt")), "{alg:?}");
            assert!(!is_compressed(&t.path().join("sub dir")), "{alg:?}");
            assert_eq!(read_all(&t), content, "{alg:?}");
        }
    }

    /// A read-only file is compressed and stays read-only; a cancelled run stops between
    /// files.
    #[cfg(windows)]
    #[test]
    fn read_only_files_and_cancel() {
        let t = tree();
        if !supported(t.path()) {
            eprintln!("skipped: the temp folder's drive has no NTFS compression");
            return;
        }
        let a = t.path().join("a.txt");
        let mut perms = std::fs::metadata(&a).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&a, perms.clone()).unwrap();
        let no = AtomicBool::new(false);
        let r = run(
            std::slice::from_ref(&a),
            Action::Compress(Algorithm::Lznt1),
            false,
            1,
            &|_| {},
            &no,
        )
        .unwrap();
        assert_eq!(r.files, 1, "{r:?}");
        assert!(r.saved() > 0, "{r:?}");
        assert!(std::fs::metadata(&a).unwrap().permissions().readonly());
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&a, perms).unwrap();

        let stop = AtomicBool::new(true);
        let err = run(
            &[t.path().to_owned()],
            Action::Compress(Algorithm::Lznt1),
            true,
            3,
            &|_| {},
            &stop,
        )
        .unwrap_err();
        assert!(err.is::<Cancelled>());
        assert!(!is_compressed(&t.path().join("sub dir/b.txt")));
    }
}
