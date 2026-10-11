//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

use std::path::Path;

fn with_file(data: &[u8], name: &str, f: impl FnOnce(&Path)) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, data).unwrap();
    f(&path);
}

/// A TIFF's orientation (the 0.13.0 IFD reader) and its metadata.
pub fn tiff(data: &[u8]) {
    with_file(data, "input.tif", |path| {
        let _ = crate::media::tiff_orientation(path);
        let _ = crate::media::read_meta(path);
    });
}

/// A camera RAW file: the walk to its embedded previews, its metadata and, when the largest
/// preview is small, its pixels.
pub fn raw(data: &[u8]) {
    with_file(data, "input.dng", |path| {
        let small = crate::media::raw_previews(path)
            .ok()
            .and_then(|p| p.first().copied())
            .is_some_and(|p| u64::from(p.width) * u64::from(p.height) <= 1 << 16);
        let _ = crate::media::read_meta(path);
        if small {
            let _ = crate::media::thumbnail(path, 64);
        }
    });
}

/// A photo's metadata (EXIF, XMP, dimensions by content) and, when it is small, its pixels.
pub fn image_meta(data: &[u8]) {
    with_file(data, "input.jpg", |path| {
        let _ = crate::media::read_meta(path);
        let small = image::ImageReader::open(path)
            .and_then(|r| r.with_guessed_format())
            .ok()
            .and_then(|r| r.into_dimensions().ok())
            .is_some_and(|(w, h)| u64::from(w) * u64::from(h) <= 1 << 16);
        if small {
            let _ = crate::media::thumbnail(path, 64);
        }
    });
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = crate::media::parse_xmp(text);
    }
}

/// `ffprobe -print_format json` output as the video metadata reader takes it.
pub fn probe(data: &[u8]) {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) {
        let _ = crate::media::parse_probe(&v);
    }
}

/// The library search box: `LibraryQuery::parse` and the SQL it becomes, and the filter
/// chips (Task 57): a chip's click turns it on or off whatever the text around it.
pub fn query(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let mut chips = crate::chips::Chip::kinds();
    chips.extend(crate::chips::Chip::sizes());
    chips.push(crate::chips::Chip::favorites());
    chips.extend(crate::chips::Chip::ext("pdf"));
    chips.extend(crate::chips::Chip::tag("two words"));
    chips.push(crate::chips::Chip::contents());
    for c in &chips {
        let on = crate::chips::is_on(&text, c);
        assert_ne!(crate::chips::is_on(&crate::chips::toggle(&text, c), c), on);
    }
    let _ = crate::chips::values(&text, crate::chips::Group::Type);
    for offset in [0, -36_000, 50_400] {
        if let Ok(q) = crate::search::LibraryQuery::parse_at(&text, 1_760_000_000, offset) {
            let _ = q.filters();
            let _ = q.ext_match();
            // Whatever was typed, the MATCH strings are valid FTS5 (no operator from the
            // user's text errors or scans everything).
            let names = q.text_parts(true).into_iter().map(|m| ("n", m));
            let content = q.content_match().into_iter().map(|m| ("c", m));
            MATCH.with(|c| {
                for (table, m) in names.chain(content) {
                    let hits = c.query_row(
                        &format!("SELECT count(*) FROM {table} WHERE {table} MATCH ?1"),
                        [&m],
                        |r| r.get::<_, i64>(0),
                    );
                    assert!(hits.is_ok(), "{m}: {hits:?}");
                }
            });
        }
    }
}

thread_local! {
    /// FTS5 tables shaped like `record_fts` and `record_content`, one row each.
    static MATCH: rusqlite::Connection = {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE VIRTUAL TABLE n USING fts5(name, path, tokenize='unicode61 remove_diacritics 2', prefix='2 3');
             CREATE VIRTUAL TABLE c USING fts5(text, tokenize='unicode61 remove_diacritics 2');
             INSERT INTO n VALUES ('Keel report.pdf', 'docs/Keel report.pdf');
             INSERT INTO c VALUES ('Keel Test Document near the end');",
        )
        .unwrap();
        c
    };
}

/// A library backup (`.keelbackup`): the manifest and entry checks (`inspect`), then a
/// restore into a scratch data folder (hashes, database checks, the library's layout).
pub fn backup(data: &[u8]) {
    with_file(data, "input.keelbackup", |path| {
        let _ = crate::backup::inspect(path);
        let root = tempfile::tempdir().unwrap();
        let _ = crate::backup::restore(root.path(), path, None);
    });
}

#[cfg(test)]
mod tests {
    use keel_vfs::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_tiff() {
        run("tiff", super::tiff);
    }
    #[test]
    fn fuzz_raw() {
        run("raw", super::raw);
    }
    #[test]
    fn fuzz_image_meta() {
        run("image_meta", super::image_meta);
    }
    #[test]
    fn fuzz_probe() {
        run("probe", super::probe);
    }
    #[test]
    fn fuzz_query() {
        run("query", super::query);
    }
    #[test]
    fn fuzz_backup() {
        run("backup", super::backup);
    }
}
