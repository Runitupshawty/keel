//! Application state and the message loop. All IO happens in `worker`; this file only
//! reacts to messages and user actions.

use crate::clipboard::Clipboard;
use crate::dialogs::{self, Dialog};
use crate::jobs::{self, Jobs, Source, Transfer};
use crate::jump::Jump;
use crate::keys::Action;
use crate::palette::Palette;
use crate::pane::Pane;
use crate::preview_panel::{PreviewKey, PreviewPanel};
use crate::search_tab::DEBOUNCE;
use crate::session::Session;
use crate::settings::Settings;
use crate::sidebar::{Drive, Sidebar, DRIVES_REFRESH};
use crate::tab::{Listing, Tab, TabKind};
use crate::theme::Theme;
use crate::toast::Toasts;
use crate::view_grid::Thumbs;
use crate::{platform, worker};
use crossbeam_channel::{Receiver, Sender};
use keel_search::Searcher;
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
    /// Planning a transfer found nothing to do (or no files on the clipboard).
    PlanFailed(String),
    /// Sources of a failed cut-move that still exist: the cut to put back.
    RestoreCut(Vec<PathBuf>),
    /// `name` was created or renamed in `dir`: relist and put the cursor on it.
    Select {
        dir: VPath,
        name: String,
    },
    Search {
        id: u64,
        result: anyhow::Result<Vec<keel_search::Hit>>,
    },
    /// The platform searcher loaded; `reason` says why it does not work, if it does not.
    Searcher {
        searcher: Arc<dyn Searcher>,
        reason: Option<String>,
    },
    SearchProbe(Option<String>),
    /// Folder list for Ctrl+P.
    JumpIndexed(Vec<String>),
    Jump {
        id: u64,
        results: Vec<String>,
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
    pub preview: PreviewPanel,
    /// Loaded on a worker at startup (`load_searcher`); None until then.
    pub searcher: Option<Arc<dyn Searcher>>,
    /// Why search does not work (banner text), None when it does.
    pub search_reason: Option<String>,
    pub jump: Jump,
    pub palette: Palette,
    pub jobs: Jobs,
    pub clipboard: Clipboard,
    /// A clipboard paste is being planned; further Ctrl+V wait (no double move of a cut).
    pub paste_pending: bool,
    /// The move job started from a cut, and its sources (put back as a cut if it fails).
    cut_job: Option<(u64, Vec<PathBuf>)>,
    pub dialog: Option<Dialog>,
    /// Theme, layout and preview options; saved by `settings::Persist`.
    pub settings: Settings,
    pub settings_open: bool,
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
    /// One tab on `start` per pane, default settings (tests).
    #[cfg(test)]
    pub fn new(ctx: egui::Context, router: Arc<Router>, start: VPath) -> Self {
        Self::restore(ctx, router, Session::single(start), Settings::default())
    }

    /// Opens the tabs of `session` (already repaired, see `Session::repair`) with
    /// `settings` applied, and lists every tab.
    pub fn restore(
        ctx: egui::Context,
        router: Arc<Router>,
        session: Session,
        settings: Settings,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let theme = Theme::load(&settings.theme);
        let thumbs = Thumbs::new(tx.clone(), ctx.clone(), router.clone());
        let previewer = worker::spawn_previewer(router.clone(), tx.clone(), ctx.clone());
        let jump = Jump::new(tx.clone(), ctx.clone());
        let mut panes = session
            .panes
            .into_iter()
            .zip(session.active_tab)
            .map(|(dirs, active)| {
                let mut dirs = dirs.into_iter();
                let mut pane = Pane::new(dirs.next().expect("repaired session has a tab per pane"));
                pane.tabs.extend(dirs.map(Tab::new));
                pane.active = active.min(pane.tabs.len() - 1);
                pane
            });
        let panes = [
            panes.next().expect("two panes"),
            panes.next().expect("two panes"),
        ];
        let mut preview = PreviewPanel::new(previewer);
        preview.open = settings.preview_open;
        preview.max_bytes = settings.max_preview_bytes();
        let mut state = Self {
            router,
            panes,
            active: if settings.dual {
                session.active.min(1)
            } else {
                0
            },
            dual: settings.dual,
            show_hidden: settings.show_hidden,
            sidebar: Sidebar::default(),
            preview,
            searcher: None,
            search_reason: None,
            jump,
            palette: Palette::default(),
            jobs: Jobs::new(ctx.clone()),
            clipboard: Clipboard::default(),
            paste_pending: false,
            cut_job: None,
            dialog: None,
            settings,
            settings_open: false,
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
        // Tabs are only listed when asked; restored background tabs need it now.
        for p in 0..2 {
            for t in 0..state.panes[p].tabs.len() {
                state.list(p, t);
            }
        }
        state
    }

    /// Shows the Settings window (when open) on a copy of the live options and applies
    /// what the user changed.
    pub fn settings_ui(&mut self, ctx: &egui::Context) {
        let s = &mut self.settings;
        s.show_hidden = self.show_hidden;
        s.dual = self.dual;
        s.preview_open = self.preview.open;
        if !self.settings_open {
            return;
        }
        let theme_changed = crate::settings::window(ctx, &mut self.settings_open, s);
        self.show_hidden = s.show_hidden;
        self.preview.open = s.preview_open;
        let max = s.max_preview_bytes();
        if self.preview.max_bytes != max {
            self.preview.max_bytes = max;
            // Re-evaluate the shown file against the new limit.
            self.preview.key = None;
        }
        if self.dual != s.dual {
            self.run(self.active, Action::ToggleDual);
        }
        if theme_changed {
            self.theme = Theme::load(&self.settings.theme);
            self.theme.apply(&self.ctx);
        }
    }

    /// Loads the platform searcher off the UI thread (the Everything DLL load may block).
    pub fn load_searcher(&self) {
        worker::spawn_searcher(self.tx.clone(), self.ctx.clone());
    }

    pub fn tab(&self, pane: usize) -> &Tab {
        self.panes[pane].tab()
    }

    pub fn tab_mut(&mut self, pane: usize) -> &mut Tab {
        self.panes[pane].tab_mut()
    }

    /// (Re)lists tab `t` of pane `p` on a worker; the old entries stay until it answers.
    /// A folder already being listed is not listed twice: the tab waits for that answer.
    /// A search tab reruns its query instead.
    pub fn list(&mut self, p: usize, t: usize) {
        let tab = &mut self.panes[p].tabs[t];
        if let TabKind::Search { due, .. } = &mut tab.kind {
            *due = Some(Instant::now());
            self.ctx.request_repaint();
            return;
        }
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
                if let Some((job, src)) = self.cut_job.take() {
                    if job != id {
                        self.cut_job = Some((job, src));
                    } else if result.is_err() {
                        // The move failed or was cancelled: what was not moved can be
                        // pasted again (checked on a worker: stat may block).
                        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                        worker::spawn("keel-recut", move || {
                            let left: Vec<PathBuf> =
                                src.into_iter().filter(|p| p.exists()).collect();
                            worker::send(&tx, &ctx, Msg::RestoreCut(left));
                        });
                    }
                }
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
                if from_clipboard {
                    self.paste_pending = false;
                }
                if conflicts.is_empty() {
                    // Skip: a clash that appeared since the scan is never overwritten.
                    self.start_transfer(op, keel_vfs::Conflict::Skip, from_clipboard);
                } else if self.dialog.is_some() {
                    self.toasts
                        .error("Another dialog is open; nothing was copied or moved");
                } else {
                    self.dialog = Some(Dialog::Conflict {
                        names: conflicts,
                        op,
                        from_clipboard,
                    });
                }
            }
            Msg::RestoreCut(paths) => {
                // Only when nothing was copied since, here or in another app.
                if !paths.is_empty()
                    && self.clipboard.paths.is_empty()
                    && !self.clipboard.changed_outside()
                {
                    self.clipboard.set(paths, true);
                }
            }
            Msg::PlanFailed(text) => {
                self.paste_pending = false;
                self.toasts.error(text);
            }
            Msg::Select { dir, name } => {
                for p in 0..2 {
                    for t in 0..self.panes[p].tabs.len() {
                        let tab = &mut self.panes[p].tabs[t];
                        if tab.is_search() {
                            // A renamed or new file may now (not) match.
                            self.list(p, t);
                        } else if tab.dir == dir {
                            tab.selected = [name.clone()].into();
                            tab.cursor = Some(name.clone());
                            tab.anchor = Some(name.clone());
                            self.list(p, t);
                        }
                    }
                }
            }
            Msg::Preview { key, preview } => self.preview.insert(&self.ctx, key, preview),
            Msg::Search { id, result } => self.searched(id, result),
            Msg::Searcher { searcher, reason } => {
                self.searcher = Some(searcher);
                self.search_reason = reason;
                if std::mem::take(&mut self.jump.waiting) {
                    self.index_folders();
                }
            }
            Msg::SearchProbe(reason) => {
                self.search_reason = reason;
                if self.search_reason.is_none() {
                    self.toasts.info("Search is available");
                }
            }
            Msg::JumpIndexed(items) => self.jump.indexed(items),
            Msg::Jump { id, results } => self.jump.results(id, results),
        }
    }

    fn searched(&mut self, id: u64, result: anyhow::Result<Vec<keel_search::Hit>>) {
        let Some(tab) = self
            .panes
            .iter_mut()
            .flat_map(|p| p.tabs.iter_mut())
            .find(|t| matches!(t.kind, TabKind::Search { req, .. } if req == id))
        else {
            return;
        };
        tab.loading = false;
        match result {
            Ok(hits) => {
                tab.set_listing(crate::tab::hits_listing(hits));
                tab.error = None;
                self.search_reason = None;
            }
            Err(e) => {
                let text = crate::search_tab::banner(&format!("{e:#}"));
                // Only "not running" is global; a bad query stays on its tab.
                if text == crate::search_tab::NOT_RUNNING {
                    self.search_reason = Some(text.clone());
                }
                tab.error = Some(text);
            }
        }
    }

    /// Runs the due query of search tab `t` in pane `p`.
    fn start_search(&mut self, p: usize, t: usize) {
        let tab = &mut self.panes[p].tabs[t];
        let TabKind::Search { query, due, req } = &mut tab.kind else {
            return;
        };
        let Some(searcher) = self.searcher.clone() else {
            // Still loading: try again shortly.
            *due = Some(Instant::now() + DEBOUNCE);
            self.ctx.request_repaint_after(DEBOUNCE);
            return;
        };
        *due = None;
        self.next_req += 1;
        *req = self.next_req;
        let text = query.trim().to_owned();
        if text.is_empty() {
            tab.set_listing(Listing::new(Vec::new()));
            tab.loading = false;
            return;
        }
        tab.loading = true;
        worker::spawn_search(
            searcher,
            text,
            self.next_req,
            self.tx.clone(),
            self.ctx.clone(),
        );
    }

    /// Builds the Ctrl+P folder index on a worker (or once the searcher has loaded).
    fn index_folders(&mut self) {
        if self.jump.indexing {
            return;
        }
        let Some(searcher) = self.searcher.clone() else {
            self.jump.waiting = true;
            return;
        };
        self.jump.indexing = true;
        if self.jump.indexed_at.is_none() {
            self.toasts.info("Indexing folders…");
        }
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        worker::spawn("keel-index", move || {
            let items = crate::jump::build_index(searcher.as_ref());
            worker::send(&tx, &ctx, Msg::JumpIndexed(items));
        });
    }

    /// Asks the searcher again whether it works (status bar click).
    pub fn probe_search(&self) {
        if let Some(s) = self.searcher.clone() {
            worker::spawn_probe(s, self.tx.clone(), self.ctx.clone());
        }
    }

    /// The entry the preview panel shows: the active pane's cursor, else its first target.
    pub fn preview_target(&mut self) -> Option<Entry> {
        let show_hidden = self.show_hidden;
        let tab = self.tab_mut(self.active);
        tab.visible(show_hidden);
        tab.cursor_pos()
            .map(|pos| tab.entries()[tab.visible_cached()[pos]].clone())
            .or_else(|| tab.targets().first().map(|e| (*e).clone()))
    }

    fn start_transfer(&mut self, op: Transfer, conflict: keel_vfs::Conflict, from_clipboard: bool) {
        // `op.src` is what was actually pasted (the system clipboard may have won).
        let cut = (from_clipboard && op.mv).then(|| op.src.clone());
        if cut.is_some() {
            // A cut pastes once, as in Explorer.
            self.clipboard.set(Vec::new(), false);
        }
        let id = self.jobs.start(op, conflict, self.tx.clone());
        if let Some(src) = cut {
            self.cut_job = Some((id, src));
        }
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
                // A search tab keeps the folder it was opened from; never fill it.
                tab.dir == dir && req > tab.listed_req && !tab.is_search()
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
                    tab.left_search = None;
                };
                for &hit in rest {
                    fill(hit, listing.clone());
                }
                // 100k entries: the last (usually only) tab takes the listing itself.
                fill(last, listing);
                let show_hidden = self.show_hidden;
                for &(p, t) in &hits {
                    let tab = &mut self.panes[p].tabs[t];
                    if tab.reveal {
                        tab.visible(show_hidden);
                        tab.reveal_cursor();
                    }
                }
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
                        // Leaving a search failed: it is still the search tab.
                        if let Some(kind) = tab.left_search.take() {
                            tab.kind = kind;
                        }
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
                    let tab = &self.panes[p].tabs[t];
                    if tab.dir == dir && !tab.is_search() {
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
        let mut next_search: Option<Instant> = None;
        for p in 0..2 {
            for t in 0..self.panes[p].tabs.len() {
                if let TabKind::Search { due: Some(at), .. } = self.panes[p].tabs[t].kind {
                    if at <= now {
                        self.start_search(p, t);
                    } else {
                        next_search = Some(next_search.map_or(at, |n| n.min(at)));
                    }
                }
            }
        }
        if let Some(at) = next_search {
            self.ctx
                .request_repaint_after(at.saturating_duration_since(now));
        }
        self.sync_watchers();
    }

    /// One watcher per visible pane, on its active tab's local folder. Created off-thread
    /// because opening a dead network folder can block.
    fn sync_watchers(&mut self) {
        for p in 0..2 {
            let wanted = (p == 0 || self.dual)
                .then(|| self.tab(p))
                .filter(|t| !t.is_search())
                .map(|t| t.dir.clone())
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
                if let TabKind::Search { query, due, .. } = &mut self.tab_mut(p).kind {
                    // Backspace in a search tab edits the query.
                    query.pop();
                    *due = Some(Instant::now() + DEBOUNCE);
                    self.ctx.request_repaint_after(DEBOUNCE);
                } else if self.tab_mut(p).up() {
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
                if let TabKind::Search { query, due, .. } = &mut tab.kind {
                    // Typing in a search tab edits the query.
                    query.push_str(&text);
                    *due = Some(Instant::now() + DEBOUNCE);
                    self.ctx.request_repaint_after(DEBOUNCE);
                } else {
                    tab.filter.push_str(&text);
                    tab.filter_open = true;
                }
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
                let target = tab
                    .targets()
                    .first()
                    .map(|e| (e.name.clone(), tab.shown_name(e).to_owned()));
                if target.is_some() {
                    tab.renaming = target;
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
                let name = if self.theme.dark { "light" } else { "dark" };
                self.settings.theme = name.to_owned();
                self.theme = Theme::load(name);
                self.theme.apply(&self.ctx);
            }
            Action::Settings => self.settings_open = !self.settings_open,
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
            Action::Paste | Action::NewFolder | Action::NewFile if self.tab(p).is_search() => {
                self.toasts
                    .error("Open a folder first (search results have no folder)");
            }
            Action::Paste if self.paste_pending => {
                self.toasts.info("Paste already in progress");
            }
            Action::Paste => match self.tab(p).dir.to_local_path() {
                Some(dst) => {
                    self.paste_pending = jobs::spawn_plan(
                        Source::Clipboard(self.clipboard.clone()),
                        dst,
                        self.tx.clone(),
                        self.ctx.clone(),
                    );
                    if !self.paste_pending {
                        self.toasts.error("Could not start the paste");
                    }
                }
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
                if self.tab(p).is_search() && self.tab(p).dir == dst {
                    return self
                        .toasts
                        .error("Drop onto a folder row, or open a folder first");
                }
                // In-app drags: Shift = move; within one pane a drag into a subfolder moves,
                // like Explorer. Drops from other apps always copy (winit reports no
                // modifiers during an OS drag).
                let shift = self.ctx.input(|i| i.modifiers.shift);
                let mv = from.is_some_and(|(pane, _)| shift || pane == p);
                if !jobs::spawn_plan(
                    Source::Paths(paths, mv),
                    dst_local,
                    self.tx.clone(),
                    self.ctx.clone(),
                ) {
                    self.toasts.error("Could not start the transfer");
                }
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
                // Both fail when the name exists; nothing is ever truncated.
                self.spawn_in_dir(dir, name, move |p, target| {
                    if folder {
                        p.mkdir(target)
                    } else {
                        p.create_new(target).map(drop)
                    }
                });
            }
            Action::Properties => self.toasts.not_yet("Properties"),
            Action::Search => {
                if !self.tab(p).is_search() {
                    let mut tab = Tab::search(self.tab(p).dir.clone());
                    tab.error = self.search_reason.clone();
                    let pane = &mut self.panes[p];
                    pane.tabs.push(tab);
                    pane.active = pane.tabs.len() - 1;
                }
                self.panes[p].focus_filter = true;
            }
            Action::OpenLocation => self.open_location(p),
            Action::JumpFolder => {
                self.jump.show();
                if self.jump.stale() {
                    self.index_folders();
                }
            }
            Action::ReindexFolders => self.index_folders(),
            Action::Palette => {
                let selection = !self.tab(p).targets().is_empty();
                self.palette.show(selection);
            }
            Action::TogglePreview => self.preview.open = !self.preview.open,
        }
    }

    /// Opens the cursor entry's folder with the entry selected: in the other pane, or a
    /// new tab here when single-pane (or when the other pane shows a search).
    fn open_location(&mut self, p: usize) {
        let Some(e) = self.tab(p).targets().first().map(|e| (*e).clone()) else {
            return;
        };
        let Some(dir) = e.path.parent() else { return };
        let q = if self.dual { 1 - p } else { p };
        if !self.dual || self.tab(q).is_search() {
            self.open_tab(q, dir);
        } else {
            self.tab_mut(q).navigate(dir);
            self.list_active(q);
        }
        let name = e.path.name().to_owned();
        let tab = self.tab_mut(q);
        tab.selected = [name.clone()].into();
        tab.cursor = Some(name.clone());
        tab.anchor = Some(name);
        tab.reveal = true;
        self.active = q;
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
    fn second_paste_waits_for_the_first() {
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(egui::Context::default(), Arc::new(Router::new()), dir);
        state.run(0, Action::Paste);
        assert!(state.paste_pending);
        state.run(0, Action::Paste);
        assert!(state
            .toasts
            .list
            .iter()
            .any(|t| t.text == "Paste already in progress"));
        state.apply(Msg::PlanFailed("The clipboard holds no files".into()));
        assert!(!state.paste_pending);
    }

    #[test]
    fn search_tab_is_never_filled_by_its_folder_and_survives_a_failed_exit() {
        let mut router = Router::new();
        router.register(Arc::new(Gone));
        let home = VPath::parse("gone://usb/").unwrap();
        let mut state = AppState::new(egui::Context::default(), Arc::new(router), home.clone());
        state.run(0, Action::Search);
        let query = |s: &AppState| match &s.tab(0).kind {
            TabKind::Search { query, .. } => Some(query.clone()),
            TabKind::Dir => None,
        };
        if let TabKind::Search { query, .. } = &mut state.tab_mut(0).kind {
            query.push_str("ab");
        }
        // A refresh of the folder the search was opened from (watcher, F5, job done).
        state.apply(Msg::Listed {
            dir: home.clone(),
            req: 100,
            result: Ok(Listing::new(vec![test_entry(
                &home,
                "a.txt",
                Kind::File,
                1,
            )])),
        });
        assert!(
            state.tab(0).entries().is_empty(),
            "search results untouched"
        );

        // Leaving the search for a folder that cannot be listed keeps the search tab.
        let locked = home.join("locked");
        state.tab_mut(0).navigate(locked.clone());
        assert!(!state.tab(0).is_search());
        state.apply(Msg::Listed {
            dir: locked,
            req: 101,
            result: Err(anyhow::anyhow!("access denied")),
        });
        assert_eq!(query(&state).as_deref(), Some("ab"));
        assert_eq!(state.tab(0).dir, home);

        // Backspace edits the query instead of leaving the search.
        state.run(0, Action::Up);
        assert_eq!(query(&state).as_deref(), Some("a"));
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
