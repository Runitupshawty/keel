//! Drag-out to the OS (Task 24): a row drag whose pointer leaves the window becomes an OS
//! drag (`keel_vfs::drag_out`, its own thread on Windows). When it ends, the source folder
//! is refreshed (the receiving app may have moved the files away) and egui is handed the
//! button release it never saw (the drag thread held the mouse). Dropping the files back
//! onto Keel itself does nothing (as in Explorer; it would only copy them onto themselves).

use crate::pane::DragPayload;
use crate::state::{AppState, Msg};
use egui::{DragAndDrop, Event, PointerButton, Pos2};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub struct DragOut {
    /// An OS drag runs.
    busy: Arc<AtomicBool>,
    /// Set by the drag thread when the OS drag is over.
    ended: Arc<AtomicBool>,
    /// Where the pointer left the window (for the synthetic release).
    left_at: Pos2,
}

impl DragOut {
    /// Once a frame, after the panes ran.
    pub fn check(&mut self, ctx: &egui::Context, s: &mut AppState) {
        if !keel_vfs::drag_out::SUPPORTED {
            return;
        }
        let Some(drag) = DragAndDrop::payload::<DragPayload>(ctx) else {
            return;
        };
        let left = ctx.input(|i| {
            let pos = (i.pointer.latest_pos())
                .or(i.pointer.interact_pos())
                .unwrap_or_default();
            let gone = i.events.iter().any(|e| matches!(e, Event::PointerGone));
            (i.pointer.primary_down() && (gone || !i.screen_rect().contains(pos))).then_some(pos)
        });
        let Some(at) = left else { return };
        DragAndDrop::clear_payload(ctx);
        let Some(paths) = drag
            .paths
            .iter()
            .map(keel_vfs::VPath::to_local_path)
            .collect::<Option<Vec<_>>>()
        else {
            s.toasts
                .error("Only local files can be dragged to other apps");
            return;
        };
        self.left_at = at;
        self.busy.store(true, Ordering::Release);
        let (tx, ctx, busy, ended, dir) = (
            s.tx.clone(),
            ctx.clone(),
            self.busy.clone(),
            self.ended.clone(),
            drag.dir.clone(),
        );
        keel_vfs::drag_out::start(paths, true, move |result| {
            ended.store(true, Ordering::Release);
            busy.store(false, Ordering::Release);
            let msg = match result {
                Ok(effect) => {
                    tracing::info!("drag out: {effect:?}");
                    Msg::Changed { dir }
                }
                Err(e) => Msg::Toast(format!("Drag to another app failed: {e:#}")),
            };
            crate::worker::send(&tx, &ctx, msg);
        });
    }

    /// From `eframe::App::raw_input_hook`: drops our own OS drag onto Keel, and after it
    /// releases the button egui still thinks is down. (The drop reaches winit before the
    /// drag thread ends, so the frame that sees it sees `busy` or `ended`.)
    pub fn input_hook(&self, raw: &mut egui::RawInput) {
        // `busy` first: the drag thread sets `ended` before it clears `busy`.
        let busy = self.busy.load(Ordering::Acquire);
        let ended = self.ended.swap(false, Ordering::AcqRel);
        if busy || ended {
            raw.dropped_files.clear();
            raw.hovered_files.clear();
        }
        if ended {
            raw.events.push(Event::PointerButton {
                pos: self.left_at,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: raw.modifiers,
            });
            raw.events.push(Event::PointerGone);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_ignores_own_drops_then_releases_the_button() {
        let out = DragOut::default();
        let with_drop = || egui::RawInput {
            dropped_files: vec![egui::DroppedFile::default()],
            ..Default::default()
        };
        // While the OS drag runs, our files dropped back onto Keel are ignored.
        out.busy.store(true, Ordering::Release);
        let mut raw = with_drop();
        out.input_hook(&mut raw);
        assert!(raw.dropped_files.is_empty() && raw.events.is_empty());
        // When it ends: one release, once.
        out.ended.store(true, Ordering::Release);
        out.busy.store(false, Ordering::Release);
        let mut raw = with_drop();
        out.input_hook(&mut raw);
        assert!(raw.dropped_files.is_empty());
        assert!(matches!(
            raw.events[0],
            Event::PointerButton { pressed: false, .. }
        ));
        // Later drops from other apps are untouched.
        let mut raw = with_drop();
        out.input_hook(&mut raw);
        assert_eq!((raw.dropped_files.len(), raw.events.len()), (1, 0));
    }
}
