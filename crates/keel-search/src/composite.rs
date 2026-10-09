//! Windows search: Everything while it runs, else Keel's own index. Each query
//! checks Everything's cached availability (an atomic) and, while it is down,
//! re-probes it at most every [`REPROBE`], so starting or quitting Everything
//! after Keel takes effect within one query or 30 s.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::{Hit, Query, Searcher};

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
        };
        if !this.primary.available() {
            this.fallback();
        }
        this
    }

    fn fallback(&self) -> Option<&dyn Searcher> {
        let open = self.open.as_ref()?;
        Some(self.fallback.get_or_init(open).as_ref())
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
}

impl Searcher for Composite {
    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        if let Some(primary) = self.live_primary() {
            match primary.query(query) {
                Ok(hits) => return Ok(hits),
                // Still up: a bad query, not a stopped backend.
                Err(e) if primary.available() || self.open.is_none() => return Err(e),
                Err(_) => {}
            }
        }
        match self.fallback() {
            Some(fallback) => fallback.query(query),
            None => self.primary.query(query),
        }
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
}
