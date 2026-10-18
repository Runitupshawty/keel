//! File search backends (Everything or Keel's own NTFS index on Windows, Spotlight
//! on macOS, locate or a home-directory walk on Linux) and folder-path fuzzy
//! matching for Keel, file lists (EFU and plain lists: read, search, export, create) and an ETP
//! server (Everything's, or keel-daemon's) as a backend ([`etp::EtpSearcher`]).

mod composite;
pub mod etp;
#[cfg(windows)]
mod everything;
mod exclude;
pub mod filelist;
mod folders;
mod fswalk;
/// Fuzz entry points (`fuzz/` and the in-tree runs; docs/fuzzing.md).
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzz;
mod fuzzy;
mod index_settings;
pub mod journal;
mod language;
#[cfg(target_os = "linux")]
mod locate;
/// The raw MFT record parser: used by the Windows index, built everywhere for fuzzing.
#[cfg(any(windows, test, feature = "fuzzing"))]
#[path = "ntfs/mft.rs"]
mod mft;
#[cfg(windows)]
mod ntfs;
pub mod propindex;
/// The index service's wire protocol: used on Windows, built everywhere for fuzzing.
#[cfg(any(windows, test, feature = "fuzzing"))]
#[path = "ntfs/proto.rs"]
mod proto;
/// Where the MFT lies on a volume and the raw read of its records.
#[cfg(any(windows, test, feature = "fuzzing"))]
#[path = "ntfs/raw.rs"]
mod raw;
#[cfg(windows)]
mod rdcw;
pub mod runs;
mod schedule;
#[cfg(target_os = "macos")]
mod spotlight;
pub mod sysindex;
mod terms;
mod volumes;
mod walkindex;
/// What the elevated index helper hands the unelevated Keel: raw MFT records.
#[cfg(any(windows, test, feature = "fuzzing"))]
#[path = "ntfs/wire.rs"]
mod wire;

pub use composite::Merge;
#[cfg(windows)]
pub use everything::EverythingSearcher;
pub use exclude::{attr, ExcludeRules};
pub use filelist::FileListSearcher;
pub use folders::FolderIndexes;
pub use fuzzy::Fuzzy;
pub use index_settings::{
    split_list, ExcludeSettings, FileListRoot, FolderRoot, IndexSettings, IndexedProperty,
    JournalSettings, PropertySettings, ScanOptions, Schedule, VolumeRoot, DEFAULT_JOURNAL_MB,
};
pub use language::{
    backend_query, compile_query, evaluate_hits, Evaluated, HitFilter, EVALUATED_SCAN,
};
#[cfg(windows)]
pub use ntfs::{
    index_dir, install_service, is_elevated, request_full_index, request_service_change,
    run_index_service, run_service, service_state, set_hide_unlistable, uninstall_service,
    NtfsSearcher, ServiceState, FALLBACK_STATUS, FROZEN_STATUS,
};
pub use runs::RunHistorySettings;
pub use schedule::{parse_time_of_day, Clock, SystemClock};
pub use terms::Highlighter;
pub use volumes::{SystemVolumes, VolumeInfo, VolumeKind, VolumeSource};
pub use walkindex::{
    home_root, index_dir as walk_index_dir, EntryInfo, WalkIndexSearcher, WalkOptions,
};
// Task 85: the system cleanups' one-shot elevated helper.
#[cfg(windows)]
pub use ntfs::run_elevated;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Parameters accepted by a search backend.
#[derive(Clone, Debug)]
pub struct Query {
    /// The search language ([`compile_query`]): Everything's syntax, parsed by
    /// `keel-query`. Keel's own indexes evaluate all of it; Everything gets the text as
    /// typed.
    pub text: String,
    pub folders_only: bool,
    pub max: u32,
    /// The whole text is one regular expression.
    pub regex: bool,
    pub match_case: bool,
    /// Match Whole Words: a plain term matches only whole words of the name.
    pub whole_word: bool,
    /// Match Prefix: a plain term matches only at the start of a word.
    pub match_prefix: bool,
    /// Match Suffix: a plain term matches only at the end of a word.
    pub match_suffix: bool,
    /// Ignore Punctuation: `a-b` finds `ab`.
    pub ignore_punctuation: bool,
    /// Ignore White-space: `a b` finds `ab`.
    pub ignore_whitespace: bool,
    /// Match Path: terms match the full path (folders and name), not only the name.
    pub match_path: bool,
    /// Match Diacritics: `cafe` does not find `café`. Off (the default), letters match
    /// their accented forms both ways.
    pub match_diacritics: bool,
    /// Match whole filename when using wildcards (the default): `*.pdf` must match the
    /// whole name; off, a wildcard term may match anywhere in it.
    pub whole_filename_wildcards: bool,
    /// Fill in every hit's size and date (the default). The Keel index has to ask
    /// the file system per hit (cold disk: tens to hundreds of ms for 500 hits);
    /// with `false` it skips that and the caller fills them later with
    /// [`fill_meta`] (on a worker). Everything has them either way.
    pub meta: bool,
    /// Only hits under this folder (a pane's search box). A typed `in:` in `text` takes
    /// its place where the backend reads one.
    pub within: Option<PathBuf>,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            match_prefix: false,
            match_suffix: false,
            ignore_punctuation: false,
            ignore_whitespace: false,
            text: String::new(),
            folders_only: false,
            max: 500,
            regex: false,
            match_case: false,
            whole_word: false,
            match_path: false,
            match_diacritics: false,
            whole_filename_wildcards: true,
            meta: true,
            within: None,
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
    /// The library's hits found by the text inside the document: a snippet of it.
    pub snippet: Option<Snippet>,
}

/// A line of a document's text around the words that matched (the library's content
/// search): `marks` are byte ranges of those words in `text`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    pub marks: Vec<(usize, usize)>,
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
    /// Whether the backend evaluates the whole search language and every [`Query`] option
    /// itself (Keel's own indexes do; Everything does with its own, and is asked for the
    /// text as typed). A backend that does not gets a superset ([`backend_query`]) and its
    /// hits go through [`evaluate_hits`], or is wrapped in [`Evaluated`].
    fn unified_query(&self) -> bool {
        false
    }
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
    /// Indexed folders and volumes that are offline: their hits come from the last
    /// scan (Settings → Search keeps offline volumes searchable unless
    /// **Automatically remove offline volumes** is on). Never blocks.
    fn offline_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    /// Settings → Search changed: index these folders and volumes from now on. May
    /// list volumes and open indexes: worker threads only.
    fn set_index_settings(&self, _settings: &IndexSettings) {}
    /// The settings as the backend changed them on its own (a new volume included,
    /// an offline one removed), once per change, for the app to save. Never blocks.
    fn index_settings_changed(&self) -> Option<IndexSettings> {
        None
    }
    /// Whether the index holds `dir` (a pane's folder, [`Query::within`]); Keel's own
    /// indexes walk a folder they do not hold instead. Backends over everything: true.
    fn covers(&self, _dir: &Path) -> bool {
        true
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
///
/// Keel's own index searches the folders and volumes of `IndexSettings::default()`
/// with it ([`default_searcher_with`] takes the user's).
pub fn default_searcher() -> Box<dyn Searcher> {
    default_searcher_with(&IndexSettings::default())
}

/// [`default_searcher`] with the folders and volumes of Settings → Search
/// ([`FolderIndexes`], merged with Keel's own index; Everything has its own list), and
/// the file lists of its File lists tab added to whatever answers
/// ([`filelist::FileListIndexes`], read on a background thread).
///
/// Every backend answers `si:` terms through the Windows Search index
/// ([`sysindex::SystemIndexed`]).
pub fn default_searcher_with(settings: &IndexSettings) -> Box<dyn Searcher> {
    Box::new(sysindex::SystemIndexed::new(Box::new(
        filelist::WithFileLists::new(
            base_searcher(settings),
            filelist::FileListIndexes::open(settings),
        ),
    )))
}

/// [`default_searcher_with`] without the file lists and `si:`.
fn base_searcher(settings: &IndexSettings) -> Box<dyn Searcher> {
    #[cfg(windows)]
    return match (EverythingSearcher::load(), ntfs::index_dir()) {
        (Ok(everything), dir) => {
            let settings = settings.clone();
            Box::new(composite::Composite::new(
                Box::new(everything),
                dir.map(|dir| -> Box<dyn Fn() -> Box<dyn Searcher> + Send + Sync> {
                    Box::new(move || {
                        let ntfs = Box::new(NtfsSearcher::open(dir.clone()));
                        with_folders(ntfs, &dir, &settings)
                    })
                }),
                composite::REPROBE,
            ))
        }
        (Err(_), Some(dir)) => {
            with_folders(Box::new(NtfsSearcher::open(dir.clone())), &dir, settings)
        }
        (Err(error), None) => walk_index_or(Unavailable::new(format!(
            "Everything search is unavailable: {error:#}"
        ))),
    };
    #[cfg(not(windows))]
    {
        let base = unix_searcher();
        match walkindex::index_dir() {
            Some(dir) => with_folders(base, &dir, settings),
            None => base,
        }
    }
}

/// `main` searched together with the folder and volume indexes (dbs in `dir`).
fn with_folders(
    main: Box<dyn Searcher>,
    dir: &Path,
    settings: &IndexSettings,
) -> Box<dyn Searcher> {
    let folders = FolderIndexes::open(dir.join("folders"), settings.clone());
    Box::new(Merge::new(main, Box::new(folders)))
}

#[cfg(not(windows))]
fn unix_searcher() -> Box<dyn Searcher> {
    #[cfg(target_os = "macos")]
    if let Some(searcher) = spotlight::Spotlight::new() {
        return Box::new(Evaluated::new(Box::new(searcher)));
    }
    #[cfg(target_os = "linux")]
    if let Some(searcher) = locate::Locate::new() {
        return Box::new(Evaluated::new(Box::new(searcher)));
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return walk_index_or(Unavailable::new(
        "no locate database and no home directory to search",
    ));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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

/// Splits the first `in:<folder>` term (`in:"a folder"` when it has spaces) off a query
/// that is not a regex: the folder, and the rest of the text as typed. Keel's own indexes
/// read `in:` themselves; the backends whose syntax has none use this (Everything gets a
/// path term, Spotlight `-onlyin`, locate a path filter).
pub fn take_within(text: &str) -> (Option<String>, String) {
    let mut start = None;
    let mut quoted = false;
    let mut found = None;
    for (i, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && (!quoted || i == text.len()) => {
                if let Some(s) = start.take() {
                    let term = &text[s..i];
                    if term
                        .get(..3)
                        .is_some_and(|head| head.eq_ignore_ascii_case("in:"))
                    {
                        found = Some((s, i));
                        break;
                    }
                }
            }
            _ if start.is_none() => start = Some(i),
            _ => {}
        }
        if c == '"' && start.is_none() {
            start = Some(i);
        }
    }
    let Some((s, e)) = found else {
        return (None, text.to_owned());
    };
    let folder: String = text[s + 3..e].chars().filter(|&c| c != '"').collect();
    let rest = format!("{} {}", text[..s].trim_end(), text[e..].trim_start());
    (
        Some(folder).filter(|f| !f.is_empty()),
        rest.trim().to_owned(),
    )
}

/// The folder `query` keeps to and its text without it: a typed `in:<folder>` (outside
/// regex mode, see [`take_within`]), else [`Query::within`].
pub(crate) fn scope(query: &Query) -> (Option<PathBuf>, String) {
    let (typed, text) = match query.regex {
        false => take_within(&query.text),
        true => (None, query.text.clone()),
    };
    (typed.map(PathBuf::from).or(query.within.clone()), text)
}

/// [`Query::within`] when no typed `in:` takes its place.
pub(crate) fn pane_folder(query: &Query) -> Option<&Path> {
    query
        .within
        .as_deref()
        .filter(|_| !language::has_typed_in(query))
}

/// Whether `path` is `dir` or below it, by whole names; Windows ignores case.
pub fn is_under(path: &Path, dir: &Path) -> bool {
    #[cfg(windows)]
    {
        let low = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        low(path).starts_with(low(dir))
    }
    #[cfg(not(windows))]
    path.starts_with(dir)
}

/// Last resort: walk the home folder (a typed `in:` or [`Query::within`]: that folder) on
/// every query.
struct HomeWalk(std::path::PathBuf);

impl Searcher for HomeWalk {
    fn unified_query(&self) -> bool {
        true
    }

    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        let dir = language::folder_scope(query)?;
        try_walk(dir.as_deref().unwrap_or(&self.0), query)
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

/// fd-style search under `root` with the `ignore` crate (hidden and git-ignored entries
/// are skipped), in the shared search language ([`compile_query`]); stops after
/// `query.max` hits. A query that does not parse finds nothing ([`Searcher::query`] of the
/// backends reports why). Blocks on disk IO: worker threads only.
pub fn walk(root: &Path, query: &Query) -> Vec<Hit> {
    try_walk(root, query).unwrap_or_default()
}

/// [`walk`] with the parse error of a bad query.
pub(crate) fn try_walk(root: &Path, query: &Query) -> anyhow::Result<Vec<Hit>> {
    fswalk::walk(root, query)
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn hit_for_path(path: &Path) -> Option<Hit> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(Hit {
        path: keel_vfs::VPath::local(path),
        is_dir: metadata.is_dir(),
        size: if metadata.is_dir() { 0 } else { metadata.len() },
        modified: metadata.modified().ok(),
        snippet: None,
    })
}

/// Runs a tool that prints one path per line and keeps up to `query.max` hits
/// (directories only when `folders_only`, and only under `within` when given), then
/// kills it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn command_hits(
    mut command: std::process::Command,
    query: &Query,
    within: Option<&Path>,
) -> anyhow::Result<Vec<Hit>> {
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
        let path = Path::new(std::ffi::OsStr::from_bytes(&line));
        if within.is_some_and(|w| !is_under(path, w)) {
            continue;
        }
        match hit_for_path(path) {
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

        // Match Path, Match Whole Words, diacritics.
        assert_eq!(walk(&root, &query("sub")).len(), 1, "the folder only");
        let path = Query {
            match_path: true,
            ..query("sub")
        };
        assert_eq!(walk(&root, &path).len(), 3, "the folder and all below it");
        let whole = Query {
            whole_word: true,
            ..query("needle")
        };
        assert!(walk(&root, &whole).is_empty());
        std::fs::write(root.join("café menu.txt"), b"x").unwrap();
        assert_eq!(names(walk(&root, &query("cafe"))), ["café menu.txt"]);
        let exact = Query {
            match_diacritics: true,
            ..query("cafe")
        };
        assert!(walk(&root, &exact).is_empty());
        let whole = Query {
            whole_word: true,
            ..query("menu")
        };
        assert_eq!(names(walk(&root, &whole)), ["café menu.txt"]);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn takes_the_in_term_off_a_query() {
        use super::take_within;
        assert_eq!(take_within("report"), (None, "report".into()));
        assert_eq!(
            take_within(r"in:C:\Users report"),
            (Some(r"C:\Users".into()), "report".into())
        );
        assert_eq!(
            take_within(r#"two "words here" IN:"C:\a b" ext:pdf"#),
            (Some(r"C:\a b".into()), r#"two "words here" ext:pdf"#.into())
        );
        assert_eq!(
            take_within("x in:/home/me"),
            (Some("/home/me".into()), "x".into())
        );
        // Only the first; a quoted "in:" is a phrase; an empty one is dropped.
        assert_eq!(
            take_within("in:/a in:/b"),
            (Some("/a".into()), "in:/b".into())
        );
        assert_eq!(take_within(r#""in:x" y"#), (None, r#""in:x" y"#.into()));
        assert_eq!(take_within("in: y"), (None, "y".into()));
        assert_eq!(
            take_within(r#"in:"/a b"#),
            (Some("/a b".into()), String::new())
        );
        assert_eq!(take_within("ünï in:/ü"), (Some("/ü".into()), "ünï".into()));
    }

    /// `Query::within` (a pane's folder) for the backends that read `scope` (Everything,
    /// Spotlight, locate) and the home walk; a typed `in:` takes its place.
    #[test]
    fn a_pane_folder_scopes_the_query() {
        use super::{pane_folder, scope, HomeWalk};
        use std::path::Path;
        use std::path::PathBuf;
        let q = |text: &str, regex, within: Option<&str>| Query {
            text: text.into(),
            regex,
            within: within.map(PathBuf::from),
            ..Query::default()
        };
        let pane = Some(PathBuf::from("/pane"));
        assert_eq!(
            scope(&q("x", false, Some("/pane"))),
            (pane.clone(), "x".into())
        );
        let typed = (Some(PathBuf::from("/typed")), "x".into());
        assert_eq!(scope(&q("x in:/typed", false, Some("/pane"))), typed);
        // A regex is the whole text; the pane's folder still applies.
        let regex = (pane, "in:/x ^a".into());
        assert_eq!(scope(&q("in:/x ^a", true, Some("/pane"))), regex);
        assert_eq!(scope(&q("x", false, None)), (None, "x".into()));

        let root = std::env::temp_dir().join(format!("keel-walk-within-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["a", "b"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("needle.txt"), b"x").unwrap();
        }
        let home = HomeWalk(root.clone());
        assert_eq!(home.query(&q("needle", false, None)).unwrap().len(), 2);
        let a = root.join("a").display().to_string();
        let hits = home.query(&q("needle", false, Some(&a))).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.display().starts_with(&a));
        // A typed in: is walked too, without the term in the text.
        let b = root.join("b").display().to_string();
        let typed = home.query(&q(&format!("needle in:\"{b}\""), false, Some(&a)));
        let typed = typed.unwrap();
        assert_eq!(typed.len(), 1);
        assert!(typed[0].path.display().starts_with(&b));
        // A pane's folder is the query's unless a typed in: replaces it.
        let pane = q("x", false, Some("/pane"));
        assert_eq!(pane_folder(&pane), Some(Path::new("/pane")));
        assert_eq!(pane_folder(&q("x in:/t", false, Some("/pane"))), None);
        assert_eq!(
            pane_folder(&q("x in:/t", true, Some("/pane"))),
            pane_folder(&pane)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn is_under_takes_whole_names() {
        use super::is_under;
        use std::path::Path;
        let p = Path::new;
        assert!(is_under(p("/a/b/c.txt"), p("/a/b")) && is_under(p("/a/b"), p("/a/b")));
        assert!(!is_under(p("/a/bc/d"), p("/a/b")));
        #[cfg(windows)]
        assert!(is_under(p(r"C:\Users\Ann\x.txt"), p(r"c:\users\ann")));
    }

    #[test]
    fn fuzzy_path_matching_ranks_expected_folders() {
        let mut fuzzy = Fuzzy::new(vec![
            r"D:\Work\home\filemgr".into(),
            r"C:\Users\me\Obsidian".into(),
        ]);

        assert_eq!(fuzzy.search("wkhm", 10)[0].1, r"D:\Work\home\filemgr");
        assert_eq!(fuzzy.search("obs", 10)[0].1, r"C:\Users\me\Obsidian");
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
            snippet: None,
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
