//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

/// A Web Share Target upload (`POST /share`): the first line is the `Content-Type` header,
/// the rest the `multipart/form-data` body, streamed into a fresh folder.
pub fn multipart(data: &[u8]) {
    let (head, body) = match data.iter().position(|&b| b == b'\n') {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    let boundary =
        crate::share::boundary(&String::from_utf8_lossy(head)).unwrap_or_else(|| "b".to_owned());
    let dir = tempfile::tempdir().unwrap();
    let _ = crate::share::parse(body, &boundary, dir.path());
}

/// An Everything HTTP query string (`--web`'s `/everything?...`): parsed, written back
/// and parsed again, its search compiled and run over a few names, the HTML and JSON
/// answers made; the same bytes as an `Authorization` header.
pub fn everything_query(data: &[u8]) {
    use crate::everything;
    let text = String::from_utf8_lossy(data);
    let _ = everything::authorized(Some(&text), "token", Some(&("u".into(), "p".into())));
    let _ = everything::is_search(&text);
    let Ok(r) = everything::parse(&text) else {
        return;
    };
    let link = everything::query_string(&r);
    if link.len() <= everything::MAX_QUERY_STRING {
        let again = everything::parse(&link).expect("its own links parse");
        assert_eq!(again.params.query, r.params.query);
    }
    let names = [
        "/a/b.txt",
        r"C:\Users\x\Café del Mar.mp3",
        "/r/report-final 2026.pdf",
    ];
    if keel_api::system::check_paths(&r.params, &names).is_err() {
        return;
    }
    let found = keel_api::system::Found {
        backend: "fuzz".into(),
        total: names.len(),
        capped: false,
        offset: r.params.offset.unwrap_or(0),
        hits: names
            .iter()
            .map(|n| keel_api::system::Hit {
                path: keel_vfs::VPath::local(n),
                is_dir: false,
                size: 1,
                modified: None,
                snippet: None,
            })
            .collect(),
    };
    let page = everything::Page {
        base: "/everything",
        downloads: true,
        unencrypted: true,
    };
    let html = everything::to_html(&r, Ok(&found), &page);
    assert!(!html.contains("<script"), "the search text is escaped");
    let _ = everything::to_json(&found, &r).to_string();
}

#[cfg(test)]
mod tests {
    use keel_vfs::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_multipart() {
        run("multipart", super::multipart);
    }

    #[test]
    fn fuzz_everything_query() {
        run("everything_query", super::everything_query);
    }
}
