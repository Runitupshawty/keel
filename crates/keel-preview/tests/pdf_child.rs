//! `isolate_pdf_text`: the parent's side, with shell scripts standing in for the child (one
//! test, since the child program is process-wide). keel-daemon's tests run the real child.
#![cfg(unix)]

use keel_core::TEXT_CAPS;
use keel_preview::{extract_text, isolate_pdf_text, Extracted, TextError};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[test]
fn a_crashing_pdf_reader_ends_only_its_child() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/corrupt.pdf");
    let read = |body: &str| {
        let script = dir.path().join(format!("child-{}", body.len()));
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        isolate_pdf_text(script);
        extract_text(&pdf, "pdf", &TEXT_CAPS)
    };
    // pdfium segfaulting on a document (Task: keel-daemon died with it).
    assert_eq!(
        read("kill -SEGV $$"),
        Err(TextError::Failed("the PDF reader crashed".into()))
    );
    assert_eq!(
        read("kill -ABRT $$"),
        Err(TextError::Failed("the PDF reader crashed".into()))
    );
    // Exiting without saying anything is no text either.
    assert_eq!(
        read("exit 0"),
        Err(TextError::Failed("the PDF reader crashed".into()))
    );
    assert_eq!(
        read("printf '0Keel Test'"),
        Ok(Extracted {
            text: "Keel Test".into(),
            truncated: false
        })
    );
    assert_eq!(
        read("printf '1Kee'"),
        Ok(Extracted {
            text: "Kee".into(),
            truncated: true
        })
    );
    assert_eq!(
        read("printf 'not a valid PDF'; exit 2"),
        Err(TextError::Failed("not a valid PDF".into()))
    );
    assert_eq!(read("exit 3"), Err(TextError::Kind));
    assert_eq!(read("printf 77; exit 4"), Err(TextError::TooLarge(77)));
    // The arguments the child gets.
    let args = dir.path().join("args");
    let echo = format!("printf '%s\\n' \"$@\" > '{}'; printf 0", args.display());
    read(&echo).unwrap();
    assert_eq!(
        std::fs::read_to_string(&args).unwrap(),
        format!(
            "--pdf-text-child\n{}\n{}\n{}\n",
            pdf.display(),
            TEXT_CAPS.max_chars,
            TEXT_CAPS.time.as_millis()
        )
    );
    // A program that is not there.
    isolate_pdf_text(dir.path().join("missing"));
    assert_eq!(
        extract_text(&pdf, "pdf", &TEXT_CAPS),
        Err(TextError::Failed("the PDF reader did not start".into()))
    );
    // Other kinds never go to the child.
    let txt = dir.path().join("a.txt");
    std::fs::write(&txt, "plain").unwrap();
    assert_eq!(
        extract_text(&txt, "txt", &TEXT_CAPS).unwrap().text,
        "plain\n"
    );
}
