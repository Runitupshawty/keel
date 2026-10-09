use super::*;
use crate::tab::Listing;
use keel_vfs::{Kind, Router, VPath};

/// The media view draws a tile per entry: placeholders for photos without a texture yet,
/// icons for other files, and asks the workers for the visible photos.
#[test]
fn media_view_draws_tiles_and_requests_visible_sidecars() {
    let dir = VPath::parse("mem://t/").unwrap();
    let ctx = egui::Context::default();
    let (tx, _rx) = crossbeam_channel::unbounded();
    let router = Arc::new(Router::new());
    let library = crate::library::LibraryUi::new(&router, tx.clone(), ctx.clone());
    let mut thumbs = crate::view_grid::Thumbs::new(tx, ctx.clone(), router.clone());
    let mut media = Media::with_threads_for_tests(ctx.clone(), router);
    let theme = crate::theme::Theme::load("dark");
    let mut tab = Tab::new(dir.clone());
    tab.set_entries(
        ["a.jpg", "b.txt", "c.mp4"]
            .map(|n| crate::tab::test_entry(&dir, n, Kind::File, 1))
            .into(),
    );
    let mut shapes = 0;
    for _ in 0..2 {
        let out = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |p| {
                let mut cx = ViewCx {
                    theme: &theme,
                    show_hidden: false,
                    thumbs: &mut thumbs,
                    active: false,
                    banner: None,
                    preview: None,
                    column_widths: &mut Vec::new(),
                    library: &library,
                    drives: &[],
                    searcher: None,
                    tags_column: false,
                    media: &mut media,
                };
                super::ui(p, (0, 0), &mut tab, &mut cx, &mut Vec::new());
            });
        });
        shapes = out.shapes.len();
    }
    assert!(shapes > 3);
    // a.jpg (thumbnail) and c.mp4 (strip) were asked for, b.txt was not.
    assert_eq!(media.queued_for_tests(), 2);
    assert!(tab.row_step >= 1 && tab.page_rows >= tab.row_step);
}

/// Review focus 1, `#[ignore]`, release only:
/// `cargo test --release -p keel-app media_grid_perf -- --ignored --nocapture`.
/// 129,000 synthetic sidecars (made once, cached in the temp folder) behind a fake
/// listing; scrolling 10 screens in a kittest harness (wgpu) must keep the mean frame
/// under 16 ms and every frame under 50 ms (frame = egui pass + texture uploads +
/// tessellation, with the frames paced at 60 Hz so the workers run as they would).
#[test]
#[ignore]
fn media_grid_perf() {
    use egui_kittest::Harness;
    use keel_core::{SidecarKey, Sidecars};
    use std::time::Instant;
    const N: usize = 129_000;
    if cfg!(debug_assertions) {
        eprintln!("media_grid_perf: run with --release");
        return;
    }
    let root = std::env::temp_dir().join("keel-media-perf-129k");
    let fake = root.join("photos");
    let store_dir = root.join("store");
    let mtime = 1_700_000_000_i64;
    let entry = |i: usize| keel_vfs::Entry {
        path: VPath::local(fake.join(format!("img{i:06}.jpg"))),
        name: format!("img{i:06}.jpg"),
        kind: Kind::File,
        size: 1000,
        modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(mtime as u64)),
        hidden: false,
        is_link: false,
        encrypted: false,
        ext: "jpg".into(),
    };
    if !root.join("ready").exists() {
        let t = Instant::now();
        std::fs::create_dir_all(&store_dir).unwrap();
        // 64 tiny lossless WebP variants, written under each key's sidecar folder.
        let webps: Vec<Vec<u8>> = (0..64u8)
            .map(|c| {
                let img = image::RgbImage::from_fn(32, 24, |x, y| {
                    image::Rgb([c * 4, (x * 8) as u8, (y * 10) as u8])
                });
                let mut out = std::io::Cursor::new(Vec::new());
                img.write_to(&mut out, image::ImageFormat::WebP).unwrap();
                out.into_inner()
            })
            .collect();
        std::thread::scope(|s| {
            for part in 0..8 {
                let (webps, store_dir, fake) = (&webps, &store_dir, &fake);
                s.spawn(move || {
                    for i in (part..N).step_by(8) {
                        let p = fake.join(format!("img{i:06}.jpg"));
                        let key = SidecarKey::local(&p, mtime, 1000, None);
                        let dir = store_dir.join(key.dir_name());
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("thumb-256.webp"), &webps[i % 64]).unwrap();
                    }
                });
            }
        });
        std::fs::write(root.join("ready"), b"").unwrap();
        println!("made {N} sidecars in {:.1} s", t.elapsed().as_secs_f32());
    }
    let store = Arc::new(Sidecars::open(&store_dir, 64 << 30).unwrap());

    struct Perf {
        tab: Tab,
        media: Media,
        thumbs: crate::view_grid::Thumbs,
        library: crate::library::LibraryUi,
        theme: crate::theme::Theme,
    }
    let ctx = egui::Context::default();
    let (tx, _rx) = crossbeam_channel::unbounded();
    let router = Arc::new(Router::new());
    let media = Media::new(ctx.clone(), router.clone());
    media.set_store(store);
    let mut tab = Tab::new(VPath::local(&fake));
    tab.set_listing(Listing::new((0..N).map(entry).collect()));
    let mut media_tile = media;
    media_tile.tile = TileSize::S;
    let state = Perf {
        tab,
        media: media_tile,
        thumbs: crate::view_grid::Thumbs::new(tx.clone(), ctx.clone(), router.clone()),
        library: crate::library::LibraryUi::new(&router, tx, ctx.clone()),
        theme: crate::theme::Theme::load("dark"),
    };
    let mut h = Harness::builder()
        .with_size(egui::vec2(1600.0, 1000.0))
        .wgpu()
        .build_ui_state(
            |ui, s: &mut Perf| {
                s.media.upload(ui.ctx());
                let mut cx = ViewCx {
                    theme: &s.theme,
                    show_hidden: false,
                    thumbs: &mut s.thumbs,
                    active: false,
                    banner: None,
                    preview: None,
                    column_widths: &mut Vec::new(),
                    library: &s.library,
                    drives: &[],
                    searcher: None,
                    tags_column: false,
                    media: &mut s.media,
                };
                super::ui(ui, (0, 0), &mut s.tab, &mut cx, &mut Vec::new());
            },
            state,
        );
    let frame = |h: &mut Harness<Perf>| {
        let t = Instant::now();
        h.step();
        let ppp = h.ctx.pixels_per_point();
        let shapes = h.output().shapes.clone();
        let _ = h.ctx.tessellate(shapes, ppp);
        t.elapsed()
    };
    for _ in 0..5 {
        frame(&mut h);
    }
    let (cols, page) = (h.state().tab.row_step, h.state().tab.page_rows);
    let screens = 10;
    let steps_per_screen = 6;
    let rows_per_step = (page / cols).div_ceil(steps_per_screen).max(1);
    let mut pos = page.saturating_sub(1);
    let mut times = Vec::new();
    let mut ready_seen = 0usize;
    for _ in 0..screens * steps_per_screen {
        pos = (pos + rows_per_step * cols).min(N - 1);
        h.state_mut().tab.scroll_to = Some(pos);
        let dt = frame(&mut h);
        times.push(dt);
        ready_seen += h.state().media.len();
        std::thread::sleep(Duration::from_millis(16).saturating_sub(dt));
    }
    // Settle: the last screen's thumbnails all arrive.
    let settle = Instant::now();
    for _ in 0..120 {
        let dt = frame(&mut h);
        times.push(dt);
        std::thread::sleep(Duration::from_millis(16).saturating_sub(dt));
    }
    let ms: Vec<f64> = times.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    let max = ms.iter().cloned().fold(0.0, f64::max);
    let mut sorted = ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p95 = sorted[sorted.len() * 95 / 100];
    println!(
        "media grid perf: {N} items, {cols} cols, {} frames: mean {mean:.2} ms, p95 {p95:.2} ms, max {max:.2} ms; textures {} (avg {} while scrolling), settle {:.1} s",
        ms.len(),
        h.state().media.len(),
        ready_seen / (screens * steps_per_screen),
        settle.elapsed().as_secs_f32()
    );
    assert!(mean < 16.0, "mean frame {mean:.2} ms");
    assert!(max < 50.0, "max frame {max:.2} ms");
}
