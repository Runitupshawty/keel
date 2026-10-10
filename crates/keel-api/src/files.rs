//! Reading files through the host, for clients that cannot reach them (the web client):
//! byte ranges, previews rendered exactly as the desktop app renders them, media
//! thumbnails from the sidecar store, and one-time download links (`/file/<token>` on
//! keel-daemon's `--web` address).

use crate::error::{ApiError, Result};
use crate::ops::{record_at, vpath};
use crate::types::*;
use crate::Ctx;
use base64::Engine;
use keel_core::{SidecarKey, SidecarKind, SourceId};
use keel_vfs::{Entry, Kind, Provider, VPath};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Most bytes one `read` returns.
pub const READ_MAX: u64 = 4 << 20;
/// Largest side of a rendered image.
pub const RENDER_MAX_PX: u32 = 2048;
/// Most text (bytes) one `preview.render` returns.
const TEXT_MAX: usize = 1 << 20;
/// How long a `file.get` link works (once).
pub const LINK_TTL: Duration = Duration::from_secs(60);
/// Furthest a `read` skips into a file whose provider cannot read a range (each call
/// re-reads from the start up to its offset).
pub const SKIP_MAX: u64 = 64 << 20;

pub(crate) fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn io(e: std::io::Error) -> ApiError {
    match e.kind() {
        std::io::ErrorKind::NotFound => ApiError::not_found(e.to_string()),
        _ => ApiError::failed(e.to_string()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A library path's real path (where its source is); other paths as they are.
pub(crate) fn real(ctx: &Ctx, p: &VPath) -> Result<VPath> {
    let Some((id, rel)) = keel_vfs::library::split(p) else {
        return Ok(p.clone());
    };
    let src = ctx
        .lib
        .source(&SourceId(id.to_owned()))
        .ok_or_else(|| ApiError::not_found(format!("no source {id}")))?;
    Ok(src.absolute(rel))
}

/// What the read operations must not reach: anything in Keel's configuration folder (the
/// daemon token, keys) and, on Windows, UNC and device paths (`\\server\share`,
/// `\\?\UNC\...`, `\\.\...`) outside the library's sources (opening one sends the
/// user's credentials to that server).
pub(crate) fn readable(ctx: &Ctx, p: &VPath) -> Result<()> {
    // An archive path is read through the archive file that holds it.
    let mut outer = p.clone();
    while let Some((o, _)) = outer.split_archive() {
        outer = o;
    }
    let Some(local) = outer.to_local_path() else {
        return Ok(());
    };
    let raw = local.to_string_lossy();
    if cfg!(windows)
        && (raw.starts_with(r"\\") || raw.starts_with("//"))
        && ctx.lib.source_for(&outer).is_none()
    {
        return Err(ApiError::failed(format!(
            "{}: network and device paths are read only inside library sources",
            p.display()
        )));
    }
    if let Some(cfg) = &ctx.config_dir {
        if inside(&local, cfg) {
            return Err(ApiError::failed(format!(
                "{}: Keel's configuration folder is not readable through the API",
                p.display()
            )));
        }
    }
    Ok(())
}

/// `p` is `dir` or inside it, after resolving links, `..` and (Windows, macOS) case.
pub(crate) fn inside(p: &std::path::Path, dir: &std::path::Path) -> bool {
    let resolve = |p: &std::path::Path| -> Option<std::path::PathBuf> {
        if let Ok(c) = std::fs::canonicalize(p) {
            return Some(c);
        }
        // Not there (yet): its folder resolved, plus its name.
        match (p.parent(), p.file_name()) {
            (Some(parent), Some(name)) => std::fs::canonicalize(parent).ok().map(|c| c.join(name)),
            _ => None,
        }
        .or_else(|| std::path::absolute(p).ok())
    };
    let (Some(p), Some(dir)) = (resolve(p), resolve(dir)) else {
        return false;
    };
    let fold = |p: std::path::PathBuf| -> std::path::PathBuf {
        if cfg!(any(windows, target_os = "macos")) {
            p.to_string_lossy().to_lowercase().into()
        } else {
            p
        }
    };
    fold(p).starts_with(fold(dir))
}

fn provider(ctx: &Ctx, p: &VPath) -> Result<Arc<dyn Provider>> {
    ctx.router
        .provider_for(p)
        .ok_or_else(|| ApiError::not_found(format!("nothing serves {}", p.display())))
}

/// The real path, its provider and its entry (a file, not a folder).
fn file(ctx: &Ctx, path: &str) -> Result<(VPath, Arc<dyn Provider>, Entry)> {
    let p = real(ctx, &vpath(path)?)?;
    readable(ctx, &p)?;
    let prov = provider(ctx, &p)?;
    let entry = prov
        .stat(&p)
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    if entry.kind == Kind::Dir {
        return Err(ApiError::invalid_params(format!("{path} is a folder")));
    }
    Ok((p, prov, entry))
}

fn ns(t: Option<SystemTime>) -> i64 {
    t.map_or(0, keel_core::unix_ns)
}

/// The indexed content id of `path` while the file is as `now` says (None: as indexed).
fn content_id(ctx: &Ctx, path: &str, now: Option<(i64, u64)>) -> Option<[u8; 32]> {
    let h = record_at(ctx, path).ok()?;
    ctx.lib.content_id(&h.record, now).ok().flatten()
}

pub(crate) fn read(ctx: &Ctx, p: ReadParams) -> Result<Chunk> {
    let len = p.len.unwrap_or(READ_MAX).min(READ_MAX);
    let (path, prov, _) = file(ctx, &p.path)?;
    let mut buf = Vec::new();
    match path.to_local_path() {
        Some(local) => {
            let mut f = std::fs::File::open(local).map_err(io)?;
            f.seek(SeekFrom::Start(p.offset)).map_err(io)?;
            f.take(len + 1).read_to_end(&mut buf).map_err(io)?;
        }
        // A range from the provider (SFTP, cloud, devices); else read up to the offset,
        // which is capped.
        None => match prov.read_range(&path, p.offset, len + 1)? {
            Some(r) => {
                r.take(len + 1).read_to_end(&mut buf).map_err(io)?;
            }
            None if p.offset > SKIP_MAX => {
                return Err(ApiError::invalid_params(format!(
                    "{} cannot be read from an offset past {} MiB",
                    path.display(),
                    SKIP_MAX >> 20
                )))
            }
            None => {
                let mut r = prov.read(&path)?;
                std::io::copy(&mut (&mut r).take(p.offset), &mut std::io::sink()).map_err(io)?;
                r.take(len + 1).read_to_end(&mut buf).map_err(io)?;
            }
        },
    }
    let eof = buf.len() as u64 <= len;
    buf.truncate(len as usize);
    Ok(Chunk {
        offset: p.offset,
        data: b64(&buf),
        eof,
    })
}

fn none(message: String) -> Rendered {
    Rendered {
        kind: RenderKind::None,
        text: None,
        png: None,
        width: None,
        height: None,
        pages: None,
        truncated: false,
        message: Some(message),
        content_id: None,
    }
}

fn text(mut s: String, truncated: bool) -> Rendered {
    let cut = s.len() > TEXT_MAX;
    if cut {
        let mut end = TEXT_MAX;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    Rendered {
        kind: RenderKind::Text,
        text: Some(s),
        truncated: truncated || cut,
        message: None,
        ..none(String::new())
    }
}

fn image(rgba: keel_preview::Rgba, max_px: u32, pages: Option<u32>) -> Result<Rendered> {
    let mut img = image::RgbaImage::from_raw(rgba.w, rgba.h, rgba.data)
        .ok_or_else(|| ApiError::failed("the previewer returned a malformed image"))?;
    if img.width() > max_px || img.height() > max_px {
        img = image::DynamicImage::ImageRgba8(img)
            .resize(max_px, max_px, image::imageops::FilterType::Triangle)
            .to_rgba8();
    }
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| ApiError::failed(e.to_string()))?;
    Ok(Rendered {
        kind: RenderKind::Image,
        png: Some(b64(png.get_ref())),
        width: Some(img.width()),
        height: Some(img.height()),
        pages,
        message: None,
        ..none(String::new())
    })
}

fn hex_dump(head: &[u8], size: u64) -> String {
    let mut out = String::new();
    for (i, row) in head.chunks(16).enumerate() {
        let bytes: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = row
            .iter()
            .map(|&b| if b.is_ascii_graphic() { b as char } else { '.' })
            .collect();
        out.push_str(&format!(
            "{:08x}  {:<47}  {ascii}\n",
            i * 16,
            bytes.join(" ")
        ));
    }
    if size > head.len() as u64 {
        out.push_str(&format!("... {size} bytes in all\n"));
    }
    out
}

pub(crate) fn render(ctx: &Ctx, p: RenderParams) -> Result<Rendered> {
    use keel_preview::{DocBlock, Preview};
    let max_px = p.max_px.unwrap_or(1024).clamp(64, RENDER_MAX_PX);
    let (path, prov, entry) = file(ctx, &p.path)?;
    let cas = content_id(ctx, &p.path, Some((ns(entry.modified), entry.size)));
    if entry.size > keel_preview::MAX_PREVIEW_BYTES && !keel_preview::streams(&entry.ext) {
        return Ok(none(format!("too large to preview ({} bytes)", entry.size)));
    }
    let bytes_path = match path.to_local_path() {
        Some(local) => local,
        None => prov.local_copy(&path)?,
    };
    let preview = keel_preview::preview(&keel_preview::Request {
        entry,
        bytes_path,
        page: p.page,
        max_px,
        fit_width: false,
    });
    let mut out = match preview {
        Preview::Text {
            lines, truncated, ..
        } => {
            let lines: Vec<String> = lines
                .into_iter()
                .map(|spans| spans.into_iter().map(|(_, s)| s).collect())
                .collect();
            text(lines.join("\n"), truncated)
        }
        Preview::Markdown(s) => text(s, false),
        Preview::Table {
            headers,
            rows,
            truncated,
        } => {
            let rows: Vec<String> = std::iter::once(headers)
                .chain(rows)
                .map(|r| r.join("\t"))
                .collect();
            text(rows.join("\n"), truncated)
        }
        Preview::Doc { blocks } => {
            let mut s = String::new();
            for b in blocks {
                match b {
                    DocBlock::Heading(_, t) | DocBlock::Para(t) => s.push_str(&t),
                    DocBlock::Table(rows) => {
                        let rows: Vec<String> = rows.iter().map(|r| r.join("\t")).collect();
                        s.push_str(&rows.join("\n"));
                    }
                    DocBlock::Image(_) => s.push_str("[image]"),
                }
                s.push_str("\n\n");
            }
            text(s, false)
        }
        Preview::Hex { head, size } => text(hex_dump(&head, size), false),
        Preview::Image(rgba) => image(rgba, max_px, None)?,
        Preview::Pdf {
            pages, image: rgba, ..
        } => image(rgba, max_px, Some(pages))?,
        Preview::Video { thumb, .. } => image(thumb, max_px, None)?,
        Preview::TooLarge(n) => none(format!("too large to preview ({n} bytes)")),
        Preview::Unsupported => none("no preview for this kind of file".into()),
        Preview::Missing(why) => none(why.into()),
        Preview::Error(e) => return Err(ApiError::failed(e)),
    };
    out.content_id = cas.map(|c| hex(&c));
    Ok(out)
}

pub(crate) fn thumb(ctx: &Ctx, p: ThumbParams) -> Result<Thumb> {
    let kind = match p.size {
        ThumbSize::Thumb256 => SidecarKind::Thumb256,
        ThumbSize::Thumb1024 => SidecarKind::Thumb1024,
    };
    let path = real(ctx, &vpath(&p.path)?)?;
    readable(ctx, &path)?;
    let local = path.to_local_path().ok_or_else(|| {
        ApiError::invalid_params("thumbnails are made for local files and library records")
    })?;
    // The sidecar job's key: the source root joined with the record path.
    let keyed = match ctx.lib.source_for(&path) {
        Some((src, rel)) => src
            .def
            .root
            .to_local_path()
            .map_or_else(|| local.clone(), |root| root.join(rel)),
        None => local.clone(),
    };
    let store = ctx.lib.sidecars()?;
    let key = match std::fs::metadata(&local) {
        Ok(m) => {
            let now = (ns(m.modified().ok()), m.len());
            let cas = content_id(ctx, &p.path, Some(now));
            SidecarKey::local(&keyed, now.0, now.1, cas)
        }
        // Offline: only a sidecar made earlier, found by content id.
        Err(_) => {
            let cas = content_id(ctx, &p.path, None)
                .ok_or_else(|| ApiError::not_found(format!("{} is not reachable", p.path)))?;
            SidecarKey::local(&keyed, 0, 0, Some(cas))
        }
    };
    let _pin = store.pin(&key);
    let made = match store.get(&key, kind) {
        Some(made) => made,
        None if local.is_file() => store.ensure(&key, kind, &local)?,
        None => return Err(ApiError::not_found(format!("no thumbnail of {}", p.path))),
    };
    let data = std::fs::read(made).map_err(io)?;
    Ok(Thumb {
        mime: "image/webp".into(),
        data: b64(&data),
        content_id: key.cas_id.map(|c| hex(&c)),
    })
}

/// One-time download links made by `file.get`, taken by the daemon's `/file/<token>`.
#[derive(Default)]
pub struct Downloads(Mutex<HashMap<String, (VPath, Instant)>>);

impl Downloads {
    fn insert(&self, p: VPath) -> Result<String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| ApiError::failed(e.to_string()))?;
        let token = hex(&bytes);
        let mut map = self.0.lock();
        map.retain(|_, (_, at)| at.elapsed() < LINK_TTL);
        map.insert(token.clone(), (p, Instant::now()));
        Ok(token)
    }

    /// The file behind `token`, once; None when unknown, used or expired.
    pub fn take(&self, token: &str) -> Option<VPath> {
        let (p, at) = self.0.lock().remove(token)?;
        (at.elapsed() < LINK_TTL).then_some(p)
    }
}

/// Opens what a `/file/<token>` link names (the link is used up): name, size, contents.
pub fn open_link(ctx: &Ctx, token: &str) -> Result<(String, u64, Box<dyn Read + Send>)> {
    let p = ctx
        .downloads
        .take(token)
        .ok_or_else(|| ApiError::not_found("unknown, used or expired link"))?;
    let prov = provider(ctx, &p)?;
    let entry = prov
        .stat(&p)
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    Ok((entry.name, entry.size, prov.read(&p)?))
}

pub(crate) fn file_get(ctx: &Ctx, p: PathParams) -> Result<FileLink> {
    let (path, _, entry) = file(ctx, &p.path)?;
    let token = ctx.downloads.insert(path)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Ok(FileLink {
        url: format!("/file/{token}"),
        name: entry.name,
        size: entry.size,
        expires_at: (now + LINK_TTL).as_secs() as i64,
    })
}
