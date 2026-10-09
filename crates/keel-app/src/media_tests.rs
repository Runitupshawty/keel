use super::*;
use keel_core::{JobStatus, SourceDef, SourceKind};

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("keel-media-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A Media without worker threads (queue and uploads only).
fn idle_media() -> (egui::Context, Media) {
    let ctx = egui::Context::default();
    let m = Media::with_threads(ctx.clone(), Arc::new(Router::new()), 0, 0);
    (ctx, m)
}

fn key(i: usize) -> TexKey {
    TexKey {
        path: VPath::parse(&format!("mem://t/{i}.jpg")).unwrap(),
        mtime: 0,
        size: 1,
        kind: SidecarKind::Thumb256,
        px: 96,
    }
}

fn loaded(i: usize) -> Loaded {
    Loaded {
        key: key(i),
        image: Some(ColorImage::new([2, 2], egui::Color32::RED)),
    }
}

/// Review focus 1: a burst of decoded thumbnails is uploaded at most 8 per frame.
#[test]
fn uploads_are_budgeted_per_frame() {
    let (ctx, mut m) = idle_media();
    for i in 0..20 {
        m.sh.tx.send(loaded(i)).unwrap();
    }
    assert_eq!(m.upload(&ctx), MAX_UPLOADS);
    assert_eq!(m.len(), 8);
    assert_eq!(m.upload(&ctx), MAX_UPLOADS);
    assert_eq!(m.upload(&ctx), 4);
    assert_eq!(m.upload(&ctx), 0);
    assert!(matches!(m.get(&key(3)), Tex::Ready(..)));
    // A failed sidecar is remembered as failed (icon), not retried.
    m.sh.tx
        .send(Loaded {
            key: key(99),
            image: None,
        })
        .unwrap();
    m.upload(&ctx);
    assert_eq!(m.get(&key(99)), Tex::Failed);
    assert_eq!(m.get(&key(100)), Tex::Missing);
}

#[test]
fn texture_cache_is_a_bounded_lru() {
    let (ctx, mut m) = idle_media();
    for i in 0..TEXTURE_CACHE {
        m.sh.tx.send(loaded(i)).unwrap();
    }
    while m.upload(&ctx) > 0 {}
    assert_eq!(m.len(), TEXTURE_CACHE);
    // Tile 0 is drawn again: it is now among the newest.
    assert!(matches!(m.get(&key(0)), Tex::Ready(..)));
    for i in TEXTURE_CACHE..TEXTURE_CACHE + 10 {
        m.sh.tx.send(loaded(i)).unwrap();
    }
    while m.upload(&ctx) > 0 {}
    assert!(m.len() <= TEXTURE_CACHE, "{}", m.len());
    assert!(m.has(&key(0)), "recently drawn kept");
    assert!(!m.has(&key(1)), "least recently drawn evicted");
    assert!(m.has(&key(TEXTURE_CACHE + 9)), "newest kept");
}

/// Requests for tiles that scrolled away are dropped before they run; visible ones go
/// first; a key another view still wants survives.
#[test]
fn queue_drops_requests_that_scrolled_away() {
    let q: Queue<&str, ()> = Queue::new(3);
    q.want(0, vec![("a", 5, ()), ("b", 1, ()), ("c", 9, ())]);
    assert_eq!(q.queued(), 3);
    // Scrolled: a and c left the window, d came in.
    q.want(0, vec![("b", 1, ()), ("d", 0, ())]);
    assert_eq!(q.queued(), 2);
    assert_eq!(q.try_pop(Stage::Load).unwrap().0, "d", "most urgent first");
    assert_eq!(q.try_pop(Stage::Load).unwrap().0, "b");
    assert!(q.try_pop(Stage::Load).is_none());
    // b's sidecar is missing: it moves to the make stage while still wanted...
    assert!(q.promote(&"b"));
    assert!(q.try_pop(Stage::Load).is_none());
    // ...but scrolls away before a maker takes it.
    q.want(0, vec![("d", 0, ())]);
    assert!(q.try_pop(Stage::Make).is_none(), "dropped unmade");
    // d is busy when it scrolls away: the worker finishes, nothing is left behind.
    q.want(0, vec![]);
    q.done(&"d");
    assert_eq!(q.queued(), 0);
    // Answered keys still listed are not queued again; evicted ones are.
    q.want(0, vec![("e", 0, ())]);
    q.try_pop(Stage::Load).unwrap();
    q.done(&"e");
    q.want(0, vec![("e", 0, ())]);
    assert_eq!(q.queued(), 0);
    q.forget(&["e"]);
    q.want(0, vec![("e", 0, ())]);
    assert_eq!(q.queued(), 1);
    // Two views (pane grid and viewer) want x; one drops it, the other keeps it.
    q.want(0, vec![("x", 3, ())]);
    q.want(VIEWER_SLOT, vec![("x", 0, ())]);
    q.want(0, vec![]);
    assert_eq!(q.try_pop(Stage::Load).unwrap().0, "x");
    // Closing wakes and ends the workers.
    q.close();
    assert!(q.pop(Stage::Load).is_none());
}

#[test]
fn strip_hover_maps_pointer_to_frame() {
    assert_eq!(strip_frame(0.0), 0);
    assert_eq!(strip_frame(-0.3), 0, "left of the tile");
    assert_eq!(strip_frame(0.049), 0);
    assert_eq!(strip_frame(0.05), 1);
    assert_eq!(strip_frame(0.5), 10);
    assert_eq!(strip_frame(0.999), 19);
    assert_eq!(strip_frame(1.0), 19, "right edge");
    assert_eq!(strip_frame(7.0), 19);
    let uv = strip_uv(19);
    assert!((uv.min.x - 0.95).abs() < 1e-6 && (uv.max.x - 1.0).abs() < 1e-6);
    assert_eq!((uv.min.y, uv.max.y), (0.0, 1.0));
    // A 160x90 frame of a 3200x90 strip, cropped to its centred 90x90 square.
    let sq = cover_uv(vec2_(3200.0, 90.0), strip_uv(0));
    let w_px = sq.width() * 3200.0;
    assert!((w_px - 90.0).abs() < 0.01, "{w_px}");
    assert!((sq.center().x - strip_uv(0).center().x).abs() < 1e-6);
}

fn vec2_(x: f32, y: f32) -> Vec2 {
    egui::vec2(x, y)
}

#[test]
fn cover_crops_to_the_centred_square() {
    let uv = cover_uv(vec2_(200.0, 100.0), FULL_UV);
    assert_eq!(
        (uv.min.x, uv.max.x, uv.min.y, uv.max.y),
        (0.25, 0.75, 0.0, 1.0)
    );
    let uv = cover_uv(vec2_(100.0, 400.0), FULL_UV);
    assert_eq!((uv.min.y, uv.max.y), (0.375, 0.625));
    assert_eq!(fit_scale(vec2_(4000.0, 3000.0), vec2_(800.0, 800.0)), 0.2);
    assert_eq!(
        fit_scale(vec2_(40.0, 30.0), vec2_(800.0, 800.0)),
        1.0,
        "never enlarged"
    );
}

#[test]
fn date_headers_group_by_day_in_listing_order() {
    let days = [Some(1), Some(1), Some(2), Some(2), Some(2), Some(3), None];
    let f = |i: usize| days[i];
    let rows = layout(days.len(), 2, Some(&f));
    use Row::*;
    assert_eq!(
        rows,
        [
            Header(Some(1)),
            Tiles(0, 2),
            Header(Some(2)),
            Tiles(2, 4),
            Tiles(4, 5),
            Header(Some(3)),
            Tiles(5, 6),
            Header(None),
            Tiles(6, 7),
        ]
    );
    let tops = row_tops(&rows, 100.0, 30.0);
    assert_eq!(tops[1], 30.0);
    assert_eq!(tops[2], 130.0);
    assert_eq!(*tops.last().unwrap(), 4.0 * 30.0 + 5.0 * 100.0);
    // Without dates: plain rows of `cols`.
    assert_eq!(layout(5, 2, None), [Tiles(0, 2), Tiles(2, 4), Tiles(4, 5)]);
    assert!(layout(0, 3, None).is_empty());
    assert_eq!(day(86_399), 0);
    assert_eq!(day(86_400), 1);
    assert_eq!(day(-1), -1, "before 1970");
    assert_eq!(day_label(19_844), "Wednesday, May 1, 2024");
}

#[test]
fn tiles_pick_the_sidecar_for_their_size() {
    let dir = VPath::parse("mem://t/").unwrap();
    let e = crate::tab::test_entry(&dir, "a.jpg", Kind::File, 1);
    assert_eq!(tile_key(&e, false, 96).kind, SidecarKind::Thumb256);
    assert_eq!(tile_key(&e, false, 256).kind, SidecarKind::Thumb256);
    assert_eq!(tile_key(&e, false, 384).kind, SidecarKind::Thumb1024);
    assert_eq!(tile_key(&e, true, 384).kind, SidecarKind::Strip);
    assert_eq!(media_type(&e), Some(MediaType::Image));
    let v = crate::tab::test_entry(&dir, "clip.MP4".to_lowercase().as_str(), Kind::File, 1);
    assert_eq!(media_type(&v), Some(MediaType::Video));
    let d = crate::tab::test_entry(&dir, "x.jpg", Kind::Dir, 1);
    assert_eq!(media_type(&d), None, "a folder named like a photo");
    let mut locked = e.clone();
    locked.encrypted = true;
    assert_eq!(media_type(&locked), None);
    assert_eq!(TileSize::S.step(false), TileSize::S);
    assert_eq!(TileSize::S.step(true), TileSize::M);
    assert_eq!(TileSize::L.step(true), TileSize::L);
}

/// A real worker round trip without the library: a missing thumbnail is made in the
/// cache store, decoded at tile size and uploaded.
#[test]
fn missing_sidecars_are_made_decoded_and_uploaded() {
    let tmp = temp("roundtrip");
    let photo = tmp.join("wide.png");
    image::RgbImage::from_pixel(600, 300, image::Rgb([10, 200, 30]))
        .save(&photo)
        .unwrap();
    let ctx = egui::Context::default();
    let mut m = Media::new(ctx.clone(), Arc::new(Router::new()));
    m.set_store(Arc::new(
        Sidecars::open(&tmp.join("store"), 1 << 30).unwrap(),
    ));
    let dir = VPath::local(&tmp);
    let mut e = crate::tab::test_entry(&dir, "wide.png", Kind::File, 1);
    e.size = std::fs::metadata(&photo).unwrap().len();
    let k = tile_key(&e, false, 96);
    let req = Req {
        entry: e.clone(),
        real: e.path.clone(),
    };
    m.want(0, vec![(k.clone(), 0, req)]);
    let until = Instant::now() + Duration::from_secs(20);
    let size = loop {
        m.upload(&ctx);
        if let Tex::Ready(_, size) = m.get(&k) {
            break size;
        }
        assert!(Instant::now() < until, "no thumbnail");
        std::thread::sleep(Duration::from_millis(20));
    };
    // 600x300 -> Thumb256 (256x128) -> shorter side 96.
    assert_eq!(size, egui::vec2(192.0, 96.0));
}

/// With the library open, tiles find the sidecars the library's sidecar job made (same
/// key: source root joined with the record path, the record's content id), so nothing is
/// decoded twice; nested folders included.
#[test]
fn tiles_reuse_the_sidecar_jobs_thumbnails() {
    let tmp = temp("libkeys");
    let src = tmp.join("photos");
    std::fs::create_dir_all(src.join("2024")).unwrap();
    for (rel, color) in [("top.png", 50), ("2024/nested.png", 150)] {
        image::RgbImage::from_pixel(64, 48, image::Rgb([color, 0, 0]))
            .save(src.join(rel))
            .unwrap();
    }
    let lib = keel_core::Library::open(&tmp.join("data"), "media").unwrap();
    let id = lib
        .add_source(SourceDef {
            label: "Photos".into(),
            root: VPath::local(&src),
            kind: SourceKind::Folder,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        })
        .unwrap();
    let wait = |job| assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    wait(lib.index(&id).unwrap());
    wait(lib.hash().unwrap());
    wait(lib.media_job(&id).unwrap());
    let lib = Arc::new(lib);
    let (_ctx, mut m) = idle_media();
    m.sync_library(Some(&lib));
    let store = m.sh.store().unwrap();
    for (dir, name) in [(src.clone(), "top.png"), (src.join("2024"), "nested.png")] {
        let path = dir.join(name);
        let md = std::fs::metadata(&path).unwrap();
        let entry = Entry {
            path: VPath::local(&path),
            name: name.into(),
            kind: Kind::File,
            size: md.len(),
            modified: md.modified().ok(),
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: "png".into(),
        };
        let req = Req {
            real: entry.path.clone(),
            entry,
        };
        let (key, _) = m.sh.resolve(&req, false).unwrap();
        assert!(
            key.cas_id.is_some(),
            "{name}: hashed record keyed by content"
        );
        assert!(
            store.get(&key, SidecarKind::Thumb256).is_some(),
            "{name}: the job's thumbnail is found"
        );
        assert_eq!(m.sh.meta(&req, false).unwrap().width, 64);
    }
    drop(m);
    if let Ok(lib) = Arc::try_unwrap(lib) {
        lib.close(Duration::from_secs(5));
    }
}
