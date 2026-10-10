use super::*;
use crate::index::tests::{id_of, walk, write};
use crate::library::tests::folder;
use crate::{JobStatus, SourceDef, SourceKind};
use keel_vfs::VPath;

/// A device with a library of one folder source holding `files` (name, contents), hashed.
struct Device {
    _files: tempfile::TempDir,
    _data: tempfile::TempDir,
    id: &'static str,
    lib: Library,
    src: Arc<Source>,
}

fn device(id: &'static str, files: &[(&str, &str)]) -> Device {
    let dir = tempfile::tempdir().unwrap();
    for (name, data) in files {
        write(&dir.path().join(name), data);
    }
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "t").unwrap();
    lib.set_sync_device(id).unwrap();
    let src = lib
        .source(&lib.add_source(folder("F", dir.path())).unwrap())
        .unwrap();
    walk(&src, &lib.router()).unwrap();
    Device {
        _files: dir,
        _data: data,
        id,
        lib,
        src,
    }
}

impl Device {
    fn hash(&self) {
        let info = self.lib.jobs().wait(self.lib.hash().unwrap()).unwrap();
        assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    }

    fn rec(&self, rel: &str) -> RecordRef {
        RecordRef {
            source: self.src.id.clone(),
            id: id_of(&self.src, rel).unwrap(),
        }
    }

    fn tag(&self, name: &str) -> Option<crate::Tag> {
        self.lib
            .tags()
            .unwrap()
            .into_iter()
            .find(|t| t.name == name)
    }

    /// Pulls everything `from` logged since the last pull, page by page.
    fn pull(&self, from: &Device) -> SyncApplied {
        let mut total = SyncApplied::default();
        loop {
            let since = self.lib.sync_since(from.id).unwrap();
            let page = from.lib.sync_page(since, SYNC_PAGE).unwrap();
            let done = self.lib.sync_apply(from.id, &page).unwrap();
            total.applied += done.applied;
            total.stale += done.stale;
            total.rejected += done.rejected;
            if !page.more && !done.restart {
                return total;
            }
        }
    }

    fn uid(&self, tag: TagId) -> String {
        self.lib.tag_uid(tag).unwrap()
    }
}

fn names(tags: &[TagId], lib: &Library) -> Vec<String> {
    let all = lib.tags().unwrap();
    tags.iter()
        .filter_map(|id| all.iter().find(|t| t.id == *id).map(|t| t.name.clone()))
        .collect()
}

fn page_of(entries: Vec<SyncEntry>) -> SyncPage {
    let upto = entries.iter().map(|e| e.seq).max().unwrap_or(0);
    SyncPage {
        entries,
        more: false,
        upto,
        epoch: String::new(),
    }
}

fn entry(seq: u64, lamport: u64, op: SyncOp) -> SyncEntry {
    SyncEntry {
        seq,
        device: String::new(),
        lamport,
        op,
    }
}

/// A source of `device`'s folder `source` (a `node://` source), with the files at `paths`
/// recorded as if walked.
fn device_source(lib: &Library, device: &str, source: &str, paths: &[&str]) -> Arc<Source> {
    let def = SourceDef {
        label: "Remote".into(),
        root: VPath::parse(&format!("node://{device}/{source}")).unwrap(),
        kind: SourceKind::Device,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
        hash_shares: false,
    };
    let src = lib.source(&lib.add_source(def).unwrap()).unwrap();
    let c = src.store.get().unwrap();
    c.execute(
        "INSERT INTO record(parent, name, path, kind, fs_id, gen) VALUES (NULL, '', '', 1, 'h:root', 1)",
        [],
    )
    .unwrap();
    let root = c.last_insert_rowid();
    for p in paths {
        let parts: Vec<&str> = p.split('/').collect();
        let mut parent = root;
        for (i, name) in parts.iter().enumerate() {
            let path = parts[..=i].join("/");
            let kind = if i + 1 == parts.len() { 0 } else { 1 };
            let found: Option<i64> = c
                .query_row(
                    "SELECT id FROM record WHERE parent = ?1 AND name = ?2",
                    params![parent, name],
                    |r| r.get(0),
                )
                .optional()
                .unwrap();
            parent = match found {
                Some(id) => id,
                None => {
                    c.execute(
                        "INSERT INTO record(parent, name, path, kind, fs_id, gen)
                         VALUES (?1, ?2, ?3, ?4, ?5, 1)",
                        params![parent, name, path, kind, format!("h:{path}")],
                    )
                    .unwrap();
                    c.last_insert_rowid()
                }
            };
        }
    }
    drop(c);
    src
}

#[test]
fn local_edits_are_logged_in_order_and_superseded_entries_pruned() {
    let a = device("deva", &[("a.txt", "alpha")]);
    let work = a.lib.create_tag("Work", None, None).unwrap();
    a.lib.recolor_tag(work, Some("#e5484d")).unwrap();
    a.lib.set_tag(work, &[a.rec("a.txt")], true).unwrap();
    // Putting a tag on twice changes nothing and logs nothing.
    a.lib.set_tag(work, &[a.rec("a.txt")], true).unwrap();
    let page = a.lib.sync_page(0, SYNC_PAGE).unwrap();
    assert_eq!(page.entries.len(), 3, "{page:?}");
    assert!(!page.more);
    assert!(page
        .entries
        .windows(2)
        .all(|w| w[0].seq < w[1].seq && w[0].lamport < w[1].lamport));
    let uid = a.uid(work);
    assert_eq!(
        page.entries[1].op,
        SyncOp::Tag {
            uid: uid.clone(),
            name: "Work".into(),
            color: Some("#e5484d".into()),
            parent: None
        }
    );
    // Not hashed yet: kept by its path in this device's source.
    assert_eq!(
        page.entries[2].op,
        SyncOp::Assign {
            tag: uid,
            target: SyncTarget::Path {
                source: a.src.id.0.clone(),
                path: "a.txt".into()
            },
            on: true
        }
    );
    // Pages are bounded and resume after `upto`.
    let first = a.lib.sync_page(0, 2).unwrap();
    assert_eq!((first.entries.len(), first.more), (2, true));
    let rest = a.lib.sync_page(first.upto, 2).unwrap();
    assert_eq!((rest.entries.len(), rest.more), (1, false));
    // The created and recolored tag is one key: the older entry goes.
    a.lib.sync_prune().unwrap();
    assert_eq!(a.lib.sync_page(0, SYNC_PAGE).unwrap().entries.len(), 2);
}

#[test]
fn a_tag_on_content_follows_the_same_bytes_to_another_device() {
    let a = device(
        "deva",
        &[("x/report.pdf", "same bytes"), ("other.txt", "a only")],
    );
    let b = device(
        "devb",
        &[
            ("copy-of-report.pdf", "same bytes"),
            ("other.txt", "b only"),
        ],
    );
    a.hash();
    b.hash();
    let work = a.lib.create_tag("Work", Some("#30a46c"), None).unwrap();
    let client = a.lib.create_tag("Client", None, Some(work)).unwrap();
    a.lib
        .set_tag(client, &[a.rec("x/report.pdf")], true)
        .unwrap();
    a.lib.set_favorite(&[a.rec("x/report.pdf")], true).unwrap();

    let done = b.pull(&a);
    assert_eq!((done.applied, done.rejected), (4, 0), "{done:?}");
    let b_work = b.tag("Work").expect("tag arrived");
    let b_client = b.tag("Client").unwrap();
    assert_eq!(b_work.color.as_deref(), Some("#30a46c"));
    assert_eq!(b_client.parent, Some(b_work.id), "nesting arrived");
    assert_eq!(b.uid(b_work.id), a.uid(work), "same stable id");
    let copy = b.rec("copy-of-report.pdf");
    let mut on_copy = names(&b.lib.tags_of(&copy).unwrap(), &b.lib);
    on_copy.sort();
    assert_eq!(on_copy, ["Client"]);
    assert!(b.lib.tags_of(&copy).unwrap().contains(&FAVORITES));
    assert!(b.lib.tags_of(&b.rec("other.txt")).unwrap().is_empty());
    // Received changes are not logged again (sync is pairwise).
    assert!(b.lib.sync_page(0, SYNC_PAGE).unwrap().entries.is_empty());
    let peers = b.lib.sync_peers().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!((peers[0].device.as_str(), peers[0].received), ("deva", 4));
    assert!(peers[0].last_sync.is_some());

    // Taking it off travels too.
    a.lib
        .set_tag(client, &[a.rec("x/report.pdf")], false)
        .unwrap();
    assert_eq!(b.pull(&a).applied, 1);
    assert!(!b.lib.tags_of(&copy).unwrap().contains(&b_client.id));
}

#[test]
fn concurrent_edits_of_one_tag_converge_on_both_devices() {
    let a = device("deva", &[]);
    let b = device("devb", &[]);
    let tag = a.lib.create_tag("Shared", None, None).unwrap();
    b.pull(&a);
    let on_b = b.tag("Shared").unwrap().id;
    // Both change it before either pulls: the same Lamport time on both sides.
    a.lib.recolor_tag(tag, Some("#ff0000")).unwrap();
    b.lib.recolor_tag(on_b, Some("#0000ff")).unwrap();
    b.pull(&a);
    a.pull(&b);
    let (ca, cb) = (
        a.tag("Shared").unwrap().color,
        b.tag("Shared").unwrap().color,
    );
    assert_eq!(ca, cb, "converged");
    // Equal Lamport times: the higher device id wins.
    assert_eq!(ca.as_deref(), Some("#0000ff"));
    // A later edit wins over both.
    a.lib.rename_tag(tag, "Team").unwrap();
    b.pull(&a);
    a.pull(&b);
    assert!(a.tag("Team").is_some() && b.tag("Team").is_some());
    assert_eq!(b.tag("Team").unwrap().color.as_deref(), Some("#0000ff"));
}

#[test]
fn deletes_are_tombstones_until_they_expire() {
    let a = device("deva", &[("f.txt", "data")]);
    let b = device("devb", &[("f.txt", "data")]);
    a.hash();
    b.hash();
    let tag = a.lib.create_tag("Old", None, None).unwrap();
    a.lib.set_tag(tag, &[a.rec("f.txt")], true).unwrap();
    let before = a.lib.sync_page(0, SYNC_PAGE).unwrap();
    b.pull(&a);
    assert!(b.tag("Old").is_some());
    a.lib.delete_tag(tag).unwrap();
    assert_eq!(b.pull(&a).applied, 1);
    assert!(b.tag("Old").is_none(), "deleted on b");
    assert!(b.lib.tags_of(&b.rec("f.txt")).unwrap().is_empty());
    // The old entries again (a slow relay, a replay): the tombstone wins.
    let replay = SyncPage {
        upto: 0,
        ..before.clone()
    };
    let done = b.lib.sync_apply("deva", &replay).unwrap();
    assert_eq!(done.applied, 0, "{done:?}");
    assert!(b.tag("Old").is_none());
    // The tombstone is kept for 30 days, then dropped (with a's log entry).
    b.lib.sync_prune().unwrap();
    let tombstones = |lib: &Library| -> i64 {
        lib.shared
            .db
            .get()
            .unwrap()
            .query_row("SELECT count(*) FROM sync_key WHERE live = 0", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    assert_eq!(tombstones(&b.lib), 1);
    let later = crate::now() + TOMBSTONE_SECS + 1;
    b.lib.sync_prune_before(later).unwrap();
    a.lib.sync_prune_before(later).unwrap();
    assert_eq!(tombstones(&b.lib), 0);
    assert!(a.lib.sync_page(0, SYNC_PAGE).unwrap().entries.is_empty());
}

#[test]
fn applying_a_page_again_changes_nothing() {
    let a = device("deva", &[("f.txt", "data")]);
    let b = device("devb", &[("f.txt", "data")]);
    a.hash();
    b.hash();
    let tag = a.lib.create_tag("T", None, None).unwrap();
    a.lib.set_tag(tag, &[a.rec("f.txt")], true).unwrap();
    let page = a.lib.sync_page(0, SYNC_PAGE).unwrap();
    assert_eq!(b.lib.sync_apply("deva", &page).unwrap().applied, 2);
    let tags = b.lib.tags().unwrap();
    let again = b.lib.sync_apply("deva", &page).unwrap();
    assert_eq!((again.applied, again.stale), (0, 2));
    assert_eq!(b.lib.tags().unwrap(), tags);
    assert_eq!(b.lib.tags_of(&b.rec("f.txt")).unwrap().len(), 1);
}

#[test]
fn path_targets_land_only_in_that_devices_sources() {
    let a = device("deva", &[("docs/plan.txt", "plan v1")]);
    // b has a local file of the same name and a source of a's folder.
    let b = device("devb", &[("docs/plan.txt", "different")]);
    let tag = a.lib.create_tag("Plans", None, None).unwrap();
    a.lib.set_tag(tag, &[a.rec("docs/plan.txt")], true).unwrap();
    b.pull(&a);
    let plans = b.tag("Plans").unwrap().id;
    assert!(b.lib.tags_of(&b.rec("docs/plan.txt")).unwrap().is_empty());
    // a's folder added on b later (and another device's folder of the same source id):
    // the assignment waits, then lands in a's folder only.
    let theirs = device_source(&b.lib, "deva", &a.src.id.0, &["docs/plan.txt"]);
    let elsewhere = device_source(&b.lib, "devc", &a.src.id.0, &["docs/plan.txt"]);
    b.lib.sync_reapply().unwrap();
    let rec = |s: &Source| RecordRef {
        source: s.id.clone(),
        id: id_of(s, "docs/plan.txt").unwrap(),
    };
    assert_eq!(b.lib.tags_of(&rec(&theirs)).unwrap(), [plans]);
    assert!(b.lib.tags_of(&rec(&elsewhere)).unwrap().is_empty());
    assert!(b.lib.tags_of(&b.rec("docs/plan.txt")).unwrap().is_empty());

    // A peer cannot name another device's paths: the target is always in its own
    // namespace, whatever the entry says it is from.
    let forged = page_of(vec![entry(
        1,
        100,
        SyncOp::Assign {
            tag: FAVORITES_UID.into(),
            target: SyncTarget::Path {
                source: b.src.id.0.clone(),
                path: "docs/plan.txt".into(),
            },
            on: true,
        },
    )]);
    b.lib.sync_apply("devc", &forged).unwrap();
    assert!(b.lib.tags_of(&b.rec("docs/plan.txt")).unwrap().is_empty());
    assert!(b.lib.favorites().unwrap().is_empty());
}

#[test]
fn a_path_tag_reaches_copies_once_the_file_is_hashed() {
    let a = device("deva", &[("song.flac", "the same song")]);
    let b = device("devb", &[("music/song.flac", "the same song")]);
    b.hash();
    let tag = a.lib.create_tag("Loved", None, None).unwrap();
    a.lib.set_tag(tag, &[a.rec("song.flac")], true).unwrap();
    b.pull(&a);
    let loved = b.tag("Loved").unwrap().id;
    assert!(b.lib.tags_of(&b.rec("music/song.flac")).unwrap().is_empty());
    // a hashes the file: its next page says what the content is.
    a.hash();
    let done = b.pull(&a);
    assert_eq!(done.applied, 1, "the content id: {done:?}");
    assert_eq!(b.lib.tags_of(&b.rec("music/song.flac")).unwrap(), [loved]);
}

#[test]
fn malformed_entries_are_refused() {
    let b = device("devb", &[("f.txt", "x")]);
    let tag = |name: &str| SyncOp::Tag {
        uid: "0123456789abcdef".into(),
        name: name.into(),
        color: None,
        parent: None,
    };
    let bad = vec![
        entry(1, 1, tag(" padded")),
        entry(2, 2, tag(&"x".repeat(300))),
        entry(3, 3, tag("bidi\u{202E}name")),
        entry(4, MAX_LAMPORT + 1, tag("late")),
        entry(
            5,
            5,
            SyncOp::Tag {
                uid: FAVORITES_UID.into(),
                name: "Mine".into(),
                color: None,
                parent: None,
            },
        ),
        entry(
            6,
            6,
            SyncOp::Assign {
                tag: "0123456789abcdef".into(),
                target: SyncTarget::Path {
                    source: "src".into(),
                    path: "../outside".into(),
                },
                on: true,
            },
        ),
        entry(
            7,
            7,
            SyncOp::Assign {
                tag: "0123456789abcdef".into(),
                target: SyncTarget::Content("not hex".into()),
                on: true,
            },
        ),
    ];
    let done = b.lib.sync_apply("devc", &page_of(bad)).unwrap();
    assert_eq!((done.applied, done.rejected), (0, 7));
    assert!(b.lib.tags().unwrap().is_empty());
    // The position still moves on: a refused entry is not asked for again.
    assert_eq!(b.lib.sync_since("devc").unwrap(), 7);
    let too_many = page_of(
        (1..=SYNC_PAGE as u64 + 1)
            .map(|i| entry(i, i, tag("t")))
            .collect(),
    );
    assert!(b.lib.sync_apply("devc", &too_many).is_err());
    assert!(
        b.lib.sync_apply("devb", &page_of(Vec::new())).is_err(),
        "itself"
    );
    assert!(b.lib.sync_apply("bad id!", &page_of(Vec::new())).is_err());
}

#[test]
fn the_same_tag_made_on_both_devices_is_one_tag() {
    let a = device("deva", &[("f.txt", "shared")]);
    let b = device("devb", &[("g.txt", "shared")]);
    a.hash();
    b.hash();
    let on_a = a.lib.create_tag("Taxes", None, None).unwrap();
    let on_b = b.lib.create_tag("taxes", None, None).unwrap();
    a.lib.set_tag(on_a, &[a.rec("f.txt")], true).unwrap();
    b.pull(&a);
    a.pull(&b);
    assert_eq!(b.lib.tags().unwrap().len(), 1, "{:?}", b.lib.tags());
    assert_eq!(a.lib.tags().unwrap().len(), 1, "{:?}", a.lib.tags());
    assert_eq!(b.lib.tags_of(&b.rec("g.txt")).unwrap(), [on_b]);
}

#[test]
fn tags_and_favorites_from_before_sync_are_logged_once() {
    let a = device("deva", &[("f.txt", "x")]);
    let tag = a.lib.create_tag("Before", None, None).unwrap();
    a.lib.set_tag(tag, &[a.rec("f.txt")], true).unwrap();
    a.lib.set_favorite(&[a.rec("f.txt")], true).unwrap();
    // As a library from before sync: nothing logged, not seeded.
    {
        let c = a.lib.shared.db.get().unwrap();
        c.execute_batch(
            "DELETE FROM sync_log; DELETE FROM sync_key; DELETE FROM meta WHERE key = 'sync_seeded';",
        )
        .unwrap();
    }
    a.lib.sync_seed().unwrap();
    a.lib.sync_seed().unwrap();
    let page = a.lib.sync_page(0, SYNC_PAGE).unwrap();
    assert_eq!(page.entries.len(), 3, "{page:?}");
    assert!(matches!(page.entries[0].op, SyncOp::Tag { .. }));
}

#[test]
fn a_huge_lamport_time_cannot_push_this_devices_changes_out_of_range() {
    let a = device("deva", &[]);
    let c = device("devc", &[]);
    let huge = page_of(vec![entry(
        1,
        MAX_LAMPORT,
        SyncOp::Tag {
            uid: "0123456789abcdef".into(),
            name: "Pushed".into(),
            color: None,
            parent: None,
        },
    )]);
    assert_eq!(a.lib.sync_apply("devb", &huge).unwrap().applied, 1);
    a.lib.create_tag("Mine", None, None).unwrap();
    let page = a.lib.sync_page(0, SYNC_PAGE).unwrap();
    assert!(page.entries.iter().all(|e| e.lamport <= MAX_LAMPORT));
    // Another device still takes a's changes.
    assert_eq!(c.pull(&a).applied, 1);
    assert!(c.tag("Mine").is_some());
}

#[test]
fn a_merged_tag_edited_on_both_devices_converges() {
    let a = device("deva", &[]);
    let b = device("devb", &[]);
    let ta = a.lib.create_tag("Work", None, None).unwrap();
    let tb = b.lib.create_tag("Work", None, None).unwrap();
    b.pull(&a);
    a.pull(&b);
    assert_eq!(a.lib.tags().unwrap().len(), 1);
    assert_eq!(b.lib.tags().unwrap().len(), 1);
    a.lib.recolor_tag(ta, Some("#ff0000")).unwrap();
    b.lib.recolor_tag(tb, Some("#0000ff")).unwrap();
    b.pull(&a);
    a.pull(&b);
    let ca = a.lib.tags().unwrap()[0].color.clone();
    let cb = b.lib.tags().unwrap()[0].color.clone();
    a.lib.rename_tag(ta, "Job").unwrap();
    b.lib.rename_tag(tb, "Office").unwrap();
    b.pull(&a);
    a.pull(&b);
    let na = a.lib.tags().unwrap()[0].name.clone();
    let nb = b.lib.tags().unwrap()[0].name.clone();
    assert_eq!((ca, na), (cb, nb), "merged tag diverged");
}

#[test]
fn a_tag_taken_off_a_local_copy_stays_off() {
    let a = device("deva", &[("song.flac", "same")]);
    let b = device("devb", &[("music/song.flac", "same")]);
    b.hash();
    let tag = a.lib.create_tag("Loved", None, None).unwrap();
    a.lib.set_tag(tag, &[a.rec("song.flac")], true).unwrap();
    a.hash();
    b.pull(&a);
    let loved = b.tag("Loved").unwrap().id;
    assert_eq!(b.lib.tags_of(&b.rec("music/song.flac")).unwrap(), [loved]);
    b.lib
        .set_tag(loved, &[b.rec("music/song.flac")], false)
        .unwrap();
    assert!(b.lib.tags_of(&b.rec("music/song.flac")).unwrap().is_empty());
    a.lib.create_tag("Other", None, None).unwrap();
    b.pull(&a);
    let after = names(&b.lib.tags_of(&b.rec("music/song.flac")).unwrap(), &b.lib);
    assert!(after.is_empty(), "the tag came back on the untagged copy");
}

#[test]
fn a_tag_taken_off_after_hashing_reaches_the_copy() {
    let a = device("deva", &[("song.flac", "same")]);
    let b = device("devb", &[("music/song.flac", "same")]);
    b.hash();
    let tag = a.lib.create_tag("Loved", None, None).unwrap();
    a.lib.set_tag(tag, &[a.rec("song.flac")], true).unwrap();
    a.hash();
    b.pull(&a);
    let loved = b.tag("Loved").unwrap().id;
    assert_eq!(b.lib.tags_of(&b.rec("music/song.flac")).unwrap(), [loved]);
    a.lib.set_tag(tag, &[a.rec("song.flac")], false).unwrap();
    assert!(a.lib.tags_of(&a.rec("song.flac")).unwrap().is_empty());
    assert_eq!(b.pull(&a).applied, 2, "the content and path entries");
    let after = names(&b.lib.tags_of(&b.rec("music/song.flac")).unwrap(), &b.lib);
    assert!(
        after.is_empty(),
        "removal on the origin did not reach the copy"
    );
}

#[test]
fn a_merged_tag_deleted_elsewhere_is_not_served_on() {
    let a = device("deva", &[]);
    let b = device("devb", &[]);
    let d = device("devd", &[]);
    a.lib.create_tag("Work", None, None).unwrap();
    let tb = b.lib.create_tag("Work", None, None).unwrap();
    a.pull(&b);
    b.lib.delete_tag(tb).unwrap();
    a.pull(&b);
    assert!(a.tag("Work").is_none(), "deleted on a");
    d.pull(&a);
    assert!(d.tag("Work").is_none(), "a serves a tag it no longer has");
}

#[test]
fn a_delete_against_a_later_rename_keeps_assignments_alike() {
    let a = device("deva", &[("f.txt", "same")]);
    let b = device("devb", &[("f.txt", "same")]);
    a.hash();
    b.hash();
    let t = b.lib.create_tag("T", None, None).unwrap();
    b.lib.set_tag(t, &[b.rec("f.txt")], true).unwrap();
    a.pull(&b);
    let on_a = a.tag("T").unwrap().id;
    assert_eq!(a.lib.tags_of(&a.rec("f.txt")).unwrap(), [on_a]);
    // Concurrently: a deletes the tag, b renames it twice (a later Lamport time).
    a.lib.delete_tag(on_a).unwrap();
    b.lib.rename_tag(t, "T2").unwrap();
    b.lib.rename_tag(t, "T3").unwrap();
    a.pull(&b);
    b.pull(&a);
    let ta = a
        .tag("T3")
        .map(|x| a.lib.tags_of(&a.rec("f.txt")).unwrap().contains(&x.id));
    let tb = b
        .tag("T3")
        .map(|x| b.lib.tags_of(&b.rec("f.txt")).unwrap().contains(&x.id));
    assert_eq!(ta, tb, "devices disagree after delete vs rename");
    assert_eq!(
        ta,
        Some(true),
        "the later rename brings the tag back, on the file"
    );
}

#[test]
fn a_device_whose_library_was_made_again_is_read_from_the_start() {
    let b = device("devb", &[]);
    {
        let a = device("deva", &[]);
        for name in ["One", "Two", "Three"] {
            a.lib.create_tag(name, None, None).unwrap();
        }
        b.pull(&a);
        assert_eq!(b.lib.sync_since("deva").unwrap(), 3);
    }
    // The same device id with a new library: its log starts over at 1.
    let a = device("deva", &[]);
    a.lib.create_tag("Fresh", None, None).unwrap();
    let page = a.lib.sync_page(3, SYNC_PAGE).unwrap();
    assert!(page.entries.is_empty() && !page.epoch.is_empty());
    let done = b.lib.sync_apply("deva", &page).unwrap();
    assert!(done.restart, "{done:?}");
    assert_eq!(b.lib.sync_since("deva").unwrap(), 0);
    b.pull(&a);
    assert!(b.tag("Fresh").is_some());
    // An unchanged log is read on from where it was.
    assert!(
        !b.lib
            .sync_apply("deva", &a.lib.sync_page(1, SYNC_PAGE).unwrap())
            .unwrap()
            .restart
    );
}

#[test]
fn a_large_library_from_before_sync_is_logged_in_chunks() {
    let a = device("deva", &[]);
    let tag = a.lib.create_tag("Many", None, None).unwrap();
    const N: i64 = 3_000;
    {
        // N records tagged, as a library from before sync (nothing logged, not seeded).
        let mut c = a.src.store.get().unwrap();
        let tx = c.transaction().unwrap();
        let root: i64 = tx
            .query_row("SELECT id FROM record WHERE parent IS NULL", [], |r| {
                r.get(0)
            })
            .unwrap();
        for i in 0..N {
            tx.execute(
                "INSERT INTO record(parent, name, path, kind, fs_id, gen) VALUES (?1, ?2, ?2, 0, ?3, 1)",
                params![root, format!("f{i}.txt"), format!("h:{i}")],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO record_tag(record, tag) VALUES (?1, ?2)",
                params![tx.last_insert_rowid(), tag],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        a.lib
            .shared
            .db
            .get()
            .unwrap()
            .execute_batch(
                "DELETE FROM sync_log; DELETE FROM sync_key; DELETE FROM meta WHERE key = 'sync_seeded';",
            )
            .unwrap();
    }
    a.lib.sync_seed_in(1_000).unwrap();
    let (rows, keys): (i64, i64) = a
        .lib
        .shared
        .db
        .get()
        .unwrap()
        .query_row(
            "SELECT count(*), count(DISTINCT key) FROM sync_log",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    // The tag and every assignment, once each.
    assert_eq!((rows, keys), (N + 1, N + 1));
}

#[test]
fn tombstones_expire_while_the_library_stays_open() {
    let a = device("deva", &[]);
    let tag = a.lib.create_tag("Gone", None, None).unwrap();
    a.lib.delete_tag(tag).unwrap();
    let c = a.lib.shared.db.get().unwrap();
    let old = crate::now() - TOMBSTONE_SECS - 10;
    c.execute("UPDATE sync_key SET ts = ?1 WHERE live = 0", [old])
        .unwrap();
    c.execute("UPDATE sync_log SET ts = ?1 WHERE live = 0", [old])
        .unwrap();
    let count = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT count(*) FROM sync_key WHERE live = 0", [], |r| {
            r.get(0)
        })
        .unwrap()
    };
    // Pruned at open a moment ago: not again yet.
    a.lib.sync_page(0, SYNC_PAGE).unwrap();
    assert_eq!(count(&c), 1);
    // A day later, the next page served lets it go.
    c.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'sync_pruned'",
        [(crate::now() - PRUNE_SECS).to_string()],
    )
    .unwrap();
    a.lib.sync_page(0, SYNC_PAGE).unwrap();
    assert_eq!(count(&c), 0);
}
