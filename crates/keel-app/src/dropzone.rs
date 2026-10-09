//! Drop zone: a strip above the status bar holding a stash of paths across navigation.
//! Rows dragged onto it, or Ctrl+Shift+S, stash them; Paste here / Move here send them
//! to the active pane's folder through the usual transfer plan.

use crate::jobs::{self, Source};
use crate::keys::Action;
use crate::pane::DragPayload;
use crate::state::AppState;
use crossbeam_channel::{Receiver, Sender};
use keel_vfs::{Router, VPath};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often a shown strip checks that its items still exist.
const RECHECK: Duration = Duration::from_secs(3);
/// Chips drawn; the rest are counted.
const MAX_CHIPS: usize = 60;

pub struct DropZone {
    pub items: Vec<VPath>,
    /// The strip is shown (Ctrl+Shift+Z); it also shows while rows are dragged.
    pub open: bool,
    /// Items the last check did not find: greyed, skipped on paste.
    pub missing: HashSet<VPath>,
    checked: Option<Instant>,
    checking: bool,
    tx: Sender<HashSet<VPath>>,
    rx: Receiver<HashSet<VPath>>,
}

impl Default for DropZone {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            items: Vec::new(),
            open: false,
            missing: HashSet::new(),
            checked: None,
            checking: false,
            tx,
            rx,
        }
    }
}

impl DropZone {
    /// Adds `paths` not stashed yet; the number added.
    pub fn add(&mut self, paths: impl IntoIterator<Item = VPath>) -> usize {
        let before = self.items.len();
        for p in paths {
            if !self.items.contains(&p) {
                self.items.push(p);
            }
        }
        self.checked = None;
        self.items.len() - before
    }

    pub fn remove(&mut self, path: &VPath) {
        self.items.retain(|p| p != path);
        self.missing.remove(path);
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.missing.clear();
    }

    /// What Paste here / Move here transfers: the items not known to be missing.
    pub fn paste_paths(&self) -> Vec<VPath> {
        self.items
            .iter()
            .filter(|p| !self.missing.contains(*p))
            .cloned()
            .collect()
    }

    /// Takes check answers; starts a check (on a worker: `stat` may block) when the strip
    /// is shown and the last one is stale.
    fn tick(&mut self, shown: bool, router: &Arc<Router>, ctx: &egui::Context) {
        while let Ok(missing) = self.rx.try_recv() {
            self.missing = missing;
            self.checking = false;
        }
        if !shown || self.checking || self.items.is_empty() {
            return;
        }
        if self.checked.is_some_and(|at| at.elapsed() < RECHECK) {
            ctx.request_repaint_after(RECHECK);
            return;
        }
        self.checked = Some(Instant::now());
        let (items, router, tx, ctx) = (
            self.items.clone(),
            router.clone(),
            self.tx.clone(),
            ctx.clone(),
        );
        self.checking = crate::worker::spawn("keel-stash-check", move || {
            let exists = |p: &VPath| match p.to_local_path() {
                Some(local) => local.exists(),
                None => router.provider_for(p).is_some_and(|pr| pr.stat(p).is_ok()),
            };
            let missing = items.into_iter().filter(|p| !exists(p)).collect();
            let _ = tx.send(missing);
            ctx.request_repaint();
        });
    }
}

/// The drop zone actions, on pane `p`.
pub fn run(s: &mut AppState, p: usize, action: Action) {
    match action {
        Action::ToggleDropZone => s.dropzone.open = !s.dropzone.open,
        Action::StashSelection => {
            let paths: Vec<VPath> = s.tab(p).targets().iter().map(|e| e.path.clone()).collect();
            if paths.is_empty() {
                return s.toasts.info("Select something to stash");
            }
            let added = s.dropzone.add(paths);
            s.dropzone.open = true;
            s.toasts.info(format!("Stashed {}", jobs::items(added)));
        }
        Action::Stash(paths) => {
            s.dropzone.add(paths);
            s.dropzone.open = true;
        }
        Action::Unstash(path) => s.dropzone.remove(&path),
        Action::ClearStash => s.dropzone.clear(),
        Action::StashPaste { mv } => {
            let tab = s.tab(p);
            if tab.is_search() {
                return s
                    .toasts
                    .error("Open a folder first (search results have no folder)");
            }
            let dst = tab.dir.clone();
            if dst.split_archive().is_some() {
                return s.toasts.error(crate::state::READ_ONLY);
            }
            let paths = s.dropzone.paste_paths();
            let skipped = s.dropzone.items.len() - paths.len();
            if paths.is_empty() {
                return s
                    .toasts
                    .error("Nothing to paste: the stashed items are gone");
            }
            if skipped > 0 {
                s.toasts.info(format!(
                    "Skipped {} that no longer exist",
                    jobs::items(skipped)
                ));
            }
            if mv {
                // Moved items leave the stash (they are no longer where it points).
                for path in &paths {
                    s.dropzone.remove(path);
                }
            }
            let started = jobs::spawn_plan(
                Source::Paths(paths, mv),
                dst,
                s.router.clone(),
                s.tx.clone(),
                s.ctx.clone(),
            );
            if !started {
                s.toasts.error("Could not start the transfer");
            }
        }
        _ => {}
    }
}

/// The strip (above the status bar): shown when open or while rows are dragged.
/// Returns the clicked action.
pub fn panel(ctx: &egui::Context, s: &mut AppState) -> Option<Action> {
    let dragging = egui::DragAndDrop::has_payload_of_type::<DragPayload>(ctx);
    let zone = &mut s.dropzone;
    let shown = zone.open || dragging;
    zone.tick(shown, &s.router, ctx);
    let mut action = None;
    egui::TopBottomPanel::bottom("dropzone").show_animated(ctx, shown, |ui| {
        let frame = egui::Frame::new().inner_margin(egui::Margin::symmetric(4, 3));
        let (_, dropped) = ui.dnd_drop_zone::<DragPayload, _>(frame, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.strong("Drop zone");
                if zone.items.is_empty() {
                    ui.weak("Drag files here or press Ctrl+Shift+S to stash the selection");
                }
                for path in zone.items.iter().take(MAX_CHIPS) {
                    if let Some(a) = chip(ui, path, zone.missing.contains(path)) {
                        action = Some(a);
                    }
                }
                if zone.items.len() > MAX_CHIPS {
                    ui.weak(format!("+{} more", zone.items.len() - MAX_CHIPS));
                }
                ui.separator();
                let any = !zone.items.is_empty();
                let buttons = [
                    (
                        "Paste here",
                        "Copy into the active folder",
                        Action::StashPaste { mv: false },
                    ),
                    (
                        "Move here",
                        "Move into the active folder",
                        Action::StashPaste { mv: true },
                    ),
                    ("Clear", "Empty the drop zone", Action::ClearStash),
                ];
                for (text, tip, a) in buttons {
                    if ui
                        .add_enabled(any, egui::Button::new(text))
                        .on_hover_text(tip)
                        .clicked()
                    {
                        action = Some(a);
                    }
                }
                if ui
                    .add(egui::Button::new("⏷").frame(false))
                    .on_hover_text("Hide (Ctrl+Shift+Z)")
                    .clicked()
                {
                    action = Some(Action::ToggleDropZone);
                }
            });
        });
        if let Some(drag) = dropped {
            action = Some(Action::Stash(drag.paths.clone()));
        }
    });
    action
}

/// One stashed item: name (source folder on hover), greyed when missing, and ✕.
fn chip(ui: &mut egui::Ui, path: &VPath, missing: bool) -> Option<Action> {
    let mut action = None;
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let name = egui::RichText::new(path.name());
            let name = if missing {
                name.weak().strikethrough()
            } else {
                name
            };
            let folder = path.parent().map(|d| d.display()).unwrap_or_default();
            let tip = if missing {
                format!("{folder}\n(no longer exists: skipped on paste)")
            } else {
                folder
            };
            ui.label(name).on_hover_text(tip);
            if ui
                .add(egui::Button::new("✕").frame(false).small())
                .on_hover_text("Remove from the drop zone")
                .clicked()
            {
                action = Some(Action::Unstash(path.clone()));
            }
        });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> VPath {
        VPath::parse(&format!("mem://t/{s}")).unwrap()
    }

    #[test]
    fn stash_adds_dedupes_and_removes() {
        let mut z = DropZone::default();
        assert_eq!(z.add([p("a"), p("b"), p("a")]), 2);
        assert_eq!(z.add([p("b"), p("c")]), 1);
        assert_eq!(z.items, [p("a"), p("b"), p("c")]);
        z.remove(&p("b"));
        assert_eq!(z.items, [p("a"), p("c")]);
        z.clear();
        assert!(z.items.is_empty());
    }

    #[test]
    fn paste_skips_missing_items() {
        let mut z = DropZone::default();
        z.add([p("a"), p("gone"), p("c")]);
        z.missing.insert(p("gone"));
        assert_eq!(z.paste_paths(), [p("a"), p("c")]);
    }

    /// Through `AppState`: Ctrl+Shift+S stashes the selection, the stash survives the
    /// session, a missing item is found and skipped, Paste here copies the rest into the
    /// active folder and Move here empties the stash of what it moved.
    #[test]
    fn stash_persists_and_pastes_through_the_plan() {
        use crate::session::Session;
        let tmp = std::env::temp_dir().join(format!("keel-stash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::create_dir_all(tmp.join("dst")).unwrap();
        for n in ["one.txt", "two.txt", "gone.txt"] {
            std::fs::write(tmp.join("src").join(n), n).unwrap();
        }
        let src = VPath::local(tmp.join("src"));
        let dst = VPath::local(tmp.join("dst"));
        let mut s = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            src.clone(),
        );
        let settle = |s: &mut AppState, done: &dyn Fn(&AppState) -> bool| {
            let until = Instant::now() + Duration::from_secs(10);
            while !done(s) && Instant::now() < until {
                s.tick();
                s.jobs.tick();
                s.dropzone.tick(true, &s.router.clone(), &s.ctx.clone());
                if let Ok(msg) = s.rx.recv_timeout(Duration::from_millis(20)) {
                    s.apply(msg);
                }
            }
            assert!(done(s), "timed out");
        };
        settle(&mut s, &|s| !s.tab(0).loading);
        s.run(0, Action::SelectAll);
        s.run(0, Action::StashSelection);
        assert_eq!(s.dropzone.items.len(), 3);
        assert!(s.dropzone.open);

        let saved = Session::of(&s);
        assert_eq!(saved.stash.len(), 3);
        let mut s = AppState::restore(
            egui::Context::default(),
            Arc::new(Router::new()),
            saved,
            crate::settings::Settings::default(),
            src.clone(),
        );
        assert_eq!(s.dropzone.items.len(), 3);

        std::fs::remove_file(tmp.join("src").join("gone.txt")).unwrap();
        settle(&mut s, &|s| s.dropzone.missing.len() == 1);
        s.run(0, Action::Navigate(dst.clone()));
        settle(&mut s, &|s| !s.tab(0).loading);
        s.run(0, Action::StashPaste { mv: false });
        let landed = |n: &str| tmp.join("dst").join(n).exists();
        settle(&mut s, &|_| landed("one.txt") && landed("two.txt"));
        assert!(!landed("gone.txt"));
        assert_eq!(s.dropzone.items.len(), 3, "a copy keeps the stash");

        s.run(0, Action::StashPaste { mv: true });
        assert_eq!(
            s.dropzone.items,
            [src.join("gone.txt")],
            "moved items leave"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
