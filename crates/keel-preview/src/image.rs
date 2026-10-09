use crate::{Preview, Request, Rgba};
use image::{DynamicImage, GenericImageView};
use resvg::usvg::{fontdb, Options, Tree};
use std::sync::{Arc, OnceLock};

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg"];

pub(crate) fn accepts(ext: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&ext)
}

pub(crate) fn render(req: &Request, ext: &str) -> Preview {
    let result = if ext == "svg" {
        render_svg(req)
    } else {
        image::open(&req.bytes_path)
            .map_err(|error| error.to_string())
            .map(|image| downscale(image, req.max_px))
    };
    match result {
        Ok(image) => Preview::Image(image),
        Err(error) => Preview::Error(error),
    }
}

fn downscale(image: DynamicImage, max_px: u32) -> Rgba {
    let (w, h) = image.dimensions();
    let rgba = if max_px > 0 && (w > max_px || h > max_px) {
        image.thumbnail(max_px, max_px).to_rgba8()
    } else {
        image.to_rgba8()
    };
    Rgba {
        w: rgba.width(),
        h: rgba.height(),
        data: rgba.into_raw(),
    }
}

/// System fonts, loaded once (a few hundred ms) so SVG `<text>` renders.
fn fonts() -> Arc<fontdb::Database> {
    static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            // usvg resolves generic families through fontdb; its defaults
            // (Times New Roman / Arial / Courier New) do not exist on Linux or
            // macOS CI, so point each generic family at a face that is present.
            let families: Vec<String> = db
                .faces()
                .filter_map(|f| f.families.first().map(|(name, _)| name.clone()))
                .collect();
            let pick = |prefs: &[&str]| -> Option<String> {
                prefs
                    .iter()
                    .find_map(|p| families.iter().find(|f| f.eq_ignore_ascii_case(p)).cloned())
                    .or_else(|| families.first().cloned())
            };
            let sans = pick(&[
                "Segoe UI",
                "Helvetica",
                "Arial",
                "DejaVu Sans",
                "Liberation Sans",
                "Noto Sans",
            ]);
            let serif = pick(&[
                "Times New Roman",
                "Times",
                "DejaVu Serif",
                "Liberation Serif",
                "Noto Serif",
            ]);
            let mono = pick(&[
                "Consolas",
                "Menlo",
                "Courier New",
                "DejaVu Sans Mono",
                "Liberation Mono",
                "Noto Sans Mono",
            ]);
            if let Some(f) = sans {
                db.set_sans_serif_family(f);
            }
            if let Some(f) = serif {
                db.set_serif_family(f);
            }
            if let Some(f) = mono {
                db.set_monospace_family(f);
            }
            Arc::new(db)
        })
        .clone()
}

fn render_svg(req: &Request) -> Result<Rgba, String> {
    let bytes = std::fs::read(&req.bytes_path).map_err(|error| error.to_string())?;
    let options = Options {
        fontdb: fonts(),
        font_family: "sans-serif".to_string(),
        ..Options::default()
    };
    let tree = Tree::from_data(&bytes, &options).map_err(|error| error.to_string())?;
    let size = tree.size();
    // Fit box both ways: tiny icons scale up to max_px, big drawings scale down.
    let scale = if req.max_px > 0 {
        req.max_px as f32 / size.width().max(size.height())
    } else {
        1.0
    };
    let w = (size.width() * scale).round().max(1.0) as u32;
    let h = (size.height() * scale).round().max(1.0) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).ok_or("invalid SVG dimensions")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(Rgba {
        w,
        h,
        data: pixmap.take(),
    })
}
