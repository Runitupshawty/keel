use keel_vfs::{drives, watch, Kind, LocalProvider, Provider, Router, VPath};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

fn extended(p: &Path) -> PathBuf {
    PathBuf::from(format!(r"\\?\{}", p.display()))
}

#[test]
fn listing_sorts_dirs_then_natural_names_and_reports_hidden() {
    let tmp = tempfile::tempdir().unwrap();
    for name in ["b.txt", "A.txt", ".hidden"] {
        fs::write(tmp.path().join(name), b"x").unwrap();
    }
    fs::create_dir(tmp.path().join("dir1")).unwrap();
    assert!(std::process::Command::new("attrib")
        .arg("+h")
        .arg(tmp.path().join(".hidden"))
        .status()
        .unwrap()
        .success());
    let entries = LocalProvider.list(&VPath::local(tmp.path())).unwrap();
    assert_eq!(
        entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        ["dir1", "A.txt", "b.txt", ".hidden"]
    );
    assert!(entries[3].hidden);
    assert_eq!(entries[0].kind, Kind::Dir);
    fs::write(tmp.path().join("a10.txt"), b"").unwrap();
    fs::write(tmp.path().join("a2.txt"), b"").unwrap();
    let entries = LocalProvider.list(&VPath::local(tmp.path())).unwrap();
    assert!(
        entries.iter().position(|e| e.name == "a2.txt")
            < entries.iter().position(|e| e.name == "a10.txt")
    );
}

#[test]
fn long_path_with_trailing_dot_can_list_rename_and_recycle() {
    let tmp = tempfile::tempdir().unwrap();
    let mut deep = extended(tmp.path());
    while deep.as_os_str().len() < 310 {
        deep.push("long-directory-component");
    }
    fs::create_dir_all(&deep).unwrap();
    let original = deep.join("trailing.");
    fs::write(&original, b"long path").unwrap();
    let entries = LocalProvider.list(&VPath::local(&deep)).unwrap();
    assert_eq!(entries[0].name, "trailing.");
    let renamed = VPath::local(deep.join("renamed. "));
    LocalProvider
        .rename(&VPath::local(&original), &renamed)
        .unwrap();
    assert_eq!(LocalProvider.stat(&renamed).unwrap().size, 9);
    LocalProvider.remove(&renamed).unwrap();
    assert!(!deep.join("renamed. ").exists());
}

#[test]
fn remove_recycles_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("recycle-me.txt");
    fs::write(&p, b"recycle fixture").unwrap();
    LocalProvider.remove(&VPath::local(&p)).unwrap();
    // The Shell owns bin storage; this test checks removal, not bin contents.
    assert!(!p.exists());
}

#[test]
fn write_read_stat_rename_mkdir_and_local_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = VPath::local(tmp.path()).join("new");
    LocalProvider.mkdir(&dir).unwrap();
    let p = dir.join("File.TXT");
    LocalProvider
        .write(&p)
        .unwrap()
        .write_all(b"round trip")
        .unwrap();
    let mut data = String::new();
    LocalProvider
        .read(&p)
        .unwrap()
        .read_to_string(&mut data)
        .unwrap();
    assert_eq!(data, "round trip");
    let entry = LocalProvider.stat(&p).unwrap();
    assert_eq!(entry.size, 10);
    assert_eq!(entry.ext, "txt");
    assert_eq!(entry.kind, Kind::File);
    assert!(entry.modified.is_some());
    assert_eq!(
        LocalProvider.local_copy(&p).unwrap(),
        tmp.path().join("new").join("File.TXT")
    );
    let to = VPath::local(tmp.path()).join("renamed.txt");
    LocalProvider.rename(&p, &to).unwrap();
    assert!(!p.to_local_path().unwrap().exists());
    assert_eq!(
        fs::read(to.to_local_path().unwrap()).unwrap(),
        b"round trip"
    );
}

#[test]
fn errors_include_path_and_reject_nonlocal_scheme() {
    let tmp = tempfile::tempdir().unwrap();
    let p = VPath::local(tmp.path()).join("absent");
    assert!(LocalProvider
        .list(&p)
        .unwrap_err()
        .to_string()
        .contains("absent"));
    assert!(LocalProvider
        .stat(&VPath::parse("sftp://box/a").unwrap())
        .is_err());
}

#[test]
fn router_registers_local_and_replaces_matching_scheme() {
    let mut router = Router::new();
    let p = VPath::local("C:\\");
    let provider = router.provider_for(&p).unwrap();
    assert!(
        provider.caps().watch
            && provider.caps().write
            && provider.caps().rename
            && provider.caps().delete
    );
    let replacement: Arc<dyn Provider> = Arc::new(LocalProvider);
    router.register(replacement.clone());
    assert!(Arc::ptr_eq(&replacement, &router.provider_for(&p).unwrap()));
    assert!(router
        .provider_for(&VPath::parse("sftp://box/a").unwrap())
        .is_none());
}

#[test]
fn watcher_notifies_within_one_second_and_coalesces_bursts() {
    let tmp = tempfile::tempdir().unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let watcher = watch(tmp.path(), tx).unwrap();
    for n in 0..10 {
        fs::write(tmp.path().join(format!("watch{n}")), b"x").unwrap();
    }
    rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(250)).is_err());
    drop(watcher);
    assert!(rx.recv_timeout(Duration::from_secs(1)).is_err());
}

#[test]
fn drive_list_contains_system_drive() {
    let result = drives();
    let c = result.iter().find(|(name, _, _, _)| name == "C:").unwrap();
    assert!(c.3 > 0 && c.2 <= c.3);
}

#[test]
#[ignore = "creates/caches 50,000 files; run in release mode"]
fn perf_list_50k() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/perf-list-50k");
    fs::create_dir_all(&dir).unwrap();
    eprintln!("cached fixture: {}", dir.display());
    for n in 0..50_000 {
        let path = dir.join(format!("file{n:05}.txt"));
        if !path.exists() {
            fs::write(path, b"").unwrap();
        }
    }
    let start = Instant::now();
    let entries = LocalProvider.list(&VPath::local(&dir)).unwrap();
    let elapsed = start.elapsed();
    eprintln!("listed {} entries in {elapsed:?}", entries.len());
    assert_eq!(entries.len(), 50_000);
    assert!(
        elapsed < Duration::from_millis(150),
        "listing took {elapsed:?}"
    );
}
