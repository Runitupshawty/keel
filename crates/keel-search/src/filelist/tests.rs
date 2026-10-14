use super::*;
use crate::{Query, Searcher};

/// 2024-03-05T14:07:09Z.
const T: u64 = 133_541_212_290_000_000;

fn efu(text: &str) -> FileList {
    read(text.as_bytes(), ListKind::Efu, &Limits::default()).unwrap()
}

fn exported(list: &FileList, format: Format, header: bool, bom: bool) -> String {
    let mut out = Vec::new();
    let opts = ExportOptions {
        format,
        header,
        bom,
    };
    export(list, &opts, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn sample() -> FileList {
    FileList {
        columns: vec!["Title".into()],
        entries: vec![
            Entry {
                path: r"C:\Music".into(),
                is_dir: true,
                attributes: Some(ATTR_DIRECTORY),
                modified: Some(T),
                extra: vec![String::new()],
                ..Entry::default()
            },
            Entry {
                path: r#"C:\Music\Song, "live".mp3"#.into(),
                size: Some(4096),
                modified: Some(T),
                created: Some(T - 10_000_000),
                attributes: Some(0x20),
                extra: vec!["Song, live".into()],
                ..Entry::default()
            },
            Entry {
                path: "/home/user name/notes.txt".into(),
                size: Some(0),
                extra: vec![String::new()],
                ..Entry::default()
            },
        ],
    }
}

#[test]
fn reads_everything_efu() {
    let list = efu(concat!(
        "Filename,Size,Date Modified,Date Created,Attributes\r\n",
        "\"C:\\Windows\",,133541212290000000,133541212290000000,16\r\n",
        "\"C:\\Windows\\notepad.exe\",201216,2024-03-05T14:07:09Z,,32\r\n",
        "\"C:\\a \"\"quoted\"\" name.txt\",7,,,\r\n",
        "\"D:\\folder\\\",,,,\r\n",
        "\r\n",
    ));
    assert!(list.columns.is_empty());
    let e = &list.entries;
    assert_eq!(e.len(), 4);
    assert_eq!(e[0].path, r"C:\Windows");
    assert!(e[0].is_dir);
    assert_eq!((e[0].modified, e[0].created), (Some(T), Some(T)));
    assert_eq!(e[1].name(), "notepad.exe");
    assert_eq!(
        (e[1].size, e[1].modified, e[1].attributes),
        (Some(201_216), Some(T), Some(32))
    );
    assert!(!e[1].is_dir);
    assert_eq!(e[2].path, r#"C:\a "quoted" name.txt"#);
    assert_eq!(e[2].created, None);
    // A trailing separator marks a folder (and goes).
    assert_eq!(e[3].path, r"D:\folder");
    assert!(e[3].is_dir);
}

#[test]
fn efu_columns_in_any_order_and_extra_properties() {
    let list = efu("\u{feff}size,ATTRIBUTES,Length,filename,Album\n12,0x10,3:01,/a/b,\"X, Y\"\n");
    assert_eq!(list.columns, ["Length", "Album"]);
    let e = &list.entries[0];
    assert_eq!(
        (e.path.as_str(), e.size, e.is_dir),
        ("/a/b", Some(12), true)
    );
    assert_eq!(e.extra, ["3:01", "X, Y"]);
    // A short row leaves the rest empty.
    let short = efu("Filename,Size,Comment\n/x\n");
    assert_eq!(short.entries[0].extra, [""]);
    assert_eq!(short.entries[0].size, None);
}

#[test]
fn efu_needs_a_filename_column() {
    let err = read(&b"Path,Size\n/a,1\n"[..], ListKind::Efu, &Limits::default()).unwrap_err();
    assert!(err.to_string().contains("Filename"), "{err}");
    assert!(read(&b""[..], ListKind::Efu, &Limits::default()).is_err());
}

#[test]
fn txt_csv_and_m3u() {
    let txt = read(
        &b"\xEF\xBB\xBFC:\\one.txt\r\n\r\n  \"C:\\two words.txt\"  \n/var/\n#not a comment\n"[..],
        ListKind::Txt,
        &Limits::default(),
    )
    .unwrap();
    let paths: Vec<&str> = txt.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        [r"C:\one.txt", r"C:\two words.txt", "/var", "#not a comment"]
    );
    assert!(txt.entries[2].is_dir);

    let m3u = read(
        &b"#EXTM3U\n#EXTINF:123,Artist - Title\n/music/a.mp3\n"[..],
        ListKind::M3u,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(m3u.entries.len(), 1);

    // Everything's CSV export: Name and Path (the folder) columns.
    let csv = read(
        &b"Name,Path,Size,Date Modified\r\n\"a.txt\",\"C:\\dir\",5,2024-03-05T14:07:09\r\n"[..],
        ListKind::Csv,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(csv.entries[0].path, r"C:\dir\a.txt");
    assert_eq!(csv.entries[0].modified, Some(T));
    // No header it knows: the first column of every row is the path.
    let bare = read(&b"/a/b.txt,1\n/c\n"[..], ListKind::Csv, &Limits::default()).unwrap();
    assert_eq!(bare.entries.len(), 2);
    assert_eq!(bare.entries[0].path, "/a/b.txt");
    let tsv = read(
        &b"Full Path\tSize\n/x y/z\t9\n"[..],
        ListKind::Tsv,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        (tsv.entries[0].path.as_str(), tsv.entries[0].size),
        ("/x y/z", Some(9))
    );
}

#[test]
fn reading_is_bounded() {
    let limits = Limits {
        max_rows: 2,
        max_line: 32,
    };
    let three = "Filename\n/a\n/b\n/c\n";
    let err = read(three.as_bytes(), ListKind::Efu, &limits).unwrap_err();
    assert!(err.to_string().contains("more than 2 entries"), "{err}");
    assert!(read(&b"/a\n/b\n/c\n"[..], ListKind::Txt, &limits).is_err());
    assert!(read(&b"/a\n/b\n"[..], ListKind::Txt, &limits).is_ok());
    let long = format!("Filename\n/{}\n", "x".repeat(40));
    let err = read(long.as_bytes(), ListKind::Efu, &limits).unwrap_err();
    assert!(err.to_string().contains("longer than 32"), "{err}");
    // A quote that never closes cannot swallow the rest of the file.
    let open = format!("Filename\n\"/a\n{}", "/b\n".repeat(20));
    assert!(read(open.as_bytes(), ListKind::Efu, &limits).is_err());
}

#[test]
fn efu_round_trip() {
    let list = sample();
    let text = exported(&list, Format::Efu, true, false);
    assert!(text.starts_with("Filename,Size,Date Modified,Date Created,Attributes,Title\r\n"));
    assert!(
        text.contains(r#""C:\Music\Song, ""live"".mp3",4096,133541212290000000,133541212280000000,32,"Song, live""#),
        "{text}"
    );
    assert!(
        text.contains("\"C:\\Music\",,133541212290000000,,16,\r\n"),
        "{text}"
    );
    let back = efu(&text);
    assert_eq!(back, list);
    // With a BOM, and the header option cannot drop EFU's header.
    let bom = exported(&list, Format::Efu, false, true);
    assert!(bom.starts_with("\u{feff}Filename,"));
    assert_eq!(efu(&bom), list);
}

#[test]
fn csv_and_tsv_use_iso_dates() {
    let list = sample();
    let csv = exported(&list, Format::Csv, true, false);
    assert!(
        csv.contains(",4096,2024-03-05T14:07:09Z,2024-03-05T14:07:08Z,32,"),
        "{csv}"
    );
    let back = read(csv.as_bytes(), ListKind::Csv, &Limits::default()).unwrap();
    assert_eq!(back, list, "CSV reads back the same");
    let headless = exported(&list, Format::Csv, false, false);
    assert!(headless.starts_with("\"C:\\Music\","), "{headless}");

    let tsv = exported(&list, Format::Tsv, true, false);
    let lines: Vec<&str> = tsv.split("\r\n").collect();
    assert_eq!(
        lines[0],
        "Filename\tSize\tDate Modified\tDate Created\tAttributes\tTitle"
    );
    assert_eq!(
        lines[2],
        "\"C:\\Music\\Song, \"\"live\"\".mp3\"\t4096\t2024-03-05T14:07:09Z\t2024-03-05T14:07:08Z\t32\tSong, live"
    );
    assert_eq!(lines[3], "/home/user name/notes.txt\t0\t\t\t\t");
    let back = read(tsv.as_bytes(), ListKind::Tsv, &Limits::default()).unwrap();
    assert_eq!(back, list, "TSV reads back the same");
}

#[test]
fn txt_m3u_and_json() {
    let list = sample();
    let eol = if cfg!(windows) { "\r\n" } else { "\n" };
    let txt = exported(&list, Format::Txt, true, false);
    assert_eq!(
        txt,
        [
            r"C:\Music",
            r#"C:\Music\Song, "live".mp3"#,
            "/home/user name/notes.txt",
            ""
        ]
        .join(eol)
    );
    let m3u = exported(&list, Format::M3u8, true, true);
    assert!(m3u.starts_with(&format!("\u{feff}#EXTM3U{eol}C:\\Music{eol}")));
    let back = read(m3u.as_bytes(), ListKind::M3u, &Limits::default()).unwrap();
    assert_eq!(back.entries.len(), 3);
    assert!(!exported(&list, Format::M3u, false, false).contains("#EXTM3U"));

    let json = exported(&list, Format::Json, true, false);
    let expected = concat!(
        "[\n",
        r#"{"filename":"C:\\Music","date_modified":"2024-03-05T14:07:09Z","attributes":16},"#,
        "\n",
        r#"{"filename":"C:\\Music\\Song, \"live\".mp3","size":4096,"date_modified":"2024-03-05T14:07:09Z","date_created":"2024-03-05T14:07:08Z","attributes":32,"Title":"Song, live"},"#,
        "\n",
        r#"{"filename":"/home/user name/notes.txt","size":0}"#,
        "\n]\n"
    );
    assert_eq!(json, expected);
    let empty = exported(&FileList::default(), Format::Json, true, false);
    assert_eq!(empty, "[\n]\n");
}

#[test]
fn format_names() {
    for f in Format::ALL {
        assert_eq!(Format::parse(f.extension()), Some(f));
    }
    assert_eq!(Format::parse(".EFU"), Some(Format::Efu));
    assert_eq!(Format::parse("xls"), None);
    assert_eq!(
        ListKind::from_path(Path::new("a/list.M3U8")),
        Some(ListKind::M3u)
    );
    assert_eq!(ListKind::from_path(Path::new("a/list")), None);
}

fn names(s: &FileListSearcher, text: &str) -> Vec<String> {
    let q = Query {
        text: text.into(),
        ..Query::default()
    };
    s.query(&q)
        .unwrap()
        .iter()
        .map(|h| h.path.name().to_owned())
        .collect()
}

#[test]
fn searching_a_list() {
    let text = concat!(
        "Filename,Size,Date Modified,Attributes\n",
        "/data/Reports,,,16\n",
        "/data/Reports/report.pdf,10,2024-03-05T14:07:09Z,32\n",
        "/data/Reports/annual report.PDF,20,,32\n",
        "/data/report,30,,32\n",
        "/other/notes.txt,40,,32\n",
    );
    let s = FileListSearcher::new("test.efu", efu(text));
    assert_eq!(s.len(), 5);
    // Exact name, then prefix, then the rest; by path within each.
    assert_eq!(
        names(&s, "report"),
        ["report", "Reports", "report.pdf", "annual report.PDF"]
    );
    assert_eq!(names(&s, "*.pdf"), ["annual report.PDF", "report.pdf"]);
    assert_eq!(names(&s, "regex:^rep.*s$"), ["Reports"]);
    assert_eq!(names(&s, "folder:"), ["Reports"]);
    assert_eq!(names(&s, "in:/other"), ["notes.txt"]);
    assert_eq!(names(&s, "annual pdf"), ["annual report.PDF"]);
    assert!(names(&s, "missing").is_empty());
    // The shared search language: operators, and the list's sizes, dates and attributes.
    assert_eq!(
        names(&s, "ext:txt | ext:pdf size:>15"),
        ["annual report.PDF", "notes.txt"]
    );
    assert_eq!(
        names(&s, "file: size:<=20"),
        ["annual report.PDF", "report.pdf"]
    );
    assert_eq!(names(&s, "dm:2024"), ["report.pdf"]);
    assert_eq!(names(&s, "attrib:D"), ["Reports"]);
    assert_eq!(names(&s, "<report | notes> !pdf"), ["report", "Reports", "notes.txt"]);
    assert!(s
        .query(&Query {
            text: "<report".into(),
            ..Query::default()
        })
        .unwrap_err()
        .to_string()
        .ends_with("(at 0)"));
    // An empty query lists everything.
    assert_eq!(names(&s, "").len(), 5);
    let case = Query {
        text: "PDF".into(),
        match_case: true,
        ..Query::default()
    };
    assert_eq!(s.query(&case).unwrap().len(), 1);
    // The list's own sizes and dates; nothing is read from disk.
    let hits = s
        .query(&Query {
            text: "report.pdf".into(),
            ..Query::default()
        })
        .unwrap();
    assert_eq!(hits[0].size, 10);
    assert_eq!(hits[0].modified, system_from_filetime(T));
    // `max`, and a cap that overrides it.
    let one = Query {
        max: 1,
        ..Query::default()
    };
    assert_eq!(s.query(&one).unwrap().len(), 1);
    let capped = FileListSearcher::new("x", efu(text)).with_cap(3);
    assert_eq!(capped.query(&one).unwrap().len(), 3);
    assert!(s.status().unwrap().contains("5 entries"));
    assert!(s
        .query(&Query {
            text: "regex:(".into(),
            ..Query::default()
        })
        .is_err());
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("keel-filelist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, rel: &str, body: &[u8]) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tree(tag: &str) -> TempDir {
    let d = TempDir::new(tag);
    d.file("a.mp3", b"123");
    d.file("b.txt", b"1");
    d.file("music/c.mp3", b"12345");
    d.file("music/live/d.flac", b"1");
    d.file("node_modules/pkg/e.mp3", b"1");
    d.file("docs/f.txt", b"1");
    d
}

fn created(d: &TempDir, opts: &CreateOptions) -> Vec<String> {
    let list = create(&d.0, opts, &Limits::default()).unwrap();
    (list.entries.iter())
        .map(|e| {
            let rel = Path::new(&e.path).strip_prefix(&d.0).unwrap();
            let rel = rel.to_string_lossy().replace('\\', "/");
            if e.is_dir {
                format!("{rel}/")
            } else {
                rel
            }
        })
        .collect()
}

#[test]
fn create_walks_in_path_order_with_metadata() {
    let d = tree("walk");
    let all = created(&d, &CreateOptions::default());
    assert_eq!(
        all,
        [
            "a.mp3",
            "b.txt",
            "docs/",
            "docs/f.txt",
            "music/",
            "music/c.mp3",
            "music/live/",
            "music/live/d.flac",
            "node_modules/",
            "node_modules/pkg/",
            "node_modules/pkg/e.mp3",
        ]
    );
    let list = create(&d.0, &CreateOptions::default(), &Limits::default()).unwrap();
    let mp3 = &list.entries[0];
    assert_eq!(mp3.size, Some(3));
    assert!(mp3.modified.is_some());
    assert_eq!(mp3.attributes.map(|a| a & ATTR_DIRECTORY), Some(0));
    let docs = &list.entries[2];
    assert_eq!(
        (docs.size, docs.attributes.map(|a| a & ATTR_DIRECTORY)),
        (None, Some(ATTR_DIRECTORY))
    );
    // Made into an EFU and read back: the same list.
    let mut out = Vec::new();
    write_efu(&list, &mut out).unwrap();
    assert_eq!(
        read(&out[..], ListKind::Efu, &Limits::default()).unwrap(),
        list
    );
    // Bounded too.
    let small = Limits {
        max_rows: 3,
        ..Limits::default()
    };
    assert!(create(&d.0, &CreateOptions::default(), &small).is_err());
    assert!(create(
        &d.0.join("a.mp3"),
        &CreateOptions::default(),
        &Limits::default()
    )
    .is_err());
}

#[test]
fn create_filters() {
    let d = tree("filters");
    let opts = |f: fn(&mut CreateOptions)| {
        let mut o = CreateOptions::default();
        f(&mut o);
        created(&d, &o)
    };
    assert_eq!(
        opts(|o| {
            o.include_files = split_wildcards("*.MP3; *.flac");
            o.no_folders = true;
        }),
        [
            "a.mp3",
            "music/c.mp3",
            "music/live/d.flac",
            "node_modules/pkg/e.mp3"
        ]
    );
    assert_eq!(
        opts(|o| {
            o.include_files = split_wildcards("*.mp3");
            o.exclude_folders = split_wildcards("node_modules");
            o.no_folders = true;
        }),
        ["a.mp3", "music/c.mp3"]
    );
    assert_eq!(
        opts(|o| o.exclude_files = split_wildcards("*.mp3,*.flac")),
        [
            "b.txt",
            "docs/",
            "docs/f.txt",
            "music/",
            "music/live/",
            "node_modules/",
            "node_modules/pkg/"
        ]
    );
    assert_eq!(
        opts(|o| o.no_subfolders = true),
        ["a.mp3", "b.txt", "docs/", "music/", "node_modules/"]
    );
    assert_eq!(
        opts(|o| o.no_files = true),
        [
            "docs/",
            "music/",
            "music/live/",
            "node_modules/",
            "node_modules/pkg/"
        ]
    );
    // Only what is inside a matching folder; others are walked to find them.
    assert_eq!(
        opts(|o| o.include_folders = split_wildcards("live;pkg")),
        [
            "music/live/",
            "music/live/d.flac",
            "node_modules/pkg/",
            "node_modules/pkg/e.mp3"
        ]
    );
    // A wildcard with a separator matches the path below the root.
    assert_eq!(
        opts(|o| {
            o.exclude_folders = split_wildcards(r"music\live");
            o.no_folders = true;
            o.include_files = split_wildcards("*.mp3;*.flac");
        }),
        ["a.mp3", "music/c.mp3", "node_modules/pkg/e.mp3"]
    );
}

#[test]
fn read_file_by_extension_and_sniffing() {
    let d = TempDir::new("read");
    d.file("list.efu", b"Filename,Size\n/a,1\n");
    d.file("list.txt", b"/a\n/b\n");
    d.file("noext", b"\"Filename\",\"Size\"\n/a,1\n/b,2\n");
    d.file("other.lst", b"Filename,Size\n");
    let limits = Limits::default();
    assert_eq!(
        read_file(&d.0.join("list.efu"), &limits)
            .unwrap()
            .entries
            .len(),
        1
    );
    assert_eq!(
        read_file(&d.0.join("list.txt"), &limits)
            .unwrap()
            .entries
            .len(),
        2
    );
    assert_eq!(
        read_file(&d.0.join("noext"), &limits)
            .unwrap()
            .entries
            .len(),
        2
    );
    assert!(read_file(&d.0.join("other.lst"), &limits)
        .unwrap()
        .entries
        .is_empty());
    assert!(read_file(&d.0.join("missing.efu"), &limits).is_err());
    assert!(read_file(&d.0, &limits).is_err(), "a folder is not a list");
}
