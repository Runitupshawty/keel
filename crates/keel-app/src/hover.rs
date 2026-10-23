//! Hover previews: resting the pointer on a file row or tile for `Settings::hover_preview_ms`
//! shows a large floating preview (an image, a PDF page, a video frame, a cover, a font
//! sample or the start of a text file), from the preview panel's worker and cache. Moving
//! off the row, scrolling, a key or a click hides it until the pointer rests on another file.

use crate::preview_panel::{PreviewKey, PreviewPanel};
use egui::{Context, Id, TextureHandle};
use keel_preview::Preview;
use keel_vfs::{Entry, Kind, VPath};
use std::time::{Duration, Instant};

/// The share of the window a preview may take, each way.
pub const MAX_SHARE: f32 = 0.4;
/// Lines of a text file shown.
const TEXT_LINES: usize = 30;
const STASH: &str = "keel-hover-entry";

/// Called by the views for the row or tile under the pointer (this frame).
pub fn stash(ctx: &Context, entry: &Entry) {
    ctx.data_mut(|d| d.insert_temp(Id::new(STASH), Some(entry.clone())));
}

#[derive(Default)]
pub struct Hover {
    /// The file under the pointer and since when.
    at: Option<(VPath, Instant)>,
    /// A key, a scroll or a click hid it; until the pointer rests on another file.
    dismissed: bool,
    /// The last preview asked for (asked once).
    asked: Option<PreviewKey>,
    tex: Option<(PreviewKey, TextureHandle)>,
    /// Shown this frame (tests).
    pub shown: Option<VPath>,
}

impl Hover {
    /// Once per frame after the panes drew: takes the stashed row and shows its preview
    /// once the pointer rested `delay` on it.
    pub fn show(&mut self, ctx: &Context, panel: &PreviewPanel, on: bool, delay: Duration) {
        let entry = ctx.data_mut(|d| d.remove_temp::<Option<Entry>>(Id::new(STASH)).flatten());
        self.shown = None;
        let Some(e) = entry.filter(|e| on && e.kind != Kind::Dir && !e.encrypted) else {
            self.at = None;
            // Clear the asked-once guard too, so the next landing re-requests.
            self.asked = None;
            return;
        };
        if self.at.as_ref().map(|(p, _)| p) != Some(&e.path) {
            self.at = Some((e.path.clone(), Instant::now()));
            self.dismissed = false;
            // Fresh file: a preview that left the shared cache is re-requested.
            self.asked = None;
        }
        let interrupted = ctx.input(|i| {
            i.pointer.any_down()
                || i.smooth_scroll_delta != egui::Vec2::ZERO
                || i.events
                    .iter()
                    .any(|ev| matches!(ev, egui::Event::Key { .. }))
        });
        self.dismissed |= interrupted;
        let waited = self
            .at
            .as_ref()
            .map_or(Duration::ZERO, |(_, t)| t.elapsed());
        if self.dismissed {
            return;
        }
        if waited < delay {
            ctx.request_repaint_after(delay - waited);
            return;
        }
        let screen = ctx.screen_rect();
        let max = screen.size() * MAX_SHARE;
        let px = (max.x * ctx.pixels_per_point()).round() as u32;
        let key = PreviewKey::of(&e, 0, crate::preview_panel::width_bucket(&e, px));
        let Some(preview) = panel.cached(&key) else {
            if self.asked.as_ref() != Some(&key) && panel.request(&key, &e, px) {
                self.asked = Some(key);
            }
            return;
        };
        let image = match &preview {
            Preview::Image(r) | Preview::Pdf { image: r, .. } | Preview::Video { thumb: r, .. } => {
                Some(r)
            }
            Preview::Audio { cover: Some(r), .. } | Preview::Font { sample: r, .. } => Some(r),
            _ => None,
        };
        let text = match &preview {
            Preview::Text { lines, .. } => Some(
                lines
                    .iter()
                    .take(TEXT_LINES)
                    .map(|l| l.iter().map(|(_, s)| s.as_str()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Preview::Markdown(s) => Some(s.lines().take(TEXT_LINES).collect::<Vec<_>>().join("\n")),
            _ => None,
        };
        if image.is_none() && text.is_none() {
            return;
        }
        let size = match image {
            Some(r) => {
                if self.tex.as_ref().map(|(k, _)| k) != Some(&key) {
                    let name = format!("hover:{}", e.path.display());
                    self.tex = crate::preview_panel::texture(ctx, &name, r).map(|t| (key, t));
                }
                let Some((_, tex)) = &self.tex else { return };
                let s = tex.size_vec2() / ctx.pixels_per_point();
                s * (max.x / s.x).min(max.y / s.y).min(1.0)
            }
            None => egui::vec2(max.x.min(560.0), max.y),
        };
        let Some(at) = ctx.pointer_hover_pos() else {
            return;
        };
        // Below and right of the pointer, flipped to the other side where it would leave
        // the window (the frame adds about 16 points).
        let gap = 18.0;
        let fit = |pos: f32, len: f32, lo: f32, hi: f32| {
            let after = pos + gap;
            if after + len + 16.0 <= hi {
                after
            } else {
                (pos - gap - len - 16.0).max(lo)
            }
        };
        let pos = egui::pos2(
            fit(at.x, size.x, screen.left(), screen.right()),
            fit(at.y, size.y, screen.top(), screen.bottom()),
        );
        egui::Area::new(Id::new("keel-hover-preview"))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| match (&self.tex, text) {
                    (_, Some(text)) => {
                        ui.set_max_size(size);
                        ui.add(egui::Label::new(egui::RichText::new(text).monospace()).wrap());
                    }
                    (Some((_, tex)), None) => {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(size));
                    }
                    _ => {}
                });
            });
        self.shown = Some(e.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview_panel::PreviewPanel;
    use crate::tab::test_entry;

    /// The "asked once" guard must not survive the file leaving the preview
    /// cache: landing back on a file whose preview was evicted (or wiped by
    /// `PreviewPanel::reset` on crash-recovery / profile switch) re-requests it.
    #[test]
    fn relanding_on_a_missing_preview_rerequests_it() {
        let dir = VPath::local(std::env::temp_dir());
        let (jobs, rx) = crossbeam_channel::unbounded();
        let mut panel = PreviewPanel::new(jobs);
        let ctx = egui::Context::default();
        let a = test_entry(&dir, "a.png", Kind::File, 10);
        let mut hover = Hover::default();

        // Land on `a`: not cached, so it is requested once.
        stash(&ctx, &a);
        hover.show(&ctx, &panel, true, Duration::ZERO);
        assert!(rx.try_recv().is_ok(), "first landing requests the preview");
        while rx.try_recv().is_ok() {} // drain

        // The shared cache loses the preview (LRU eviction / reset on crash
        // recovery or profile switch), and the pointer moves off the row.
        panel.reset();
        hover.show(&ctx, &panel, true, Duration::ZERO);

        // Landing back on `a` must re-request the now-missing preview.
        stash(&ctx, &a);
        hover.show(&ctx, &panel, true, Duration::ZERO);
        assert!(
            rx.try_recv().is_ok(),
            "relanding on a file that left the cache re-requests its preview"
        );
    }

    /// `PreviewPanel::reset` wipes the shared cache; the pointer passing over a
    /// folder (leaving all files) must clear the asked-once guard as well, so
    /// landing back on the file re-requests its preview.
    #[test]
    fn relanding_after_reset_rerequests_the_preview() {
        let dir = VPath::local(std::env::temp_dir());
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut panel = PreviewPanel::new(tx);
        let ctx = egui::Context::default();
        let a = test_entry(&dir, "a.md", Kind::File, 10);
        let folder = test_entry(&dir, "sub", Kind::Dir, 0);
        let mut hover = Hover::default();

        // Land on `a`: not cached, so it is requested once.
        stash(&ctx, &a);
        hover.show(&ctx, &panel, true, Duration::ZERO);
        assert!(rx.try_recv().is_ok(), "first landing requests the preview");
        while rx.try_recv().is_ok() {} // drain

        // The worker answers (cached), then the panel is reset (crash recovery /
        // profile switch) and the cache is wiped.
        let px = (ctx.screen_rect().size().x * MAX_SHARE * ctx.pixels_per_point()).round() as u32;
        let key = PreviewKey::of(&a, 0, crate::preview_panel::width_bucket(&a, px));
        panel.insert(&ctx, key, Preview::Markdown("# hi\n".into()));
        panel.reset();

        // The pointer passes over a folder row: no preview here, but the
        // asked-once guard for `a` must not survive.
        stash(&ctx, &folder);
        hover.show(&ctx, &panel, true, Duration::ZERO);

        // Landing back on `a` must re-request the now-missing preview.
        stash(&ctx, &a);
        hover.show(&ctx, &panel, true, Duration::ZERO);
        assert!(
            rx.try_recv().is_ok(),
            "relanding after a reset re-requests the preview"
        );
    }
}
