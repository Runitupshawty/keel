#![cfg(all(feature = "zip", feature = "sevenz", feature = "tar"))]
use keel_vfs::VPath;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

fn zip_file(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, bytes) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn zip_magic_lists_metadata_and_streams_one_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("misnamed.bin");
    zip_file(&file, &[("dir/a.txt", b"hello"), ("b.txt", b"world")]);
    let mut reader = keel_vfs::archive::open_archive(&file).unwrap();
    let entries = reader.entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].inner, "dir/a.txt");
    assert_eq!(entries[0].size, 5);
    assert!(!entries[0].encrypted);
    let mut body = String::new();
    reader
        .read("dir/a.txt")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "hello");
    assert!(reader.read("missing").is_err());
}

#[test]
fn zip_list_does_not_decode_corrupt_bodies() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("bad.zip");
    zip_file(&file, &[("a.txt", b"body")]);
    let mut bytes = fs::read(&file).unwrap();
    let offset = 30 + "a.txt".len();
    bytes[offset] ^= 0xff;
    fs::write(&file, bytes).unwrap();
    assert_eq!(
        keel_vfs::archive::open_archive(&file)
            .unwrap()
            .entries()
            .unwrap()
            .len(),
        1
    );
}

fn tar_bytes(name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(123);
    header.set_cksum();
    archive.append_data(&mut header, name, bytes).unwrap();
    archive.into_inner().unwrap()
}

#[test]
fn tar_family_dispatches_by_magic_and_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let bytes = tar_bytes("sub/a.txt", b"tar hello");
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), Default::default());
    gzip.write_all(&bytes).unwrap();
    let mut bz = bzip2::write::BzEncoder::new(Vec::new(), Default::default());
    bz.write_all(&bytes).unwrap();
    let mut xz = Vec::new();
    lzma_rs::xz_compress(&mut &bytes[..], &mut xz).unwrap();
    for data in [
        bytes.clone(),
        gzip.finish().unwrap(),
        bz.finish().unwrap(),
        xz,
        ruzstd::encoding::compress_to_vec(&bytes[..], ruzstd::encoding::CompressionLevel::Fastest),
    ] {
        let path = tmp.path().join("unknown.bin");
        fs::write(&path, data).unwrap();
        let mut archive = keel_vfs::archive::open_archive(&path).unwrap();
        let entries = archive.entries().unwrap();
        assert_eq!(entries[0].inner, "sub/a.txt");
        assert_eq!(entries[0].size, 9);
        let mut body = String::new();
        archive
            .read("sub/a.txt")
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "tar hello");
    }
}

#[test]
fn sevenz_metadata_and_single_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("a.txt"), b"seven hello").unwrap();
    let path = tmp.path().join("seven.bin");
    sevenz_rust::compress_to_path(&src, &path).unwrap();
    let mut archive = keel_vfs::archive::open_archive(&path).unwrap();
    let entries = archive.entries().unwrap();
    let entry = entries.iter().find(|e| e.inner == "a.txt").unwrap();
    assert_eq!(entry.size, 11);
    let mut body = String::new();
    archive
        .read("a.txt")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "seven hello");
}

#[cfg(feature = "rar")]
#[test]
fn rar_fixture_lists_and_reads() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.rar");
    assert!(fs::metadata(&path).unwrap().len() < 4096);
    let mut archive = keel_vfs::archive::open_archive(&path).unwrap();
    assert_eq!(
        archive
            .entries()
            .unwrap()
            .iter()
            .filter(|e| !e.is_dir)
            .count(),
        2
    );
    let mut body = String::new();
    archive
        .read("a.txt")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "rar hello\n");
    let names: Vec<_> = archive
        .entries()
        .unwrap()
        .into_iter()
        .map(|e| e.inner)
        .collect();
    assert_eq!(names, ["a.txt", "b.txt"]);
}

#[test]
fn archive_paths_split_at_last_boundary_and_keep_provider() {
    let outer = VPath::parse("sftp://host/backups/x.zip").unwrap();
    let nested = VPath::join_archive(&outer, "dir/y.tar!/a.txt");
    let (container, inner) = nested.split_archive().unwrap();
    assert_eq!(container.path, "/backups/x.zip!/dir/y.tar");
    assert_eq!(container.authority, "host");
    assert_eq!(inner, "a.txt");
    assert_eq!(VPath::join_archive(&outer, "").parent(), outer.parent());
    assert!(nested.to_local_path().is_none());
    let win = VPath::parse("file:///D:/x.zip!/dir/a.txt").unwrap();
    assert_eq!(win.split_archive().unwrap().1, "dir/a.txt");
    assert_eq!(win.parent().unwrap().name(), "dir");
    assert!(win.to_local_path().is_none());
    for name in [
        "a.ZIP",
        "a.jar",
        "a.7z",
        "a.tar",
        "a.tgz",
        "a.tar.gz",
        "a.tar.bz2",
        "a.tar.xz",
        "a.tar.zst",
        "a.rar",
    ] {
        assert!(VPath::is_archive_name(name), "{name}");
    }
    assert!(!VPath::is_archive_name("a.txt"));
}
