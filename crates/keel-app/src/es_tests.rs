use super::*;
use keel_search::{Hit, Query};
use std::sync::Mutex;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn p(list: &[&str]) -> EsArgs {
    parse(&args(list)).unwrap_or_else(|e| panic!("{list:?}: {e}"))
}

fn err(list: &[&str]) -> String {
    parse(&args(list)).expect_err(&format!("{list:?} should fail"))
}

/// A made-up absolute folder (never touched).
fn root() -> PathBuf {
    std::env::temp_dir().join("keel-es-root")
}

const T: u64 = 1_700_000_000;

fn row(rel: &str, is_dir: bool, size: u64, secs: u64) -> Row {
    let path = root().join(rel);
    Row {
        name: path.file_name().unwrap().to_string_lossy().into_owned(),
        path: path.display().to_string(),
        is_dir,
        size: (!is_dir).then_some(size),
        modified: Some(UNIX_EPOCH + Duration::from_secs(secs)),
        meta: true,
    }
}

fn rows() -> Vec<Row> {
    vec![
        row("a/Report.pdf", false, 300, T + 30),
        row("a/b/report-old.pdf", false, 100, T + 10),
        row("c/reporting.txt", false, 2048, T + 20),
        row("a", true, 0, T),
        row("c/résumé.doc", false, 5, T + 5),
        row("c/resume.doc", false, 7, T + 7),
    ]
}

fn names(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|r| r.name.as_str()).collect()
}

fn filtered(list: &[&str]) -> Vec<String> {
    let a = p(list);
    let mut r = rows();
    filter(&mut r, &a).unwrap();
    let (key, asc) = a.sort_order();
    sort(&mut r, key, asc);
    names(&r).into_iter().map(String::from).collect()
}

fn printed(rows: Vec<Row>, list: &[&str]) -> String {
    let a = p(list);
    let mut out = Vec::new();
    output(rows, &a, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn parses_match_switches_in_any_spelling() {
    let a = p(&["-i", "/W", "--match-path", "-DIACRITICS", "two words", "x"]);
    assert!(a.case && a.whole_word && a.match_path && a.diacritics);
    assert_eq!(a.terms, ["two words", "x"]);
    assert_eq!(a.text(), "\"two words\" x");
    let a = p(&[
        "-case",
        "-no-case",
        "-ww",
        "-no-whole-word",
        "-p",
        "-no-match-path",
    ]);
    assert!(!a.case && !a.whole_word && !a.match_path);
    let a = p(&["-r", "^ab+c$", "-regex", "d e"]);
    assert!(a.regex);
    assert_eq!(a.text(), "^ab+c$ d e", "a regex is not quoted");
    let a = p(&["-prefix", "rep"]);
    assert!(a.prefix && !a.suffix);
    // After `--` everything is the search.
    assert_eq!(p(&["--", "-n", "/ad"]).terms, ["-n", "/ad"]);
    assert_eq!(p(&["-"]).terms, ["-"]);
    // Accepted and ignored.
    assert_eq!(p(&["-ipc1", "-IPC2", "/ipc3", "x"]), p(&["x"]));
}

#[test]
fn parses_paging_scope_and_sort() {
    let a = p(&[
        "-n",
        "5",
        "-o",
        "10",
        "-path",
        "a b",
        "-parent",
        "c",
        "-parent-path",
        "d",
    ]);
    assert_eq!((a.max, a.offset), (Some(5), 10));
    assert_eq!(a.path.as_deref(), Some(Path::new("a b")));
    assert_eq!(a.parent.as_deref(), Some(Path::new("c")));
    assert_eq!(a.parent_path.as_deref(), Some(Path::new("d")));
    let a = p(&["-max-results", "3", "-offset", "1"]);
    assert_eq!((a.max, a.offset), (Some(3), 1));
    for (list, key, asc) in [
        (&["-sort", "size"][..], SortKey::Size, None),
        (
            &["-sort", "Date-Modified-Descending"],
            SortKey::DateModified,
            Some(false),
        ),
        (&["-sort", "date_modified"], SortKey::DateModified, None),
        (
            &["-sort", "ext", "-sort-descending"],
            SortKey::Extension,
            Some(false),
        ),
        (
            &["-sort-descending", "-sort", "path-ascending"],
            SortKey::Path,
            Some(true),
        ),
        (&["/on"], SortKey::Name, Some(true)),
        (&["/O-S"], SortKey::Size, Some(false)),
        (&["/oe"], SortKey::Extension, Some(true)),
        (&["/o-d"], SortKey::DateModified, Some(false)),
    ] {
        let a = p(list);
        assert_eq!((a.sort, a.ascending), (Some(key), asc), "{list:?}");
    }
    assert_eq!(p(&["/ad"]).only, Some(Only::Folders));
    assert_eq!(p(&["/A-D"]).only, Some(Only::Files));
    assert_eq!(p(&["/a"]).only, None);
}

#[test]
fn parses_columns_formats_and_reports() {
    let a = p(&[
        "-size",
        "-name",
        "-dm",
        "-size",
        "-path-column",
        "-ext",
        "-full-path-and-name",
    ]);
    assert_eq!(
        a.columns,
        [
            Column::Size,
            Column::Name,
            Column::DateModified,
            Column::Path,
            Column::Extension,
            Column::FullPath
        ]
    );
    let a = p(&["-size-format", "3", "-date-format", "2"]);
    assert_eq!((a.size_format, a.date_format), (Some(3), Some(2)));
    for (sw, f) in [
        ("-csv", Format::Csv),
        ("-efu", Format::Efu),
        ("-json", Format::Json),
        ("-m3u", Format::M3u),
        ("-m3u8", Format::M3u8),
        ("-tsv", Format::Tsv),
        ("-txt", Format::Txt),
    ] {
        assert_eq!(p(&[sw]).format, f);
        let a = p(&[&format!("-export{sw}"), "out.file"]);
        assert_eq!(
            (a.format, a.export.as_deref()),
            (f, Some(Path::new("out.file")))
        );
    }
    let a = p(&[
        "-no-header",
        "-utf8-bom",
        "-no-result-error",
        "-timeout",
        "250",
        "-library",
    ]);
    assert!(a.no_header && a.utf8_bom && a.no_result_error && a.library);
    assert_eq!(a.timeout, Some(Duration::from_millis(250)));
    assert_eq!(p(&["-count"]).report, Some(Report::Count));
    assert_eq!(p(&["-get-result-count"]).report, Some(Report::Count));
    assert_eq!(p(&["-get-total-size"]).report, Some(Report::TotalSize));
    assert_eq!(
        p(&["-get-folder-size", "dir"]).report,
        Some(Report::FolderSize("dir".into()))
    );
    assert!(p(&["-h"]).help && p(&["/?"]).help && p(&["-help"]).help);
}

#[test]
fn refuses_bad_switches() {
    assert!(err(&["-bogus"]).contains("unknown switch -bogus"));
    assert!(err(&["-n"]).contains("-n needs a number"));
    assert!(err(&["-n", "lots"]).contains("needs a number"));
    assert!(err(&["-path"]).contains("needs a folder"));
    assert!(err(&["-export-csv"]).contains("needs a file"));
    assert!(err(&["-sort", "run-count"]).contains("not supported"));
    assert!(err(&["-size-format", "4"]).contains("0, 1, 2 or 3"));
    assert!(err(&["/ah"]).contains("only /ad and /a-d"));
    assert!(err(&["/oc"]).contains("sort by n"));
    assert!(err(&["-r", "x", "-w"]).contains("-regex cannot"));
    assert!(err(&["-prefix", "a", "b"]).contains("one search word"));
    assert!(err(&["-suffix"]).contains("one search word"));
    assert!(err(&["-library", "-r", "x"]).contains("library"));
    // A slash word that is not a switch is searched for (a Unix path).
    assert_eq!(p(&["/home/me", "/opt"]).terms, ["/home/me", "/opt"]);
}

#[test]
fn backend_text_scopes_and_matches_paths() {
    let a = p(&["report"]);
    assert_eq!(a.backend_text(false).unwrap(), "report");
    let dir = root();
    let a = p(&["-path", &dir.display().to_string(), "two words"]);
    assert_eq!(
        a.backend_text(false).unwrap(),
        format!("in:\"{}\" \"two words\"", dir.display())
    );
    // -parent-path searches the folder that holds the path.
    let a = p(&["-parent-path", &dir.join("x").display().to_string(), "q"]);
    assert_eq!(
        a.backend_text(false).unwrap(),
        format!("in:\"{}\" q", dir.display())
    );
    // -p needs Everything, where each term gets path:.
    let a = p(&["-p", "docs", "!old", "\"a b\""]);
    assert!(a
        .backend_text(false)
        .unwrap_err()
        .contains("needs Everything"));
    assert_eq!(
        a.backend_text(true).unwrap(),
        "path:docs !path:old path:\"a b\""
    );
    assert!(p(&["-p", "-r", "x"]).backend_text(true).is_err());
    // A regex is the whole text: the folder is filtered afterwards instead.
    let a = p(&["-path", "x", "-r", "^a"]);
    assert_eq!(a.backend_text(false).unwrap(), "^a");
}

#[test]
fn filters_by_scope_kind_and_words() {
    let a = root().join("a").display().to_string();
    assert_eq!(
        filtered(&["-path", &a]),
        ["report-old.pdf", "Report.pdf"],
        "under a, not a itself"
    );
    assert_eq!(filtered(&["-parent", &a]), ["Report.pdf"]);
    let in_a = root().join("a").join("x").display().to_string();
    assert_eq!(
        filtered(&["-parent-path", &in_a]),
        ["report-old.pdf", "Report.pdf"]
    );
    assert_eq!(filtered(&["/ad"]), ["a"]);
    assert_eq!(filtered(&["/a-d"]).len(), 5);
    assert_eq!(
        filtered(&["-w", "report"]),
        ["report-old.pdf", "Report.pdf"],
        "reporting is not the word report"
    );
    assert_eq!(filtered(&["-w", "-i", "Report"]), ["Report.pdf"]);
    assert_eq!(
        filtered(&["-prefix", "rep"]),
        ["report-old.pdf", "Report.pdf", "reporting.txt"]
    );
    assert_eq!(
        filtered(&["-suffix", ".PDF"]),
        ["report-old.pdf", "Report.pdf"]
    );
    assert_eq!(filtered(&["-diacritics", "résumé"]), ["résumé.doc"]);
    // Wildcards and functions are left to the backend.
    assert_eq!(filtered(&["-w", "*.pdf", "ext:pdf"]).len(), 6);
    // -p filters the full path.
    assert_eq!(filtered(&["-p", "-w", "b"]), ["report-old.pdf"]);
}

#[test]
fn sorts_like_everything() {
    assert_eq!(
        filtered(&[]),
        [
            "a",
            "report-old.pdf",
            "Report.pdf",
            "reporting.txt",
            "resume.doc",
            "résumé.doc"
        ]
    );
    assert_eq!(
        filtered(&["/a-d", "/o-s"]),
        [
            "reporting.txt",
            "Report.pdf",
            "report-old.pdf",
            "resume.doc",
            "résumé.doc"
        ]
    );
    assert_eq!(
        filtered(&["/a-d", "-sort", "date-modified"]),
        [
            "résumé.doc",
            "resume.doc",
            "report-old.pdf",
            "reporting.txt",
            "Report.pdf"
        ]
    );
    assert_eq!(
        filtered(&["/a-d", "/oe"]),
        [
            "resume.doc",
            "résumé.doc",
            "report-old.pdf",
            "Report.pdf",
            "reporting.txt"
        ]
    );
    assert_eq!(filtered(&["-sort", "path"])[0], "a");
}

#[test]
fn prints_paths_and_columns() {
    let mut r = rows();
    sort(&mut r, SortKey::Name, true);
    let text = printed(r.clone(), &["-n", "2"]);
    let want: Vec<String> = r[..2].iter().map(|r| r.path.clone()).collect();
    assert_eq!(text, format!("{}\n{}\n", want[0], want[1]));
    // Offset past the end prints nothing.
    assert_eq!(printed(r.clone(), &["-o", "99"]), "");
    let text = printed(r.clone(), &["-o", "3", "-n", "1", "-name", "-size", "-ext"]);
    assert_eq!(text, "reporting.txt  2048  txt\n");
    let text = printed(
        r.clone(),
        &["-o", "3", "-n", "1", "-size", "-size-format", "2"],
    );
    assert_eq!(text, "2 KB\n");
    let text = printed(r.clone(), &["-n", "1", "-size", "-name", "-path-column"]);
    assert_eq!(
        text,
        format!("  a  {}\n", root().display()),
        "folders have no size"
    );
    let text = printed(
        r.clone(),
        &["-o", "3", "-n", "1", "-dm", "-date-format", "3"],
    );
    assert_eq!(text, "2023-11-14T22:13:40Z\n");
    let text = printed(r, &["-o", "3", "-n", "1", "-dm", "-date-format", "2"]);
    assert_eq!(text, "133444736200000000\n");
}

#[test]
fn exports_csv_tsv_efu_json_and_m3u() {
    let two = vec![
        row("a/Report \"Q1\".pdf", false, 300, T),
        row("a", true, 0, T),
    ];
    let full = |i: usize| two[i].path.clone();
    let csv = printed(two.clone(), &["-csv", "-name", "-size", "-dm"]);
    assert_eq!(
        csv,
        "\"Name\",\"Size\",\"Date Modified\"\n\
         \"Report \"\"Q1\"\".pdf\",300,133444736000000000\n\
         \"a\",,133444736000000000\n"
    );
    let csv = printed(two.clone(), &["-csv", "-no-header"]);
    assert_eq!(
        csv,
        format!("\"{}\"\n\"{}\"\n", full(0).replace('"', "\"\""), full(1))
    );
    let tsv = printed(two.clone(), &["-tsv", "-name", "-ext"]);
    assert_eq!(tsv, "Name\tExtension\nReport \"Q1\".pdf\tpdf\na\t\n");
    let efu = printed(two.clone(), &["-efu"]);
    assert_eq!(
        efu,
        format!(
            "Filename,Size,Date Modified,Attributes\n\"{}\",300,133444736000000000,0\n\"{}\",,133444736000000000,16\n",
            full(0).replace('"', "\"\""),
            full(1)
        )
    );
    let json = printed(two.clone(), &["-json", "-name", "-size"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        v,
        serde_json::json!([
            {"name": "Report \"Q1\".pdf", "size": 300},
            {"name": "a", "size": null}
        ])
    );
    let json = printed(two.clone(), &["-json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[1]["filename"], serde_json::json!(full(1)));
    let m3u = printed(two.clone(), &["-m3u8"]);
    assert_eq!(
        m3u,
        format!("#EXTM3U\n{}\n", full(0)),
        "folders are left out"
    );
    let bom = printed(two, &["-txt", "-utf8-bom", "-n", "0"]);
    assert_eq!(bom, "\u{feff}");
}

#[test]
fn reports_count_and_total_size() {
    assert_eq!(printed(rows(), &["-count", "-n", "1"]), "6\n");
    assert_eq!(printed(rows(), &["-get-total-size"]), "2460\n");
    assert_eq!(
        printed(rows(), &["-get-total-size", "-size-format", "2"]),
        "3 KB\n"
    );
}

#[test]
fn exports_to_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.csv");
    let a = p(&["-export-csv", &file.display().to_string(), "-name"]);
    let mut out = Vec::new();
    assert_eq!(output(rows(), &a, &mut out), Ok(6));
    assert!(out.is_empty(), "nothing on stdout");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("\"Name\"\n\"Report.pdf\"\n"), "{text}");
    // A folder that does not exist fails.
    let a = p(&[
        "-export-csv",
        &dir.path().join("no/such.csv").display().to_string(),
    ]);
    assert!(matches!(output(rows(), &a, &mut out), Err(Fail::Failed(_))));
}

/// A backend that answers with fixed hits and keeps the last query.
struct Fake {
    name: &'static str,
    hits: Vec<Hit>,
    seen: Mutex<Option<Query>>,
}

impl Searcher for Fake {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        *self.seen.lock().unwrap() = Some(q.clone());
        Ok(self.hits.clone())
    }
    fn available(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        self.name
    }
}

#[test]
fn searches_the_system_index_and_reads_sizes_it_lacks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("b.txt"), "12345").unwrap();
    std::fs::write(dir.path().join("a.txt"), "1").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let hit = |name: &str, is_dir| Hit {
        path: keel_vfs::VPath::local(dir.path().join(name)),
        is_dir,
        size: 0,
        modified: None,
        snippet: None,
    };
    let fake = Fake {
        name: "Keel index",
        hits: vec![hit("b.txt", false), hit("a.txt", false), hit("sub", true)],
        seen: Mutex::new(None),
    };
    let a = p(&[
        "-path",
        &dir.path().display().to_string(),
        "-i",
        "/a-d",
        "-sort",
        "size-descending",
        "txt",
    ]);
    let rows = search(&a, Source::System(&fake)).unwrap();
    assert_eq!(names(&rows), ["b.txt", "a.txt"]);
    assert_eq!(rows[0].size, Some(5), "read for the size sort");
    let q = fake.seen.lock().unwrap().clone().unwrap();
    assert_eq!(q.text, format!("in:\"{}\" txt", dir.path().display()));
    assert!(q.match_case && !q.regex && !q.folders_only && !q.meta);
    assert_eq!(q.max, FETCH_CAP);
    // Sizes for a size column are read for the page only.
    let a = p(&["/ad", "txt"]);
    let rows = search(&a, Source::System(&fake)).unwrap();
    assert_eq!(names(&rows), ["sub"]);
    assert!(fake.seen.lock().unwrap().as_ref().unwrap().folders_only);
    let a = p(&["-name", "-size", "/a-d", "/os"]);
    let rows = search(&a, Source::System(&fake)).unwrap();
    let mut out = Vec::new();
    output(rows, &a, &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "a.txt  1\nb.txt  5\n");
    // -p without Everything is a usage error; with it, the query carries path:.
    assert!(matches!(
        search(&p(&["-p", "x"]), Source::System(&fake)),
        Err(Fail::Usage(_))
    ));
    let everything = Fake {
        name: "Everything",
        ..fake
    };
    search(&p(&["-p", "x"]), Source::System(&everything)).unwrap();
    assert_eq!(
        everything.seen.lock().unwrap().as_ref().unwrap().text,
        "path:x"
    );
}

#[test]
fn reports_a_folder_size_and_a_building_index() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("f"), "123").unwrap();
    std::fs::write(dir.path().join("g"), "45").unwrap();
    let a = p(&["-get-folder-size", &dir.path().display().to_string()]);
    let mut out = Vec::new();
    folder_size(dir.path(), &a, &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "5\n");

    struct Building;
    impl Searcher for Building {
        fn query(&self, _: &Query) -> anyhow::Result<Vec<Hit>> {
            unreachable!("not queried while building")
        }
        fn available(&self) -> bool {
            false
        }
        fn state(&self) -> SearchState {
            SearchState::Indexing { done: 42 }
        }
    }
    let a = p(&["-timeout", "0", "x"]);
    match search(&a, Source::System(&Building)) {
        Err(Fail::Failed(e)) => assert!(e.contains("42 entries"), "{e}"),
        other => panic!("{other:?}"),
    }
}
