use crate::{Preview, Request, Rgba};
use image::{DynamicImage, GenericImageView};

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg"];

pub(crate) fn accepts(ext: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&ext)
}

pub(crate) fn render(req: &Request) -> Preview {
    let result = if req.entry.ext.eq_ignore_ascii_case("svg") {
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

fn render_svg(req: &Request) -> Result<Rgba, String> {
    let bytes = std::fs::read(&req.bytes_path).map_err(|error| error.to_string())?;
    let tree = resvg::usvg::Tree::from_data(&bytes, &resvg::usvg::Options::default())
        .map_err(|error| error.to_string())?;
    let size = tree.size();
    let scale = if req.max_px > 0 {
        (req.max_px as f32 / size.width().max(size.height())).min(1.0)
    } else {
        1.0
    };
    let w = (size.width() * scale).ceil().max(1.0) as u32;
    let h = (size.height() * scale).ceil().max(1.0) as u32;
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
