//! Conformance: one case per operator, modifier, constant and implemented function of
//! Everything's search language (docs/search.md), over an in-memory fixture tree (with its
//! parent links, for the tree functions, and the set functions applied). "Now" is Monday
//! 2026-10-12 12:00 UTC and local time is UTC.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};

use keel_query::{
    attr, functions, modifiers, normalize, parent_of, parse, ChildCounts, DateOrder, DateProp,
    ErrorKind, NodeId, Options, Precedence, Props, Tree,
};

const NOW: i64 = 1_791_806_400_000;

/// UTC milliseconds of a civil date and time.
fn ts(y: i64, m: i64, d: i64, h: i64, mi: i64) -> i64 {
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    ((days * 24 + h) * 60 + mi) * 60_000
}

#[derive(Default)]
struct E {
    name: &'static str,
    path: &'static str,
    dir: bool,
    size: Option<u64>,
    alloc: Option<u64>,
    dm: Option<i64>,
    dc: Option<i64>,
    da: Option<i64>,
    rc: Option<i64>,
    run: Option<i64>,
    taken: Option<i64>,
    attrs: Option<u32>,
    runs: Option<u64>,
    children: Option<(u64, u64)>,
    id: Option<(u64, u64)>,
    type_name: Option<&'static str>,
    link_count: Option<u64>,
    links: Option<&'static [&'static str]>,
    streams: Option<&'static [&'static str]>,
    contents: Option<&'static [&'static str]>,
    list: Option<&'static str>,
}

thread_local! {
    static TYPE_CALLS: Cell<u32> = const { Cell::new(0) };
}

impl Props for E {
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
        self.alloc
    }
    fn date(&self, which: DateProp) -> Option<i64> {
        match which {
            DateProp::Modified => self.dm,
            DateProp::Created => self.dc,
            DateProp::Accessed => self.da,
            DateProp::RecentlyChanged => self.rc,
            DateProp::Run => self.run,
            DateProp::Taken => self.taken,
            _ => None,
        }
    }
    fn attributes(&self) -> Option<u32> {
        self.attrs
    }
    fn run_count(&self) -> Option<u64> {
        self.runs
    }
    fn child_counts(&self) -> Option<ChildCounts> {
        self.children
            .map(|(files, folders)| ChildCounts { files, folders })
    }
    fn file_id(&self) -> Option<u64> {
        self.id.map(|i| i.0)
    }
    fn parent_file_id(&self) -> Option<u64> {
        self.id.map(|i| i.1)
    }
    fn type_name(&self) -> Option<Cow<'_, str>> {
        TYPE_CALLS.with(|c| c.set(c.get() + 1));
        self.type_name.map(Cow::Borrowed)
    }
    fn hard_link_count(&self) -> Option<u64> {
        self.link_count
    }
    fn hard_link_names(&self) -> Option<Vec<String>> {
        self.links
            .map(|v| v.iter().map(|s| s.to_string()).collect())
    }
    fn stream_names(&self) -> Option<Vec<String>> {
        self.streams
            .map(|v| v.iter().map(|s| s.to_string()).collect())
    }
    fn container_file_names(&self) -> Option<Vec<String>> {
        self.contents
            .map(|v| v.iter().map(|s| s.to_string()).collect())
    }
    fn file_list(&self) -> Option<Cow<'_, str>> {
        self.list.map(Cow::Borrowed)
    }
}

/// The fixture's parent links: an arena over the items, found by path.
struct Fixture {
    items: Vec<E>,
    parent: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
    by_path: HashMap<String, usize>,
}

fn path_key(path: &str) -> String {
    normalize(path).trim_end_matches('\\').to_lowercase()
}

impl Fixture {
    fn new(items: Vec<E>) -> Self {
        let by_path: HashMap<String, usize> = items
            .iter()
            .enumerate()
            .map(|(i, e)| (path_key(e.path), i))
            .collect();
        let parent: Vec<Option<usize>> = items
            .iter()
            .map(|e| {
                parent_of(&normalize(e.path)).and_then(|p| by_path.get(&path_key(&p)).copied())
            })
            .collect();
        let mut children = vec![Vec::new(); items.len()];
        for (i, p) in parent.iter().enumerate() {
            if let Some(p) = p {
                children[*p].push(i);
            }
        }
        Self {
            items,
            parent,
            children,
            by_path,
        }
    }

    fn select(&self, o: &Options, query: &str) -> Vec<&'static str> {
        let q = parse(query, o).unwrap_or_else(|e| panic!("{query}: {e}"));
        q.select(&self.items, Some(self))
            .unwrap_or_else(|e| panic!("{query}: {e}"))
            .into_iter()
            .map(|i| self.items[i].name)
            .collect()
    }
}

impl Tree for Fixture {
    fn node_of(&self, item: &dyn Props) -> Option<NodeId> {
        self.find(item.path())
    }
    fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.parent[node as usize].map(|p| p as NodeId)
    }
    fn children(&self, node: NodeId, f: &mut dyn FnMut(NodeId) -> bool) {
        for &c in &self.children[node as usize] {
            if !f(c as NodeId) {
                break;
            }
        }
    }
    fn with_props(&self, node: NodeId, f: &mut dyn FnMut(&dyn Props)) {
        f(&self.items[node as usize]);
    }
    fn find(&self, path: &str) -> Option<NodeId> {
        self.by_path.get(&path_key(path)).map(|&i| i as NodeId)
    }
}

const A: u32 = attr::ARCHIVE;
const D: u32 = attr::DIRECTORY;

fn tree() -> Vec<E> {
    let dir = |name, path, children| E {
        name,
        path,
        dir: true,
        attrs: Some(D),
        children: Some(children),
        ..E::default()
    };
    let file = |name, path, size: u64, dm| E {
        name,
        path,
        size: Some(size),
        alloc: Some(size.div_ceil(4096) * 4096),
        dm,
        attrs: Some(A),
        ..E::default()
    };
    vec![
        E {
            attrs: Some(D | attr::HIDDEN | attr::SYSTEM),
            ..dir("C:", "C:\\", (0, 1))
        },
        dir("fixture", "C:\\fixture", (4, 5)),
        E {
            dm: Some(ts(2026, 10, 1, 10, 0)),
            ..dir("docs", "C:\\fixture\\docs", (3, 1))
        },
        E {
            alloc: Some(151_552),
            dc: Some(ts(2024, 2, 29, 13, 5)),
            da: Some(ts(2026, 10, 12, 11, 0)),
            rc: Some(ts(2026, 10, 12, 9, 30)),
            run: Some(ts(2026, 10, 11, 20, 0)),
            runs: Some(5),
            id: Some((1001, 1000)),
            links: Some(&[
                "C:\\fixture\\docs\\report.pdf",
                "C:\\fixture\\bin\\report-link.pdf",
            ]),
            ..file(
                "report.pdf",
                "C:\\fixture\\docs\\report.pdf",
                150_000,
                Some(ts(2026, 10, 12, 9, 30)),
            )
        },
        E {
            dc: Some(ts(2025, 12, 31, 23, 59)),
            rc: Some(ts(2026, 10, 11, 18, 0)),
            attrs: Some(A | attr::READONLY),
            runs: Some(0),
            streams: Some(&[]),
            ..file(
                "Notes.TXT",
                "C:\\fixture\\docs\\Notes.TXT",
                5_000,
                Some(ts(2026, 10, 11, 18, 0)),
            )
        },
        E {
            dc: Some(ts(2026, 9, 15, 10, 0)),
            ..file(
                "Résumé.docx",
                "C:\\fixture\\docs\\Résumé.docx",
                20_000,
                Some(ts(2026, 10, 5, 8, 0)),
            )
        },
        E {
            size: Some(0),
            dm: Some(ts(2026, 1, 1, 0, 0)),
            ..dir("empty", "C:\\fixture\\docs\\empty", (0, 0))
        },
        dir("music", "C:\\fixture\\music", (2, 0)),
        E {
            list: Some("D:\\lists\\Media.efu"),
            ..file(
                "song one.mp3",
                "C:\\fixture\\music\\song one.mp3",
                4_000_000,
                Some(ts(2025, 6, 15, 14, 30)),
            )
        },
        E {
            attrs: Some(A | attr::HIDDEN),
            ..file(
                "Song-Two.flac",
                "C:\\fixture\\music\\Song-Two.flac",
                30_000_000,
                Some(ts(2023, 3, 15, 9, 0)),
            )
        },
        dir("pics", "C:\\fixture\\pics", (2, 0)),
        E {
            da: Some(ts(2026, 10, 10, 9, 15)),
            taken: Some(ts(2026, 7, 4, 16, 20)),
            streams: Some(&["Zone.Identifier"]),
            list: Some("D:/lists/Photos 2026.txt"),
            ..file(
                "photo.jpg",
                "C:\\fixture\\pics\\photo.jpg",
                2_000_000,
                Some(ts(2026, 10, 9, 17, 45)),
            )
        },
        E {
            attrs: Some(A | attr::COMPRESSED),
            alloc: Some(249_856),
            ..file(
                "IMG_0001.PNG",
                "C:\\fixture\\pics\\IMG_0001.PNG",
                500_000,
                Some(ts(2026, 8, 20, 12, 0)),
            )
        },
        dir("bin", "C:\\fixture\\bin", (2, 0)),
        E {
            run: Some(ts(2026, 10, 1, 8, 0)),
            runs: Some(12),
            link_count: Some(1),
            ..file(
                "tool.exe",
                "C:\\fixture\\bin\\tool.exe",
                200_000_000,
                Some(ts(2024, 2, 29, 13, 5)),
            )
        },
        E {
            attrs: Some(A | attr::SYSTEM),
            ..file(
                "setup.msi",
                "C:\\fixture\\bin\\setup.msi",
                1_048_576,
                Some(ts(2026, 10, 13, 9, 0)),
            )
        },
        E {
            contents: Some(&["docs\\a.txt", "b.md"]),
            ..file(
                "archive.zip",
                "C:\\fixture\\archive.zip",
                0,
                Some(ts(2026, 9, 30, 23, 59)),
            )
        },
        file(
            "README",
            "C:\\fixture\\README",
            1024,
            Some(ts(2020, 1, 1, 0, 0)),
        ),
        dir("2024", "C:\\fixture\\2024", (1, 0)),
        file(
            "123.txt",
            "C:\\fixture\\2024\\123.txt",
            2047,
            Some(ts(2024, 7, 4, 12, 0)),
        ),
        E {
            attrs: Some(A | attr::HIDDEN),
            ..file(
                ".hidden",
                "C:\\fixture\\.hidden",
                10,
                Some(ts(2026, 10, 12, 11, 59)),
            )
        },
        E {
            name: "offline.dat",
            path: "C:\\fixture\\offline.dat",
            type_name: Some("Offline Data"),
            ..E::default()
        },
        E {
            alloc: None,
            ..file("red;blue.txt", "C:\\fixture\\red;blue.txt", 3, None)
        },
    ]
}

const DIRS: [&str; 8] = [
    "2024", "C:", "bin", "docs", "empty", "fixture", "music", "pics",
];

fn opts() -> Options {
    Options {
        now: NOW,
        utc_offset_secs: 0,
        ..Options::default()
    }
}

/// The result names in result order (set functions order them).
fn ordered_o(o: &Options, query: &str) -> Vec<&'static str> {
    Fixture::new(tree()).select(o, query)
}

fn ordered(query: &str) -> Vec<&'static str> {
    ordered_o(&opts(), query)
}

fn qo(o: &Options, query: &str) -> Vec<&'static str> {
    let mut names = ordered_o(o, query);
    names.sort_unstable();
    names
}

fn q(query: &str) -> Vec<&'static str> {
    qo(&opts(), query)
}

macro_rules! check {
    ($query:expr, [$($name:expr),* $(,)?]) => {{
        let mut want: Vec<&str> = vec![$($name),*];
        want.sort_unstable();
        assert_eq!(q($query), want, "query {:?}", $query);
    }};
}

fn count(query: &str) -> usize {
    q(query).len()
}

fn err(query: &str) -> keel_query::Error {
    parse(query, &opts()).expect_err(query)
}

// ---- operators -------------------------------------------------------------------------

#[test]
fn op_empty_query_matches_all() {
    assert_eq!(count(""), tree().len());
}

#[test]
fn op_space_is_and() {
    check!("song one", ["song one.mp3"]);
}

#[test]
fn op_bar_is_or() {
    check!("photo|readme", ["README", "photo.jpg"]);
    check!("photo | readme", ["README", "photo.jpg"]);
}

#[test]
fn op_bang_is_not() {
    check!("song !two", ["song one.mp3"]);
}

#[test]
fn op_angle_brackets_group() {
    check!("<song|photo> !mp3", ["Song-Two.flac", "photo.jpg"]);
    check!("!<a|e|i|o|u|2>", ["C:"]);
}

#[test]
fn op_or_binds_before_and_by_default() {
    check!("txt notes|readme", ["Notes.TXT"]);
}

#[test]
fn op_and_precedence_option_and_modifiers() {
    let o = Options {
        precedence: Precedence::AndFirst,
        ..opts()
    };
    assert_eq!(qo(&o, "txt notes|readme"), ["Notes.TXT", "README"]);
    check!("and-precedence: txt notes|readme", ["Notes.TXT", "README"]);
    assert_eq!(qo(&o, "or-precedence: txt notes|readme"), ["Notes.TXT"]);
}

#[test]
fn op_quotes() {
    check!("\"g o\"", ["song one.mp3"]);
    check!("g o", ["Song-Two.flac", "photo.jpg", "song one.mp3"]);
    check!("\"o|readme\"", []);
}

#[test]
fn op_wildcard_star_matches_whole_name() {
    check!("*.txt", ["123.txt", "Notes.TXT", "red;blue.txt"]);
    check!("s*.m?3", ["song one.mp3"]);
    check!("o*e", []);
}

#[test]
fn op_wildcard_question_mark() {
    check!("?hidden", [".hidden"]);
}

#[test]
fn op_whole_filename_wildcards_option_off() {
    let o = Options {
        whole_filename_wildcards: false,
        ..opts()
    };
    assert_eq!(
        qo(&o, "o*e"),
        ["Notes.TXT", "offline.dat", "song one.mp3", "tool.exe"]
    );
}

#[test]
fn op_double_star_crosses_folders() {
    check!(
        "C:\\fixture\\**.txt",
        ["123.txt", "Notes.TXT", "red;blue.txt"]
    );
    check!("C:\\fixture\\*.txt", ["red;blue.txt"]);
}

#[test]
fn op_backslash_searches_paths() {
    check!("\\music", ["Song-Two.flac", "music", "song one.mp3"]);
    check!("docs\\e", ["empty"]);
}

#[test]
fn op_trailing_backslash_is_a_folder() {
    check!("music\\", ["Song-Two.flac", "song one.mp3"]);
    check!("music", ["music"]);
}

#[test]
fn op_regex() {
    check!(
        "regex:\"^s.*\\.(mp3|flac)$\"",
        ["Song-Two.flac", "song one.mp3"]
    );
}

#[test]
fn op_range_dots() {
    check!("size:1kb..2kb", ["123.txt", "README"]);
    check!(
        "size:..10",
        [".hidden", "archive.zip", "empty", "red;blue.txt"]
    );
}

#[test]
fn op_range_dash() {
    check!("size:1kb-2kb", ["123.txt", "README"]);
}

#[test]
fn op_semicolon_list() {
    check!("ext:mp3;flac", ["Song-Two.flac", "song one.mp3"]);
}

#[test]
fn op_less_than() {
    check!(
        "size:<1kb",
        [".hidden", "archive.zip", "empty", "red;blue.txt"]
    );
}

#[test]
fn op_less_or_equal() {
    check!(
        "size:<=1kb",
        [".hidden", "README", "archive.zip", "empty", "red;blue.txt"]
    );
}

#[test]
fn op_equal() {
    check!("size:=1kb", ["123.txt", "README"]);
    check!("size:==1kb", ["123.txt", "README"]);
    check!("size:1kb", ["123.txt", "README"]);
}

#[test]
fn op_greater_than() {
    check!(
        "size:>1mb",
        ["Song-Two.flac", "photo.jpg", "song one.mp3", "tool.exe"]
    );
}

#[test]
fn op_greater_or_equal() {
    check!(
        "size:>=1mb",
        [
            "Song-Two.flac",
            "photo.jpg",
            "setup.msi",
            "song one.mp3",
            "tool.exe"
        ]
    );
}

#[test]
fn op_not_equal() {
    assert_eq!(count("size:!=1kb"), 13);
    assert_eq!(q("size:!1kb"), q("size:!=1kb"));
}

#[test]
fn op_unknown() {
    check!(
        "size:unknown",
        [
            "2024",
            "C:",
            "bin",
            "docs",
            "fixture",
            "music",
            "offline.dat",
            "pics"
        ]
    );
    assert_eq!(count("size:!unknown"), 15);
}

#[test]
fn op_empty_value_is_any_nonzero() {
    check!("run-count:", ["report.pdf", "tool.exe"]);
}

#[test]
fn op_function_star_is_literal_text() {
    check!("name*:song one", ["song one.mp3"]);
    check!("name*:a|b", []);
}

#[test]
fn op_literal_operators_option() {
    let o = Options {
        allow_literal_operators: true,
        ..opts()
    };
    assert_eq!(qo(&o, "photo OR readme"), ["README", "photo.jpg"]);
    assert_eq!(qo(&o, "song AND NOT two"), ["song one.mp3"]);
    check!("photo OR readme", []);
}

#[test]
fn op_replace_forward_slashes_option() {
    let o = Options {
        replace_forward_slashes: true,
        ..opts()
    };
    assert_eq!(qo(&o, "music/"), ["Song-Two.flac", "song one.mp3"]);
    check!("music/", []);
}

#[test]
fn op_substitution() {
    check!("$stem:.txt", ["123.txt", "Notes.TXT", "red;blue.txt"]);
    assert_eq!(count("full-path:$name:"), tree().len());
}

#[test]
fn op_regex_captures() {
    check!(
        "ungreedy:regex:^(.+)o $1:ong",
        ["Song-Two.flac", "song one.mp3"]
    );
    check!("regex:^(.+)o $1:ong", []);
}

#[test]
fn op_text_macros() {
    check!("#78:otes", ["Notes.TXT"]);
    check!("#x4E:otes", ["Notes.TXT"]);
    check!("quot:x", []);
    let ast = keel_query::parse_ast("quot:a apos:b amp:c lt:d gt:e", &opts()).unwrap();
    let items: Vec<String> = match ast.expr {
        keel_query::Expr::And(v) => v
            .into_iter()
            .map(|e| match e {
                keel_query::Expr::Term(t) => t.items[0].text.clone(),
                e => panic!("{e:?}"),
            })
            .collect(),
        e => panic!("{e:?}"),
    };
    assert_eq!(items, ["\"a", "'b", "&c", "<d", ">e"]);
}

// ---- modifiers -------------------------------------------------------------------------

#[test]
fn mod_case() {
    check!("case:Song", ["Song-Two.flac"]);
    check!("Song", ["Song-Two.flac", "song one.mp3"]);
}

#[test]
fn mod_no_case() {
    let o = Options {
        match_case: true,
        ..opts()
    };
    assert_eq!(qo(&o, "Song"), ["Song-Two.flac"]);
    assert_eq!(qo(&o, "no-case:Song"), ["Song-Two.flac", "song one.mp3"]);
    assert_eq!(qo(&o, "nocase:Song"), ["Song-Two.flac", "song one.mp3"]);
}

#[test]
fn mod_diacritics() {
    check!("resume", ["Résumé.docx"]);
    check!("diacritics:resume", []);
    check!("diac:résumé", ["Résumé.docx"]);
}

#[test]
fn mod_no_diacritics() {
    let o = Options {
        match_diacritics: true,
        ..opts()
    };
    assert!(qo(&o, "resume").is_empty());
    assert_eq!(qo(&o, "no-diacritics:resume"), ["Résumé.docx"]);
}

#[test]
fn mod_whole_words() {
    check!("ww:one", ["song one.mp3"]);
    check!("ww:on", []);
    check!("wholeword:one", ["song one.mp3"]);
    check!("whole-words:one", ["song one.mp3"]);
}

#[test]
fn mod_no_whole_words() {
    let o = Options {
        match_whole_words: true,
        ..opts()
    };
    assert!(qo(&o, "on").is_empty());
    assert_eq!(qo(&o, "no-whole-words:on"), qo(&opts(), "on"));
    assert_eq!(qo(&o, "noww:on"), qo(&opts(), "on"));
}

#[test]
fn mod_path() {
    check!("path:music", ["Song-Two.flac", "music", "song one.mp3"]);
    check!("\\:music", ["Song-Two.flac", "music", "song one.mp3"]);
}

#[test]
fn mod_no_path() {
    let o = Options {
        match_path: true,
        ..opts()
    };
    assert_eq!(qo(&o, "music"), ["Song-Two.flac", "music", "song one.mp3"]);
    assert_eq!(qo(&o, "no-path:music"), ["music"]);
    assert_eq!(qo(&o, "nopath:music"), ["music"]);
}

#[test]
fn mod_prefix() {
    check!("prefix:on", ["song one.mp3"]);
    check!("prefix:ong", []);
    let o = Options {
        match_prefix: true,
        ..opts()
    };
    assert_eq!(qo(&o, "no-prefix:ong"), ["Song-Two.flac", "song one.mp3"]);
}

#[test]
fn mod_suffix() {
    check!(
        "suffix:ng",
        ["IMG_0001.PNG", "Song-Two.flac", "song one.mp3"]
    );
    let o = Options {
        match_suffix: true,
        ..opts()
    };
    assert!(qo(&o, "so").is_empty());
    assert_eq!(qo(&o, "no-suffix:so"), ["Song-Two.flac", "song one.mp3"]);
}

#[test]
fn mod_start_with() {
    check!("start-with:so", ["Song-Two.flac", "song one.mp3"]);
    check!("^:so", ["Song-Two.flac", "song one.mp3"]);
}

#[test]
fn mod_end_with() {
    check!("end-with:.txt", ["123.txt", "Notes.TXT", "red;blue.txt"]);
    check!("$:.txt", ["123.txt", "Notes.TXT", "red;blue.txt"]);
}

#[test]
fn mod_whole() {
    check!("read", ["README"]);
    check!("whole:read", []);
    check!("whole:readme", ["README"]);
    check!("wfn:readme", ["README"]);
    check!("entire:readme", ["README"]);
    check!("whole-filename:readme", ["README"]);
}

#[test]
fn mod_regex_and_no_regex() {
    let o = Options {
        regex: true,
        ..opts()
    };
    assert_eq!(qo(&o, "^s"), ["Song-Two.flac", "setup.msi", "song one.mp3"]);
    assert!(qo(&o, "no-regex:^s").is_empty());
    assert!(qo(&o, "noregex:^s").is_empty());
}

#[test]
fn mod_wildcards() {
    check!("no-wildcards:*.txt", []);
    check!("wildcards:*.txt", ["123.txt", "Notes.TXT", "red;blue.txt"]);
}

#[test]
fn mod_file() {
    check!("file:2024", []);
    check!("2024", ["2024"]);
    check!("files:readme", ["README"]);
}

#[test]
fn mod_folder() {
    assert_eq!(q("folder:"), DIRS);
    check!("folder:music", ["music"]);
}

#[test]
fn mod_ignore_punctuation() {
    check!("songtwo", []);
    check!("ignore-punctuation:songtwo", ["Song-Two.flac"]);
    check!("nopunc:songtwo", ["Song-Two.flac"]);
    let o = Options {
        ignore_punctuation: true,
        ..opts()
    };
    assert!(qo(&o, "no-ignore-punctuation:songtwo").is_empty());
}

#[test]
fn mod_ignore_whitespace() {
    check!("songone", []);
    check!("ignore-whitespace:songone", ["song one.mp3"]);
    check!("nows:songone", ["song one.mp3"]);
    let o = Options {
        ignore_whitespace: true,
        ..opts()
    };
    assert!(qo(&o, "no-ignore-whitespace:songone").is_empty());
}

#[test]
fn mod_len() {
    check!("len:<4", ["C:", "bin"]);
    check!("len:stem:4", ["2024", "docs", "pics", "tool.exe"]);
}

#[test]
fn mod_metric() {
    check!(
        "metric:size:>1mb",
        [
            "Song-Two.flac",
            "photo.jpg",
            "setup.msi",
            "song one.mp3",
            "tool.exe"
        ]
    );
    let o = Options {
        metric: true,
        ..opts()
    };
    assert_eq!(qo(&o, "no-metric:size:>1mb"), q("size:>1mb"));
}

#[test]
fn mod_hex() {
    check!("hex:524541444d45", ["README"]);
    assert_eq!(err("hex:52454").kind, ErrorKind::Value);
}

#[test]
fn mod_hex_to_number() {
    check!("hex-to-number:stem:>0x100", ["123.txt", "2024"]);
}

#[test]
fn mod_binary() {
    check!("binary:readme", []);
    check!("binary:README", ["README"]);
}

#[test]
fn mod_to_integer() {
    check!("to-integer:>100", ["123.txt", "2024"]);
}

#[test]
fn mod_to_number() {
    check!("to-number:<200", ["123.txt"]);
    check!("#:<200", ["123.txt"]);
}

#[test]
fn mod_to_string() {
    check!(
        "to-string:size:5000",
        ["IMG_0001.PNG", "Notes.TXT", "report.pdf"]
    );
}

#[test]
fn mod_multi_string() {
    check!("stem:=blue", []);
    check!("multi-string:stem:=blue", ["red;blue.txt"]);
    check!("no-multi-string:stem:=blue", []);
}

#[test]
fn mod_all_strings() {
    check!("multi-string:stem:b", ["bin", "red;blue.txt"]);
    check!("all-strings:stem:b", ["bin"]);
}

#[test]
fn mod_regex_flags() {
    // Names have no line breaks: these change what a regex may match, not these results.
    for m in [
        "each-line:",
        "multi-line:",
        "dot-all:",
        "global:",
        "no-fast-regex:",
    ] {
        assert_eq!(q(&format!("{m}regex:^so")), q("regex:^so"), "{m}");
    }
}

#[test]
fn mod_ungreedy() {
    check!(
        "ungreedy:regex:^(.+)o $1:ong",
        ["Song-Two.flac", "song one.mp3"]
    );
}

#[test]
fn mod_star_all() {
    check!(
        "star-all:C:\\fixture\\*.txt",
        ["123.txt", "Notes.TXT", "red;blue.txt"]
    );
}

#[test]
fn mod_soundex() {
    check!("soundex:tule", ["tool.exe"]);
}

#[test]
fn mod_pinyin_is_not_yet_supported() {
    for m in [
        "pinyin:a",
        "py:a",
        "pinyin-jian:a",
        "pyj:a",
        "pinyin-quan:a",
        "pyq:a",
    ] {
        let e = err(m);
        assert_eq!(e.kind, ErrorKind::Unsupported, "{m}");
        assert!(e.message.contains("not yet supported"));
    }
    check!("no-pinyin:song", ["Song-Two.flac", "song one.mp3"]);
}

#[test]
fn mod_no_substitute() {
    check!("no-substitute:$name:", []);
}

#[test]
fn mod_accepted_without_effect_on_matching() {
    for m in [
        "no-highlight:",
        "from-disk:",
        "fromdisk:",
        "global:",
        "no-fast-regex:",
    ] {
        assert_eq!(q(&format!("{m}song")), q("song"), "{m}");
    }
}

#[test]
fn mod_no_reorder() {
    let p = parse("no-reorder: type:file zzz", &opts()).unwrap();
    assert!(p.globals().no_reorder);
    TYPE_CALLS.with(|c| c.set(0));
    let _ = q("type:file zzz");
    assert_eq!(TYPE_CALLS.with(Cell::get), 0, "cheap name term first");
    let _ = q("no-reorder: type:file zzz");
    assert_eq!(TYPE_CALLS.with(Cell::get), tree().len() as u32);
}

#[test]
fn mod_global_switches() {
    let g = |s: &str| parse(s, &opts()).unwrap().globals().clone();
    assert_eq!(
        g("search-current-results:").search_current_results,
        Some(true)
    );
    assert_eq!(
        g("no-search-current-results:").search_current_results,
        Some(false)
    );
    assert_eq!(
        g("and-filter:").filter_combine,
        Some(keel_query::FilterCombine::And)
    );
    assert_eq!(
        g("or-filter:").filter_combine,
        Some(keel_query::FilterCombine::Or)
    );
    assert_eq!(g("enable-result-omissions:").result_omissions, Some(true));
    assert_eq!(g("disable-result-omissions:").result_omissions, Some(false));
    assert_eq!(
        g("enable-temp-result-omissions:").temp_result_omissions,
        Some(true)
    );
    assert_eq!(
        g("disable-temp-result-omissions:").temp_result_omissions,
        Some(false)
    );
    assert!(g("menu-break:").menu_break);
    assert!(g("menu-separator:").menu_separator);
    assert_eq!(g("and-precedence:").precedence, Precedence::AndFirst);
    assert_eq!(g("or-precedence:").precedence, Precedence::OrFirst);
    // A global modifier alone filters nothing, even inside an OR.
    assert_eq!(count("zzz | no-reorder:"), 0);
}

#[test]
fn mod_no_not() {
    check!("no-!:!x", []);
    assert_eq!(count("!x"), 17);
    assert_eq!(count("case:!x"), 18);
}

// ---- size constants and units ----------------------------------------------------------

#[test]
fn size_empty() {
    check!("size:empty", ["archive.zip", "empty"]);
}

#[test]
fn size_tiny() {
    check!(
        "size:tiny",
        [".hidden", "123.txt", "Notes.TXT", "README", "red;blue.txt"]
    );
}

#[test]
fn size_small() {
    check!("size:small", ["Résumé.docx"]);
}

#[test]
fn size_medium() {
    check!("size:medium", ["IMG_0001.PNG", "report.pdf", "setup.msi"]);
}

#[test]
fn size_large() {
    check!("size:large", ["photo.jpg", "song one.mp3"]);
}

#[test]
fn size_huge() {
    check!("size:huge", ["Song-Two.flac"]);
}

#[test]
fn size_gigantic() {
    check!("size:gigantic", ["tool.exe"]);
}

#[test]
fn size_constant_comparison() {
    assert_eq!(count("size:<small"), 7);
}

#[test]
fn size_units() {
    check!("size:5000b", ["Notes.TXT"]);
    check!("size:1048576", ["setup.msi"]);
    check!("size:1mb", ["photo.jpg", "setup.msi"]);
    check!("size:1mib", ["photo.jpg", "setup.msi"]);
    check!("size:>0.1gb", ["tool.exe"]);
    check!(
        "size:150kb..1mb",
        ["IMG_0001.PNG", "photo.jpg", "setup.msi"]
    );
    assert_eq!(err("size:big").span, 5..8);
}

// ---- date constants and formats --------------------------------------------------------

#[test]
fn date_today() {
    check!("dm:today", [".hidden", "report.pdf"]);
}

#[test]
fn date_yesterday() {
    check!("dm:yesterday", ["Notes.TXT"]);
}

#[test]
fn date_tomorrow() {
    check!("dm:tomorrow", ["setup.msi"]);
}

#[test]
fn date_this_week() {
    check!("dm:thisweek", [".hidden", "report.pdf", "setup.msi"]);
    check!("dm:currentweek", [".hidden", "report.pdf", "setup.msi"]);
}

#[test]
fn date_last_week() {
    check!("dm:lastweek", ["Notes.TXT", "Résumé.docx", "photo.jpg"]);
    check!("dm:prevweek", ["Notes.TXT", "Résumé.docx", "photo.jpg"]);
}

#[test]
fn date_week_starts_sunday_option() {
    let o = Options {
        week_starts_sunday: true,
        ..opts()
    };
    assert_eq!(
        qo(&o, "dm:thisweek"),
        [".hidden", "Notes.TXT", "report.pdf", "setup.msi"]
    );
}

#[test]
fn date_next_week() {
    check!("dm:nextweek", []);
    check!("dm:comingweek", []);
}

#[test]
fn date_this_month() {
    check!(
        "dm:thismonth",
        [
            ".hidden",
            "Notes.TXT",
            "Résumé.docx",
            "docs",
            "photo.jpg",
            "report.pdf",
            "setup.msi"
        ]
    );
}

#[test]
fn date_last_month() {
    check!("dm:lastmonth", ["archive.zip"]);
}

#[test]
fn date_this_year() {
    assert_eq!(count("dm:thisyear"), 10);
}

#[test]
fn date_last_year() {
    check!("dm:lastyear", ["song one.mp3"]);
}

#[test]
fn date_past_week() {
    check!(
        "dm:pastweek",
        [".hidden", "Notes.TXT", "photo.jpg", "report.pdf"]
    );
}

#[test]
fn date_past_n_days() {
    check!("dm:past2days", [".hidden", "Notes.TXT", "report.pdf"]);
    check!("dm:last2days", [".hidden", "Notes.TXT", "report.pdf"]);
}

#[test]
fn date_n_hours() {
    check!("dm:3hours", [".hidden", "report.pdf"]);
    check!("dm:past3hours", [".hidden", "report.pdf"]);
}

#[test]
fn date_last_n_weeks() {
    assert_eq!(count("dm:last2weeks"), 7);
}

#[test]
fn date_next_n_days() {
    check!("dm:next2days", ["setup.msi"]);
}

#[test]
fn date_to_date() {
    assert_eq!(count("dm:mtd"), 6);
    assert_eq!(count("dm:qtd"), 6);
    assert_eq!(count("dm:ytd"), 9);
}

#[test]
fn date_month_names() {
    check!("dm:august", ["IMG_0001.PNG"]);
    check!("dm:aug", ["IMG_0001.PNG"]);
}

#[test]
fn date_weekday_names() {
    check!("dm:friday", ["photo.jpg"]);
    check!("dm:sun", ["Notes.TXT"]);
}

#[test]
fn date_iso_8601() {
    check!("dm:2024", ["123.txt", "tool.exe"]);
    check!("dm:2024-02", ["tool.exe"]);
    check!("dm:2024-02-29", ["tool.exe"]);
    check!("dm:2024-02-29T13", ["tool.exe"]);
    check!("dm:2024-02-29T13:05", ["tool.exe"]);
    check!("dc:2024-02-29T13:05:00", ["report.pdf"]);
}

#[test]
fn date_iso_basic_and_yymmdd() {
    check!("dm:20240229", ["tool.exe"]);
    check!("dm:20240229T1305", ["tool.exe"]);
    check!("dm:240229", ["tool.exe"]);
}

#[test]
fn date_local_formats() {
    check!("dm:02/29/2024", ["tool.exe"]);
    check!("dm:2024/02/29", ["tool.exe"]);
    check!("dm:07/2024", ["123.txt"]);
    let o = Options {
        date_order: DateOrder::Dmy,
        ..opts()
    };
    assert_eq!(qo(&o, "dm:29/02/2024"), ["tool.exe"]);
    assert_eq!(qo(&o, "dm:29.02.2024"), ["tool.exe"]);
}

#[test]
fn date_excel_and_filetime() {
    check!("dm:45000", ["Song-Two.flac"]);
    check!("dm:133536855000000000", ["tool.exe"]);
}

#[test]
fn date_comparisons() {
    assert_eq!(count("dm:>2025"), 10);
    assert_eq!(count("dm:>=2025"), 11);
    check!("dm:<2024", ["README", "Song-Two.flac"]);
    check!(
        "dm:<=2024",
        ["123.txt", "README", "Song-Two.flac", "tool.exe"]
    );
}

#[test]
fn date_ranges() {
    check!("dm:2023..2024", ["123.txt", "Song-Two.flac", "tool.exe"]);
    check!("dm:2023-2024", ["123.txt", "Song-Two.flac", "tool.exe"]);
}

#[test]
fn date_time_of_day() {
    check!("dm:9am..10am", ["report.pdf"]);
}

#[test]
fn date_iso_wildcards() {
    assert_eq!(count("dm:*-*-*T9:00..*-*-*T13:59"), 8);
    check!(
        "dm:*-10-*",
        [
            ".hidden",
            "Notes.TXT",
            "Résumé.docx",
            "docs",
            "photo.jpg",
            "report.pdf",
            "setup.msi"
        ]
    );
}

#[test]
fn date_unknown_and_any() {
    check!(
        "dm:unknown",
        [
            "2024",
            "C:",
            "bin",
            "fixture",
            "music",
            "offline.dat",
            "pics",
            "red;blue.txt"
        ]
    );
    assert_eq!(count("dm:"), 15);
    assert_eq!(err("x dm:soon").span, 5..9);
}

// ---- functions: name, path, type -------------------------------------------------------

#[test]
fn fn_name() {
    check!("name:song", ["Song-Two.flac", "song one.mp3"]);
    check!("name:*.mp3", ["song one.mp3"]);
}

#[test]
fn fn_stem() {
    check!("stem:readme", ["README"]);
    check!("stem:empty", ["empty"]);
    check!("stem:=report", ["report.pdf"]);
}

#[test]
fn fn_extension() {
    check!(
        "extension:x",
        [
            "123.txt",
            "Notes.TXT",
            "Résumé.docx",
            "red;blue.txt",
            "tool.exe"
        ]
    );
}

#[test]
fn fn_ext() {
    check!("ext:pdf", ["report.pdf"]);
    check!("ext:.PDF", ["report.pdf"]);
    check!("ext:jp*", ["photo.jpg"]);
    check!("ext:p", []);
    assert_eq!(count("ext:"), 14);
}

#[test]
fn fn_full_path() {
    check!("full-path:docs\\r", ["Résumé.docx", "report.pdf"]);
    check!("fullpath:docs\\r", ["Résumé.docx", "report.pdf"]);
}

#[test]
fn fn_full_path_length() {
    check!("full-path-length:<=3", ["C:"]);
    check!("fullpathlen:26", ["report.pdf"]);
}

#[test]
fn fn_name_length() {
    check!("name-length:2", ["C:"]);
    check!("namelen:>12", ["Song-Two.flac"]);
}

#[test]
fn fn_parent() {
    check!(
        "parent:C:\\fixture\\docs",
        ["Notes.TXT", "Résumé.docx", "empty", "report.pdf"]
    );
    check!(
        "parent:C:\\fixture\\docs\\",
        ["Notes.TXT", "Résumé.docx", "empty", "report.pdf"]
    );
    check!("parent:c:\\", ["fixture"]);
    check!(
        "infolder:C:\\fixture\\music",
        ["Song-Two.flac", "song one.mp3"]
    );
    check!("nosubfolders:C:\\fixture\\bin", ["setup.msi", "tool.exe"]);
}

#[test]
fn fn_parent_name() {
    check!("parent-name:music", ["Song-Two.flac", "song one.mp3"]);
    check!("parent-name:2024", ["123.txt"]);
}

#[test]
fn fn_parent_count() {
    check!("depth:0", ["C:"]);
    check!("parent-count:1", ["fixture"]);
    assert_eq!(count("parents:>2"), 11);
}

#[test]
fn fn_root() {
    check!("root:", ["C:"]);
    assert_eq!(err("root:x").kind, ErrorKind::Value);
}

#[test]
fn fn_type() {
    check!(
        "type:\"Text Document\"",
        ["123.txt", "Notes.TXT", "red;blue.txt"]
    );
    assert_eq!(q("type:\"File folder\""), DIRS);
    // Windows calls a zip a "Compressed (zipped) Folder".
    check!("type:zipped", ["archive.zip"]);
    check!("type:offline", ["offline.dat"]);
    check!("type:\"FLAC File\"", ["Song-Two.flac"]);
}

#[test]
fn fn_kind() {
    check!("kind:image", ["IMG_0001.PNG", "photo.jpg"]);
    assert_eq!(q("kind:folder"), DIRS);
    check!("kind:archive", ["archive.zip"]);
    check!("kind:executable", ["setup.msi", "tool.exe"]);
    check!("kind:audio", ["Song-Two.flac", "song one.mp3"]);
    assert_eq!(count("kind:document"), 5);
    assert_eq!(err("kind:bogus").kind, ErrorKind::Value);
}

#[test]
fn fn_in() {
    check!(
        "in:C:\\fixture\\docs",
        ["Notes.TXT", "Résumé.docx", "empty", "report.pdf"]
    );
    check!("in:C:\\fixture\\music\\", ["Song-Two.flac", "song one.mp3"]);
}

// ---- functions: sizes, counts, attributes ----------------------------------------------

#[test]
fn fn_size() {
    check!("size:150000", ["report.pdf"]);
}

#[test]
fn fn_size_on_disk() {
    check!("size-on-disk:8kb", ["Notes.TXT"]);
    check!("sizeondisk:>100mb", ["tool.exe"]);
}

#[test]
fn fn_child_count() {
    check!("child-count:0", ["empty"]);
    check!("childcount:>3", ["docs", "fixture"]);
}

#[test]
fn fn_child_file_count() {
    check!("child-file-count:2", ["bin", "music", "pics"]);
}

#[test]
fn fn_child_folder_count() {
    check!("child-folder-count:>=1", ["C:", "docs", "fixture"]);
}

#[test]
fn fn_empty() {
    check!("empty:", ["empty"]);
}

#[test]
fn fn_attributes() {
    check!("attrib:H", [".hidden", "C:", "Song-Two.flac"]);
    check!("attr:RA", ["Notes.TXT"]);
    assert_eq!(q("attributes:D"), DIRS);
    check!("attrib:0x4", ["C:", "setup.msi"]);
    check!("attribute:c", ["IMG_0001.PNG"]);
    assert_eq!(err("attrib:Q").kind, ErrorKind::Value);
}

#[test]
fn fn_run_count() {
    check!("run-count:>3", ["report.pdf", "tool.exe"]);
    check!("runcount:12", ["tool.exe"]);
}

#[test]
fn fn_file_ids() {
    check!("file-id:1001", ["report.pdf"]);
    check!("frn:1001", ["report.pdf"]);
    check!("parent-file-id:1000", ["report.pdf"]);
    check!("parentfrn:1000", ["report.pdf"]);
}

// ---- functions: dates ------------------------------------------------------------------

#[test]
fn fn_date_modified() {
    check!("date-modified:yesterday", ["Notes.TXT"]);
    check!("datemodified:yesterday", ["Notes.TXT"]);
}

#[test]
fn fn_date_created() {
    check!("dc:2025", ["Notes.TXT"]);
    check!("date-created:2025", ["Notes.TXT"]);
}

#[test]
fn fn_date_accessed() {
    check!("da:today", ["report.pdf"]);
    check!("date-accessed:sat", ["photo.jpg"]);
}

#[test]
fn fn_date_recently_changed() {
    check!("rc:today", ["report.pdf"]);
    check!("date-recently-changed:yesterday", ["Notes.TXT"]);
}

#[test]
fn fn_date_run() {
    check!("date-run:yesterday", ["report.pdf"]);
    check!("dr:thismonth", ["report.pdf", "tool.exe"]);
    check!("daterun:thismonth", ["report.pdf", "tool.exe"]);
}

#[test]
fn fn_lazy_dates() {
    check!("date-taken:2026-07", ["photo.jpg"]);
    assert_eq!(count("date-taken:unknown"), tree().len() - 1);
    for f in [
        "date-deleted",
        "date-saved",
        "date-printed",
        "date-received",
        "date-sent",
        "date-released",
        "date-acquired",
        "date-changed",
        "date-content-created",
        "date-encoded",
        "date-media-created",
    ] {
        assert_eq!(count(&format!("{f}:unknown")), tree().len(), "{f}");
    }
}

#[test]
fn fn_date_year() {
    check!("dmyear:2024", ["123.txt", "tool.exe"]);
    check!(
        "dmyear:2023..2024",
        ["123.txt", "Song-Two.flac", "tool.exe"]
    );
    check!("dcyear:2024", ["report.pdf"]);
    check!("date-created-year:2024", ["report.pdf"]);
    check!("dayear:2026", ["photo.jpg", "report.pdf"]);
}

#[test]
fn fn_date_month() {
    check!("dmmonth:3", ["Song-Two.flac"]);
    check!("dmmonth:february", ["tool.exe"]);
}

#[test]
fn fn_date_day() {
    check!("dmday:29", ["tool.exe"]);
    check!("dmday:15", ["Song-Two.flac", "song one.mp3"]);
    check!("rcday:11", ["Notes.TXT"]);
}

#[test]
fn fn_date_day_of_week() {
    check!("dmdow:friday", ["photo.jpg"]);
    check!(
        "dmdow:thu",
        ["123.txt", "IMG_0001.PNG", "docs", "empty", "tool.exe"]
    );
    check!("dmdow:0", ["Notes.TXT", "song one.mp3"]);
    check!(
        "dmdow:monday..tuesday",
        [".hidden", "Résumé.docx", "report.pdf", "setup.msi"]
    );
    check!("dadow:sat", ["photo.jpg"]);
    check!("date-modified-day-of-week:fri", ["photo.jpg"]);
}

#[test]
fn fn_date_hour() {
    check!("dmhour:9", ["Song-Two.flac", "report.pdf", "setup.msi"]);
    check!("dmhour:2pm", ["song one.mp3"]);
    check!("dmhour:>=17", ["Notes.TXT", "archive.zip", "photo.jpg"]);
    check!("dm-hour:13", ["tool.exe"]);
}

#[test]
fn fn_date_time() {
    check!(
        "dmtime:9am..10am",
        ["Song-Two.flac", "docs", "report.pdf", "setup.msi"]
    );
    check!("dmtime:17:45", ["photo.jpg"]);
}

#[test]
fn fn_date_date() {
    check!("dmdate:2024-02-29", ["tool.exe"]);
    check!("dmdate:yesterday", ["Notes.TXT"]);
    check!("dmdate:today", [".hidden", "report.pdf"]);
}

// ---- functions: tree -------------------------------------------------------------------

const BELOW_FIXTURE_FOLDERS: [&str; 11] = [
    "123.txt",
    "IMG_0001.PNG",
    "Notes.TXT",
    "Résumé.docx",
    "Song-Two.flac",
    "empty",
    "photo.jpg",
    "report.pdf",
    "setup.msi",
    "song one.mp3",
    "tool.exe",
];

const IN_DOCS: [&str; 4] = ["Notes.TXT", "Résumé.docx", "empty", "report.pdf"];

/// A second tree: folders with sizes and no child counts of their own.
fn sized() -> Fixture {
    let dir = |name, path, size| E {
        name,
        path,
        dir: true,
        size: Some(size),
        ..E::default()
    };
    let file = |name, path| E {
        name,
        path,
        size: Some(10),
        ..E::default()
    };
    Fixture::new(vec![
        dir("C:", "C:\\", 5_050),
        dir("big", "C:\\big", 5_000),
        file("a", "C:\\big\\a"),
        dir("small", "C:\\small", 50),
        file("b", "C:\\small\\b"),
        file("c", "C:\\small\\c"),
        dir("none", "C:\\none", 0),
    ])
}

fn sorted(mut v: Vec<&'static str>) -> Vec<&'static str> {
    v.sort_unstable();
    v
}

#[test]
fn tree_functions_need_a_tree() {
    for s in [
        "child:docs",
        "sibling:readme",
        "exists:readme",
        "parentdm:2026",
    ] {
        let q = parse(s, &opts()).unwrap();
        assert!(!tree().iter().any(|e| q.matches(e)), "{s}");
    }
}

#[test]
fn fn_child() {
    check!("child:report.pdf", ["docs"]);
    check!("child:*.mp3", ["music"]);
    check!("child:docs", ["fixture"]);
    check!("child:fixture", ["C:"]);
    check!("child:report.pdf;photo.jpg", ["docs", "pics"]);
    check!(
        "child:",
        ["2024", "C:", "bin", "docs", "fixture", "music", "pics"]
    );
}

#[test]
fn fn_child_file() {
    check!("child-file:readme", ["fixture"]);
    check!("child-file:docs", []);
}

#[test]
fn fn_child_folder() {
    check!("child-folder:docs", ["fixture"]);
    check!("child-folder:empty", ["docs"]);
    check!("child-folder:readme", []);
}

#[test]
fn fn_child_counts_from_the_tree() {
    let t = sized();
    let q = |s: &str| sorted(t.select(&opts(), s));
    assert_eq!(q("child-count:2"), ["small"]);
    assert_eq!(q("child-count:3"), ["C:"]);
    assert_eq!(q("child-file-count:1"), ["big"]);
    assert_eq!(q("child-folder-count:3"), ["C:"]);
    assert_eq!(q("empty:"), ["none"]);
}

#[test]
fn fn_descendant() {
    check!("descendant:*.jpg", ["C:", "fixture", "pics"]);
    check!("descendant:empty", ["C:", "docs", "fixture"]);
}

#[test]
fn fn_descendant_file() {
    check!("descendant-file:123.txt", ["2024", "C:", "fixture"]);
    check!("descendant-file:empty", []);
}

#[test]
fn fn_descendant_folder() {
    check!("descendant-folder:empty", ["C:", "docs", "fixture"]);
    check!("descendant-folder:*.txt", []);
}

#[test]
fn fn_descendant_count() {
    check!("descendant-count:>10", ["C:", "fixture"]);
    check!("descendant-count:2", ["bin", "music", "pics"]);
    check!("descendant-count:0", ["empty"]);
    // Files have no descendants to count.
    assert_eq!(count("descendant-count:unknown"), 15);
}

#[test]
fn fn_descendant_file_count() {
    check!("descendant-file-count:3", ["docs"]);
    check!("descendant-file-count:15", ["C:", "fixture"]);
}

#[test]
fn fn_descendant_folder_count() {
    check!("descendant-folder-count:>=6", ["C:", "fixture"]);
    check!("descendant-folder-count:1", ["docs"]);
}

#[test]
fn tree_scans_are_bounded() {
    let o = Options {
        tree_scan_limit: 5,
        ..opts()
    };
    // C: and fixture have more than five items below: unknown, not a wrong count.
    assert_eq!(qo(&o, "descendant-count:>10"), Vec::<&str>::new());
    assert_eq!(qo(&o, "descendant-count:2"), ["bin", "music", "pics"]);
    assert_eq!(qo(&o, "descendant:*.jpg"), ["pics"]);
}

#[test]
fn fn_sibling() {
    check!(
        "sibling:README",
        [
            ".hidden",
            "2024",
            "archive.zip",
            "bin",
            "docs",
            "music",
            "offline.dat",
            "pics",
            "red;blue.txt"
        ]
    );
    check!("sibling:*.flac", ["song one.mp3"]);
    // An item is not its own sibling.
    check!("sibling:\"song one.mp3\"", ["Song-Two.flac"]);
}

#[test]
fn fn_sibling_file() {
    check!("sibling-file:*.mp3", ["Song-Two.flac"]);
    check!("sibling-file:empty", []);
}

#[test]
fn fn_sibling_folder() {
    check!(
        "sibling-folder:empty",
        ["Notes.TXT", "Résumé.docx", "report.pdf"]
    );
}

#[test]
fn fn_parent_sibling() {
    check!(
        "parent-sibling:pics",
        [
            "123.txt",
            "Notes.TXT",
            "Résumé.docx",
            "Song-Two.flac",
            "empty",
            "report.pdf",
            "setup.msi",
            "song one.mp3",
            "tool.exe"
        ]
    );
}

#[test]
fn fn_ancestor_sibling() {
    assert_eq!(q("ancestor-sibling:archive.zip"), BELOW_FIXTURE_FOLDERS);
    check!("ancestor-sibling:fixture", []);
}

#[test]
fn fn_ancestor() {
    assert_eq!(q("ancestor:C:\\fixture\\docs"), IN_DOCS);
    assert_eq!(q("ancestor:C:\\fixture\\docs\\"), IN_DOCS);
    assert_eq!(count("ancestor:C:\\fixture"), 21);
    assert_eq!(count("ancestor:C:\\"), 22);
    // `*` stays inside one folder name; `**` crosses folders.
    check!("ancestor:**\\music", ["Song-Two.flac", "song one.mp3"]);
    check!("ancestor:C:\\*\\music", ["Song-Two.flac", "song one.mp3"]);
    check!("ancestor:*\\music", []);
    check!("ancestor:C:\\fix", []);
}

#[test]
fn fn_ancestor_name() {
    assert_eq!(q("ancestor-name:docs"), IN_DOCS);
    check!("ancestor-name:pic", ["IMG_0001.PNG", "photo.jpg"]);
    assert_eq!(count("ancestor-name:fixture"), 21);
}

#[test]
fn fn_exists() {
    assert_eq!(q("exists:empty"), IN_DOCS);
    assert_eq!(q("exists:NOTES.txt"), IN_DOCS);
    assert_eq!(q("exists:..\\README"), BELOW_FIXTURE_FOLDERS);
    assert_eq!(count("exists:docs\\report.pdf"), 10);
    assert_eq!(count("exists:C:\\fixture\\bin\\tool.exe"), tree().len());
    check!("exists:C:\\nope", []);
    // With substitution: files that have a sibling of the same stem with another extension.
    check!("exists:$stem:.mp3", ["song one.mp3"]);
    assert_eq!(err("exists:").kind, ErrorKind::Value);
}

#[test]
fn fn_file_exists() {
    assert_eq!(q("file-exists:report.pdf"), IN_DOCS);
    check!("file-exists:empty", []);
}

#[test]
fn fn_folder_exists() {
    assert_eq!(q("folder-exists:empty"), IN_DOCS);
    check!("folder-exists:report.pdf", []);
}

#[test]
fn fn_parentsize() {
    let t = sized();
    assert_eq!(t.select(&opts(), "parentsize:5000"), ["a"]);
    assert_eq!(
        sorted(t.select(&opts(), "parentsize:>1kb")),
        ["a", "big", "none", "small"]
    );
    assert_eq!(sorted(t.select(&opts(), "parentsize:50")), ["b", "c"]);
    assert_eq!(
        sorted(t.select(&opts(), "parentsize:5050")),
        ["big", "none", "small"]
    );
}

#[test]
fn fn_parentdm() {
    assert_eq!(q("parentdm:2026-10-01"), IN_DOCS);
    assert_eq!(q("parentdm:thismonth"), IN_DOCS);
}

#[test]
fn fn_pathlist() {
    check!(
        "pathlist:C:\\fixture\\README;c:\\FIXTURE\\bin\\tool.exe",
        ["README", "tool.exe"]
    );
    check!("pathlist:C:\\", ["C:"]);
    check!("pathlist:\"C:\\fixture\\red;blue.txt\"", ["red;blue.txt"]);
}

#[test]
fn fn_filelist() {
    // Names, `|` or `;` separated, whole and without regard to case.
    check!("filelist:\"README|tool.exe\"", ["README", "tool.exe"]);
    check!("filelist:readme;SETUP.MSI", ["README", "setup.msi"]);
    check!("filelist:\"tool\"", []);
    check!("case:filelist:\"readme|tool.exe\"", ["tool.exe"]);
    assert_eq!(err("filelist:\"|\"").kind, ErrorKind::Value);
    assert_eq!(err("filelist:>a").kind, ErrorKind::Value);
}

#[test]
fn fn_filelist_filename_and_full_path() {
    // Only items read from a file list have these; the rest are unknown.
    check!("filelist-filename:media", ["song one.mp3"]);
    check!("filelist-filename:*.txt", ["photo.jpg"]);
    check!("filelist-filename:\"Photos 2026.txt\"", ["photo.jpg"]);
    check!(
        "filelist-full-path:d:\\lists",
        ["photo.jpg", "song one.mp3"]
    );
    // Forward slashes in the list's path read as backslashes.
    check!("filelist-full-path:\"D:\\lists\\Photos\"", ["photo.jpg"]);
    check!("filelist-filename: ext:mp3", ["song one.mp3"]);
    check!("filelist-filename:unknown ext:mp3;jpg", []);
    assert_eq!(count("filelist-filename:"), 2);
}

// ---- functions: duplicates and sets ----------------------------------------------------

const DM_2026_FILES: [&str; 8] = [
    "report.pdf",
    "Notes.TXT",
    "Résumé.docx",
    "photo.jpg",
    "IMG_0001.PNG",
    "setup.msi",
    "archive.zip",
    ".hidden",
];

const TXT: [&str; 3] = ["Notes.TXT", "123.txt", "red;blue.txt"];

fn both(a: &[&'static str], b: &[&'static str]) -> Vec<&'static str> {
    a.iter().chain(b).copied().collect()
}

#[test]
fn fn_dupe() {
    // Groups in value order, items of a group in result order.
    assert_eq!(ordered("dupe:size"), ["empty", "archive.zip"]);
    assert_eq!(ordered("file: dupe:ext"), TXT);
    assert_eq!(ordered("file:dupe:ext"), TXT);
    assert_eq!(ordered("file: dupe:type"), TXT);
    assert_eq!(
        ordered("file: dupe:dmyear"),
        both(&["tool.exe", "123.txt"], &DM_2026_FILES)
    );
    assert_eq!(ordered("dupe:size;ext"), Vec::<&str>::new());
    assert_eq!(ordered("dupe:size ext:zip"), Vec::<&str>::new());
    // Without a property, names compare (case-insensitively).
    assert_eq!(ordered("dupe:"), Vec::<&str>::new());
    assert_eq!(ordered("dupe:name"), Vec::<&str>::new());
    // Unknown values are never duplicates: offline.dat has no size.
    assert!(!ordered("dupe:size").contains(&"offline.dat"));
    let p = parse("dupe:size", &opts()).unwrap();
    assert!(p.has_set_functions());
    assert!(!parse("size:1", &opts()).unwrap().has_set_functions());
}

#[test]
fn fn_dupe_count() {
    assert_eq!(
        ordered("file: dupe:dmyear dupe-count:2"),
        ["tool.exe", "123.txt"]
    );
    assert_eq!(ordered("file: dupe:dmyear dupe-count:>=3"), DM_2026_FILES);
}

#[test]
fn fn_dupe_min() {
    assert_eq!(ordered("file: dupe:dmyear dupe-min:3"), DM_2026_FILES);
    assert_eq!(ordered("file: dupe:ext dupe-min:4"), Vec::<&str>::new());
    // Alone, `dupe-min:` means `dupe:name`; a later `dupe:` replaces that.
    assert_eq!(ordered("dupe-min:2"), Vec::<&str>::new());
    assert_eq!(ordered("file: dupe-min:2 dupe:ext"), TXT);
}

#[test]
fn fn_dupe_max() {
    assert_eq!(
        ordered("file: dupe:dmyear dupe-max:2"),
        ["tool.exe", "123.txt"]
    );
}

#[test]
fn fn_dupe_from() {
    assert_eq!(
        ordered("file: dupe:dmyear dupe-from:C:\\fixture\\2024"),
        ["tool.exe", "123.txt"]
    );
    assert_eq!(
        ordered("file: dupe:dmyear dupe-from:C:\\fixture\\bin"),
        both(&["tool.exe", "123.txt"], &DM_2026_FILES)
    );
    assert_eq!(
        ordered("file: dupe:dmyear dupe-from:C:\\fixture\\music"),
        Vec::<&str>::new()
    );
}

#[test]
fn fn_legacy_dupes() {
    assert_eq!(ordered("sizedupe:"), ["empty", "archive.zip"]);
    assert_eq!(ordered("namepartdupe:"), Vec::<&str>::new());
    assert_eq!(ordered("dmdupe:"), Vec::<&str>::new());
    assert_eq!(ordered("dcdupe:"), Vec::<&str>::new());
    assert_eq!(ordered("dadupe:"), Vec::<&str>::new());
    assert_eq!(ordered("folder: attribdupe:"), plain_dirs());
    assert_eq!(err("sizedupe:x").kind, ErrorKind::Value);
}

/// Folders with only the directory attribute, in fixture order.
fn plain_dirs() -> Vec<&'static str> {
    vec!["fixture", "docs", "empty", "music", "pics", "bin", "2024"]
}

#[test]
fn fn_unique() {
    assert_eq!(
        ordered("file: unique:ext"),
        [
            "report.pdf",
            "Résumé.docx",
            "song one.mp3",
            "Song-Two.flac",
            "photo.jpg",
            "IMG_0001.PNG",
            "tool.exe",
            "setup.msi",
            "archive.zip",
            "README",
            ".hidden",
            "offline.dat"
        ]
    );
}

#[test]
fn fn_distinct() {
    assert_eq!(
        ordered("file: distinct:ext"),
        [
            "report.pdf",
            "Notes.TXT",
            "Résumé.docx",
            "song one.mp3",
            "Song-Two.flac",
            "photo.jpg",
            "IMG_0001.PNG",
            "tool.exe",
            "setup.msi",
            "archive.zip",
            "README",
            ".hidden",
            "offline.dat"
        ]
    );
}

#[test]
fn fn_distinct_sort() {
    assert_eq!(
        ordered("file: distinct-sort:ext"),
        [
            "README",
            "offline.dat",
            "Résumé.docx",
            "tool.exe",
            "Song-Two.flac",
            ".hidden",
            "photo.jpg",
            "song one.mp3",
            "setup.msi",
            "report.pdf",
            "IMG_0001.PNG",
            "Notes.TXT",
            "archive.zip"
        ]
    );
}

#[test]
fn fn_count() {
    assert_eq!(ordered("count:3"), ["C:", "fixture", "docs"]);
    assert_eq!(ordered("ext:txt count:2"), ["Notes.TXT", "123.txt"]);
    assert_eq!(
        ordered("file: dupe:dmyear count:3"),
        ["tool.exe", "123.txt", "report.pdf"]
    );
    assert_eq!(ordered("count:0"), Vec::<&str>::new());
}

#[test]
fn set_functions_errors() {
    for (s, kind) in [
        ("a | dupe:size", ErrorKind::Syntax),
        ("!dupe:size", ErrorKind::Syntax),
        ("<a | count:2>", ErrorKind::Syntax),
        ("dupe:bogus", ErrorKind::Value),
        ("dupe:md5", ErrorKind::Value),
        ("count:x", ErrorKind::Value),
        ("count:1 count:2", ErrorKind::Value),
        ("dupe:size unique:ext", ErrorKind::Value),
        ("unique:ext dupe-min:3", ErrorKind::Value),
        ("dupe:size dupe-min:x", ErrorKind::Value),
    ] {
        assert_eq!(err(s).kind, kind, "{s}");
    }
    // Inside a group that is ANDed it is fine.
    assert_eq!(ordered("<file: dupe:ext>"), TXT);
}

#[test]
fn set_functions_are_bounded_in_memory() {
    let o = Options {
        set_memory_limit: 64,
        ..opts()
    };
    let t = Fixture::new(tree());
    let q = parse("dupe:size", &o).unwrap();
    let e = q.select(&t.items, Some(&t)).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Limit);
    assert_eq!(e.span, 0..9);
    // `count:` alone keeps no values and stops early.
    let q = parse("count:2", &o).unwrap();
    assert_eq!(q.select(&t.items, Some(&t)).unwrap(), [0, 1]);
}

// ---- functions: hard links, streams, containers ----------------------------------------

#[test]
fn fn_hard_link_count() {
    check!("hard-link-count:2", ["report.pdf"]);
    check!("hard-link-count:1", ["tool.exe"]);
    check!("hard-link-count:>=1", ["report.pdf", "tool.exe"]);
    assert_eq!(count("hard-link-count:unknown"), tree().len() - 2);
}

#[test]
fn fn_hard_link_filenames() {
    check!("hard-link-filenames:report-link", ["report.pdf"]);
    check!("hard-link-filenames:**\\bin\\*", ["report.pdf"]);
    check!("hard-link-filenames:nothing", []);
}

#[test]
fn fn_alternate_data_stream_count() {
    check!("alternate-data-stream-count:1", ["photo.jpg"]);
    check!("adscount:0", ["Notes.TXT"]);
    assert_eq!(count("adscount:unknown"), tree().len() - 2);
}

#[test]
fn fn_alternate_data_stream_names() {
    check!("alternate-data-stream-names:*.identifier", ["photo.jpg"]);
    check!("adsnames:zone", ["photo.jpg"]);
}

#[test]
fn fn_container_file_count() {
    check!("container-file-count:2", ["archive.zip"]);
    assert_eq!(count("container-file-count:unknown"), tree().len() - 1);
}

#[test]
fn fn_container_filenames() {
    check!("container-filenames:*.md", ["archive.zip"]);
    check!("container-filenames:a.txt", ["archive.zip"]);
    check!("container-filenames:docs\\a.txt", ["archive.zip"]);
}

// ---- macros, aliases, unsupported, UI --------------------------------------------------

#[test]
fn macro_audio() {
    check!("audio:", ["Song-Two.flac", "song one.mp3"]);
    check!("audio:two", ["Song-Two.flac"]);
}

#[test]
fn macro_zip() {
    check!("zip:", ["archive.zip"]);
}

#[test]
fn macro_doc() {
    check!(
        "doc:",
        [
            "123.txt",
            "Notes.TXT",
            "Résumé.docx",
            "red;blue.txt",
            "report.pdf"
        ]
    );
}

#[test]
fn macro_exe() {
    check!("exe:", ["setup.msi", "tool.exe"]);
}

#[test]
fn macro_pic_and_image() {
    check!("pic:", ["IMG_0001.PNG", "photo.jpg"]);
    check!("image:", ["IMG_0001.PNG", "photo.jpg"]);
}

#[test]
fn macro_video() {
    check!("video:", []);
}

#[test]
fn macro_user() {
    let mut macros = BTreeMap::new();
    macros.insert("mine".to_string(), "ext:mp3|ext:jpg".to_string());
    macros.insert("audio".to_string(), "ext:flac".to_string());
    let o = Options { macros, ..opts() };
    assert_eq!(qo(&o, "mine:"), ["photo.jpg", "song one.mp3"]);
    assert_eq!(qo(&o, "mine:song"), ["song one.mp3"]);
    assert_eq!(qo(&o, "audio:"), ["Song-Two.flac"]);
}

#[test]
fn unsupported_functions_are_refused_with_the_reason() {
    for s in [
        "utf8content:x",
        "ansicontent:x",
        "utf16content:x",
        "content:x",
        "width:>100",
        "artist:x",
        "md5:x",
        "tag:x",
        "network-index-host:x",
    ] {
        let e = err(s);
        assert_eq!(e.kind, ErrorKind::Unsupported, "{s}");
        assert!(
            e.message.contains("not yet supported"),
            "{s}: {}",
            e.message
        );
        assert_eq!(e.span.start, 0, "{s}");
    }
}

#[test]
fn unknown_names_are_errors() {
    let e = err("song extt:mp3");
    assert_eq!((e.kind, e.span), (ErrorKind::Unknown, 5..10));
    // Drive letters stay text, and quotes make anything text.
    check!("C:\\fixture\\README", ["README"]);
    check!("\"foo:bar\"", []);
}

#[test]
fn syntax_errors_have_positions() {
    let e = err("song <one");
    assert_eq!((e.kind, e.span), (ErrorKind::Syntax, 5..6));
    assert_eq!(err("regex:\"(\"").kind, ErrorKind::Regex);
    assert!(err("size:big").to_string().contains("at 5"));
}

#[test]
fn ui_functions_are_recorded_not_filtered() {
    let p = parse("sort:size columns:name;size song", &opts()).unwrap();
    let ui: Vec<(&str, &str)> = p
        .globals()
        .ui
        .iter()
        .map(|u| (u.name.as_str(), u.value.as_str()))
        .collect();
    assert_eq!(ui, [("sort", "size"), ("columns", "name;size")]);
    check!("sort:size song", ["Song-Two.flac", "song one.mp3"]);
}

// ---- the language reference lists every name -------------------------------------------

#[test]
fn docs_list_every_name() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/search.md");
    let doc = std::fs::read_to_string(path).unwrap();
    let mut missing = Vec::new();
    let mut need = |n: &str| {
        if !doc.contains(&format!("`{n}:`")) {
            missing.push(n.to_string());
        }
    };
    for m in modifiers::MODIFIERS {
        m.names.iter().for_each(|n| need(n));
    }
    for f in functions::FUNCTIONS {
        f.names.iter().for_each(|n| need(n));
    }
    for (names, _, _) in functions::DATES {
        names.iter().for_each(|n| need(n));
    }
    for (names, _) in functions::FILTER_MACROS {
        names.iter().for_each(|n| need(n));
    }
    for (names, _) in functions::UNSUPPORTED {
        names.iter().for_each(|n| need(n));
    }
    functions::UI_FUNCTIONS.iter().for_each(|n| need(n));
    for (part, _) in functions::DATE_PARTS {
        if !doc.contains(&format!("`-{part}`")) {
            missing.push(format!("-{part}"));
        }
    }
    assert!(
        missing.is_empty(),
        "docs/search.md does not list: {missing:?}"
    );
}
