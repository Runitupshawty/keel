//! `trash://`: the current user's Recycle Bin / Trash as a read-only folder. The root lists
//! every trashed item (`trash://<key>/<name>`, `key` = a hash of the OS item id); `remove`
//! deletes an item for good and [`TrashProvider::restore_paths`] puts it back where it was.
//! Windows and freedesktop Linux go through `trash::os_limited`. macOS has no listing API:
//! its Trash is read as plain folders (`~/.Trash`, `/Volumes/<x>/.Trashes/<uid>`), which do
//! not say where an item came from, so items are restored to a folder the user picks
//! ([`TrashProvider::restore_to`]). The files themselves are reachable for previews only
//! when the OS keeps them at a real path (Windows `$R...` files, Linux `files/<name>`, every
//! macOS item).

use crate::{Caps, Entry, Kind, Provider, RemoveKind, VPath};
use ::trash::TrashItem;
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const SCHEME: &str = "trash";
/// Whether this platform can list its trash at all.
pub const SUPPORTED: bool = cfg!(any(windows, unix));
/// Whether the trash records where each item came from, so Restore can put it back there.
/// False on macOS: restore with [`TrashProvider::restore_to`] instead.
pub const KNOWS_ORIGIN: bool = !cfg!(target_os = "macos");
/// Size lookups cost a shell call per item on Windows; items past this many show no size.
const META_CAP: usize = 5000;

/// The platform's name for it, for the sidebar and dialogs.
pub fn label() -> &'static str {
    if cfg!(windows) {
        "Recycle Bin"
    } else {
        "Trash"
    }
}

/// `trash:///`.
pub fn root() -> VPath {
    VPath {
        scheme: SCHEME.into(),
        authority: String::new(),
        path: "/".into(),
    }
}

/// What the listing knows about an item beyond its `Entry`.
#[derive(Clone, Debug)]
pub struct TrashInfo {
    /// The full path it was deleted from.
    pub original: PathBuf,
    pub deleted: Option<SystemTime>,
    id: std::ffi::OsString,
}

impl TrashInfo {
    /// Where the OS keeps the trashed file, when it is a plain path.
    pub fn payload(&self) -> Option<PathBuf> {
        #[cfg(windows)]
        {
            let p = PathBuf::from(&self.id);
            p.exists().then_some(p)
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            // The id is `<trash>/info/<stored name>.trashinfo`.
            let info = Path::new(&self.id);
            let p = info
                .parent()?
                .parent()?
                .join("files")
                .join(info.file_stem()?);
            p.symlink_metadata().ok().map(|_| p)
        }
        #[cfg(target_os = "macos")]
        {
            let p = PathBuf::from(&self.id);
            p.symlink_metadata().ok().map(|_| p)
        }
    }
}

/// The last listing's items by key (the listing is where `original` is known cheaply).
fn cache() -> &'static Mutex<HashMap<String, TrashInfo>> {
    static CACHE: OnceLock<Mutex<HashMap<String, TrashInfo>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn key_of_item(item: &TrashItem) -> String {
    // FNV-1a: stable across runs, unlike `DefaultHasher`.
    let mut h = 0xcbf29ce484222325u64;
    for b in item.id.to_string_lossy().bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// `trash://<key>/<name>` for `item`.
pub fn path_of(item: &TrashItem) -> VPath {
    VPath {
        scheme: SCHEME.into(),
        authority: key_of_item(item),
        path: format!("/{}", item.name.to_string_lossy()),
    }
}

/// The item key of a `trash://<key>/...` path (None for the root and other schemes).
pub fn key_of(p: &VPath) -> Option<&str> {
    (p.scheme == SCHEME && !p.authority.is_empty()).then_some(p.authority.as_str())
}

/// What the last listing knew about `p`.
pub fn info(p: &VPath) -> Option<TrashInfo> {
    cache().lock().ok()?.get(key_of(p)?).cloned()
}

fn time_of(item: &TrashItem) -> Option<SystemTime> {
    u64::try_from(item.time_deleted)
        .ok()
        .map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

fn entry_of(item: &TrashItem, shown: String, with_meta: bool) -> Entry {
    let (kind, size) = if with_meta {
        sys::meta(item).unwrap_or((Kind::File, 0))
    } else {
        (Kind::File, 0)
    };
    let ext = if kind == Kind::Dir {
        String::new()
    } else {
        Path::new(&item.name)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    Entry {
        path: path_of(item),
        name: shown,
        kind,
        size,
        modified: time_of(item),
        hidden: false,
        is_link: false,
        encrypted: false,
        ext,
    }
}

/// `a.txt` -> `a (2).txt`: selection is keyed by entry name, and two trashed files often
/// share one.
fn numbered(name: &str, n: usize) -> String {
    let p = Path::new(name);
    match (p.file_stem(), p.extension()) {
        (Some(stem), Some(ext)) if !name.starts_with('.') => {
            format!("{} ({n}).{}", stem.to_string_lossy(), ext.to_string_lossy())
        }
        _ => format!("{name} ({n})"),
    }
}

fn list_entries() -> Result<Vec<Entry>> {
    let mut items = sys::list()?;
    items.sort_by_key(|a| std::cmp::Reverse(a.time_deleted));
    let mut seen = HashSet::new();
    let mut known = HashMap::new();
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let name = item.name.to_string_lossy().into_owned();
        let mut shown = name.clone();
        let mut n = 2;
        while !seen.insert(shown.to_lowercase()) {
            shown = numbered(&name, n);
            n += 1;
        }
        known.insert(
            key_of_item(item),
            TrashInfo {
                original: item.original_path(),
                deleted: time_of(item),
                id: item.id.clone(),
            },
        );
        out.push(entry_of(item, shown, i < META_CAP));
    }
    if let Ok(mut c) = cache().lock() {
        *c = known;
    }
    Ok(out)
}

/// The live items behind these paths (one OS listing for all of them).
fn find(paths: &[VPath]) -> Result<Vec<TrashItem>> {
    let mut wanted = Vec::with_capacity(paths.len());
    for p in paths {
        wanted.push(key_of(p).with_context(|| format!("not a trash item: {}", p.display()))?);
    }
    let items = sys::list()?;
    let mut out = Vec::with_capacity(paths.len());
    for (p, key) in paths.iter().zip(wanted) {
        let item = items
            .iter()
            .find(|i| key_of_item(i) == key)
            .with_context(|| format!("{} is no longer in the {}", p.name(), label()))?;
        out.push(item.clone());
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TrashProvider;

impl TrashProvider {
    /// Puts the items back where they were deleted from. Fails, restoring nothing on
    /// Windows, when one of the original paths exists already.
    pub fn restore_paths(&self, paths: &[VPath]) -> Result<()> {
        sys::restore(find(paths)?)
    }

    /// Moves the items into `dest` under their own names (macOS, where the Trash does not
    /// record their original folders). Stops before moving anything when a name exists in
    /// `dest`.
    pub fn restore_to(&self, paths: &[VPath], dest: &Path) -> Result<()> {
        sys::restore_to(find(paths)?, dest)
    }

    /// Deletes the items for good.
    pub fn purge_paths(&self, paths: &[VPath]) -> Result<()> {
        sys::purge(&find(paths)?)
    }

    /// Deletes everything in the trash for good; returns how many items it held.
    pub fn empty(&self) -> Result<usize> {
        let items = sys::list()?;
        sys::purge(&items)?;
        if let Ok(mut c) = cache().lock() {
            c.clear();
        }
        Ok(items.len())
    }
}

fn read_only(p: &VPath) -> anyhow::Error {
    anyhow::anyhow!("{}: the {} is read-only here", p.display(), label())
}

impl Provider for TrashProvider {
    fn scheme(&self) -> &'static str {
        SCHEME
    }
    fn caps(&self) -> Caps {
        Caps {
            write: false,
            rename: false,
            delete: true,
            watch: false,
        }
    }
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        if key_of(dir).is_some() {
            anyhow::bail!("Restore {} to browse its contents", dir.name());
        }
        list_entries().with_context(|| format!("list the {}", label()))
    }
    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.list(dir)
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        if key_of(p).is_none() {
            return Ok(Entry {
                path: p.clone(),
                name: label().into(),
                kind: Kind::Dir,
                size: 0,
                modified: None,
                hidden: false,
                is_link: false,
                encrypted: false,
                ext: String::new(),
            });
        }
        let item = find(std::slice::from_ref(p))?.remove(0);
        Ok(entry_of(
            &item,
            item.name.to_string_lossy().into_owned(),
            true,
        ))
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        let file = self.local_copy(p)?;
        Ok(Box::new(
            std::fs::File::open(&file).with_context(|| format!("read {}", p.display()))?,
        ))
    }
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Err(read_only(p))
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        Err(read_only(p))
    }
    fn rename(&self, from: &VPath, _to: &VPath) -> Result<()> {
        Err(read_only(from))
    }
    /// Permanent.
    fn remove(&self, p: &VPath) -> Result<()> {
        self.purge_paths(std::slice::from_ref(p))
    }
    fn remove_kind(&self) -> RemoveKind {
        RemoveKind::Permanent
    }
    /// The trashed file where the OS keeps it; "no preview" when it is not a plain path.
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        let info = match info(p) {
            Some(i) => i,
            None => {
                list_entries()?;
                info(p).context("no preview")?
            }
        };
        info.payload().with_context(|| {
            format!(
                "no preview: the {} keeps {} out of reach",
                label(),
                p.name()
            )
        })
    }
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
mod sys {
    use super::*;

    pub fn list() -> Result<Vec<TrashItem>> {
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut items = ::trash::os_limited::list()?;
        #[cfg(windows)]
        items.iter_mut().for_each(restore_extension);
        Ok(items)
    }

    /// The shell names items as Explorer shows them, minus the extensions it hides, and `restore`
    /// would give the file that short name back. The `$R...` file the item is stored as keeps
    /// the real extension.
    // ponytail: a file called `a.txt.txt` reads as `a.txt` already, so it restores as `a.txt`;
    // asking the shell for SIGDN_FILESYSPATH names would settle it.
    #[cfg(windows)]
    fn restore_extension(item: &mut TrashItem) {
        let Some(ext) = Path::new(&item.id).extension() else {
            return;
        };
        let (name, ext) = (item.name.to_string_lossy(), ext.to_string_lossy());
        if !name
            .to_lowercase()
            .ends_with(&format!(".{}", ext.to_lowercase()))
        {
            item.name = format!("{name}.{ext}").into();
        }
    }
    pub fn purge(items: &[TrashItem]) -> Result<()> {
        if !items.is_empty() {
            ::trash::os_limited::purge_all(items)?;
        }
        Ok(())
    }
    pub fn restore(items: Vec<TrashItem>) -> Result<()> {
        match ::trash::os_limited::restore_all(items) {
            Err(::trash::Error::RestoreCollision { path, .. }) => anyhow::bail!(
                "{} already exists; move or rename it, then restore again",
                path.display()
            ),
            Err(::trash::Error::RestoreTwins { path, .. }) => anyhow::bail!(
                "two items would both restore to {}; restore them one at a time",
                path.display()
            ),
            other => Ok(other?),
        }
    }
    pub fn restore_to(_: Vec<TrashItem>, _: &Path) -> Result<()> {
        anyhow::bail!("the {} restores items to where they came from", label())
    }
    pub fn meta(item: &TrashItem) -> Option<(Kind, u64)> {
        match ::trash::os_limited::metadata(item).ok()?.size {
            ::trash::TrashItemSize::Bytes(b) => Some((Kind::File, b)),
            ::trash::TrashItemSize::Entries(_) => Some((Kind::Dir, 0)),
        }
    }
}

/// macOS: the Trash as plain folders; see [`folder`].
#[cfg(target_os = "macos")]
mod sys {
    use super::*;

    pub fn list() -> Result<Vec<TrashItem>> {
        let home = directories::BaseDirs::new()
            .context("no home folder")?
            .home_dir()
            .to_path_buf();
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        folder::list(&folder::roots(&home, Path::new("/Volumes"), uid)).map_err(|e| {
            let denied = e
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
            if denied {
                anyhow::anyhow!(
                    "macOS shows the Trash only to apps with Full Disk Access \
                     (System Settings > Privacy & Security > Full Disk Access)"
                )
            } else {
                e
            }
        })
    }
    pub fn purge(items: &[TrashItem]) -> Result<()> {
        folder::purge(items)
    }
    pub fn restore(items: Vec<TrashItem>) -> Result<()> {
        let name = items.first().map(|i| i.name.to_string_lossy().into_owned());
        anyhow::bail!(
            "the Trash does not record where {} came from: use Restore to... and pick a folder",
            name.unwrap_or_default()
        )
    }
    pub fn restore_to(items: Vec<TrashItem>, dest: &Path) -> Result<()> {
        folder::restore_to(&items, dest)
    }
    pub fn meta(item: &TrashItem) -> Option<(Kind, u64)> {
        folder::meta(item)
    }
}

/// A trash that is only folders of trashed items (macOS): an item's id is its full path, its
/// original folder is unknown (`original_parent` empty) and its deletion time is the time it
/// was moved in (the change time).
#[cfg(any(target_os = "macos", test))]
mod folder {
    use super::*;

    /// `<home>/.Trash`, then `<volumes>/<x>/.Trashes/<uid>` of each mounted volume that has one.
    pub fn roots(home: &Path, volumes: &Path, uid: u32) -> Vec<PathBuf> {
        let mut out = vec![home.join(".Trash")];
        if let Ok(dir) = std::fs::read_dir(volumes) {
            let mut more: Vec<_> = dir
                .flatten()
                .map(|v| v.path().join(".Trashes").join(uid.to_string()))
                .filter(|t| t.is_dir())
                .collect();
            more.sort();
            out.extend(more);
        }
        out
    }

    /// Every item in `roots`. A missing root is empty; any other error fails the listing.
    pub fn list(roots: &[PathBuf]) -> Result<Vec<TrashItem>> {
        let mut out = Vec::new();
        for root in roots {
            let dir = match std::fs::read_dir(root) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(anyhow::Error::new(e).context(root.display().to_string())),
            };
            for entry in dir {
                let entry = entry?;
                let name = entry.file_name();
                if name == ".DS_Store" {
                    continue;
                }
                let changed = entry.metadata().ok().map_or(0, |m| changed(&m));
                out.push(TrashItem {
                    id: entry.path().into_os_string(),
                    name,
                    original_parent: PathBuf::new(),
                    time_deleted: changed,
                });
            }
        }
        Ok(out)
    }

    #[cfg(unix)]
    fn changed(m: &std::fs::Metadata) -> i64 {
        std::os::unix::fs::MetadataExt::ctime(m)
    }
    #[cfg(not(unix))]
    fn changed(m: &std::fs::Metadata) -> i64 {
        m.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64)
    }

    pub fn meta(item: &TrashItem) -> Option<(Kind, u64)> {
        let m = Path::new(&item.id).symlink_metadata().ok()?;
        Some(if m.is_dir() {
            (Kind::Dir, 0)
        } else {
            (Kind::File, m.len())
        })
    }

    /// Removes the items for good (a link, never what it points to).
    pub fn purge(items: &[TrashItem]) -> Result<()> {
        for item in items {
            let p = Path::new(&item.id);
            let is_dir = p.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false);
            let removed = if is_dir {
                std::fs::remove_dir_all(p)
            } else {
                std::fs::remove_file(p)
            };
            removed.with_context(|| format!("delete {}", item.name.to_string_lossy()))?;
        }
        Ok(())
    }

    /// Moves the items into `dest`, checking every name first so nothing moves when one
    /// would land on an existing file.
    // ponytail: a plain rename, so an item on another volume than `dest` fails with a message;
    // copy-then-delete would lift that.
    pub fn restore_to(items: &[TrashItem], dest: &Path) -> Result<()> {
        let mut names = HashSet::new();
        for item in items {
            let to = dest.join(&item.name);
            anyhow::ensure!(
                to.symlink_metadata().is_err(),
                "{} already exists; move or rename it, then restore again",
                to.display()
            );
            anyhow::ensure!(
                names.insert(item.name.clone()),
                "two items would both restore to {}; restore them one at a time",
                to.display()
            );
        }
        for item in items {
            std::fs::rename(&item.id, dest.join(&item.name)).with_context(|| {
                format!(
                    "move {} to {} (pick a folder on the same drive)",
                    item.name.to_string_lossy(),
                    dest.display()
                )
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn item(id: &str, name: &str) -> TrashItem {
        TrashItem {
            id: OsString::from(id),
            name: OsString::from(name),
            original_parent: PathBuf::from("/home/u"),
            time_deleted: 1_700_000_000,
        }
    }

    #[test]
    fn paths_map_to_items_and_back() {
        let a = item("/h/.local/share/Trash/info/a.txt.trashinfo", "a.txt");
        let b = item("/h/.local/share/Trash/info/a.txt.2.trashinfo", "a.txt");
        let (pa, pb) = (path_of(&a), path_of(&b));
        assert_eq!(pa.scheme, SCHEME);
        assert_eq!(pa.name(), "a.txt");
        assert_ne!(pa, pb, "same name, different items");
        assert_eq!(key_of(&pa), Some(key_of_item(&a).as_str()));
        assert_eq!(key_of(&root()), None);
        assert_eq!(key_of(&VPath::local("/x")), None);
        assert_eq!(VPath::parse(&pa.display()).unwrap(), pa);
        assert_eq!(root().parent(), None);
        assert_eq!(numbered("a.txt", 2), "a (2).txt");
        assert_eq!(numbered(".bashrc", 2), ".bashrc (2)");
    }

    #[test]
    fn a_folder_trash_lists_restores_and_purges() {
        let dir = test_dir();
        let (home, volumes) = (dir.path().join("home"), dir.path().join("Volumes"));
        let trash = home.join(".Trash");
        let usb = volumes.join("USB").join(".Trashes").join("501");
        let other_user = volumes.join("Other").join(".Trashes").join("502");
        for d in [&trash, &usb, &other_user] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(volumes.join("Plain")).unwrap();
        let roots = folder::roots(&home, &volumes, 501);
        assert_eq!(
            roots,
            [trash.clone(), usb.clone()],
            "own uid only, home first"
        );
        let none = dir.path().join("none");
        assert_eq!(folder::roots(&none, &none, 501), [none.join(".Trash")]);
        assert!(folder::list(&[none.join(".Trash")]).unwrap().is_empty());

        std::fs::write(trash.join("a.txt"), "hello").unwrap();
        std::fs::write(trash.join(".DS_Store"), "x").unwrap();
        std::fs::create_dir_all(trash.join("folder").join("inner")).unwrap();
        std::fs::write(usb.join("a.txt"), "usb").unwrap();
        let items = folder::list(&roots).unwrap();
        let names: Vec<_> = items.iter().map(|i| i.name.to_string_lossy()).collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(!names.iter().any(|n| n == ".DS_Store"));
        let by_path = |p: PathBuf| items.iter().find(|i| Path::new(&i.id) == p).unwrap();
        let a = by_path(trash.join("a.txt"));
        assert_eq!(folder::meta(a), Some((Kind::File, 5)));
        assert!(a.time_deleted > 0);
        assert_eq!(a.original_path(), PathBuf::from("a.txt"), "no known origin");
        let f = by_path(trash.join("folder"));
        assert_eq!(folder::meta(f), Some((Kind::Dir, 0)));
        let b = by_path(usb.join("a.txt"));
        assert_ne!(key_of_item(a), key_of_item(b), "same name, different items");

        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let err = folder::restore_to(&[a.clone(), b.clone()], &dest).unwrap_err();
        assert!(err.to_string().contains("one at a time"), "{err}");
        assert!(trash.join("a.txt").exists(), "nothing moved");
        std::fs::write(dest.join("a.txt"), "mine").unwrap();
        let err = folder::restore_to(std::slice::from_ref(a), &dest).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "mine");
        std::fs::remove_file(dest.join("a.txt")).unwrap();
        folder::restore_to(std::slice::from_ref(a), &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("a.txt")).unwrap(),
            "hello"
        );
        assert!(!trash.join("a.txt").exists());

        folder::purge(&[f.clone(), b.clone()]).unwrap();
        assert!(folder::list(&roots).unwrap().is_empty());
        assert!(
            folder::purge(std::slice::from_ref(b)).is_err(),
            "already gone"
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_payload_is_the_files_entry() {
        let dir = test_dir();
        let (info_dir, files) = (dir.path().join("info"), dir.path().join("files"));
        std::fs::create_dir_all(&info_dir).unwrap();
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("a.txt"), "x").unwrap();
        let mut info = TrashInfo {
            original: PathBuf::from("/h/a.txt"),
            deleted: None,
            id: info_dir.join("a.txt.trashinfo").into_os_string(),
        };
        assert_eq!(info.payload(), Some(files.join("a.txt")));
        info.id = info_dir.join("gone.trashinfo").into_os_string();
        assert_eq!(info.payload(), None);
    }

    // The tests below use the real Recycle Bin: each trashes only files named
    // `keel-trash-test-*` that it created, and purges them again. A system with no usable
    // trash (CI without a desktop) skips.
    static BIN: Mutex<()> = Mutex::new(());

    /// A folder to delete test files from. On macOS NSFileManager moves a file into the
    /// Trash of the file's own volume and only the home volume's Trash is listed, so the
    /// folder is made under the home folder there (a temp folder can be on another volume,
    /// as on the CI machines).
    fn test_dir() -> tempfile::TempDir {
        #[cfg(target_os = "macos")]
        {
            if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
                if let Ok(d) = tempfile::Builder::new()
                    .prefix(".keel-trash-test-")
                    .tempdir_in(&home)
                {
                    return d;
                }
            }
        }
        tempfile::tempdir().unwrap()
    }

    fn unique(tag: &str) -> String {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("keel-trash-test-{tag}-{}-{n}.txt", std::process::id())
    }

    fn purge_named(name: &str) {
        if let Ok(items) = sys::list() {
            let ours: Vec<_> = items.into_iter().filter(|i| i.name == name).collect();
            let _ = sys::purge(&ours);
        }
    }

    /// Cleans up after a test even if it panics.
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            purge_named(&self.0);
        }
    }

    /// Trashes a new file; None (with a message) when this system cannot.
    fn trash_new(dir: &Path, name: &str) -> Option<PathBuf> {
        if !SUPPORTED || sys::list().is_err() {
            eprintln!("skipped: this system has no usable trash");
            return None;
        }
        let file = dir.join(name);
        std::fs::write(&file, "hello").unwrap();
        #[allow(unused_mut)]
        let mut bin = ::trash::TrashContext::default();
        // Finder would need the Automation permission (and a session that may ask).
        #[cfg(target_os = "macos")]
        ::trash::macos::TrashContextExtMacos::set_delete_method(
            &mut bin,
            ::trash::macos::DeleteMethod::NsFileManager,
        );
        if let Err(e) = bin.delete(&file) {
            eprintln!("skipped: cannot trash a file here: {e}");
            let _ = std::fs::remove_file(&file);
            return None;
        }
        Some(file)
    }

    /// Restore as the app does: to the original folder, or on macOS to the folder picked
    /// (here the one it was deleted from).
    fn restore(p: &VPath, picked: &Path) -> Result<()> {
        if KNOWS_ORIGIN {
            TrashProvider.restore_paths(std::slice::from_ref(p))
        } else {
            let err = TrashProvider
                .restore_paths(std::slice::from_ref(p))
                .unwrap_err();
            assert!(err.to_string().contains("Restore to"), "{err}");
            TrashProvider.restore_to(std::slice::from_ref(p), picked)
        }
    }

    fn find_ours(name: &str) -> Option<Entry> {
        let all = TrashProvider.list(&root()).unwrap();
        all.into_iter().find(|e| e.path.name() == name)
    }

    #[test]
    fn list_restore_purge_round_trip() {
        let _g = BIN.lock().unwrap_or_else(|e| e.into_inner());
        let name = unique("round");
        let _clean = Cleanup(name.clone());
        let dir = test_dir();
        let Some(file) = trash_new(dir.path(), &name) else {
            return;
        };
        assert!(!file.exists());
        let e = find_ours(&name).expect("listed");
        assert_eq!(e.size, 5);
        assert_eq!(e.kind, Kind::File);
        assert!(e.modified.is_some());
        let i = info(&e.path).expect("cached");
        assert_eq!(i.original.file_name().unwrap(), name.as_str());
        let payload = i.payload();
        eprintln!("payload path on this system: {payload:?}");
        if let Some(p) = payload {
            assert_eq!(std::fs::read_to_string(p).unwrap(), "hello");
            let mut s = String::new();
            TrashProvider
                .read(&e.path)
                .unwrap()
                .read_to_string(&mut s)
                .unwrap();
            assert_eq!(s, "hello");
        }

        restore(&e.path, dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
        assert!(find_ours(&name).is_none(), "restored items leave the bin");

        std::fs::remove_file(&file).unwrap();
        let file = trash_new(dir.path(), &name).unwrap();
        let e = find_ours(&name).expect("listed again");
        assert_eq!(TrashProvider.remove_kind(), RemoveKind::Permanent);
        TrashProvider.remove(&e.path).unwrap();
        assert!(find_ours(&name).is_none());
        assert!(!file.exists(), "a purged item does not come back");
        assert!(TrashProvider.restore_paths(&[e.path]).is_err());
    }

    #[test]
    fn restore_reports_an_existing_original() {
        let _g = BIN.lock().unwrap_or_else(|e| e.into_inner());
        let name = unique("clash");
        let _clean = Cleanup(name.clone());
        let dir = test_dir();
        let Some(file) = trash_new(dir.path(), &name) else {
            return;
        };
        std::fs::write(&file, "new").unwrap();
        let e = find_ours(&name).expect("listed");
        let err = restore(&e.path, dir.path()).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        assert!(find_ours(&name).is_some(), "still in the bin");
    }

    #[test]
    fn the_provider_is_read_only() {
        let p = root().join("x");
        assert!(!TrashProvider.caps().write && !TrashProvider.caps().rename);
        assert!(TrashProvider.write(&p).is_err());
        assert!(TrashProvider.mkdir(&p).is_err());
        assert!(TrashProvider.rename(&p, &root().join("y")).is_err());
    }
}
