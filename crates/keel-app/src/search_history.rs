//! Search history (Task 76): the searches run in this profile, for the suggestions
//! dropdown under the search field. Kept in `<config>/profiles/<name>/search_history.json`
//! (never sent anywhere); **Do not save search history** records nothing and empties it,
//! **Clear history** empties it. Each entry keeps how often and when it was last used, so
//! the dropdown can list the most recent or the most used first.

use crate::settings::write_atomic;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Most entries Settings accepts for `max`, and kept from a file.
pub const MAX_LIMIT: usize = 10_000;
/// Longest search kept (characters); longer ones are not recorded.
pub const MAX_TEXT: usize = 1024;
/// A history file bigger than this is ignored (and replaced on the next save).
const MAX_FILE: u64 = 8 * 1024 * 1024;
/// Rows in the suggestions dropdown.
pub const SUGGESTIONS: usize = 8;
/// Writes wait this long after the last change.
const SAVE_EVERY: Duration = Duration::from_secs(1);

/// One search the user ran.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub text: String,
    /// Times it was run.
    pub count: u32,
    /// Last run (unix seconds).
    pub last: u64,
}

/// Suggestion order.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    #[default]
    MostRecent,
    MostUsed,
}

/// `[search_history]` in config.toml.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct HistorySettings {
    /// Off: **Do not save search history** (nothing is recorded; what was kept is cleared).
    pub save: bool,
    /// Most entries kept; the least recently used go first.
    pub max: usize,
    pub order: Order,
    /// Always show suggestions while typing (else only on Alt+Down).
    pub suggestions: bool,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            save: true,
            max: 200,
            order: Order::MostRecent,
            suggestions: true,
        }
    }
}

/// The entries, in no particular order.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    pub entries: Vec<Entry>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl History {
    /// Notes that `text` was searched at `now`; false when nothing changed (saving is off,
    /// the text is empty or too long).
    pub fn record(&mut self, text: &str, now: u64, s: &HistorySettings) -> bool {
        let text = text.trim();
        if !s.save || text.is_empty() || text.chars().count() > MAX_TEXT {
            return false;
        }
        match self.entries.iter_mut().find(|e| e.text == text) {
            Some(e) => {
                e.count = e.count.saturating_add(1);
                e.last = e.last.max(now);
            }
            None => self.entries.push(Entry {
                text: text.to_owned(),
                count: 1,
                last: now,
            }),
        }
        self.trim(s.max);
        true
    }

    /// Keeps the `max` most recently used entries.
    pub fn trim(&mut self, max: usize) {
        let max = max.min(MAX_LIMIT);
        if self.entries.len() > max {
            self.sort(Order::MostRecent);
            self.entries.truncate(max);
        }
    }

    fn sort(&mut self, order: Order) {
        match order {
            Order::MostRecent => self
                .entries
                .sort_by(|a, b| (b.last, b.count).cmp(&(a.last, a.count))),
            Order::MostUsed => self
                .entries
                .sort_by(|a, b| (b.count, b.last).cmp(&(a.count, a.last))),
        }
    }

    /// Every entry in `order` (ties: the other key, then the text).
    pub fn ordered(&self, order: Order) -> Vec<&Entry> {
        let mut out: Vec<&Entry> = self.entries.iter().collect();
        out.sort_by(|a, b| {
            let key = |e: &Entry| match order {
                Order::MostRecent => (e.last, u64::from(e.count)),
                Order::MostUsed => (u64::from(e.count), e.last),
            };
            key(b).cmp(&key(a)).then_with(|| a.text.cmp(&b.text))
        });
        out
    }

    /// The history at `path`: empty when there is none or it cannot be read.
    pub fn load(path: &Path) -> History {
        let text = match std::fs::metadata(path) {
            Ok(m) if m.len() <= MAX_FILE => std::fs::read(path).unwrap_or_default(),
            _ => return History::default(),
        };
        match serde_json::from_slice::<History>(&text) {
            Ok(mut h) => {
                h.entries
                    .retain(|e| !e.text.trim().is_empty() && e.text.chars().count() <= MAX_TEXT);
                h.trim(MAX_LIMIT);
                h
            }
            Err(e) => {
                tracing::warn!("{}: {e}", path.display());
                History::default()
            }
        }
    }
}

/// The profile's history file.
pub fn path(profile: &str) -> PathBuf {
    crate::profiles::config_path(profile).with_file_name("search_history.json")
}

/// The history of the current profile and its writes. Only the running app (`disk`) reads
/// and writes the file; tests keep it in memory.
#[derive(Default)]
pub struct HistoryStore {
    pub history: History,
    /// The profile `history` belongs to (its file).
    profile: String,
    disk: bool,
    dirty: bool,
    changed_at: Option<Instant>,
}

impl HistoryStore {
    /// Reads `profile`'s history (from now on written back there).
    pub fn open(&mut self, profile: &str) {
        self.flush();
        self.disk = true;
        self.profile = profile.to_owned();
        self.history = History::load(&path(profile));
    }

    /// Follows a profile switch.
    pub fn follow(&mut self, profile: &str) {
        if self.disk && self.profile != profile {
            self.open(profile);
        }
    }

    pub fn record(&mut self, text: &str, s: &HistorySettings) {
        if self.history.record(text, now(), s) {
            self.changed();
        }
    }

    /// Empties the history (and its file).
    pub fn clear(&mut self) {
        self.history = History::default();
        self.changed();
        self.flush();
    }

    fn changed(&mut self) {
        self.dirty = true;
        self.changed_at = Some(Instant::now());
    }

    /// Every frame: applies the settings (saving off empties it; a smaller `max` trims it)
    /// and writes a change `SAVE_EVERY` after it, on a worker.
    pub fn tick(&mut self, s: &HistorySettings) {
        if !s.save && !self.history.entries.is_empty() {
            self.clear();
        }
        if self.history.entries.len() > s.max.min(MAX_LIMIT) {
            self.history.trim(s.max);
            self.changed();
        }
        if self.dirty && self.changed_at.is_some_and(|t| t.elapsed() >= SAVE_EVERY) {
            self.write(false);
        }
    }

    /// Writes a pending change now (exit, profile switch, Clear history).
    pub fn flush(&mut self) {
        if self.dirty {
            self.write(true);
        }
    }

    fn write(&mut self, wait: bool) {
        self.dirty = false;
        if !self.disk {
            return;
        }
        let file = path(&self.profile);
        let bytes = match self.history.entries.is_empty() {
            true => None,
            false => serde_json::to_vec_pretty(&self.history).ok(),
        };
        let job = move || {
            let result = match bytes {
                Some(bytes) => write_atomic(&file, &bytes),
                // Empty: no file at all.
                None => match std::fs::remove_file(&file) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                    _ => Ok(()),
                },
            };
            if let Err(e) = result {
                tracing::error!("save search history: {e:#}");
            }
        };
        if wait {
            job();
        } else {
            crate::worker::spawn("keel-history", job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(max: usize) -> HistorySettings {
        HistorySettings {
            max,
            ..HistorySettings::default()
        }
    }

    #[test]
    fn suggestions_by_most_recent_and_most_used() {
        let mut h = History::default();
        let s = on(100);
        // "invoice" three times long ago, "report" once lately, "notes" in between.
        for t in [10, 11, 12] {
            assert!(h.record("invoice ext:pdf", t, &s));
        }
        h.record("notes", 20, &s);
        h.record("  report  ", 30, &s);
        let texts = |v: Vec<&Entry>| v.into_iter().map(|e| e.text.clone()).collect::<Vec<_>>();
        let suggest = |h: &History, typed: &str, order: Order, n: usize| {
            crate::bookmarks::suggestions(&texts(h.ordered(order)), typed, n)
        };
        assert_eq!(
            texts(h.ordered(Order::MostRecent)),
            ["report", "notes", "invoice ext:pdf"]
        );
        assert_eq!(
            texts(h.ordered(Order::MostUsed)),
            ["invoice ext:pdf", "report", "notes"]
        );
        assert_eq!(h.entries[0].count, 3);
        // Matching any case; the text as typed is not suggested back.
        assert_eq!(
            suggest(&h, "O", Order::MostRecent, 8),
            ["report", "notes", "invoice ext:pdf"]
        );
        assert_eq!(suggest(&h, "INV", Order::MostUsed, 8), ["invoice ext:pdf"]);
        assert_eq!(
            suggest(&h, "notes", Order::MostUsed, 8),
            Vec::<String>::new()
        );
        assert_eq!(suggest(&h, "", Order::MostUsed, 2).len(), 2);
        // Running an old search again makes it the most recent.
        h.record("notes", 40, &s);
        assert_eq!(h.ordered(Order::MostRecent)[0].text, "notes");
    }

    #[test]
    fn max_entries_drop_the_least_recent() {
        let mut h = History::default();
        for (i, t) in ["a", "b", "c", "d"].into_iter().enumerate() {
            h.record(t, i as u64, &on(3));
        }
        let mut kept: Vec<_> = h.entries.iter().map(|e| e.text.as_str()).collect();
        kept.sort();
        assert_eq!(kept, ["b", "c", "d"]);
        // Empty and over-long searches are never kept.
        assert!(!h.record("   ", 9, &on(3)));
        assert!(!h.record(&"x".repeat(MAX_TEXT + 1), 9, &on(3)));
    }

    /// Review focus 2: with **Do not save search history** nothing is recorded, what was
    /// kept is cleared, and the file goes away; Clear history empties it too.
    #[test]
    fn do_not_save_records_nothing_and_clears_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = crate::settings::TEST_ENV.lock();
        let _env = crate::settings::EnvRestore::save(&["KEEL_CONFIG_DIR"]);
        std::env::set_var("KEEL_CONFIG_DIR", dir.path());
        let mut store = HistoryStore::default();
        store.open("default");
        let mut s = HistorySettings::default();
        store.record("budget 2026", &s);
        store.flush();
        let file = path("default");
        assert!(file.is_file());
        let mut again = HistoryStore::default();
        again.open("default");
        assert_eq!(again.history.entries[0].text, "budget 2026");

        s.save = false;
        store.record("secret plans", &s);
        assert_eq!(store.history.entries.len(), 1, "nothing new is recorded");
        store.tick(&s);
        assert!(
            store.history.entries.is_empty(),
            "turning saving off clears"
        );
        assert!(!file.exists(), "and removes the file");
        store.record("secret plans", &s);
        store.tick(&s);
        store.flush();
        assert!(!file.exists());

        s.save = true;
        store.record("holiday", &s);
        store.flush();
        assert!(file.is_file());
        store.clear();
        assert!(!file.exists(), "Clear history removes the file");
        assert!(store.history.entries.is_empty());
    }

    #[test]
    fn a_damaged_or_huge_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("h.json");
        std::fs::write(&file, b"{not json").unwrap();
        assert_eq!(History::load(&file), History::default());
        std::fs::write(
            &file,
            br#"{"entries":[{"text":"ok","count":2,"last":5},{"text":" ","count":1,"last":1}]}"#,
        )
        .unwrap();
        assert_eq!(History::load(&file).entries.len(), 1);
    }
}
