use super::*;
use crate::media::tests::{exif_jpeg, png};

fn key_of(path: &Path) -> SidecarKey {
    let m = fs::metadata(path).unwrap();
    SidecarKey::local(path, 1, m.len(), None)
}

/// Three 200x100 PNGs with their keys.
fn three(dir: &Path) -> Vec<(PathBuf, SidecarKey)> {
    (0..3)
        .map(|i| {
            let p = dir.join(format!("{i}.png"));
            png(&p, 200 + i, 100);
            let k = key_of(&p);
            (p, k)
        })
        .collect()
}

#[test]
fn ensure_makes_get_finds_stats_count() {
    let files = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let s = Sidecars::open(store.path(), DEFAULT_BUDGET).unwrap();
    let photo = files.path().join("p.jpg");
    exif_jpeg(&photo);
    let key = key_of(&photo);
    assert_eq!(s.get(&key, SidecarKind::Thumb256), None);
    let thumb = s.ensure(&key, SidecarKind::Thumb256, &photo).unwrap();
    assert_eq!(
        thumb,
        store.path().join(key.dir_name()).join("thumb-256.webp")
    );
    assert_eq!(s.get(&key, SidecarKind::Thumb256), Some(thumb.clone()));
    let meta = s.ensure(&key, SidecarKind::Meta, &photo).unwrap();
    assert!(meta.ends_with("meta.json"));
    assert_eq!(s.meta(&key).unwrap().orientation, 6);
    assert!(
        s.ensure(&key, SidecarKind::Strip, &photo).is_err(),
        "no strip for photos"
    );
    let stats = s.stats();
    assert_eq!(stats.keys, 1);
    assert_eq!(
        stats.bytes,
        fs::metadata(&thumb).unwrap().len() + fs::metadata(&meta).unwrap().len()
    );
    assert_eq!(stats.budget, DEFAULT_BUDGET);
    // No temp files left behind.
    assert_eq!(fs::read_dir(thumb.parent().unwrap()).unwrap().count(), 2);

    // Content-addressed: another path with the same content id shares the folder.
    let cas = SidecarKey {
        cas_id: Some([7; 32]),
        ..key.clone()
    };
    let other = SidecarKey {
        path_hash: [1; 32],
        ..cas.clone()
    };
    assert_eq!(cas.dir_name(), "07".repeat(32));
    s.ensure(&cas, SidecarKind::Thumb256, &photo).unwrap();
    assert!(s.get(&other, SidecarKind::Thumb256).is_some());
    assert_ne!(key.dir_name(), cas.dir_name());
    assert!(key.dir_name().starts_with('p'));
}

#[test]
fn eviction_is_lru_and_never_deletes_pinned_keys() {
    let files = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let s = Sidecars::open(store.path(), DEFAULT_BUDGET).unwrap();
    let all = three(files.path());
    for (p, k) in &all {
        s.ensure(k, SidecarKind::Thumb256, p).unwrap();
    }
    let size = |k: &SidecarKey| folder_bytes(&store.path().join(k.dir_name()));
    let (a, b, c) = (&all[0].1, &all[1].1, &all[2].1);
    // Use order now: b, c, a (a was touched last).
    assert!(s.get(a, SidecarKind::Thumb256).is_some());

    // Room for two: the least recently used (b) goes.
    s.set_budget(size(c) + size(a));
    s.evict_to_budget(&|_| false);
    assert!(s.get(b, SidecarKind::Thumb256).is_none());
    assert!(s.get(c, SidecarKind::Thumb256).is_some());
    assert!(s.get(a, SidecarKind::Thumb256).is_some());
    assert_eq!(s.stats().keys, 2);
    // Order now: c, a. Room for one, but c is pinned by a guard: a goes instead.
    let guard = s.pin(c);
    s.set_budget(size(a).max(size(c)));
    s.evict_to_budget(&|_| false);
    assert!(store.path().join(c.dir_name()).is_dir());
    assert!(!store.path().join(a.dir_name()).exists());
    // Nothing fits, c still pinned (now by the callback too): over budget, kept.
    s.set_budget(0);
    s.evict_to_budget(&|k| k == c);
    drop(guard);
    s.evict_to_budget(&|k| k == c);
    assert!(store.path().join(c.dir_name()).is_dir());
    assert_eq!(s.stats().keys, 1);
    // Released: gone.
    s.evict_to_budget(&|_| false);
    assert_eq!(
        s.stats(),
        SidecarStats {
            keys: 0,
            bytes: 0,
            budget: 0
        }
    );
    assert!(!store.path().join(c.dir_name()).exists());
}

#[test]
fn lru_order_survives_reopen() {
    let files = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let all = three(files.path());
    {
        let s = Sidecars::open(store.path(), DEFAULT_BUDGET).unwrap();
        for (p, k) in &all {
            s.ensure(k, SidecarKind::Thumb256, p).unwrap();
        }
        s.get(&all[0].1, SidecarKind::Thumb256).unwrap();
    }
    let s = Sidecars::open(store.path(), 1).unwrap();
    // Evicts b then c; a (newest use) would go last but 1 byte fits nothing.
    s.evict_to_budget(&|k| k == &all[0].1);
    assert_eq!(s.stats().keys, 1);
    assert!(s.get(&all[0].1, SidecarKind::Thumb256).is_some());
}

#[test]
fn corrupt_files_record_their_error_once() {
    let files = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let s = Sidecars::open(store.path(), DEFAULT_BUDGET).unwrap();
    let bad = files.path().join("bad.png");
    fs::write(&bad, b"\x89PNG\r\n\x1a\n broken").unwrap();
    let key = key_of(&bad);
    let err = s.ensure(&key, SidecarKind::Thumb256, &bad).unwrap_err();
    let recorded = s.meta(&key).unwrap().error.unwrap();
    assert_eq!(recorded, err.to_string());
    // Meta exists (with the error); thumbnails fail at once without decoding again.
    s.ensure(&key, SidecarKind::Meta, &bad).unwrap();
    fs::remove_file(&bad).unwrap();
    let again = s.ensure(&key, SidecarKind::Thumb1024, &bad).unwrap_err();
    assert_eq!(again.to_string(), recorded);
    assert!(s.get(&key, SidecarKind::Thumb256).is_none());
}

#[test]
fn relink_moves_sidecars_to_the_content_key() {
    let files = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let s = Sidecars::open(store.path(), DEFAULT_BUDGET).unwrap();
    let p = files.path().join("a.png");
    png(&p, 10, 10);
    let by_path = key_of(&p);
    s.ensure(&by_path, SidecarKind::Thumb256, &p).unwrap();
    let by_cas = SidecarKey {
        cas_id: Some([9; 32]),
        ..by_path.clone()
    };
    {
        let _pin = s.pin(&by_path);
        assert!(!s.relink(&by_path, &by_cas).unwrap(), "pinned: not moved");
    }
    assert!(s.relink(&by_path, &by_cas).unwrap());
    assert!(s.get(&by_path, SidecarKind::Thumb256).is_none());
    assert!(s.get(&by_cas, SidecarKind::Thumb256).is_some());
    assert_eq!(s.stats().keys, 1);
    assert!(
        !s.relink(&by_path, &by_cas).unwrap(),
        "nothing left to move"
    );
}
