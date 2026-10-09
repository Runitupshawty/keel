use crate::{Caps, Entry, Kind, Provider, VPath};
use anyhow::{Context, Result};
use notify::Watcher;
use std::{
    fs,
    io::{Read, Write},
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::RPC_E_CHANGED_MODE,
        Storage::FileSystem::{
            GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW, FILE_ATTRIBUTE_HIDDEN,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
        },
        UI::Shell::{
            FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName,
            FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION,
            FOF_NOERRORUI, FOF_SILENT,
        },
    },
};

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalProvider;

/// Produce an absolute extended-length path without stripping trailing dots or spaces.
pub(crate) fn long(p: &Path) -> Result<PathBuf> {
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        // Prefix before GetFullPathName-style normalization can trim trailing dots/spaces.
        anyhow::ensure!(
            !p.has_root()
                && !matches!(p.components().next(), Some(std::path::Component::Prefix(_))),
            "ambiguous drive-relative path: {}",
            p.display()
        );
        std::env::current_dir()?.join(p)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    let absolute = normalized;
    let units: Vec<_> = absolute.as_os_str().encode_wide().collect();
    use std::os::windows::ffi::OsStringExt;
    let prefix: Vec<_> = r"\\?\".encode_utf16().collect();
    if units.starts_with(&prefix) {
        return Ok(absolute);
    }
    let extended = if units.starts_with(&[92, 92]) {
        r"\\?\UNC\"
            .encode_utf16()
            .chain(units[2..].iter().copied())
            .collect::<Vec<_>>()
    } else {
        prefix.into_iter().chain(units).collect::<Vec<_>>()
    };
    Ok(std::ffi::OsString::from_wide(&extended).into())
}

pub(crate) fn wide(p: &Path) -> Result<Vec<u16>> {
    let mut units: Vec<_> = p.as_os_str().encode_wide().collect();
    anyhow::ensure!(!units.contains(&0), "path contains NUL: {}", p.display());
    units.push(0);
    Ok(units)
}

fn local(p: &VPath) -> Result<PathBuf> {
    p.to_local_path()
        .ok_or_else(|| anyhow::anyhow!("not a local path: {}", p.display()))
        .and_then(|p| long(&p))
}

fn entry(path: VPath, metadata: fs::Metadata) -> Entry {
    let name = path.name().to_owned();
    let kind = if metadata.is_symlink() {
        Kind::Symlink
    } else if metadata.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    };
    let ext = if kind == Kind::Dir {
        String::new()
    } else {
        Path::new(&name)
            .extension()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    Entry {
        path,
        name,
        kind,
        size: metadata.len(),
        modified: metadata.modified().ok(),
        hidden: metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN.0 != 0,
        ext,
    }
}

impl Provider for LocalProvider {
    fn scheme(&self) -> &'static str {
        "file"
    }
    fn caps(&self) -> Caps {
        Caps {
            write: true,
            rename: true,
            delete: true,
            watch: true,
        }
    }
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        (|| -> Result<_> {
            // DirEntry::metadata uses the metadata already returned by FindNextFileW on Windows.
            let mut entries = fs::read_dir(local(dir)?)?
                .map(|item| {
                    let item = item?;
                    let metadata = item.metadata()?;
                    Ok(entry(
                        dir.join(&item.file_name().to_string_lossy()),
                        metadata,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            // Cache case folding once per entry, not once per comparison.
            let mut keyed: Vec<_> = entries
                .drain(..)
                .map(|e| (e.name.to_lowercase(), e))
                .collect();
            keyed.sort_unstable_by(|(ak, a), (bk, b)| {
                (a.kind != Kind::Dir)
                    .cmp(&(b.kind != Kind::Dir))
                    .then(a.hidden.cmp(&b.hidden))
                    .then_with(|| natord::compare(ak, bk))
                    .then_with(|| a.name.cmp(&b.name))
            });
            Ok(keyed.into_iter().map(|(_, e)| e).collect())
        })()
        .with_context(|| format!("list {}", dir.display()))
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        fs::symlink_metadata(local(p)?)
            .map(|m| entry(p.clone(), m))
            .with_context(|| format!("stat {}", p.display()))
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(
            fs::File::open(local(p)?).with_context(|| format!("read {}", p.display()))?,
        ))
    }
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(
            fs::File::create(local(p)?).with_context(|| format!("write {}", p.display()))?,
        ))
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        fs::create_dir(local(p)?).with_context(|| format!("mkdir {}", p.display()))
    }
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        // MoveFileEx without REPLACE_EXISTING prevents a rename dialog from losing another file.
        let src = wide(&local(from)?)?;
        let dst = wide(&local(to)?)?;
        unsafe {
            windows::Win32::Storage::FileSystem::MoveFileExW(
                PCWSTR(src.as_ptr()),
                PCWSTR(dst.as_ptr()),
                windows::Win32::Storage::FileSystem::MOVE_FILE_FLAGS(0),
            )
        }
        .with_context(|| format!("rename {} to {}", from.display(), to.display()))
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        recycle(&local(p)?).with_context(|| format!("recycle {}", p.display()))
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        p.to_local_path()
            .ok_or_else(|| anyhow::anyhow!("not a local path: {}", p.display()))
    }
}

struct ComGuard(bool);
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

pub(crate) fn recycle(path: &Path) -> Result<()> {
    // Shell parsing names use ordinary DOS/UNC syntax, unlike filesystem APIs.
    // All COM objects are released before the per-call apartment guard.
    unsafe {
        let status = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if status != RPC_E_CHANGED_MODE {
            status.ok()?;
        }
        let _guard = ComGuard(status.is_ok());
        let op: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_ALL).context("create IFileOperation")?;
        op.SetOperationFlags(
            FOF_ALLOWUNDO
                | FOF_NOCONFIRMATION
                | FOF_SILENT
                | FOF_NOERRORUI
                | FOFX_RECYCLEONDELETE
                | FOFX_EARLYFAILURE,
        )?;
        let item: IShellItem = shell_item(path).context("create Shell item")?;
        op.DeleteItem(&item, None).context("queue recycle")?;
        op.PerformOperations().context("perform recycle")?;
        anyhow::ensure!(
            !op.GetAnyOperationsAborted()?.as_bool(),
            "recycle operation aborted"
        );
    }
    anyhow::ensure!(
        !path.try_exists()?,
        "Shell did not recycle {}",
        path.display()
    );
    Ok(())
}

/// A trailing-edge 100 ms debounce. Dropping the watcher disconnects and stops the worker.
pub fn watch(dir: &Path, tx: crossbeam_channel::Sender<()>) -> Result<notify::RecommendedWatcher> {
    let (events, rx) = crossbeam_channel::unbounded();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if !matches!(event, Ok(ref e) if matches!(e.kind, notify::EventKind::Access(_))) {
            let _ = events.send(());
        }
    })?;
    watcher
        .watch(&long(dir)?, notify::RecursiveMode::NonRecursive)
        .with_context(|| format!("watch {}", dir.display()))?;
    std::thread::Builder::new()
        .name("keel-watch".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                loop {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(()) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                    }
                }
                // Never block watcher shutdown on a full UI queue. One queued refresh is sufficient.
                if matches!(
                    tx.try_send(()),
                    Err(crossbeam_channel::TrySendError::Disconnected(_))
                ) {
                    break;
                }
            }
        })?;
    Ok(watcher)
}

pub fn drives() -> Vec<(String, String, u64, u64)> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| {
            let name = format!("{}:", (b'A' + i) as char);
            let root: Vec<_> = format!("{name}\\\0").encode_utf16().collect();
            let mut label = [0u16; 261];
            let (mut free, mut total) = (0, 0);
            unsafe {
                let _ = GetVolumeInformationW(
                    PCWSTR(root.as_ptr()),
                    Some(&mut label),
                    None,
                    None,
                    None,
                    None,
                );
                let _ = GetDiskFreeSpaceExW(
                    PCWSTR(root.as_ptr()),
                    Some(&mut free),
                    Some(&mut total),
                    None,
                );
            }
            let len = label.iter().position(|&u| u == 0).unwrap_or(label.len());
            (name, String::from_utf16_lossy(&label[..len]), free, total)
        })
        .collect()
}

// Enumerating a Shell item preserves names that the parsing API would normalize.
unsafe fn shell_item(path: &Path) -> Result<IShellItem> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{BHID_EnumItems, IEnumShellItems, SIGDN_PARENTRELATIVEPARSING},
    };
    let plain = VPath::local(path).to_local_path().expect("local path");
    if !plain
        .components()
        .any(|c| c.as_os_str().to_string_lossy().ends_with(['.', ' ']))
    {
        let name = wide(&plain)?;
        return Ok(SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None)?);
    }
    let parent = shell_item(path.parent().context("cannot recycle a root")?)?;
    let children: IEnumShellItems = parent.BindToHandler(None, &BHID_EnumItems)?;
    loop {
        let mut items = [None];
        let mut fetched = 0;
        children.Next(&mut items, Some(&mut fetched))?;
        if fetched == 0 {
            anyhow::bail!("Shell item not found: {}", path.display());
        }
        let item = items[0].take().context("missing Shell item")?;
        let name = item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING)?;
        let actual = name.to_string();
        CoTaskMemFree(Some(name.0.cast()));
        if actual? == path.file_name().context("no file name")?.to_string_lossy() {
            return Ok(item);
        }
    }
}
