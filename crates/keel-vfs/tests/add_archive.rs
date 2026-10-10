#![cfg(all(feature = "sevenz", feature = "tar"))]
use keel_vfs::ops::add_to_archive;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

fn read(archive: &Path, inner: &str) -> String {
    let mut body = String::new();
    keel_vfs::archive::open_archive(archive)
        .unwrap()
        .read(inner)
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    body
}

fn names(archive: &Path) -> Vec<String> {
    let mut v: Vec<_> = keel_vfs::archive::open_archive(archive)
        .unwrap()
        .entries()
        .unwrap()
        .into_iter()
        .map(|e| e.inner.trim_end_matches('/').to_owned())
        .collect();
    v.sort();
    v
}

fn add(archive: &Path, src: &[PathBuf], at: &str) -> anyhow::Result<()> {
    add_to_archive(archive, src, at, &|_| {}, &AtomicBool::new(false))
}

fn leftovers(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("keel-partial")
        })
        .count()
}

fn roundtrip(name: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join(name);
    let a = tmp.path().join("a.txt");
    fs::write(&a, "first").unwrap();
    let folder = tmp.path().join("folder");
    fs::create_dir_all(folder.join("sub")).unwrap();
    fs::write(folder.join("sub").join("deep.txt"), "deep").unwrap();
    fs::write(folder.join("empty.txt"), "").unwrap();
    // Creates the archive, then adds to it, then replaces one entry.
    add(&archive, std::slice::from_ref(&a), "dir").unwrap();
    add(&archive, &[folder], "").unwrap();
    fs::write(&a, "updated").unwrap();
    add(&archive, &[a], "dir").unwrap();
    assert_eq!(
        names(&archive),
        [
            "dir/a.txt",
            "folder",
            "folder/empty.txt",
            "folder/sub",
            "folder/sub/deep.txt"
        ]
    );
    assert_eq!(read(&archive, "dir/a.txt"), "updated");
    assert_eq!(read(&archive, "folder/sub/deep.txt"), "deep");
    assert_eq!(read(&archive, "folder/empty.txt"), "");
    assert_eq!(leftovers(tmp.path()), 0);
}

#[test]
fn add_to_7z_creates_extends_and_replaces() {
    roundtrip("out.7z");
}

#[test]
fn add_to_tar_gz_creates_extends_and_replaces() {
    roundtrip("out.tar.gz");
    roundtrip("out.tar");
}

#[test]
fn tar_gz_stays_gzip() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join("x.tar.gz");
    let a = tmp.path().join("a.txt");
    fs::write(&a, "x").unwrap();
    add(&archive, &[a], "").unwrap();
    assert_eq!(fs::read(&archive).unwrap()[..2], [0x1f, 0x8b]);
}

#[test]
fn cancel_leaves_the_original_and_no_staging_file() {
    for name in ["c.7z", "c.tar.gz"] {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join(name);
        let a = tmp.path().join("a.txt");
        fs::write(&a, "first").unwrap();
        add(&archive, std::slice::from_ref(&a), "").unwrap();
        let original = fs::read(&archive).unwrap();
        fs::write(&a, vec![42u8; 2 << 20]).unwrap();
        let cancel = AtomicBool::new(false);
        let result = add_to_archive(
            &archive,
            &[tmp.path().join("a.txt")],
            "big",
            &|p| {
                if p.done_bytes > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        );
        assert!(result.is_err(), "{name}");
        assert_eq!(fs::read(&archive).unwrap(), original, "{name}");
        assert_eq!(leftovers(tmp.path()), 0, "{name}");
    }
}

#[test]
fn adding_an_archive_into_itself_is_refused() {
    for name in ["s.7z", "s.tar.gz"] {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join(name);
        let a = tmp.path().join("a.txt");
        fs::write(&a, "x").unwrap();
        add(&archive, &[a], "").unwrap();
        let original = fs::read(&archive).unwrap();
        let err = add(&archive, std::slice::from_ref(&archive), "").unwrap_err();
        assert!(err.to_string().contains("itself"), "{err}");
        // A folder holding it counts too.
        assert!(add(&archive, &[tmp.path().to_path_buf()], "").is_err());
        assert_eq!(fs::read(&archive).unwrap(), original);
        assert_eq!(leftovers(tmp.path()), 0);
    }
}

#[test]
fn rar_and_unknown_formats_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.txt");
    fs::write(&a, "x").unwrap();
    let err = add(&tmp.path().join("x.rar"), std::slice::from_ref(&a), "").unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
    assert!(add(&tmp.path().join("x.tar.xz"), &[a], "").is_err());
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
}
