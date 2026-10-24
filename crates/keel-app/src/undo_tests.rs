use super::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn p(s: &str) -> VPath {
    VPath::local(PathBuf::from(format!("/data/{s}")))
}

fn st(n: u64) -> Stamp {
    Stamp::new(n, None)
}

fn item(now: &str, was: Option<&str>, stamp: u64) -> Item {
    Item {
        now: p(now),
        was: was.map(p),
        stamp: Some(st(stamp)),
    }
}

fn entry(op: Op, items: Vec<Item>) -> Entry {
    Entry { id: 1, op, items }
}

/// A fake disk: path -> stamp.
fn disk(files: &[(&str, u64)]) -> impl Fn(&VPath) -> Option<Stamp> {
    let mut map: HashMap<VPath, Stamp> = files.iter().map(|(n, s)| (p(n), st(*s))).collect();
    map.insert(VPath::local(PathBuf::from("/data")), st(0));
    for dir in ["a", "b"] {
        map.insert(p(dir), st(0));
    }
    move |x: &VPath| map.get(x).copied()
}

/// The palette's "Undo <what>" and the refused plain label in Spanish.
#[test]
fn undo_labels_in_spanish() {
    crate::i18n::set("es");
    let cases = [
        (Op::Rename(Vec::new()), "Deshacer renombrar"),
        (Op::Move, "Deshacer movimiento"),
        (Op::Copy, "Deshacer copia"),
        (Op::Create(true), "Deshacer nueva carpeta"),
        (Op::Create(false), "Deshacer nuevo archivo"),
        (Op::Trash, "Deshacer eliminación"),
        (
            Op::Arrange {
                what: Arranged::Merge,
                made: vec![],
                removed: vec![],
                merged: 0,
            },
            "Deshacer fusión",
        ),
        (
            Op::Arrange {
                what: Arranged::Flatten,
                made: vec![],
                removed: vec![],
                merged: 0,
            },
            "Deshacer aplanar",
        ),
        (
            Op::Arrange {
                what: Arranged::Organize,
                made: vec![],
                removed: vec![],
                merged: 0,
            },
            "Deshacer organizar",
        ),
        (Op::Attributes(Vec::new()), "Deshacer atributos"),
        (Op::Link, "Deshacer enlace"),
        (Op::Hardlink(Vec::new()), "Deshacer enlaces duros"),
        (Op::FolderDates(Vec::new()), "Deshacer fechas de carpetas"),
    ];
    for (op, expected) in cases {
        let mut j = Journal::default();
        j.record(op, vec![item("a/x", Some("a/y"), 1)]);
        assert_eq!(j.label().as_deref(), Some(expected));
    }
    let mut j = Journal::default();
    j.refuse("deleted permanently");
    assert_eq!(j.label().as_deref(), Some("Deshacer"));
    assert_eq!(
        j.next().unwrap_err(),
        "No se puede deshacer: deleted permanently"
    );
    let mut empty = Journal::default();
    assert_eq!(empty.next().unwrap_err(), "Nada que deshacer");
    crate::i18n::set("en");
}

#[test]
fn journal_keeps_twenty_and_says_a_refusal_once() {
    let mut j = Journal::default();
    assert_eq!(j.next(), Err("Nothing to undo".to_owned()));
    assert!(j.record(Op::Copy, Vec::new()).is_none(), "nothing to undo");
    for i in 0..25 {
        j.record(Op::Copy, vec![item(&format!("a/{i}"), None, 1)]);
    }
    assert_eq!(j.entries.len(), KEEP);
    let newest = j.next().unwrap();
    assert_eq!(j.get(newest).unwrap().items[0].now, p("a/24"));
    assert_eq!(j.label().as_deref(), Some("Undo copy"));
    j.refuse("deleted permanently");
    assert_eq!(j.label().as_deref(), Some("Undo"));
    assert_eq!(j.next(), Err("Can't undo: deleted permanently".to_owned()));
    assert_eq!(j.next(), Ok(newest), "then the entry before");
    // A newer recorded operation clears the refusal.
    j.refuse("deleted permanently");
    j.record(Op::Move, vec![item("b/x", Some("a/x"), 1)]);
    assert_eq!(j.label().as_deref(), Some("Undo move"));
    let id = j.next().unwrap();
    assert!(j.take(id).is_some() && j.take(id).is_none());
    assert_eq!(j.next(), Ok(newest));
    j.clear();
    assert_eq!(j.next(), Err("Nothing to undo".to_owned()));
    let id = j.record(Op::Copy, vec![item("a/1", None, 1)]).unwrap().id;
    assert!(id > newest, "ids never repeat");
}

#[test]
fn renamed_follows_temporary_names() {
    // A swap through temporary names, as bulk rename runs it.
    let done = [
        (p("a/x"), p("a/x.keel-tmp-0")),
        (p("a/y"), p("a/y.keel-tmp-1")),
        (p("a/x.keel-tmp-0"), p("a/y")),
        (p("a/y.keel-tmp-1"), p("a/x")),
    ];
    let items = renamed(&done, disk(&[("a/x", 2), ("a/y", 1)]));
    assert_eq!(
        items,
        [item("a/y", Some("a/x"), 1), item("a/x", Some("a/y"), 2)]
    );
    // A temporary name that went back to the old one leaves nothing.
    let done = [(p("a/x"), p("a/t")), (p("a/t"), p("a/x"))];
    assert!(renamed(&done, disk(&[("a/x", 1)])).is_empty());
}

#[test]
fn landed_keeps_what_the_transfer_made_or_moved() {
    let l = |s: &str, t: &str, new, complete| Landed {
        source: p(s),
        target: p(t),
        new,
        complete,
    };
    let landed_now = [
        l("a/new", "b/new", true, true),
        l("a/merged", "b/merged", false, true),
        l("a/half", "b/half", true, false),
    ];
    let fs = disk(&[("b/new", 1), ("b/merged", 2), ("b/half", 3), ("a/half", 3)]);
    // A copy: what it made, a partly copied folder too (the part that was done).
    assert_eq!(
        landed(&landed_now, false, &fs),
        [item("b/new", None, 1), item("b/half", None, 3)]
    );
    // A move: only sources that went completely.
    assert_eq!(
        landed(&landed_now, true, &fs),
        [item("b/new", Some("a/new"), 1)]
    );
}

#[test]
fn check_refuses_gone_changed_and_taken_items() {
    let mv = entry(
        Op::Move,
        vec![item("b/x", Some("a/x"), 5), item("b/y", Some("a/y"), 6)],
    );
    assert_eq!(mv.check(disk(&[("b/x", 5), ("b/y", 6)])), Ok(()));
    let err = |e: &Entry, files: &[(&str, u64)]| e.check(disk(files)).unwrap_err().why;
    assert_eq!(err(&mv, &[("b/x", 5)]), "y is gone");
    assert_eq!(err(&mv, &[("b/x", 5), ("b/y", 7)]), "y changed since");
    assert!(err(&mv, &[("b/x", 5), ("b/y", 6), ("a/y", 1)]).ends_with("y is taken"));
    let lost = entry(Op::Move, vec![item("b/x", Some("gone/x"), 5)]);
    assert!(err(&lost, &[("b/x", 5)]).ends_with("gone is gone"));
    // Copies and new items: only their own state.
    let copy = entry(Op::Copy, vec![item("b/x", None, 5)]);
    assert_eq!(copy.check(disk(&[("b/x", 5), ("a/x", 5)])), Ok(()));
    assert_eq!(err(&copy, &[("b/x", 9)]), "x changed since");
    let new = entry(Op::Create(true), vec![item("a/New folder", None, 0)]);
    assert_eq!(err(&new, &[]), "New folder is gone");
    // A swap frees its own places; a rename back onto a new item does not.
    let swap = entry(
        Op::Rename(Vec::new()),
        vec![item("a/y", Some("a/x"), 1), item("a/x", Some("a/y"), 2)],
    );
    assert_eq!(swap.check(disk(&[("a/x", 2), ("a/y", 1)])), Ok(()));
    let ren = entry(Op::Rename(Vec::new()), vec![item("a/y", Some("a/x"), 1)]);
    assert!(err(&ren, &[("a/y", 1), ("a/x", 3)]).ends_with("x is taken"));
    // Deleted items: still in the bin, old place free.
    let bin = |n: &str| VPath {
        scheme: keel_vfs::trashbin::SCHEME.into(),
        authority: "k1".into(),
        path: format!("/{n}"),
    };
    let del = entry(
        Op::Trash,
        vec![Item {
            now: bin("x"),
            was: Some(p("a/x")),
            stamp: None,
        }],
    );
    let fs = |taken: bool, listed: bool| {
        let bin = bin("x");
        move |x: &VPath| ((*x == bin && listed) || (*x == p("a/x") && taken)).then_some(st(0))
    };
    assert_eq!(del.check(fs(false, true)), Ok(()));
    let gone = del.check(fs(false, false)).unwrap_err();
    assert!(gone.why.contains("no longer in the") && !gone.transient);
    let taken = del.check(fs(true, true)).unwrap_err();
    assert!(taken.why.ends_with("x is taken") && taken.transient);
    let none = entry(Op::Copy, Vec::new());
    assert!(none.check(disk(&[])).is_err());
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn a_case_only_rename_frees_its_own_place() {
    let ren = entry(
        Op::Rename(Vec::new()),
        vec![item("a/X.txt", Some("a/x.txt"), 1)],
    );
    // Case-insensitive: the old name finds the same file.
    assert_eq!(ren.check(disk(&[("a/X.txt", 1), ("a/x.txt", 1)])), Ok(()));
}

#[test]
fn texts_say_what_and_where() {
    let mv = entry(
        Op::Move,
        vec![item("b/x", Some("a/x"), 1), item("b/y", Some("a/y"), 1)],
    );
    let a = p("a").display();
    assert_eq!(mv.preview(), format!("Undo move: 2 items back to {a}?"));
    assert_eq!(
        mv.done_text(),
        format!("Moved 2 items to {}", p("b").display())
    );
    let copy = entry(Op::Copy, vec![item("b/x", None, 1)]);
    assert_eq!(
        copy.preview(),
        format!(
            "Undo copy: move the copy in {} to the trash?",
            p("b").display()
        )
    );
    let new = entry(Op::Create(false), vec![item("a/n.txt", None, 1)]);
    assert_eq!(new.preview(), "Undo new file: move n.txt to the trash?");
    assert_eq!(new.done_text(), "Created n.txt");
    let ren = entry(Op::Rename(Vec::new()), vec![item("a/y", Some("a/x"), 1)]);
    assert_eq!(ren.done_text(), "Renamed x to y");
    let del = entry(Op::Trash, vec![item("t/x", Some("a/x"), 1)]);
    assert_eq!(
        del.preview(),
        format!("Undo delete: restore 1 item to {a}?")
    );
    assert_eq!(del.done_text(), "Moved 1 item to the trash");
}

#[test]
fn refusals_name_what_cannot_be_undone() {
    use crate::jobs::{ArchiveSrc, Transfer};
    let t = |src: VPath, dst: VPath| Transfer {
        src: vec![src],
        dst,
        mv: false,
        extract: None,
    };
    assert_eq!(transfer_refusal(&t(p("a/x"), p("b")), Conflict::Skip), None);
    assert_eq!(
        transfer_refusal(&t(p("a/x"), p("b")), Conflict::RenameNew),
        None
    );
    assert!(transfer_refusal(&t(p("a/x"), p("b")), Conflict::Overwrite)
        .unwrap()
        .contains("Overwrite"));
    let zip = VPath::join_archive(&p("a/z.zip"), "in");
    assert_eq!(
        transfer_refusal(&t(p("a/x"), zip.clone()), Conflict::Skip),
        Some("changes inside archives")
    );
    let host = VPath {
        scheme: "sftp".into(),
        authority: "box".into(),
        path: "/x".into(),
    };
    assert!(transfer_refusal(&t(host.clone(), p("b")), Conflict::Skip)
        .unwrap()
        .contains("remote"));
    let node = VPath {
        scheme: "node".into(),
        ..host.clone()
    };
    assert_eq!(
        transfer_refusal(&t(node.clone(), p("b")), Conflict::Skip),
        Some("files on another device")
    );
    let mut extract = t(p("a/x"), p("b"));
    extract.extract = Some(ArchiveSrc::whole(p("a/z.zip")));
    assert!(transfer_refusal(&extract, Conflict::Skip).is_some());
    assert_eq!(place_refusal(&p("a"), false), None);
    assert_eq!(
        place_refusal(&host, true),
        None,
        "a remote rename goes back"
    );
    assert!(place_refusal(&host, false).is_some());
    assert!(place_refusal(&zip, true).is_some() && place_refusal(&node, true).is_some());
}

/// A library operation is recorded from its plan and the disk once its job ended: a Skip
/// left what existed, a move counts only sources that went, remote deletes are refused.
#[test]
fn library_operations_record_what_their_job_did() {
    use keel_core::{OnConflict, Op as L};
    let planned = |op: L, existed: &[&str]| Planned {
        op,
        existed: existed.iter().map(|n| p(n).display()).collect(),
        since: std::time::SystemTime::now(),
        generation: 0,
        moves: Vec::new(),
        made: Vec::new(),
        prune: Vec::new(),
        taken: Vec::new(),
        before: Vec::new(),
        dated: Vec::new(),
        dates: Vec::new(),
    };
    let copy = planned(
        L::Copy {
            src: vec![p("a/x"), p("a/y"), p("a/z")],
            dst_dir: p("b"),
            on_conflict: OnConflict::Skip,
        },
        &["b/y"],
    );
    // y existed (skipped), z never arrived (the job failed there).
    let fs = disk(&[("a/x", 1), ("a/y", 2), ("a/z", 3), ("b/x", 1), ("b/y", 9)]);
    assert_eq!(copy.done(&fs), Ok((Op::Copy, vec![item("b/x", None, 1)])));
    let mv = planned(
        L::Move {
            src: vec![p("a/x"), p("a/y")],
            dst_dir: p("b"),
            on_conflict: OnConflict::Skip,
        },
        &[],
    );
    // y is still in a: not moved.
    let fs = disk(&[("b/x", 1), ("a/y", 2), ("b/y", 2)]);
    assert_eq!(
        mv.done(&fs),
        Ok((Op::Move, vec![item("b/x", Some("a/x"), 1)]))
    );
    let ren = planned(
        L::Rename {
            path: p("a/x"),
            new_name: "w".into(),
        },
        &[],
    );
    let (op, items) = ren.done(disk(&[("a/w", 4)])).unwrap();
    assert_eq!(op, Op::Rename(vec![(p("a/x"), p("a/w"))]));
    assert_eq!(items, [item("a/w", Some("a/x"), 4)]);
    let host = VPath {
        scheme: "sftp".into(),
        authority: "box".into(),
        path: "/x".into(),
    };
    let remote = planned(L::Delete { paths: vec![host] }, &[]);
    assert_eq!(
        remote.done(disk(&[])),
        Err(crate::state::PERMANENT.to_owned())
    );
    // Overwrite replaced files: never recorded, not even the targets that were new.
    for mv in [false, true] {
        let (src, dst_dir, on_conflict) = (vec![p("a/x")], p("b"), OnConflict::Overwrite);
        let op = if mv {
            L::Move {
                src,
                dst_dir,
                on_conflict,
            }
        } else {
            L::Copy {
                src,
                dst_dir,
                on_conflict,
            }
        };
        assert_eq!(
            planned(op, &[]).done(disk(&[("b/x", 1)])),
            Err("files were replaced by Overwrite".to_owned())
        );
    }
}

/// Taken places and unreachable folders may pass: the entry stays. A changed or missing
/// item does not.
#[test]
fn transient_refusals_keep_the_entry() {
    let mv = entry(Op::Move, vec![item("b/x", Some("a/x"), 5)]);
    let check = |files: &[(&str, u64)]| mv.check(disk(files)).unwrap_err();
    assert!(check(&[("b/x", 5), ("a/x", 1)]).transient, "taken");
    assert!(!check(&[("b/x", 6)]).transient, "changed");
    assert!(!check(&[]).transient, "gone, its folder is there");
    let offline = entry(Op::Move, vec![item("share/x", Some("a/x"), 5)]);
    let r = offline.check(disk(&[])).unwrap_err();
    assert!(r.transient && r.why.ends_with("can't be reached"), "{r:?}");
}

/// A profile switch bumps the generation operations are tagged with.
#[test]
fn a_profile_switch_starts_a_new_generation() {
    let mut j = Journal::default();
    let first = j.generation();
    j.record(Op::Copy, vec![item("a/1", None, 1)]);
    j.clear();
    assert_ne!(j.generation(), first);
    assert_eq!(j.label(), None);
}

// ---- in a temporary folder ------------------------------------------------------------

/// Every path under `root` with its bytes (folders: none), sorted.
fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                out.push((rel, Vec::new()));
                walk(root, &path, out);
            } else {
                out.push((rel, std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    std::fs::create_dir_all(a.join("folder/sub")).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("one.txt"), "one").unwrap();
    std::fs::write(a.join("folder/sub/deep.bin"), [0u8, 1, 2, 255]).unwrap();
    (tmp, a, b)
}

/// Runs a real transfer and records it as the app does.
fn transfer(router: &Router, names: &[&str], from: &Path, to: &Path, mv: bool) -> Entry {
    let src: Vec<VPath> = names.iter().map(|n| VPath::local(from.join(n))).collect();
    let mut out = Vec::new();
    keel_vfs::ops::transfer_landed(
        &src,
        &VPath::local(to),
        mv,
        Conflict::Skip,
        &|_| {},
        &AtomicBool::new(false),
        router,
        &mut out,
    )
    .unwrap();
    let op = if mv { Op::Move } else { Op::Copy };
    let mut j = Journal::default();
    j.record(op, landed(&out, mv, stat_fn(router)))
        .unwrap()
        .clone()
}

fn undo(router: &Router, e: &Entry) -> anyhow::Result<()> {
    run(e, router, &|_| {}, &AtomicBool::new(false))
}

#[test]
fn a_move_undone_leaves_the_tree_as_it_was() {
    let (tmp, a, b) = fixture();
    let before = tree(tmp.path());
    let router = Router::new();
    let e = transfer(&router, &["one.txt", "folder"], &a, &b, true);
    assert_eq!(e.items.len(), 2);
    assert!(b.join("folder/sub/deep.bin").exists() && !a.join("folder").exists());
    assert_eq!(
        e.preview(),
        format!("Undo move: 2 items back to {}?", a.display())
    );
    undo(&router, &e).unwrap();
    assert_eq!(tree(tmp.path()), before, "byte-identical");
}

#[test]
fn an_undo_is_refused_when_the_old_place_is_taken_or_the_file_changed() {
    let (tmp, a, b) = fixture();
    let router = Router::new();
    let e = transfer(&router, &["one.txt"], &a, &b, true);
    // Something new at the old place: nothing moves, nothing is overwritten.
    std::fs::write(a.join("one.txt"), "new").unwrap();
    let err = format!("{:#}", undo(&router, &e).unwrap_err());
    assert!(
        err.starts_with("Can't undo: ") && err.ends_with("is taken"),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(a.join("one.txt")).unwrap(), "new");
    assert_eq!(std::fs::read_to_string(b.join("one.txt")).unwrap(), "one");
    // The moved file changed since.
    std::fs::remove_file(a.join("one.txt")).unwrap();
    std::fs::write(b.join("one.txt"), "changed!").unwrap();
    let err = format!("{:#}", undo(&router, &e).unwrap_err());
    assert_eq!(err, "Can't undo: one.txt changed since");
    assert!(b.join("one.txt").exists() && !a.join("one.txt").exists());
    // A rename back onto a name that is taken stops the same way.
    let renamed_to = VPath::local(b.join("two.txt"));
    let from = VPath::local(b.join("one.txt"));
    let local = keel_vfs::LocalProvider;
    keel_vfs::Provider::rename(&local, &from, &renamed_to).unwrap();
    let steps = vec![(from.clone(), renamed_to.clone())];
    let items = renamed(&steps, stat_fn(&router));
    let ren = entry(Op::Rename(steps), items);
    std::fs::write(b.join("one.txt"), "other").unwrap();
    assert!(format!("{:#}", undo(&router, &ren).unwrap_err()).ends_with("is taken"));
    std::fs::remove_file(b.join("one.txt")).unwrap();
    undo(&router, &ren).unwrap();
    assert_eq!(
        std::fs::read_to_string(b.join("one.txt")).unwrap(),
        "changed!"
    );
    drop(tmp);
}

/// The tests that use the real Recycle Bin / Trash run one at a time.
static BIN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Whether this system can trash and find a file again (CI machines often cannot).
fn trash_works(dir: &Path) -> bool {
    if cfg!(target_os = "macos") && std::env::var_os("CI").is_some() {
        eprintln!("skipped under CI on macOS: the runner's Trash is not observable");
        return false;
    }
    let probe = dir.join(unique("probe"));
    std::fs::write(&probe, "x").unwrap();
    let since = std::time::SystemTime::now();
    match keel_vfs::local::trash(&VPath::local(&probe)) {
        Ok(kept) => {
            let router = Router::new();
            let items = trashed(&[(VPath::local(&probe), kept)], since, stat_fn(&router))
                .unwrap_or_default();
            purge(&items);
            if items.is_empty() {
                eprintln!("skipped: a trashed file is not found again here");
            }
            !items.is_empty()
        }
        Err(e) => {
            eprintln!("skipped: this system has no usable trash: {e:#}");
            false
        }
    }
}

fn unique(tag: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("keel-trash-test-undo-{tag}-{}-{n}.txt", std::process::id())
}

/// Deletes for good what these tests put in the trash.
fn purge(items: &[Item]) {
    for it in items {
        if it.now.scheme == keel_vfs::trashbin::SCHEME {
            let _ = keel_vfs::TrashProvider.purge_paths(std::slice::from_ref(&it.now));
        } else if let Some(local) = it.now.to_local_path() {
            let _ = std::fs::remove_file(local);
        }
    }
}

#[test]
fn a_copy_undone_goes_to_the_trash_and_the_originals_stay() {
    let _g = BIN.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, a, b) = fixture();
    if !trash_works(&b) {
        return;
    }
    let name = unique("copy");
    std::fs::write(a.join(&name), "copy me").unwrap();
    let router = Router::new();
    let e = transfer(&router, &[&name], &a, &b, false);
    assert!(b.join(&name).exists());
    let since = std::time::SystemTime::now();
    undo(&router, &e).unwrap();
    assert!(!b.join(&name).exists(), "the copy left");
    assert_eq!(std::fs::read_to_string(a.join(&name)).unwrap(), "copy me");
    // In the trash (found by its path where the trash records it).
    if keel_vfs::trashbin::KNOWS_ORIGIN {
        let found = trashed(
            &[(VPath::local(b.join(&name)), None)],
            since,
            stat_fn(&router),
        )
        .unwrap();
        assert_eq!(found.len(), 1, "the copy is in the trash");
        purge(&found);
    }
}

/// A copy of many small files (made on the copy worker pool) is undone exactly: every copy
/// goes to the trash, the originals and what was in the destination before stay.
#[test]
fn a_parallel_copy_undone_trashes_exactly_the_copies() {
    let _g = BIN.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, a, b) = fixture();
    if !trash_works(&b) {
        return;
    }
    std::fs::write(b.join("kept.txt"), "mine").unwrap();
    let names: Vec<String> = (0..40).map(|i| unique(&format!("pool{i}"))).collect();
    for (i, name) in names.iter().enumerate() {
        std::fs::write(a.join(name), format!("copy {i}")).unwrap();
    }
    let (a_before, b_before) = (tree(&a), tree(&b));
    let mut sources: Vec<&str> = names.iter().map(String::as_str).collect();
    sources.push("folder");
    let router = Router::new();
    let e = transfer(&router, &sources, &a, &b, false);
    assert_eq!(e.items.len(), 41);
    assert!(b.join("folder/sub/deep.bin").exists());
    let since = std::time::SystemTime::now();
    undo(&router, &e).unwrap();
    assert_eq!(tree(&b), b_before, "only the copies left");
    assert_eq!(tree(&a), a_before, "the originals stay");
    if keel_vfs::trashbin::KNOWS_ORIGIN {
        let copies: Vec<_> = sources
            .iter()
            .map(|n| (VPath::local(b.join(n)), None))
            .collect();
        let found = trashed(&copies, since, stat_fn(&router)).unwrap();
        assert_eq!(found.len(), 41, "every copy is in the trash");
        purge(&found);
    }
}

#[test]
fn a_delete_undone_comes_back_from_the_trash() {
    let _g = BIN.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, a, _b) = fixture();
    if !trash_works(&a) {
        return;
    }
    let name = unique("delete");
    let file = a.join(&name);
    std::fs::write(&file, "bring me back").unwrap();
    let since = std::time::SystemTime::now();
    let kept = keel_vfs::local::trash(&VPath::local(&file)).unwrap();
    assert!(!file.exists());
    let router = Router::new();
    let items = trashed(&[(VPath::local(&file), kept)], since, stat_fn(&router)).unwrap();
    let e = entry(Op::Trash, items.clone());
    // Taken meanwhile: refused, the trashed file stays where it is.
    std::fs::write(&file, "squatter").unwrap();
    let err = format!("{:#}", undo(&router, &e).unwrap_err());
    assert!(err.ends_with("is taken"), "{err}");
    std::fs::remove_file(&file).unwrap();
    if let Err(e) = undo(&router, &e) {
        purge(&items);
        panic!("{e:#}");
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "bring me back");
    assert!(
        e.check(stat_fn(&router)).is_err(),
        "no longer in the trash: a second undo is refused"
    );
}

/// A delete whose items are not found in the trash again is refused, not recorded short:
/// the next Ctrl+Z must not go on to the operation before.
#[test]
fn a_delete_not_found_in_the_trash_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let never = VPath::local(tmp.path().join(unique("never-trashed")));
    let since = std::time::SystemTime::now();
    let router = Router::new();
    let err = trashed(&[(never.clone(), None)], since, stat_fn(&router)).unwrap_err();
    assert!(
        err.starts_with(&format!("couldn't find {} in the ", never.name())),
        "{err}"
    );
    let two = [(never.clone(), None), (never, None)];
    let err = trashed(&two, since, stat_fn(&router)).unwrap_err();
    assert!(err.starts_with("couldn't find 2 of the 2 items"), "{err}");
    assert_eq!(trashed(&[], since, stat_fn(&router)), Ok(Vec::new()));
}

/// A copy keeps its original's size and modified time; its stamp still tells them apart by
/// the file's identity, so Undo copy refuses once the original took the copy's place.
#[test]
fn stamps_tell_a_copy_from_its_original() {
    let tmp = tempfile::tempdir().unwrap();
    let (orig, copy) = (tmp.path().join("orig.txt"), tmp.path().join("copy.txt"));
    std::fs::write(&orig, "same bytes").unwrap();
    std::fs::copy(&orig, &copy).unwrap();
    let mtime = std::fs::metadata(&orig).unwrap().modified().unwrap();
    std::fs::File::options()
        .write(true)
        .open(&copy)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let router = Router::new();
    let stat = stat_fn(&router);
    let (a, b) = (
        stat(&VPath::local(&orig)).unwrap(),
        stat(&VPath::local(&copy)).unwrap(),
    );
    assert_eq!((a.size, a.mtime), (b.size, b.mtime), "same size and time");
    if a.id.is_none() {
        return eprintln!("skipped: no file ids on this file system");
    }
    assert_ne!(a, b, "told apart by identity");
    assert_eq!(stat(&VPath::local(&orig)), Some(a), "stable for one file");
    let e = entry(
        Op::Copy,
        vec![Item {
            now: VPath::local(&copy),
            was: None,
            stamp: Some(b),
        }],
    );
    assert_eq!(e.check(stat_fn(&router)), Ok(()));
    // The original moved onto the copy's path: same size and time, another file.
    std::fs::remove_file(&copy).unwrap();
    std::fs::rename(&orig, &copy).unwrap();
    let r = e.check(stat_fn(&router)).unwrap_err();
    assert_eq!(r.why, "copy.txt changed since");
}

/// Undoing a folder move never merges into a folder that appeared at the old place: it is
/// refused and both folders keep what they hold.
#[test]
fn a_folder_move_back_onto_a_new_folder_is_refused() {
    let (tmp, a, b) = fixture();
    let router = Router::new();
    let e = transfer(&router, &["folder"], &a, &b, true);
    std::fs::create_dir(a.join("folder")).unwrap();
    std::fs::write(a.join("folder/newer.txt"), "new").unwrap();
    let before = tree(tmp.path());
    let err = format!("{:#}", undo(&router, &e).unwrap_err());
    assert!(err.ends_with("is taken"), "{err}");
    assert_eq!(tree(tmp.path()), before, "nothing moved or merged");
}

// ---- folder operations, attributes and links (library plans) -------------------------

/// Runs a library plan as the app does (`Planned` read before, recorded when it ends).
fn run_plan(lib: &keel_core::Library, op: keel_core::Op) -> Entry {
    let (u, id) = start_plan(lib, op);
    let info = lib.jobs().wait(id).unwrap();
    assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    recorded(&u)
}

/// Previews `op` and starts its job, with what undo reads before it runs.
fn start_plan(lib: &keel_core::Library, op: keel_core::Op) -> (Planned, keel_core::JobId) {
    let plan = keel_core::validate_preview_execute(lib, op.clone()).unwrap();
    let pick = |a: keel_core::Action| -> Vec<VPath> {
        (plan.changes.iter())
            .filter(|c| c.action == a)
            .map(|c| c.from.clone())
            .collect()
    };
    let mut u = Planned {
        op,
        existed: Vec::new(),
        since: std::time::SystemTime::now(),
        generation: 0,
        moves: (plan.changes.iter())
            .filter(|c| c.action == keel_core::Action::Move)
            .filter_map(|c| Some((c.from.clone(), c.to.clone()?)))
            .collect(),
        made: pick(keel_core::Action::MakeFolder),
        prune: pick(keel_core::Action::RemoveFolder),
        taken: Vec::new(),
        before: Vec::new(),
        dated: pick(keel_core::Action::SetFolderDate),
        dates: Vec::new(),
    };
    u.read_before(stat_fn(&Router::new()));
    let id = plan.execute(lib, true).unwrap();
    (u, id)
}

/// The journal entry of a plan whose job ended.
fn recorded(u: &Planned) -> Entry {
    let (op, items) = u.done(stat_fn(&Router::new())).unwrap();
    let mut j = Journal::default();
    j.record(op, items).unwrap().clone()
}

/// A flatten cancelled part way is undone for what it moved: every file back in its
/// folder, byte for byte.
#[test]
fn a_flatten_cancelled_part_way_is_undone() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("f");
    for i in 0..300 {
        let dir = root.join(format!("d{}", i % 7));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("f{i}.txt")), i.to_string()).unwrap();
    }
    let before = tree(tmp.path());
    let (_data, lib) = library();
    let op = keel_core::Op::arrange(
        keel_core::FolderOp::Flatten {
            folder: VPath::local(&root),
        },
        keel_core::OnConflict::Skip,
    );
    let (u, id) = start_plan(&lib, op);
    // Cancel once something moved up (or the job is over: then it is undone whole).
    let moved = || {
        std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .any(|e| e.path().is_file())
    };
    let running = || {
        lib.jobs().info(id).is_ok_and(|i| {
            matches!(
                i.status,
                keel_core::JobStatus::Queued | keel_core::JobStatus::Running
            )
        })
    };
    while !moved() && running() {
        std::thread::yield_now();
    }
    let _ = lib.jobs().cancel(id);
    let info = lib.jobs().wait(id).unwrap();
    assert!(
        matches!(
            info.status,
            keel_core::JobStatus::Cancelled | keel_core::JobStatus::Done
        ),
        "{:?}",
        info.status
    );
    let e = recorded(&u);
    assert!(!e.items.is_empty());
    undo(&Router::new(), &e).unwrap();
    assert_eq!(tree(tmp.path()), before, "every file back where it was");
}

fn library() -> (tempfile::TempDir, keel_core::Library) {
    let data = tempfile::tempdir().unwrap();
    let lib = keel_core::Library::open(data.path(), "t").unwrap();
    (data, lib)
}

#[test]
fn a_flatten_undone_makes_its_folders_again_and_puts_every_file_back() {
    let (tmp, a, _b) = fixture();
    std::fs::write(a.join("folder/sub/one.txt"), "clash").unwrap();
    std::fs::write(a.join("folder/two.txt"), "two").unwrap();
    let before = tree(tmp.path());
    let (_data, lib) = library();
    let op = keel_core::Op::arrange(
        keel_core::FolderOp::Flatten {
            folder: VPath::local(&a),
        },
        keel_core::OnConflict::Skip,
    );
    let e = run_plan(&lib, op);
    // one.txt clashed and stayed (with its folder); the other two moved up.
    assert!(a.join("deep.bin").exists() && a.join("two.txt").exists());
    assert!(a.join("folder/sub/one.txt").exists());
    assert_eq!(e.items.len(), 2);
    assert_eq!(e.noun(), "flatten");
    undo(&Router::new(), &e).unwrap();
    assert_eq!(tree(tmp.path()), before, "byte-identical");
}

#[test]
fn an_organize_undone_removes_the_folders_it_made() {
    let (tmp, a, _b) = fixture();
    std::fs::write(a.join("photo.jpg"), "jpg").unwrap();
    let before = tree(tmp.path());
    let (_data, lib) = library();
    let op = keel_core::Op::arrange(
        keel_core::FolderOp::Organize {
            folder: VPath::local(&a),
            by: keel_core::OrganizeBy::Type,
        },
        keel_core::OnConflict::Skip,
    );
    let e = run_plan(&lib, op);
    assert!(a.join("Images/photo.jpg").exists() && a.join("Documents/one.txt").exists());
    assert_eq!(e.done_text(), "Organized 2 items into folders");
    // A place taken since refuses, and the entry stays for later.
    std::fs::write(a.join("photo.jpg"), "new").unwrap();
    let refused = e.check(stat_fn(&Router::new())).unwrap_err();
    assert!(
        refused.transient && refused.why.contains("is taken"),
        "{refused:?}"
    );
    std::fs::remove_file(a.join("photo.jpg")).unwrap();
    undo(&Router::new(), &e).unwrap();
    assert_eq!(tree(tmp.path()), before);
}

#[test]
fn a_merge_undone_moves_the_items_back_into_the_folders_it_removed() {
    let (tmp, a, b) = fixture();
    std::fs::write(b.join("mine.txt"), "b").unwrap();
    let before = tree(tmp.path());
    let (_data, lib) = library();
    let op = keel_core::Op::arrange(
        keel_core::FolderOp::Merge {
            folders: vec![VPath::local(&a), VPath::local(&b)],
            into: VPath::local(&b),
        },
        keel_core::OnConflict::Skip,
    );
    let e = run_plan(&lib, op);
    assert!(!a.exists(), "the emptied folder went");
    assert!(b.join("folder/sub/deep.bin").exists());
    undo(&Router::new(), &e).unwrap();
    assert_eq!(tree(tmp.path()), before);
}

/// Two sources with a subfolder of one name: the second lands as `Vacation (2)` (one plan
/// never lands two items on one name), so undo puts each back.
#[test]
fn a_merge_of_two_same_named_subfolders_undone_puts_each_back() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        tmp.path().join("A"),
        tmp.path().join("B"),
        tmp.path().join("C"),
    );
    std::fs::create_dir_all(a.join("Vacation")).unwrap();
    std::fs::create_dir_all(b.join("Vacation")).unwrap();
    std::fs::create_dir_all(&c).unwrap();
    std::fs::write(a.join("Vacation/a.jpg"), "a").unwrap();
    std::fs::write(b.join("Vacation/b.jpg"), "b").unwrap();
    let before = tree(tmp.path());
    let (_data, lib) = library();
    let op = keel_core::Op::arrange(
        keel_core::FolderOp::Merge {
            folders: vec![VPath::local(&a), VPath::local(&b), VPath::local(&c)],
            into: VPath::local(&c),
        },
        keel_core::OnConflict::Skip,
    );
    let e = run_plan(&lib, op);
    assert!(c.join("Vacation/a.jpg").exists() && c.join("Vacation (2)/b.jpg").exists());
    assert_eq!(e.items.len(), 2);
    undo(&Router::new(), &e).unwrap();
    assert_eq!(tree(tmp.path()), before);
}

/// A folder that merged into one already there stays on undo, and the toast says so; the
/// rest goes back.
#[test]
fn a_merge_into_a_folder_already_there_is_left_out_of_the_undo() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, c) = (tmp.path().join("A"), tmp.path().join("C"));
    std::fs::create_dir_all(a.join("Vacation")).unwrap();
    std::fs::create_dir_all(c.join("Vacation")).unwrap();
    std::fs::write(a.join("Vacation/a.jpg"), "a").unwrap();
    std::fs::write(a.join("x.txt"), "x").unwrap();
    std::fs::write(c.join("Vacation/c.jpg"), "c").unwrap();
    let (_data, lib) = library();
    let op = keel_core::Op::arrange(
        keel_core::FolderOp::Merge {
            folders: vec![VPath::local(&a), VPath::local(&c)],
            into: VPath::local(&c),
        },
        keel_core::OnConflict::Skip,
    );
    let e = run_plan(&lib, op);
    assert!(c.join("Vacation/a.jpg").exists() && !a.exists());
    assert_eq!(e.items.len(), 1, "{e:?}");
    let text = e.done_text();
    assert!(
        text.contains("1 item merged into a folder already there, so Undo leaves it"),
        "{text}"
    );
    assert!(e.preview().contains("Undo leaves it"), "{}", e.preview());
    undo(&Router::new(), &e).unwrap();
    assert_eq!(std::fs::read_to_string(a.join("x.txt")).unwrap(), "x");
    assert!(c.join("Vacation/a.jpg").exists() && c.join("Vacation/c.jpg").exists());
    assert!(
        !a.join("Vacation").exists(),
        "the merged folder was not split"
    );
}

#[test]
fn attributes_and_links_undone() {
    let (tmp, a, _b) = fixture();
    let (_data, lib) = library();
    let one = VPath::local(a.join("one.txt"));
    let op = keel_core::Op::SetAttributes {
        paths: vec![one.clone()],
        attrs: keel_vfs::Attributes {
            readonly: Some(true),
            ..Default::default()
        },
    };
    let e = run_plan(&lib, op);
    let local = a.join("one.txt");
    assert_eq!(
        keel_vfs::fsattr::attributes(&local).unwrap().readonly,
        Some(true)
    );
    undo(&Router::new(), &e).unwrap();
    assert_eq!(
        keel_vfs::fsattr::attributes(&local).unwrap().readonly,
        Some(false)
    );

    let before = tree(tmp.path());
    let kind = if cfg!(windows) {
        keel_vfs::LinkKind::Junction
    } else {
        keel_vfs::LinkKind::Symlink
    };
    let op = keel_core::Op::Link {
        target: VPath::local(a.join("folder")),
        link: VPath::local(tmp.path().join("link")),
        kind,
    };
    let e = run_plan(&lib, op);
    assert!(tmp.path().join("link/sub/deep.bin").exists());
    assert_eq!(e.preview(), "Undo link: remove link?");
    undo(&Router::new(), &e).unwrap();
    assert!(!tmp.path().join("link").exists());
    assert_eq!(tree(tmp.path()), before, "what it pointed to stays");
}

/// Task 85: remove empty folders is undone from the Recycle Bin / Trash: every branch it
/// sent there comes back where it was.
#[test]
fn removed_empty_folders_come_back_from_the_trash() {
    let _g = BIN.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, a, _b) = fixture();
    if !trash_works(&a) || !keel_vfs::trashbin::KNOWS_ORIGIN {
        return;
    }
    let root = a.join(unique("empties"));
    for d in ["x/y/z", "w"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(root.join("keep.txt"), "k").unwrap();
    let (_data, lib) = library();
    let op = keel_core::Op::arrange(
        keel_core::FolderOp::RemoveEmpty {
            folder: VPath::local(&root),
        },
        keel_core::OnConflict::Skip,
    );
    let e = run_plan(&lib, op);
    assert!(!root.join("x").exists() && !root.join("w").exists());
    assert_eq!(e.items.len(), 2);
    undo(&Router::new(), &e).unwrap();
    assert!(root.join("x/y/z").is_dir(), "the branch came back whole");
    assert!(root.join("w").is_dir());
    assert!(root.join("keep.txt").exists());
}

/// Task 85: folder dates undone get every folder's old time back; a folder changed since
/// refuses the undo.
#[test]
fn folder_dates_undone() {
    use std::time::{Duration, UNIX_EPOCH};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let old = UNIX_EPOCH + Duration::from_secs(1_300_000_000);
    let f = std::fs::File::create(root.join("sub/a.txt")).unwrap();
    f.set_modified(old).unwrap();
    drop(f);
    let time = |p: &std::path::Path| std::fs::metadata(p).unwrap().modified().unwrap();
    let before = (time(&root), time(&root.join("sub")));
    let (_data, lib) = library();
    let e = run_plan(&lib, keel_core::Op::folder_dates(VPath::local(&root)));
    assert_eq!((time(&root), time(&root.join("sub"))), (old, old));
    assert_eq!(e.noun(), "folder dates");
    assert_eq!(e.done_text(), "Set the dates of 2 folders");
    assert_eq!(
        e.preview(),
        "Undo folder dates: set 2 folders back as they were?"
    );
    undo(&Router::new(), &e).unwrap();
    assert_eq!((time(&root), time(&root.join("sub"))), before);

    // Again, then a file added: that folder changed since, so nothing is set back.
    let e = run_plan(&lib, keel_core::Op::folder_dates(VPath::local(&root)));
    std::fs::write(root.join("sub/new.txt"), "n").unwrap();
    let refused = e.check(stat_fn(&Router::new())).unwrap_err();
    assert!(refused.why.contains("changed since"), "{refused:?}");
}
