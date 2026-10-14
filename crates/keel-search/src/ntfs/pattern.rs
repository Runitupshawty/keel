//! Native query compiler; the shared AST owns the language.
use super::index::Matcher;
use crate::Query;

pub(crate) fn folder_prefix(dir: &std::path::Path) -> String {
    format!(
        "{}\\",
        dir.display().to_string().trim_end_matches(['\\', '/'])
    )
}

pub(crate) fn compile(query: &Query) -> anyhow::Result<Matcher> {
    let mut m = crate::walkindex::compile(query)?;
    m.within = m.within.map(|p| p.replace('/', "\\"));
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntfs::index::Index;

    fn index() -> Index {
        let mut ix = Index::new("C:", 5);
        for (frn, parent, name, dir) in [
            (10, 5, "Users", true),
            (11, 10, "ann", true),
            (12, 11, "report.pdf", false),
            (13, 11, "Report 2026.PDF", false),
            (14, 11, "report", true),
            (15, 11, "notes.txt", false),
            (16, 11, "my report.docx", false),
            (20, 5, "Windows", true),
            (21, 20, "report.pdf", false),
            (22, 11, "reportage", false),
        ] {
            ix.upsert(frn, parent, name, dir);
        }
        ix
    }

    fn find(text: &str) -> Vec<String> {
        find_q(Query {
            text: text.into(),
            ..Query::default()
        })
    }

    fn find_q(q: Query) -> Vec<String> {
        let m = compile(&q).unwrap();
        index()
            .search(&m, q.max as usize)
            .into_iter()
            .map(|f| f.path)
            .collect()
    }

    #[test]
    fn substring_is_case_insensitive_and_ranked() {
        assert_eq!(
            find("REPORT"),
            [
                r"C:\Users\ann\report",
                r"C:\Users\ann\Report 2026.PDF",
                r"C:\Users\ann\report.pdf",
                r"C:\Users\ann\reportage",
                r"C:\Windows\report.pdf",
                r"C:\Users\ann\my report.docx",
            ]
        );
        assert!(find("nothing-like-this").is_empty());
    }

    #[test]
    fn match_case_is_exact() {
        let hits = find_q(Query {
            text: "Report".into(),
            match_case: true,
            ..Query::default()
        });
        assert_eq!(hits, [r"C:\Users\ann\Report 2026.PDF"]);
    }

    #[test]
    fn glob_matches_the_whole_name() {
        assert_eq!(
            find("*.pdf"),
            [
                r"C:\Users\ann\Report 2026.PDF",
                r"C:\Users\ann\report.pdf",
                r"C:\Windows\report.pdf",
            ]
        );
        assert_eq!(find("not?s.*"), [r"C:\Users\ann\notes.txt"]);
        assert!(find("*.pd").is_empty());
    }

    #[test]
    fn regex_prefix_and_regex_mode() {
        assert_eq!(find(r"regex:^rep.*\d"), [r"C:\Users\ann\Report 2026.PDF"]);
        let hits = find_q(Query {
            text: r"^n.tes\.txt$".into(),
            regex: true,
            ..Query::default()
        });
        assert_eq!(hits, [r"C:\Users\ann\notes.txt"]);
        // A pattern that could run across names still only matches within one.
        assert!(find(r"regex:docx\swindows").is_empty());
        assert!(compile(&Query {
            text: "regex:(".into(),
            ..Query::default()
        })
        .is_err());
    }

    #[test]
    fn terms_and_folder_filter() {
        assert_eq!(
            find("report pdf"),
            [
                r"C:\Users\ann\Report 2026.PDF",
                r"C:\Users\ann\report.pdf",
                r"C:\Windows\report.pdf",
            ]
        );
        assert_eq!(find("\"my report\""), [r"C:\Users\ann\my report.docx"]);
        assert_eq!(find("folder: report"), [r"C:\Users\ann\report"]);
        let all_folders = find_q(Query {
            folders_only: true,
            ..Query::default()
        });
        assert_eq!(
            all_folders,
            [
                r"C:\Users",
                r"C:\Users\ann",
                r"C:\Users\ann\report",
                r"C:\Windows"
            ]
        );
    }

    #[test]
    fn in_filter_takes_a_path_prefix() {
        assert_eq!(
            find(r"report.pdf in:c:\windows"),
            [r"C:\Windows\report.pdf"]
        );
        assert_eq!(find(r"report.pdf in:C:\Win"), [r"C:\Windows\report.pdf"]);
        assert_eq!(find(r"in:C:\Users\ann\rep").len(), 4);
        assert_eq!(find(r"report.pdf in:C:\").len(), 2);
        assert!(find(r"report in:D:\").is_empty());
        assert!(find(r"report in:C:\Nope\x").is_empty());
        assert_eq!(find(r#"in:"C:\Users\ann\my report.docx""#).len(), 1);
    }

    #[test]
    fn a_pane_folder_keeps_to_whole_folder_names() {
        let within = |text: &str, dir: &str, regex| {
            find_q(Query {
                text: text.into(),
                regex,
                within: Some(dir.into()),
                ..Query::default()
            })
        };
        let windows = [r"C:\Windows\report.pdf"];
        assert_eq!(within("report.pdf", r"C:\Windows", false), windows);
        assert_eq!(within("report.pdf", r"c:\windows\", false), windows);
        assert_eq!(within(r"^report\.pdf$", r"C:\Windows", true), windows);
        assert!(within("report.pdf", r"C:\Win", false).is_empty());
        assert_eq!(within("report.pdf", r"C:\", false).len(), 2);
        // A typed in: takes its place.
        assert_eq!(
            within(r"report.pdf in:C:\Windows", r"C:\Users", false),
            windows
        );
        // What the index holds; NtfsSearcher walks any other folder.
        let covers = |dir: &str| index().covers(&folder_prefix(std::path::Path::new(dir)));
        assert!(covers(r"C:\Windows") && covers(r"c:\users\ann\") && covers(r"C:\"));
        assert!(!covers(r"C:\Win") && !covers(r"D:\x") && !covers(r"\\server\share"));
    }

    /// Match Whole Words, Match Path, Match Diacritics and whole-filename wildcards.
    #[test]
    fn search_toggles() {
        let with = |text: &str, set: fn(&mut Query)| {
            let mut q = Query {
                text: text.into(),
                ..Query::default()
            };
            set(&mut q);
            find_q(q)
        };
        let words = with("report", |q| q.whole_word = true);
        assert_eq!(words.len(), 5);
        assert!(!words.iter().any(|p| p.ends_with("reportage")));
        assert_eq!(with("port", |q| q.whole_word = true).len(), 0);
        assert_eq!(find("ann"), [r"C:\Users\ann"]);
        assert_eq!(with("ann", |q| q.match_path = true).len(), 7);
        assert_eq!(
            with("windows pdf", |q| q.match_path = true),
            [r"C:\Windows\report.pdf"]
        );
        assert_eq!(
            with(r"**\ann\rep* folder:", |q| q.match_path = true),
            [r"C:\Users\ann\report"],
            "a wildcard over the whole path (`**` crosses folders)"
        );
        assert_eq!(find("nOtés"), [r"C:\Users\ann\notes.txt"]);
        assert!(with("notés", |q| q.match_diacritics = true).is_empty());
        assert!(find("*.pd").is_empty());
        assert_eq!(
            with("*.pd", |q| q.whole_filename_wildcards = false).len(),
            3
        );
    }

    /// The language reads the index's columns, its folder totals and its parent links.
    #[test]
    fn the_shared_language_reads_columns_totals_and_parents() {
        use crate::ntfs::index::Info;
        let mut ix = index();
        // 2026-06-15 12:00 UTC as FILETIME (the same year in every time zone).
        let modified = 134_259_984_000_000_000;
        let file = |size: u64, modified: i64| Info {
            size,
            allocated: size.div_ceil(4096) * 4096,
            modified,
            created: modified,
            attributes: 0x20,
            links: 1,
            ..Info::default()
        };
        ix.set_info(12, file(5000, modified));
        ix.set_info(13, file(10, 0));
        ix.set_info(21, file(7, 0));
        ix.settle();
        let find = |text: &str| {
            let q = Query {
                text: text.into(),
                ..Query::default()
            };
            let mut found: Vec<String> = ix
                .search(&compile(&q).unwrap(), 100)
                .into_iter()
                .map(|f| f.path)
                .collect();
            found.sort();
            found
        };
        assert_eq!(find("file: size:>1000"), [r"C:\Users\ann\report.pdf"]);
        assert_eq!(
            find("file: size-on-disk:8192"),
            [r"C:\Users\ann\report.pdf"]
        );
        assert_eq!(find("file: dm:2026"), [r"C:\Users\ann\report.pdf"]);
        assert_eq!(find("file: dm:2025"), Vec::<String>::new());
        assert_eq!(find("attrib:D"), find("folder:"));
        // A folder's size is the total below it; its counts come from the folder totals.
        assert_eq!(find("folder: size:5010"), [r"C:\Users", r"C:\Users\ann"]);
        assert_eq!(
            find("descendant-file-count:>=5 folder:"),
            [r"C:\Users", r"C:\Users\ann"]
        );
        // Direct children and depth come from the parent links.
        assert_eq!(find("child-file-count:5"), [r"C:\Users\ann"]);
        assert_eq!(find("child:notes.txt"), [r"C:\Users\ann"]);
        assert_eq!(find("depth:2 folder:"), [r"C:\Users\ann"]);
        assert_eq!(find("file-id:12"), [r"C:\Users\ann\report.pdf"]);
        assert_eq!(find("parent-file-id:20"), [r"C:\Windows\report.pdf"]);
        assert_eq!(
            find("sibling:notes.txt report"),
            [
                r"C:\Users\ann\Report 2026.PDF",
                r"C:\Users\ann\my report.docx",
                r"C:\Users\ann\report",
                r"C:\Users\ann\report.pdf",
                r"C:\Users\ann\reportage",
            ]
        );
        // Set functions run over the whole result.
        assert_eq!(
            find("dupe:name"),
            [r"C:\Users\ann\report.pdf", r"C:\Windows\report.pdf"]
        );
    }

    #[test]
    fn max_keeps_the_best() {
        let hits = find_q(Query {
            text: "report".into(),
            max: 2,
            ..Query::default()
        });
        // The exact name, then the first prefix match in index order.
        assert_eq!(hits, [r"C:\Users\ann\report", r"C:\Users\ann\report.pdf"]);
    }
}
