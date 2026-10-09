use super::*;
use crate::index::tests::{id_of, walk, write};
use crate::library::tests::folder;
use crate::{validate_preview_execute, JobStatus, Op};

struct Fixture {
    files: tempfile::TempDir,
    data: tempfile::TempDir,
    lib: Library,
    src: Arc<Source>,
}

fn fixture() -> Fixture {
    let files = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt", "sub/d.txt"] {
        write(&files.path().join(name), name);
    }
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "t").unwrap();
    let src = lib
        .source(&lib.add_source(folder("F", files.path())).unwrap())
        .unwrap();
    walk(&src, &lib.router()).unwrap();
    Fixture {
        files,
        data,
        lib,
        src,
    }
}

fn r(src: &Source, rel: &str) -> RecordRef {
    RecordRef {
        source: src.id.clone(),
        id: id_of(src, rel).unwrap(),
    }
}

fn names(hits: Vec<LibraryHit>) -> Vec<String> {
    hits.into_iter().map(|h| h.name).collect()
}

#[test]
fn tags_are_created_renamed_recolored_nested_and_deleted() {
    let f = fixture();
    let lib = &f.lib;
    let work = lib.create_tag("Work", Some("#e5484d"), None).unwrap();
    let client = lib.create_tag("client", None, Some(work)).unwrap();
    assert!(
        lib.create_tag("work", None, None).is_err(),
        "siblings are unique"
    );
    let other = lib.create_tag("client", None, None).unwrap(); // same name elsewhere is fine
    assert!(lib.create_tag(" x", None, None).is_err());
    assert!(lib.create_tag("x", None, Some(999)).is_err());

    lib.rename_tag(work, "Job").unwrap();
    lib.recolor_tag(work, Some("#30a46c")).unwrap();
    assert!(lib.nest_tag(work, Some(client)).is_err(), "no cycles");
    assert!(lib.nest_tag(work, Some(work)).is_err());
    let tags = lib.tags().unwrap();
    assert_eq!(
        tags.iter()
            .map(|t| (t.name.as_str(), t.parent))
            .collect::<Vec<_>>(),
        [("client", Some(work)), ("client", None), ("Job", None)]
    );
    assert_eq!(tags[2].color.as_deref(), Some("#30a46c"));
    // Favorites is reserved.
    assert!(tags.iter().all(|t| t.id != FAVORITES));
    assert!(lib.rename_tag(FAVORITES, "x").is_err());
    assert!(lib.delete_tag(FAVORITES).is_err());
    assert!(lib.create_tag("x", None, Some(FAVORITES)).is_err());

    // The source store carries a copy of the definitions.
    let mirrored: Vec<String> = {
        let c = f.src.store.get().unwrap();
        let mut stmt = c.prepare("SELECT name FROM tag ORDER BY id").unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.map(Result::unwrap).collect()
    };
    assert_eq!(mirrored, ["Favorites", "Job", "client", "client"]);

    lib.set_tag(work, &[r(&f.src, "a.txt")], true).unwrap();
    assert!(
        lib.delete_tag(work).is_err(),
        "its nested client would clash"
    );
    lib.rename_tag(other, "clients").unwrap();
    lib.delete_tag(work).unwrap();
    assert!(lib.tags_of(&r(&f.src, "a.txt")).unwrap().is_empty());
    let nested = lib.tags().unwrap();
    assert_eq!(nested.len(), 2);
    assert!(
        nested.iter().all(|t| t.parent.is_none()),
        "moved up a level"
    );
    assert!(lib.rename_tag(work, "gone").is_err());
}

#[test]
fn tags_apply_in_bulk_nest_in_queries_and_persist() {
    let f = fixture();
    let lib = &f.lib;
    let work = lib.create_tag("work", None, None).unwrap();
    let client = lib.create_tag("client", None, Some(work)).unwrap();
    let (a, b, c, d) = (
        r(&f.src, "a.txt"),
        r(&f.src, "b.txt"),
        r(&f.src, "c.txt"),
        r(&f.src, "sub/d.txt"),
    );
    lib.set_tag(work, &[a.clone(), b.clone(), b.clone()], true)
        .unwrap();
    lib.set_tag(client, std::slice::from_ref(&d), true).unwrap();
    assert_eq!(lib.tags_of(&b).unwrap(), [work]);
    assert_eq!(
        names(lib.records_with_tag(work).unwrap()),
        ["a.txt", "b.txt", "d.txt"]
    );
    assert_eq!(names(lib.records_with_tag(client).unwrap()), ["d.txt"]);
    let search = |q: &str| {
        let mut n = names(lib.search(&LibraryQuery::parse(q).unwrap()).unwrap());
        n.sort();
        n
    };
    assert_eq!(search("tag:work"), ["a.txt", "b.txt", "d.txt"]);
    assert_eq!(search("tag:client"), ["d.txt"]);

    lib.set_tag(work, &[a.clone(), c.clone()], false).unwrap();
    assert_eq!(
        names(lib.records_with_tag(work).unwrap()),
        ["b.txt", "d.txt"]
    );
    let ghost = RecordRef {
        source: f.src.id.clone(),
        id: 9_999,
    };
    lib.set_tag(work, &[ghost], true).unwrap(); // skipped
    let stranger = RecordRef {
        source: crate::SourceId("nope".into()),
        id: 1,
    };
    assert!(lib.set_tag(work, &[stranger], true).is_err());

    // Renamed tags still resolve in search (the copies follow).
    lib.rename_tag(work, "office").unwrap();
    assert_eq!(search("tag:office"), ["b.txt", "d.txt"]);
    assert!(search("tag:work").is_empty());

    // Links survive a reopen and go with the record.
    let Fixture {
        files,
        data,
        lib,
        src,
    } = f;
    drop((lib, src));
    let lib = Library::open(data.path(), "t").unwrap();
    assert_eq!(
        names(lib.records_with_tag(work).unwrap()),
        ["b.txt", "d.txt"]
    );
    std::fs::remove_file(files.path().join("b.txt")).unwrap();
    let src = lib.source(&lib.sources()[0].id).unwrap();
    walk(&src, &lib.router()).unwrap();
    assert_eq!(names(lib.records_with_tag(work).unwrap()), ["d.txt"]);
}

#[test]
fn favorites_are_a_reserved_tag() {
    let f = fixture();
    let (a, b) = (r(&f.src, "a.txt"), r(&f.src, "b.txt"));
    f.lib.set_favorite(&[a.clone(), b.clone()], true).unwrap();
    f.lib.set_favorite(std::slice::from_ref(&a), false).unwrap();
    assert_eq!(names(f.lib.favorites().unwrap()), ["b.txt"]);
    assert_eq!(f.lib.tags_of(&b).unwrap(), [FAVORITES]);
    assert!(f.lib.tags().unwrap().is_empty());
}

#[test]
fn recents_merge_opens_and_executed_operations() {
    let f = fixture();
    let lib = &f.lib;
    let a = r(&f.src, "a.txt");
    lib.note_open(&a).unwrap();
    lib.note_open(&r(&f.src, "c.txt")).unwrap();
    lib.note_open(&a).unwrap(); // once, at its latest
                                // Make the opens older than the operation below.
    lib.shared
        .db
        .get()
        .unwrap()
        .execute("UPDATE opened SET ts = ts - 100", [])
        .unwrap();
    let plan = validate_preview_execute(
        lib,
        Op::Rename {
            path: VPath::local(f.files.path().join("b.txt")),
            new_name: "renamed.txt".into(),
        },
    )
    .unwrap();
    let job = plan.execute(lib).unwrap();
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    let copy = validate_preview_execute(
        lib,
        Op::Copy {
            src: vec![VPath::local(f.files.path().join("sub/d.txt"))],
            dst_dir: VPath::local(f.files.path()),
            on_conflict: crate::OnConflict::Skip,
        },
    )
    .unwrap();
    let job = copy.execute(lib).unwrap();
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);

    let recents = lib.recents(10).unwrap();
    let mut ops = names(recents[..2].to_vec());
    ops.sort();
    assert_eq!(ops, ["d.txt", "renamed.txt"], "operations are newest");
    assert_eq!(names(recents[2..].to_vec()), ["a.txt", "c.txt"]);
    assert_eq!(recents[2].record, a);
    assert_eq!(lib.recents(1).unwrap().len(), 1);

    // A record that is gone drops out.
    std::fs::remove_file(f.files.path().join("c.txt")).unwrap();
    walk(&f.src, &lib.router()).unwrap();
    assert!(names(lib.recents(10).unwrap()).iter().all(|n| n != "c.txt"));
    let _ = &f.data;
}

#[test]
fn views_are_saved_listed_updated_and_deleted() {
    let f = fixture();
    let lib = &f.lib;
    let mut pdfs = lib.create_view("PDFs", "ext:pdf", "details").unwrap();
    let recent = lib.create_view("This month", "dm:2026-10", "grid").unwrap();
    assert!(lib.create_view("Bad", "size:lots", "grid").is_err());
    assert!(lib.create_view("", "x", "grid").is_err());
    assert_eq!(lib.views().unwrap(), [pdfs.clone(), recent.clone()]);
    pdfs.query = "ext:pdf;docx".into();
    pdfs.layout = "grid".into();
    lib.update_view(&pdfs).unwrap();
    pdfs.query = "dm:never".into();
    assert!(lib.update_view(&pdfs).is_err());
    lib.delete_view(recent.id).unwrap();
    assert!(lib.delete_view(recent.id).is_err());
    let views = lib.views().unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(
        (views[0].query.as_str(), views[0].layout.as_str()),
        ("ext:pdf;docx", "grid")
    );
}
