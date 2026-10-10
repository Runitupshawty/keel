//! File search backends (Everything or Keel's own NTFS index on Windows, Spotlight
//! on macOS, locate or a home-directory walk on Linux) and folder-path fuzzy
//! matching for Keel.

#[cfg(windows)]
mod composite;
#[cfg(windows)]
mod everything;
mod fuzzy;
#[cfg(target_os = "linux")]
mod locate;
#[cfg(windows)]
mod ntfs;
#[cfg(target_os = "macos")]
mod spotlight;
mod walkindex;

#[cfg(windows)]
pub use everything::EverythingSearcher;
pub use fuzzy::Fuzzy;
#[cfg(windows)]
pub use ntfs::{
    index_dir, is_elevated, request_full_index, run_index_service, NtfsSearcher, FALLBACK_STATUS,
    FROZEN_STATUS,
};
pub use walkindex::{home_root, index_dir as walk_index_dir, WalkIndexSearcher};

use std::path::Path;
use std::time::SystemTime;

/// Parameters accepted by a search backend.
#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    pub folders_only: bool,
    pub max: u32,
    pub regex: bool,
    pub match_case: bool,
    /// Fill in every hit's size and date (the default). The Keel index has to ask
    /// the file system per hit (cold disk: tens to hundreds of ms for 500 hits);
    /// with `false` it skips that and the caller fills them later with
    /// [`fill_meta`] (on a worker). Everything has them either way.
    pub meta: bool,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            text: String::new(),
            folders_only: false,
            max: 500,
            regex: false,
            match_case: false,
            meta: true,
        }
    }
}

/// A result returned by a search backend.
#[derive(Clone, Debug)]
pub struct Hit {
    pub path: keel_vfs::VPath,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// What a backend can do right now, for the status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchState {
    Ready,
    /// Building its index (`done` entries so far); queries fail until it is ready.
    Indexing {
        done: usize,
    },
    Unavailable,
}

/// A pluggable file search backend.
pub trait Searcher: Send + Sync {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>>;
    /// The last known state; never blocks.
    fn available(&self) -> bool;
    /// Checks again whether the backend works (e.g. Everything started since) and
    /// returns `available()`. May block on IPC: worker threads only.
    fn probe(&self) -> bool {
        self.available()
    }
    /// A note for the status bar about what the backend covers (e.g. "user folders
    /// only"), None when there is nothing to say.
    fn status(&self) -> Option<String> {
        None
    }
    /// The backend's name for the status bar ("Everything", "Keel index", ...).
    fn name(&self) -> &'static str {
        "Search"
    }
    /// Ready, still indexing, or unavailable. Never blocks.
    fn state(&self) -> SearchState {
        if self.available() {
            SearchState::Ready
        } else {
            SearchState::Unavailable
        }
    }
    /// The ready notification: true once each time the backend finished building
    /// (or swapped in) an index since the last call, so the caller refreshes what it
    /// derived from the old one (status, folder list). Poll it; never blocks.
    fn take_ready(&self) -> bool {
        false
    }
}

/// A soft-failure backend the app falls back to when no search backend works;
/// `reason` is shown to the user.
#[derive(Clone, Debug)]
pub struct Unavailable {
    pub reason: String,
}

impl Unavailable {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl Searcher for Unavailable {
    fn query(&self, _query: &Query) -> anyhow::Result<Vec<Hit>> {
        Err(anyhow::anyhow!(self.reason.clone()))
    }

    fn available(&self) -> bool {
        false
    }
}

/// The best search backend for this OS, or [`Unavailable`] when none works.
/// Probes the backend (IPC or a child process) and may load a saved index, so call
/// it off the UI thread.
///
/// Windows: Everything whenever it is running (checked per query, re-probed every
/// 30 s while it is down), else Keel's own index ([`NtfsSearcher`]: the saved drive
/// index, or the user-folder walk when there is none and Keel is not elevated); with
/// no index folder, the persistent walk index.
///
/// macOS and Linux: Spotlight / `plocate` / `locate` when they answer, else the
/// persistent walk index of the home folder ([`WalkIndexSearcher`]), else a plain walk.
pub fn default_searcher() -> Box<dyn Searcher> {
    #[cfg(windows)]
    return match (EverythingSearcher::load(), ntfs::index_dir()) {
        (Ok(everything), dir) => Box::new(composite::Composite::new(
            Box::new(everything),
            dir.map(|dir| -> Box<dyn Fn() -> Box<dyn Searcher> + Send + Sync> {
                Box::new(move || Box::new(NtfsSearcher::open(dir.clone())))
            }),
            composite::REPROBE,
        )),
        (Err(_), Some(dir)) => Box::new(NtfsSearcher::open(dir)),
        (Err(error), None) => walk_index_or(Unavailable::new(format!(
            "Everything search is unavailable: {error:#}"
        ))),
    };
    #[cfg(target_os = "macos")]
    if let Some(searcher) = spotlight::Spotlight::new() {
        return Box::new(searcher);
    }
    #[cfg(target_os = "linux")]
    if let Some(searcher) = locate::Locate::new() {
        return Box::new(searcher);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return walk_index_or(Unavailable::new(
        "no locate database and no home directory to search",
    ));
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    walk_index_or(Unavailable::new("no search backend on this platform"))
}

/// The persistent index of the home folder; if it cannot start, a plain walk of home;
/// `none` when there is no home either.
fn walk_index_or(none: Unavailable) -> Box<dyn Searcher> {
    let Some(home) = home_root() else {
        return Box::new(none);
    };
    match walkindex::index_dir().map(|dir| WalkIndexSearcher::open(dir, vec![home.clone()])) {
        Some(Ok(index)) => Box::new(index),
        _ => Box::new(HomeWalk(home)),
    }
}

/// Last resort: walk the home folder on every query.
struct HomeWalk(std::path::PathBuf);

impl Searcher for HomeWalk {
    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        Ok(walk(&self.0, query))
    }

    fn available(&self) -> bool {
        true
    }
}

/// Builds the display-path list consumed by the folder jump popup.
pub fn folder_index(searcher: &dyn Searcher) -> anyhow::Result<Vec<String>> {
    let hits = searcher.query(&Query {
        folders_only: true,
        max: 200_000,
        meta: false,
        ..Query::default()
    })?;

    Ok(hits.into_iter().map(|hit| hit.path.display()).collect())
}

/// fd-style name search under `root` with the `ignore` crate (hidden and
/// git-ignored entries are skipped). Matches a substring of the file name, or the
/// `regex` pattern when `query.regex` (an invalid pattern finds nothing),
/// case-insensitive unless `match_case`, and stops after `query.max` hits.
/// Blocks on disk IO: worker threads only.
pub fn walk(root: &Path, query: &Query) -> Vec<Hit> {
    let max = query.max as usize;
    let mut hits = Vec::new();
    if max == 0 {
        return hits;
    }
    let pattern = if query.regex {
        query.text.clone()
    } else {
        regex::escape(&query.text)
    };
    let Ok(matcher) = regex::RegexBuilder::new(&pattern)
        .case_insensitive(!query.match_case)
        .build()
    else {
        return hits;
    };
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if entry.depth() == 0 || !matcher.is_match(&entry.file_name().to_string_lossy()) {
            continue;
        }
        match hit_for_path(entry.path()) {
            Some(hit) if hit.is_dir || !query.folders_only => hits.push(hit),
            _ => continue,
        }
        if hits.len() >= max {
            break;
        }
    }
    hits
}

/// Fills in the size and date of `hits` (from a query run with `meta: false`) from
/// the file system, in parallel. Hits that are gone keep 0 / None. Blocks on disk IO:
/// worker threads only.
pub fn fill_meta(hits: &mut [Hit]) {
    let chunk = hits.len().div_ceil(8).max(64);
    std::thread::scope(|s| {
        for part in hits.chunks_mut(chunk) {
            s.spawn(move || {
                for hit in part {
                    if let Some((size, modified)) = hit.path.to_local_path().and_then(|p| stat(&p))
                    {
                        hit.size = size;
                        hit.modified = modified;
                    }
                }
            });
        }
    });
}

/// Size (0 for a folder) and modification time.
fn stat(path: &Path) -> Option<(u64, Option<SystemTime>)> {
    // One GetFileAttributesExW call, no file handle: cheaper than std's metadata.
    #[cfg(windows)]
    if let Some(meta) = ntfs::file_meta(path) {
        return Some(meta);
    }
    let meta = std::fs::metadata(path).ok()?;
    let size = if meta.is_dir() { 0 } else { meta.len() };
    Some((size, meta.modified().ok()))
}

fn hit_for_path(path: &Path) -> Option<Hit> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(Hit {
        path: keel_vfs::VPath::local(path),
        is_dir: metadata.is_dir(),
        size: if metadata.is_dir() { 0 } else { metadata.len() },
        modified: metadata.modified().ok(),
    })
}

/// Runs a tool that prints one path per line and keeps up to `query.max` hits
/// (directories only when `folders_only`), then kills it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn command_hits(mut command: std::process::Command, query: &Query) -> anyhow::Result<Vec<Hit>> {
    use std::io::BufRead;
    use std::os::unix::ffi::OsStrExt;
    use std::process::Stdio;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut hits = Vec::new();
    for line in std::io::BufReader::new(stdout).split(b'\n') {
        if hits.len() >= query.max as usize {
            break;
        }
        let line = line?;
        match hit_for_path(Path::new(std::ffi::OsStr::from_bytes(&line))) {
            Some(hit) if hit.is_dir || !query.folders_only => hits.push(hit),
            _ => {}
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::{default_searcher, folder_index, walk, Fuzzy, Query, Searcher, Unavailable};

    #[test]
    fn query_defaults_to_five_hundred_results() {
        assert_eq!(Query::default().max, 500);
    }

    #[test]
    fn unavailable_searcher_fails_softly() {
        let searcher = Unavailable::new("Everything is not running");

        assert!(!searcher.available());
        assert_eq!(
            searcher.query(&Query::default()).unwrap_err().to_string(),
            "Everything is not running"
        );
    }

    #[test]
    fn default_searcher_reports_availability_without_panicking() {
        let _ = default_searcher().available();
    }

    #[test]
    fn walk_finds_files_and_folders_by_name() {
        let root = std::env::temp_dir().join(format!("keel-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub").join("NeedleDir")).unwrap();
        std::fs::write(root.join("sub").join("KeelNeedle.txt"), b"x").unwrap();
        std::fs::write(root.join("other.txt"), b"x").unwrap();
        let query = |text: &str| Query {
            text: text.into(),
            ..Query::default()
        };

        let hits = walk(&root, &query("keelneedle"));
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.display().ends_with("KeelNeedle.txt"));
        assert!(!hits[0].is_dir);
        assert_eq!(hits[0].size, 1);

        let folders = walk(
            &root,
            &Query {
                folders_only: true,
                ..query("needle")
            },
        );
        assert_eq!(folders.len(), 1);
        assert!(folders[0].is_dir);

        let exact_case = Query {
            match_case: true,
            ..query("keelneedle")
        };
        assert!(walk(&root, &exact_case).is_empty());
        let capped = Query {
            max: 1,
            ..query("")
        };
        assert_eq!(walk(&root, &capped).len(), 1);

        // Polish backlog: `regex` is a real pattern, not a literal substring.
        let regex = |text: &str| Query {
            regex: true,
            ..query(text)
        };
        let names = |hits: Vec<super::Hit>| -> Vec<String> {
            hits.iter().map(|h| h.path.name().to_owned()).collect()
        };
        assert_eq!(
            names(walk(&root, &regex(r"^keel\w+\.txt$"))),
            ["KeelNeedle.txt"]
        );
        assert!(walk(&root, &regex(r"^needle$")).is_empty(), "anchored");
        assert!(walk(&root, &regex("(unclosed")).is_empty(), "bad pattern");
        // Without `regex`, pattern characters are literal.
        assert!(walk(&root, &query(r"needle\.txt")).is_empty());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn fuzzy_path_matching_ranks_expected_folders() {
        let mut fuzzy = Fuzzy::new(vec![
            r"D:\Work\home\filemgr".into(),
            r"C:\Users\james\Obsidian".into(),
        ]);

        assert_eq!(fuzzy.search("wkhm", 10)[0].1, r"D:\Work\home\filemgr");
        assert_eq!(fuzzy.search("obs", 10)[0].1, r"C:\Users\james\Obsidian");
        assert_eq!(fuzzy.search("", 10).len(), 2);
    }

    #[test]
    fn replacing_fuzzy_items_discards_old_items() {
        let mut fuzzy = Fuzzy::new(vec!["old".into()]);
        fuzzy.set_items(vec!["new".into()]);

        assert!(fuzzy.search("old", 10).is_empty());
        assert_eq!(fuzzy.search("new", 10)[0].1, "new");
    }

    #[test]
    fn folder_index_asks_for_folders_only() {
        #[derive(Default)]
        struct RecordingSearcher(std::sync::Mutex<Option<Query>>);

        impl Searcher for RecordingSearcher {
            fn query(&self, query: &Query) -> anyhow::Result<Vec<super::Hit>> {
                *self.0.lock().unwrap() = Some(query.clone());
                Ok(Vec::new())
            }

            fn available(&self) -> bool {
                true
            }
        }

        let searcher = RecordingSearcher::default();
        assert!(folder_index(&searcher).unwrap().is_empty());
        let query = searcher.0.lock().unwrap().clone().unwrap();
        assert!(query.folders_only);
        assert!(query.text.is_empty());
        assert_eq!(query.max, 200_000);
        assert!(!query.meta, "folder paths need no size or date");
    }

    #[test]
    fn fill_meta_adds_size_and_date() {
        let dir = std::env::temp_dir().join(format!("keel-fill-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("five.txt"), b"12345").unwrap();
        let hit = |path: std::path::PathBuf, is_dir| super::Hit {
            path: keel_vfs::VPath::local(path),
            is_dir,
            size: 0,
            modified: None,
        };
        let mut hits = vec![
            hit(dir.join("five.txt"), false),
            hit(dir.clone(), true),
            hit(dir.join("gone.txt"), false),
        ];
        super::fill_meta(&mut hits);
        assert_eq!(hits[0].size, 5);
        assert!(hits[0].modified.is_some());
        assert_eq!(hits[1].size, 0);
        assert!(hits[1].modified.is_some());
        assert_eq!((hits[2].size, hits[2].modified), (0, None));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
