//! `run-count:` / `date-run:` through [`WithRuns`] (Keel's run history lives beside the
//! index) and `si:` / `system-index:` answered by the item's source.

use std::collections::HashSet;

use keel_query::{parse, DateProp, ErrorKind, Options, Props, WithRuns};

/// Monday 2026-10-12 12:00 UTC.
const NOW: i64 = 1_791_806_400_000;
const DAY: i64 = 24 * 3600 * 1000;

fn opts() -> Options {
    Options {
        now: NOW,
        utc_offset_secs: 0,
        ..Options::default()
    }
}

struct Item {
    name: &'static str,
    path: &'static str,
    /// The `si:` texts the Windows Search index found this item for.
    found_for: &'static [&'static str],
}

impl Props for Item {
    fn name(&self) -> &str {
        self.name
    }
    fn path(&self) -> &str {
        self.path
    }
    fn is_dir(&self) -> bool {
        false
    }
    fn size(&self) -> Option<u64> {
        Some(10)
    }
    fn allocated(&self) -> Option<u64> {
        None
    }
    fn date(&self, _which: DateProp) -> Option<i64> {
        None
    }
    fn attributes(&self) -> Option<u32> {
        None
    }
    fn system_index(&self, text: &str) -> Option<bool> {
        Some(self.found_for.contains(&text))
    }
}

const REPORT: Item = Item {
    name: "report.pdf",
    path: "C:\\work\\report.pdf",
    found_for: &["budget", "quarterly figures"],
};
const NOTES: Item = Item {
    name: "notes.txt",
    path: "C:\\work\\notes.txt",
    found_for: &["budget"],
};
const TOOL: Item = Item {
    name: "tool.exe",
    path: "C:\\bin\\tool.exe",
    found_for: &[],
};

fn matching(query: &str) -> Vec<&'static str> {
    let q = parse(query, &opts()).unwrap_or_else(|e| panic!("{query}: {e}"));
    [REPORT, NOTES, TOOL]
        .iter()
        .filter(|i| q.matches(*i))
        .map(|i| i.name)
        .collect()
}

#[test]
fn run_count_and_date_run_come_from_the_run_history() {
    let q = parse("run-count:>2 date-run:today", &opts()).unwrap();
    let today = WithRuns {
        item: &REPORT,
        count: Some(3),
        last_run: Some(NOW - 3600 * 1000),
    };
    let yesterday = WithRuns {
        item: &NOTES,
        count: Some(5),
        last_run: Some(NOW - DAY),
    };
    let few = WithRuns {
        item: &TOOL,
        count: Some(1),
        last_run: Some(NOW),
    };
    let never = WithRuns {
        item: &TOOL,
        count: None,
        last_run: None,
    };
    assert!(q.matches(&today));
    assert!(!q.matches(&yesterday));
    assert!(!q.matches(&few));
    assert!(!q.matches(&never));
    // Legacy names and `unknown`.
    assert!(parse("runcount:3", &opts()).unwrap().matches(&today));
    assert!(parse("daterun:yesterday", &opts()).unwrap().matches(&yesterday));
    assert!(parse("run-count:unknown", &opts()).unwrap().matches(&never));
    assert!(!parse("run-count:unknown", &opts()).unwrap().matches(&today));
    // Everything else is the item's.
    assert_eq!(today.name(), "report.pdf");
    assert_eq!(today.extension(), "pdf");
    assert_eq!(today.size(), Some(10));
}

#[test]
fn si_matches_what_the_system_index_found() {
    assert_eq!(matching("si:budget"), ["report.pdf", "notes.txt"]);
    assert_eq!(matching("system-index:\"quarterly figures\""), ["report.pdf"]);
    assert_eq!(matching("si:budget ext:txt"), ["notes.txt"]);
    assert_eq!(matching("!si:budget"), ["tool.exe"]);
    assert_eq!(matching("si:nothing | tool"), ["tool.exe"]);
    assert_eq!(matching("si:nothing;budget"), ["report.pdf", "notes.txt"]);
    // An item whose source never asked the index: no match.
    let q = parse("si:budget", &opts()).unwrap();
    let plain = WithRuns {
        item: &Plain,
        count: None,
        last_run: None,
    };
    assert!(!q.matches(&plain));
}

#[test]
fn the_query_lists_its_system_index_texts() {
    let q = parse(
        "si:budget <ext:pdf | system-index:\"quarterly figures\"> !si:budget si:a;b",
        &opts(),
    )
    .unwrap();
    assert_eq!(
        q.system_index_texts(),
        ["budget", "quarterly figures", "a", "b"]
    );
    assert!(parse("report", &opts()).unwrap().system_index_texts().is_empty());
}

#[test]
fn si_needs_words() {
    for bad in ["si:", "si:>3"] {
        let e = parse(bad, &opts()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Value, "{bad}: {e}");
    }
    let names: HashSet<&str> = keel_query::functions::UNSUPPORTED
        .iter()
        .flat_map(|(n, _)| n.iter().copied())
        .collect();
    assert!(!names.contains("si") && !names.contains("system-index"));
}

struct Plain;

impl Props for Plain {
    fn name(&self) -> &str {
        "plain.txt"
    }
    fn path(&self) -> &str {
        "C:\\plain.txt"
    }
    fn is_dir(&self) -> bool {
        false
    }
    fn size(&self) -> Option<u64> {
        None
    }
    fn allocated(&self) -> Option<u64> {
        None
    }
    fn date(&self, _which: DateProp) -> Option<i64> {
        None
    }
    fn attributes(&self) -> Option<u32> {
        None
    }
}
