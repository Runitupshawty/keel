//! Application state and the message loop. All IO happens in `worker`; this file only
//! reacts to messages and user actions.

use crate::clipboard::Clipboard;
use crate::dialogs::{self, Dialog};
use crate::jobs::{self, Jobs, Source, Transfer};
use crate::keys::Action;
use crate::pane::Pane;
use crate::preview_panel::{PreviewKey, PreviewPanel};
use crate::sidebar::{Drive, Sidebar, DRIVES_REFRESH};
use crate::tab::{Listing, Tab};
use crate::theme::Theme;
use crate::toast::Toasts;
use crate::view_grid::Thumbs;
use crate::{platform, worker};
use crossbeam_channel::{Receiver, Sender};
use keel_vfs::{Entry, Kind, Router, VPath};
use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Watcher events for a folder are coalesced for this long before relisting.
pub const REFRESH_COALESCE: Duration = Duration::from_millis(200);

pub enum Msg {
    /// Answer to listing request `req` (numbers only grow; older answers lose).
    Listed {
        dir: VPath,
        req: u64,
        result: anyhow::Result<Listing>,
    },
    Changed {
        dir: VPath,
    },
    /// A watcher for `dir` is live; held only for its `Drop`.
    Watching {
        pane: usize,
        dir: VPath,
        watcher: Box<dyn Any + Send>,
    },
    Drives(Vec<Drive>),
    Thumb {
        key: PreviewKey,
        preview: keel_preview::Preview,
    },
    Toast(String),
    /// Task 7: preview panel result.
    #[allow(dead_code)]
    Preview {
        key: PreviewKey,
        preview: keel_preview::Preview,
    },
    JobProgress {
        id: u64,
        p: keel_vfs::Progress,
    },
    JobDone {
        id: u64,
        result: anyhow::Result<()>,
    },
    /// Sources resolved and `dst` scanned for name clashes; start or ask.
    Planned {
        op: Transfer,
        conflicts: Vec<String>,
        from_clipboard: bool,
    },
    /// `name` was created or renamed in `dir`: relist and put the cursor on it.
    Select {
        dir: VPath,
        name: String,
    },
    /// Task 7: search tab results.
    #[allow(dead_code)]
    Search {
        id: u64,
        result: anyhow::Result<Vec<keel_search::Hit>>,
    },
}

/// The folder a watcher was requested for, and the live watcher (held for its `Drop`).
type WatchSlot = (Option<VPath>, Option<Box<dyn Any + Send>>);

pub struct AppState {
    pub router: Arc<Router>,
    pub panes: [Pane; 2],
    pub active: usize,
    pub dual: bool,
    pub show_hidden: bool,
    pub sidebar: Sidebar,
    #[allow(dead_code)]
    pub preview: PreviewPanel,
    pub jobs: Jobs,
    pub clipboard: Clipboard,
    pub dialog: Option<Dialog>,
    pub toasts: Toasts,
    pub theme: Theme,
    pub thumbs: Thumbs,
    pub tx: Sender<Msg>,
    pub rx: Receiver<Msg>,
    pub ctx: egui::Context,
    /// One watcher per pane.
    watchers: [WatchSlot; 2],
    pending_refresh: HashMap<VPath, Instant>,
    /// Folders being listed, with their request number; tabs on them share the answer.
    inflight: HashMap<VPath, u64>,
    next_req: u64,
}

impl AppState {
    pub fn new(ctx: egui::Context, router: Arc<Router>, start: VPath) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let theme = Theme::load(if ctx.style().visuals.dark_mode {
            "dark"
        } else {
            "light"
        });
        let thumbs = Thumbs::new(tx.clone(), ctx.clone(), router.clone());
        let mut state = Self {
            router,
            panes: [Pane::new(start.clone()), Pane::new(start)],
            active: 0,
            dual: true,
            show_hidden: false,
            sidebar: Sidebar::default(),
            preview: PreviewPanel::default(),
            jobs: Jobs::new(ctx.clone()),
            clipboard: Clipboard::default(),
            dialog: None,
            toasts: Toasts::default(),
            theme,
            thumbs,
            tx,
            rx,
            ctx,
            watchers: Default::default(),
            pending_refresh: HashMap::new(),
            inflight: HashMap::new(),
            next_req: 0,
        };
        state.theme.apply(&state.ctx);
        state.list(0, 0);
        state.list(1, 0);
        state
    }

    pub fn tab(&self, pane: usize) -> &Tab {
        self.panes[pane].tab()
    }

    pub fn tab_mut(&mut self, pane: usize) -> &mut Tab {
        self.panes[pane].tab_mut()
    }

    /// (Re)lists tab `t` of pane `p` on a worker; the old entries stay until it answers.
    /// A folder already being listed is not listed twice: the tab waits for that answer.
    pub fn list(&mut self, p: usize, t: usize) {
        let tab = &mut self.panes[p].tabs[t];
        tab.refresh();
        let dir = tab.dir.clone();
        if self.inflight.contains_key(&dir) {
            return;
        }
        self.next_req += 1;
        self.inflight.insert(dir.clone(), self.next_req);
        worker::spawn_list(
            self.router.clone(),
            dir,
            self.next_req,
            self.tx.clone(),
            self.ctx.clone(),
        );
    }

    fn list_active(&mut self, p: usize) {
        let t = self.panes[p].active;
        self.list(p, t);
    }

    /// Drains worker messages without blocking.
    pub fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.apply(msg);
        }
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Listed { dir, req, result } => self.listed(dir, req, result),
            Msg::Changed { dir } => {
                self.pending_refresh
                    .entry(dir)
                    .or_insert_with(|| Instant::now() + REFRESH_COALESCE);
            }
            Msg::Watching { pane, dir, watcher } => {
                if self.watchers[pane].0.as_ref() == Some(&dir) {
                    self.watchers[pane].1 = Some(watcher);
                }
            }
            Msg::Drives(drives) => self.sidebar.drives = drives,
            Msg::Thumb { key, preview } => self.thumbs.insert(&self.ctx, key, preview),
            Msg::Toast(text) => self.toasts.error(text),
            Msg::JobProgress { id, p } => self.jobs.progress(id, p),
            Msg::JobDone { id, result } => {
                self.jobs.finish(id, result);
                // Watchers usually beat us to it; network folders may not have one.
                for p in 0..2 {
                    self.list_active(p);
                }
            }
            Msg::Planned {
                op,
                conflicts,
                from_clipboard,
            } => {
                if conflicts.is_empty() {
                    // Skip: a clash that appeared since the scan is never overwritten.
                    self.start_transfer(op, keel_vfs::Conflict::Skip, from_clipboard);
                } else {
                    self.dialog = Some(Dialog::Conflict {
                        names: conflicts,
                        op,
                        from_clipboard,
                    });
                }
            }
            Msg::Select { dir, name } => {
                for p in 0..2 {
                    for t in 0..self.panes[p].tabs.len() {
                        let tab = &mut self.panes[p].tabs[t];
                        if tab.dir == dir {
                            tab.selected = [name.clone()].into();
                            tab.cursor = Some(name.clone());
                            tab.anchor = Some(name.clone());
                            self.list(p, t);
                        }
                    }
                }
            }
            Msg::Preview { .. } | Msg::Search { .. } => {}
        }
    }

    fn start_transfer(&mut self, op: Transfer, conflict: keel_vfs::Conflict, from_clipboard: bool) {
        if from_clipboard && op.mv {
            // A cut pastes once, as in Explorer.
            self.clipboard.set(Vec::new(), false);
        }
        self.jobs.start(op, conflict, self.tx.clone());
    }

    /// Local paths of the targets, or a toast when some are remote (Phase 2+).
    fn local_targets(&mut self, p: usize) -> Option<Vec<PathBuf>> {
        let targets: Vec<VPath> = self
            .tab(p)
            .targets()
            .iter()
            .map(|e| e.path.clone())
            .collect();
        if targets.is_empty() {
            return None;
        }
        let local: Option<Vec<PathBuf>> = targets.iter().map(VPath::to_local_path).collect();
        if local.is_none() {
            self.toasts
                .error("Only local files can be copied or moved for now");
        }
        local
    }

    /// Runs a provider call for `dir` off the UI thread; success selects `name`.
    fn spawn_in_dir(
        &self,
        dir: VPath,
        name: String,
        f: impl FnOnce(&dyn keel_vfs::Provider, &VPath) -> anyhow::Result<()> + Send + 'static,
    ) {
        let (router, tx, ctx) = (self.router.clone(), self.tx.clone(), self.ctx.clone());
        worker::spawn("keel-io", move || {
            let target = dir.join(&name);
            let msg = match router
                .provider_for(&dir)
                .ok_or_else(|| anyhow::anyhow!("no provider for {}", dir.display()))
                .and_then(|p| f(p.as_ref(), &target))
            {
                Ok(()) => Msg::Select { dir, name },
                Err(e) => Msg::Toast(format!("{e:#}")),
            };
            worker::send(&tx, &ctx, msg);
        });
    }

    fn listed(&mut self, dir: VPath, req: u64, result: anyhow::Result<Listing>) {
        if self.inflight.get(&dir) == Some(&req) {
            self.inflight.remove(&dir);
        }
        let still_loading = self.inflight.contains_key(&dir);
        // Tabs may have been reordered or closed since the request: match by folder, and
        // never let an older answer replace a newer one (A -> B -> A).
        let hits: Vec<(usize, usize)> = (0..2)
            .flat_map(|p| (0..self.panes[p].tabs.len()).map(move |t| (p, t)))
            .filter(|&(p, t)| {
                let tab = &self.panes[p].tabs[t];
                tab.dir == dir && req > tab.listed_req
            })
            .collect();
        let Some((&last, rest)) = hits.split_last() else {
            return;
        };
        match result {
            Ok(listing) => {
                let mut fill = |(p, t): (usize, usize), listing: Listing| {
                    let tab = &mut self.panes[p].tabs[t];
                    tab.set_listing(listing);
                    tab.listed_dir = Some(dir.clone());
                    tab.listed_req = req;
                    tab.loading = still_loading;
                    tab.error = None;
                };
                for &hit in rest {
                    fill(hit, listing.clone());
                }
                // 100k entries: the last (usually only) tab takes the listing itself.
                fill(last, listing);
            }
            Err(e) => {
                // Review Focus 2: keep the last good listing, say why.
                let text = format!("{e:#}");
                for &(p, t) in &hits {
                    let tab = &mut self.panes[p].tabs[t];
                    tab.loading = still_loading;
                    tab.listed_req = req;
                    tab.error = Some(text.clone());
                    // A folder we could not enter: stay where the entries came from and
                    // undo the history step (navigate/forward push history, back pushes
                    // future).
                    if let Some(prev) = tab.listed_dir.clone().filter(|p| *p != dir) {
                        if tab.history.last() == Some(&prev) {
                            tab.history.pop();
                        } else if tab.future.last() == Some(&prev) {
                            tab.future.pop();
                        }
                        tab.dir = prev;
                    }
                }
                self.toasts.error(text);
            }
        }
    }

    /// Per-frame housekeeping: due watcher refreshes, drive list, watchers.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let due: Vec<VPath> = self
            .pending_refresh
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(d, _)| d.clone())
            .collect();
        for dir in due {
            if self.inflight.contains_key(&dir) {
                // Relisted once the running listing answers (its repaint runs this again).
                continue;
            }
            self.pending_refresh.remove(&dir);
            for p in 0..2 {
                for t in 0..self.panes[p].tabs.len() {
                    if self.panes[p].tabs[t].dir == dir {
                        self.list(p, t);
                    }
                }
            }
        }
        if let Some(next) = self
            .pending_refresh
            .iter()
            .filter(|(d, _)| !self.inflight.contains_key(*d))
            .map(|(_, at)| at)
            .min()
        {
            self.ctx
                .request_repaint_after(next.saturating_duration_since(now));
        }
        if self
            .sidebar
            .drives_requested
            .is_none_or(|at| at.elapsed() >= DRIVES_REFRESH)
        {
            self.sidebar.drives_requested = Some(now);
            worker::spawn_drives(self.tx.clone(), self.ctx.clone());
            self.ctx.request_repaint_after(DRIVES_REFRESH);
        }
        self.sync_watchers();
    }

    /// One watcher per visible pane, on its active tab's local folder. Created off-thread
    /// because opening a dead network folder can block.
    fn sync_watchers(&mut self) {
        for p in 0..2 {
            let wanted = (p == 0 || self.dual)
                .then(|| self.tab(p).dir.clone())
                .filter(|d| d.to_local_path().is_some());
            if self.watchers[p].0 == wanted {
                continue;
            }
            self.watchers[p] = (wanted.clone(), None);
            let Some(dir) = wanted else { continue };
            let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
            std::thread::Builder::new()
                .name("keel-watch".into())
                .spawn(move || {
                    let local = dir.to_local_path().expect("filtered to local paths");
                    let (wtx, wrx) = crossbeam_channel::bounded(1);
                    let Ok(watcher) = keel_vfs::watch(&local, wtx) else {
                        return;
                    };
                    let _ = tx.send(Msg::Watching {
                        pane: p,
                        dir: dir.clone(),
                        watcher: Box::new(watcher),
                    });
                    ctx.request_repaint();
                    // Ends when the UI drops the watcher.
                    while wrx.recv().is_ok() {
                        let _ = tx.send(Msg::Changed { dir: dir.clone() });
                        ctx.request_repaint();
                    }
                })
                .map_err(|e| tracing::error!("spawn watcher: {e}"))
                .ok();
        }
    }

    pub fn run(&mut self, p: usize, action: Action) {
        let p = if self.dual { p } else { 0 };
        // Targets come from the visible rows; make sure they match this listing.
        let show_hidden = self.show_hidden;
        self.tab_mut(p).visible(show_hidden);
        match action {
            Action::Up => {
                if self.tab_mut(p).up() {
                    self.list_active(p);
                }
            }
            Action::Back => {
                if self.tab_mut(p).back() {
                    self.list_active(p);
                }
            }
            Action::Forward => {
                if self.tab_mut(p).forward() {
                    self.list_active(p);
                }
            }
            Action::Refresh => self.list_active(p),
            Action::Navigate(to) => {
                self.tab_mut(p).navigate(to);
                self.list_active(p);
            }
            Action::NewTab => {
                let dir = self.tab(p).dir.clone();
                self.open_tab(p, dir);
            }
            Action::NewTabAt(dir) => self.open_tab(p, dir),
            Action::CloseTab => self.panes[p].close_tab(self.panes[p].active),
            Action::ToggleDual => {
                self.dual = !self.dual;
                if !self.dual {
                    self.active = 0;
                }
            }
            Action::SwitchPane => {
                if self.dual {
                    self.active = 1 - self.active;
                }
            }
            Action::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.toasts.info(if self.show_hidden {
                    "Showing hidden files"
                } else {
                    "Hiding hidden files"
                });
            }
            Action::FocusFilter => {
                self.tab_mut(p).filter_open = true;
                self.panes[p].focus_filter = true;
            }
            Action::Type(text) => {
                let tab = self.tab_mut(p);
                tab.filter.push_str(&text);
                tab.filter_open = true;
                self.panes[p].focus_filter = true;
            }
            Action::ClearFilter => {
                let tab = self.tab_mut(p);
                if tab.renaming.take().is_none() {
                    if tab.filter_open {
                        tab.filter.clear();
                        tab.filter_open = false;
                    } else {
                        tab.selected.clear();
                    }
                }
            }
            Action::FocusPath => self.panes[p].edit_path(),
            Action::Enter => {
                let target = self.tab(p).targets().first().map(|e| (*e).clone());
                if let Some(e) = target {
                    self.open_entry(p, e);
                }
            }
            Action::Move(nav, extend) => {
                let rows = self.show_hidden;
                let tab = self.tab_mut(p);
                tab.visible(rows);
                tab.move_cursor(nav, extend);
            }
            Action::ToggleSelect => self.tab_mut(p).toggle_cursor(),
            Action::SelectAll => {
                let show_hidden = self.show_hidden;
                let tab = self.tab_mut(p);
                tab.visible(show_hidden);
                tab.select_all();
            }
            Action::InvertSelection => {
                let show_hidden = self.show_hidden;
                let tab = self.tab_mut(p);
                tab.visible(show_hidden);
                tab.invert_selection();
            }
            Action::Rename => {
                let tab = self.tab_mut(p);
                if let Some(name) = tab.targets().first().map(|e| e.name.clone()) {
                    tab.renaming = Some((name.clone(), name));
                }
            }
            Action::CopyPath => {
                let paths: Vec<String> = match self.tab(p).targets() {
                    t if t.is_empty() => vec![self.tab(p).dir.display()],
                    t => t.iter().map(|e| e.path.display()).collect(),
                };
                self.ctx.copy_text(paths.join("\n"));
                self.toasts.info(match paths.len() {
                    1 => "Copied path".to_owned(),
                    n => format!("Copied {n} paths"),
                });
            }
            Action::OpenWith => {
                if let Some(e) = self.tab(p).targets().first() {
                    let path = e.path.clone();
                    self.launch(path, platform::open_with);
                }
            }
            Action::RevealInSystem => {
                let path = self
                    .tab(p)
                    .targets()
                    .first()
                    .map_or_else(|| self.tab(p).dir.clone(), |e| e.path.clone());
                self.launch(path, platform::reveal);
            }
            Action::OpenTerminal => {
                let dir = self.tab(p).dir.clone();
                self.launch(dir, platform::terminal);
            }
            Action::ToggleTheme => {
                self.theme = Theme::load(if self.theme.dark { "light" } else { "dark" });
                self.theme.apply(&self.ctx);
            }
            Action::RenameTo { from, to } => {
                if let Some(why) = dialogs::invalid_name(&to) {
                    return self.toasts.error(why);
                }
                let Some(dir) = from.parent() else { return };
                self.spawn_in_dir(dir, to, move |p, target| p.rename(&from, target));
            }
            Action::Delete => {
                let paths: Vec<VPath> = self
                    .tab(p)
                    .targets()
                    .iter()
                    .map(|e| e.path.clone())
                    .collect();
                if !paths.is_empty() {
                    self.dialog = Some(Dialog::Confirm {
                        text: format!("Move {} to the trash?", jobs::items(paths.len())),
                        on_yes: Action::Trash(paths),
                    });
                }
            }
            Action::Trash(paths) => {
                self.jobs
                    .delete(paths, self.router.clone(), self.tx.clone());
            }
            Action::Copy | Action::Cut => {
                let cut = action == Action::Cut;
                if let Some(paths) = self.local_targets(p) {
                    let n = paths.len();
                    self.clipboard.set(paths, cut);
                    let verb = if cut { "Cut" } else { "Copied" };
                    self.toasts.info(format!("{verb} {}", jobs::items(n)));
                }
            }
            Action::Paste => match self.tab(p).dir.to_local_path() {
                Some(dst) => jobs::spawn_plan(
                    Source::Clipboard(self.clipboard.clone()),
                    dst,
                    self.tx.clone(),
                    self.ctx.clone(),
                ),
                None => self
                    .toasts
                    .error("Paste into remote folders is not supported yet"),
            },
            Action::StartTransfer {
                op,
                conflict,
                from_clipboard,
            } => self.start_transfer(op, conflict, from_clipboard),
            Action::Drop { paths, from, dst } => {
                let Some(dst_local) = dst.to_local_path() else {
                    return self
                        .toasts
                        .error("Drop into remote folders is not supported yet");
                };
                if paths.is_empty() || from.as_ref().is_some_and(|(_, dir)| *dir == dst) {
                    return;
                }
                // Shift = move; within one pane a drag into a subfolder moves, like Explorer.
                let shift = self.ctx.input(|i| i.modifiers.shift);
                let mv = shift || from.is_some_and(|(pane, _)| pane == p);
                jobs::spawn_plan(
                    Source::Paths(paths, mv),
                    dst_local,
                    self.tx.clone(),
                    self.ctx.clone(),
                );
            }
            Action::NewFolder | Action::NewFile => {
                let folder = action == Action::NewFolder;
                self.dialog = Some(Dialog::NewItem {
                    dir: self.tab(p).dir.clone(),
                    folder,
                    text: if folder { "New folder" } else { "New file.txt" }.to_owned(),
                    focus: true,
                });
            }
            Action::Create { dir, name, folder } => {
                if let Some(why) = dialogs::invalid_name(&name) {
                    return self.toasts.error(why);
                }
                self.spawn_in_dir(dir, name, move |p, target| {
                    // ponytail: stat-then-create race; Provider has no create_new yet.
                    anyhow::ensure!(
                        p.stat(target).is_err(),
                        "{} already exists",
                        target.display()
                    );
                    if folder {
                        p.mkdir(target)
                    } else {
                        p.write(target).map(drop)
                    }
                });
            }
            Action::Properties => self.toasts.not_yet("Properties"),
            Action::Search => self.toasts.not_yet("Search"),
            Action::JumpFolder => self.toasts.not_yet("Jump to folder"),
            Action::Palette => self.toasts.not_yet("Command palette"),
            Action::TogglePreview => self.toasts.not_yet("Preview panel"),
        }
    }

    fn open_tab(&mut self, p: usize, dir: VPath) {
        let pane = &mut self.panes[p];
        pane.tabs.push(Tab::new(dir));
        pane.active = pane.tabs.len() - 1;
        self.list_active(p);
    }

    /// Folders (and links, which usually point at folders) navigate; files open in
    /// the OS default app.
    pub fn open_entry(&mut self, p: usize, e: Entry) {
        // Links carry their target's kind; a dangling link opens like a file (and fails
        // with the OS message).
        if e.kind == Kind::Dir {
            self.run(p, Action::Navigate(e.path));
        } else {
            self.launch(e.path, platform::open);
        }
    }

    fn launch(&self, path: VPath, f: fn(&std::path::Path) -> std::io::Result<()>) {
        worker::spawn_local(
            self.router.clone(),
            path,
            self.tx.clone(),
            self.ctx.clone(),
            f,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tab::test_entry;
    use keel_vfs::{Caps, Provider};

    /// A provider whose folder has vanished.
    struct Gone;
    impl Provider for Gone {
        fn scheme(&self) -> &'static str {
            "gone"
        }
        fn caps(&self) -> Caps {
            Caps::default()
        }
        fn list(&self, dir: &VPath) -> anyhow::Result<Vec<Entry>> {
            anyhow::bail!("{} disappeared", dir.display())
        }
        fn stat(&self, _: &VPath) -> anyhow::Result<Entry> {
            anyhow::bail!("gone")
        }
        fn read(&self, _: &VPath) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
            anyhow::bail!("gone")
        }
        fn write(&self, _: &VPath) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
            anyhow::bail!("gone")
        }
        fn mkdir(&self, _: &VPath) -> anyhow::Result<()> {
            anyhow::bail!("gone")
        }
        fn rename(&self, _: &VPath, _: &VPath) -> anyhow::Result<()> {
            anyhow::bail!("gone")
        }
        fn remove(&self, _: &VPath) -> anyhow::Result<()> {
            anyhow::bail!("gone")
        }
        fn local_copy(&self, _: &VPath) -> anyhow::Result<std::path::PathBuf> {
            anyhow::bail!("gone")
        }
    }

    #[test]
    fn listed_err_keeps_entries_and_toasts() {
        let mut router = Router::new();
        router.register(Arc::new(Gone));
        let dir = VPath::parse("gone://usb/photos").unwrap();
        let mut state = AppState::new(egui::Context::default(), Arc::new(router), dir.clone());
        // Both panes start on the vanished folder: one shared listing, one toast.
        let msg = state
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker answers");
        state.apply(msg);
        assert!(state.rx.recv_timeout(Duration::from_millis(200)).is_err());
        assert_eq!(state.toasts.list.len(), 1, "same error is one toast");
        state.toasts.list.clear();

        let good = vec![
            test_entry(&dir, "a.jpg", Kind::File, 1),
            test_entry(&dir, "b.jpg", Kind::File, 2),
        ];
        state.next_req += 10;
        let good_req = state.next_req;
        state.apply(Msg::Listed {
            dir: dir.clone(),
            req: good_req,
            result: Ok(Listing::new(good)),
        });
        assert_eq!(state.tab(0).entries().len(), 2);
        // An older answer (A -> B -> A) never replaces it.
        state.apply(Msg::Listed {
            dir: dir.clone(),
            req: good_req - 1,
            result: Ok(Listing::new(Vec::new())),
        });
        assert_eq!(state.tab(0).entries().len(), 2);

        // The folder disappears; a watcher refresh fails.
        state.list(0, 0);
        let msg = state.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(&msg, Msg::Listed { result: Err(_), .. }));
        state.apply(msg);
        let tab = state.tab(0);
        assert_eq!(tab.entries().len(), 2, "last good listing kept");
        assert!(!tab.loading);
        assert!(tab.error.as_deref().unwrap().contains("disappeared"));
        assert_eq!(tab.dir, dir);
        assert_eq!(state.toasts.list.len(), 1);
    }

    #[test]
    fn failed_navigation_returns_to_the_listed_folder() {
        let mut router = Router::new();
        router.register(Arc::new(Gone));
        let home = VPath::parse("gone://usb/").unwrap();
        let mut state = AppState::new(egui::Context::default(), Arc::new(router), home.clone());
        state.apply(Msg::Listed {
            dir: home.clone(),
            req: 100,
            result: Ok(Listing::new(vec![test_entry(
                &home,
                "locked",
                Kind::Dir,
                0,
            )])),
        });
        let locked = home.join("locked");
        state.tab_mut(0).navigate(locked.clone());
        state.apply(Msg::Listed {
            dir: locked.clone(),
            req: 101,
            result: Err(anyhow::anyhow!("access denied")),
        });
        let tab = state.tab(0);
        assert_eq!(tab.dir, home);
        assert!(tab.history.is_empty());
        assert_eq!(tab.entries().len(), 1);

        // A failed Back leaves no stray Forward entry.
        state.tab_mut(0).history.push(locked.clone());
        assert!(state.tab_mut(0).back());
        state.apply(Msg::Listed {
            dir: locked,
            req: 102,
            result: Err(anyhow::anyhow!("access denied")),
        });
        let tab = state.tab(0);
        assert_eq!(tab.dir, home);
        assert!(tab.future.is_empty());
    }
}
