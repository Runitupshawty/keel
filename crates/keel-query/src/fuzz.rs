//! Fuzz entry point (docs/fuzzing.md): `query_parse` parses any text with several option
//! sets and evaluates what compiles against a few items, alone and as a tree with the set
//! functions applied. It must never panic, and a typed query is bounded (group and macro
//! depth, regex size, tree scans, set memory).

use std::collections::BTreeMap;

use crate::{
    normalize, parse, ChildCounts, DateOrder, DateProp, NodeId, Options, Precedence, Props, Tree,
};

struct Item {
    name: &'static str,
    path: &'static str,
    dir: bool,
    size: Option<u64>,
    date: Option<i64>,
}

impl Props for Item {
    fn name(&self) -> &str {
        self.name
    }
    fn path(&self) -> &str {
        self.path
    }
    fn is_dir(&self) -> bool {
        self.dir
    }
    fn size(&self) -> Option<u64> {
        self.size
    }
    fn allocated(&self) -> Option<u64> {
        self.size.map(|s| s.div_ceil(4096).saturating_mul(4096))
    }
    fn date(&self, _: DateProp) -> Option<i64> {
        self.date
    }
    fn attributes(&self) -> Option<u32> {
        Some(if self.dir { 0x10 } else { 0x20 })
    }
    fn run_count(&self) -> Option<u64> {
        Some(3)
    }
    fn child_counts(&self) -> Option<ChildCounts> {
        self.dir.then_some(ChildCounts {
            files: 1,
            folders: 0,
        })
    }
}

const ITEMS: &[Item] = &[
    Item {
        name: "Résumé (final).PDF",
        path: "C:\\Users\\x\\Docs\\Résumé (final).PDF",
        dir: false,
        size: Some(123_456),
        date: Some(1_700_000_000_000),
    },
    Item {
        name: "Docs",
        path: "C:\\Users\\x\\Docs",
        dir: true,
        size: None,
        date: Some(i64::MIN),
    },
    Item {
        name: "C:",
        path: "C:\\",
        dir: true,
        size: Some(u64::MAX),
        date: Some(i64::MAX),
    },
    Item {
        name: "notes",
        path: "/home/x/notes",
        dir: false,
        size: Some(0),
        date: None,
    },
];

/// Any bytes as a query.
pub fn query_parse(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let mut macros = BTreeMap::new();
    macros.insert("pair".to_string(), "a|b".to_string());
    macros.insert("twice".to_string(), "pair: pair:".to_string());
    let base = Options {
        now: 1_791_806_400_000,
        utc_offset_secs: 3600,
        macros,
        ..Options::default()
    };
    let sets = [
        base.clone(),
        Options {
            precedence: Precedence::AndFirst,
            allow_literal_operators: true,
            replace_forward_slashes: true,
            metric: true,
            match_path: true,
            date_order: DateOrder::Dmy,
            week_starts_sunday: true,
            ..base.clone()
        },
        Options {
            regex: true,
            match_case: true,
            match_whole_words: true,
            ignore_punctuation: true,
            whole_filename_wildcards: false,
            utc_offset_secs: -50_400,
            ..base
        },
    ];
    for opts in &sets {
        if let Ok(q) = parse(&text, opts) {
            for item in ITEMS {
                let _ = q.matches(item);
            }
            let _ = q.select(ITEMS, Some(&Links));
        }
    }
}

/// Parent links over [`ITEMS`]: the file in `Docs`, `Docs` under the drive, a cycle-free
/// forest with a lone Unix item.
struct Links;

const PARENTS: [Option<NodeId>; 4] = [Some(1), Some(2), None, None];

impl Tree for Links {
    fn node_of(&self, item: &dyn Props) -> Option<NodeId> {
        ITEMS
            .iter()
            .position(|i| i.path == item.path())
            .map(|i| i as NodeId)
    }
    fn parent(&self, node: NodeId) -> Option<NodeId> {
        PARENTS.get(node as usize).copied().flatten()
    }
    fn children(&self, node: NodeId, f: &mut dyn FnMut(NodeId) -> bool) {
        for (i, p) in PARENTS.iter().enumerate() {
            if *p == Some(node) && !f(i as NodeId) {
                return;
            }
        }
    }
    fn with_props(&self, node: NodeId, f: &mut dyn FnMut(&dyn Props)) {
        if let Some(item) = ITEMS.get(node as usize) {
            f(item);
        }
    }
    fn find(&self, path: &str) -> Option<NodeId> {
        ITEMS
            .iter()
            .position(|i| normalize(i.path).eq_ignore_ascii_case(path))
            .map(|i| i as NodeId)
    }
}

#[cfg(test)]
mod tests {
    //! The in-tree run (docs/fuzzing.md): the corpus, then seeded mutations of it, each case
    //! under a time limit. A small copy of `keel_vfs::fuzz_harness`'s loop, so this crate
    //! builds without keel-vfs.
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    fn corpus() -> Vec<Vec<u8>> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/query_parse");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        files.iter().map(|p| std::fs::read(p).unwrap()).collect()
    }

    /// SplitMix64, seeded, the same on every platform.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const SYNTAX: &[&str] = &[
        "<",
        ">",
        "|",
        "!",
        "\"",
        ";",
        "..",
        "-",
        ":",
        "*",
        "?",
        "\\",
        "/",
        " ",
        "$",
        "#",
        "^",
        "<=",
        ">=",
        "!=",
        "case:",
        "regex:",
        "ww:",
        "path:",
        "size:",
        "dm:",
        "ext:",
        "name*:",
        "unknown",
        "today",
        "2024-02-29T13:05",
        "*-*-*T9",
        "1e999",
        "99999999999999999999",
        "0x",
        "attrib:",
        "dmdow:",
        "$name:",
        "$1:",
        "pair:",
        "child:",
        "descendant:",
        "sibling:",
        "exists:..\\",
        "parentsize:",
        "dupe:size;name",
        "dupe-min:",
        "distinct-sort:",
        "unique:",
        "count:",
        "AND",
        "OR",
        "NOT",
        "\0",
        "é",
    ];

    fn mutate(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
        let mut data = seeds[rng.below(seeds.len())].clone();
        for _ in 0..1 + rng.below(8) {
            let len = data.len();
            match rng.below(5) {
                0 if len > 0 => data[rng.below(len)] = rng.next() as u8,
                1 if len > 0 => {
                    let at = rng.below(len);
                    let n = 1 + rng.below((len - at).min(32));
                    data.drain(at..at + n);
                }
                2 if len > 0 => {
                    let at = rng.below(len);
                    let n = 1 + rng.below((len - at).min(32));
                    let chunk = data[at..at + n].to_vec();
                    let to = rng.below(data.len() + 1);
                    data.splice(to..to, chunk);
                }
                _ => {
                    let at = rng.below(len + 1);
                    let s = SYNTAX[rng.below(SYNTAX.len())];
                    data.splice(at..at, s.bytes());
                }
            }
        }
        data.truncate(64 << 10);
        data
    }

    #[test]
    fn fuzz_query_parse() {
        let seeds = corpus();
        assert!(!seeds.is_empty());
        let ci = std::env::var_os("CI").is_some();
        let cases = std::env::var("KEEL_FUZZ_CASES")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(if ci { 500 } else { 2000 });
        let limit = Duration::from_secs(if ci { 30 } else { 10 });
        let mut rng = Rng(0x6b65_656c_7175_6572);
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let (done_tx, done_rx) = mpsc::channel::<bool>();
        std::thread::spawn(move || {
            for input in rx {
                let ok = std::panic::catch_unwind(|| super::query_parse(&input)).is_ok();
                if done_tx.send(ok).is_err() {
                    break;
                }
            }
        });
        for case in 0..seeds.len() + cases {
            let input = match seeds.get(case) {
                Some(s) => s.clone(),
                None => mutate(&mut rng, &seeds),
            };
            tx.send(input.clone()).unwrap();
            let lossy = String::from_utf8_lossy(&input).into_owned();
            match done_rx.recv_timeout(limit) {
                Ok(true) => {}
                Ok(false) => panic!("case {case} panicked: {lossy:?}"),
                Err(_) => panic!("case {case} ran past {limit:?}: {lossy:?}"),
            }
        }
    }
}
