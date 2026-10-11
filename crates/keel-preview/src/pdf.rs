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
        return Preview::Missing(missing());
    };
    match render_inner(req, pdfium) {
        Ok(preview) => preview,
        Err(error) => Preview::Error(error),
    }
}

/// Where pdfium belongs: next to the program that runs (keel, or keel-daemon for the web
/// client, whose PDFs used to point at keel.exe).
fn missing() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        let exe = std::env::current_exe().ok();
        let name = exe.as_deref().and_then(Path::file_name);
        let name = name.map_or("keel".into(), |n| n.to_string_lossy());
        format!("PDF previews need pdfium next to {name} (run scripts/fetch-deps)")
    })
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
    // Fit box: the page's longer side lands on max_px; panel previews fit the width.
    let max_px = req.max_px.max(1) as f32;
    let side = if req.fit_width {
        page.width().value
    } else {
        page.width().value.max(page.height().value)
    };
    let bitmap = page
        .render_with_config(&PdfRenderConfig::new().scale_page_by_factor(max_px / side.max(1.0)))
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

/// A PDF's text, page by page, until the sink is full or out of time (Task 62). Without
/// pdfium the PDF has no reader here.
pub(crate) fn text(path: &Path, out: &mut crate::extract::Sink) -> Result<(), crate::TextError> {
    let Some(pdfium) = PDFIUM.get().and_then(Option::as_ref) else {
        return Err(crate::TextError::Kind);
    };
    // A fixed reason: pdfium's error code, never a message about the file.
    let failed = |error: PdfiumError| {
        crate::TextError::Failed(match error {
            PdfiumError::PdfiumLibraryInternalError(code) => format!("pdfium error {code:?}"),
            _ => "not a valid PDF".into(),
        })
    };
    let document = pdfium.load_pdf_from_file(path, None).map_err(failed)?;
    for page in document.pages().iter() {
        let text = page.text().map_err(failed)?;
        if !out.line(&text.all()) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn missing_pdfium_names_the_running_program() {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy();
        assert!(super::missing().contains(&*name), "{}", super::missing());
    }
}
