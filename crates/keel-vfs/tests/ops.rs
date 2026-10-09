use keel_vfs::{copy_local, move_local, plan_size, Conflict};
use std::{
    cell::RefCell,
    fs,
    sync::atomic::{AtomicBool, Ordering},
};

#[test]
fn copies_tree_and_reports_accurate_totals() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    let dst = tmp.path().join("dest");
    fs::create_dir_all(src.join("empty")).unwrap();
    fs::create_dir(&dst).unwrap();
    fs::write(src.join("a"), b"abc").unwrap();
    fs::write(src.join("b"), b"hello").unwrap();
    assert_eq!(plan_size(std::slice::from_ref(&src)).unwrap(), (8, 4));
    let updates = RefCell::new(Vec::new());
    copy_local(
        &[src],
        &dst,
        Conflict::Skip,
        &|p| updates.borrow_mut().push(p),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(fs::read(dst.join("source/a")).unwrap(), b"abc");
    assert!(dst.join("source/empty").is_dir());
    let updates = updates.borrow();
    let final_p = updates.last().unwrap();
    assert_eq!(
        (
            final_p.done_bytes,
            final_p.total_bytes,
            final_p.done_items,
            final_p.total_items
        ),
        (8, 8, 4, 4)
    );
    assert!(updates
        .windows(2)
        .all(|p| p[0].done_bytes <= p[1].done_bytes));
}

#[test]
fn refuses_folder_into_itself_or_subfolder_including_case_aliases() {
    let tmp = tempfile::tempdir().unwrap();
    let sub = tmp.path().join("child");
    fs::create_dir(&sub).unwrap();
    for dst in [
        tmp.path().to_path_buf(),
        sub,
        tmp.path().to_string_lossy().to_uppercase().into(),
    ] {
        let err = copy_local(
            &[tmp.path().to_path_buf()],
            &dst,
            Conflict::Overwrite,
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cannot copy a folder into itself"));
    }
}

#[test]
fn copy_onto_itself_is_refused_without_changing_contents() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("x.txt");
    fs::write(&src, b"keep").unwrap();
    assert!(copy_local(
        std::slice::from_ref(&src),
        tmp.path(),
        Conflict::Overwrite,
        &|_| {},
        &AtomicBool::new(false)
    )
    .is_err());
    assert_eq!(fs::read(src).unwrap(), b"keep");
}

#[test]
fn all_conflict_modes_preserve_or_replace_as_requested() {
    for conflict in [Conflict::Skip, Conflict::RenameNew, Conflict::Overwrite] {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("x.txt");
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();
        fs::write(&src, b"new").unwrap();
        fs::write(dst.join("x.txt"), b"old").unwrap();
        copy_local(&[src], &dst, conflict, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            fs::read(dst.join("x.txt")).unwrap(),
            if conflict == Conflict::Overwrite {
                b"new"
            } else {
                b"old"
            }
        );
        assert_eq!(
            dst.join("x (2).txt").exists(),
            conflict == Conflict::RenameNew
        );
        if conflict == Conflict::RenameNew {
            assert_eq!(fs::read(dst.join("x (2).txt")).unwrap(), b"new");
        }
    }
}

#[test]
fn cancellation_after_first_file_stops_with_partial_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    let sources: Vec<_> = ["a", "b", "c"]
        .map(|name| {
            let p = tmp.path().join(name);
            fs::write(&p, b"data").unwrap();
            p
        })
        .into();
    let cancel = AtomicBool::new(false);
    let last = RefCell::new(None);
    let err = copy_local(
        &sources,
        &dst,
        Conflict::Skip,
        &|p| {
            if p.done_items == 1 {
                cancel.store(true, Ordering::Relaxed);
            }
            *last.borrow_mut() = Some(p);
        },
        &cancel,
    )
    .unwrap_err();
    assert!(err.to_string().contains("cancel"));
    assert_eq!(last.borrow().as_ref().unwrap().done_items, 1);
    assert!(dst.join("a").exists());
    assert!(!dst.join("b").exists());
}

#[test]
fn mid_file_cancel_keeps_existing_destination_and_source() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("large");
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    fs::write(&src, vec![42; 16 * 1024 * 1024]).unwrap();
    fs::write(dst.join("large"), b"original").unwrap();
    let cancel = AtomicBool::new(false);
    let err = copy_local(
        std::slice::from_ref(&src),
        &dst,
        Conflict::Overwrite,
        &|p| {
            if p.done_bytes > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert!(err.to_string().contains("cancel"));
    assert_eq!(fs::read(dst.join("large")).unwrap(), b"original");
    assert_eq!(fs::metadata(src).unwrap().len(), 16 * 1024 * 1024);
}

#[test]
fn move_tree_and_skip_collisions_preserves_source() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir_all(src.join("nested")).unwrap();
    fs::create_dir(&dst).unwrap();
    fs::write(src.join("nested/a"), b"a").unwrap();
    move_local(
        std::slice::from_ref(&src),
        &dst,
        Conflict::Skip,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!src.exists());
    assert_eq!(fs::read(dst.join("src/nested/a")).unwrap(), b"a");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("keep"), b"keep").unwrap();
    move_local(
        std::slice::from_ref(&src),
        &dst,
        Conflict::Skip,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(src.join("keep").exists());
}

#[test]
fn refuses_junction_sources_instead_of_copying_their_targets() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("keep"), b"target").unwrap();
    let alias = tmp.path().join("alias");
    assert!(std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&alias)
        .arg(&src)
        .output()
        .unwrap()
        .status
        .success());
    assert!(plan_size(&[alias]).is_err());
}

#[test]
fn hard_link_alias_is_not_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("same");
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    fs::write(&src, b"preserve").unwrap();
    fs::hard_link(&src, dst.join("same")).unwrap();
    assert!(copy_local(
        std::slice::from_ref(&src),
        &dst,
        Conflict::Overwrite,
        &|_| {},
        &AtomicBool::new(false)
    )
    .is_err());
    assert_eq!(fs::read(src).unwrap(), b"preserve");
}
