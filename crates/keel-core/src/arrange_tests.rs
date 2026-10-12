//! Folder operations through plans: preview, execution, conflicts, cancel, a changed disk.

use crate::index::tests::{library_with, walk, write};
use crate::library::tests::folder;
use crate::{
    validate_preview_execute, Action, FolderOp, JobStatus, Library, OnConflict, Op, OrganizeBy,
    PlanChanged, Warning,
};
use keel_vfs::VPath;
use std::path::Path;

fn v(p: &Path) -> VPath {
    VPath::local(p)
}

fn run(lib: &Library, op: Op) -> crate::JobInfo {
    let plan = validate_preview_execute(lib, op).unwrap();
    let id = plan.execute(lib, true).unwrap();
    lib.jobs().wait(id).unwrap()
}

/// Every file under `dir` as (path relative to it with `/`, content).
fn tree(dir: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = walkdir(dir)
        .into_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (rel, std::fs::read_to_string(&p).unwrap())
        })
        .collect();
    out.sort();
    out
}

fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.file_type().unwrap().is_dir() {
            out.extend(walkdir(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

/// The folders under `dir` (relative, with `/`).
fn dirs(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.file_type().unwrap().is_dir() {
            let name = e.file_name().to_string_lossy().into_owned();
            out.push(name.clone());
            out.extend(dirs(&e.path()).into_iter().map(|d| format!("{name}/{d}")));
        }
    }
    out.sort();
    out
}

fn pairs(t: &[(&str, &str)]) -> Vec<(String, String)> {
    t.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

#[test]
fn merge_moves_everything_into_one_folder_and_removes_the_emptied_ones() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("a/one.txt"), "1");
    write(&root.join("a/sub/deep.txt"), "d");
    write(&root.join("b/two.txt"), "2");
    write(&root.join("b/same.txt"), "from b");
    write(&root.join("c/same.txt"), "from c");
    let (_data, lib, src) = library_with(folder("f", root));
    walk(&src, &lib.router()).unwrap();
    let (a, b, c) = (v(&root.join("a")), v(&root.join("b")), v(&root.join("c")));
    let op = Op::arrange(
        FolderOp::Merge {
            folders: vec![a.clone(), b.clone(), c.clone()],
            into: c.clone(),
        },
        OnConflict::Skip,
    );
    let plan = validate_preview_execute(&lib, op.clone()).unwrap();
    let moved: Vec<(Action, String)> = plan
        .changes
        .iter()
        .map(|ch| (ch.action, ch.to.as_ref().unwrap_or(&ch.from).display()))
        .collect();
    let at = |s: &str| v(&root.join(s)).display();
    assert_eq!(
        moved,
        [
            (Action::Move, at("c/one.txt")),
            (Action::Move, at("c/sub")),
            (Action::Move, at("c/same.txt")),
            (Action::Move, at("c/two.txt")),
            (Action::RemoveFolder, at("a")),
        ]
    );
    assert_eq!(plan.changes[1].files, 1, "a folder counts its files");
    assert!(plan.warnings.contains(&Warning::Exists {
        path: c.join("same.txt"),
        on_conflict: OnConflict::Skip,
    }));
    let id = plan.execute(&lib, true).unwrap();
    assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Done);
    assert_eq!(
        tree(root),
        pairs(&[
            ("b/same.txt", "from b"),
            ("c/one.txt", "1"),
            ("c/same.txt", "from c"),
            ("c/sub/deep.txt", "d"),
            ("c/two.txt", "2"),
        ])
    );
    // a was emptied and removed; b keeps the skipped file.
    assert_eq!(dirs(root), ["b", "c", "c/sub"]);
    let log = lib.op_log(5).unwrap();
    assert_eq!(log[0].kind, "merge");
}

#[test]
fn merge_refuses_nested_folders_and_a_lone_folder() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("a/in/x.txt"), "x");
    let (_data, lib, _src) = library_with(folder("f", root));
    let (a, inner) = (v(&root.join("a")), v(&root.join("a/in")));
    let nested = Op::arrange(
        FolderOp::Merge {
            folders: vec![a.clone(), inner.clone()],
            into: inner.clone(),
        },
        OnConflict::Skip,
    );
    let err = validate_preview_execute(&lib, nested).unwrap_err();
    assert!(format!("{err:#}").contains("is inside"), "{err:#}");
    let alone = Op::arrange(
        FolderOp::Merge {
            folders: vec![a.clone()],
            into: a,
        },
        OnConflict::Skip,
    );
    assert!(validate_preview_execute(&lib, alone).is_err());
}

#[test]
fn flatten_moves_files_up_and_follows_the_conflict_rule() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().join("f");
    write(&root.join("top.txt"), "top");
    write(&root.join("x/a.txt"), "xa");
    write(&root.join("x/y/a.txt"), "xya");
    write(&root.join("x/y/b.txt"), "b");
    write(&root.join("z/top.txt"), "z");
    std::fs::create_dir_all(root.join("empty/inner")).unwrap();
    let (_data, lib, src) = library_with(folder("f", files.path()));
    walk(&src, &lib.router()).unwrap();
    let op = Op::arrange(
        FolderOp::Flatten { folder: v(&root) },
        OnConflict::RenameNew,
    );
    let plan = validate_preview_execute(&lib, op).unwrap();
    let moves = plan.changes.iter().filter(|c| c.action == Action::Move);
    assert_eq!(moves.count(), 4);
    let removes: Vec<String> = (plan.changes.iter())
        .filter(|c| c.action == Action::RemoveFolder)
        .map(|c| c.from.name().to_owned())
        .collect();
    // Deepest first.
    assert_eq!(removes.len(), 5);
    assert!(removes.iter().position(|n| n == "y") < removes.iter().position(|n| n == "x"));
    assert!(removes.iter().position(|n| n == "inner") < removes.iter().position(|n| n == "empty"));
    // top.txt exists; a.txt is taken by the first of the two, so the second is renamed.
    let exists: Vec<String> = (plan.warnings.iter())
        .filter_map(|w| match w {
            Warning::Exists { path, .. } => Some(path.name().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(exists, ["top.txt"]);
    let to = |from: &str| {
        let c = plan.changes.iter().find(|c| c.from == v(&root.join(from)));
        c.unwrap().to.as_ref().unwrap().name().to_owned()
    };
    assert_eq!(
        (to("x/a.txt"), to("x/y/a.txt")),
        ("a.txt".into(), "a (2).txt".into())
    );
    let id = plan.execute(&lib, true).unwrap();
    assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Done);
    let got = tree(&root);
    assert_eq!(got.len(), 5, "{got:?}");
    let contents: std::collections::BTreeSet<&str> = got.iter().map(|(_, c)| c.as_str()).collect();
    assert_eq!(
        contents,
        ["b", "top", "xa", "xya", "z"].into_iter().collect()
    );
    assert!(got.iter().all(|(p, _)| !p.contains('/')), "{got:?}");
    assert!(dirs(&root).is_empty(), "{:?}", dirs(&root));
}

/// Two items of one plan never land on one name: under Overwrite the plan is refused (the
/// later one would replace the first), else the later ones get the next free name; the
/// check folds case on every system (macOS volumes fold it too).
#[test]
fn one_plan_never_lands_two_items_on_one_name() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().join("f");
    write(&root.join("x/same.txt"), "x");
    write(&root.join("y/same.txt"), "y");
    write(&root.join("x/deep/same.txt"), "deep");
    write(&root.join("a/Read.txt"), "upper");
    write(&root.join("b/read.txt"), "lower");
    let (_data, lib, _src) = library_with(folder("f", files.path()));
    let before = tree(&root);
    let flatten = |c| Op::arrange(FolderOp::Flatten { folder: v(&root) }, c);
    let err = validate_preview_execute(&lib, flatten(OnConflict::Overwrite)).unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("pick Skip or Rename new"), "{err}");
    assert_eq!(err.lines().skip(1).count(), 3, "one line per item: {err}");
    assert_eq!(tree(&root), before, "nothing moved");
    for c in [OnConflict::Skip, OnConflict::RenameNew] {
        let plan = validate_preview_execute(&lib, flatten(c)).unwrap();
        assert!(
            !plan
                .warnings
                .iter()
                .any(|w| matches!(w, Warning::Exists { .. })),
            "{c:?}: nothing was there: {:?}",
            plan.warnings
        );
        let mut shown: Vec<String> = (plan.changes.iter())
            .filter(|ch| ch.action == Action::Move)
            .map(|ch| ch.to.as_ref().unwrap().name().to_owned())
            .collect();
        shown.sort();
        assert_eq!(
            shown,
            [
                "Read.txt",
                "read (2).txt",
                "same (2).txt",
                "same (3).txt",
                "same.txt"
            ]
        );
        let id = plan.execute(&lib, true).unwrap();
        let info = lib.jobs().wait(id).unwrap();
        assert_eq!(info.status, JobStatus::Done, "{}", info.log);
        let got = tree(&root);
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "Read.txt",
                "read (2).txt",
                "same (2).txt",
                "same (3).txt",
                "same.txt"
            ],
            "every file kept, under the name the preview showed"
        );
        // Put them back for the next rule.
        for (dir, name) in [("a", "Read.txt"), ("b", "read (2).txt")] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            let back = if dir == "b" { "read.txt" } else { name };
            std::fs::rename(root.join(name), root.join(dir).join(back)).unwrap();
        }
        let mut same: Vec<(String, String)> = (got.iter())
            .filter(|(n, _)| n.starts_with("same"))
            .cloned()
            .collect();
        same.sort();
        for (name, body) in same {
            let dir = match body.as_str() {
                "x" => "x",
                "y" => "y",
                _ => "x/deep",
            };
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::rename(root.join(&name), root.join(dir).join("same.txt")).unwrap();
        }
        assert_eq!(tree(&root), before);
    }
}

#[test]
fn flatten_with_skip_keeps_the_clashing_file_and_its_folder() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("a.txt"), "top");
    write(&root.join("s/a.txt"), "inner");
    write(&root.join("s/b.txt"), "b");
    let (_data, lib, _src) = library_with(folder("f", root));
    let op = Op::arrange(FolderOp::Flatten { folder: v(root) }, OnConflict::Skip);
    // The preview does not say the folder goes: the skipped file keeps it.
    let plan = validate_preview_execute(&lib, op.clone()).unwrap();
    assert!(
        !plan
            .changes
            .iter()
            .any(|c| c.action == Action::RemoveFolder),
        "{:?}",
        plan.changes
    );
    assert_eq!(run(&lib, op).status, JobStatus::Done);
    assert_eq!(
        tree(root),
        pairs(&[("a.txt", "top"), ("b.txt", "b"), ("s/a.txt", "inner")])
    );
}

#[test]
fn organize_makes_group_folders_by_type_and_by_date() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    for (name, body) in [
        ("a.jpg", "i"),
        ("b.PDF", "d"),
        ("c.mp3", "m"),
        ("d.xyz", "o"),
    ] {
        write(&root.join(name), body);
    }
    std::fs::create_dir_all(root.join("Images")).unwrap();
    write(&root.join("keep/inner.jpg"), "k");
    let (_data, lib, _src) = library_with(folder("f", root));
    let op = Op::arrange(
        FolderOp::Organize {
            folder: v(root),
            by: OrganizeBy::Type,
        },
        OnConflict::Skip,
    );
    let plan = validate_preview_execute(&lib, op.clone()).unwrap();
    let made: Vec<&str> = (plan.changes.iter())
        .filter(|c| c.action == Action::MakeFolder)
        .map(|c| c.from.name())
        .collect();
    // Images exists already.
    assert_eq!(made, ["Documents", "Audio", "Other"]);
    let id = plan.execute(&lib, true).unwrap();
    assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Done);
    assert_eq!(
        tree(root),
        pairs(&[
            ("Audio/c.mp3", "m"),
            ("Documents/b.PDF", "d"),
            ("Images/a.jpg", "i"),
            ("Other/d.xyz", "o"),
            ("keep/inner.jpg", "k"),
        ])
    );
    // Nothing left to organize.
    let err = validate_preview_execute(&lib, op).unwrap_err();
    assert!(
        format!("{err:#}").contains("nothing to organize"),
        "{err:#}"
    );

    // By year and by month of the modified date.
    let dated = tempfile::tempdir().unwrap();
    let june = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_623_758_400);
    let march = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_710_460_800);
    for (name, t) in [("old.txt", june), ("new.txt", march)] {
        let p = dated.path().join(name);
        write(&p, name);
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(t).unwrap();
    }
    let by = |by| {
        Op::arrange(
            FolderOp::Organize {
                folder: v(dated.path()),
                by,
            },
            OnConflict::Skip,
        )
    };
    let plan = validate_preview_execute(&lib, by(OrganizeBy::Month)).unwrap();
    let to: Vec<String> = (plan.changes.iter())
        .filter(|c| c.action == Action::Move)
        .map(|c| {
            c.to.as_ref()
                .unwrap()
                .path
                .rsplit('/')
                .nth(1)
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(to, ["2024-03", "2021-06"]);
    assert_eq!(run(&lib, by(OrganizeBy::Year)).status, JobStatus::Done);
    assert_eq!(
        tree(dated.path()),
        pairs(&[("2021/old.txt", "old.txt"), ("2024/new.txt", "new.txt")])
    );
}

#[test]
fn organize_refuses_a_group_name_taken_by_a_file() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("Other"), "a file named like the group");
    write(&root.join("x.unknown"), "x");
    let (_data, lib, _src) = library_with(folder("f", root));
    let op = Op::arrange(
        FolderOp::Organize {
            folder: v(root),
            by: OrganizeBy::Type,
        },
        OnConflict::Skip,
    );
    let err = validate_preview_execute(&lib, op).unwrap_err();
    assert!(format!("{err:#}").contains("is a file"), "{err:#}");
    assert_eq!(
        std::fs::read_to_string(root.join("Other")).unwrap(),
        "a file named like the group"
    );
    assert!(root.join("x.unknown").exists());
}

#[test]
fn a_changed_disk_refuses_the_confirmed_preview() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("s/a.txt"), "a");
    let (_data, lib, _src) = library_with(folder("f", root));
    let op = Op::arrange(FolderOp::Flatten { folder: v(root) }, OnConflict::Skip);
    let plan = validate_preview_execute(&lib, op.clone()).unwrap();
    write(&root.join("s/new.txt"), "n");
    let err = plan.clone().execute(&lib, true).unwrap_err();
    let fresh = err.downcast::<PlanChanged>().unwrap().0;
    assert!(fresh.changes.iter().any(|c| c.from.name() == "new.txt"));
    // Without the recheck the job finds it before its first step.
    let id = plan.execute(&lib, false).unwrap();
    let info = lib.jobs().wait(id).unwrap();
    assert_eq!(info.status, JobStatus::Failed);
    assert!(root.join("s/a.txt").exists(), "nothing ran");
    // A file gone since the preview refuses too (the fresh preview cannot be made the same).
    let plan = validate_preview_execute(&lib, op).unwrap();
    std::fs::remove_file(root.join("s/new.txt")).unwrap();
    assert!(plan.execute(&lib, true).is_err());
}

#[test]
fn a_cancelled_operation_leaves_every_file_once() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    for i in 0..300 {
        write(&root.join(format!("d{}/f{i}.txt", i % 7)), &i.to_string());
    }
    let (_data, lib, _src) = library_with(folder("f", root));
    let op = Op::arrange(FolderOp::Flatten { folder: v(root) }, OnConflict::Skip);
    let plan = validate_preview_execute(&lib, op).unwrap();
    let id = plan.execute(&lib, true).unwrap();
    lib.jobs().cancel(id).unwrap();
    let info = lib.jobs().wait(id).unwrap();
    assert!(
        matches!(info.status, JobStatus::Cancelled | JobStatus::Done),
        "{:?}",
        info.status
    );
    let got = tree(root);
    assert_eq!(got.len(), 300);
    let mut contents: Vec<usize> = got.iter().map(|(_, c)| c.parse().unwrap()).collect();
    contents.sort();
    assert_eq!(contents, (0..300).collect::<Vec<_>>());
}

#[test]
fn attributes_and_links_run_as_plans() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("f.txt"), "f");
    std::fs::create_dir_all(root.join("dir")).unwrap();
    let (_data, lib, _src) = library_with(folder("f", root));
    let f = v(&root.join("f.txt"));
    let ro = keel_vfs::Attributes {
        readonly: Some(true),
        ..Default::default()
    };
    let op = Op::SetAttributes {
        paths: vec![f.clone()],
        attrs: ro,
    };
    let plan = validate_preview_execute(&lib, op.clone()).unwrap();
    assert_eq!(plan.changes[0].action, Action::SetAttributes);
    assert_eq!(run(&lib, op).status, JobStatus::Done);
    let now = keel_vfs::fsattr::attributes(&root.join("f.txt")).unwrap();
    assert_eq!(now.readonly, Some(true));
    let rw = keel_vfs::Attributes {
        readonly: Some(false),
        ..Default::default()
    };
    let back = Op::SetAttributes {
        paths: vec![f.clone()],
        attrs: rw,
    };
    assert_eq!(run(&lib, back).status, JobStatus::Done);

    let hard = Op::Link {
        target: f.clone(),
        link: v(&root.join("hard.txt")),
        kind: keel_vfs::LinkKind::Hardlink,
    };
    let plan = validate_preview_execute(&lib, hard.clone()).unwrap();
    assert_eq!(plan.changes[0].action, Action::Link);
    assert_eq!(run(&lib, hard.clone()).status, JobStatus::Done);
    assert_eq!(std::fs::read_to_string(root.join("hard.txt")).unwrap(), "f");
    let err = validate_preview_execute(&lib, hard).unwrap_err();
    assert!(format!("{err:#}").contains("already exists"));
    let dir_hard = Op::Link {
        target: v(&root.join("dir")),
        link: v(&root.join("dir2")),
        kind: keel_vfs::LinkKind::Hardlink,
    };
    assert!(validate_preview_execute(&lib, dir_hard).is_err());

    #[cfg(windows)]
    {
        write(&root.join("dir/in.txt"), "in");
        let junction = Op::Link {
            target: v(&root.join("dir")),
            link: v(&root.join("j")),
            kind: keel_vfs::LinkKind::Junction,
        };
        assert_eq!(run(&lib, junction).status, JobStatus::Done);
        assert_eq!(
            std::fs::read_to_string(root.join("j/in.txt")).unwrap(),
            "in"
        );
        let symlink = Op::Link {
            target: f.clone(),
            link: v(&root.join("sym.txt")),
            kind: keel_vfs::LinkKind::Symlink,
        };
        match validate_preview_execute(&lib, symlink) {
            Ok(_) => assert!(keel_vfs::fsattr::symlinks_allowed()),
            Err(e) => assert!(format!("{e:#}").contains("Developer Mode"), "{e:#}"),
        }
    }
}

#[test]
fn folder_operations_refuse_archives_and_remote_attributes() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("a.txt"), "a");
    let (_data, lib, _src) = library_with(folder("f", root));
    let inside = VPath::join_archive(&v(&root.join("x.zip")), "dir");
    let op = Op::arrange(FolderOp::Flatten { folder: inside }, OnConflict::Skip);
    let err = validate_preview_execute(&lib, op).unwrap_err();
    assert!(format!("{err:#}").contains("archive"), "{err:#}");
    let remote = VPath::parse("sftp://host/a.txt").unwrap();
    let op = Op::SetAttributes {
        paths: vec![remote],
        attrs: keel_vfs::Attributes {
            readonly: Some(true),
            ..Default::default()
        },
    };
    let err = validate_preview_execute(&lib, op).unwrap_err();
    assert!(format!("{err:#}").contains("this computer"), "{err:#}");
}

/// Task 85: remove empty folders lists the outermost folder of each empty branch, refuses a
/// changed disk, and sends what it lists to the trash.
#[test]
fn remove_empty_trashes_each_empty_branch_whole() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().join("tree with spaces");
    for d in ["a/b/c", "d/e", "g", "kept/inner"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    write(&root.join("d/f.txt"), "f");
    write(&root.join("kept/inner/x.txt"), "x");
    let (_data, lib, _src) = library_with(folder("f", &root));
    let at = |p: &str| v(&root.join(p));
    let op = Op::arrange(FolderOp::RemoveEmpty { folder: v(&root) }, OnConflict::Skip);
    let plan = validate_preview_execute(&lib, op.clone()).unwrap();
    let listed: Vec<(Action, VPath)> = plan
        .changes
        .iter()
        .map(|c| (c.action, c.from.clone()))
        .collect();
    assert_eq!(
        listed,
        [
            (Action::RemoveFolder, at("a")),
            (Action::RemoveFolder, at("d/e")),
            (Action::RemoveFolder, at("g")),
        ]
    );
    let headline = plan.headline().unwrap();
    assert!(
        headline.starts_with("Remove 3 empty folders below"),
        "{headline}"
    );

    // A file that came into a listed folder since the preview: refused, with the fresh
    // preview (which no longer lists that branch).
    write(&root.join("a/b/new.txt"), "n");
    let err = plan.execute(&lib, true).unwrap_err();
    let fresh = err.downcast::<PlanChanged>().unwrap().0;
    assert!(fresh.changes.iter().all(|c| c.from != at("a")));
    assert!(fresh.changes.iter().any(|c| c.from == at("a/b/c")));
    std::fs::remove_file(root.join("a/b/new.txt")).unwrap();

    let info = run(&lib, op.clone());
    if info.status != JobStatus::Done {
        // No Recycle Bin or Trash here (a CI runner without one): nothing else to check.
        eprintln!("skipped: {}", info.log);
        assert!(root.join("a/b/c").is_dir(), "nothing removed for good");
        return;
    }
    assert_eq!(dirs(&root), ["d", "kept", "kept/inner"]);
    assert!(root.join("d/f.txt").exists() && root.join("kept/inner/x.txt").exists());
    // Nothing left to remove.
    let err = validate_preview_execute(&lib, op).unwrap_err();
    assert!(format!("{err:#}").contains("no empty folders"), "{err:#}");
}

/// Remove empty folders where there is no trash (an SFTP host here): each branch is warned
/// as permanent, the headline says so, and the folders go one at a time, each only while
/// empty.
#[test]
fn remove_empty_without_a_trash_warns_and_removes_folder_by_folder() {
    use keel_vfs::Provider;
    let files = tempfile::tempdir().unwrap();
    let (_data, lib, _src) = library_with(folder("f", files.path()));
    let mem = std::sync::Arc::new(keel_vfs::memory::MemoryProvider::new());
    lib.router()
        .register_remote_provider("box".into(), mem.clone());
    let at = |p: &str| VPath::parse(&format!("sftp://box/srv{p}")).unwrap();
    for d in ["", "/a", "/a/b", "/a/b/c", "/keep"] {
        mem.mkdir(&at(d)).unwrap();
    }
    mem.put("/srv/keep/x.txt", "x");
    let op = Op::arrange(FolderOp::RemoveEmpty { folder: at("") }, OnConflict::Skip);
    let plan = validate_preview_execute(&lib, op.clone()).unwrap();
    assert!(
        plan.warnings
            .contains(&Warning::Permanent { path: at("/a") }),
        "{:?}",
        plan.warnings
    );
    let headline = plan.headline().unwrap();
    assert!(
        headline.contains("deleted for good") && !headline.contains(keel_vfs::trashbin::label()),
        "{headline}"
    );
    let info = run(&lib, op);
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    let dirs = mem.dirs();
    assert!(!dirs.iter().any(|d| d.starts_with("/srv/a")), "{dirs:?}");
    assert!(dirs.iter().any(|d| d == "/srv/keep"), "{dirs:?}");
    assert_eq!(mem.get("/srv/keep/x.txt").as_deref(), Some(&b"x"[..]));

    // Folder by folder: a file found below stops it, and nothing holding it goes.
    let p: &dyn Provider = &*mem;
    mem.mkdir(&at("/late")).unwrap();
    mem.mkdir(&at("/late/sub")).unwrap();
    mem.put("/srv/late/sub/new.txt", "n");
    assert!(crate::arrange::remove_empty_tree(p, &at("/late")).is_err());
    assert_eq!(mem.get("/srv/late/sub/new.txt").as_deref(), Some(&b"n"[..]));
}
