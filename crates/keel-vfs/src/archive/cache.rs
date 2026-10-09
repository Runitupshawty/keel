use crate::VPath;
use anyhow::Result;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
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
    /// `%LOCALAPPDATA%\Keel\archives`, `~/Library/Caches/Keel/archives`, `~/.cache/keel/archives`.
    fn default() -> Self {
        let app = if cfg!(target_os = "linux") {
            "keel"
        } else {
            "Keel"
        };
        let root = directories::BaseDirs::new()
            .map(|dirs| dirs.cache_dir().join(app))
            .unwrap_or_else(|| std::env::temp_dir().join(app))
            .join("archives");
        Self::new(root, 2 << 30)
    }
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
    /// Returned paths belong to the cache and stay valid until evicted: open them promptly,
    /// never persist them. `extract` runs unlocked (a 1 GiB entry must not stall other
    /// previews, and nested archives re-enter the cache from inside it); a failed or
    /// over-budget extraction leaves nothing behind.
    pub fn get_or_extract(
        &self,
        key: &CacheKey,
        extract: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<PathBuf> {
        if let Some(path) = self.hit(key) {
            return Ok(path);
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
        // Another thread may have materialised the same key meanwhile: keep theirs.
        if let Some(path) = self.hit(key) {
            return Ok(path);
        }
        let mut state = self.state.lock();
        evict(&mut state, self.budget - size);
        state.clock += 1;
        let used = state.clock;
        let path = temp.to_path_buf();
        state.entries.insert(
            key.clone(),
            Cached {
                path: temp,
                size,
                used,
            },
        );
        Ok(path)
    }
    pub fn evict_to_budget(&self) {
        evict(&mut self.state.lock(), self.budget);
    }
    fn hit(&self, key: &CacheKey) -> Option<PathBuf> {
        let mut state = self.state.lock();
        state.clock += 1;
        let now = state.clock;
        match state.entries.get_mut(key) {
            Some(entry) if entry.path.is_file() => {
                entry.used = now;
                Some(entry.path.to_path_buf())
            }
            Some(_) => {
                state.entries.remove(key);
                None
            }
            None => None,
        }
    }
}
/// Drops least-recently-used entries until the total is within `budget`.
fn evict(state: &mut State, budget: u64) {
    let mut total: u64 = state.entries.values().map(|e| e.size).sum();
    while total > budget {
        let Some(key) = state
            .entries
            .iter()
            .min_by_key(|(_, e)| e.used)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if let Some(gone) = state.entries.remove(&key) {
            total -= gone.size;
        }
    }
}
