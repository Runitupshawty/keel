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
    let err = |e: &Entry, files: &[(&str, u64)]| e.check(disk(files)).unwrap_err();
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
    assert!(del
        .check(fs(false, false))
        .unwrap_err()
        .contains("no longer in the"));
    assert!(del
        .check(fs(true, true))
        .unwrap_err()
        .ends_with("x is taken"));
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
            let items = trashed(&[(VPath::local(&probe), kept)], since);
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
        let found = trashed(&[(VPath::local(b.join(&name)), None)], since);
        assert_eq!(found.len(), 1, "the copy is in the trash");
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
    let items = trashed(&[(VPath::local(&file), kept)], since);
    let e = entry(Op::Trash, items.clone());
    let router = Router::new();
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
