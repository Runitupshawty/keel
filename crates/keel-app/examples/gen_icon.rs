//! Renders `assets/keel.svg` into `assets/keel.png` (256 px, window icon and Linux
//! tarball) and `assets/keel.ico` (16/32/48/256, embedded in keel.exe).
//!
//! Run after editing the SVG: `cargo run -p keel-app --example gen_icon`

use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::{ExtendedColorType, RgbaImage};
use resvg::{tiny_skia, usvg};
use std::path::Path;

fn render(tree: &usvg::Tree, px: u32) -> RgbaImage {
    let mut pixmap = tiny_skia::Pixmap::new(px, px).expect("non-zero size");
    let scale = px as f32 / tree.size().width();
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia stores premultiplied alpha; image wants straight alpha.
    let data = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    RgbaImage::from_raw(px, px, data).expect("buffer matches size")
}

fn main() {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    let svg = std::fs::read(assets.join("keel.svg")).expect("read assets/keel.svg");
    let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("parse keel.svg");

    render(&tree, 256)
        .save(assets.join("keel.png"))
        .expect("write keel.png");

    let images: Vec<RgbaImage> = [16, 32, 48, 256].map(|px| render(&tree, px)).into();
    let frames: Vec<IcoFrame> = images
        .iter()
        .map(|img| {
            // PNG-compressed frames: supported by every Windows since Vista.
            IcoFrame::as_png(
                img.as_raw(),
                img.width(),
                img.height(),
                ExtendedColorType::Rgba8,
            )
            .expect("encode icon frame")
        })
        .collect();
    let ico = std::fs::File::create(assets.join("keel.ico")).expect("create keel.ico");
    IcoEncoder::new(ico)
        .encode_images(&frames)
        .expect("write keel.ico");
    println!("wrote {}", assets.display());
}
