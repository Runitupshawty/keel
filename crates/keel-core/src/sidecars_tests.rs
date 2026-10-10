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
    let meta = s.meta(&key).unwrap();
    assert_eq!(
        meta.failure(SidecarKind::Thumb256),
        Some(err.to_string().as_str())
    );
    // Its metadata could not be read either: recorded as the metadata's own error.
    assert!(meta.error.is_some());
    // Meta exists (with the error); the thumbnail fails at once without decoding again.
    s.ensure(&key, SidecarKind::Meta, &bad).unwrap();
    fs::remove_file(&bad).unwrap();
    let again = s.ensure(&key, SidecarKind::Thumb256, &bad).unwrap_err();
    assert_eq!(again.to_string(), err.to_string());
    assert!(s.get(&key, SidecarKind::Thumb256).is_none());
}

/// Review M4: a failure is recorded per kind; the other kinds are still made.
#[test]
fn one_failed_kind_leaves_the_others() {
    let files = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let s = Sidecars::open(store.path(), DEFAULT_BUDGET).unwrap();
    let p = files.path().join("a.png");
    png(&p, 300, 200);
    let key = key_of(&p);
    // As if the 1024 px thumbnail had failed (a strip that timed out the same way).
    let meta_path = s.ensure(&key, SidecarKind::Meta, &p).unwrap();
    let mut meta = s.meta(&key).unwrap();
    meta.failed
        .insert("thumb-1024.webp".into(), "could not decode".into());
    fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    assert!(s.ensure(&key, SidecarKind::Thumb256, &p).is_ok());
    let err = s.ensure(&key, SidecarKind::Thumb1024, &p).unwrap_err();
    assert_eq!(err.to_string(), "could not decode");
    assert_eq!(s.meta(&key).unwrap().width, 300);
    // A tool that ran out of time is not the file's fault: never recorded.
    assert!(!media::is_corrupt(&anyhow::Error::from(media::TimedOut)));
}

/// Review M4: a thumbnail that fails before the metadata exists leaves the real metadata
/// (made first), not an empty stand-in that would never be read again.
#[test]
fn a_failed_thumbnail_keeps_the_real_metadata() {
    let files = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let s = Sidecars::open(store.path(), DEFAULT_BUDGET).unwrap();
    // A BMP header for 20000 x 20000 (400 MP, over the decode limit) and no pixels.
    let big = files.path().join("big.bmp");
    let mut b = b"BM".to_vec();
    for v in [54u32, 0, 54, 40, 20_000, 20_000] {
        b.extend(v.to_le_bytes());
    }
    b.extend(1u16.to_le_bytes());
    b.extend(24u16.to_le_bytes());
    b.extend([0u8; 24]);
    fs::write(&big, b).unwrap();
    let key = key_of(&big);
    let err = s.ensure(&key, SidecarKind::Thumb256, &big).unwrap_err();
    assert!(err.to_string().contains("MP"), "{err:#}");
    let meta = s.meta(&key).unwrap();
    assert_eq!((meta.width, meta.height), (20_000, 20_000));
    assert_eq!(meta.error, None);
    assert!(meta.failure(SidecarKind::Thumb256).is_some());
    assert!(meta.failure(SidecarKind::Thumb1024).is_none());
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

#[test]
fn strip_timeouts_wait_a_week_or_a_change() {
    let week = STRIP_RETRY.as_secs() as i64;
    let t = TimedOutAt {
        at: 1_000_000,
        mtime: 5,
        size: 10,
    };
    assert!(!strip_retry_due(&t, 1_000_000, 5, 10), "just now");
    assert!(!strip_retry_due(&t, 1_000_000 + week - 1, 5, 10));
    assert!(strip_retry_due(&t, 1_000_000 + week, 5, 10), "a week later");
    assert!(strip_retry_due(&t, 1_000_000, 6, 10), "modified");
    assert!(strip_retry_due(&t, 1_000_000, 5, 11), "resized");
    assert!(strip_retry_due(&t, 999_999, 5, 10), "the clock went back");
}

/// The timeout is kept in `meta.json`: `ensure` refuses the strip at once (no ffmpeg run)
/// until the file changes or `retry` forgets it; other kinds are not affected.
#[test]
fn a_strip_timeout_is_remembered_until_retried() {
    let files = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let s = Sidecars::open(store.path(), DEFAULT_BUDGET).unwrap();
    let clip = files.path().join("clip.mp4");
    fs::write(&clip, b"not really a video").unwrap();
    let key = SidecarKey {
        cas_id: Some([3; 32]),
        ..key_of(&clip)
    };
    let meta = MediaMeta {
        duration_ms: Some(90 * 60 * 1000),
        ..MediaMeta::default()
    };
    s.write(&key, SidecarKind::Meta, &serde_json::to_vec(&meta).unwrap())
        .unwrap();
    s.record_timeout(&key, crate::now()).unwrap();
    // Reopened: the record is on disk.
    let s = Sidecars::open(store.path(), DEFAULT_BUDGET).unwrap();
    let kept = s.meta(&key).unwrap();
    assert_eq!(
        kept.duration_ms, meta.duration_ms,
        "on the file's real metadata"
    );
    assert_eq!(kept.timed_out["strip.webp"].size, key.size);
    let err = s.ensure(&key, SidecarKind::Strip, &clip).unwrap_err();
    assert!(err.is::<StripWaits>(), "{err:#}");
    // The same content at another mtime (a copy, an edit restoring the bytes): tried.
    let touched = SidecarKey {
        mtime: key.mtime + 1,
        ..key.clone()
    };
    let tried = s.ensure(&touched, SidecarKind::Strip, &clip);
    assert!(!tried.is_err_and(|e| e.is::<StripWaits>()));
    // Retry strip forgets it (and a recorded failure).
    s.record_timeout(&key, crate::now()).unwrap();
    s.retry(&key, SidecarKind::Strip).unwrap();
    let meta = s.meta(&key).unwrap();
    assert!(meta.timed_out.is_empty() && meta.failure(SidecarKind::Strip).is_none());
    let tried = s.ensure(&key, SidecarKind::Strip, &clip);
    assert!(!tried.is_err_and(|e| e.is::<StripWaits>()));
    // A key without a meta.json: nothing to forget.
    let unknown = SidecarKey {
        cas_id: None,
        size: 1,
        ..key
    };
    s.retry(&unknown, SidecarKind::Strip).unwrap();
    assert!(s.meta(&unknown).is_none());
}
