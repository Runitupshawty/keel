//! Windows search: Everything while it runs, else Keel's own index ([`Merge`] of the
//! NTFS index and the folder and volume indexes). Each query
//! checks Everything's cached availability (an atomic) and, while it is down,
//! re-probes it at most every [`REPROBE`], so starting or quitting Everything
//! after Keel takes effect within one query or 30 s.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::index_settings::IndexSettings;
use crate::{Hit, Query, SearchState, Searcher};

/// Least time between two probes of a stopped Everything.
pub const REPROBE: Duration = Duration::from_secs(30);

type Open = Box<dyn Fn() -> Box<dyn Searcher> + Send + Sync>;

pub struct Composite {
    primary: Box<dyn Searcher>,
    /// Opened on first need (or at once when the primary is down at start).
    fallback: OnceLock<Box<dyn Searcher>>,
    open: Option<Open>,
    probed: Mutex<Instant>,
    reprobe: Duration,
    /// Which one answered the last query, and whether that changed since
    /// `take_ready` (a backend switch is announced like a finished index).
    on_primary: AtomicBool,
    switched: AtomicBool,
    /// Index settings sent before the fallback opened, applied when it does.
    pending: Mutex<Option<IndexSettings>>,
}

impl Composite {
    /// `primary` (Everything) first, else what `open` returns (the Keel index),
    /// opened now when the primary is down, else when first needed.
    pub fn new(primary: Box<dyn Searcher>, open: Option<Open>, reprobe: Duration) -> Self {
        let this = Self {
            primary,
            fallback: OnceLock::new(),
            open,
            probed: Mutex::new(Instant::now()),
            reprobe,
            on_primary: AtomicBool::new(true),
            switched: AtomicBool::new(false),
            pending: Mutex::new(None),
        };
        if !this.primary.available() {
            this.fallback();
        }
        this
    }

    fn fallback(&self) -> Option<&dyn Searcher> {
        let open = self.open.as_ref()?;
        Some(
            self.fallback
                .get_or_init(|| {
                    let fallback = open();
                    let pending = self
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some(settings) = pending {
                        fallback.set_index_settings(&settings);
                    }
                    fallback
                })
                .as_ref(),
        )
    }

    /// The primary when it is up: its cached state, or a probe once `reprobe` has
    /// passed since the last one.
    fn live_primary(&self) -> Option<&dyn Searcher> {
        if self.primary.available() {
            return Some(self.primary.as_ref());
        }
        let mut probed = self.probed.lock().unwrap_or_else(|e| e.into_inner());
        if probed.elapsed() < self.reprobe {
            return None;
        }
        *probed = Instant::now();
        self.primary.probe().then_some(self.primary.as_ref())
    }

    fn answered_by(&self, primary: bool) {
        if self.on_primary.swap(primary, Ordering::Relaxed) != primary {
            self.switched.store(true, Ordering::Relaxed);
        }
    }
}

impl Searcher for Composite {
    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        if let Some(primary) = self.live_primary() {
            match primary.query(query) {
                Ok(hits) => {
                    self.answered_by(true);
                    return Ok(hits);
                }
                // Still up: a bad query, not a stopped backend.
                Err(e) if primary.available() || self.open.is_none() => return Err(e),
                Err(_) => {}
            }
        }
        match self.fallback() {
            Some(fallback) => {
                self.answered_by(false);
                fallback.query(query)
            }
            None => self.primary.query(query),
        }
    }

    fn state(&self) -> SearchState {
        match self.fallback.get() {
            Some(fallback) if !self.primary.available() => fallback.state(),
            _ => self.primary.state(),
        }
    }

    fn take_ready(&self) -> bool {
        let fallback = self.fallback.get().is_some_and(|f| f.take_ready());
        self.switched.swap(false, Ordering::Relaxed) | fallback
    }

    fn available(&self) -> bool {
        self.primary.available() || self.fallback.get().is_some_and(|f| f.available())
    }

    fn probe(&self) -> bool {
        *self.probed.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        self.primary.probe() || self.fallback().is_some_and(|f| f.probe())
    }

    fn status(&self) -> Option<String> {
        if self.primary.available() {
            return None;
        }
        self.fallback.get()?.status()
    }

    fn name(&self) -> &'static str {
        match self.fallback.get() {
            Some(fallback) if !self.primary.available() => fallback.name(),
            _ => self.primary.name(),
        }
    }

    fn offline_roots(&self) -> Vec<PathBuf> {
        match self.fallback.get() {
            Some(fallback) if !self.primary.available() => fallback.offline_roots(),
            _ => Vec::new(),
        }
    }

    fn set_index_settings(&self, settings: &IndexSettings) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(settings.clone());
        if let Some(fallback) = self.fallback.get() {
            let pending = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(settings) = pending {
                fallback.set_index_settings(&settings);
            }
        }
    }

    fn index_settings_changed(&self) -> Option<IndexSettings> {
        self.fallback.get()?.index_settings_changed()
    }
}

/// Keel's own index: the main backend (the NTFS index, Spotlight, `locate` or the home
/// walk) and the folder and volume indexes of Settings → Search, searched together.
/// Hits found by both appear once, ranked as one list.
pub struct Merge {
    main: Box<dyn Searcher>,
    extra: Box<dyn Searcher>,
}

impl Merge {
    pub fn new(main: Box<dyn Searcher>, extra: Box<dyn Searcher>) -> Self {
        Self { main, extra }
    }
}

impl Searcher for Merge {
    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        let main = self.main.query(query);
        let extra = if self.extra.available() {
            self.extra.query(query).ok().filter(|h| !h.is_empty())
        } else {
            None
        };
        match (main, extra) {
            (Ok(mut hits), Some(more)) => {
                hits.extend(more);
                Ok(crate::walkindex::merge_hits(query, hits))
            }
            (Ok(hits), None) => Ok(hits),
            // The main index is still building: the folders answer meanwhile.
            (Err(_), Some(more)) => Ok(more),
            (Err(e), None) => Err(e),
        }
    }

    fn available(&self) -> bool {
        self.main.available() || self.extra.available()
    }

    fn probe(&self) -> bool {
        let main = self.main.probe();
        self.extra.probe() || main
    }

    fn status(&self) -> Option<String> {
        let notes: Vec<String> = [self.main.status(), self.extra.status()]
            .into_iter()
            .flatten()
            .collect();
        (!notes.is_empty()).then(|| notes.join("; "))
    }

    fn name(&self) -> &'static str {
        self.main.name()
    }

    fn state(&self) -> SearchState {
        match self.main.state() {
            SearchState::Unavailable if self.extra.state() == SearchState::Ready => {
                SearchState::Ready
            }
            state => state,
        }
    }

    fn take_ready(&self) -> bool {
        let main = self.main.take_ready();
        self.extra.take_ready() || main
    }

    fn offline_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.main.offline_roots();
        roots.extend(self.extra.offline_roots());
        roots
    }

    fn set_index_settings(&self, settings: &IndexSettings) {
        self.main.set_index_settings(settings);
        self.extra.set_index_settings(settings);
    }

    fn index_settings_changed(&self) -> Option<IndexSettings> {
        self.extra
            .index_settings_changed()
            .or_else(|| self.main.index_settings_changed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Answers with one hit named after itself while `up`; like Everything, a failed
    /// query marks it down until a probe finds it up.
    struct Fake {
        name: &'static str,
        up: Arc<AtomicBool>,
        seen_up: AtomicBool,
        probes: Arc<AtomicUsize>,
    }

    impl Fake {
        fn new(name: &'static str, up: &Arc<AtomicBool>, probes: &Arc<AtomicUsize>) -> Self {
            Self {
                name,
                up: up.clone(),
                seen_up: AtomicBool::new(up.load(Ordering::Relaxed)),
                probes: probes.clone(),
            }
        }
    }

    impl Searcher for Fake {
        fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
            let up = self.up.load(Ordering::Relaxed);
            self.seen_up.store(up, Ordering::Relaxed);
            if !up {
                anyhow::bail!("{} is not running", self.name);
            }
            if query.text == "(bad" {
                anyhow::bail!("bad pattern");
            }
            Ok(vec![Hit {
                path: keel_vfs::VPath::local(self.name),
                is_dir: false,
                size: 0,
                modified: None,
                snippet: None,
            }])
        }

        fn available(&self) -> bool {
            self.seen_up.load(Ordering::Relaxed)
        }

        fn probe(&self) -> bool {
            self.probes.fetch_add(1, Ordering::Relaxed);
            let up = self.up.load(Ordering::Relaxed);
            self.seen_up.store(up, Ordering::Relaxed);
            up
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    fn who(searcher: &dyn Searcher, text: &str) -> String {
        match searcher.query(&Query {
            text: text.into(),
            ..Query::default()
        }) {
            Ok(hits) => hits[0].path.display(),
            Err(e) => format!("error: {e}"),
        }
    }

    fn setup(
        reprobe: Duration,
    ) -> (
        Composite,
        Arc<AtomicBool>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let (up, probes, opened) = (
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        let primary = Box::new(Fake::new("everything", &up, &probes));
        let count = opened.clone();
        let open: Open = Box::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
            let up = Arc::new(AtomicBool::new(true));
            Box::new(Fake::new("index", &up, &Arc::default()))
        });
        (
            Composite::new(primary, Some(open), reprobe),
            up,
            probes,
            opened,
        )
    }

    #[test]
    fn falls_back_when_everything_stops_and_returns_when_it_starts() {
        let (search, up, probes, opened) = setup(Duration::ZERO);
        assert_eq!(who(&search, "x"), "everything");
        assert_eq!(search.name(), "everything");
        assert_eq!(
            opened.load(Ordering::Relaxed),
            0,
            "index opened only when needed"
        );
        // A bad query while Everything runs is its error, not a fallback.
        assert_eq!(who(&search, "(bad"), "error: bad pattern");

        up.store(false, Ordering::Relaxed);
        assert_eq!(who(&search, "x"), "index");
        assert_eq!(search.name(), "index");
        assert!(search.available());
        assert_eq!(opened.load(Ordering::Relaxed), 1);

        up.store(true, Ordering::Relaxed);
        assert_eq!(who(&search, "x"), "everything", "re-probed");
        assert_eq!(search.name(), "everything");
        assert_eq!(search.state(), SearchState::Ready);
        assert!(search.take_ready(), "the switches are announced");
        assert!(!search.take_ready());
        assert!(probes.load(Ordering::Relaxed) >= 1);
        assert_eq!(opened.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_stopped_everything_is_probed_at_most_every_interval() {
        let (search, up, probes, _) = setup(REPROBE);
        up.store(false, Ordering::Relaxed);
        assert_eq!(who(&search, "x"), "index");
        up.store(true, Ordering::Relaxed);
        for _ in 0..5 {
            assert_eq!(who(&search, "x"), "index");
        }
        assert_eq!(
            probes.load(Ordering::Relaxed),
            0,
            "cached atomic, no IPC per query"
        );
        // The status-bar click probes at once.
        assert!(search.probe());
        assert_eq!(who(&search, "x"), "everything");
    }

    #[test]
    fn index_opens_at_once_when_everything_is_down_at_start() {
        let up = Arc::new(AtomicBool::new(false));
        let opened = Arc::new(AtomicUsize::new(0));
        let count = opened.clone();
        let primary = Box::new(Fake::new("everything", &up, &Arc::default()));
        let open: Open = Box::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
            Box::new(Fake::new(
                "index",
                &Arc::new(AtomicBool::new(true)),
                &Arc::default(),
            ))
        });
        let search = Composite::new(primary, Some(open), REPROBE);
        assert_eq!(opened.load(Ordering::Relaxed), 1);
        assert_eq!(search.name(), "index");
        // Without an index folder the primary's own error stands.
        let alone = Composite::new(
            Box::new(Fake::new("everything", &up, &Arc::default())),
            None,
            REPROBE,
        );
        assert_eq!(who(&alone, "x"), "error: everything is not running");
    }

    /// Answers with fixed paths (or an error), and records index settings it gets.
    #[derive(Default)]
    struct Fixed {
        paths: Vec<&'static str>,
        fails: bool,
        settings: Arc<Mutex<Vec<IndexSettings>>>,
    }

    impl Searcher for Fixed {
        fn query(&self, _: &Query) -> anyhow::Result<Vec<Hit>> {
            anyhow::ensure!(!self.fails, "indexing");
            Ok(self
                .paths
                .iter()
                .map(|p| Hit {
                    path: keel_vfs::VPath::local(p),
                    is_dir: false,
                    size: 0,
                    modified: None,
                    snippet: None,
                })
                .collect())
        }

        fn available(&self) -> bool {
            !self.fails
        }

        fn status(&self) -> Option<String> {
            self.fails.then(|| "indexing".into())
        }

        fn set_index_settings(&self, settings: &IndexSettings) {
            self.settings.lock().unwrap().push(settings.clone());
        }
    }

    fn hit_names(searcher: &dyn Searcher, text: &str) -> Vec<String> {
        let query = Query {
            text: text.into(),
            ..Query::default()
        };
        (searcher.query(&query).unwrap().iter())
            .map(|h| h.path.name().to_owned())
            .collect()
    }

    #[test]
    fn merge_searches_both_and_ranks_once() {
        let merged = Merge::new(
            Box::new(Fixed {
                paths: vec!["/c/my notes", "/c/notes.txt"],
                ..Fixed::default()
            }),
            Box::new(Fixed {
                paths: vec!["/e/notes", "/c/notes.txt"],
                ..Fixed::default()
            }),
        );
        assert_eq!(
            hit_names(&merged, "notes"),
            ["notes", "notes.txt", "my notes"]
        );
        // The NTFS index still building: the folders answer, and both notes show.
        let building = Merge::new(
            Box::new(Fixed {
                fails: true,
                ..Fixed::default()
            }),
            Box::new(Fixed {
                paths: vec!["/e/notes"],
                ..Fixed::default()
            }),
        );
        assert_eq!(hit_names(&building, "notes"), ["notes"]);
        assert_eq!(building.status().as_deref(), Some("indexing"));
        assert!(building.available());
    }

    #[test]
    fn index_settings_reach_a_fallback_opened_later() {
        let up = Arc::new(AtomicBool::new(true));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (record, opened) = (seen.clone(), Arc::new(AtomicUsize::new(0)));
        let count = opened.clone();
        let open: Open = Box::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
            Box::new(Fixed {
                paths: vec!["index"],
                settings: record.clone(),
                ..Fixed::default()
            })
        });
        let search = Composite::new(
            Box::new(Fake::new("everything", &up, &Arc::default())),
            Some(open),
            Duration::ZERO,
        );
        let settings = IndexSettings {
            auto_remove_offline: true,
            ..IndexSettings::default()
        };
        search.set_index_settings(&settings);
        assert_eq!(opened.load(Ordering::Relaxed), 0, "Everything answers");
        up.store(false, Ordering::Relaxed);
        assert_eq!(who(&search, "x"), "index");
        assert_eq!(*seen.lock().unwrap(), [settings.clone()]);
        search.set_index_settings(&IndexSettings::default());
        assert_eq!(seen.lock().unwrap().len(), 2, "forwarded once open");
    }
}
