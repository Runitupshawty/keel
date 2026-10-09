use crate::{Preview, Request, Rgba};
use pdfium_render::prelude::*;
use std::path::Path;
use std::sync::OnceLock;

static PDFIUM: OnceLock<Option<Pdfium>> = OnceLock::new();

pub(crate) fn accepts(ext: &str) -> bool {
    ext == "pdf"
}

pub(crate) fn init(dll_dir: &Path) {
    // pdfium.dll / libpdfium.dylib / libpdfium.so; a failed bind leaves PDFs `Missing`.
    PDFIUM.get_or_init(|| {
        Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(dll_dir))
            .ok()
            .map(Pdfium::new)
    });
}

pub(crate) fn render(req: &Request) -> Preview {
    let Some(pdfium) = PDFIUM.get().and_then(Option::as_ref) else {
        return Preview::Missing(if cfg!(windows) {
            "PDF previews need pdfium next to keel.exe (run scripts/fetch-deps)"
        } else {
            "PDF previews need pdfium next to keel (run scripts/fetch-deps)"
        });
    };
    match render_inner(req, pdfium) {
        Ok(preview) => preview,
        Err(error) => Preview::Error(error),
    }
}

fn render_inner(req: &Request, pdfium: &Pdfium) -> Result<Preview, String> {
    let document = pdfium
        .load_pdf_from_file(&req.bytes_path, None)
        .map_err(|error| error.to_string())?;
    let pages = document.pages().len() as u32;
    if req.page >= pages {
        return Err(format!("page {} is out of range", req.page));
    }
    let page = document
        .pages()
        .get(req.page as u16)
        .map_err(|error| error.to_string())?;
    // Fit box: the page's longer side lands on max_px.
    let max_px = req.max_px.max(1) as f32;
    let longest = page.width().value.max(page.height().value).max(1.0);
    let bitmap = page
        .render_with_config(&PdfRenderConfig::new().scale_page_by_factor(max_px / longest))
        .map_err(|error| error.to_string())?;
    let image = bitmap.as_image().to_rgba8();
    Ok(Preview::Pdf {
        pages,
        page: req.page,
        image: Rgba {
            w: image.width(),
            h: image.height(),
            data: image.into_raw(),
        },
    })
}
