//! Preview panel (right side, F3): follows the active pane's cursor,
//! renders on `worker::spawn_previewer` and keeps the last 64 previews.

use crate::view_details::{date_text, size_text};
use crate::worker::PreviewJob;
use crossbeam_channel::Sender;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, RichText, TextureHandle};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use egui_extras::{Column, TableBuilder};
use keel_preview::{DocBlock, Preview, Rgba};
use keel_vfs::{Entry, Kind, VPath};
use std::collections::HashMap;

pub const CACHE: usize = 64;
/// base16-ocean.dark background: the text previewer's span colours are made for it.
const CODE_BG: Color32 = Color32::from_rgb(0x2b, 0x30, 0x3b);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PreviewKey {
    pub path: VPath,
    pub mtime: u64,
    pub size: u64,
    pub page: u32,
}

impl PreviewKey {
    pub fn of(entry: &Entry, page: u32) -> Self {
        Self {
            path: entry.path.clone(),
            mtime: entry
                .modified
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs()),
            size: entry.size,
            page,
        }
    }
}

pub struct PreviewPanel {
    pub open: bool,
    pub key: Option<PreviewKey>,
    pub current: Option<Preview>,
    pub page: u32,
    pub tex: Option<TextureHandle>,
    pub md_cache: CommonMarkCache,
    /// Panel width in physical pixels (last frame): the render size of new requests.
    pub width_px: u32,
    /// Larger files show "too large" without being read (Settings, max preview size).
    pub max_bytes: u64,
    entry: Option<Entry>,
    doc_tex: Vec<TextureHandle>,
    cache: HashMap<PreviewKey, (Preview, u64)>,
    clock: u64,
    jobs: Sender<PreviewJob>,
}

impl PreviewPanel {
    pub fn new(jobs: Sender<PreviewJob>) -> Self {
        Self {
            open: false,
            key: None,
            current: None,
            page: 0,
            tex: None,
            md_cache: CommonMarkCache::default(),
            width_px: 480,
            max_bytes: keel_preview::MAX_PREVIEW_BYTES,
            entry: None,
            doc_tex: Vec::new(),
            cache: HashMap::new(),
            clock: 0,
            jobs,
        }
    }

    /// Shows `entry` (the active pane's cursor): from the cache, or asks the worker.
    pub fn follow(&mut self, ctx: &egui::Context, entry: Option<&Entry>) {
        let Some(e) = entry else {
            self.entry = None;
            self.key = None;
            self.set(ctx, None);
            return;
        };
        if self.entry.as_ref().map(|x| &x.path) != Some(&e.path) {
            self.page = 0;
        }
        let key = PreviewKey::of(e, self.page);
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = Some(key.clone());
        self.entry = Some(e.clone());
        self.set(ctx, None);
        if e.kind == Kind::Dir {
            return;
        }
        if e.size > self.max_bytes {
            self.set(ctx, Some(Preview::TooLarge(e.size)));
            return;
        }
        self.clock += 1;
        let cached = self.cache.get_mut(&key).map(|(p, used)| {
            *used = self.clock;
            p.clone()
        });
        match cached {
            Some(p) => self.set(ctx, Some(p)),
            None => {
                let _ = self.jobs.send((key, e.clone(), self.width_px.max(64)));
            }
        }
    }

    /// A worker answer: cached, and shown if it is still the one wanted.
    pub fn insert(&mut self, ctx: &egui::Context, key: PreviewKey, preview: Preview) {
        if self.key.as_ref() == Some(&key) {
            self.set(ctx, Some(preview.clone()));
        }
        self.clock += 1;
        self.cache.insert(key, (preview, self.clock));
        if self.cache.len() > CACHE {
            // ponytail: O(n) scan over 64 entries.
            if let Some(oldest) = self
                .cache
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            {
                self.cache.remove(&oldest);
            }
        }
    }

    fn set(&mut self, ctx: &egui::Context, preview: Option<Preview>) {
        let name = self
            .key
            .as_ref()
            .map(|k| k.path.display())
            .unwrap_or_default();
        let load = |n: usize, r: &Rgba| texture(ctx, &format!("preview:{name}:{n}"), r);
        self.tex = match &preview {
            Some(
                Preview::Image(r) | Preview::Pdf { image: r, .. } | Preview::Video { thumb: r, .. },
            ) => load(0, r),
            _ => None,
        };
        self.doc_tex = match &preview {
            Some(Preview::Doc { blocks }) => blocks
                .iter()
                .filter_map(|b| match b {
                    DocBlock::Image(r) => Some(r),
                    _ => None,
                })
                .enumerate()
                .filter_map(|(i, r)| load(i + 1, r))
                .collect(),
            _ => Vec::new(),
        };
        self.current = preview;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, muted: Color32) {
        let Some(entry) = self.entry.clone() else {
            centered(ui, "Select a file to preview", muted);
            return;
        };
        ui.add(egui::Label::new(RichText::new(entry.path.name()).strong()).truncate());
        let mut meta: Vec<String> = [size_text(&entry), date_text(&entry)]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(Preview::Text {
            language,
            lines,
            truncated,
        }) = &self.current
        {
            meta.push(format!(
                "{language} · {}{} lines",
                if *truncated { "first " } else { "" },
                lines.len()
            ));
        }
        ui.weak(meta.join("  ·  "));
        ui.separator();
        if entry.kind == Kind::Dir {
            centered(ui, "Folder", muted);
            return;
        }
        let Some(current) = &self.current else {
            ui.centered_and_justified(|ui| ui.spinner());
            return;
        };
        match current {
            Preview::Text { lines, .. } => text(ui, lines),
            Preview::Markdown(md) => {
                // Keyed by path + mtime + size: parsed once, only visible blocks laid out.
                CommonMarkViewer::new().show_scrollable(
                    ("md", &self.key),
                    ui,
                    &mut self.md_cache,
                    md,
                );
            }
            Preview::Image(_) => {
                if let Some(t) = &self.tex {
                    egui::ScrollArea::vertical().show(ui, |ui| fit(ui, t));
                }
            }
            Preview::Pdf { pages, page, .. } => {
                let (pages, page) = (*pages, *page);
                ui.horizontal(|ui| {
                    if ui.add_enabled(page > 0, egui::Button::new("⏴")).clicked() {
                        self.page = page - 1;
                    }
                    ui.label(format!("page {} / {pages}", page + 1));
                    if ui
                        .add_enabled(page + 1 < pages, egui::Button::new("⏵"))
                        .clicked()
                    {
                        self.page = page + 1;
                    }
                });
                if let Some(t) = &self.tex {
                    egui::ScrollArea::vertical().show(ui, |ui| fit(ui, t));
                }
            }
            Preview::Video {
                duration_s, meta, ..
            } => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if let Some(t) = &self.tex {
                        fit(ui, t);
                    }
                    let s = duration_s.max(0.0) as u64;
                    ui.label(format!("{}:{:02}", s / 60, s % 60));
                    ui.weak(meta);
                });
            }
            Preview::Table {
                headers,
                rows,
                truncated,
            } => table(ui, headers, rows, *truncated),
            Preview::Doc { blocks } => {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| doc(ui, blocks, &self.doc_tex));
            }
            Preview::Hex { head, size } => hex(ui, head, *size),
            Preview::TooLarge(n) => centered(
                ui,
                &format!(
                    "Too large to preview ({})",
                    humansize::format_size(*n, humansize::DECIMAL)
                ),
                muted,
            ),
            Preview::Unsupported => centered(ui, "No preview for this file", muted),
            Preview::Error(e) => centered(ui, &format!("Preview failed: {e}"), muted),
        }
    }
}

fn texture(ctx: &egui::Context, name: &str, r: &Rgba) -> Option<TextureHandle> {
    (r.w > 0 && r.h > 0 && r.data.len() == r.w as usize * r.h as usize * 4).then(|| {
        ctx.load_texture(
            name,
            egui::ColorImage::from_rgba_unmultiplied([r.w as usize, r.h as usize], &r.data),
            egui::TextureOptions::LINEAR,
        )
    })
}

fn centered(ui: &mut egui::Ui, text: &str, muted: Color32) {
    ui.centered_and_justified(|ui| ui.label(RichText::new(text).color(muted)));
}

/// The texture at its pixel size, shrunk to the panel width.
fn fit(ui: &mut egui::Ui, tex: &TextureHandle) {
    let mut size = tex.size_vec2() / ui.ctx().pixels_per_point();
    let avail = ui.available_width();
    if size.x > avail {
        size *= avail / size.x;
    }
    ui.add(egui::Image::new((tex.id(), size)));
}

fn mono_rows(ui: &mut egui::Ui, n: usize, mut row: impl FnMut(&mut egui::Ui, usize)) {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let h = ui.fonts(|f| f.row_height(&font));
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, h, n, |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in range {
                row(ui, i);
            }
        });
}

fn text(ui: &mut egui::Ui, lines: &[Vec<([u8; 4], String)>]) {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    egui::Frame::new()
        .fill(CODE_BG)
        .inner_margin(6.0)
        .show(ui, |ui| {
            mono_rows(ui, lines.len(), |ui, i| {
                let mut job = LayoutJob::default();
                for ([r, g, b, a], s) in &lines[i] {
                    let format = TextFormat {
                        font_id: font.clone(),
                        color: Color32::from_rgba_unmultiplied(*r, *g, *b, *a),
                        ..Default::default()
                    };
                    job.append(s.trim_end_matches(['\n', '\r']), 0.0, format);
                }
                if job.text.is_empty() {
                    job.append(" ", 0.0, TextFormat::simple(font.clone(), Color32::GRAY));
                }
                ui.add(egui::Label::new(job).extend());
            });
        });
}

fn hex(ui: &mut egui::Ui, head: &[u8], size: u64) {
    ui.weak(format!(
        "{} bytes, first {} shown",
        size,
        head.len().min(size as usize)
    ));
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    mono_rows(ui, head.len().div_ceil(16), |ui, i| {
        let chunk = &head[i * 16..(i * 16 + 16).min(head.len())];
        let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        let line = format!("{:08x}  {hex:<48} {ascii}", i * 16);
        ui.add(egui::Label::new(RichText::new(line).font(font.clone())).extend());
    });
}

fn table(ui: &mut egui::Ui, headers: &[String], rows: &[Vec<String>], truncated: bool) {
    if truncated {
        ui.weak(format!("Showing the first {} rows", rows.len()));
    }
    let cols = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if cols == 0 {
        ui.weak("Empty table");
        return;
    }
    let cell = |ui: &mut egui::Ui, text: &str| {
        ui.add(egui::Label::new(text).truncate());
    };
    egui::ScrollArea::horizontal().show(ui, |ui| {
        TableBuilder::new(ui)
            .id_salt("preview-table")
            .striped(true)
            .resizable(true)
            .max_scroll_height(f32::INFINITY)
            .columns(Column::initial(110.0).at_least(40.0).clip(true), cols)
            .header(20.0, |mut h| {
                for c in 0..cols {
                    h.col(|ui| {
                        ui.strong(headers.get(c).map_or("", String::as_str));
                    });
                }
            })
            .body(|body| {
                body.rows(18.0, rows.len(), |mut row| {
                    let r = &rows[row.index()];
                    for c in 0..cols {
                        row.col(|ui| cell(ui, r.get(c).map_or("", String::as_str)));
                    }
                });
            });
    });
}

fn doc(ui: &mut egui::Ui, blocks: &[DocBlock], images: &[TextureHandle]) {
    let mut img = images.iter();
    for (i, block) in blocks.iter().enumerate() {
        match block {
            DocBlock::Heading(level, s) => {
                let size = match level {
                    1 => 22.0,
                    2 => 18.0,
                    _ => 15.0,
                };
                ui.add_space(4.0);
                ui.label(RichText::new(s).strong().size(size));
            }
            DocBlock::Para(s) => {
                ui.label(s);
            }
            DocBlock::Table(rows) => {
                egui::Grid::new(("doc-table", i))
                    .striped(true)
                    .show(ui, |ui| {
                        for row in rows {
                            for cell in row {
                                ui.label(cell);
                            }
                            ui.end_row();
                        }
                    });
            }
            DocBlock::Image(_) => {
                if let Some(t) = img.next() {
                    fit(ui, t);
                }
            }
        }
        ui.add_space(4.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, Msg};
    use keel_vfs::Router;
    use std::sync::Arc;

    fn draw(state: &mut AppState) {
        let ctx = state.ctx.clone();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                state.preview.ui(ui, Color32::GRAY);
            });
        });
    }

    /// Review Focus 4: a failed preview renders as text, never panics.
    #[test]
    fn error_preview_renders_without_panic() {
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            dir.clone(),
        );
        let entry = crate::tab::test_entry(&dir, "corrupt.pdf", Kind::File, 9);
        state.preview.open = true;
        let ctx = state.ctx.clone();
        state.preview.follow(&ctx, Some(&entry));
        let key = state.preview.key.clone().unwrap();
        state.apply(Msg::Preview {
            key,
            preview: Preview::Error("previewer panicked: boom".into()),
        });
        assert!(matches!(state.preview.current, Some(Preview::Error(_))));
        draw(&mut state);

        // Every other variant draws too.
        for p in [
            Preview::Text {
                lines: vec![vec![([200, 100, 50, 255], "fn main() {}".into())], vec![]],
                language: "Rust".into(),
                truncated: false,
            },
            Preview::Markdown("# Title\n\n* item".into()),
            Preview::Image(Rgba {
                w: 2,
                h: 1,
                data: vec![255; 8],
            }),
            Preview::Pdf {
                pages: 3,
                page: 1,
                image: Rgba {
                    w: 1,
                    h: 1,
                    data: vec![0; 4],
                },
            },
            Preview::Table {
                headers: vec!["a".into(), "b".into()],
                rows: vec![vec!["1".into(), "2".into(), "3".into()]],
                truncated: true,
            },
            Preview::Doc {
                blocks: vec![
                    DocBlock::Heading(1, "H".into()),
                    DocBlock::Para("p".into()),
                    DocBlock::Table(vec![vec!["x".into()]]),
                ],
            },
            Preview::Video {
                thumb: Rgba {
                    w: 0,
                    h: 0,
                    data: vec![],
                },
                duration_s: 2.0,
                meta: "h264".into(),
            },
            Preview::Hex {
                head: (0..40).collect(),
                size: 40,
            },
            Preview::TooLarge(1 << 30),
            Preview::Unsupported,
        ] {
            state.preview.set(&ctx, Some(p));
            draw(&mut state);
        }
    }

    #[test]
    fn cache_keeps_the_newest_64_and_serves_hits() {
        let dir = VPath::local(std::env::temp_dir());
        let (jobs, rx) = crossbeam_channel::unbounded();
        let mut panel = PreviewPanel::new(jobs);
        let ctx = egui::Context::default();
        for i in 0..70 {
            let e = crate::tab::test_entry(&dir, &format!("{i}.txt"), Kind::File, i);
            panel.follow(&ctx, Some(&e));
            let (key, ..) = rx.try_recv().expect("cache miss asks the worker");
            panel.insert(&ctx, key, Preview::Unsupported);
        }
        assert_eq!(panel.cache.len(), CACHE);
        let e = crate::tab::test_entry(&dir, "69.txt", Kind::File, 69);
        panel.follow(&ctx, None);
        panel.follow(&ctx, Some(&e));
        assert!(rx.try_recv().is_err(), "served from the cache");
        assert!(matches!(panel.current, Some(Preview::Unsupported)));
    }
}
