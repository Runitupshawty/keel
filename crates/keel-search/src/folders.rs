//! The folder and volume indexes of Settings → Search: one [`WalkIndexSearcher`]
//! per folder of the Folders list and per walked volume (FAT, exFAT, ReFS, removable,
//! network), searched together. A poll every [`POLL`] notices volumes that appear or
//! go away: new fixed / removable volumes are included when their switch is on, and an
//! offline volume is either kept searchable from its last scan (marked offline) or,
//! with **Automatically remove offline volumes**, dropped with its saved index.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;

use crate::exclude::ExcludeRules;
use crate::index_settings::{IndexSettings, ScanOptions, VolumeRoot};
use crate::schedule::{Clock, SystemClock};
use crate::volumes::{same_root, SystemVolumes, VolumeInfo, VolumeKind, VolumeSource};
use crate::walkindex::{self, merge_hits, WalkIndexSearcher, WalkOptions};
use crate::{Hit, Query, SearchState, Searcher};

/// How often the volume list is checked.
pub const POLL: Duration = Duration::from_secs(10);

/// One root to index and how.
#[derive(Clone, Debug, PartialEq)]
struct Planned {
    root: PathBuf,
    scan: ScanOptions,
}

/// What the settings and the mounted volumes ask for: the roots to index, the roots
/// whose saved index should be deleted, and whether `settings` changed (volumes
/// included or removed automatically).
fn plan(s: &mut IndexSettings, volumes: &[VolumeInfo]) -> (Vec<Planned>, Vec<PathBuf>, bool) {
    let mut changed = false;
    for v in volumes.iter().filter(|v| v.needs_walk()) {
        if s.volumes
            .iter()
            .any(|e| same_root(Path::new(&e.root), &v.root))
        {
            continue;
        }
        let auto = match v.kind {
            VolumeKind::Fixed => s.auto_include_fixed,
            VolumeKind::Removable => s.auto_include_removable,
            // Shares are added by hand, as in Everything.
            VolumeKind::Network => false,
        };
        if auto {
            s.volumes.push(VolumeRoot {
                root: v.root.to_string_lossy().into_owned(),
                ..VolumeRoot::default()
            });
            changed = true;
        }
    }
    let online = |e: &VolumeRoot| {
        volumes
            .iter()
            .find(|v| same_root(Path::new(&e.root), &v.root))
    };
    let mut deleted = Vec::new();
    if s.auto_remove_offline {
        s.volumes.retain(|e| {
            let keep = online(e).is_some();
            if !keep {
                deleted.extend(volume_roots(e));
                changed = true;
            }
            keep
        });
    }
    let mut planned = Vec::new();
    for e in &s.volumes {
        // A fixed NTFS volume is the NTFS index's.
        if !e.include || online(e).is_some_and(|v| !v.needs_walk()) {
            continue;
        }
        planned.extend(volume_roots(e).into_iter().map(|root| Planned {
            root,
            scan: e.scan.clone(),
        }));
    }
    planned.extend(
        (s.folders.iter())
            .filter(|f| !f.path.trim().is_empty())
            .map(|f| Planned {
                root: PathBuf::from(f.path.trim()),
                scan: f.scan.clone(),
            }),
    );
    // One index per root: a root inside another one is already covered.
    let all: Vec<PathBuf> = planned.iter().map(|p| p.root.clone()).collect();
    let mut kept: Vec<Planned> = Vec::new();
    for p in planned {
        let inside = all
            .iter()
            .any(|o| !same_root(o, &p.root) && p.root.starts_with(o));
        if !inside && !kept.iter().any(|k| same_root(&k.root, &p.root)) {
            kept.push(p);
        }
    }
    (kept, deleted, changed)
}

/// The folders a volume entry indexes: its Include only folders (relative to its
/// root, or absolute), else its root.
fn volume_roots(e: &VolumeRoot) -> Vec<PathBuf> {
    let root = PathBuf::from(&e.root);
    let only: Vec<PathBuf> = (e.include_only.iter())
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .map(|f| {
            let f = Path::new(f);
            if f.is_absolute() {
                f.to_path_buf()
            } else {
                root.join(f)
            }
        })
        .collect();
    if only.is_empty() {
        vec![root]
    } else {
        only
    }
}

struct Open {
    planned: Planned,
    rules: ExcludeRules,
    index: Arc<WalkIndexSearcher>,
}

struct Inner {
    dir: PathBuf,
    source: Arc<dyn VolumeSource>,
    clock: Arc<dyn Clock>,
    settings: Mutex<IndexSettings>,
    open: Mutex<Vec<Open>>,
    /// `settings` changed on its own (volumes included or removed).
    changed: AtomicBool,
    /// The set of indexes changed since the last `take_ready`.
    swapped: AtomicBool,
}

impl Inner {
    /// Brings the open indexes in line with the settings and the mounted volumes.
    /// Lists volumes (may block on a network drive): worker threads only.
    fn refresh(&self) {
        let volumes = self.source.volumes();
        let (planned, deleted, changed, rules) = {
            let mut s = self.settings.lock();
            let (planned, deleted, changed) = plan(&mut s, &volumes);
            (planned, deleted, changed, ExcludeRules::new(&s.exclude))
        };
        if changed {
            self.changed.store(true, Relaxed);
        }
        let mut open = self.open.lock();
        let mut kept: Vec<Open> = Vec::new();
        let mut reopened = false;
        for p in planned {
            let same = open.iter().position(|o| o.planned == p && o.rules == rules);
            if let Some(i) = same {
                kept.push(open.swap_remove(i));
                continue;
            }
            let opts = WalkOptions {
                rules: Some(rules.clone()),
                scan: p.scan.clone(),
                max_age_secs: None,
                clock: self.clock.clone(),
            };
            // Close an older index of this root (other options) before reopening.
            open.retain(|o| !same_root(&o.planned.root, &p.root));
            if let Ok(index) =
                WalkIndexSearcher::open_with(self.dir.clone(), vec![p.root.clone()], opts)
            {
                kept.push(Open {
                    planned: p,
                    rules: rules.clone(),
                    index: Arc::new(index),
                });
                reopened = true;
            }
        }
        let dropped = !open.is_empty();
        *open = kept;
        drop(open);
        for root in deleted {
            walkindex::delete_db(&self.dir, &root);
        }
        if reopened || dropped {
            self.swapped.store(true, Relaxed);
        }
    }

    fn indexes(&self) -> Vec<Arc<WalkIndexSearcher>> {
        self.open.lock().iter().map(|o| o.index.clone()).collect()
    }
}

/// The folder and volume indexes, searched as one backend.
pub struct FolderIndexes {
    inner: Arc<Inner>,
    stop: Arc<AtomicBool>,
}

impl FolderIndexes {
    /// The indexes `settings` ask for, with their dbs in `dir`, kept in line with the
    /// mounted volumes by a background poll. Lists volumes: off the UI thread.
    pub fn open(dir: PathBuf, settings: IndexSettings) -> Self {
        let this = Self::with(
            dir,
            settings,
            Arc::new(SystemVolumes),
            Arc::new(SystemClock),
        );
        let (weak, stop) = (Arc::downgrade(&this.inner), this.stop.clone());
        let _ = std::thread::Builder::new()
            .name("keel-volumes".into())
            .spawn(move || poll(weak, stop));
        this
    }

    /// Without the poll, from any volume source and clock (tests).
    pub(crate) fn with(
        dir: PathBuf,
        settings: IndexSettings,
        source: Arc<dyn VolumeSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let inner = Arc::new(Inner {
            dir,
            source,
            clock,
            settings: Mutex::new(settings),
            open: Mutex::new(Vec::new()),
            changed: AtomicBool::new(false),
            swapped: AtomicBool::new(false),
        });
        inner.refresh();
        Self {
            inner,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Checks the volumes again now.
    pub fn refresh(&self) {
        self.inner.refresh();
    }

    /// The roots indexed now.
    pub fn roots(&self) -> Vec<PathBuf> {
        let open = self.inner.open.lock();
        open.iter().map(|o| o.planned.root.clone()).collect()
    }
}

impl Drop for FolderIndexes {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

fn poll(inner: Weak<Inner>, stop: Arc<AtomicBool>) {
    loop {
        std::thread::sleep(POLL);
        if stop.load(Relaxed) {
            return;
        }
        let Some(inner) = inner.upgrade() else {
            return;
        };
        inner.refresh();
    }
}

impl Searcher for FolderIndexes {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let mut hits = Vec::new();
        let mut error = None;
        for index in self.inner.indexes() {
            if !index.available() {
                continue;
            }
            match index.query(q) {
                Ok(h) => hits.extend(h),
                Err(e) => error = Some(e),
            }
        }
        match error {
            // A bad pattern fails in every index alike.
            Some(e) if hits.is_empty() => Err(e),
            _ => Ok(merge_hits(q, hits)),
        }
    }

    fn available(&self) -> bool {
        self.inner.indexes().iter().any(|i| i.available())
    }

    fn state(&self) -> SearchState {
        let indexes = self.inner.indexes();
        if indexes.iter().any(|i| i.state() == SearchState::Ready) {
            return SearchState::Ready;
        }
        let done: Option<usize> = indexes
            .iter()
            .filter_map(|i| match i.state() {
                SearchState::Indexing { done } => Some(done),
                _ => None,
            })
            .reduce(|a, b| a + b);
        done.map_or(SearchState::Unavailable, |done| SearchState::Indexing {
            done,
        })
    }

    fn status(&self) -> Option<String> {
        let notes: Vec<String> = self
            .inner
            .indexes()
            .iter()
            .filter_map(|i| i.status())
            .collect();
        (!notes.is_empty()).then(|| notes.join("; "))
    }

    fn name(&self) -> &'static str {
        "Keel index"
    }

    fn take_ready(&self) -> bool {
        let mut any = self.inner.swapped.swap(false, Relaxed);
        for index in self.inner.indexes() {
            any |= index.take_ready();
        }
        any
    }

    fn offline_roots(&self) -> Vec<PathBuf> {
        self.inner
            .indexes()
            .iter()
            .flat_map(|i| i.offline_roots())
            .collect()
    }

    fn set_index_settings(&self, settings: &IndexSettings) {
        {
            let mut s = self.inner.settings.lock();
            if *s == *settings {
                return;
            }
            *s = settings.clone();
        }
        self.inner.refresh();
    }

    fn index_settings_changed(&self) -> Option<IndexSettings> {
        self.inner
            .changed
            .swap(false, Relaxed)
            .then(|| self.inner.settings.lock().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_settings::{FolderRoot, Schedule};
    use crate::schedule::fake::FakeClock;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    /// Volumes the test mounts and unmounts by hand.
    #[derive(Default)]
    struct Fake(Mutex<Vec<VolumeInfo>>);

    impl VolumeSource for Fake {
        fn volumes(&self) -> Vec<VolumeInfo> {
            self.0.lock().clone()
        }
    }

    fn vol(root: &Path, kind: VolumeKind, fs: &str) -> VolumeInfo {
        VolumeInfo {
            root: root.to_path_buf(),
            kind,
            fs: fs.into(),
            label: String::new(),
        }
    }

    fn base() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "keel-folders-{}-{}",
            std::process::id(),
            N.fetch_add(1, Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn names(s: &dyn Searcher, text: &str) -> Vec<String> {
        let q = Query {
            text: text.into(),
            ..Query::default()
        };
        let mut n: Vec<String> = s
            .query(&q)
            .unwrap_or_default()
            .iter()
            .map(|h| h.path.name().to_owned())
            .collect();
        n.sort();
        n
    }

    fn settle(s: &dyn Searcher, text: &str, want: &[&str]) {
        let end = Instant::now() + Duration::from_secs(60);
        while names(s, text) != want {
            assert!(
                Instant::now() < end,
                "{text}: wanted {want:?}, got {:?}",
                names(s, text)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn clock() -> Arc<dyn Clock> {
        Arc::new(FakeClock::new(1_791_763_200, 0))
    }

    #[test]
    fn plans_volumes_and_folders() {
        let mut s = IndexSettings {
            auto_include_removable: true,
            ..IndexSettings::default()
        };
        s.volumes.push(VolumeRoot {
            root: "/v/off".into(),
            include: false,
            ..VolumeRoot::default()
        });
        s.volumes.push(VolumeRoot {
            root: "/v/photos".into(),
            include_only: vec!["2024".into(), "/v/photos/raw".into()],
            ..VolumeRoot::default()
        });
        s.folders.push(FolderRoot {
            path: "/v/photos/2024/inside".into(),
            ..FolderRoot::default()
        });
        s.folders.push(FolderRoot {
            path: "/data".into(),
            ..FolderRoot::default()
        });
        let vols = [
            vol(Path::new("/v/off"), VolumeKind::Removable, "vfat"),
            vol(Path::new("/v/photos"), VolumeKind::Removable, "exfat"),
            vol(Path::new("/v/usb"), VolumeKind::Removable, "vfat"),
            vol(Path::new("/v/share"), VolumeKind::Network, "cifs"),
        ];
        let (planned, deleted, changed) = plan(&mut s, &vols);
        let roots: Vec<PathBuf> = planned.iter().map(|p| p.root.clone()).collect();
        assert_eq!(
            roots,
            [
                PathBuf::from("/v/photos").join("2024"),
                PathBuf::from("/v/photos/raw"),
                PathBuf::from("/v/usb"),
                PathBuf::from("/data"),
            ]
        );
        assert!(changed, "the new removable volume was added");
        assert!(deleted.is_empty());
        assert!(s.volumes.iter().any(|v| v.root == "/v/usb"));
        assert!(
            !s.volumes.iter().any(|v| v.root == "/v/share"),
            "shares by hand"
        );
        // Settled: nothing changes the second time.
        let (_, _, changed) = plan(&mut s, &vols);
        assert!(!changed);
        // Offline volumes stay planned (searchable from their last scan)...
        let (planned, _, _) = plan(&mut s, &vols[..1]);
        assert_eq!(planned.len(), 4);
        // ...unless they are removed automatically.
        s.auto_remove_offline = true;
        let (planned, deleted, changed) = plan(&mut s, &vols[..1]);
        assert!(changed);
        assert_eq!(planned.len(), 2, "only the folders: {planned:?}");
        assert_eq!(deleted.len(), 3);
        assert_eq!(
            s.volumes.len(),
            1,
            "the offline ones are gone, /v/off stays"
        );
    }

    #[test]
    fn auto_include_follows_the_switches() {
        let mut s = IndexSettings::default();
        let fixed = vol(Path::new("/f"), VolumeKind::Fixed, "exFAT");
        let usb = vol(Path::new("/u"), VolumeKind::Removable, "FAT32");
        let (planned, _, _) = plan(&mut s, &[fixed.clone(), usb.clone()]);
        assert_eq!(planned.len(), 1, "fixed on, removable off by default");
        let mut fresh = IndexSettings {
            auto_include_fixed: false,
            ..IndexSettings::default()
        };
        assert!(plan(&mut fresh, &[fixed]).0.is_empty());
        if cfg!(windows) {
            let ntfs = vol(Path::new("C:\\"), VolumeKind::Fixed, "NTFS");
            let mut s = IndexSettings::default();
            assert!(
                plan(&mut s, &[ntfs]).0.is_empty(),
                "the NTFS index covers it"
            );
        }
    }

    #[test]
    fn volumes_are_searched_and_kept_or_removed_when_offline() {
        let b = base();
        let (usb, idx) = (b.join("usb"), b.join("idx"));
        std::fs::create_dir_all(usb.join("docs")).unwrap();
        std::fs::write(usb.join("docs").join("notes.txt"), b"x").unwrap();
        let folder = b.join("folder");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("plan.txt"), b"x").unwrap();
        let source = Arc::new(Fake::default());
        source
            .0
            .lock()
            .push(vol(&usb, VolumeKind::Removable, "exFAT"));
        let mut settings = IndexSettings {
            auto_include_removable: true,
            ..IndexSettings::default()
        };
        settings.folders.push(FolderRoot {
            path: folder.display().to_string(),
            ..FolderRoot::default()
        });
        let f = FolderIndexes::with(idx.clone(), settings, source.clone(), clock());
        assert_eq!(f.roots().len(), 2);
        settle(&f, ".txt", &["notes.txt", "plan.txt"]);
        let added = f.index_settings_changed().expect("the volume was added");
        assert_eq!(added.volumes.len(), 1);
        assert!(f.index_settings_changed().is_none());
        assert!(f.take_ready());
        // Unplugged: still searchable, marked offline.
        source.0.lock().clear();
        let away = b.join("usb-away");
        std::fs::rename(&usb, &away).unwrap();
        f.refresh();
        let end = Instant::now() + Duration::from_secs(60);
        while f.offline_roots().is_empty() {
            assert!(Instant::now() < end, "never offline");
            f.inner.indexes().iter().for_each(|i| i.rescan());
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(f.offline_roots().len(), 1);
        assert_eq!(names(&f, "notes"), ["notes.txt"]);
        assert!(f.status().unwrap().contains("offline"));
        // With Automatically remove offline volumes: dropped with its saved index.
        let mut s = added.clone();
        s.auto_remove_offline = true;
        let dbs = || {
            std::fs::read_dir(&idx)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".db")
                })
                .count()
        };
        assert_eq!(dbs(), 2);
        f.set_index_settings(&s);
        assert_eq!(f.roots(), std::slice::from_ref(&folder));
        assert!(names(&f, "notes").is_empty());
        let end = Instant::now() + Duration::from_secs(10);
        while dbs() != 1 {
            assert!(Instant::now() < end, "db not deleted");
            std::thread::sleep(Duration::from_millis(50));
            f.refresh();
        }
        assert!(f.index_settings_changed().unwrap().volumes.is_empty());
        drop(f);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn changed_options_reopen_the_index() {
        let b = base();
        let folder = b.join("folder");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a.log"), b"x").unwrap();
        std::fs::write(folder.join("b.txt"), b"x").unwrap();
        let mut s = IndexSettings::default();
        s.folders.push(FolderRoot {
            path: folder.display().to_string(),
            ..FolderRoot::default()
        });
        let f = FolderIndexes::with(b.join("idx"), s.clone(), Arc::new(Fake::default()), clock());
        settle(&f, "a.log", &["a.log"]);
        let first = Arc::as_ptr(&f.inner.indexes()[0]);
        // The same settings keep the index.
        f.set_index_settings(&s);
        assert_eq!(Arc::as_ptr(&f.inner.indexes()[0]), first);
        s.exclude.files = vec!["*.log".into()];
        s.folders[0].scan.schedule = Schedule::EveryHours(2);
        f.set_index_settings(&s);
        assert_ne!(Arc::as_ptr(&f.inner.indexes()[0]), first);
        // The new index serves the saved rows, then walks again by the new rules.
        settle(&f, "b.txt", &["b.txt"]);
        settle(&f, "a.log", &[]);
        drop(f);
        let _ = std::fs::remove_dir_all(&b);
    }
}
