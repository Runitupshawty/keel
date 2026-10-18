//! keel-daemon as `isolate_pdf_text`'s child (`--pdf-text-child`): the same text as reading
//! in this process, and a damaged PDF fails without taking the caller down.

use keel_core::TEXT_CAPS;
use keel_preview::{extract_text, init_pdfium, isolate_pdf_text, TextError};
use std::path::Path;

/// pdfium as the build's deps step leaves it (scripts/fetch-deps).
const PDFIUM: &str = if cfg!(windows) {
    "pdfium.dll"
} else if cfg!(target_os = "macos") {
    "libpdfium.dylib"
} else {
    "libpdfium.so"
};

#[test]
fn the_daemon_reads_pdf_text_in_a_child() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lib = root.join("target/deps").join(PDFIUM);
    if !lib.exists() {
        eprintln!("skipping: pdfium library missing from target/deps");
        return;
    }
    let fixtures = root.join("crates/keel-preview/tests/fixtures");
    let (sample, corrupt) = (fixtures.join("sample.pdf"), fixtures.join("corrupt.pdf"));
    init_pdfium(lib.parent().unwrap());
    let here = extract_text(&sample, "pdf", &TEXT_CAPS).unwrap();
    assert!(!here.text.trim().is_empty());

    // The child binds pdfium beside its own executable.
    let dir = tempfile::tempdir().unwrap();
    let daemon = Path::new(env!("CARGO_BIN_EXE_keel-daemon"));
    let child = dir.path().join(daemon.file_name().unwrap());
    std::fs::copy(daemon, &child).unwrap();
    std::fs::copy(&lib, dir.path().join(PDFIUM)).unwrap();
    isolate_pdf_text(child);
    assert_eq!(extract_text(&sample, "pdf", &TEXT_CAPS), Ok(here));
    assert!(matches!(
        extract_text(&corrupt, "pdf", &TEXT_CAPS),
        Err(TextError::Failed(_))
    ));
    // Truncation crosses over too.
    let mut caps = TEXT_CAPS;
    caps.max_chars = 3;
    assert!(extract_text(&sample, "pdf", &caps).unwrap().truncated);
}
