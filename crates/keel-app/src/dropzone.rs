//! Drop zone: a strip above the status bar holding a stash of paths across navigation.
//! Rows dragged onto it, or Ctrl+Shift+S, stash them; Paste here / Move here send them
//! to the active pane's folder through the usual transfer plan.

use crate::jobs::{self, Source};
use crate::keys::Action;
use crate::pane::DragPayload;
use crate::state::{AppState, Msg};
use crossbeam_channel::{Receiver, Sender};
use keel_vfs::{Router, VPath};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often a shown strip checks that its items still exist.
const RECHECK: Duration = Duration::from_secs(3);
/// SFTP and cloud items are checked this often at most (each check is a round trip).
const REMOTE_RECHECK: Duration = Duration::from_secs(30);
/// A check that has not answered after this long (a hung `stat`) is given up.
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
/// Chips drawn; the rest are counted.
const MAX_CHIPS: usize = 60;
/// The stash holds at most this many items.
pub const MAX_STASH: usize = 50_000;

/// A check's answer: `remote` says whether SFTP / cloud items were part of it.
struct Checked {
    generation: u64,
    missing: HashSet<VPath>,
    remote: bool,
}

pub struct DropZone {
    items: Vec<VPath>,
    /// `items` as a set (dedupe without a scan).
    index: HashSet<VPath>,
    /// Bumped by every change of `items` (the session is saved when it moves).
    pub version: u64,
    /// The strip is shown (Ctrl+Shift+Z); it also shows while rows are dragged.
    pub open: bool,
    /// Items the last check did not find: greyed, skipped on paste.
    pub missing: HashSet<VPath>,
    checked: Option<Instant>,
    remote_checked: Option<Instant>,
    /// The running check: its generation and start (only the newest one's answer counts).
    checking: Option<(u64, Instant)>,
    generation: u64,
    check_timeout: Duration,
    tx: Sender<Checked>,
    rx: Receiver<Checked>,
    /// Move here: items whose move is being planned, and the move jobs started on them.
    /// Items leave the stash only once their job is over and they are gone from where
    /// they were (a cancelled conflict, Skip or a failure keeps them).
    moving: HashSet<VPath>,
    move_jobs: Vec<(u64, Vec<VPath>)>,
}

impl Default for DropZone {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            items: Vec::new(),
            index: HashSet::new(),
            version: 0,
            open: false,
            missing: HashSet::new(),
            checked: None,
            remote_checked: None,
            checking: None,
            generation: 0,
            check_timeout: CHECK_TIMEOUT,
            tx,
            rx,
            moving: HashSet::new(),
            move_jobs: Vec::new(),
        }
    }
}

/// `p` is known not to exist: a local `NotFound`, or a provider's not-found error. An
/// unreachable host, a permission error or a provider not registered (yet) is not that.
fn gone(router: &Router, p: &VPath) -> bool {
    match p.to_local_path() {
        Some(local) => matches!(local.try_exists(), Ok(false)),
        None => router
            .provider_for(p)
            .is_some_and(|provider| provider.stat(p).is_err_and(|e| is_not_found(&e))),
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    })
}

impl DropZone {
    pub fn items(&self) -> &[VPath] {
        &self.items
    }

    /// Replaces the stash (a restored session or profile); forgets what was missing.
    pub fn set_items(&mut self, items: Vec<VPath>) {
        self.clear();
        self.add(items);
    }

    /// Adds `paths` not stashed yet, up to `MAX_STASH`: the number added, and whether
    /// some did not fit.
    pub fn add(&mut self, paths: impl IntoIterator<Item = VPath>) -> (usize, bool) {
        let before = self.items.len();
        let mut full = false;
        for p in paths {
            if self.index.contains(&p) {
                continue;
            }
            if self.items.len() >= MAX_STASH {
                full = true;
                break;
            }
            self.index.insert(p.clone());
            self.items.push(p);
        }
        let added = self.items.len() - before;
        if added > 0 {
            self.version += 1;
            self.checked = None;
        }
        (added, full)
    }

    pub fn remove(&mut self, path: &VPath) {
        self.remove_all(std::slice::from_ref(path));
    }

    pub fn remove_all(&mut self, paths: &[VPath]) {
        let paths: HashSet<&VPath> = paths.iter().filter(|p| self.index.contains(*p)).collect();
        if paths.is_empty() {
            return;
        }
        self.items.retain(|p| !paths.contains(p));
        for p in paths {
            self.index.remove(p);
            self.missing.remove(p);
        }
        self.version += 1;
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.index.clear();
        self.missing.clear();
        self.version += 1;
    }

    /// What Paste here / Move here transfers: the items not known to be missing.
    pub fn paste_paths(&self) -> Vec<VPath> {
        self.items
            .iter()
            .filter(|p| !self.missing.contains(*p))
            .cloned()
            .collect()
    }

    /// `AppState::start_transfer`: a move of stashed items planned by Move here becomes
    /// that job's (its items are dropped from the stash when it is over).
    pub fn claim(
        &mut self,
        op: &crate::jobs::Transfer,
        from_clipboard: bool,
    ) -> Option<Vec<VPath>> {
        let ours = op.mv
            && !from_clipboard
            && !op.src.is_empty()
            && op.src.iter().all(|p| self.moving.contains(p));
        if !ours {
            return None;
        }
        for p in &op.src {
            self.moving.remove(p);
        }
        Some(op.src.clone())
    }

    /// Takes check answers; starts a check (on a worker: `stat` may block) when the strip
    /// is shown and the last one is stale. SFTP / cloud items are checked every
    /// `REMOTE_RECHECK` at most; a check stuck past `check_timeout` is given up.
    fn tick(&mut self, shown: bool, router: &Arc<Router>, ctx: &egui::Context) {
        while let Ok(c) = self.rx.try_recv() {
            if c.generation != self.generation {
                continue;
            }
            self.checking = None;
            let mut missing = c.missing;
            if !c.remote {
                let old = std::mem::take(&mut self.missing);
                missing.extend(old.into_iter().filter(crate::remotes::is_network));
            }
            self.missing = missing;
        }
        if let Some((_, at)) = self.checking {
            if at.elapsed() < self.check_timeout {
                return;
            }
            self.checking = None;
        }
        if !shown || self.items.is_empty() {
            return;
        }
        if self.checked.is_some_and(|at| at.elapsed() < RECHECK) {
            ctx.request_repaint_after(RECHECK);
            return;
        }
        let now = Instant::now();
        self.checked = Some(now);
        let remote = self
            .remote_checked
            .is_none_or(|at| at.elapsed() >= REMOTE_RECHECK);
        if remote {
            self.remote_checked = Some(now);
        }
        let items: Vec<VPath> = self
            .items
            .iter()
            .filter(|p| remote || !crate::remotes::is_network(p))
            .cloned()
            .collect();
        self.generation += 1;
        let generation = self.generation;
        let (router, tx, repaint) = (router.clone(), self.tx.clone(), ctx.clone());
        let started = crate::worker::spawn("keel-stash-check", move || {
            let missing = items.into_iter().filter(|p| gone(&router, p)).collect();
            let _ = tx.send(Checked {
                generation,
                missing,
                remote,
            });
            repaint.request_repaint();
        });
        if started {
            self.checking = Some((generation, now));
            ctx.request_repaint_after(self.check_timeout);
        }
    }
}

impl AppState {
    /// `Msg::JobDone`: a Move here job is over (moved, skipped, failed or cancelled):
    /// what is gone from where it was leaves the stash (checked on a worker: stat may
    /// block); what is still there stays.
    pub(crate) fn stash_job_done(&mut self, id: u64) {
        let Some(i) = self.dropzone.move_jobs.iter().position(|(j, _)| *j == id) else {
            return;
        };
        let (_, src) = self.dropzone.move_jobs.swap_remove(i);
        let (router, tx, ctx) = (self.router.clone(), self.tx.clone(), self.ctx.clone());
        crate::worker::spawn("keel-stash-moved", move || {
            let moved: Vec<VPath> = src.into_iter().filter(|p| gone(&router, p)).collect();
            crate::worker::send(&tx, &ctx, Msg::StashMoved(moved));
        });
    }

    /// `AppState::start_transfer`: job `id` was started for `op`.
    pub(crate) fn stash_job_started(&mut self, id: u64, src: Option<Vec<VPath>>) {
        if let Some(src) = src {
            self.dropzone.move_jobs.push((id, src));
        }
    }
}

/// Plans for Paste here / Move here: the entries of one archive folder per plan (they
/// are extracted from it), everything else in one plan (one conflict dialog).
fn groups(paths: Vec<VPath>) -> Vec<Vec<VPath>> {
    let mut groups: Vec<(Option<VPath>, Vec<VPath>)> = Vec::new();
    for p in paths {
        let key = p.split_archive().and_then(|_| p.parent());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.push(p),
            None => groups.push((key, vec![p])),
        }
    }
    groups.into_iter().map(|(_, g)| g).collect()
}

/// The drop zone actions, on pane `p`.
pub fn run(s: &mut AppState, p: usize, action: Action) {
    let full = |s: &mut AppState| {
        s.toasts.error(format!(
            "The drop zone holds at most {MAX_STASH} items; the rest were not stashed"
        ))
    };
    match action {
        Action::ToggleDropZone => s.dropzone.open = !s.dropzone.open,
        Action::StashSelection => {
            let paths: Vec<VPath> = s.tab(p).targets().iter().map(|e| e.path.clone()).collect();
            if paths.is_empty() {
                return s.toasts.info("Select something to stash");
            }
            let (added, capped) = s.dropzone.add(paths);
            s.dropzone.open = true;
            s.toasts.info(format!("Stashed {}", jobs::items(added)));
            if capped {
                full(s);
            }
        }
        Action::Stash(paths) => {
            if s.dropzone.add(paths).1 {
                full(s);
            }
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
                // They leave the stash once their job is over (`claim`), not before.
                s.dropzone.moving.extend(paths.iter().cloned());
            }
            for group in groups(paths) {
                let started = jobs::spawn_plan(
                    Source::Paths(group, mv),
                    dst.clone(),
                    s.router.clone(),
                    s.tx.clone(),
                    s.ctx.clone(),
                );
                if !started {
                    s.toasts.error("Could not start the transfer");
                }
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
                .add(egui::Button::new("×").frame(false).small())
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
        assert_eq!(z.add([p("a"), p("b"), p("a")]), (2, false));
        assert_eq!(z.add([p("b"), p("c")]), (1, false));
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

        for n in ["one.txt", "two.txt"] {
            std::fs::remove_file(tmp.join("dst").join(n)).unwrap();
        }
        s.run(0, Action::StashPaste { mv: true });
        assert_eq!(s.dropzone.items.len(), 3, "nothing leaves before it moved");
        settle(&mut s, &|s| s.dropzone.items.len() == 1);
        assert_eq!(
            s.dropzone.items,
            [src.join("gone.txt")],
            "moved items leave"
        );
        assert!(landed("one.txt") && landed("two.txt"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn app(start: &VPath) -> AppState {
        AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            start.clone(),
        )
    }

    fn settle(s: &mut AppState, done: &dyn Fn(&AppState) -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done(s) && Instant::now() < until {
            s.tick();
            s.jobs.tick();
            if let Ok(msg) = s.rx.recv_timeout(Duration::from_millis(20)) {
                s.apply(msg);
            }
        }
        assert!(done(s), "timed out");
    }

    /// Move here keeps an item until it has really moved: a cancelled conflict dialog,
    /// a skipped clash and a failed job leave the stash as it was.
    #[test]
    fn move_here_drops_items_only_once_moved() {
        let tmp = std::env::temp_dir().join(format!("keel-stash-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::create_dir_all(tmp.join("dst")).unwrap();
        for n in ["one.txt", "two.txt"] {
            std::fs::write(tmp.join("src").join(n), "new").unwrap();
        }
        std::fs::write(tmp.join("dst").join("two.txt"), "old").unwrap();
        let src = VPath::local(tmp.join("src"));
        let dst = VPath::local(tmp.join("dst"));
        let (one, two) = (src.join("one.txt"), src.join("two.txt"));
        let mut s = app(&dst);
        settle(&mut s, &|s| !s.tab(0).loading);
        s.dropzone.add([one.clone(), two.clone()]);

        // The clash asks; cancelling moves nothing and keeps both.
        s.run(0, Action::StashPaste { mv: true });
        settle(&mut s, &|s| s.dialog.is_some());
        assert_eq!(s.dropzone.items(), [one.clone(), two.clone()]);
        let Some(crate::dialogs::Dialog::Conflict { op, .. }) = s.dialog.take() else {
            panic!("conflict dialog");
        };
        assert_eq!(s.dropzone.items(), [one.clone(), two.clone()]);

        // Skip: the clashing one stays where it was, and in the stash.
        s.run(
            0,
            Action::StartTransfer {
                op,
                conflict: keel_vfs::Conflict::Skip,
                from_clipboard: false,
            },
        );
        settle(&mut s, &|s| s.dropzone.items().len() == 1);
        assert_eq!(s.dropzone.items(), std::slice::from_ref(&two));
        assert!(tmp.join("dst").join("one.txt").exists());

        // A failed or cancelled job: what is still there stays.
        s.dropzone
            .move_jobs
            .push((999, vec![two.clone(), one.clone()]));
        s.dropzone.add([one.clone()]);
        s.apply(Msg::JobDone {
            id: 999,
            result: Err(anyhow::anyhow!("cancelled")),
        });
        settle(&mut s, &|s| s.dropzone.items().len() == 1);
        assert_eq!(s.dropzone.items(), [two], "only the moved one left");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A stash with entries of two archives and a plain file pastes all of them (one
    /// plan per archive folder, one for the rest).
    #[test]
    fn a_mixed_stash_pastes_every_source() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("keel-stash-mixed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("dst")).unwrap();
        for (zip, entry) in [("x.zip", "from-x.txt"), ("y.zip", "from-y.txt")] {
            let file = std::fs::File::create(tmp.join(zip)).unwrap();
            let mut w = zip::ZipWriter::new(file);
            w.start_file(entry, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(b"hi").unwrap();
            w.finish().unwrap();
        }
        std::fs::write(tmp.join("plain.txt"), "hi").unwrap();
        let root = VPath::local(&tmp);
        let x = VPath::join_archive(&root.join("x.zip"), "from-x.txt");
        let y = VPath::join_archive(&root.join("y.zip"), "from-y.txt");
        let plain = root.join("plain.txt");
        assert_eq!(
            groups(vec![x.clone(), plain.clone(), y.clone()]),
            [vec![x.clone()], vec![plain.clone()], vec![y.clone()]]
        );
        let dst = VPath::local(tmp.join("dst"));
        let mut s = app(&dst);
        settle(&mut s, &|s| !s.tab(0).loading);
        s.dropzone.add([x, plain, y]);
        s.run(0, Action::StashPaste { mv: false });
        let landed = |n: &str| tmp.join("dst").join(n).exists();
        settle(&mut s, &|_| {
            landed("from-x.txt") && landed("from-y.txt") && landed("plain.txt")
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn adding_20k_items_is_fast_and_the_stash_is_capped() {
        let paths: Vec<VPath> = (0..20_000).map(|i| p(&format!("f{i}"))).collect();
        let mut z = DropZone::default();
        let at = Instant::now();
        assert_eq!(z.add(paths.clone()), (20_000, false));
        assert_eq!(z.add(paths), (0, false), "all known");
        let took = at.elapsed();
        // The Vec scan this replaced took 1.6 s in release; debug builds are ~10x slower.
        let limit = Duration::from_millis(if cfg!(debug_assertions) { 500 } else { 50 });
        assert!(took < limit, "{took:?}");
        let more: Vec<VPath> = (0..MAX_STASH).map(|i| p(&format!("g{i}"))).collect();
        assert_eq!(z.add(more), (MAX_STASH - 20_000, true));
        assert_eq!(z.items().len(), MAX_STASH);
    }

    /// A provider whose `stat` fails by name: `denied`, `slow` (after a while), else not
    /// found.
    struct Probe;
    impl keel_vfs::Provider for Probe {
        fn scheme(&self) -> &'static str {
            "probe"
        }
        fn caps(&self) -> keel_vfs::Caps {
            keel_vfs::Caps::default()
        }
        fn list(&self, _: &VPath) -> anyhow::Result<Vec<keel_vfs::Entry>> {
            anyhow::bail!("no")
        }
        fn stat(&self, p: &VPath) -> anyhow::Result<keel_vfs::Entry> {
            use std::io::{Error, ErrorKind};
            let kind = match p.name() {
                "denied" => ErrorKind::PermissionDenied,
                "slow" => {
                    std::thread::sleep(Duration::from_millis(300));
                    ErrorKind::NotFound
                }
                _ => ErrorKind::NotFound,
            };
            Err(anyhow::Error::new(Error::from(kind)).context("stat"))
        }
        fn read(&self, _: &VPath) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
            anyhow::bail!("no")
        }
        fn write(&self, _: &VPath) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
            anyhow::bail!("no")
        }
        fn mkdir(&self, _: &VPath) -> anyhow::Result<()> {
            anyhow::bail!("no")
        }
        fn rename(&self, _: &VPath, _: &VPath) -> anyhow::Result<()> {
            anyhow::bail!("no")
        }
        fn remove(&self, _: &VPath) -> anyhow::Result<()> {
            anyhow::bail!("no")
        }
        fn list_complete(&self, dir: &VPath) -> anyhow::Result<Vec<keel_vfs::Entry>> {
            self.list(dir)
        }
        fn remove_kind(&self) -> keel_vfs::RemoveKind {
            keel_vfs::RemoveKind::Permanent
        }
        fn local_copy(&self, _: &VPath) -> anyhow::Result<std::path::PathBuf> {
            anyhow::bail!("no")
        }
    }

    /// Only "not found" is missing (not a permission error); a check that hangs is given
    /// up, and its late answer does not count.
    #[test]
    fn the_missing_check_knows_not_found_and_gives_up_on_a_hang() {
        let router = Arc::new(Router::new());
        router.register(Arc::new(Probe));
        let probe = |n: &str| VPath::parse(&format!("probe://h/{n}")).unwrap();
        assert!(gone(&router, &probe("nope")));
        assert!(!gone(&router, &probe("denied")));
        assert!(!gone(&router, &p("no-provider")));
        let ctx = egui::Context::default();

        let mut z = DropZone::default();
        z.add([probe("nope"), probe("denied")]);
        z.tick(true, &router, &ctx);
        let until = Instant::now() + Duration::from_secs(5);
        while z.checking.is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
            z.tick(true, &router, &ctx);
        }
        assert_eq!(z.missing, HashSet::from([probe("nope")]));

        let mut z = DropZone {
            check_timeout: Duration::from_millis(50),
            ..Default::default()
        };
        z.add([probe("slow")]);
        z.tick(true, &router, &ctx);
        assert_eq!(z.checking.map(|c| c.0), Some(1));
        std::thread::sleep(Duration::from_millis(80));
        z.tick(true, &router, &ctx);
        assert!(z.checking.is_none(), "given up");
        z.checked = None;
        z.tick(true, &router, &ctx);
        assert_eq!(z.checking.map(|c| c.0), Some(2), "asked again");
        // The first check's late answer is ignored.
        z.tx.send(Checked {
            generation: 1,
            missing: HashSet::from([probe("slow")]),
            remote: true,
        })
        .unwrap();
        z.tick(true, &router, &ctx);
        assert!(z.missing.is_empty());
    }
}
