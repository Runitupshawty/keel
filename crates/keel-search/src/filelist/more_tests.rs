//! The search language over file lists, `.metadata.efu` sidecars, the editor's operations
//! and file lists in the index.

use super::edit::{add_paths, merge, remove_duplicates, roots, update, Updated};
use super::search::wants_language;
use super::sidecar::{properties_of, Sidecar, MAX_SIDECAR_BYTES, SIDECAR_NAME};
use super::*;
use crate::filelist::index::WithFileLists;
use crate::{FileListRoot, IndexSettings, Query, Searcher, Unavailable};
use std::path::{Path, PathBuf};

fn efu(text: &str) -> FileList {
    read(text.as_bytes(), ListKind::Efu, &Limits::default()).unwrap()
}

fn names(s: &dyn Searcher, text: &str) -> Vec<String> {
    let q = Query {
        text: text.into(),
        ..Query::default()
    };
    s.query(&q)
        .unwrap_or_else(|e| panic!("{text}: {e:#}"))
        .iter()
        .map(|h| h.path.name().to_owned())
        .collect()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("keel-filelist2-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, rel: &str, body: &[u8]) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path
    }

    fn path(&self, rel: &str) -> String {
        self.0.join(rel).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const LIST: &str = concat!(
    "Filename,Size,Date Modified,Attributes,Rating\n",
    "/data/Reports,,,16,\n",
    "/data/Reports/report.pdf,10,2024-03-05T14:07:09Z,32,5\n",
    "/data/Reports/annual report.PDF,20,2026-01-02T00:00:00Z,33,\n",
    "/data/report,30,,32,\n",
    "/other/notes.txt,40,,32,3\n",
);

#[test]
fn queries_that_need_the_language() {
    for plain in ["", "report", "*.pdf", "folder:", "in:/data", "regex:^a", "C:\\x y"] {
        assert!(!wants_language(plain), "{plain}");
    }
    for lang in ["size:>1", "a|b", "!a", "<a b>", "ext:pdf", "case:A", "dm:2024", "filelist:a"] {
        assert!(wants_language(lang), "{lang}");
    }
    // An unknown function stays text for the index syntax; a bad value is the language's.
    assert!(!wants_language("nosuchfunction:x"));
    assert!(wants_language("size:big"));
}

#[test]
fn a_list_is_searched_with_the_whole_language() {
    let s = FileListSearcher::new("test.efu", efu(LIST)).with_source("/lists/test.efu");
    assert_eq!(names(&s, "size:>15"), ["report", "annual report.PDF", "notes.txt"]);
    assert_eq!(names(&s, "ext:pdf;txt size:<35"), ["annual report.PDF", "report.pdf"]);
    assert_eq!(names(&s, "report !pdf"), ["report", "Reports"]);
    assert_eq!(names(&s, "notes | annual"), ["annual report.PDF", "notes.txt"]);
    assert_eq!(names(&s, "dm:2026"), ["annual report.PDF"]);
    assert_eq!(names(&s, "attrib:R"), ["annual report.PDF"]);
    assert_eq!(names(&s, "attrib:D"), ["Reports"]);
    assert_eq!(names(&s, "filelist:\"notes.txt|REPORT\""), ["report", "notes.txt"]);
    assert_eq!(names(&s, "filelist-filename:test.efu").len(), 5);
    assert_eq!(names(&s, "filelist-full-path:lists ext:txt"), ["notes.txt"]);
    assert!(names(&s, "filelist-filename:other.efu").is_empty());
    // Set functions keep their own order.
    assert_eq!(names(&s, "count:2 report").len(), 2);
    // Without a source, the list's name is unknown.
    let bare = FileListSearcher::new("x", efu(LIST));
    assert!(names(&bare, "filelist-filename:x").is_empty());
    assert_eq!(names(&bare, "filelist-filename:unknown").len(), 5);
    // The query's own switches and folder.
    let q = Query {
        text: "ext:pdf".into(),
        within: Some(PathBuf::from("/data/Reports")),
        folders_only: false,
        ..Query::default()
    };
    assert_eq!(s.query(&q).unwrap().len(), 2);
    let case = Query {
        text: "ext:PDF".into(),
        match_case: true,
        ..Query::default()
    };
    assert_eq!(s.query(&case).unwrap().len(), 1);
    // Errors are reported, with the language's message.
    let e = s
        .query(&Query {
            text: "size:big".into(),
            ..Query::default()
        })
        .unwrap_err();
    assert!(format!("{e}").contains("big"), "{e}");
}

#[test]
fn sidecars_give_a_folders_files_properties() {
    let text = concat!(
        "Filename,Size,Rating,Tags,Comment\n",
        "a.jpg,1,5,holiday;beach,\n",
        "\"C:\\Photos\\B.JPG\",,4,,\"the \"\"best\"\" one\"\n",
        "C:\\Other\\c.jpg,,1,,\n",
        "sub\\d.jpg,,2,,\n",
        "a.jpg,,,,first comment\n",
        "..,,3,,\n",
    );
    let s = Sidecar::parse(text.as_bytes(), Some("c:/photos/"), &Limits::default()).unwrap();
    assert_eq!(s.columns, ["Rating", "Tags", "Comment"]);
    assert_eq!(s.len(), 2, "c.jpg is in another folder, d.jpg below, `..` is no file");
    assert_eq!(
        s.properties("A.jpg"),
        [("Rating", "5"), ("Tags", "holiday;beach"), ("Comment", "first comment")]
    );
    assert_eq!(s.property("b.jpg", "comment"), Some("the \"best\" one"));
    assert_eq!(s.property("b.jpg", "Tags"), None);
    assert_eq!(s.property("b.jpg", "Size"), None, "not a property");
    assert!(s.properties("c.jpg").is_empty());
    // Without the folder, only bare names count.
    let s = Sidecar::parse(text.as_bytes(), None, &Limits::default()).unwrap();
    assert_eq!(s.len(), 1);
    // Bounded.
    let small = Limits {
        max_rows: 2,
        max_line: 4096,
    };
    assert!(Sidecar::parse(text.as_bytes(), None, &small).is_err());
    assert!(Sidecar::parse(&b"Name,Rating\nx,1\n"[..], None, &Limits::default()).is_err());
}

#[test]
fn sidecars_are_read_from_the_files_folder() {
    let d = TempDir::new("sidecar");
    let photo = d.file("photo.jpg", b"x");
    let other = d.file("other.jpg", b"x");
    assert!(properties_of(&photo).unwrap().is_empty(), "no sidecar");
    assert_eq!(Sidecar::read(&d.0).unwrap(), None);
    d.file(SIDECAR_NAME, b"Filename,Rating,Title\nphoto.jpg,4,Sunset\n");
    assert_eq!(
        properties_of(&photo).unwrap(),
        [
            ("Rating".to_owned(), "4".to_owned()),
            ("Title".to_owned(), "Sunset".to_owned())
        ]
    );
    assert!(properties_of(&other).unwrap().is_empty());
    assert!(properties_of(&d.0.join(SIDECAR_NAME)).unwrap().is_empty());
    // A broken sidecar is an error, not empty properties.
    d.file(SIDECAR_NAME, b"Rating\n4\n");
    assert!(properties_of(&photo).is_err());
    // Too large to read.
    let big = std::fs::File::create(d.0.join(SIDECAR_NAME)).unwrap();
    big.set_len(MAX_SIDECAR_BYTES + 1).unwrap();
    drop(big);
    assert!(Sidecar::read(&d.0).is_err());
}

#[test]
fn merge_joins_columns_and_remove_duplicates_keeps_the_first() {
    let mut a = efu("Filename,Rating\n/x/a.mp3,5\n/x/b.mp3,\n");
    let b = efu("Filename,Album,rating\n/x/B.MP3,Live,2\n/y/c.mp3,Studio,\n/x\\a.mp3,,\n");
    merge(&mut a, b, &Limits::default()).unwrap();
    assert_eq!(a.columns, ["Rating", "Album"]);
    let rows: Vec<(&str, Vec<&str>)> = (a.entries.iter())
        .map(|e| (e.path.as_str(), e.extra.iter().map(String::as_str).collect()))
        .collect();
    assert_eq!(
        rows,
        [
            ("/x/a.mp3", vec!["5", ""]),
            ("/x/b.mp3", vec!["", ""]),
            ("/x/B.MP3", vec!["2", "Live"]),
            ("/y/c.mp3", vec!["", "Studio"]),
            ("/x\\a.mp3", vec!["", ""]),
        ]
    );
    assert_eq!(remove_duplicates(&mut a), 2);
    let paths: Vec<&str> = a.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["/x/a.mp3", "/x/b.mp3", "/y/c.mp3"]);
    assert_eq!(remove_duplicates(&mut a), 0);
    // Bounded: nothing is added past the limit.
    let small = Limits {
        max_rows: 4,
        ..Limits::default()
    };
    assert!(merge(&mut a, efu("Filename\n/1\n/2\n"), &small).is_err());
    assert_eq!(a.entries.len(), 3);
}

#[test]
fn roots_are_the_outermost_listed_folders() {
    let list = efu(concat!(
        "Filename,Attributes\n",
        "C:\\Music\\Live,16\n",
        "C:\\Music,16\n",
        "c:/music/live/2024,16\n",
        "C:\\Musical,16\n",
        "C:\\Music\\a.mp3,32\n",
        "D:\\b.txt,32\n",
        "C:\\MUSIC\\,16\n",
    ));
    assert_eq!(roots(&list), ["C:\\Music", "C:\\Musical"]);
}

fn rel(d: &TempDir, list: &FileList) -> Vec<String> {
    (list.entries.iter())
        .map(|e| {
            let p = Path::new(&e.path);
            let r = p.strip_prefix(&d.0).map_or_else(
                |_| e.path.clone(),
                |r| r.to_string_lossy().replace('\\', "/"),
            );
            if e.is_dir {
                format!("{r}/")
            } else {
                r
            }
        })
        .collect()
}

#[test]
fn add_paths_lists_folders_with_everything_below() {
    let d = TempDir::new("add");
    d.file("music/a.mp3", b"1");
    d.file("music/live/b.mp3", b"12");
    let single = d.file("notes.txt", b"123");
    let mut list = FileList::default();
    let n = add_paths(
        &mut list,
        &[d.0.join("music"), single, d.0.join("missing")],
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(n, 5);
    assert_eq!(
        rel(&d, &list),
        ["music/", "music/a.mp3", "music/live/", "music/live/b.mp3", "notes.txt"]
    );
    assert_eq!(list.entries[4].size, Some(3));
    let small = Limits {
        max_rows: 6,
        ..Limits::default()
    };
    assert!(add_paths(&mut list, &[d.0.join("music")], &small).is_err());
    assert_eq!(list.entries.len(), 5, "left as it was");
}

#[test]
fn update_walks_the_listed_folders_again() {
    let d = TempDir::new("update");
    d.file("music/a.mp3", b"1");
    d.file("music/gone.mp3", b"1");
    let notes = d.file("notes.txt", b"1");
    let mut list = FileList {
        columns: vec!["Rating".into()],
        ..FileList::default()
    };
    add_paths(&mut list, &[d.0.join("music"), notes.clone()], &Limits::default()).unwrap();
    // A rating on a.mp3, and a listed file and folder that are not there.
    let a = (list.entries.iter_mut()).find(|e| e.path.ends_with("a.mp3")).unwrap();
    a.extra = vec!["5".into()];
    list.entries.push(Entry {
        path: d.path("offline/x.mp3"),
        size: Some(9),
        extra: vec![String::new()],
        ..Entry::default()
    });
    list.entries.push(Entry {
        path: d.path("drive"),
        is_dir: true,
        extra: vec![String::new()],
        ..Entry::default()
    });
    list.entries.push(Entry {
        path: d.path("drive/y.mp3"),
        size: Some(7),
        extra: vec![String::new()],
        ..Entry::default()
    });
    std::fs::remove_file(d.0.join("music/gone.mp3")).unwrap();
    d.file("music/new/c.mp3", b"12");
    std::fs::write(&notes, b"12345").unwrap();
    let (fresh, stats) = update(&list, &Limits::default()).unwrap();
    assert_eq!(
        stats,
        Updated {
            folders: 1,
            missing: 2
        }
    );
    assert_eq!(
        rel(&d, &fresh),
        [
            "music/",
            "music/a.mp3",
            "music/new/",
            "music/new/c.mp3",
            "notes.txt",
            "offline/x.mp3",
            "drive/",
            "drive/y.mp3",
        ]
    );
    assert_eq!(fresh.entries[1].extra, ["5"], "a.mp3 keeps its rating");
    assert_eq!(fresh.entries[2].extra, [""]);
    assert_eq!(fresh.entries[4].size, Some(5), "notes.txt read again");
    assert_eq!(fresh.entries[5].size, Some(9), "kept as it was");
    let small = Limits {
        max_rows: 3,
        ..Limits::default()
    };
    assert!(update(&list, &small).is_err());
}

fn settings(lists: &[(&str, bool)]) -> IndexSettings {
    IndexSettings {
        file_lists: (lists.iter())
            .map(|(path, monitor)| FileListRoot {
                path: (*path).to_owned(),
                monitor: *monitor,
            })
            .collect(),
        ..IndexSettings::default()
    }
}

#[test]
fn file_lists_in_the_index_are_read_and_reloaded() {
    let d = TempDir::new("index");
    let one = d.file("one.efu", b"Filename,Size\n/m/alpha.mp3,1\n/m/beta.mp3,2\n");
    let two = d.file("two.txt", b"/t/alpha.txt\n");
    let (one_s, two_s) = (one.to_string_lossy().into_owned(), two.to_string_lossy().into_owned());
    let bad = d.path("missing.efu");
    let lists = FileListIndexes::new(
        &settings(&[(one_s.as_str(), true), (two_s.as_str(), false), (bad.as_str(), true)]),
        Limits::default(),
    );
    assert!(!lists.available(), "nothing read before a refresh");
    lists.refresh();
    assert!(lists.available() && lists.take_ready() && !lists.take_ready());
    assert_eq!(names(&lists, "alpha"), ["alpha.mp3", "alpha.txt"]);
    assert_eq!(names(&lists, "filelist-filename:two.txt"), ["alpha.txt"]);
    assert!(lists.status().unwrap().contains("missing.efu"));
    assert!(!lists.covers(Path::new("/m")));
    // A monitored list is read again when its file changes; one that is not, is not.
    std::fs::write(&one, b"Filename\n/m/gamma.mp3\n").unwrap();
    std::fs::write(&two, b"/t/gamma.txt\n/t/delta.txt\n").unwrap();
    lists.refresh();
    assert!(lists.take_ready());
    assert_eq!(names(&lists, "gamma"), ["gamma.mp3"]);
    lists.refresh();
    assert!(!lists.take_ready(), "nothing changed");
    // The missing list appears.
    std::fs::write(&bad, b"Filename\n/b/gamma.flac\n").unwrap();
    lists.refresh();
    assert_eq!(names(&lists, "gamma"), ["gamma.flac", "gamma.mp3"]);
    assert_eq!(lists.status(), None);
    // Settings change: a list removed is gone; turning monitoring on re-reads it.
    lists.set_index_settings(&settings(&[(two_s.as_str(), true)]));
    assert_eq!(names(&lists, "gamma"), ["gamma.txt"]);
    assert_eq!(lists.counts(), [(two_s.clone(), 2)]);
    lists.set_index_settings(&settings(&[]));
    assert!(!lists.available());
}

#[test]
fn file_lists_are_added_to_any_backend() {
    let d = TempDir::new("with");
    let one = d.file("one.efu", b"Filename\n/m/alpha.mp3\n");
    let one_s = one.to_string_lossy().into_owned();
    let lists = FileListIndexes::new(&settings(&[(one_s.as_str(), true)]), Limits::default());
    lists.refresh();
    let with = WithFileLists::new(Box::new(Unavailable::new("down")), lists);
    // The backend is down: the lists answer, and the state says so.
    assert_eq!(names(&with, "alpha"), ["alpha.mp3"]);
    assert_eq!(with.state(), crate::SearchState::Ready);
    assert!(with.query(&Query {
        text: "missing".into(),
        ..Query::default()
    })
    .is_err());
    // The backend's name, and what it covers.
    assert_eq!(with.name(), "Search");
    assert!(with.covers(Path::new("/m")));
    // Settings reach the lists too.
    with.set_index_settings(&settings(&[]));
    assert!(with.query(&Query::default()).is_err());
}
