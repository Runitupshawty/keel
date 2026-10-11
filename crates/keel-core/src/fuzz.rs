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

/// The library search box: `LibraryQuery::parse` and the SQL it becomes.
pub fn query(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    for offset in [0, -36_000, 50_400] {
        if let Ok(q) = crate::search::LibraryQuery::parse_at(&text, 1_760_000_000, offset) {
            let _ = q.filters();
            let _ = q.text_parts(true);
            let _ = q.ext_match();
        }
    }
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
