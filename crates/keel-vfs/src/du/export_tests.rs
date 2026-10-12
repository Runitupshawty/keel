//! Exports and the report (Task 86): every format and list, on a hand-made scan and on
//! scans of temp fixture trees.

use super::export::{self, Format, List, Options};
use super::report;
use super::*;
use std::fs;

/// `/r` with `a,"b"` (two files, one named like a formula, one with a line break and
/// markup), `docs` (three files, one sparse) and free and unknown space.
const DOC: &str = r#"{"format":"keel-du-scan","version":1,"started":1700000000,"complete":true,
"roots":[{"node":1,"path":"/r","total":100000,"free":30000}],
"exts":["","txt","bin"],
"nodes":[
 ["",4294967295,0,0,0,0,0,0,0,0],
 ["/r",0,1,0,0,0,0,0,0,0],
 ["a,\"b\"",1,2,0,0,0,0,0,0,0],
 ["docs",1,2,0,0,0,0,0,0,0],
 ["<Free space>",1,5,0,0,0,0,0,0,0],
 ["<Unknown>",1,6,0,0,0,0,0,0,0],
 ["=SUM(A1).txt",2,3,0,0,1,0,5000,8192,1700000000000000000],
 ["line\nbreak <script>&'.bin",2,3,0,0,2,0,300,4096,-9223372036854775808],
 ["big.bin",3,3,0,0,2,0,10000,4096,0],
 ["sparse.txt",3,3,128,512,1,0,20000,0,0],
 ["noext",3,3,0,0,0,0,1,4096,0]
]}"#;

fn fixture() -> Tree {
    load_from(DOC.as_bytes()).unwrap()
}

fn out(tree: &Tree, list: List, format: Format, options: Options) -> String {
    let mut v = Vec::new();
    export::write(tree, list, format, options, &mut v).unwrap();
    String::from_utf8(v).unwrap()
}

fn json(tree: &Tree, list: List, options: Options) -> serde_json::Value {
    serde_json::from_str(&out(tree, list, Format::Json, options)).unwrap()
}

/// A strict RFC 4180 reader: every field quoted, CRLF records.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut chars = text.chars().peekable();
    while chars.peek().is_some() {
        let mut record = Vec::new();
        loop {
            assert_eq!(chars.next(), Some('"'), "every field is quoted");
            let mut field = String::new();
            loop {
                match chars.next().expect("unterminated field") {
                    '"' if chars.peek() == Some(&'"') => {
                        chars.next();
                        field.push('"');
                    }
                    '"' => break,
                    c => field.push(c),
                }
            }
            record.push(field);
            match chars.next() {
                Some(',') => continue,
                Some('\r') => {
                    assert_eq!(chars.next(), Some('\n'), "CRLF");
                    break;
                }
                other => panic!("after a field: {other:?}"),
            }
        }
        records.push(record);
    }
    records
}

#[test]
fn format_by_extension() {
    assert_eq!(Format::from_path(Path::new("a.csv")).unwrap(), Format::Csv);
    assert_eq!(
        Format::from_path(Path::new("x/a.JSON")).unwrap(),
        Format::Json
    );
    assert!(Format::from_path(Path::new("a.txt")).is_err());
    assert!(Format::from_path(Path::new("csv")).is_err());
}

#[test]
fn tree_csv_quotes_everything_and_keeps_the_structure() {
    let t = fixture();
    let text = out(&t, List::Tree, Format::Csv, Options::default());
    assert!(
        !text.starts_with('\u{feff}'),
        "no byte order mark unless asked"
    );
    assert!(text.starts_with("\"id\",\"parent\",\"path\",\"name\",\"kind\""));
    let rows = parse_csv(&text);
    assert_eq!(rows.len(), 1 + t.live());
    assert!(rows.iter().all(|r| r.len() == 15));
    let col = |name: &str| rows[0].iter().position(|h| h == name).unwrap();
    let (id, parent, name, kind) = (col("id"), col("parent"), col("name"), col("kind"));
    let by_name = |n: &str| rows.iter().find(|r| r[name] == n).unwrap();
    // Quotes, commas and line breaks survive; a formula-like name does not run.
    let a = by_name("a,\"b\"");
    assert_eq!(a[kind], "dir");
    assert_eq!(a[col("bytes")], "5300");
    assert_eq!(a[col("allocated")], "12288");
    assert_eq!(a[col("files")], "2");
    by_name("line\nbreak <script>&'.bin");
    let formula = by_name("'=SUM(A1).txt");
    assert_eq!(formula[col("extension")], "txt");
    assert_eq!(formula[col("modified")], "2023-11-14T22:13:20Z");
    assert!(
        formula[col("path")].starts_with("/r"),
        "a path starts with its root"
    );
    assert_eq!(formula[parent], a[id], "a file's parent is its folder's id");
    let sparse = by_name("sparse.txt");
    assert_eq!(sparse[col("notes")], "sparse");
    assert_eq!(sparse[col("attributes")], "512");
    // Parents come before their children; the top item has no parent.
    for (i, r) in rows.iter().enumerate().skip(1) {
        assert_eq!(r[id], (i - 1).to_string());
        if i == 1 {
            assert_eq!((r[kind].as_str(), r[parent].as_str()), ("top", ""));
        } else {
            assert!(r[parent].parse::<usize>().unwrap() < i - 1);
        }
    }
    assert_eq!(by_name("<Unknown>")[col("bytes")], "49520");

    let bom = Options {
        bom: true,
        ..Options::default()
    };
    let with_bom = out(&t, List::Tree, Format::Csv, bom);
    assert_eq!(with_bom.strip_prefix('\u{feff}'), Some(text.as_str()));
    // Never in JSON.
    assert!(out(&t, List::Types, Format::Json, bom).starts_with('['));
}

#[test]
fn tree_json_is_a_scan_file() {
    let t = fixture();
    let text = out(&t, List::Tree, Format::Json, Options::default());
    let back = load_from(text.as_bytes()).unwrap();
    assert_eq!(back.nodes, t.nodes);
    assert_eq!(back.roots, t.roots);
    assert_eq!(back.node(TOP).size, 35_301);
}

#[test]
fn file_types_aggregate_by_extension() {
    let t = fixture();
    let types = export::file_types(&t, false);
    let got: Vec<(&str, u64, u64, u64)> = (types.iter())
        .map(|t| (t.extension.as_str(), t.files, t.bytes, t.allocated))
        .collect();
    assert_eq!(
        got,
        [
            ("txt", 2, 25_000, 8192),
            ("bin", 2, 10_300, 8192),
            ("", 1, 1, 4096)
        ]
    );
    assert_eq!(types[0].percent, 70.82);
    let sum: f64 = types.iter().map(|t| t.percent).sum();
    assert!((sum - 100.0).abs() < 0.05, "{sum}");
    // By size on disk: a tie goes by extension.
    let disk: Vec<String> = (export::file_types(&t, true).into_iter())
        .map(|t| t.extension)
        .collect();
    assert_eq!(disk, ["bin", "txt", ""]);

    let rows = parse_csv(&out(&t, List::Types, Format::Csv, Options::default()));
    assert_eq!(
        rows[0],
        ["extension", "files", "bytes", "allocated", "percent"]
    );
    assert_eq!(rows[1], ["txt", "2", "25000", "8192", "70.82"]);
    assert_eq!(rows[3], ["", "1", "1", "4096", "0.00"]);

    let v = json(&t, List::Types, Options::default());
    assert_eq!(v.as_array().unwrap().len(), 3);
    assert_eq!(v[1]["extension"], "bin");
    assert_eq!(v[1]["bytes"], 10_300);
    assert_eq!(v[1]["files"], 2);
}

#[test]
fn largest_files_by_size_or_on_disk() {
    let t = fixture();
    let names = |rows: Vec<export::Row>| rows.into_iter().map(|r| r.name).collect::<Vec<_>>();
    let top3 = Options {
        top: 3,
        ..Options::default()
    };
    assert_eq!(
        names(export::largest(&t, top3)),
        ["sparse.txt", "big.bin", "=SUM(A1).txt"]
    );
    let disk = Options {
        top: 2,
        size_on_disk: true,
        ..Options::default()
    };
    assert_eq!(
        names(export::largest(&t, disk)),
        ["=SUM(A1).txt", "big.bin"]
    );
    let all = Options {
        top: 0,
        ..Options::default()
    };
    assert_eq!(export::largest(&t, all).len(), 5);

    let v = json(&t, List::Largest, top3);
    let v = v.as_array().unwrap();
    assert_eq!(v.len(), 3);
    assert_eq!(v[0]["name"], "sparse.txt");
    assert_eq!(v[0]["kind"], "file");
    assert_eq!(v[0]["bytes"], 20_000);
    assert_eq!(v[0]["allocated"], 0);
    assert_eq!(v[0]["notes"][0], "sparse");
    assert!(v[0]["path"].as_str().unwrap().ends_with("sparse.txt"));

    let rows = parse_csv(&out(&t, List::Largest, Format::Csv, top3));
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[1][3], "sparse.txt");
    assert_eq!(rows[3][3], "'=SUM(A1).txt");
}

#[test]
fn hard_link_names_counted_elsewhere_are_left_out() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("tree");
    fs::create_dir_all(root.join("x")).unwrap();
    fs::create_dir_all(root.join("y")).unwrap();
    fs::write(root.join("x/data.bin"), vec![1u8; 1000]).unwrap();
    fs::hard_link(root.join("x/data.bin"), root.join("y/same.bin")).unwrap();
    fs::write(root.join("y/other.bin"), vec![2u8; 7]).unwrap();
    let t = scan(
        &[Root::Path(root.clone())],
        &ScanOptions::default(),
        &ScanHandle::new(),
    )
    .unwrap();
    let types = export::file_types(&t, false);
    assert_eq!(types.len(), 1);
    assert_eq!((types[0].files, types[0].bytes), (2, 1007));
    assert_eq!(export::largest(&t, Options::default()).len(), 2);
}

#[test]
fn save_replaces_the_file_and_checks_the_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let t = fixture();
    let csv = tmp.path().join("types.csv");
    fs::write(&csv, "old").unwrap();
    export::save(&t, List::Types, Options::default(), &csv).unwrap();
    assert!(fs::read_to_string(&csv)
        .unwrap()
        .starts_with("\"extension\""));
    let json = tmp.path().join("tree.json");
    export::save(&t, List::Tree, Options::default(), &json).unwrap();
    assert_eq!(load(&json).unwrap().node(TOP).size, 35_301);
    let txt = tmp.path().join("tree.txt");
    assert!(export::save(&t, List::Tree, Options::default(), &txt).is_err());
    assert!(!txt.exists());
    let missing = tmp.path().join("no/such/dir/x.csv");
    assert!(export::save(&t, List::Tree, Options::default(), &missing).is_err());
    // No temporary files are left behind.
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 2);
}

#[test]
fn sizes_read_as_keel_shows_them() {
    assert_eq!(report::human(0), "0 B");
    assert_eq!(report::human(999), "999 B");
    assert_eq!(report::human(1000), "1.00 kB");
    assert_eq!(report::human(35_301), "35.30 kB");
    assert_eq!(report::human(999_999), "1.00 MB");
    assert_eq!(report::human(u64::MAX), "18.45 EB");
    assert_eq!(
        report::escape("<a href='x'>&\""),
        "&lt;a href=&#39;x&#39;&gt;&amp;&quot;"
    );
}

#[test]
fn report_is_self_contained_html() {
    let t = fixture();
    let mut v = Vec::new();
    report::write(&t, Options::default(), &mut v).unwrap();
    let html = String::from_utf8(v).unwrap();
    assert!(html.starts_with("<!doctype html>"));
    for part in [
        "<h2>Top folders</h2>",
        "<h2>File types</h2>",
        "<h2>Largest files</h2>",
        "<svg",
        "</svg>",
        "Scanned 2023-11-14T22:13:20Z",
        "35.30 kB",
        "70.82 %",
        "a,&quot;b&quot;",
        "break &lt;script&gt;&amp;&#39;.bin",
        ".txt",
        "(none)",
    ] {
        assert!(html.contains(part), "{part} missing");
    }
    // No scripts, no external assets.
    assert!(!html.contains("<script"));
    for external in ["src=", "href=", "url(", "@import"] {
        assert!(!html.contains(external), "{external}");
    }
    assert!(!html
        .replace("http://www.w3.org/2000/svg", "")
        .contains("http"));
    // One bar per top folder, the biggest first.
    assert_eq!(html.matches("<rect").count(), 2);
    let (docs, a) = (
        html.find(">docs<").unwrap(),
        html.find(">a,&quot;b&quot;<").unwrap(),
    );
    assert!(docs < a);
    // Ranked on disk, `a,"b"` (12 kB) comes before docs (8 kB).
    let mut v = Vec::new();
    let disk = Options {
        top: 1,
        size_on_disk: true,
        ..Options::default()
    };
    report::write(&t, disk, &mut v).unwrap();
    let html = String::from_utf8(v).unwrap();
    assert!(html.contains("ranked by size on disk"));
    assert_eq!(html.matches("<rect").count(), 1);
    assert!(html.contains(">a,&quot;b&quot;<"));
    assert!(html.contains("2 more types"));
}

#[test]
fn report_save_and_mail_link() {
    let tmp = tempfile::tempdir().unwrap();
    let t = fixture();
    let file = tmp.path().join("report é.html");
    report::save(&t, Options::default(), &file).unwrap();
    assert!(fs::read_to_string(&file)
        .unwrap()
        .contains("Disk usage report"));
    let uri = report::mailto(&t, &file).unwrap();
    let prefix = "mailto:?subject=Disk%20usage%20report&body=";
    assert!(uri.starts_with(prefix), "{uri}");
    let body = &uri[prefix.len()..];
    assert!(body
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._~%".contains(&b)));
    let decoded = percent_decode(body);
    assert!(decoded.contains(&file.display().to_string()), "{decoded}");
    assert!(decoded.contains("35.30 kB"));
    assert!(decoded.contains("Scanned: /r"));
    assert!(decoded.contains("\r\n"));
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

#[test]
fn summary_ranks_by_size_on_disk_on_request() {
    let t = fixture();
    let names = |s: Summary| -> Vec<String> {
        s.roots[0].children.iter().map(|c| c.name.clone()).collect()
    };
    assert_eq!(names(t.summary(10)), ["docs", "a,\"b\""]);
    assert_eq!(names(t.summary_by(10, true)), ["a,\"b\"", "docs"]);
}

#[test]
fn filters_check_compiles_the_regex() {
    assert!(Filters::default().check().is_ok());
    let bad = Filters {
        exclude_path_regex: Some("(".into()),
        ..Filters::default()
    };
    assert!(bad.check().is_err());
}
