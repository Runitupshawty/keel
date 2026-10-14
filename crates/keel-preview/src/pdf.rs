use crate::{Preview, Request, Rgba};
use pdfium_render::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Duration;

static PDFIUM: OnceLock<Option<Pdfium>> = OnceLock::new();
/// Held for every use of `PDFIUM`: pdfium is not thread-safe, and pdfium-render's own
/// `thread_safe` lock belongs to whoever made the `Pdfium`, so it guards nothing between the
/// threads sharing this one (text readers, abandoned ones included, and previews).
static LOCK: Mutex<()> = Mutex::new(());
/// The program `isolate_pdf_text` reads PDFs with (a RwLock so tests can change it).
static CHILD: RwLock<Option<PathBuf>> = RwLock::new(None);
/// The child's own argument for `pdf_text_child`.
const CHILD_ARG: &str = "--pdf-text-child";
/// How long past the text cap a child may run before it is killed.
const CHILD_GRACE: Duration = Duration::from_secs(10);

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

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
    let _pdfium = lock();
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
    let _pdfium = lock();
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

/// From now on `extract_text` reads PDFs in a child process, `program --pdf-text-child
/// <path> <max_chars> <time_ms>`, whose `main` calls `pdf_text_child` first: a pdfium crash
/// or hang then ends only the child (`TextError::Failed`), never the window or the daemon.
pub fn isolate_pdf_text(program: PathBuf) {
    *CHILD.write().unwrap_or_else(|e| e.into_inner()) = Some(program);
}

/// The child's side of `isolate_pdf_text`: when this process was started as
/// `--pdf-text-child ...`, binds pdfium beside the executable, reads the PDF, writes the
/// result to stdout and exits; otherwise returns at once.
pub fn pdf_text_child() {
    use std::io::Write;
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(CHILD_ARG.as_ref()) {
        return;
    }
    let (Some(path), Some(max_chars), Some(time_ms)) = (
        args.next().map(PathBuf::from),
        args.next().and_then(|a| a.to_str()?.parse().ok()),
        args.next().and_then(|a| a.to_str()?.parse().ok()),
    ) else {
        std::process::exit(64);
    };
    if let Some(dir) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
    {
        init(dir);
    }
    let cap = keel_core::TextCaps {
        max_chars,
        time: Duration::from_millis(time_ms),
        ..keel_core::TEXT_CAPS
    };
    // Exit codes: 0 the text (after a truncated flag), 2 failed (the reason), 3 no reader,
    // 4 too large (the size).
    let (code, out) = match crate::extract::extract_text(&path, "pdf", &cap) {
        Ok(e) => (0, format!("{}{}", u8::from(e.truncated), e.text)),
        Err(crate::TextError::Failed(why)) => (2, why),
        Err(crate::TextError::Kind) => (3, String::new()),
        Err(crate::TextError::TooLarge(n)) => (4, n.to_string()),
    };
    let mut stdout = std::io::stdout().lock();
    let written = stdout
        .write_all(out.as_bytes())
        .and_then(|()| stdout.flush());
    std::process::exit(if written.is_ok() { code } else { 1 });
}

/// A PDF's text read by the `isolate_pdf_text` child; None when there is none (read here).
pub(crate) fn text_in_child(
    path: &Path,
    cap: &keel_core::TextCaps,
) -> Option<Result<crate::Extracted, crate::TextError>> {
    use crate::TextError::{Failed, Kind, TooLarge};
    let program = CHILD.read().unwrap_or_else(|e| e.into_inner()).clone()?;
    let mut command = Command::new(program);
    command
        .arg(CHILD_ARG)
        .arg(path)
        .arg(cap.max_chars.to_string())
        .arg(cap.time.as_millis().to_string());
    let crashed = || Failed("the PDF reader crashed".into());
    Some(
        match crate::video::run_until(command, cap.time + CHILD_GRACE) {
            Err(_) => Err(Failed("the PDF reader did not start".into())),
            Ok(None) => Err(Failed("timed out".into())),
            Ok(Some((status, out, _))) => match (status.code(), out.split_first()) {
                (Some(0), Some((flag, text))) => Ok(crate::Extracted {
                    text: String::from_utf8_lossy(text).into_owned(),
                    truncated: *flag == b'1',
                }),
                (Some(2), _) => Err(Failed(String::from_utf8_lossy(&out).into_owned())),
                (Some(3), _) => Err(Kind),
                (Some(4), _) => Err(TooLarge(String::from_utf8_lossy(&out).parse().unwrap_or(0))),
                // A signal (the crash this guards against), or anything the child never says.
                _ => Err(crashed()),
            },
        },
    )
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
