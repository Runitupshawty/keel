//! Task 68: the Keel Finder icons parse, render and stay as drawn.

use super::*;
use egui::load::SizeHint;

/// Rasterizes an icon the way the window does (egui_extras' SVG loader, usvg/resvg).
fn render(icon: &Icon, light: bool, px: u32) -> egui::ColorImage {
    let text = svg(icon, light, px);
    egui_extras::image::load_svg_bytes_with_size(text.as_bytes(), Some(SizeHint::Size(px, px)))
        .unwrap_or_else(|e| panic!("{} ({px} px, light {light}): {e}", icon.key()))
}

/// Share of pixels with any coverage.
fn coverage(img: &egui::ColorImage) -> f32 {
    let lit = img.pixels.iter().filter(|p| p.a() > 0).count();
    lit as f32 / img.pixels.len() as f32
}

/// Mean color of the mostly opaque pixels (premultiplied, so only opaque ones).
fn mean_rgb(img: &egui::ColorImage) -> [f32; 3] {
    let opaque: Vec<_> = img.pixels.iter().filter(|p| p.a() > 240).collect();
    let n = opaque.len().max(1) as f32;
    let sum = |f: fn(&egui::Color32) -> u8| opaque.iter().map(|p| f(p) as f32).sum::<f32>() / n;
    [sum(|p| p.r()), sum(|p| p.g()), sum(|p| p.b())]
}

#[test]
fn every_icon_is_a_self_contained_svg() {
    for icon in Icon::catalog() {
        for light in [false, true] {
            for px in SIZES {
                let text = svg(&icon, light, px);
                assert_eq!(
                    crate::icon_theme::svg_unsafe(text.as_bytes()),
                    None,
                    "{} {px}: {text}",
                    icon.key()
                );
            }
        }
    }
}

#[test]
fn every_icon_renders_at_16_and_128_px() {
    for icon in Icon::catalog() {
        for light in [false, true] {
            for px in [16, 128] {
                let img = render(&icon, light, px);
                assert_eq!(img.size, [px as usize, px as usize], "{}", icon.key());
                let c = coverage(&img);
                assert!(
                    (0.15..0.95).contains(&c),
                    "{} at {px} px covers {c:.2} of its square",
                    icon.key()
                );
                if let Icon::Folder { color: None, .. } = icon {
                    let [r, g, b] = mean_rgb(&img);
                    assert!(
                        b > r + 40.0 && b > g,
                        "{} is not blue: {r} {g} {b}",
                        icon.key()
                    );
                }
                if let Icon::Folder {
                    color: Some([0xe5, ..]),
                    glyph: None,
                    ..
                } = icon
                {
                    let [r, _, b] = mean_rgb(&img);
                    assert!(r > b + 40.0, "{} is not red", icon.key());
                }
            }
        }
    }
}

/// Every size bucket draws; the band's label only from 64 px, the emboss and shadows only
/// above 16 px.
#[test]
fn every_bucket_renders_and_small_sizes_are_simpler() {
    let png = Icon::file("photo.PNG");
    let downloads = Icon::Folder {
        glyph: Some(Special::Downloads),
        open: false,
        color: None,
    };
    for px in SIZES {
        for icon in [FOLDER, downloads, png, Icon::Place(Place::TrashFull)] {
            assert_eq!(render(&icon, true, px).size, [px as usize; 2]);
        }
    }
    let labelled = |px| svg(&png, true, px).matches("stroke=\"#ffffff\"").count();
    assert!(labelled(64) > labelled(32), "label text from 64 px");
    assert!(
        svg(&downloads, true, 32).contains("opacity=\"0.5\""),
        "emboss"
    );
    assert!(!svg(&downloads, true, 16).contains("opacity=\"0.5\""));
    assert!(svg(&FOLDER, false, 16).len() < svg(&FOLDER, false, 64).len());
    // Light and dark differ where it matters: paper, folder blue, outlines.
    for icon in [FOLDER, png, Icon::Place(Place::Drive)] {
        assert_ne!(
            svg(&icon, true, 64),
            svg(&icon, false, 64),
            "{}",
            icon.key()
        );
    }
}

/// FNV-1a, stable across platforms and Rust versions (unlike `DefaultHasher`).
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The artwork as drawn: a hash of every icon's SVG at 16 and 128 px in both variants.
/// After a deliberate change, run with `KEEL_UPDATE_SNAPSHOTS=1` and commit the file.
#[test]
fn snapshot_hashes() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/finder-icons.snap");
    let mut now = String::new();
    for icon in Icon::catalog() {
        for light in [false, true] {
            for px in [16, 128] {
                let variant = if light { "light" } else { "dark" };
                let hash = fnv(svg(&icon, light, px).as_bytes());
                let _ = writeln!(now, "{} {variant} {px} {hash:016x}", icon.key());
            }
        }
    }
    if std::env::var_os("KEEL_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &now).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    let changed: Vec<_> = (now.lines())
        .filter(|l| !want.lines().any(|w| w == *l))
        .collect();
    assert!(
        changed.is_empty() && want.lines().count() == now.lines().count(),
        "icons changed (KEEL_UPDATE_SNAPSHOTS=1 to accept): {changed:#?}"
    );
}

#[test]
fn keys_and_uris_round_trip() {
    let mut seen = std::collections::HashSet::new();
    for icon in Icon::catalog() {
        let key = icon.key();
        assert!(seen.insert(key.clone()), "duplicate key {key}");
        assert_eq!(Icon::parse_key(&key), Some(icon), "{key}");
        for light in [false, true] {
            let u = uri(&icon, light, 128);
            assert!(
                u.ends_with(".svg"),
                "egui's SVG loader goes by the extension"
            );
            assert_eq!(parse_uri(&u), Some((icon, light, 128)));
        }
    }
    let open_red = Icon::Folder {
        glyph: Some(Special::Home),
        open: true,
        color: Some([1, 2, 3]),
    };
    assert_eq!(open_red.key(), "folder-open-home-c010203");
    assert_eq!(Icon::parse_key(&open_red.key()), Some(open_red));
    for bad in [
        "",
        "folder-",
        "folder-home-open",
        "folder-c01020",
        "folder-cxyzxyz",
        "folder-home-home",
        "doc-nope",
        "doc-text-abc",
        "doc-text-ABCDE",
        "doc-text-A-B",
        "drives",
    ] {
        assert_eq!(Icon::parse_key(bad), None, "{bad}");
    }
    for bad in [
        "bytes://keel-finder/light/100/folder.svg",
        "bytes://keel-finder/dim/16/folder.svg",
        "bytes://keel-finder/light/16/folder.png",
        "bytes://other/light/16/folder.svg",
    ] {
        assert_eq!(parse_uri(bad), None, "{bad}");
    }
}

#[test]
fn kinds_and_labels() {
    let key = |name: &str| Icon::file(name).key();
    assert_eq!(key("photo.JPG"), "doc-image-JPG");
    assert_eq!(key("clip.mkv"), "doc-video-MKV");
    assert_eq!(key("song.flac"), "doc-audio-FLAC");
    assert_eq!(key("logs.tar.gz"), "doc-archive-GZ");
    assert_eq!(key("main.rs"), "doc-code-RS");
    assert_eq!(key("index.ts"), "doc-code-TS");
    assert_eq!(key("notes.txt"), "doc-text-TXT");
    assert_eq!(key("paper.pdf"), "doc-pdf-PDF");
    assert_eq!(key("budget.xlsx"), "doc-spreadsheet-XLSX");
    assert_eq!(key("talk.pptx"), "doc-presentation-PPTX");
    assert_eq!(key("setup.exe"), "doc-executable-EXE");
    assert_eq!(
        key("data.jsonc"),
        "doc-code-JSON",
        "four characters at most"
    );
    assert_eq!(key("blob.xyz"), "doc-generic-XYZ");
    assert_eq!(key("README"), "doc-generic");
    assert_eq!(key(".gitignore"), "doc-generic");
    assert_eq!(key("a.c++"), "doc-generic", "no label it cannot print");
    assert_eq!(key("a.ünï"), "doc-generic");
    assert_eq!(Label::new(""), None);
    assert_eq!(Label::new("md").unwrap().as_str(), "MD");
    // Every printable label character has strokes.
    for c in (b'A'..=b'Z').chain(b'0'..=b'9') {
        assert!(!letter(c).is_empty(), "{}", c as char);
    }
}

#[test]
fn folders_by_path() {
    let base = std::env::temp_dir().join("keel-finder-icons-test");
    let docs = VPath::local(base.join("Docs"));
    let special = |s| Icon::Folder {
        glyph: Some(s),
        open: false,
        color: None,
    };
    let known = vec![(docs.clone(), special(Special::Documents))];
    assert_eq!(
        folder_in(&docs, false, &known, false),
        special(Special::Documents)
    );
    assert_eq!(
        folder_in(&docs.join("sub"), true, &known, false),
        Icon::Folder {
            glyph: None,
            open: true,
            color: None
        }
    );
    let trash = keel_vfs::trashbin::root();
    assert_eq!(
        folder_in(&trash, false, &[], false),
        Icon::Place(Place::TrashEmpty)
    );
    assert_eq!(
        folder_in(&trash, false, &[], true),
        Icon::Place(Place::TrashFull)
    );
    let place = |s: &str| folder_in(&VPath::parse(s).unwrap(), false, &[], false);
    assert_eq!(place("sftp://host/"), Icon::Place(Place::Network));
    assert_eq!(place("ftp://host"), Icon::Place(Place::Network));
    assert_eq!(place("cloud://account/"), Icon::Place(Place::Cloud));
    assert_eq!(place("cloud://account/Photos"), FOLDER);
    assert_eq!(place("mem://t/"), FOLDER, "not a place Keel names");
    if cfg!(windows) {
        assert_eq!(place("file:///C:/"), Icon::Place(Place::Drive));
        assert_eq!(place("file:///C:/Users"), FOLDER);
        assert_eq!(
            folder_in(&VPath::local(r"\\server\share"), false, &[], false),
            Icon::Place(Place::Network)
        );
        assert_eq!(
            folder_in(&VPath::local(r"\\server\share\dir"), false, &[], false),
            FOLDER
        );
        // Windows paths match whatever their case.
        let upper = VPath::local(base.join("DOCS"));
        assert_eq!(
            folder_in(&upper, false, &known, false),
            special(Special::Documents)
        );
    } else {
        assert_eq!(place("file:///"), Icon::Place(Place::Drive));
        assert_eq!(place("file:///home"), FOLDER);
    }
}

#[test]
fn buckets_and_cache() {
    assert_eq!(bucket(1.0), 16);
    assert_eq!(bucket(16.0), 16);
    assert_eq!(bucket(16.5), 16, "within 5 %");
    assert_eq!(bucket(20.0), 32);
    assert_eq!(bucket(56.0 * 1.5), 128);
    assert_eq!(bucket(4000.0), 512);
    let bytes = |src: ImageSource<'static>| match src {
        ImageSource::Bytes { bytes, .. } => bytes,
        other => panic!("{other:?}"),
    };
    let a = bytes(source(&FOLDER, false, 64));
    let b = bytes(source(&FOLDER, false, 60));
    assert!(
        std::ptr::eq(a.as_ref(), b.as_ref()),
        "the same bucket is generated once"
    );
}

/// Writes contact sheets (PNG, one per variant and a few rows each) and the SVGs to
/// `KEEL_FINDER_ICONS_OUT` for a look at the artwork; nothing without it.
#[test]
fn preview_sheets() {
    let Some(out) = std::env::var_os("KEEL_FINDER_ICONS_OUT") else {
        return;
    };
    let out = std::path::PathBuf::from(out);
    std::fs::create_dir_all(&out).unwrap();
    let cols = [16u32, 32, 64, 128, 256];
    let pad = 12u32;
    let width = cols.iter().map(|c| c + pad).sum::<u32>() + pad;
    let row = 256 + pad;
    let icons = Icon::catalog();
    for light in [true, false] {
        let bg = if light {
            [246u8, 246, 246]
        } else {
            [32u8, 33, 36]
        };
        let variant = if light { "light" } else { "dark" };
        for (n, chunk) in icons.chunks(8).enumerate() {
            let height = row * chunk.len() as u32 + pad;
            let mut sheet = image::RgbaImage::from_pixel(
                width,
                height,
                image::Rgba([bg[0], bg[1], bg[2], 255]),
            );
            for (r, icon) in chunk.iter().enumerate() {
                let mut x = pad;
                for &px in &cols {
                    let img = render(icon, light, px);
                    let y0 = pad + r as u32 * row + (256 - px) / 2;
                    for (i, p) in img.pixels.iter().enumerate() {
                        let (ix, iy) = (i as u32 % px, i as u32 / px);
                        let dst = sheet.get_pixel_mut(x + ix, y0 + iy);
                        let a = p.a() as f32 / 255.0;
                        let blend =
                            |c: u8, b: u8| (c as f32 + b as f32 * (1.0 - a)).min(255.0) as u8;
                        *dst = image::Rgba([
                            blend(p.r(), bg[0]),
                            blend(p.g(), bg[1]),
                            blend(p.b(), bg[2]),
                            255,
                        ]);
                    }
                    x += px + pad;
                }
                let text = svg(icon, light, 128);
                std::fs::write(out.join(format!("{}-{variant}.svg", icon.key())), text).unwrap();
            }
            sheet
                .save(out.join(format!("sheet-{variant}-{n}.png")))
                .unwrap();
        }
    }
}
