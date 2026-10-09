//! Everything IPC search and folder-path fuzzy matching for Keel.

mod everything;
mod fuzzy;

pub use everything::{EverythingSearcher, Unavailable};
pub use fuzzy::Fuzzy;

use std::time::SystemTime;

/// Parameters accepted by a search backend.
#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    pub folders_only: bool,
    pub max: u32,
    pub regex: bool,
    pub match_case: bool,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            text: String::new(),
            folders_only: false,
            max: 500,
            regex: false,
            match_case: false,
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

/// A pluggable file search backend.
pub trait Searcher: Send + Sync {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>>;
    fn available(&self) -> bool;
}

/// Builds the display-path list consumed by the folder jump popup.
pub fn folder_index(searcher: &dyn Searcher) -> anyhow::Result<Vec<String>> {
    let hits = searcher.query(&Query {
        text: "folder:".to_owned(),
        max: 200_000,
        ..Query::default()
    })?;

    Ok(hits.into_iter().map(|hit| hit.path.display()).collect())
}

#[cfg(test)]
mod tests {
    use super::{folder_index, Fuzzy, Query, Searcher, Unavailable};

    #[test]
    fn query_defaults_to_five_hundred_results() {
        assert_eq!(Query::default().max, 500);
    }

    #[test]
    fn unavailable_searcher_fails_softly() {
        let searcher = Unavailable;

        assert!(!searcher.available());
        assert_eq!(
            searcher.query(&Query::default()).unwrap_err().to_string(),
            "Everything is not running"
        );
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
    fn folder_index_uses_the_documented_everything_query() {
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
        assert_eq!(query.text, "folder:");
        assert_eq!(query.max, 200_000);
    }
}
