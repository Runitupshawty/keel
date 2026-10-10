//! The screenshots in `docs/screenshots/`, rendered by the real UI (egui_kittest, wgpu) on
//! a fixture folder. Needs a GPU, so it is `#[ignore]`d and not run in CI:
//!
//! ```sh
//! cargo test -p keel-app --bin keel docs_screenshots -- --ignored --nocapture
//! ```
//!
//! (`scripts/screenshots.sh`). The fixture is written to `KEEL_SHOTS_ROOT`, by default
//! `C:\Keel demo` on Windows and `/tmp/Keel demo` elsewhere, so the pictures show a neutral
//! path, and deleted again at the end (`scripts/screenshots.sh` removes what a watcher still
//! held open). Settings, data, the search index and the device identity stay in that folder
//! too (`KEEL_CONFIG_DIR`, `KEEL_DATA_DIR`, `KEEL_NET_SECRET=memory`); the devices are
//! offline nodes on loopback. PDF previews need pdfium in `target/deps` (`scripts/fetch-deps`);
//! without it that picture is skipped and the test says so.

use crate::app::{App, Boot};
use crate::keys::Action;
use crate::library::LibCmd;
use crate::session::Session;
use crate::state::AppState;
use egui_kittest::Harness;
use keel_core::{Library, SourceDef, SourceKind};
use keel_vfs::VPath;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const SIZE: egui::Vec2 = egui::vec2(1280.0, 800.0);

fn root() -> PathBuf {
    if let Some(dir) = std::env::var_os("KEEL_SHOTS_ROOT") {
        return dir.into();
    }
    if cfg!(windows) {
        PathBuf::from(r"C:\Keel demo")
    } else {
        PathBuf::from("/tmp/Keel demo")
    }
}

fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots")
}

/// 2026-09-12 + `days`, at `hour`.
fn at(days: u64, hour: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_171_200 + days * 86_400 + hour * 3_600)
}

fn write(path: &Path, body: impl AsRef<[u8]>, modified: SystemTime) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(modified).unwrap();
}

/// A small landscape: sky, sun, two ridges and a lake, its colours from `seed`.
fn landscape(seed: u32, w: u32, h: u32) -> image::RgbImage {
    let hue = |t: f32, a: [f32; 3], b: [f32; 3]| {
        image::Rgb([0, 1, 2].map(|i| (a[i] + (b[i] - a[i]) * t).clamp(0.0, 255.0) as u8))
    };
    let skies = [
        ([40.0, 70.0, 140.0], [240.0, 170.0, 110.0]),
        ([90.0, 150.0, 220.0], [210.0, 230.0, 250.0]),
        ([30.0, 30.0, 70.0], [200.0, 90.0, 120.0]),
        ([120.0, 170.0, 200.0], [250.0, 220.0, 160.0]),
        ([60.0, 110.0, 120.0], [180.0, 220.0, 200.0]),
    ];
    let (top, bottom) = skies[seed as usize % skies.len()];
    let s = seed as f32;
    let sun = (
        w as f32 * (0.2 + 0.6 * ((s * 0.37).sin() * 0.5 + 0.5)),
        h as f32 * 0.35,
    );
    image::RgbImage::from_fn(w, h, |x, y| {
        let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
        let far = 0.55 + 0.08 * (fx * 7.0 + s).sin() + 0.04 * (fx * 17.0 + s * 2.0).sin();
        let near = 0.68 + 0.06 * (fx * 4.0 + s * 1.3).cos() + 0.03 * (fx * 23.0).sin();
        if fy > 0.82 {
            let wave = 0.05 * ((fy * 90.0 + fx * 6.0).sin());
            hue(0.4 + wave, top, [20.0, 40.0, 60.0])
        } else if fy > near {
            hue((fy - near) * 3.0, [40.0, 70.0, 45.0], [20.0, 35.0, 25.0])
        } else if fy > far {
            hue((fy - far) * 4.0, [90.0, 100.0, 130.0], [55.0, 65.0, 90.0])
        } else {
            let d = ((x as f32 - sun.0).powi(2) + (y as f32 - sun.1).powi(2)).sqrt();
            if d < h as f32 * 0.07 {
                image::Rgb([255, 236, 190])
            } else {
                hue(fy / far, top, bottom)
            }
        }
    })
}

fn jpeg(path: &Path, seed: u32, modified: SystemTime) {
    let mut out = std::io::Cursor::new(Vec::new());
    landscape(seed, 960, 640)
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .unwrap();
    write(path, out.into_inner(), modified);
}

/// A one-page report: a title band, a few lines and a bar chart (plain PDF, no fonts
/// embedded: Helvetica).
fn report_pdf() -> Vec<u8> {
    let mut page = String::from(
        "0.16 0.27 0.45 rg 0 742 595 100 re f\n\
         1 1 1 rg BT /F1 26 Tf 50 782 Td (Garden report 2026) Tj ET\n\
         0.15 0.15 0.15 rg BT /F1 14 Tf 50 700 Td (Spring harvest) Tj ET\n\
         BT /F2 11 Tf 50 676 Td 16 TL\n\
         (Tomatoes and beans did best after the new drip line.) Tj T*\n\
         (Water use fell 18% against last spring.) Tj T*\n\
         (Next: a second compost bin and netting for the berries.) Tj ET\n\
         0.6 0.6 0.6 RG 50 400 m 545 400 l S\n",
    );
    for (i, (label, kg)) in [
        ("Mar", 4),
        ("Apr", 9),
        ("May", 15),
        ("Jun", 22),
        ("Jul", 18),
    ]
    .iter()
    .enumerate()
    {
        let x = 75 + i * 95;
        page += &format!(
            "0.30 0.55 0.35 rg {x} 400 50 {} re f\n0.3 0.3 0.3 rg BT /F2 10 Tf {} 384 Td ({label}) Tj ET\n",
            kg * 9,
            x + 15
        );
    }
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 4 0 R /F2 5 0 R >> >> /Contents 6 0 R >>".to_owned(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>".to_owned(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        format!("<< /Length {} >>\nstream\n{page}endstream", page.len()),
    ];
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out += &format!("{} 0 obj\n{body}\nendobj\n", i + 1);
    }
    let xref = out.len();
    out += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for o in offsets {
        out += &format!("{o:010} 00000 n \n");
    }
    out += &format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    );
    out.into_bytes()
}

fn zip(path: &Path, files: &[(&str, &str)]) {
    let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    for (name, body) in files {
        z.start_file(*name, opts).unwrap();
        std::io::Write::write_all(&mut z, body.as_bytes()).unwrap();
    }
    z.finish().unwrap();
}

const MAIN_RS: &str = r#"//! Waters the garden when the soil is dry.

use std::time::Duration;

const DRY: f32 = 0.30;

struct Bed {
    name: &'static str,
    moisture: f32,
}

fn main() {
    let beds = [
        Bed { name: "tomatoes", moisture: 0.22 },
        Bed { name: "herbs", moisture: 0.41 },
        Bed { name: "beans", moisture: 0.28 },
    ];
    for bed in beds.iter().filter(|b| b.moisture < DRY) {
        println!("watering {} for 5 minutes", bed.name);
        std::thread::sleep(Duration::from_secs(1));
    }
}
"#;

const NOTES_MD: &str = "# Garden notes

Spring plan for the raised beds.

## To do

- Order seeds: tomatoes, basil, runner beans
- Fix the drip line on the **east** bed
- Compost delivery on *Saturday*

## Watering

| Bed | Days |
| --- | --- |
| Tomatoes | Mon, Thu |
| Herbs | Wed |
| Beans | Tue, Sat |
";

const BUDGET: &str = "Month,Seeds,Tools,Water\n\
January,12.50,0,18.20\n\
February,30.00,45.90,17.80\n\
March,22.40,12.00,21.10\n\
April,8.75,0,26.30\n";

/// Documents (left pane), Pictures (photos for the grid and the media view) and Backup
/// (a few copies, and an older notes.md, for duplicates and the conflict).
fn write_fixture(root: &Path) {
    // Only ever replace a folder this test made.
    let marker = root.join(".keel-shots");
    assert!(
        !root.exists() || marker.exists(),
        "{} exists and was not made by this test: set KEEL_SHOTS_ROOT",
        root.display()
    );
    let _ = std::fs::remove_dir_all(root);
    write(&marker, "", SystemTime::now());
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../keel-preview/tests/fixtures");
    let read = |name: &str| std::fs::read(fixtures.join(name)).unwrap();
    let docs = root.join("Documents");
    write(&docs.join("garden.rs"), MAIN_RS, at(20, 9));
    write(&docs.join("notes.md"), NOTES_MD, at(21, 18));
    write(&docs.join("budget.csv"), BUDGET, at(19, 20));
    write(&docs.join("report.pdf"), report_pdf(), at(14, 11));
    write(&docs.join("letter.docx"), read("sample.docx"), at(9, 15));
    write(&docs.join("plants.xlsx"), read("sample.xlsx"), at(12, 10));
    write(
        &docs.join("todo.txt"),
        "Call the plumber\nReturn the ladder\nBook the car service\n",
        at(22, 8),
    );
    jpeg(&docs.join("lake.jpg"), 0, at(18, 16));
    write(
        &docs.join("Projects/shed/plan.md"),
        "# Shed\n\n2.4 m x 1.8 m, felt roof.\n",
        at(5, 10),
    );
    write(
        &docs.join("Projects/shed/materials.csv"),
        "Item,Qty\nBoards,24\nScrews,200\n",
        at(5, 11),
    );
    write(
        &docs.join("Reports/2026-q1.pdf"),
        read("sample.pdf"),
        at(2, 9),
    );
    write(
        &docs.join("Reports/2026-q2.pdf"),
        read("corrupt.pdf"),
        at(3, 9),
    );
    let site = docs.join("site.zip");
    zip(
        &site,
        &[
            ("README.md", "# Site\n\nThe family recipe site.\n"),
            (
                "index.html",
                "<!doctype html>\n<title>Recipes</title>\n<h1>Recipes</h1>\n",
            ),
            ("css/style.css", "body { font: 16px/1.5 sans-serif; }\n"),
            (
                "recipes/bread.md",
                "# Bread\n\n- 500 g flour\n- 350 g water\n- 10 g salt\n- 5 g yeast\n",
            ),
            (
                "recipes/soup.md",
                "# Tomato soup\n\nRoast, blend, season.\n",
            ),
        ],
    );
    let f = std::fs::File::options().write(true).open(&site).unwrap();
    f.set_modified(at(16, 12)).unwrap();
    let pics = root.join("Pictures");
    for i in 0..18u32 {
        let (day, hour) = match i {
            0..=6 => (0, 9 + i as u64),
            7..=11 => (1, 8 + i as u64),
            _ => (20, i as u64 - 4),
        };
        jpeg(
            &pics.join(format!("IMG_{:04}.jpg", 2041 + i)),
            i,
            at(day, hour),
        );
    }
    let backup = root.join("Backup");
    write(&backup.join("budget.csv"), BUDGET, at(19, 20));
    write(&backup.join("report.pdf"), report_pdf(), at(14, 11));
    write(
        &backup.join("notes-2025.md"),
        "# Garden notes 2025\n\nToo many courgettes.\n",
        at(1, 9),
    );
    std::fs::create_dir_all(root.join("Inbox")).unwrap();
}

/// The fixture's sidebar: these quick-access folders and one drive, never the real ones.
fn neutral_sidebar(s: &mut AppState, root: &Path) {
    let p = |name: &str| VPath::local(root.join(name));
    s.sidebar.quick = vec![
        ("Home".into(), VPath::local(root)),
        ("Documents".into(), p("Documents")),
        ("Pictures".into(), p("Pictures")),
        ("Backup".into(), p("Backup")),
    ];
    let drive = if cfg!(windows) { "C:" } else { "/" };
    s.sidebar.drives = vec![(
        drive.into(),
        String::new(),
        182_000_000_000,
        511_000_000_000,
    )];
    s.sidebar.drives_requested = Some(Instant::now());
}

fn harness(
    root: &Path,
    left: VPath,
    right: VPath,
    settings: crate::settings::Settings,
) -> Harness<'static, App> {
    let root = root.to_owned();
    let mut h = Harness::builder()
        .with_size(SIZE)
        .wgpu()
        .build_eframe(move |cc| {
            let boot = Boot {
                settings,
                session: Session {
                    panes: vec![vec![left.clone()], vec![right]],
                    ..Session::single(left.clone())
                },
                ..Boot::at(VPath::local(&root))
            };
            App::new(cc, boot)
        });
    h.input_mut().max_texture_side = Some(8192);
    h
}

/// Steps until `done` (at most `secs`); false when it timed out.
fn wait(h: &mut Harness<App>, secs: u64, done: &dyn Fn(&AppState) -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while !done(&h.state().state) {
        if Instant::now() > end {
            return false;
        }
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.run_steps(3);
    true
}

fn listed(s: &AppState) -> bool {
    (0..2).all(|p| !s.tab(p).loading)
}

fn previewed(s: &AppState) -> bool {
    let target = s.tab(s.active).cursor.clone();
    s.preview.current.is_some()
        && s.preview.key.as_ref().map(|k| k.path.name()) == target.as_deref()
}

/// Renders the window to `docs/screenshots/<name>.png` (RGB, best compression).
fn shot(h: &mut Harness<App>, root: &Path, name: &str) {
    neutral_sidebar(&mut h.state_mut().state, root);
    h.run_steps(4);
    let img = image::DynamicImage::ImageRgba8(h.render().unwrap()).into_rgb8();
    let path = out_dir().join(format!("{name}.png"));
    let file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    let enc = image::codecs::png::PngEncoder::new_with_quality(
        file,
        image::codecs::png::CompressionType::Best,
        image::codecs::png::FilterType::Adaptive,
    );
    img.write_with_encoder(enc).unwrap();
    println!("shot {}", path.display());
}

fn select(s: &mut AppState, p: usize, names: &[&str]) {
    let tab = s.tab_mut(p);
    tab.selected.clear();
    for (i, n) in names.iter().enumerate() {
        tab.click(n, i > 0, false);
    }
}

#[test]
#[ignore]
fn docs_screenshots() {
    let _env = crate::settings::TEST_ENV.lock();
    let root = root();
    write_fixture(&root);
    let keel = root.join(".keel");
    std::env::set_var("KEEL_CONFIG_DIR", keel.join("config"));
    std::env::set_var("KEEL_DATA_DIR", keel.join("data"));
    std::env::set_var("KEEL_NET_SECRET", "memory");
    let pdfium = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deps");
    keel_preview::init_pdfium(&pdfium);
    std::fs::create_dir_all(out_dir()).unwrap();
    let skipped = run(&root);
    let _ = std::fs::remove_dir_all(&root);
    for var in ["KEEL_CONFIG_DIR", "KEEL_DATA_DIR", "KEEL_NET_SECRET"] {
        std::env::remove_var(var);
    }
    if !skipped.is_empty() {
        println!("skipped: {}", skipped.join(", "));
    }
}

fn run(root: &Path) -> Vec<&'static str> {
    let mut skipped = Vec::new();
    let (docs, pics, backup) = (
        root.join("Documents"),
        root.join("Pictures"),
        root.join("Backup"),
    );
    let settings = crate::settings::Settings {
        remotes: vec![keel_vfs::RemoteHost {
            id: "nas".into(),
            label: "NAS".into(),
            host: "nas.example.com".into(),
            port: 22,
            user: "demo".into(),
            auth: keel_vfs::RemoteAuth::Agent,
            home: Some("/volume1".into()),
            bookmarks: vec![("Photos".into(), "/volume1/photo".into())],
            use_ssh_config: false,
        }],
        devices: crate::devices::DeviceSettings {
            enabled: true,
            explicit: true,
            label: "Desktop".into(),
            inbox: root.join("Inbox").display().to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut h = harness(root, VPath::local(&docs), VPath::local(&pics), settings);

    // Search: the Keel index of the fixture only (not the user's folders).
    assert!(wait(&mut h, 60, &|s| s.searcher.is_some()), "searcher");
    let index = root.join(".keel").join("index");
    let walk = keel_search::WalkIndexSearcher::open(index, vec![root.to_owned()]).unwrap();
    h.state_mut().state.searcher = Some(Arc::new(walk));
    h.state_mut().state.search_reason = None;

    // The library: Documents, Pictures and Backup, indexed and hashed.
    let data = root.join(".keel").join("data");
    let lib = Arc::new(Library::open(&data, "main").unwrap());
    lib.set_router(h.state().state.router.clone());
    h.state_mut().state.library.set_open(lib.clone());
    for name in ["Documents", "Pictures", "Backup"] {
        h.state_mut().state.run(
            0,
            Action::Library(LibCmd::Register(SourceDef {
                label: name.into(),
                root: VPath::local(root.join(name)),
                kind: SourceKind::Folder,
                include_hidden: false,
                ignore: Vec::new(),
                poll_secs: None,
                hash_shares: false,
            })),
        );
    }

    // Devices: this window's node and a paired "Laptop", both offline on loopback.
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    );
    let open = |dir: &Path, lib: &Arc<Library>, label: &str| {
        let handler = Arc::new(keel_net::LibraryHandler::new(lib.clone()));
        let node = rt
            .block_on(keel_net::Node::open_with_options(
                Arc::new(keel_vfs::cloud::MemoryStore::default()),
                dir,
                handler.clone(),
                keel_net::NodeOptions::offline(),
            ))
            .unwrap();
        node.set_label(label);
        (node, handler)
    };
    let (node, handler) = open(&data, &lib, "Desktop");
    let laptop_data = root.join(".keel").join("laptop");
    let laptop_lib = Arc::new(Library::open(&laptop_data, "main").unwrap());
    let (laptop, _laptop_handler) = open(&laptop_data, &laptop_lib, "Laptop");
    let code = rt.block_on(node.pair_code()).unwrap();
    let ticket: keel_net::PairCode = code.ticket().parse().unwrap();
    rt.block_on(laptop.pair_with(&ticket)).unwrap();
    laptop.set_sync_peers([keel_net::PeerId(node.id())]);
    {
        let s = &mut h.state_mut().state;
        s.settings.devices.sync = vec![laptop.id().to_string()];
        s.devices.set_open(lib.clone(), node, handler, rt.clone());
    }
    let ready = wait(&mut h, 180, &|s| {
        s.library.sources.len() == 3
            && s.library.jobs.values().any(|j| j.kind == "hash")
            && !s.library.jobs.values().any(|j| j.active())
            && s.library.stats.files > 0
            && s.searcher.as_ref().is_some_and(|x| x.available())
            && s.sidebar.devices.as_ref().is_some_and(|d| d.len() == 1)
            && listed(s)
    });
    assert!(ready, "library, search and devices ready");

    // Main window: a second tab, a selection on the left, thumbnails on the right.
    h.state_mut().state.panes[1].view = crate::pane::ViewMode::Grid;
    let projects = VPath::local(docs.join("Projects"));
    h.state_mut().state.run(0, Action::NewTabAt(projects));
    h.state_mut().state.panes[0].active = 0;
    select(&mut h.state_mut().state, 0, &["notes.md", "budget.csv"]);
    wait(&mut h, 20, &|s| s.thumbs.len() >= 12);
    wait(&mut h, 30, &|s| {
        s.library.jobs.is_empty() && s.jobs.list.is_empty()
    });
    std::thread::sleep(Duration::from_millis(500));
    shot(&mut h, root, "main-window");

    // The preview panel on code, an image and a PDF.
    h.state_mut().state.preview.open = true;
    for (file, name) in [
        ("garden.rs", "preview-text"),
        ("lake.jpg", "preview-image"),
        ("report.pdf", "preview-pdf"),
    ] {
        select(&mut h.state_mut().state, 0, &[file]);
        let failed = !wait(&mut h, 30, &previewed)
            || matches!(
                &h.state().state.preview.current,
                Some(keel_preview::Preview::Error(_))
            );
        if failed {
            println!("{name}: {:?}", h.state().state.preview.current);
            skipped.push(name);
            continue;
        }
        h.run_steps(6);
        shot(&mut h, root, name);
    }

    // An archive as a folder, with an entry previewed.
    let zip = VPath::join_archive(&VPath::local(docs.join("site.zip")), "recipes");
    h.state_mut().state.run(0, Action::Navigate(zip));
    assert!(wait(&mut h, 30, &listed), "archive listed");
    select(&mut h.state_mut().state, 0, &["bread.md"]);
    if wait(&mut h, 30, &previewed) {
        shot(&mut h, root, "archive");
    } else {
        skipped.push("archive");
    }
    h.state_mut().state.preview.open = false;

    // The media view of Pictures, one pane, grouped by day (the fixture photos have no
    // capture time: their modified days).
    {
        let s = &mut h.state_mut().state;
        s.run(0, Action::Navigate(VPath::local(&pics)));
        s.dual = false;
        s.panes[0].view = crate::pane::ViewMode::Media;
        s.media.tile = crate::media::TileSize::L;
        s.media.dates = true;
    }
    assert!(wait(&mut h, 30, &listed), "pictures listed");
    let tiles = wait(&mut h, 90, &|s| s.media.len() >= 18);
    println!(
        "media: {} textures, {} queued",
        h.state().state.media.len(),
        h.state().state.media.queued_for_tests()
    );
    if tiles {
        std::thread::sleep(Duration::from_millis(500));
        shot(&mut h, root, "media-grid");
    } else {
        skipped.push("media-grid");
    }

    // The command palette, filtered.
    {
        let s = &mut h.state_mut().state;
        s.dual = true;
        s.panes[0].view = crate::pane::ViewMode::Details;
        s.run(0, Action::Navigate(VPath::local(&docs)));
    }
    assert!(wait(&mut h, 30, &listed), "documents listed");
    h.state_mut().state.run(0, Action::Palette);
    h.run_steps(3);
    for c in "pre".chars() {
        h.input_mut().events.push(egui::Event::Text(c.into()));
        h.run_steps(2);
    }
    shot(&mut h, root, "command-palette");
    h.state_mut().state.palette.open = false;
    h.run_steps(3);

    // Settings → Remotes.
    h.state_mut().state.remotes.page = crate::settings::Page::Remotes;
    h.state_mut().state.settings_open = true;
    shot(&mut h, root, "settings-remotes");

    // Settings → Devices, after a first library sync with the laptop.
    wait(&mut h, 20, &|s| !s.devices.last_sync.is_empty());
    h.state_mut().state.remotes.page = crate::settings::Page::Devices;
    shot(&mut h, root, "settings-devices");
    h.state_mut().state.settings_open = false;

    // Overview beside the Documents folder (its Copies column).
    h.state_mut()
        .state
        .run(0, Action::Library(LibCmd::Overview));
    h.state_mut()
        .state
        .run(1, Action::Navigate(VPath::local(&docs)));
    h.state_mut().state.panes[1].view = crate::pane::ViewMode::Details;
    let overview = wait(&mut h, 60, &|s| {
        s.library.dup_summary.is_some_and(|(n, _)| n > 0)
            && s.library.protection.is_some()
            && listed(s)
    });
    if overview {
        shot(&mut h, root, "library-overview");
    } else {
        skipped.push("library-overview");
    }

    // A library search across the sources.
    h.state_mut().state.run(0, Action::CloseTab);
    h.state_mut().state.panes[0].active = 0;
    h.state_mut().state.run(0, Action::Search);
    {
        let tab = h.state_mut().state.tab_mut(0);
        tab.library_search = true;
        if let crate::tab::TabKind::Search { query, due, .. } = &mut tab.kind {
            *query = "ext:pdf".into();
            *due = Some(Instant::now());
        }
    }
    let found = wait(&mut h, 30, &|s| {
        !s.tab(0).loading && s.tab(0).entries().len() >= 3
    });
    if found {
        shot(&mut h, root, "search");
    } else {
        skipped.push("search");
    }
    h.state_mut().state.run(0, Action::CloseTab);
    h.state_mut().state.panes[0].active = 0;

    // Copy two files onto Backup, where budget.csv already is: the preview.
    h.state_mut()
        .state
        .run(1, Action::Navigate(VPath::local(&backup)));
    assert!(wait(&mut h, 30, &listed), "listed for the copy");
    select(&mut h.state_mut().state, 0, &["budget.csv", "notes.md"]);
    let paths = vec![
        VPath::local(docs.join("budget.csv")),
        VPath::local(docs.join("notes.md")),
    ];
    h.state_mut().state.run(
        1,
        Action::Drop {
            paths,
            from: Some((0, VPath::local(&docs))),
            dst: VPath::local(&backup),
        },
    );
    if wait(&mut h, 30, &|s| s.library.plan.is_some()) {
        shot(&mut h, root, "copy-preview");
    } else {
        skipped.push("copy-preview");
    }
    h.state_mut().state.library.plan = None;

    h.state_mut().state.devices.close_now();
    rt.block_on(laptop.close());
    h.state_mut().state.library.close_now();
    laptop_lib.close(Duration::from_secs(5));
    skipped
}
