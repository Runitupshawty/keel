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

/// ETP command lines (`--etp`): each line parsed as the server parses it, `SITE
/// EVERYTHING` settings applied, paths resolved from the current folder (never above `/`,
/// never `..` or empty parts), and a `QUERY` reply written; the same lines read back by
/// the ETP client (`keel_search::etp`), and the first one as its `-connect` target.
pub fn etp_command(data: &[u8]) {
    use crate::etp::{fs::Virtual, proto};
    let text = String::from_utf8_lossy(data);
    let mut opts = proto::Options::default();
    let mut cwd = Virtual::default();
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        lines.push(line.to_owned());
        let Ok(cmd) = proto::parse(line) else {
            continue;
        };
        let arg = match &cmd {
            proto::Command::Everything(e) => {
                let _ = opts.apply(e);
                continue;
            }
            proto::Command::Cwd(a) | proto::Command::Retr(a) | proto::Command::Mlst(a) => {
                a.clone()
            }
            proto::Command::List(a) | proto::Command::Nlst(a) => {
                proto::list_target(a).to_owned()
            }
            _ => continue,
        };
        let v = Virtual::resolve(&cwd, &arg);
        assert!(
            v.0.iter().all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains('/')),
            "{v:?}"
        );
        cwd = v;
    }
    let p = opts.params();
    assert!(p.count.unwrap_or(0) <= proto::MAX_COUNT as usize);
    let _ = opts.filter_params();
    let item = proto::Item {
        name: "a.txt".into(),
        path: "Media\\x".into(),
        size: Some(1),
        ..proto::Item::default()
    };
    let reply = proto::results(3, &[item.clone(), item], &opts);
    let reply_lines: Vec<&str> = reply.split("\r\n").collect();
    let read = keel_search::etp::parse_results(&reply_lines).expect("its own reply parses");
    assert_eq!((read.total, read.items.len()), (3, 2));
    let _ = keel_search::etp::parse_results(&lines);
    if let Some(first) = lines.first() {
        let _ = keel_search::etp::Target::parse(first);
    }
    let _ = proto::welcome(&text);
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

    #[test]
    fn fuzz_etp_command() {
        run("etp_command", super::etp_command);
    }
}
