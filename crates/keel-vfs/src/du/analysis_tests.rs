//! The analysis tabs' models over scans of temp fixture trees.

use super::analysis::{counted_file, csv_cell, parse_size, SizeRange};
use super::duplicates::{self, Algorithm, HashCache};
use super::search::{search, ItemType, Query};
use super::storage::{analyze, Tier};
use super::*;
use std::collections::HashMap;
use std::fs;

fn scan_at(root: &Path, opts: &ScanOptions) -> Tree {
    scan(&[Root::Path(root.to_owned())], opts, &ScanHandle::new()).unwrap()
}

fn put(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn id_of(t: &Tree, path: &Path) -> NodeId {
    t.find(path)
        .unwrap_or_else(|| panic!("{} not in the scan", path.display()))
}

fn file_names(t: &Tree, ids: &[NodeId]) -> Vec<String> {
    ids.iter().map(|&i| t.node(i).name.to_string()).collect()
}

/// Bytes that differ from file to file only where `tag` says (a counter mixed in).
fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 ^ tag).collect()
}

// ---- shared ----

#[test]
fn sizes_parse_decimal_and_binary_units() {
    assert_eq!(parse_size("10"), Some(10));
    assert_eq!(parse_size(" 1.5 MB "), Some(1_500_000));
    assert_eq!(parse_size("2kb"), Some(2_000));
    assert_eq!(parse_size("1 KiB"), Some(1024));
    assert_eq!(parse_size("3 GiB"), Some(3 << 30));
    assert_eq!(parse_size("1 TB"), Some(1_000_000_000_000));
    assert_eq!(parse_size("-1"), None);
    assert_eq!(parse_size("ten"), None);
    assert_eq!(parse_size("5 parsecs"), None);
    assert_eq!(parse_size("1e30 TB"), None);

    let r = SizeRange::parse("1 KB", "").unwrap();
    assert_eq!((r.min, r.max), (1000, u64::MAX));
    assert!(r.contains(1000) && !r.contains(999));
    assert!(SizeRange::parse("", "").unwrap().is_any());
    assert!(SizeRange::parse("2 MB", "1 MB").is_err());
    assert!(SizeRange::parse("lots", "").is_err());
}

#[test]
fn csv_cells_are_quoted_and_formulas_neutralized() {
    assert_eq!(csv_cell("plain"), "\"plain\"");
    assert_eq!(csv_cell("a \"b\", c"), "\"a \"\"b\"\", c\"");
    assert_eq!(csv_cell("=HYPERLINK(1)"), "\"'=HYPERLINK(1)\"");
    assert_eq!(csv_cell(" +1"), "\"' +1\"");
    assert_eq!(csv_cell("@x"), "\"'@x\"");
    assert_eq!(csv_cell("-2"), "\"'-2\"");
    assert_eq!(csv_cell("\tx"), "\"'\tx\"");
    assert_eq!(csv_cell("a-b"), "\"a-b\"");
}

// ---- largest files ----

#[test]
fn largest_files_top_n_in_range_biggest_first() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    for (name, len) in [
        ("a.bin", 500),
        ("sub/b.txt", 3000),
        ("sub/deep/c.txt", 2000),
        ("d.bin", 10),
        ("e.bin", 3000),
    ] {
        put(&r.join(name), &vec![1; len]);
    }
    let t = scan_at(r, &ScanOptions::default());

    let all = largest::largest(&t, 100, SizeRange::default());
    let sizes: Vec<u64> = all.iter().map(|f| f.bytes).collect();
    assert_eq!(sizes, [3000, 3000, 2000, 500, 10]);
    // Equal sizes by path.
    assert!(all[0].path < all[1].path);
    assert_eq!(all[2].path, r.join("sub").join("deep").join("c.txt"));
    assert_eq!(all[2].extension, "txt");
    assert!(all[2].modified_ns.is_some());

    let top2 = largest::largest(&t, 2, SizeRange::default());
    assert_eq!(
        top2.iter().map(|f| f.bytes).collect::<Vec<_>>(),
        [3000, 3000]
    );

    let mid = largest::largest(
        &t,
        10,
        SizeRange {
            min: 100,
            max: 2500,
        },
    );
    assert_eq!(mid.iter().map(|f| f.bytes).collect::<Vec<_>>(), [2000, 500]);

    assert!(largest::largest(&t, 0, SizeRange::default()).is_empty());
}

#[test]
fn largest_files_count_a_hard_linked_file_once() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    put(&r.join("big.bin"), &vec![7; 4000]);
    fs::hard_link(r.join("big.bin"), r.join("again.bin")).unwrap();
    put(&r.join("small.bin"), &[7; 10]);
    let t = scan_at(r, &ScanOptions::default());
    let rows = largest::largest(&t, 10, SizeRange::default());
    assert_eq!(rows.iter().map(|f| f.bytes).collect::<Vec<_>>(), [4000, 10]);
    let linked = ScanOptions {
        count_hard_links_once: false,
        ..ScanOptions::default()
    };
    let t = scan_at(r, &linked);
    assert_eq!(largest::largest(&t, 10, SizeRange::default()).len(), 3);
}

// ---- search ----

fn search_fixture() -> (tempfile::TempDir, Tree) {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    put(&r.join("Photos/Holiday.JPG"), &vec![0; 2000]);
    put(&r.join("Photos/beach.jpg"), &vec![0; 500]);
    put(&r.join("Photos/notes.txt"), &[0; 50]);
    put(&r.join("Docs/report-2026.pdf"), &vec![0; 900]);
    put(&r.join("Docs/holiday plan.txt"), &[0; 10]);
    fs::hard_link(r.join("Photos/beach.jpg"), r.join("Docs/beach copy.jpg")).unwrap();
    let t = scan_at(r, &ScanOptions::default());
    (tmp, t)
}

#[test]
fn search_by_name_substring_and_case() {
    let (_tmp, t) = search_fixture();
    let none = HashMap::new();
    let q = |name: &str, case_sensitive| Query {
        name: name.into(),
        case_sensitive,
        ..Query::default()
    };
    let mut hits = file_names(&t, &search(&t, &q("holiday", false), &none).unwrap().nodes);
    hits.sort();
    assert_eq!(hits, ["Holiday.JPG", "holiday plan.txt"]);
    let hits = search(&t, &q("Holiday", true), &none).unwrap();
    assert_eq!(file_names(&t, &hits.nodes), ["Holiday.JPG"]);
    assert_eq!((hits.files, hits.folders, hits.bytes), (1, 0, 2000));
    // Folders match by name too.
    let hits = search(&t, &q("phot", false), &none).unwrap();
    assert_eq!(file_names(&t, &hits.nodes), ["Photos"]);
    assert_eq!((hits.files, hits.folders, hits.bytes), (0, 1, 0));
    // An empty name matches every item: the root, 2 folders, 6 files.
    let hits = search(&t, &Query::default(), &none).unwrap();
    assert_eq!((hits.files, hits.folders), (6, 3));
}

#[test]
fn search_by_regex_type_size_and_extension() {
    let (_tmp, t) = search_fixture();
    let none = HashMap::new();
    let q = Query {
        name: r"^\w+-\d{4}\.".into(),
        regex: true,
        ..Query::default()
    };
    assert_eq!(
        file_names(&t, &search(&t, &q, &none).unwrap().nodes),
        ["report-2026.pdf"]
    );
    let bad = Query {
        name: "(".into(),
        regex: true,
        ..Query::default()
    };
    assert!(search(&t, &bad, &none).is_err());

    let jpgs = Query {
        extensions: Query::parse_extensions(".JPG, png"),
        ..Query::default()
    };
    assert_eq!(jpgs.extensions, ["jpg", "png"]);
    let hits = search(&t, &jpgs, &none).unwrap();
    assert_eq!(hits.files, 3);
    // The two names of the hard-linked beach.jpg count their bytes once.
    assert_eq!(hits.bytes, 2000 + 500);

    let big = Query {
        size: SizeRange {
            min: 600,
            max: u64::MAX,
        },
        kind: ItemType::File,
        ..Query::default()
    };
    let mut hits = file_names(&t, &search(&t, &big, &none).unwrap().nodes);
    hits.sort();
    assert_eq!(hits, ["Holiday.JPG", "report-2026.pdf"]);

    let folders = Query {
        kind: ItemType::Folder,
        ..Query::default()
    };
    let hits = search(&t, &folders, &none).unwrap();
    assert_eq!((hits.files, hits.folders), (0, 3));
}

#[test]
fn search_by_owner_needs_the_owners() {
    let (tmp, t) = search_fixture();
    let report = id_of(&t, &tmp.path().join("Docs/report-2026.pdf"));
    let notes = id_of(&t, &tmp.path().join("Photos/notes.txt"));
    let owners = HashMap::from([
        (report, "EXAMPLE\\alice (S-1-5-21-1)".to_string()),
        (notes, "bob (1001)".to_string()),
    ]);
    let q = Query {
        owner: "ALICE".into(),
        kind: ItemType::File,
        ..Query::default()
    };
    let hits = search(&t, &q, &owners).unwrap();
    assert_eq!(hits.nodes, [report]);
    // The other 4 files have no owner known.
    assert_eq!(hits.owners_unknown, 4);
    let q = Query {
        owner: "1001".into(),
        kind: ItemType::File,
        ..Query::default()
    };
    assert_eq!(search(&t, &q, &owners).unwrap().nodes, [notes]);
}

// ---- storage analytics ----

const DAY: i64 = 86_400 * 1_000_000_000;

#[test]
fn storage_tiers_are_checked() {
    assert!(Tier::check(&Tier::defaults()).is_ok());
    assert!(Tier::check(&[]).is_err());
    let t = |days, cost| Tier {
        name: "t".into(),
        younger_than_days: days,
        cost_per_gb: cost,
    };
    assert!(Tier::check(&[t(Some(30), 0.0)]).is_err(), "no last tier");
    assert!(Tier::check(&[t(Some(30), 0.0), t(Some(30), 0.0), t(None, 0.0)]).is_err());
    assert!(Tier::check(&[t(None, 0.0), t(None, 0.0)]).is_err());
    assert!(Tier::check(&[t(Some(1), -1.0), t(None, 0.0)]).is_err());
    assert!(Tier::check(&[t(None, f64::NAN)]).is_err());
    assert!(Tier::check(&[t(None, 0.02)]).is_ok());
}

#[test]
fn storage_analytics_by_age_extension_and_top_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    put(&r.join("new.txt"), &vec![0; 1_000]);
    put(&r.join("Media/old.mp4"), &vec![0; 4_000]);
    put(&r.join("Media/deep/mid.mp4"), &vec![0; 2_000]);
    put(&r.join("Docs/mid.txt"), &vec![0; 500]);
    put(&r.join("Docs/undated.txt"), &[0; 7]);
    fs::hard_link(r.join("Media/old.mp4"), r.join("Media/old-link.mp4")).unwrap();
    let mut t = scan_at(r, &ScanOptions::default());
    let now = 1_000 * DAY;
    let mut age = |rel: &str, days: Option<i64>| {
        let id = id_of(&t, &r.join(rel));
        t.nodes[id as usize].modified = days.map_or(UNKNOWN_TIME, |d| now - d * DAY);
    };
    age("new.txt", Some(1));
    age("Media/old.mp4", Some(800));
    age("Media/old-link.mp4", Some(800));
    age("Media/deep/mid.mp4", Some(100));
    age("Docs/mid.txt", Some(30)); // exactly 30 days: no longer hot
    age("Docs/undated.txt", None);

    let tiers = vec![
        Tier {
            name: "Hot".into(),
            younger_than_days: Some(30),
            cost_per_gb: 0.10,
        },
        Tier {
            name: "Warm".into(),
            younger_than_days: Some(365),
            cost_per_gb: 0.01,
        },
        Tier {
            name: "Cold".into(),
            younger_than_days: None,
            cost_per_gb: 0.001,
        },
    ];
    let a = analyze(&t, &tiers, now).unwrap();
    let bytes: Vec<u64> = a.tiers.iter().map(|x| x.bytes).collect();
    // The hard-linked old.mp4 counts once.
    assert_eq!(bytes, [1_000, 2_500, 4_000]);
    assert_eq!(
        a.tiers.iter().map(|x| x.files).collect::<Vec<_>>(),
        [1, 2, 1]
    );
    assert_eq!(
        (a.undated.files, a.undated.bytes, a.undated.monthly_cost),
        (1, 7, 0.0)
    );
    assert_eq!(a.total.bytes, 7_507);
    let expect = 1_000.0 / 1e9 * 0.10 + 2_500.0 / 1e9 * 0.01 + 4_000.0 / 1e9 * 0.001;
    assert!((a.total.monthly_cost - expect).abs() < 1e-15);
    assert!((a.tiers[0].monthly_cost - 1e-7).abs() < 1e-15);

    // By extension, biggest first, with bytes per tier.
    let ext: Vec<(&str, u64)> = a
        .extensions
        .iter()
        .map(|(e, t)| (e.as_str(), t.bytes))
        .collect();
    assert_eq!(ext, [("mp4", 6_000), ("txt", 1_507)]);
    assert_eq!(a.extensions[0].1.tier_bytes, [0, 2_000, 4_000]);

    // By top folder; root files under the root's path.
    let folders: Vec<(String, u64)> = a
        .folders
        .iter()
        .map(|(f, t)| (f.clone(), t.bytes))
        .collect();
    assert_eq!(
        folders,
        [
            (r.join("Media").display().to_string(), 6_000),
            (r.display().to_string(), 1_000),
            (r.join("Docs").display().to_string(), 507),
        ]
    );
    assert!(analyze(&t, &[], now).is_err());
}

// ---- duplicates ----

fn dup_options() -> duplicates::Options {
    duplicates::Options::default()
}

fn group_names(g: &duplicates::Group) -> Vec<String> {
    (g.files.iter())
        .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn duplicates_by_size_prefix_and_full_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    let big = pattern(300_000, 0);
    let mut big_tail = big.clone();
    *big_tail.last_mut().unwrap() ^= 0xff; // same size and prefix, other bytes
    put(&r.join("a/small1.txt"), b"same small bytes");
    put(&r.join("b/small2.txt"), b"same small bytes");
    put(&r.join("c/small3.txt"), b"other small byte"); // same size, other bytes
    put(&r.join("a/big1.bin"), &big);
    put(&r.join("b/big2.bin"), &big);
    put(&r.join("c/big3.bin"), &big_tail);
    put(&r.join("empty1"), b"");
    put(&r.join("empty2"), b"");
    fs::hard_link(r.join("a/big1.bin"), r.join("c/big1-link.bin")).unwrap();
    let t = scan_at(r, &ScanOptions::default());

    for algorithm in Algorithm::ALL {
        let opts = duplicates::Options {
            algorithm,
            ..dup_options()
        };
        let rep = duplicates::find(&t, &opts, &HashCache::new(), &ScanHandle::new());
        assert!(!rep.cancelled);
        assert!(rep.skipped.is_empty(), "{:?}", rep.skipped);
        assert_eq!(rep.groups.len(), 2, "{algorithm:?}");
        // Most reclaimable first; the hard link is not a duplicate of its own file.
        // (Which name of the hard-linked big1 stands for it depends on the walk.)
        let big = group_names(&rep.groups[0]);
        assert_eq!(big.len(), 2);
        assert!(big.contains(&"big2.bin".to_string()), "{big:?}");
        assert_eq!(group_names(&rep.groups[1]), ["small1.txt", "small2.txt"]);
        assert_eq!(rep.groups[0].reclaimable(), 300_000);
        assert_eq!(rep.reclaimable(), 300_000 + 16);
        assert!(rep
            .groups
            .iter()
            .all(|g| !g.probable && g.algorithm == algorithm));
        let hex_len = if algorithm == Algorithm::Xxh3 { 32 } else { 64 };
        assert_eq!(rep.groups[0].digest.len(), hex_len);
    }
    let blake = duplicates::find(&t, &dup_options(), &HashCache::new(), &ScanHandle::new());
    assert_eq!(blake.groups[0].digest, blake3::hash(&big).to_hex().as_str());
}

#[test]
fn duplicates_sparse_hashing_marks_probable_and_verify_reads_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    let size = 2 << 20;
    let a = pattern(size, 3);
    let mut b = a.clone();
    // A byte no sample reads: between the first two samples.
    b[100_000] ^= 1;
    put(&r.join("one.bin"), &a);
    put(&r.join("two.bin"), &b);
    let t = scan_at(r, &ScanOptions::default());
    let sparse = duplicates::Options {
        sparse_above: 1 << 20,
        ..dup_options()
    };
    let rep = duplicates::find(&t, &sparse, &HashCache::new(), &ScanHandle::new());
    assert_eq!(rep.groups.len(), 1);
    assert!(rep.groups[0].probable);
    // Reading whole tells them apart.
    let err = duplicates::verify(&rep.groups[0], 0, false, &ScanHandle::new()).unwrap_err();
    assert!(
        err.to_string().contains("no longer has the same bytes"),
        "{err:#}"
    );
    let whole = duplicates::Options {
        sparse: false,
        ..sparse
    };
    let rep = duplicates::find(&t, &whole, &HashCache::new(), &ScanHandle::new());
    assert!(rep.groups.is_empty());

    let ranges = duplicates::sparse_ranges(size as u64);
    assert_eq!(ranges.len(), 16);
    assert_eq!(ranges[0], (0, 65_536));
    assert_eq!(*ranges.last().unwrap(), (size as u64 - 65_536, 65_536));
    assert!(ranges.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0));
    assert_eq!(duplicates::sparse_ranges(1 << 20), [(0, 1 << 20)]);
}

#[test]
fn duplicates_skip_changed_and_cloud_files_and_reuse_library_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    let bytes = pattern(100_000, 9);
    for n in ["x1", "x2", "x3", "x4"] {
        put(&r.join(n), &bytes);
    }
    let mut t = scan_at(r, &ScanOptions::default());
    // x4 is a placeholder per the scan: never opened, not listed.
    let x4 = id_of(&t, &r.join("x4"));
    t.nodes[x4 as usize].flags |= flag::CLOUD;
    // x3 changes after the scan.
    put(&r.join("x3"), &pattern(100_000, 10));
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
    fs::File::options()
        .write(true)
        .open(r.join("x3"))
        .unwrap()
        .set_modified(later)
        .unwrap();
    // The library knows x1's BLAKE3.
    let x1 = &t.node(id_of(&t, &r.join("x1")));
    let cache = HashCache::from([(
        r.join("x1"),
        duplicates::CachedBlake3 {
            bytes: x1.size,
            modified_ns: x1.modified,
            digest: *blake3::hash(&bytes).as_bytes(),
        },
    )]);
    let rep = duplicates::find(&t, &dup_options(), &cache, &ScanHandle::new());
    assert_eq!(rep.groups.len(), 1);
    assert_eq!(group_names(&rep.groups[0]), ["x1", "x2"]);
    assert_eq!(rep.reused_hashes, 1);
    assert_eq!(rep.skipped.len(), 1);
    assert_eq!(rep.skipped[0].path, r.join("x3"));
    assert!(
        rep.skipped[0].reason.contains("rescan"),
        "{:?}",
        rep.skipped
    );

    // Keep x2: x1 goes; both on one volume, so hard links are possible.
    let others = duplicates::verify(&rep.groups[0], 1, true, &ScanHandle::new()).unwrap();
    assert_eq!(others, [r.join("x1")]);
    assert!(duplicates::verify(&rep.groups[0], 5, false, &ScanHandle::new()).is_err());
}

#[test]
fn duplicates_never_pair_one_file_reached_through_a_followed_link() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    put(&r.join("real/data.bin"), &pattern(5_000, 1));
    #[cfg(windows)]
    let follow = {
        crate::fsattr::make_link(
            &r.join("real"),
            &r.join("via"),
            crate::fsattr::LinkKind::Junction,
        )
        .unwrap();
        ScanOptions {
            follow_junctions: true,
            ..ScanOptions::default()
        }
    };
    #[cfg(not(windows))]
    let follow = {
        std::os::unix::fs::symlink(r.join("real"), r.join("via")).unwrap();
        ScanOptions {
            follow_symlinks: true,
            ..ScanOptions::default()
        }
    };
    let t = scan_at(r, &follow);
    let reached = (t.nodes())
        .filter(|(_, n)| n.kind == NodeKind::File && &*n.name == "data.bin")
        .count();
    assert_eq!(reached, 2, "the fixture reaches the file twice");
    let rep = duplicates::find(&t, &dup_options(), &HashCache::new(), &ScanHandle::new());
    assert!(rep.groups.is_empty(), "{:?}", rep.groups);
}

#[test]
fn duplicates_cancel_and_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    put(&r.join("=SUM(1).txt"), b"twin");
    put(&r.join("plain.txt"), b"twin");
    let t = scan_at(r, &ScanOptions::default());

    let h = ScanHandle::new();
    h.cancel();
    let rep = duplicates::find(&t, &dup_options(), &HashCache::new(), &h);
    assert!(rep.cancelled && rep.groups.is_empty() && rep.skipped.is_empty());

    let rep = duplicates::find(&t, &dup_options(), &HashCache::new(), &ScanHandle::new());
    let mut csv = Vec::new();
    duplicates::write_csv(&rep, &mut csv).unwrap();
    let csv = String::from_utf8(csv).unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines[0],
        "group,status,algorithm,digest,path,extension,bytes,allocated"
    );
    assert_eq!(lines.len(), 3);
    assert!(lines[1].starts_with("1,Duplicates,BLAKE3,"));
    assert!(lines[1].contains(",\"txt\",4,"));
    // The file name is a path cell, so not a formula; the extension column is quoted.
    assert!(csv.contains("=SUM(1).txt"));

    let mut json = Vec::new();
    duplicates::write_json(&rep, &mut json).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(v["groups"][0]["files"].as_array().unwrap().len(), 2);
    assert_eq!(v["groups"][0]["algorithm"], "Blake3");
    assert_eq!(v["groups"][0]["probable"], false);
}

// ---- permissions ----

#[test]
fn unix_modes_flag_world_writable() {
    let p = permissions::from_mode(42, 7, 0o41777);
    assert_eq!(p.mode, Some(0o1777));
    assert!(p.world_writable);
    assert_eq!((p.owner_id.as_str(), p.group.as_deref()), ("42", Some("7")));
    assert!(!permissions::from_mode(42, 7, 0o40755).world_writable);
}

#[test]
fn windows_masks_and_inheritance_read_like_explorer() {
    assert_eq!(permissions::access_text(0x1F_01FF), "Full control");
    assert_eq!(permissions::access_text(0x1000_0000), "Full control");
    assert_eq!(permissions::access_text(0x13_01BF), "Modify");
    assert_eq!(permissions::access_text(0x12_00A9), "Read & execute");
    assert_eq!(permissions::access_text(0x12_0089), "Read");
    assert_eq!(permissions::access_text(0x10_0116), "Write");
    assert_eq!(permissions::access_text(0x4), "Special (0x4)");
    assert_eq!(
        permissions::applies_text(0x3),
        "This folder + Subfolders + Files"
    );
    assert_eq!(permissions::applies_text(0x8 | 0x1), "Files");
    assert_eq!(permissions::applies_text(0), "This folder");
    // Everyone with write: flagged; with read only, or only on what it is inherited by,
    // or a deny: not.
    assert!(permissions::broad_write(true, "S-1-1-0", 0x13_01BF, 0x3));
    assert!(permissions::broad_write(true, "S-1-5-11", 0x2, 0));
    assert!(!permissions::broad_write(true, "S-1-1-0", 0x12_00A9, 0x3));
    assert!(!permissions::broad_write(
        true,
        "S-1-1-0",
        0x1F_01FF,
        0x8 | 0x3
    ));
    assert!(!permissions::broad_write(false, "S-1-1-0", 0x1F_01FF, 0));
    assert!(!permissions::broad_write(
        true,
        "S-1-5-21-1-2-3-1001",
        0x1F_01FF,
        0
    ));
}

#[test]
fn permissions_scan_of_a_temp_tree_and_csv() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    put(&r.join("sub/file.txt"), b"x");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(r.join("sub"), fs::Permissions::from_mode(0o777)).unwrap();
    }
    let t = scan_at(r, &ScanOptions::default());
    let folders = permissions::scan(&t, false, &ScanHandle::new());
    assert!(!folders.cancelled);
    assert_eq!(folders.rows.len(), 2, "the root and sub");
    for row in &folders.rows {
        let p = row
            .permissions
            .as_ref()
            .unwrap_or_else(|| panic!("{:?}", row.error));
        assert!(!p.owner.is_empty() && !p.owner_id.is_empty());
        #[cfg(windows)]
        {
            assert!(p.owner_id.starts_with("S-1-"), "{}", p.owner_id);
            assert!(!p.entries.is_empty());
            // A temp folder inherits its entries.
            assert!(p.entries.iter().any(|e| e.inherited));
        }
    }
    #[cfg(unix)]
    {
        let sub = id_of(&t, &r.join("sub"));
        let row = folders.rows.iter().find(|x| x.node == sub).unwrap();
        assert!(row.permissions.as_ref().unwrap().world_writable);
        assert_eq!(folders.counts().world_writable, 1);
    }

    let with_files = permissions::scan(&t, true, &ScanHandle::new());
    assert_eq!(with_files.rows.len(), 3);
    let owners = with_files.owners();
    assert!(owners.contains_key(&id_of(&t, &r.join("sub/file.txt"))));

    let mut csv = Vec::new();
    permissions::write_csv(&folders, &mut csv).unwrap();
    let csv = String::from_utf8(csv).unwrap();
    assert!(csv.starts_with("path,owner,owner_id,group,mode,inheritance,world_writable,"));
    assert!(csv.lines().count() >= 3);

    let h = ScanHandle::new();
    h.cancel();
    let none = permissions::scan(&t, true, &h);
    assert!(none.cancelled && none.rows.is_empty());
}

#[test]
fn counted_files_leave_out_further_hard_link_names() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    put(&r.join("a"), b"1");
    fs::hard_link(r.join("a"), r.join("b")).unwrap();
    let t = scan_at(r, &ScanOptions::default());
    let counted = (t.nodes()).filter(|(id, _)| counted_file(&t, *id)).count();
    assert_eq!(counted, 1);
}
