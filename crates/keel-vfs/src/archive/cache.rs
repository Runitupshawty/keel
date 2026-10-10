use crate::VPath;
use anyhow::Result;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

/// Identifies one materialised entry: the outermost archive as it was (path, mtime, size)
/// plus the whole nested chain inside it, so editing the archive invalidates its entries.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub outer: VPath,
    pub modified: Option<SystemTime>,
    pub size: u64,
    /// Entire nested chain relative to the outermost archive, e.g. `a.tar!/b.zip`.
    pub inner: String,
}
struct Cached {
    /// Deletes the file when evicted or when the cache is dropped.
    path: tempfile::TempPath,
    size: u64,
    used: u64,
    /// Every `Pinned` handed out holds a clone; eviction skips entries still pinned.
    pin: Arc<()>,
}
/// A materialised file that is not evicted while this lives.
pub struct Pinned {
    path: PathBuf,
    _pin: Arc<()>,
}
impl Pinned {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
#[derive(Default)]
struct State {
    entries: HashMap<CacheKey, Cached>,
    clock: u64,
}
/// Archive entries copied to disk for previews, nested archives and "open with":
/// LRU by bytes under `<cache dir>/archives`, 2 GiB by default.
pub struct MaterialiseCache {
    root: PathBuf,
    budget: u64,
    state: Mutex<State>,
}
impl Default for MaterialiseCache {
    fn default() -> Self {
        Self::new(default_root(), 2 << 30)
    }
}
/// `<cache dir>/archives` (`crate::cache_dir`).
pub(crate) fn default_root() -> PathBuf {
    crate::cache_dir().join("archives")
}
impl MaterialiseCache {
    pub fn new(root: PathBuf, budget: u64) -> Self {
        // Files left by an earlier run (crash, or still open in another app at eviction) are
        // not tracked; sweep day-old ones so the folder cannot grow across sessions.
        for item in fs::read_dir(&root).into_iter().flatten().flatten() {
            let stale = item
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(24 * 3600));
            if stale && item.file_name().to_string_lossy().starts_with("keel-") {
                let _ = fs::remove_file(item.path());
            }
        }
        Self {
            root,
            budget,
            state: Mutex::new(State::default()),
        }
    }
    /// The returned file belongs to the cache and is never evicted while the `Pinned` lives;
    /// drop it when done reading. `extract` runs unlocked (a 1 GiB entry must not stall
    /// other previews, and nested archives re-enter the cache from inside it); a failed or
    /// over-budget extraction leaves nothing behind. When two callers race on one key, the
    /// first insert wins and the loser's copy is discarded.
    pub fn get_or_extract(
        &self,
        key: &CacheKey,
        extract: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<Pinned> {
        if let Some(pinned) = hit(&mut self.state.lock(), key) {
            return Ok(pinned);
        }
        fs::create_dir_all(&self.root)?;
        let ext = Path::new(&key.inner)
            .extension()
            .and_then(|s| s.to_str())
            .filter(|e| !e.is_empty() && e.bytes().all(|b| b.is_ascii_alphanumeric()))
            .unwrap_or("bin");
        let temp = tempfile::Builder::new()
            .prefix("keel-")
            .suffix(&format!(".{ext}"))
            .tempfile_in(&self.root)?
            .into_temp_path();
        extract(&temp)?;
        let size = fs::metadata(&temp)?.len();
        anyhow::ensure!(
            size <= self.budget,
            "TooLarge: entry exceeds the archive cache budget"
        );
        let mut state = self.state.lock();
        // Re-checked under the same lock as the insert: another thread may have won.
        if let Some(pinned) = hit(&mut state, key) {
            return Ok(pinned);
        }
        evict(&mut state, self.budget - size);
        state.clock += 1;
        let pin = Arc::new(());
        let pinned = Pinned {
            path: temp.to_path_buf(),
            _pin: pin.clone(),
        };
        let used = state.clock;
        state.entries.insert(
            key.clone(),
            Cached {
                path: temp,
                size,
                used,
                pin,
            },
        );
        Ok(pinned)
    }
    pub fn evict_to_budget(&self) {
        evict(&mut self.state.lock(), self.budget);
    }
}
fn hit(state: &mut State, key: &CacheKey) -> Option<Pinned> {
    state.clock += 1;
    let now = state.clock;
    match state.entries.get_mut(key) {
        Some(entry) if entry.path.is_file() => {
            entry.used = now;
            Some(Pinned {
                path: entry.path.to_path_buf(),
                _pin: entry.pin.clone(),
            })
        }
        Some(_) => {
            state.entries.remove(key);
            None
        }
        None => None,
    }
}
/// Drops least-recently-used unpinned entries until the total is within `budget` (pinned
/// ones can keep it over budget until they are released).
fn evict(state: &mut State, budget: u64) {
    let mut total: u64 = state.entries.values().map(|e| e.size).sum();
    if total <= budget {
        return;
    }
    let mut idle: Vec<_> = state
        .entries
        .iter()
        .filter(|(_, e)| Arc::strong_count(&e.pin) == 1)
        .map(|(key, e)| (e.used, key.clone()))
        .collect();
    idle.sort_unstable_by_key(|(used, _)| *used);
    for (_, key) in idle {
        if total <= budget {
            break;
        }
        if let Some(gone) = state.entries.remove(&key) {
            total -= gone.size;
        }
    }
}
