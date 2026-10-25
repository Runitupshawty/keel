use super::*;
use crate::index::tests::{id_of, walk, write};
use crate::library::tests::folder;
use crate::library::{OfflineReason, Source, SourceStatus};
use std::sync::Arc;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Library, Arc<Source>) {
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
    (files, data, lib, src)
}

fn r(src: &Source, rel: &str) -> RecordRef {
    RecordRef {
        source: src.id.clone(),
        id: id_of(src, rel).unwrap(),
    }
}

fn names(lib: &Library, id: CollectionId) -> Vec<String> {
    (lib.collection_items(id).unwrap().into_iter())
        .map(|h| h.name)
        .collect()
}

#[test]
fn collections_are_created_renamed_and_deleted() {
    let (_files, _data, lib, _src) = fixture();
    let trip = lib.create_collection("Trip").unwrap();
    let album = lib.create_collection("album").unwrap();
    assert!(lib.create_collection("trip").is_err(), "names are unique");
    assert!(lib.create_collection(" x").is_err());
    assert!(lib.rename_collection(album, "TRIP").is_err());
    lib.rename_collection(album, "Album").unwrap();
    let all = lib.collections().unwrap();
    assert_eq!(
        all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["Album", "Trip"]
    );
    lib.delete_collection(trip).unwrap();
    assert!(lib.delete_collection(trip).is_err());
    assert!(lib.rename_collection(trip, "x").is_err());
    assert_eq!(lib.collections().unwrap().len(), 1);
}

#[test]
fn items_keep_the_order_they_are_given() {
    let (_files, _data, lib, src) = fixture();
    let id = lib.create_collection("Picks").unwrap();
    let (a, b, c, d) = (
        r(&src, "a.txt"),
        r(&src, "b.txt"),
        r(&src, "c.txt"),
        r(&src, "sub/d.txt"),
    );
    // Added in this order, not by name; a repeat stays where it was.
    assert_eq!(
        lib.add_to_collection(id, &[c.clone(), a.clone()]).unwrap(),
        2
    );
    assert_eq!(
        lib.add_to_collection(id, &[d.clone(), c.clone()]).unwrap(),
        1
    );
    assert_eq!(names(&lib, id), ["c.txt", "a.txt", "d.txt"]);
    assert_eq!(lib.collections().unwrap()[0].items, 3);

    // Dragging `d` before `c`, then `c` and `a` (in collection order) to the end.
    lib.move_in_collection(id, std::slice::from_ref(&d), Some(&c))
        .unwrap();
    assert_eq!(names(&lib, id), ["d.txt", "c.txt", "a.txt"]);
    lib.move_in_collection(id, &[a.clone(), c.clone()], None)
        .unwrap();
    assert_eq!(names(&lib, id), ["d.txt", "c.txt", "a.txt"]);
    lib.move_in_collection(id, std::slice::from_ref(&a), Some(&d))
        .unwrap();
    assert_eq!(names(&lib, id), ["a.txt", "d.txt", "c.txt"]);
    // Onto itself, or before something not in the collection: no harm.
    lib.move_in_collection(id, std::slice::from_ref(&a), Some(&a))
        .unwrap();
    assert_eq!(names(&lib, id), ["d.txt", "c.txt", "a.txt"]);
    lib.move_in_collection(id, std::slice::from_ref(&d), Some(&b))
        .unwrap();
    assert_eq!(names(&lib, id), ["c.txt", "a.txt", "d.txt"]);

    assert_eq!(lib.remove_from_collection(id, &[a, b]).unwrap(), 1);
    assert_eq!(names(&lib, id), ["c.txt", "d.txt"]);
    assert!(lib.add_to_collection(999, &[c]).is_err());
}

#[test]
fn gone_records_leave_the_collection_and_the_rest_survive_reopening() {
    let (files, data, lib, src) = fixture();
    let id = lib.create_collection("Picks").unwrap();
    lib.add_to_collection(id, &[r(&src, "b.txt"), r(&src, "a.txt"), r(&src, "c.txt")])
        .unwrap();
    std::fs::remove_file(files.path().join("a.txt")).unwrap();
    walk(&src, &lib.router()).unwrap();
    assert_eq!(names(&lib, id), ["b.txt", "c.txt"]);
    assert_eq!(
        lib.collections().unwrap()[0].items,
        2,
        "forgotten when read"
    );

    drop((src, lib));
    let lib = Library::open(data.path(), "t").unwrap();
    assert_eq!(names(&lib, id), ["b.txt", "c.txt"]);
    // A removed source takes its items with it.
    let source = lib.sources()[0].id.clone();
    lib.remove_source(&source, false).unwrap();
    assert!(names(&lib, id).is_empty());
    // Deleting a collection never touches the files.
    lib.delete_collection(id).unwrap();
    assert!(files.path().join("b.txt").exists());
}

#[test]
fn a_renamed_member_keeps_its_record_and_a_replacement_is_not_it() {
    let (files, _data, lib, src) = fixture();
    let id = lib.create_collection("Picks").unwrap();
    let member = r(&src, "a.txt");
    lib.add_to_collection(id, std::slice::from_ref(&member))
        .unwrap();

    // Renamed within the source: the record keeps its id and the member follows it.
    std::fs::rename(files.path().join("a.txt"), files.path().join("a2.txt")).unwrap();
    walk(&src, &lib.router()).unwrap();
    assert_eq!(
        id_of(&src, "a2.txt"),
        Some(member.id),
        "rename keeps identity"
    );
    let items = lib.collection_items(id).unwrap();
    assert_eq!(items.len(), 1, "still one member");
    assert_eq!(items[0].record, member);
    assert_eq!(names(&lib, id), ["a2.txt"]);

    // An unrelated file takes the old path: a new record, not the member.
    write(&files.path().join("a.txt"), "replacement");
    walk(&src, &lib.router()).unwrap();
    assert_ne!(
        id_of(&src, "a.txt"),
        Some(member.id),
        "replacement is a new record"
    );
    let items = lib.collection_items(id).unwrap();
    assert_eq!(items.len(), 1, "the replacement did not join");
    assert_eq!(items[0].record, member);
    assert_eq!(names(&lib, id), ["a2.txt"]);

    // Moved into a subfolder: the member follows, still once.
    std::fs::rename(files.path().join("a2.txt"), files.path().join("sub/a2.txt")).unwrap();
    walk(&src, &lib.router()).unwrap();
    let items = lib.collection_items(id).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].record, member);
    assert!(items[0].path.path.ends_with("sub/a2.txt"));
}

#[test]
fn an_offline_source_keeps_its_members_until_they_are_removed() {
    let (files, data, lib, src) = fixture();
    let id = lib.create_collection("Picks").unwrap();
    let (a, b) = (r(&src, "a.txt"), r(&src, "b.txt"));
    lib.add_to_collection(id, &[a.clone(), b.clone()]).unwrap();
    assert_eq!(names(&lib, id), ["a.txt", "b.txt"]);

    // The root goes away (an unplugged drive): the walk stops, the source is offline,
    // and its store keeps the last generation.
    std::fs::rename(files.path(), data.path().join("unplugged")).unwrap();
    walk(&src, &lib.router()).unwrap_err();
    assert!(matches!(
        *src.status.read(),
        SourceStatus::Offline {
            reason: OfflineReason::Unreachable,
            ..
        }
    ));

    // Reading the collection does not drop the offline source's membership.
    assert_eq!(names(&lib, id), ["a.txt", "b.txt"]);
    assert_eq!(
        lib.collections().unwrap()[0].items,
        2,
        "reading does not delete membership"
    );

    // Explicit removal still works while the source is offline.
    assert_eq!(
        lib.remove_from_collection(id, std::slice::from_ref(&a))
            .unwrap(),
        1
    );
    assert_eq!(names(&lib, id), ["b.txt"]);

    // Reconnected: the walk succeeds and the remaining member resolves again.
    std::fs::rename(data.path().join("unplugged"), files.path()).unwrap();
    walk(&src, &lib.router()).unwrap();
    assert!(matches!(*src.status.read(), SourceStatus::Online { .. }));
    assert_eq!(names(&lib, id), ["b.txt"]);
}

#[test]
fn repeated_additions_stay_one_membership_and_same_names_stay_separate() {
    let (files, _data, lib, src) = fixture();
    let id = lib.create_collection("Picks").unwrap();
    // Two distinct files share the name twin.txt: two records.
    write(&files.path().join("twin.txt"), "root");
    write(&files.path().join("sub/twin.txt"), "sub");
    walk(&src, &lib.router()).unwrap();
    let root = r(&src, "twin.txt");
    let sub = r(&src, "sub/twin.txt");
    assert_ne!(root, sub, "same name, distinct files");

    assert_eq!(
        lib.add_to_collection(id, &[root.clone(), sub.clone()])
            .unwrap(),
        2
    );
    assert_eq!(
        lib.add_to_collection(id, &[root.clone(), root.clone()])
            .unwrap(),
        0,
        "repeats, twice in one call"
    );
    assert_eq!(
        lib.add_to_collection(id, std::slice::from_ref(&sub))
            .unwrap(),
        0
    );
    assert_eq!(lib.collections().unwrap()[0].items, 2, "accurate count");
    let items = lib.collection_items(id).unwrap();
    assert_eq!(items.len(), 2, "both same-named members kept");
    assert!(items.iter().any(|h| h.record == root));
    assert!(items.iter().any(|h| h.record == sub));

    // Through a supported alias: the rename resolves to the same record, and
    // re-adding through the new path adds nothing.
    std::fs::rename(
        files.path().join("twin.txt"),
        files.path().join("twin2.txt"),
    )
    .unwrap();
    walk(&src, &lib.router()).unwrap();
    assert_eq!(
        id_of(&src, "twin2.txt"),
        Some(root.id),
        "rename is an alias"
    );
    assert_eq!(
        lib.add_to_collection(id, std::slice::from_ref(&root))
            .unwrap(),
        0
    );
    assert_eq!(lib.collections().unwrap()[0].items, 2);
    let items = lib.collection_items(id).unwrap();
    assert_eq!(items.len(), 2);
    assert!(
        items.iter().any(|h| h.record == root),
        "the member followed its rename"
    );
}
