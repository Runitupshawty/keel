//! Recovery on synthetic volume images: files written, deleted the way Windows deletes
//! them, then found and read back; reused clusters lower the confidence; damaged and
//! truncated images fail cleanly; restores never land on the source volume.

use super::testimg::*;
use super::*;
use std::io::Cursor;

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ (i >> 8) as u8)
        .collect()
}

fn scan_image(img: Vec<u8>) -> (Scan, Cursor<Vec<u8>>) {
    let mut r = Cursor::new(img);
    let s = scan(&mut r).unwrap();
    (s, r)
}

fn find<'a>(s: &'a Scan, name: &str) -> &'a Candidate {
    s.candidates
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not found in {:#?}", names(s)))
}

fn names(s: &Scan) -> Vec<String> {
    s.candidates.iter().map(Candidate::path).collect()
}

fn content(r: &mut Cursor<Vec<u8>>, c: &Candidate) -> Vec<u8> {
    let mut out = Vec::new();
    let n = read_data(r, c, &mut out).unwrap();
    assert_eq!(n, c.size);
    out
}

/// No panic on any prefix of the image; every candidate reads or fails cleanly.
fn survives_truncation(img: &[u8]) {
    let cuts = [0, 100, 511, 512, 1000, 4096, img.len() / 3, img.len() / 2, img.len() - 1];
    for cut in cuts {
        let mut r = Cursor::new(img[..cut.min(img.len())].to_vec());
        if let Ok(s) = scan(&mut r) {
            for c in &s.candidates {
                let _ = read_data(&mut r, c, &mut std::io::sink());
            }
        }
    }
}

#[test]
fn fat_finds_deleted_files_with_names_and_folders() {
    for kind in [12u8, 16, 32] {
        let mut fat = FatImg::new(kind);
        let root = fat.root();
        fat.add_file(root, Some("keep me.txt"), b"KEEPME~1TXT", b"live");
        let report = pattern(1500, 1);
        let a = fat.add_file(root, Some("Quarterly report.docx"), b"QUARTE~1DOC", &report);
        let b = fat.add_file(root, None, b"NOTES   TXT", b"short name only");
        let (docs, _) = fat.add_dir(root, Some("Documents"), b"DOCUME~1   ");
        let inner = pattern(700, 2);
        let c = fat.add_file(docs, Some("inner file.bin"), b"INNERF~1BIN", &inner);
        let (old, old_entry) = fat.add_dir(root, Some("Old stuff"), b"OLDSTU~1   ");
        let d = fat.add_file(old, Some("gone.txt"), b"GONE    TXT", b"in a deleted folder");
        for f in [&a, &b, &c, &d, &old_entry] {
            fat.delete(f);
        }
        let img = fat.img.clone();
        let (s, mut r) = scan_image(img.clone());
        let want = match kind {
            12 => FsKind::Fat12,
            16 => FsKind::Fat16,
            _ => FsKind::Fat32,
        };
        assert_eq!(s.fs, want);
        assert_eq!(s.cluster_size, 512);
        let mut got = names(&s);
        got.sort();
        assert_eq!(
            got,
            [
                "Documents/inner file.bin",
                "Old stuff/gone.txt",
                "Quarterly report.docx",
                "_OTES.TXT"
            ],
            "FAT{kind}"
        );
        let q = find(&s, "Quarterly report.docx");
        assert_eq!((q.size, q.confidence), (1500, Confidence::Good));
        assert!(!q.first_char_unknown);
        assert_eq!(q.modified, Some(WHEN));
        assert!(q.local_times);
        assert_eq!(content(&mut r, q), report);
        let n = find(&s, "_OTES.TXT");
        assert!(n.first_char_unknown);
        assert_eq!(content(&mut r, n), b"short name only");
        assert_eq!(content(&mut r, find(&s, "inner file.bin")), inner);
        assert_eq!(
            content(&mut r, find(&s, "gone.txt")),
            b"in a deleted folder"
        );
        survives_truncation(&img);
    }
}

#[test]
fn fat_confidence_drops_when_clusters_are_reused() {
    let mut fat = FatImg::new(16);
    let root = fat.root();
    let f = fat.add_file(root, Some("two clusters.bin"), b"TWOCLU~1BIN", &pattern(1000, 3));
    fat.delete(&f);
    let first = f.clusters[0];
    let (s, _) = scan_image(fat.img.clone());
    assert_eq!(find(&s, "two clusters.bin").confidence, Confidence::Good);
    fat.reuse(first, &[0xAA; 512]);
    let (s, _) = scan_image(fat.img.clone());
    assert_eq!(find(&s, "two clusters.bin").confidence, Confidence::Partial);
    fat.reuse(first, &[0xBB; 1024]);
    let (s, _) = scan_image(fat.img.clone());
    assert_eq!(find(&s, "two clusters.bin").confidence, Confidence::Overwritten);
}

#[test]
fn exfat_finds_deleted_entry_sets() {
    let mut ex = ExImg::new();
    let root = ex.root();
    ex.add_file(root, "still here.txt", b"live", Layout::Contiguous);
    let photo = pattern(9000, 3);
    let a = ex.add_file(root, "contiguous photo.jpg", &photo, Layout::Contiguous);
    let chained = pattern(5000, 4);
    let b = ex.add_file(root, "chained.bin", &chained, Layout::Chained);
    let frag = pattern(4096 * 3 + 10, 5);
    let c = ex.add_file(root, "fragmented.dat", &frag, Layout::Fragmented);
    let (dir, dir_set) = ex.add_dir(root, "Photos");
    let long = "a long name spread over several file name entries.txt";
    let d = ex.add_file(dir, long, b"deep", Layout::Contiguous);
    let empty = ex.add_file(root, "empty.txt", b"", Layout::Contiguous);
    for f in [&a, &b, &c, &d, &dir_set, &empty] {
        ex.delete(f);
    }
    let img = ex.image();
    let (s, mut r) = scan_image(img.clone());
    assert_eq!(s.fs, FsKind::Exfat);
    assert_eq!(s.cluster_size, 4096);
    let mut got = names(&s);
    got.sort();
    assert_eq!(
        got,
        [
            format!("Photos/{long}"),
            "chained.bin".into(),
            "contiguous photo.jpg".into(),
            "empty.txt".into(),
            "fragmented.dat".into(),
        ]
    );
    for (name, data) in [
        ("contiguous photo.jpg", &photo),
        ("chained.bin", &chained),
        ("fragmented.dat", &frag),
    ] {
        let cand = find(&s, name);
        assert_eq!(cand.confidence, Confidence::Good, "{name}");
        assert_eq!(&content(&mut r, cand), data, "{name}");
    }
    let deep = find(&s, long);
    assert_eq!(content(&mut r, deep), b"deep");
    assert_eq!((deep.modified, deep.local_times), (Some(WHEN), false));
    assert_eq!(content(&mut r, find(&s, "empty.txt")), b"");
    // The fragmented file's data skips the cluster between its pieces.
    let Data::Extents { extents, .. } = &find(&s, "fragmented.dat").data else {
        panic!("fragmented.dat has no extents");
    };
    assert_eq!(extents.len(), 2);
    survives_truncation(&img);

    ex.reuse(a.clusters[1], &[0xCC; 4096]);
    let (s, _) = scan_image(ex.image());
    assert_eq!(
        find(&s, "contiguous photo.jpg").confidence,
        Confidence::Partial
    );
    assert!(s.notes.is_empty(), "{:?}", s.notes);
}

#[test]
fn exfat_skips_a_partly_overwritten_set() {
    let mut ex = ExImg::new();
    let root = ex.root();
    let f = ex.add_file(root, "damaged name.txt", b"x", Layout::Contiguous);
    ex.add_file(root, "fine.txt", b"y", Layout::Contiguous);
    let g = ex.add_file(root, "fine2.txt", b"z", Layout::Contiguous);
    ex.delete(&f);
    ex.delete(&g);
    let mut img = ex.image();
    // A name character of the deleted set changed: its checksum no longer matches.
    img[f.set_off + 2 * 32 + 4] ^= 0x20;
    let (s, _) = scan_image(img);
    assert_eq!(names(&s), ["fine2.txt"]);
    assert_eq!(s.notes.len(), 1, "{:?}", s.notes);
}

#[test]
fn ntfs_finds_deleted_records() {
    let mut nt = NtfsImg::new();
    nt.add_file(NT_ROOT, "live.txt", None, b"live", false);
    let small = nt.add_file(
        NT_ROOT,
        "small note.txt",
        Some("SMALLN~1.TXT"),
        b"resident content",
        false,
    );
    let big_data = pattern(5000, 6);
    let big = nt.add_file(NT_ROOT, "big.bin", None, &big_data, true);
    let dir = nt.add_dir(NT_ROOT, "Projects");
    let plan = pattern(2100, 7);
    let inner = nt.add_file(dir, "plan.md", None, &plan, false);
    for f in [small, big, inner, dir] {
        nt.delete(f);
    }
    let img = nt.image();
    let (s, mut r) = scan_image(img.clone());
    assert_eq!(s.fs, FsKind::Ntfs);
    assert_eq!(s.cluster_size, 1024);
    let mut got = names(&s);
    got.sort();
    assert_eq!(got, ["Projects/plan.md", "big.bin", "small note.txt"]);
    let n = find(&s, "small note.txt");
    assert!(matches!(n.data, Data::Resident(_)));
    assert_eq!((n.confidence, n.modified), (Confidence::Good, Some(WHEN)));
    assert_eq!(n.location, small.0);
    assert_eq!(content(&mut r, n), b"resident content");
    let b = find(&s, "big.bin");
    assert_eq!(b.confidence, Confidence::Good);
    assert_eq!(content(&mut r, b), big_data);
    assert_eq!(content(&mut r, find(&s, "plan.md")), plan);
    assert!(s.notes.is_empty(), "{:?}", s.notes);
    survives_truncation(&img);

    nt.reuse(nt.first_cluster(big), &[0xDD; 1024]);
    let (s, _) = scan_image(nt.image());
    assert_eq!(find(&s, "big.bin").confidence, Confidence::Partial);

    // A torn record (an update sequence number that does not match) is skipped.
    let (s, _) = scan_image(nt.image_torn(small.0));
    let mut got = names(&s);
    got.sort();
    assert_eq!(got, ["Projects/plan.md", "big.bin"]);
}

#[test]
fn ntfs_path_unknown_when_the_parent_record_was_reused() {
    let mut nt = NtfsImg::new();
    let dir = nt.add_dir(NT_ROOT, "Old");
    let f = nt.add_file(dir, "orphan.txt", None, b"orphan", false);
    nt.delete(f);
    nt.delete(dir);
    // The folder's record bumped twice: deleted, then reused and deleted again.
    nt.delete(dir);
    let (s, _) = scan_image(nt.image());
    assert_eq!(find(&s, "orphan.txt").folder, None);
    assert_eq!(find(&s, "orphan.txt").path(), "?/orphan.txt");
}

#[test]
fn run_lists_decode() {
    let runs = [(100u64, 3u64), (40, 2), (70_000, 1)];
    let enc = testimg::encode_runs(&runs);
    let dec = ntfs::decode_runs(&enc).unwrap();
    let want: Vec<(Option<u64>, u64)> = runs.iter().map(|&(l, n)| (Some(l), n)).collect();
    assert_eq!(dec, want);
    // Sparse run (no offset bytes), then a run back to cluster 5.
    assert_eq!(
        ntfs::decode_runs(&[0x01, 0x04, 0x11, 0x02, 0x05, 0x00]).unwrap(),
        [(None, 4), (Some(5), 2)]
    );
    // A run before cluster 0, a zero length, oversize fields, a cut-off run.
    assert_eq!(ntfs::decode_runs(&[0x11, 0x01, 0xFF, 0x00]), None);
    assert_eq!(ntfs::decode_runs(&[0x11, 0x00, 0x05, 0x00]), None);
    assert_eq!(ntfs::decode_runs(&[0x19, 0x01]), None);
    assert_eq!(ntfs::decode_runs(&[0x22, 0x01]), None);
}

#[test]
fn unknown_and_garbage_volumes_fail_cleanly() {
    for img in [vec![], vec![0u8; 100], vec![0u8; 4096], vec![0xFFu8; 4096]] {
        assert!(matches!(
            scan(&mut Cursor::new(img)),
            Err(RecoverError::UnknownFs)
        ));
    }
    // A FAT boot sector with nothing after it.
    let fat = FatImg::new(16);
    let mut boot = fat.img[..512].to_vec();
    assert!(scan(&mut Cursor::new(boot.clone())).is_err());
    boot[13] = 3; // not a power of two
    assert!(matches!(
        scan(&mut Cursor::new(boot)),
        Err(RecoverError::UnknownFs)
    ));
}

#[test]
fn recycle_bin_info_files_parse() {
    let v2 = recycle::parse_info(&info_v2(r"C:\Data\Reports\q3 report.docx", 1234)).unwrap();
    assert_eq!(v2.original, r"C:\Data\Reports\q3 report.docx");
    assert_eq!((v2.size, v2.deleted), (1234, Some(WHEN)));
    assert_eq!(v2.name(), "q3 report.docx");
    let v1 = recycle::parse_info(&info_v1(r"D:\old.txt", 7)).unwrap();
    assert_eq!((v1.original.as_str(), v1.size), (r"D:\old.txt", 7));
    let mut bad = info_v2("x", 1);
    bad[0] = 3;
    assert!(recycle::parse_info(&bad).is_err());
    let full = info_v2(r"C:\a.txt", 1);
    for cut in 0..full.len() - 2 {
        assert!(recycle::parse_info(&full[..cut]).is_err(), "cut {cut}");
    }
    let mut huge = info_v2("x", 1);
    huge[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(recycle::parse_info(&huge).is_err());
    assert!(recycle::parse_info(&info_v1("", 1)).is_err());
}

#[test]
fn recycle_bin_folder_lists_items() {
    let dir = tempfile::tempdir().unwrap();
    let sid = dir.path().join("S-1-5-21-1000");
    std::fs::create_dir(&sid).unwrap();
    std::fs::write(sid.join("$IAB12CD.docx"), info_v2(r"C:\Data\a.docx", 10)).unwrap();
    std::fs::write(sid.join("$RAB12CD.docx"), b"0123456789").unwrap();
    std::fs::write(sid.join("$IZZ99.txt"), info_v1(r"C:\Data\b.txt", 3)).unwrap();
    std::fs::write(sid.join("$Ibroken"), b"not an info file").unwrap();
    std::fs::write(sid.join("desktop.ini"), b"[x]").unwrap();
    for at in [dir.path(), sid.as_path()] {
        let items = recycle::list(at).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].info.name(), "a.docx");
        assert_eq!(items[0].data_file, Some(sid.join("$RAB12CD.docx")));
        assert_eq!(items[1].info.original, r"C:\Data\b.txt");
        assert_eq!(items[1].data_file, None);
    }
}

fn ntfs_with_deleted_file() -> (Scan, Cursor<Vec<u8>>, Vec<u8>) {
    let mut nt = NtfsImg::new();
    let data = pattern(3000, 9);
    let f = nt.add_file(NT_ROOT, "report:final?.txt", None, &data, false);
    nt.delete(f);
    let (s, r) = scan_image(nt.image());
    (s, r, data)
}

#[test]
fn restore_refuses_the_source_volume() {
    let (s, mut r, _) = ntfs_with_deleted_file();
    let dest = tempfile::tempdir().unwrap();
    let same = volume_of(dest.path()).unwrap();
    let err = restore(&mut r, same, &s.candidates[0], dest.path()).unwrap_err();
    assert!(matches!(err, RecoverError::SameVolume), "{err}");
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);

    // End to end: an image file counts as the volume it is stored on.
    let img_dir = tempfile::tempdir().unwrap();
    let image = img_dir.path().join("volume.img");
    std::fs::write(&image, r.get_ref()).unwrap();
    let mut src = Source::open(image.to_str().unwrap()).unwrap();
    assert!(!src.device);
    assert_eq!(src.id, volume_of(img_dir.path()).unwrap());
    let s = scan(src.reader()).unwrap();
    assert_eq!(s.candidates.len(), 1);
    let id = src.id;
    let err = restore(src.reader(), id, &s.candidates[0], dest.path()).unwrap_err();
    assert!(matches!(err, RecoverError::SameVolume));
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
    let err = Source::open(img_dir.path().to_str().unwrap()).err().unwrap();
    assert!(matches!(err, RecoverError::Unrecoverable(_)), "{err}");
}

#[test]
fn restore_writes_to_another_volume() {
    let (s, mut r, data) = ntfs_with_deleted_file();
    let dest = tempfile::tempdir().unwrap();
    // A volume id no real volume has: stands for the source being elsewhere.
    let elsewhere = VolumeId(u64::MAX);
    let c = &s.candidates[0];
    let first = restore(&mut r, elsewhere, c, dest.path()).unwrap();
    assert_eq!(first, dest.path().join("report_final_.txt"));
    assert_eq!(std::fs::read(&first).unwrap(), data);
    let modified = std::fs::metadata(&first).unwrap().modified().unwrap();
    assert_eq!(
        modified,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(WHEN as u64)
    );
    let second = restore(&mut r, elsewhere, c, dest.path()).unwrap();
    assert_eq!(second, dest.path().join("report_final_ (2).txt"));

    // Data the volume no longer holds: no file is left behind.
    let mut short = Cursor::new(r.get_ref()[..40 * 1024].to_vec());
    let err = restore(&mut short, elsewhere, c, dest.path()).unwrap_err();
    assert!(matches!(err, RecoverError::Unrecoverable(_)), "{err}");
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 2);
}

#[test]
fn file_names_are_made_safe() {
    assert_eq!(safe_file_name("a/b\\c:d*e?.txt"), "a_b_c_d_e_.txt");
    assert_eq!(safe_file_name("name. . "), "name");
    assert_eq!(safe_file_name(""), "recovered");
    assert_eq!(safe_file_name("..."), "recovered");
    assert_eq!(safe_file_name("con.txt"), "_con.txt");
    assert_eq!(safe_file_name("COM1"), "_COM1");
    assert_eq!(safe_file_name("COMX"), "COMX");
    assert_eq!(safe_file_name("tab\there"), "tab_here");
}
