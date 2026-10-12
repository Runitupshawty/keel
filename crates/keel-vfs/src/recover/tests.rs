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
    s.candidates.iter().map(|c| s.path(c)).collect()
}

fn content(r: &mut Cursor<Vec<u8>>, c: &Candidate) -> Vec<u8> {
    let mut out = Vec::new();
    let n = read_data(r, c, &mut out).unwrap();
    assert_eq!(n, c.size);
    out
}

/// No panic on any prefix of the image; every candidate reads or fails cleanly.
fn survives_truncation(img: &[u8]) {
    let cuts = [
        0,
        100,
        511,
        512,
        1000,
        4096,
        img.len() / 3,
        img.len() / 2,
        img.len() - 1,
    ];
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
        let d = fat.add_file(
            old,
            Some("gone.txt"),
            b"GONE    TXT",
            b"in a deleted folder",
        );
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
    let f = fat.add_file(
        root,
        Some("two clusters.bin"),
        b"TWOCLU~1BIN",
        &pattern(1000, 3),
    );
    fat.delete(&f);
    let first = f.clusters[0];
    let (s, _) = scan_image(fat.img.clone());
    assert_eq!(find(&s, "two clusters.bin").confidence, Confidence::Good);
    fat.reuse(first, &[0xAA; 512]);
    let (s, _) = scan_image(fat.img.clone());
    assert_eq!(find(&s, "two clusters.bin").confidence, Confidence::Partial);
    fat.reuse(first, &[0xBB; 1024]);
    let (s, _) = scan_image(fat.img.clone());
    assert_eq!(
        find(&s, "two clusters.bin").confidence,
        Confidence::Overwritten
    );
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
    assert_eq!(s.path(find(&s, "orphan.txt")), "?/orphan.txt");
}

/// A FAT12 volume with 4 KiB clusters written entry by entry, for layouts the image
/// builders refuse: folders spanning chains, thousands of entries, shared clusters.
struct RawFat {
    img: Vec<u8>,
}

const RAW_CLUSTER: usize = 4096;
const RAW_FAT_OFF: usize = 512;
const RAW_ROOT_OFF: usize = 3 * 512;
const RAW_DATA_OFF: usize = 5 * 512;

impl RawFat {
    /// `clusters` data clusters (under 680: one FAT of two sectors), a 32-entry root.
    fn new(clusters: usize) -> Self {
        let total = 5 + clusters * 8;
        let mut img = vec![0u8; total * 512];
        img[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
        img[3..11].copy_from_slice(b"MSWIN4.1");
        img[11..13].copy_from_slice(&512u16.to_le_bytes());
        img[13] = 8;
        img[14..16].copy_from_slice(&1u16.to_le_bytes());
        img[16] = 1;
        img[17..19].copy_from_slice(&32u16.to_le_bytes());
        img[19..21].copy_from_slice(&(total as u16).to_le_bytes());
        img[21] = 0xF8;
        img[22..24].copy_from_slice(&2u16.to_le_bytes());
        img[510..512].copy_from_slice(&[0x55, 0xAA]);
        Self { img }
    }

    fn set_fat(&mut self, n: u32, v: u16) {
        let off = RAW_FAT_OFF + n as usize + n as usize / 2;
        let cur = u16::from_le_bytes([self.img[off], self.img[off + 1]]);
        let v = v & 0xFFF;
        let new = match n & 1 {
            1 => (cur & 0x000F) | (v << 4),
            _ => (cur & 0xF000) | v,
        };
        self.img[off..off + 2].copy_from_slice(&new.to_le_bytes());
    }

    /// A folder in clusters `first..first + n`, chained, holding `entries`.
    fn folder(&mut self, first: u32, entries: &[[u8; 32]]) {
        let n = (entries.len() * 32).div_ceil(RAW_CLUSTER).max(1) as u32;
        for c in first..first + n {
            self.set_fat(
                c,
                if c + 1 < first + n {
                    (c + 1) as u16
                } else {
                    0xFFF
                },
            );
        }
        let base = RAW_DATA_OFF + (first as usize - 2) * RAW_CLUSTER;
        for (i, e) in entries.iter().enumerate() {
            self.img[base + i * 32..base + i * 32 + 32].copy_from_slice(e);
        }
    }

    fn root(&mut self, entries: &[[u8; 32]]) {
        assert!(entries.len() <= 32);
        for (i, e) in entries.iter().enumerate() {
            let at = RAW_ROOT_OFF + i * 32;
            self.img[at..at + 32].copy_from_slice(e);
        }
    }
}

fn short_entry(name: &[u8; 11], attr: u8, first: u32, size: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(name);
    e[11] = attr;
    e[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
    e[26..28].copy_from_slice(&(first as u16).to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

/// The LFN entries and the short entry of a live folder named `long`.
fn long_dir_entries(long: &str, short: &[u8; 11], first: u32) -> Vec<[u8; 32]> {
    let mut units: Vec<u16> = long.encode_utf16().collect();
    let parts = units.len().div_ceil(13);
    if !units.len().is_multiple_of(13) {
        units.push(0);
    }
    units.resize(parts * 13, 0xFFFF);
    let chk = fat::lfn_checksum(short);
    let mut out: Vec<[u8; 32]> = (0..parts)
        .map(|i| {
            let ord = parts - i;
            let mut e = [0u8; 32];
            e[0] = ord as u8 | if i == 0 { 0x40 } else { 0 };
            e[11] = 0x0F;
            e[13] = chk;
            for (k, &u) in units[(ord - 1) * 13..ord * 13].iter().enumerate() {
                let at = match k {
                    0..5 => 1 + 2 * k,
                    5..11 => 14 + 2 * (k - 5),
                    _ => 28 + 2 * (k - 11),
                };
                e[at..at + 2].copy_from_slice(&u.to_le_bytes());
            }
            e
        })
        .collect();
    out.push(short_entry(short, 0x10, first, 0));
    out
}

/// 63 nested folders, each named with 255 three-byte characters (a path of about 48 KB),
/// and in the deepest one `files` erased entries: each one a candidate.
fn deep_wide_fat(files: usize) -> Vec<u8> {
    const LEVELS: u32 = 63;
    let name = "\u{20AC}".repeat(255);
    let mut fat = RawFat::new(400);
    let short = |i: u32| -> [u8; 11] {
        let mut s = *b"DIR        ";
        s[3..8].copy_from_slice(format!("{i:05}").as_bytes());
        s
    };
    fat.root(&long_dir_entries(&name, &short(1), 2)[..]);
    for level in 1..LEVELS {
        let c = 1 + level;
        let mut entries = vec![short_entry(b".          ", 0x10, c, 0)];
        entries.extend(long_dir_entries(&name, &short(level + 1), c + 1));
        fat.folder(c, &entries);
    }
    let mut deepest = vec![short_entry(b".          ", 0x10, 1 + LEVELS, 0)];
    deepest.extend((0..files).map(|_| {
        let mut e = short_entry(b"XILE    TXT", 0x20, 0, 0);
        e[0] = 0xE5;
        e
    }));
    fat.folder(1 + LEVELS, &deepest);
    fat.img
}

#[test]
fn a_deep_folder_of_many_entries_costs_its_entries_not_its_depth() {
    // Each of these candidates once held its own copy of the 48 KB path: about 1 GB.
    let files = 20_000;
    let img = deep_wide_fat(files);
    let s = scan_limited(&mut Cursor::new(img.clone()), 16 << 20).unwrap();
    assert_eq!(s.fs, FsKind::Fat12);
    assert!(s.notes.is_empty(), "{:?}", s.notes);
    assert_eq!(s.candidates.len(), files);
    assert_eq!(s.folders.len(), 64, "the root and 63 folders, each once");
    let folder = s.candidates[0].folder.unwrap();
    assert!(s.candidates.iter().all(|c| c.folder == Some(folder)));
    let path = s.folder_path(folder).unwrap();
    assert_eq!(path.len(), 63 * 765 + 62);
    assert_eq!(s.path(&s.candidates[0]), format!("{path}/_ILE.TXT"));

    // Under a smaller limit the scan stops early and says so.
    let s = scan_limited(&mut Cursor::new(img), 1 << 20).unwrap();
    assert!(!s.candidates.is_empty() && s.candidates.len() < files);
    assert_eq!(s.notes.len(), 1, "{:?}", s.notes);
    assert!(s.notes[0].contains("memory limit"), "{:?}", s.notes);
}

#[test]
fn thousands_of_folders_on_one_cluster_are_bounded() {
    // 20,000 live folder entries all naming cluster 300 (read once) and a deleted file.
    let mut fat = RawFat::new(400);
    fat.root(&[short_entry(b"TOP        ", 0x10, 2, 0)]);
    let mut top: Vec<[u8; 32]> = (0..20_000u32)
        .map(|i| {
            let mut name = *b"SUB        ";
            name[3..8].copy_from_slice(format!("{i:05}").as_bytes());
            short_entry(&name, 0x10, 300, 0)
        })
        .collect();
    let mut gone = short_entry(b"XONE    TXT", 0x20, 0, 0);
    gone[0] = 0xE5;
    top.push(gone);
    fat.folder(2, &top);
    fat.folder(300, &[short_entry(b".          ", 0x10, 300, 0)]);
    let s = scan_limited(&mut Cursor::new(fat.img.clone()), 64 << 20).unwrap();
    assert_eq!(names(&s), ["TOP/_ONE.TXT"]);
    assert!(s.folders.len() <= 20_002, "{}", s.folders.len());
    // A limit below what the folder table needs stops the scan with a note.
    let s = scan_limited(&mut Cursor::new(fat.img), 256 << 10).unwrap();
    assert!(
        s.notes.iter().any(|n| n.contains("memory limit")),
        "{:?}",
        s.notes
    );
}

#[test]
fn ntfs_and_exfat_files_share_their_folder() {
    let mut nt = NtfsImg::new();
    let mut dir = NT_ROOT;
    for i in 0..10 {
        dir = nt.add_dir(dir, &format!("level {i}"));
    }
    for i in 0..5 {
        let f = nt.add_file(dir, &format!("f{i}.txt"), None, b"x", false);
        nt.delete(f);
    }
    let (s, _) = scan_image(nt.image());
    assert_eq!(s.candidates.len(), 5);
    assert_eq!(s.folders.len(), 11);
    let folder = s.candidates[0].folder;
    assert!(s.candidates.iter().all(|c| c.folder == folder));
    assert_eq!(
        s.path(&s.candidates[0]),
        "level 0/level 1/level 2/level 3/level 4/level 5/level 6/level 7/level 8/level 9/f0.txt"
    );

    let mut ex = ExImg::new();
    let mut dir = ex.root();
    for i in 0..4 {
        dir = ex.add_dir(dir, &format!("d{i}")).0;
    }
    for i in 0..3 {
        let f = ex.add_file(dir, &format!("e{i}.bin"), b"y", Layout::Contiguous);
        ex.delete(&f);
    }
    let (s, _) = scan_image(ex.image());
    assert_eq!(s.candidates.len(), 3);
    assert_eq!(s.folders.len(), 5);
    assert!(s
        .candidates
        .iter()
        .all(|c| c.folder == s.candidates[0].folder));
    assert_eq!(s.path(&s.candidates[2]), "d0/d1/d2/d3/e2.bin");
}

#[test]
fn folder_paths_of_a_damaged_table_end() {
    let folder = |name: &str, parent: Option<u32>| Folder {
        name: name.into(),
        parent,
    };
    let s = Scan {
        fs: FsKind::Fat16,
        cluster_size: 512,
        volume_size: 0,
        candidates: Vec::new(),
        folders: vec![
            folder("", None),
            folder("a", Some(0)),
            folder("loop", Some(2)),
            folder("ahead", Some(9)),
            folder("b", Some(1)),
        ],
        notes: Vec::new(),
    };
    assert_eq!(s.folder_path(0).as_deref(), Some(""));
    assert_eq!(s.folder_path(4).as_deref(), Some("a/b"));
    assert_eq!(s.folder_path(2), None);
    assert_eq!(s.folder_path(3), None);
    assert_eq!(s.folder_path(99), None);
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
    let err = Source::open(img_dir.path().to_str().unwrap())
        .err()
        .unwrap();
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
