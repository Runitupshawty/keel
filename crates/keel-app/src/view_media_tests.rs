use super::*;
use crate::state::AppState;
use crate::tab::Listing;
use keel_vfs::{Kind, Router, VPath};
use std::path::PathBuf;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("keel-mview-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn listed(s: &mut AppState) {
    for _ in 0..500 {
        s.drain();
        if !s.tab(0).loading {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("listing never arrived");
}

fn key(s: &mut AppState, key: Key) {
    let ctx = s.ctx.clone();
    let mut input = egui::RawInput::default();
    input.events.push(egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    let _ = ctx.run(input, |ctx| s.viewer_ui(ctx));
}

/// Space and Enter on a photo open the viewer; Left/Right walk the listing's media files
/// only; Esc closes with the grid's cursor on the file shown last. Space on other files
/// still toggles the selection.
#[test]
fn viewer_opens_navigates_and_closes() {
    let tmp = temp("viewer");
    for n in ["a.jpg", "b.png", "c.txt", "d.png"] {
        std::fs::write(tmp.join(n), b"x").unwrap();
    }
    let ctx = egui::Context::default();
    let mut s = AppState::new(ctx, Arc::new(Router::new()), VPath::local(&tmp));
    s.dual = false;
    s.panes[0].view = crate::pane::ViewMode::Media;
    listed(&mut s);
    s.tab_mut(0).visible(false);
    s.tab_mut(0).cursor = Some("c.txt".into());
    s.run(0, Action::ToggleSelect);
    assert!(s.viewer.is_none());
    assert!(s.tab(0).selected.contains("c.txt"));

    s.tab_mut(0).cursor = Some("b.png".into());
    s.tab_mut(0).selected.clear();
    s.run(0, Action::ToggleSelect);
    assert_eq!(
        s.viewer.as_ref().map(|v| v.index),
        Some(1),
        "a.jpg, [b.png], d.png"
    );
    key(&mut s, Key::ArrowRight);
    assert_eq!(s.viewer.as_ref().unwrap().index, 2, "c.txt skipped");
    key(&mut s, Key::ArrowRight);
    assert_eq!(s.viewer.as_ref().unwrap().index, 2, "no wrap");
    key(&mut s, Key::I);
    assert!(s.viewer.as_ref().unwrap().info);
    key(&mut s, Key::ArrowLeft);
    key(&mut s, Key::ArrowLeft);
    key(&mut s, Key::ArrowLeft);
    assert_eq!(s.viewer.as_ref().unwrap().index, 0);
    key(&mut s, Key::Escape);
    assert!(s.viewer.is_none());
    assert_eq!(s.tab(0).cursor.as_deref(), Some("a.jpg"));

    s.run(0, Action::Enter);
    assert_eq!(
        s.viewer.as_ref().map(|v| v.index),
        Some(0),
        "Enter opens it too"
    );
    // The details view keeps Space = toggle selection.
    s.viewer = None;
    s.panes[0].view = crate::pane::ViewMode::Details;
    s.run(0, Action::ToggleSelect);
    assert!(s.viewer.is_none());
}

#[test]
fn viewer_math() {
    use crate::media_viewer::{step, zoom_about};
    assert_eq!(step(0, 5, -1), 0);
    assert_eq!(step(2, 5, 1), 3);
    assert_eq!(step(4, 5, 1), 4);
    assert_eq!(step(3, 5, isize::MIN / 2), 0);
    assert_eq!(step(0, 5, isize::MAX / 2), 4);
    assert_eq!(step(0, 0, 1), 0, "empty list");
    // Zooming about a point keeps the image point under it still.
    let (center, pan, at) = (pos2(500.0, 400.0), vec2(30.0, -20.0), pos2(650.0, 300.0));
    let z = 0.5;
    let u = (at - (center + pan)) / z;
    let (nz, npan) = zoom_about(z, 2.0, pan, center, at);
    assert_eq!(nz, 1.0);
    let back = center + npan + u * nz;
    assert!((back - at).length() < 1e-3, "{back:?}");
    assert_eq!(zoom_about(30.0, 4.0, pan, center, at).0, 32.0, "clamped");
}

/// The full-resolution decode applies EXIF orientation (6: rotate 90 degrees clockwise).
#[test]
fn full_decode_applies_exif_orientation() {
    let tmp = temp("orient");
    let img = image::RgbImage::from_fn(40, 20, |x, _| {
        image::Rgb(if x < 20 { [255, 0, 0] } else { [0, 0, 255] })
    });
    let mut jpeg = std::io::Cursor::new(Vec::new());
    img.write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();
    let jpeg = jpeg.into_inner();
    // APP1 Exif: big-endian TIFF, one IFD entry Orientation (0x0112, SHORT) = 6.
    let tiff: &[u8] = &[
        b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0,
    ];
    let payload = [b"Exif\0\0".as_slice(), tiff].concat();
    let mut out = jpeg[..2].to_vec();
    out.extend([0xff, 0xe1]);
    out.extend(((payload.len() + 2) as u16).to_be_bytes());
    out.extend(payload);
    out.extend(&jpeg[2..]);
    let path = tmp.join("rotated.jpg");
    std::fs::write(&path, out).unwrap();
    let full = crate::media_viewer::decode_full(&path, 8192).unwrap();
    assert_eq!(full.size, [20, 40], "portrait after rotation");
    // Red (the left half) is now on top.
    let top = full.pixels[5 * 20 + 10];
    assert!(top.r() > 200 && top.b() < 60, "{top:?}");
    let capped = crate::media_viewer::decode_full(&path, 10).unwrap();
    assert_eq!(capped.size, [5, 10], "fits the texture limit");
    let meta = keel_core::MediaMeta {
        width: 20,
        height: 40,
        gps: Some((39.5, -77.25)),
        duration_ms: Some(65_400),
        ..Default::default()
    };
    let rows = crate::media_viewer::meta_rows(&meta);
    assert!(rows.contains(&("Dimensions", "20 x 40".into())));
    assert!(rows.contains(&("GPS", "39.500000, -77.250000".into())));
    assert!(rows.contains(&("Duration", "1:05.4".into())));
}

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

/// Live check of the real app (release, GPU), `#[ignore]`:
/// `KEEL_MEDIA_LIVE=<folder of photos + videos> KEEL_SHOT_DIR=<dir> KEEL_CONFIG_DIR=<empty dir>
/// cargo test --release -p keel-app media_live -- --ignored --nocapture`.
/// Media view, scroll, hover-scrub a video, viewer (full decode), zoom, Right/Left, info
/// panel, a video in the viewer; one PNG per step.
#[test]
#[ignore]
fn media_live() {
    use crate::app::{App, Boot};
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;
    let (Some(folder), Some(shots)) = (
        std::env::var_os("KEEL_MEDIA_LIVE"),
        std::env::var_os("KEEL_SHOT_DIR"),
    ) else {
        eprintln!("media_live: set KEEL_MEDIA_LIVE and KEEL_SHOT_DIR");
        return;
    };
    let shots = PathBuf::from(shots);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 900.0))
        .wgpu()
        .build_eframe(|cc| App::new(cc, Boot::at(VPath::local(PathBuf::from(&folder)))));
    {
        let s = &mut h.state_mut().state;
        s.dual = false;
        s.panes[0].view = crate::pane::ViewMode::Media;
    }
    let run = |h: &mut Harness<App>, secs: f32| {
        let until = std::time::Instant::now() + Duration::from_secs_f32(secs);
        while std::time::Instant::now() < until {
            h.step();
            std::thread::sleep(Duration::from_millis(16));
        }
    };
    let shot = |h: &mut Harness<App>, name: &str| {
        h.render()
            .unwrap()
            .save(shots.join(format!("media-{name}.png")))
            .unwrap();
        println!("shot {name}");
    };
    run(&mut h, 8.0);
    println!(
        "textures after first screen: {}",
        h.state().state.media.len()
    );
    shot(&mut h, "grid");
    let dates = h.get_by_label("Dates").bounding_box().unwrap();
    let grid_top = egui::pos2(dates.x0 as f32, dates.y1 as f32);
    // Scroll down a few screens with the wheel.
    let over = grid_top + egui::vec2(400.0, 300.0);
    for _ in 0..6 {
        h.input_mut().events.push(egui::Event::PointerMoved(over));
        h.input_mut().events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -8.0),
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
    }
    run(&mut h, 4.0);
    shot(&mut h, "scrolled");
    // Back to the top: the clips sort first; hover the first one at 70 % of its width.
    h.state_mut()
        .state
        .run(0, Action::Move(crate::tab::Nav::Home, false));
    run(&mut h, 3.0);
    let tile = 160.0;
    let at = grid_top + egui::vec2(0.7 * tile, 0.5 * tile + 8.0);
    h.input_mut().events.push(egui::Event::PointerMoved(at));
    run(&mut h, 1.0);
    shot(&mut h, "hover-video");
    h.input_mut()
        .events
        .push(egui::Event::PointerMoved(egui::pos2(5.0, 890.0)));
    // The viewer on a portrait photo with EXIF orientation 6.
    h.state_mut().state.tab_mut(0).cursor = Some("photo_003.jpg".into());
    h.press_key(Key::Space);
    let until = std::time::Instant::now() + Duration::from_secs(20);
    while !h
        .state()
        .state
        .viewer
        .as_ref()
        .is_some_and(|v| v.has_full())
    {
        assert!(std::time::Instant::now() < until, "no full decode");
        h.step();
        std::thread::sleep(Duration::from_millis(16));
    }
    run(&mut h, 0.5);
    shot(&mut h, "viewer");
    h.press_key(Key::Plus);
    h.press_key(Key::Plus);
    h.press_key(Key::Plus);
    run(&mut h, 0.5);
    shot(&mut h, "viewer-zoom");
    h.press_key(Key::ArrowRight);
    run(&mut h, 2.0);
    shot(&mut h, "viewer-right");
    h.press_key(Key::ArrowLeft);
    h.press_key(Key::I);
    run(&mut h, 2.0);
    shot(&mut h, "viewer-info");
    h.press_key(Key::Escape);
    run(&mut h, 0.5);
    assert!(h.state().state.viewer.is_none());
    h.state_mut().state.tab_mut(0).cursor = Some("clip_1.mp4".into());
    h.press_key(Key::Space);
    run(&mut h, 6.0);
    shot(&mut h, "viewer-video");
    h.press_key(Key::Escape);
    h.state_mut().state.media.dates = true;
    run(&mut h, 6.0);
    shot(&mut h, "dates");
}
