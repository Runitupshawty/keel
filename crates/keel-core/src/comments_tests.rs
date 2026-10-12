use super::*;
use crate::index::tests::{id_of, walk, write};
use crate::library::tests::folder;
use crate::{ChangeEvent, Indexer, Source};
use keel_vfs::VPath;
use std::sync::Arc;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Library, Arc<Source>) {
    let files = tempfile::tempdir().unwrap();
    for name in ["Work/plan.txt", "top.txt"] {
        write(&files.path().join(name), name);
    }
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "t").unwrap();
    let src = lib
        .source(&lib.add_source(folder("F", files.path())).unwrap())
        .unwrap();
    walk(&src, &lib.router()).unwrap();
    (files, data, lib, src)
}

fn rec(src: &Source, rel: &str) -> RecordRef {
    RecordRef {
        source: src.id.clone(),
        id: id_of(src, rel).unwrap(),
    }
}

#[test]
fn set_read_clear_and_follow_a_rename() {
    let (files, _data, lib, src) = fixture();
    let plan = rec(&src, "Work/plan.txt");
    assert_eq!(lib.comment(&plan).unwrap(), None);
    lib.set_comment(&plan, "Send to the printer\nby Friday")
        .unwrap();
    assert_eq!(
        lib.comment(&plan).unwrap().as_deref(),
        Some("Send to the printer\nby Friday")
    );
    // Folders take comments too.
    let work = rec(&src, "Work");
    lib.set_comment(&work, "Q3").unwrap();
    assert_eq!(lib.comment(&work).unwrap().as_deref(), Some("Q3"));
    // Renamed: the index keeps the record, so the comment follows.
    let root = files.path();
    std::fs::rename(root.join("Work/plan.txt"), root.join("Work/final.txt")).unwrap();
    for p in ["Work/final.txt", "Work/plan.txt"] {
        Indexer::apply_change(&src, ChangeEvent::Changed(VPath::local(root.join(p)))).unwrap();
    }
    assert_eq!(rec(&src, "Work/final.txt"), plan);
    assert!(lib.comment(&plan).unwrap().is_some());
    // Blank clears; too long and unknown records are refused.
    lib.set_comment(&plan, "  \n").unwrap();
    assert_eq!(lib.comment(&plan).unwrap(), None);
    assert!(lib.set_comment(&work, &"x".repeat(MAX_COMMENT + 1)).is_err());
    let gone = RecordRef {
        source: src.id.clone(),
        id: 999_999,
    };
    assert!(lib.set_comment(&gone, "x").is_err());
    let elsewhere = RecordRef {
        source: crate::SourceId("nope".into()),
        id: work.id,
    };
    assert!(lib.comment(&elsewhere).is_err());
    // The file leaving the index drops its comment.
    let top = rec(&src, "top.txt");
    lib.set_comment(&top, "bye").unwrap();
    std::fs::remove_file(root.join("top.txt")).unwrap();
    Indexer::apply_change(&src, ChangeEvent::Changed(VPath::local(root.join("top.txt"))))
        .unwrap();
    let c = src.store.get().unwrap();
    let left: i64 = c
        .query_row(
            "SELECT count(*) FROM record_comment WHERE record = ?1",
            [top.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0);
}
