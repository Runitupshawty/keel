use super::*;
use crate::index::tests::{fake_source, fake_tree, id_of, library_with, walk, write, State};
use crate::library::tests::folder;
use crate::oplog::OUTSIDE;
use crate::{JobStatus, SourceKind};
use keel_vfs::Router;
use parking_lot::Mutex;
use std::{path::Path, sync::Arc};

fn v(p: &Path) -> VPath {
    VPath::local(p)
}

fn set_cas(src: &Source, rel: &str, cas: &[u8]) {
    let id = id_of(src, rel).unwrap();
    src.store
        .get()
        .unwrap()
        .execute(
            "UPDATE record SET cas_id = ?2 WHERE id = ?1",
            rusqlite::params![id, cas],
        )
        .unwrap();
}

fn run(lib: &Library, op: Op) -> crate::JobInfo {
    let plan = validate_preview_execute(lib, op).unwrap();
    let id = plan.execute(lib).unwrap();
    lib.jobs().wait(id).unwrap()
}

#[test]
fn preview_projects_from_the_index_with_last_copy_warnings() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("keep/a.txt"), "aaaa");
    write(&root.join("keep/b.txt"), "b");
    write(&root.join("other/c.txt"), "cc");
    write(&root.join("other/a.txt"), "x");
    let (_data, lib, src) = library_with(folder("f", root));
    walk(&src, &lib.router()).unwrap();
    set_cas(&src, "keep/a.txt", &[1]);
    set_cas(&src, "keep/b.txt", &[2]);
    set_cas(&src, "other/c.txt", &[2]);

    let keep = v(&root.join("keep"));
    let plan = validate_preview_execute(
        &lib,
        Op::Delete {
            paths: vec![keep.clone()],
        },
    )
    .unwrap();
    assert_eq!(
        plan.changes,
        [Change {
            action: Action::Delete,
            from: keep.clone(),
            to: None,
            files: 2,
            bytes: 5,
        }]
    );
    assert_eq!(
        plan.warnings,
        [Warning::LastCopy {
            path: keep.clone(),
            files: 1,
        }]
    );

    // Deleting both copies of content 2 makes them last copies too.
    let other = v(&root.join("other"));
    let plan = validate_preview_execute(
        &lib,
        Op::Delete {
            paths: vec![keep.clone(), other.clone()],
        },
    )
    .unwrap();
    assert_eq!(
        plan.warnings,
        [
            // other/a.txt has no content id.
            Warning::ContentUnverified {
                path: other.clone(),
                files: 1,
            },
            Warning::LastCopy {
                path: keep.clone(),
                files: 2,
            },
            Warning::LastCopy {
                path: other.clone(),
                files: 1,
            },
        ]
    );

    let plan = validate_preview_execute(
        &lib,
        Op::Copy {
            src: vec![v(&root.join("keep/a.txt"))],
            dst_dir: other.clone(),
            on_conflict: OnConflict::Skip,
        },
    )
    .unwrap();
    assert_eq!(
        plan.warnings,
        [Warning::Exists {
            path: other.join("a.txt"),
            on_conflict: OnConflict::Skip,
        }]
    );
    assert_eq!((plan.changes[0].files, plan.changes[0].bytes), (1, 4));
}

#[test]
fn invalid_operations_are_refused() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("keep/a.txt"), "a");
    write(&root.join("keep/b.txt"), "b");
    write(&root.join("keep/sub/c.txt"), "c");
    let (_data, lib, src) = library_with(folder("f", root));
    walk(&src, &lib.router()).unwrap();
    let keep = v(&root.join("keep"));
    let refused = |op: Op, why: &str| {
        let err = validate_preview_execute(&lib, op).unwrap_err();
        assert!(format!("{err:#}").contains(why), "{err:#} (wanted {why})");
    };
    refused(
        Op::Copy {
            src: vec![keep.clone()],
            dst_dir: keep.join("sub"),
            on_conflict: OnConflict::Skip,
        },
        "into itself",
    );
    refused(
        Op::Move {
            src: vec![keep.join("a.txt")],
            dst_dir: keep.clone(),
            on_conflict: OnConflict::Skip,
        },
        "already in",
    );
    refused(
        Op::Copy {
            src: vec![keep.join("a.txt")],
            dst_dir: keep.join("b.txt"),
            on_conflict: OnConflict::Skip,
        },
        "not a folder",
    );
    refused(
        Op::Rename {
            path: keep.join("a.txt"),
            new_name: "b.txt".into(),
        },
        "already exists",
    );
    refused(
        Op::Rename {
            path: keep.join("a.txt"),
            new_name: "x/y".into(),
        },
        "invalid name",
    );
    refused(
        Op::Delete {
            paths: vec![keep.join("missing.txt")],
        },
        "does not exist",
    );
    refused(Op::Delete { paths: vec![] }, "nothing to delete");
}

#[test]
fn a_plan_that_no_longer_matches_is_not_executed() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("keep/a.txt"), "a");
    let (_data, lib, src) = library_with(folder("f", root));
    walk(&src, &lib.router()).unwrap();
    let keep = v(&root.join("keep"));
    let delete = Op::Delete {
        paths: vec![keep.clone()],
    };
    let plan = validate_preview_execute(&lib, delete.clone()).unwrap();
    // More files now: the confirmed preview is stale.
    write(&root.join("keep/b.txt"), "b");
    walk(&src, &lib.router()).unwrap();
    let err = plan.clone().execute(&lib).unwrap_err();
    let fresh = err.downcast::<PlanChanged>().unwrap().0;
    assert_eq!(fresh.changes[0].files, 2);
    // Warnings count too: a content id appears (no more ContentUnverified for it).
    set_cas(&src, "keep/a.txt", &[9]);
    let err = fresh.clone().execute(&lib).unwrap_err();
    let fresh = err.downcast::<PlanChanged>().unwrap().0;
    assert_eq!(*fresh, validate_preview_execute(&lib, delete).unwrap());
    assert!(root.join("keep").exists(), "nothing ran");
    let job = fresh.execute(&lib).unwrap();
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    assert!(!root.join("keep").exists());
}

/// Runs an op job as if resumed after a crash right after item 0's side effects.
fn resume_after_crash(lib: &Library, op: Op, target_existed: bool) -> crate::JobInfo {
    let job = lib
        .jobs()
        .spawn(Box::new(ExecJob {
            op,
            next: 0,
            skipped: 0,
            log_id: None,
            started: Some(Started {
                item: 0,
                target_existed,
            }),
        }))
        .unwrap();
    lib.jobs().wait(job).unwrap()
}

#[test]
fn a_step_that_ran_before_a_crash_is_not_run_again() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("a.txt"), "a");
    write(&root.join("b.txt"), "b");
    std::fs::create_dir_all(root.join("dst")).unwrap();
    let (_data, lib, src) = library_with(folder("f", root));
    walk(&src, &lib.router()).unwrap();

    // The move happened, the checkpoint after it did not.
    std::fs::rename(root.join("a.txt"), root.join("dst/a.txt")).unwrap();
    let mv = Op::Move {
        src: vec![v(&root.join("a.txt"))],
        dst_dir: v(&root.join("dst")),
        on_conflict: OnConflict::Skip,
    };
    let info = resume_after_crash(&lib, mv, false);
    assert_eq!(info.status, JobStatus::Done);
    assert!(
        info.log.contains("resumed after the step had run"),
        "{}",
        info.log
    );
    assert_eq!(lib.op_log(1).unwrap()[0].result, "ok", "done, not skipped");
    assert!(id_of(&src, "dst/a.txt").is_some(), "index caught up");
    assert!(id_of(&src, "a.txt").is_none());

    // A copy that renames on conflict: its target appeared, so no second copy.
    std::fs::copy(root.join("b.txt"), root.join("dst/b.txt")).unwrap();
    let copy = Op::Copy {
        src: vec![v(&root.join("b.txt"))],
        dst_dir: v(&root.join("dst")),
        on_conflict: OnConflict::RenameNew,
    };
    assert_eq!(
        resume_after_crash(&lib, copy.clone(), false).status,
        JobStatus::Done
    );
    let mut names: Vec<_> = std::fs::read_dir(root.join("dst"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["a.txt", "b.txt"]);
    // The crash came before the copy (its target name was already taken): it runs.
    assert_eq!(resume_after_crash(&lib, copy, true).status, JobStatus::Done);
    assert_eq!(std::fs::read_dir(root.join("dst")).unwrap().count(), 3);
}

#[test]
fn offline_sources_preview_from_their_last_generation() {
    let state = Arc::new(Mutex::new(State {
        fail: None,
        lists_left: None,
    }));
    let router = Arc::new(Router::new());
    let (_data, lib, src) = library_with(fake_source(&router, &state, SourceKind::Share));
    lib.set_router(router.clone());
    walk(&src, &router).unwrap();
    state.lock().lists_left = Some(0);
    assert!(walk(&src, &router).is_err());
    let a = VPath::parse("fake://box/a").unwrap();
    let plan = validate_preview_execute(
        &lib,
        Op::Delete {
            paths: vec![a.clone()],
        },
    )
    .unwrap();
    assert_eq!((plan.changes[0].files, plan.changes[0].bytes), (1, 7));
    assert_eq!(
        plan.warnings,
        [
            Warning::OfflineSource {
                source: src.id.clone(),
                label: "box".into(),
            },
            Warning::Permanent { path: a.clone() },
            Warning::ContentUnverified { path: a, files: 1 },
        ]
    );
}

#[test]
fn executed_operations_update_the_index_and_the_op_log() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("src/a.txt"), "a");
    std::fs::create_dir_all(root.join("dst")).unwrap();
    std::fs::create_dir_all(root.join("moved")).unwrap();
    let (_data, lib, src) = library_with(folder("f", root));
    walk(&src, &lib.router()).unwrap();

    let info = run(
        &lib,
        Op::Copy {
            src: vec![v(&root.join("src/a.txt"))],
            dst_dir: v(&root.join("dst")),
            on_conflict: OnConflict::Skip,
        },
    );
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    assert!(root.join("dst/a.txt").is_file());
    let copied = id_of(&src, "dst/a.txt").expect("copy indexed");

    let info = run(
        &lib,
        Op::Rename {
            path: v(&root.join("dst/a.txt")),
            new_name: "b.txt".into(),
        },
    );
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    assert_eq!(
        id_of(&src, "dst/b.txt"),
        Some(copied),
        "rename keeps identity"
    );
    assert_eq!(id_of(&src, "dst/a.txt"), None);

    let info = run(
        &lib,
        Op::Move {
            src: vec![v(&root.join("dst/b.txt"))],
            dst_dir: v(&root.join("moved")),
            on_conflict: OnConflict::Skip,
        },
    );
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    assert!(root.join("moved/b.txt").is_file());
    assert_eq!(
        id_of(&src, "moved/b.txt"),
        Some(copied),
        "move keeps identity"
    );

    let log = lib.op_log(10).unwrap();
    let kinds: Vec<_> = log.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["move", "rename", "copy"]);
    assert!(
        log.iter().all(|e| e.result == "ok" && e.ok == Some(true)),
        "{log:?}"
    );
    assert_eq!(
        log[2].payload["src"][0],
        serde_json::json!(v(&root.join("src/a.txt")).display())
    );
}

#[test]
fn op_log_never_records_outside_paths_or_secrets() {
    let files = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = files.path().join("lib");
    write(&root.join("a.txt"), "a");
    let (_data, lib, src) = library_with(folder("f", &root));
    walk(&src, &lib.router()).unwrap();
    let info = run(
        &lib,
        Op::Copy {
            src: vec![v(&root.join("a.txt"))],
            dst_dir: v(outside.path()),
            on_conflict: OnConflict::Skip,
        },
    );
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    assert!(outside.path().join("a.txt").is_file());
    let info = run(
        &lib,
        Op::Rename {
            path: v(&root.join("a.txt")),
            new_name: "token=abc123.txt".into(),
        },
    );
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);

    let log = lib.op_log(10).unwrap();
    assert_eq!(log[1].payload["dst"], serde_json::json!(OUTSIDE));
    assert_eq!(
        log[1].payload["src"][0],
        serde_json::json!(v(&root.join("a.txt")).display())
    );
    let raw: String = lib
        .shared
        .db
        .get()
        .unwrap()
        .query_row(
            "SELECT group_concat(payload || result, '|') FROM op_log",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let outside_text = outside.path().display().to_string();
    let outside_name = outside.path().file_name().unwrap().to_string_lossy();
    assert!(!raw.contains(&outside_text), "{raw}");
    assert!(!raw.contains(&*outside_name), "{raw}");
    assert!(!raw.contains("abc123"), "{raw}");
}

#[test]
fn remote_deletes_run_through_the_provider_and_skip_vanished_paths() {
    let state = Arc::new(Mutex::new(State {
        fail: None,
        lists_left: None,
    }));
    let router = Arc::new(Router::new());
    let fake = Arc::new(fake_tree(state.clone()));
    let (_data, lib, src) = library_with(fake_source(&router, &state, SourceKind::Share));
    router.register(fake.clone());
    lib.set_router(router.clone());
    walk(&src, &router).unwrap();
    let f = VPath::parse("fake://box/f.txt").unwrap();
    let plan = validate_preview_execute(
        &lib,
        Op::Delete {
            paths: vec![f.clone()],
        },
    )
    .unwrap();
    // No trash on this provider, and nothing is known about other copies.
    assert_eq!(
        plan.warnings,
        [
            Warning::Permanent { path: f.clone() },
            Warning::ContentUnverified {
                path: f.clone(),
                files: 1
            },
        ]
    );
    // Executed after the path vanished from the plan's second item: skipped, not failed.
    let job = lib
        .jobs()
        .spawn(Box::new(ExecJob {
            op: Op::Delete {
                paths: vec![f, VPath::parse("fake://box/gone.txt").unwrap()],
            },
            next: 0,
            skipped: 0,
            log_id: None,
            started: None,
        }))
        .unwrap();
    let info = lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    assert!(info.log.contains("skipped"), "{}", info.log);
    assert_eq!(*fake.1.lock(), ["/f.txt"]);
    let entry = &lib.op_log(1).unwrap()[0];
    assert_eq!((entry.result.as_str(), entry.ok), ("1 skipped", Some(true)));
}

#[test]
fn a_resumed_operation_continues_at_its_checkpoint() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("a.txt"), "a");
    write(&root.join("b.txt"), "b");
    std::fs::create_dir_all(root.join("dst")).unwrap();
    let (_data, lib, _src) = library_with(folder("f", root));
    let job = lib
        .jobs()
        .spawn(Box::new(ExecJob {
            op: Op::Copy {
                src: vec![v(&root.join("a.txt")), v(&root.join("b.txt"))],
                dst_dir: v(&root.join("dst")),
                on_conflict: OnConflict::Skip,
            },
            next: 1,
            skipped: 0,
            log_id: None,
            started: None,
        }))
        .unwrap();
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    assert!(!root.join("dst/a.txt").exists());
    assert!(root.join("dst/b.txt").is_file());
}
