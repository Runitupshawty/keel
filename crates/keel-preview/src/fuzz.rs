//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

/// The extension the first input byte picks (modulo the count); the rest is the file.
pub const EXTENSIONS: &[&str] = &[
    "txt", "rs", "json", "md", "csv", "tsv", "xlsx", "xls", "xlsb", "ods", "odt", "odp", "docx",
    "pptx", "svg", "png", "gif", "bmp", "ico", "webp", "jpg", "bin",
];

/// A font file's sample and names (`font::sample`).
pub fn font(data: &[u8]) {
    let _ = crate::font::sample(data, 256);
}

/// One file previewed as the panel previews it (panics are not caught here).
pub fn preview(data: &[u8]) {
    let Some((&pick, body)) = data.split_first() else {
        return;
    };
    let ext = EXTENSIONS[usize::from(pick) % EXTENSIONS.len()];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("input.{ext}"));
    std::fs::write(&path, body).unwrap();
    let req = crate::Request {
        entry: keel_vfs::Entry {
            path: keel_vfs::VPath::local(&path),
            name: format!("input.{ext}"),
            kind: keel_vfs::Kind::File,
            size: body.len() as u64,
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: ext.to_owned(),
        },
        bytes_path: path.clone(),
        page: 0,
        max_px: 256,
        fit_width: false,
    };
    let _ = crate::preview_inner(&req);
}

#[cfg(test)]
mod tests {
    use keel_vfs::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_preview() {
        crate::image::warm_fonts();
        run("preview", super::preview);
    }
    #[test]
    fn fuzz_font() {
        run("font", super::font);
    }
}
