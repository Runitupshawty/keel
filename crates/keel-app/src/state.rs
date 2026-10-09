//! Application state and the message loop. All IO happens in `worker`; this file only
//! reacts to messages and user actions.

use crate::clipboard::Clipboard;
use crate::dialogs::{self, Dialog};
use crate::jobs::{self, ArchiveSrc, Jobs, Source, Transfer};
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
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Watcher events for a folder are coalesced for this long before relisting.
pub const REFRESH_COALESCE: Duration = Duration::from_millis(200);
/// Toast for writes (delete, rename, new, paste, drop) inside an archive.
pub const READ_ONLY: &str = "Archives are read-only in this version";

/// The outermost archive file of an archive path (`D:/a.zip!/b.7z!/x` -> `D:/a.zip`).
pub fn outermost_archive(dir: &VPath) -> Option<VPath> {
    let mut outer = dir.split_archive()?.0;
    while let Some((parent, _)) = outer.split_archive() {
        outer = parent;
    }
    Some(outer)
}

pub enum Msg {
    TerminalReady {
        generation: u64,
        session: Arc<keel_term::Session>,
        shells: Vec<keel_term::Shell>,
    },
    TerminalChanged(u64),
    TerminalError {
        generation: u64,
        text: String,
    },
    /// Answer to listing request `req` (numbers only grow; older answers lose). `gone`: the
    /// folder is on a fixed local disk and does not exist (an offline share is not gone).
    Listed {
        dir: VPath,
        req: u64,
        gone: bool,
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
    PlanFailed {
        text: String,
        from_clipboard: bool,
    },
    /// Sources of a failed cut-move that still exist: the cut to put back.
    RestoreCut(Vec<VPath>),
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
    // --- remotes (Task 16) ---
    /// A connection status change or host-key prompt from an SFTP provider.
    Remote(keel_vfs::RemoteEvent),
    /// Progress of a large remote file being downloaded for preview / open.
    Download {
        name: String,
        done: u64,
        total: u64,
    },
    /// A notice for an info toast.
    Info(String),
    // --- end remotes ---
}

/// The folder a watcher was requested for, and the live watcher (held for its `Drop`).
type WatchSlot = (Option<VPath>, Option<Box<dyn Any + Send>>);

pub struct AppState {
    pub terminal: crate::term_pane::TermPane,
    pub router: Arc<Router>,
    /// Where a saved tab whose folder is gone, and a crash reset, go.
    pub home: VPath,
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
    cut_job: Option<(u64, Vec<VPath>)>,
    /// Entries copied inside an archive: Ctrl+V extracts them (until files are copied,
    /// here or, on Windows, in another app).
    pub archive_clip: Option<ArchiveSrc>,
    pub dialog: Option<Dialog>,
    /// Theme, layout and preview options; saved by `settings::Persist`.
    pub settings: Settings,
    pub settings_open: bool,
    /// SFTP hosts: providers, status, host-key prompts, host editor (`remotes.rs`).
    pub remotes: crate::remotes::Remotes,
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
        Self::restore(
            ctx,
            router,
            Session::single(start.clone()),
            Settings::default(),
            start,
        )
    }

    /// Opens the tabs of `session` (already repaired, see `Session::repair`) with
    /// `settings` applied, and lists every tab.
    pub fn restore(
        ctx: egui::Context,
        router: Arc<Router>,
        session: Session,
        settings: Settings,
        home: VPath,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let theme = Theme::load(&settings.theme);
        let thumbs = Thumbs::new(tx.clone(), ctx.clone(), router.clone());
        // Remote hosts are registered before the restored tabs list (lazily connecting).
        let mut remotes = crate::remotes::Remotes::new(tx.clone(), ctx.clone());
        remotes.sync(&router, &settings.remotes);
        let previewer = worker::spawn_previewer(
            router.clone(),
            remotes.sftp.clone(),
            tx.clone(),
            ctx.clone(),
        );
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
            terminal: crate::term_pane::TermPane::default(),
            router,
            home,
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
            archive_clip: None,
            dialog: None,
            settings,
            settings_open: false,
            remotes,
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
        let theme_changed =
            crate::settings::window(ctx, &mut self.settings_open, s, &mut self.remotes, &self.tx);
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
            Msg::TerminalReady {
                generation,
                session,
                shells,
            } => self.terminal.ready(generation, session, shells),
            Msg::TerminalChanged(generation) => self.terminal.changed(generation),
            Msg::TerminalError { generation, text } => {
                if self.terminal.error(generation) {
                    self.toasts.error(text);
                }
            }
            Msg::Listed {
                dir,
                req,
                gone,
                result,
            } => self.listed(dir, req, gone, result),
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
                        let router = self.router.clone();
                        worker::spawn("keel-recut", move || {
                            let exists = |p: &VPath| match p.to_local_path() {
                                Some(local) => local.exists(),
                                None => router
                                    .provider_for(p)
                                    .is_some_and(|provider| provider.stat(p).is_ok()),
                            };
                            let left: Vec<VPath> = src.into_iter().filter(exists).collect();
                            worker::send(&tx, &ctx, Msg::RestoreCut(left));
                        });
                    }
                }
                // Remote failures also toast (the job row may be scrolled away).
                if let Err(e) = &result {
                    if self.jobs.is_remote(id) && !jobs::is_cancel(e) {
                        self.toasts.error(format!("{e:#}"));
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
                    && self.clipboard.is_empty()
                    && !self.clipboard.changed_outside()
                {
                    self.clipboard.set_paths(paths, true);
                }
            }
            Msg::PlanFailed {
                text,
                from_clipboard,
            } => {
                // A failed drop must not unblock a clipboard paste still being planned.
                if from_clipboard {
                    self.paste_pending = false;
                }
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
            Msg::Remote(event) => self.remote_event(event),
            Msg::Download { name, done, total } => self.download_progress(name, done, total),
            Msg::Info(text) => self.toasts.info(text),
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
        let remote = tab.dir.scheme == "sftp";
        match result {
            Ok(hits) => {
                tab.set_listing(crate::tab::hits_listing(hits));
                tab.error = None;
                if !remote {
                    self.search_reason = None;
                }
            }
            Err(e) => {
                let text = crate::search_tab::banner(&format!("{e:#}"));
                // Only "not running" is global; a bad query stays on its tab.
                if text == crate::search_tab::NOT_RUNNING && !remote {
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
        // A remote tab searches names on its host (remotes.rs); Everything stays local.
        let remote = tab.dir.scheme == "sftp";
        let searcher = match self.searcher.clone() {
            _ if remote => None,
            Some(s) => Some(s),
            None => {
                // Still loading: try again shortly.
                *due = Some(Instant::now() + DEBOUNCE);
                self.ctx.request_repaint_after(DEBOUNCE);
                return;
            }
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
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        match searcher {
            Some(searcher) => worker::spawn_search(searcher, text, self.next_req, tx, ctx),
            None => crate::remotes::spawn_search(
                self.router.clone(),
                tab.dir.clone(),
                text,
                self.next_req,
                self.remotes.search_gen.clone(),
                tx,
                ctx,
            ),
        }
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
        let id = self
            .jobs
            .start(op, conflict, self.router.clone(), self.tx.clone());
        if let Some(src) = cut {
            self.cut_job = Some((id, src));
        }
    }

    fn target_paths(&self, p: usize) -> Vec<VPath> {
        self.tab(p)
            .targets()
            .iter()
            .map(|e| e.path.clone())
            .collect()
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

    fn listed(&mut self, dir: VPath, req: u64, gone: bool, result: anyhow::Result<Listing>) {
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
                let mut moved = Vec::new();
                for &(p, t) in &hits {
                    let tab = &mut self.panes[p].tabs[t];
                    // A tab that never listed (restored from the session, or opened on a
                    // stale path) whose local folder is gone: open home instead. Network
                    // and unreachable folders keep their tab and only show the error.
                    // An archive that is gone falls back to its folder.
                    if gone && tab.listed_dir.is_none() && !tab.is_search() {
                        let to = outermost_archive(&dir).and_then(|a| a.parent());
                        *tab = Tab::new(to.unwrap_or_else(|| self.home.clone()));
                        moved.push((p, t));
                        continue;
                    }
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
                if moved.is_empty() {
                    self.toasts.error(text);
                } else if let Some(archive) = outermost_archive(&dir) {
                    self.toasts.error(format!(
                        "{} no longer exists; opened its folder",
                        archive.display()
                    ));
                } else {
                    self.toasts.error(format!(
                        "{} no longer exists; opened your home folder",
                        dir.display()
                    ));
                }
                for (p, t) in moved {
                    self.list(p, t);
                }
            }
        }
    }

    /// Per-frame housekeeping: due watcher refreshes, drive list, watchers.
    pub fn tick(&mut self) {
        self.remote_tick();
        let cwd = self.panes[self.active].tab().dir.to_local_path();
        self.terminal
            .follow(cwd, &self.settings, &self.tx, &self.ctx);
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
        if self.writes_into_archive(p, &action) {
            return self.toasts.error(READ_ONLY);
        }
        match action {
            Action::Backspace if self.tab(p).is_search() => {
                if let TabKind::Search { query, due, .. } = &mut self.tab_mut(p).kind {
                    // Backspace in a search tab edits the query (Alt+Up does not).
                    query.pop();
                    *due = Some(Instant::now() + DEBOUNCE);
                    self.ctx.request_repaint_after(DEBOUNCE);
                }
            }
            Action::Up | Action::Backspace => {
                if !self.tab(p).is_search() && self.tab_mut(p).up() {
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
                self.active = p;
                let dir = self.tab(p).dir.clone();
                if let Some(local) = dir.to_local_path() {
                    self.terminal
                        .open_at(local, &self.settings, &self.tx, &self.ctx);
                } else if dir.scheme == "sftp" {
                    self.remote_cmd(dir.authority, crate::remotes::RemoteCmd::Terminal);
                } else {
                    self.toasts.error("Terminal requires a local folder");
                }
            }
            // Focused: hide it, the shell keeps running (only × ends it). Hidden or
            // unfocused: show and focus it. No shell yet: open at the pane, else home.
            Action::ToggleTerminal => {
                if self.terminal.focused(&self.ctx) {
                    self.terminal.hide(&self.ctx);
                } else if self.terminal.reopen(&self.ctx) {
                    // shown again
                } else if let Some(dir) =
                    (self.tab(p).dir.to_local_path()).or_else(|| self.home.to_local_path())
                {
                    self.terminal
                        .open_at(dir, &self.settings, &self.tx, &self.ctx);
                }
            }
            Action::LeaveTerminal => self.terminal.leave(&self.ctx),
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
                let paths = self.target_paths(p);
                let remote = paths.first().filter(|p| p.scheme == "sftp");
                if let Some(host) = remote.map(|p| p.authority.clone()) {
                    // No trash on a remote host: say so, once per batch.
                    let label = self.remotes.label(&host).to_owned();
                    self.dialog = Some(Dialog::Confirm {
                        text: crate::remotes::delete_text(paths.len(), &label),
                        on_yes: Action::DeleteRemote(paths),
                    });
                } else if !paths.is_empty() {
                    self.dialog = Some(Dialog::Confirm {
                        text: format!("Move {} to the trash?", jobs::items(paths.len())),
                        on_yes: Action::Trash(paths),
                    });
                }
            }
            Action::DeleteRemote(paths) => {
                self.jobs
                    .delete(paths, self.router.clone(), self.tx.clone());
            }
            Action::Remote { host, cmd } => self.remote_cmd(host, cmd),
            Action::Trash(paths) => {
                self.jobs
                    .delete(paths, self.router.clone(), self.tx.clone());
            }
            Action::Copy if self.tab(p).dir.split_archive().is_some() => {
                let paths: Vec<VPath> = self
                    .tab(p)
                    .targets()
                    .iter()
                    .map(|e| e.path.clone())
                    .collect();
                if let Some(src) = ArchiveSrc::picked(&self.tab(p).dir, &paths) {
                    if !src.entries.is_empty() {
                        // Nothing stale for Explorer to paste; our paste extracts.
                        self.clipboard.set(Vec::new(), false);
                        self.toasts
                            .info(format!("Copied {}", jobs::items(src.entries.len())));
                        self.archive_clip = Some(src);
                    }
                }
            }
            Action::Copy | Action::Cut => {
                let cut = action == Action::Cut;
                let paths = self.target_paths(p);
                if !paths.is_empty() {
                    self.archive_clip = None;
                    let n = paths.len();
                    self.clipboard.set_paths(paths, cut);
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
            Action::Paste => {
                if self.clipboard.changed_outside() {
                    self.archive_clip = None;
                }
                let source = match &self.archive_clip {
                    Some(src) => Source::Archive {
                        src: src.clone(),
                        clipboard: true,
                    },
                    None => Source::Clipboard(self.clipboard.clone()),
                };
                self.paste_pending = jobs::spawn_plan(
                    source,
                    self.tab(p).dir.clone(),
                    self.router.clone(),
                    self.tx.clone(),
                    self.ctx.clone(),
                );
                if !self.paste_pending {
                    self.toasts.error("Could not start the paste");
                }
            }
            Action::StartTransfer {
                op,
                conflict,
                from_clipboard,
            } => self.start_transfer(op, conflict, from_clipboard),
            Action::Drop { paths, from, dst } => {
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
                    dst,
                    self.router.clone(),
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
                    if tab.dir.scheme != "sftp" {
                        tab.error = self.search_reason.clone();
                    }
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
            Action::ExtractHere | Action::ExtractToFolder | Action::ExtractTo => {
                self.extract_targets(p, action)
            }
            Action::Extract { src, dst } => {
                // A drag that ends where it started (inside the archive): nothing to do.
                if dst.split_archive().is_some() {
                    return;
                }
                self.plan_extract(src, dst);
            }
            Action::AddToZip | Action::CompressToZip => {
                let Some(dir) = self.tab(p).dir.to_local_path() else {
                    return self.toasts.error("Zips can only be made in local folders");
                };
                let Some(src) = self
                    .target_paths(p)
                    .iter()
                    .map(VPath::to_local_path)
                    .collect::<Option<Vec<_>>>()
                else {
                    return self.toasts.error("Only local files can be zipped for now");
                };
                if src.is_empty() {
                    return;
                }
                let name = jobs::zip_name(self.tab(p));
                if action == Action::AddToZip {
                    self.run(
                        p,
                        Action::ZipTo {
                            zip: dir.join(name),
                            src,
                        },
                    );
                } else {
                    self.dialog = Some(Dialog::ZipName {
                        dir,
                        src,
                        text: name,
                        focus: true,
                    });
                }
            }
            Action::ZipTo { zip, src } => {
                let name = zip.file_name().map(|n| n.to_string_lossy().into_owned());
                if let Some(why) = dialogs::invalid_name(&name.unwrap_or_default()) {
                    return self.toasts.error(why);
                }
                self.jobs.add_to_zip(zip, src, self.tx.clone());
            }
            Action::FocusTab { pane, tab } => {
                if (pane == 0 || (pane == 1 && self.dual)) && tab < self.panes[pane].tabs.len() {
                    self.active = pane;
                    self.panes[pane].active = tab;
                }
            }
        }
    }

    /// Writes refused inside archives (read-only in this version). A drag out of an
    /// archive that ends back in its own folder is not a write.
    fn writes_into_archive(&self, p: usize, action: &Action) -> bool {
        let inside = |dir: &VPath| dir.split_archive().is_some();
        match action {
            Action::Rename
            | Action::Delete
            | Action::Cut
            | Action::Paste
            | Action::NewFolder
            | Action::NewFile
            | Action::AddToZip
            | Action::CompressToZip => inside(&self.tab(p).dir),
            Action::RenameTo { from: dir, .. }
            | Action::Create { dir, .. }
            | Action::Drop { dst: dir, .. } => inside(dir),
            Action::Trash(paths) => paths.iter().any(inside),
            Action::Extract { src, dst } => {
                inside(dst) && *dst != VPath::join_archive(&src.archive, &src.base)
            }
            _ => false,
        }
    }

    /// Extract here / to folder / to… for the archive targets.
    fn extract_targets(&mut self, p: usize, action: Action) {
        let archives: Vec<VPath> = self
            .tab(p)
            .targets()
            .into_iter()
            .filter(|e| jobs::is_archive_file(e))
            .map(|e| e.path.clone())
            .collect();
        if archives.is_empty() {
            return self.toasts.error("Select an archive to extract");
        }
        // Next to each archive (search results live in many folders).
        let Some(folders) = archives
            .iter()
            .map(|a| a.parent().and_then(|d| d.to_local_path()))
            .collect::<Option<Vec<_>>>()
        else {
            return self.toasts.error(READ_ONLY);
        };
        if action == Action::ExtractTo {
            let (tx, ctx, router) = (self.tx.clone(), self.ctx.clone(), self.router.clone());
            let start = folders[0].clone();
            // The OS dialog blocks: never on the UI thread.
            worker::spawn("keel-pick", move || {
                let Some(dst) = rfd::FileDialog::new()
                    .set_title("Extract to")
                    .set_directory(&start)
                    .pick_folder()
                else {
                    return;
                };
                for archive in archives {
                    let source = Source::Archive {
                        src: ArchiveSrc::whole(archive),
                        clipboard: false,
                    };
                    jobs::plan(source, VPath::local(&dst), &router, &tx, &ctx);
                }
            });
            return;
        }
        for (archive, folder) in archives.into_iter().zip(folders) {
            let dst = if action == Action::ExtractToFolder {
                folder.join(jobs::archive_stem(archive.name()))
            } else {
                folder
            };
            self.plan_extract(ArchiveSrc::whole(archive), VPath::local(dst));
        }
    }

    fn plan_extract(&mut self, src: ArchiveSrc, dst: VPath) {
        let source = Source::Archive {
            src,
            clipboard: false,
        };
        let (router, tx, ctx) = (self.router.clone(), self.tx.clone(), self.ctx.clone());
        if !jobs::spawn_plan(source, dst, router, tx, ctx) {
            self.toasts.error("Could not start the extraction");
        }
    }

    /// The archive each visible tab browses, once per archive, for the sidebar.
    pub fn open_archives(&self) -> Vec<(usize, usize, VPath)> {
        let mut seen: Vec<(usize, usize, VPath)> = Vec::new();
        for p in 0..if self.dual { 2 } else { 1 } {
            for (t, tab) in self.panes[p].tabs.iter().enumerate() {
                if let Some((archive, _)) = tab.dir.split_archive() {
                    if !seen.iter().any(|(_, _, a)| *a == archive) {
                        seen.push((p, t, archive));
                    }
                }
            }
        }
        seen
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

    /// Folders (and links, which usually point at folders) and archives navigate; files
    /// open in the OS default app.
    pub fn open_entry(&mut self, p: usize, e: Entry) {
        // Links carry their target's kind; a dangling link opens like a file (and fails
        // with the OS message). Archives open as folders, in this tab.
        if e.encrypted {
            self.toasts.error(crate::preview_panel::LOCKED);
        } else if e.kind == Kind::Dir {
            self.run(p, Action::Navigate(e.path));
        } else if jobs::is_archive_file(&e) {
            self.run(p, Action::Navigate(VPath::join_archive(&e.path, "")));
        } else {
            self.launch(e.path, platform::open);
        }
    }

    fn launch(&self, path: VPath, f: fn(&std::path::Path) -> std::io::Result<()>) {
        worker::spawn_local(
            self.router.clone(),
            self.remotes.sftp.clone(),
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
        let router = Router::new();
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
            gone: false,
            result: Ok(Listing::new(good)),
        });
        assert_eq!(state.tab(0).entries().len(), 2);
        // An older answer (A -> B -> A) never replaces it.
        state.apply(Msg::Listed {
            dir: dir.clone(),
            req: good_req - 1,
            gone: false,
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
    fn gone_local_tab_opens_home_but_unreachable_tab_stays() {
        let home = std::env::temp_dir().join(format!("keel-gone-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let home = VPath::local(&home);
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            home.clone(),
        );
        // A saved tab on a deleted local folder: the worker reports it gone.
        let gone = home.join("unplugged");
        state.run(0, Action::NewTabAt(gone.clone()));
        for _ in 0..10 {
            let Ok(msg) = state.rx.recv_timeout(Duration::from_secs(5)) else {
                break;
            };
            state.apply(msg);
            if state.tab(0).dir == home {
                break;
            }
        }
        assert_eq!(state.panes[0].tabs.len(), 2, "tab kept, sent home");
        assert_eq!(state.tab(0).dir, home);
        assert!(state
            .toasts
            .list
            .iter()
            .any(|t| t.text.contains("no longer exists")));

        // An unreachable share keeps its tab and shows the error.
        let offline = VPath::local(std::path::Path::new("/offline-share/projects"));
        state.panes[1].tabs.push(Tab::new(offline.clone()));
        state.apply(Msg::Listed {
            dir: offline.clone(),
            req: 1000,
            gone: false,
            result: Err(anyhow::anyhow!("The network path was not found")),
        });
        let tab = &state.panes[1].tabs[1];
        assert_eq!(tab.dir, offline);
        assert!(tab.error.is_some());
        let _ = std::fs::remove_dir_all(home.to_local_path().unwrap());
    }

    /// m19: Ctrl+` while focused hides the terminal and keeps its shell; × ends it.
    #[test]
    fn toggling_a_focused_terminal_hides_and_keeps_the_shell() {
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(egui::Context::default(), Arc::new(Router::new()), dir);
        state.run(0, Action::ToggleTerminal);
        assert!(state.terminal.open);
        let start = std::time::Instant::now();
        while !state.terminal.running() {
            assert!(start.elapsed() < Duration::from_secs(20), "no shell");
            if let Ok(msg) = state.rx.recv_timeout(Duration::from_millis(100)) {
                state.apply(msg);
            }
        }
        assert!(state.terminal.focused(&state.ctx));
        state.run(0, Action::ToggleTerminal);
        assert!(!state.terminal.open && state.terminal.running());
        state.run(0, Action::ToggleTerminal);
        assert!(state.terminal.open && state.terminal.focused(&state.ctx));
        assert!(state.terminal.running());
        let ctx = state.ctx.clone();
        state.terminal.close(&ctx);
        assert!(!state.terminal.open && !state.terminal.running());
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
        // A drop whose planning fails does not unblock the paste.
        state.apply(Msg::PlanFailed {
            text: "Already in this folder".into(),
            from_clipboard: false,
        });
        assert!(state.paste_pending);
        state.apply(Msg::PlanFailed {
            text: "The clipboard holds no files".into(),
            from_clipboard: true,
        });
        assert!(!state.paste_pending);
    }

    #[test]
    fn search_tab_is_never_filled_by_its_folder_and_survives_a_failed_exit() {
        let router = Router::new();
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
            gone: false,
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
            gone: false,
            result: Err(anyhow::anyhow!("access denied")),
        });
        assert_eq!(query(&state).as_deref(), Some("ab"));
        assert_eq!(state.tab(0).dir, home);

        // Backspace edits the query instead of leaving the search; Alt+Up does neither.
        state.run(0, Action::Backspace);
        assert_eq!(query(&state).as_deref(), Some("a"));
        state.run(0, Action::Up);
        assert_eq!(query(&state).as_deref(), Some("a"));
        assert!(state.tab(0).is_search());
    }

    /// `<tmp>/demo.zip` holding `a.txt` and `dir/b.txt`.
    pub(crate) fn demo_zip(tmp: &std::path::Path) -> std::path::PathBuf {
        use std::io::Write;
        let file = tmp.join("demo.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&file).unwrap());
        for (name, body) in [("a.txt", "alpha"), ("dir/b.txt", "beta")] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        file
    }

    /// Applies worker messages until `done` holds (or 10 s pass).
    fn settle(state: &mut AppState, done: impl Fn(&AppState) -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done(state) && Instant::now() < until {
            if let Ok(msg) = state.rx.recv_timeout(Duration::from_millis(50)) {
                state.apply(msg);
            }
        }
        assert!(done(state), "timed out waiting for workers");
    }

    fn names(state: &mut AppState) -> Vec<String> {
        let tab = state.tab_mut(0);
        let rows = tab.visible(false).to_vec();
        rows.iter()
            .map(|&i| tab.entries()[i].name.clone())
            .collect()
    }

    fn open(state: &mut AppState, name: &str) {
        let e = state
            .tab(0)
            .entries()
            .iter()
            .find(|e| e.name == name)
            .cloned()
            .unwrap();
        state.open_entry(0, e);
    }

    #[test]
    fn archive_opens_as_folder_up_returns_and_writes_are_refused() {
        let tmp = tempfile_dir("keel-archive-browse");
        let file = demo_zip(&tmp);
        let folder = VPath::local(&tmp);
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            folder.clone(),
        );
        settle(&mut state, |s| !s.tab(0).loading);
        open(&mut state, "demo.zip");
        let root = VPath::join_archive(&VPath::local(&file), "");
        assert_eq!(state.tab(0).dir, root);
        assert_eq!(state.tab(0).title(), "demo.zip");
        settle(&mut state, |s| !s.tab(0).loading);
        assert_eq!(names(&mut state), ["dir", "a.txt"]);
        assert_eq!(state.open_archives(), [(0, 0, VPath::local(&file))]);

        open(&mut state, "dir");
        settle(&mut state, |s| !s.tab(0).loading);
        assert_eq!(names(&mut state), ["b.txt"]);
        // Breadcrumb chain: folder › demo.zip › dir.
        let parent = state.tab(0).dir.parent().unwrap();
        assert_eq!(
            (parent.name(), parent.parent()),
            ("demo.zip", Some(folder.clone()))
        );

        // Writes inside the archive are refused with one toast.
        state.tab_mut(0).cursor = Some("b.txt".into());
        for action in [
            Action::Delete,
            Action::Rename,
            Action::NewFolder,
            Action::NewFile,
            Action::Paste,
            Action::Cut,
            Action::AddToZip,
        ] {
            state.toasts.list.clear();
            state.run(0, action.clone());
            let texts: Vec<&str> = state.toasts.list.iter().map(|t| t.text.as_str()).collect();
            assert_eq!(texts, [READ_ONLY], "{action:?}");
        }
        assert!(state.dialog.is_none() && state.tab(0).renaming.is_none());

        state.run(0, Action::Up);
        state.run(0, Action::Up);
        assert_eq!(state.tab(0).dir, folder);
        assert_eq!(state.tab(0).cursor.as_deref(), Some("demo.zip"));
        settle(&mut state, |s| !s.tab(0).loading);
        assert!(state.open_archives().is_empty());
        state.run(0, Action::Back);
        assert_eq!(state.tab(0).dir, root);
        state.run(0, Action::Forward);
        assert_eq!(state.tab(0).dir, folder);

        // A password-protected entry is refused, never opened.
        let mut locked = crate::tab::test_entry(&root, "secret.txt", Kind::File, 1);
        locked.encrypted = true;
        state.toasts.list.clear();
        state.open_entry(0, locked);
        assert_eq!(state.toasts.list[0].text, crate::preview_panel::LOCKED);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn archive_entries_as_plain_paths_plan_an_extraction() {
        // ops::transfer cannot read archives: a paste or drop of in-archive paths extracts.
        let tmp = tempfile_dir("keel-archive-paths");
        let file = demo_zip(&tmp);
        let out = tmp.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("a.txt"), "mine").unwrap();
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            VPath::local(&tmp),
        );
        settle(&mut state, |s| !s.tab(0).loading);
        let root = VPath::join_archive(&VPath::local(&file), "");
        jobs::plan(
            Source::Paths(vec![root.join("a.txt")], false),
            VPath::local(&out),
            &state.router,
            &state.tx,
            &state.ctx,
        );
        settle(&mut state, |s| s.dialog.is_some());
        let Some(Dialog::Conflict { names, op, .. }) = state.dialog.take() else {
            panic!("conflict dialog");
        };
        assert_eq!(names, ["a.txt"]);
        assert_eq!(
            op.extract.map(|e| e.entries),
            Some(vec!["a.txt".to_owned()])
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_plans_conflicts_and_lands_files() {
        let tmp = tempfile_dir("keel-archive-extract");
        let file = demo_zip(&tmp);
        let out = tmp.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("a.txt"), "mine").unwrap();
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            VPath::local(&tmp),
        );
        settle(&mut state, |s| !s.tab(0).loading);
        // Ctrl+C inside the archive, Ctrl+V in `out`: planned on a worker.
        let root = VPath::join_archive(&VPath::local(&file), "");
        let src = ArchiveSrc::picked(&root, &[root.join("a.txt"), root.join("dir")]).unwrap();
        assert_eq!(src.entries, ["a.txt", "dir"]);
        jobs::plan(
            Source::Archive {
                src,
                clipboard: true,
            },
            VPath::local(&out),
            &state.router,
            &state.tx,
            &state.ctx,
        );
        settle(&mut state, |s| s.dialog.is_some());
        let Some(Dialog::Conflict { names, op, .. }) = state.dialog.take() else {
            panic!("conflict dialog");
        };
        assert_eq!(names, ["a.txt"]);
        state.run(
            0,
            Action::StartTransfer {
                op,
                conflict: keel_vfs::Conflict::RenameNew,
                from_clipboard: true,
            },
        );
        assert_eq!(state.jobs.list[0].title, "Extracting demo.zip");
        settle(&mut state, |s| s.jobs.list[0].done.is_some());
        assert!(state.jobs.list[0].done.as_ref().unwrap().is_ok());
        assert_eq!(std::fs::read_to_string(out.join("a.txt")).unwrap(), "mine");
        assert_eq!(
            std::fs::read_to_string(out.join("a (2).txt")).unwrap(),
            "alpha"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("dir/b.txt")).unwrap(),
            "beta"
        );

        // Extract to folder "demo": a new folder next to the archive, no clashes.
        state.tab_mut(0).cursor = Some("demo.zip".into());
        state.run(0, Action::ExtractToFolder);
        settle(&mut state, |s| {
            s.jobs.list.len() == 2 && s.jobs.list[1].done.is_some()
        });
        assert_eq!(
            std::fs::read_to_string(tmp.join("demo/dir/b.txt")).unwrap(),
            "beta"
        );

        // Add to "a.zip" from the extracted file, then browse it.
        state.tab_mut(0).cursor = None;
        state.run(
            0,
            Action::ZipTo {
                zip: tmp.join("new.zip"),
                src: vec![out.join("a.txt")],
            },
        );
        assert_eq!(state.jobs.list[2].title, "Adding to new.zip");
        settle(&mut state, |s| s.jobs.list[2].done.is_some());
        let listed = state
            .router
            .provider_for(&VPath::join_archive(&VPath::local(tmp.join("new.zip")), ""))
            .unwrap()
            .list(&VPath::join_archive(&VPath::local(tmp.join("new.zip")), ""))
            .unwrap();
        assert_eq!(listed[0].name, "a.txt");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn session_reopens_archive_tabs_or_falls_back_to_their_folder() {
        let tmp = tempfile_dir("keel-archive-session");
        let file = demo_zip(&tmp);
        let folder = VPath::local(&tmp);
        let inside = VPath::join_archive(&VPath::local(&file), "dir");
        let saved = Session {
            panes: vec![vec![inside.clone()], vec![folder.clone()]],
            active: 0,
            active_tab: [0, 0],
        };
        let json = tmp.join("session.json");
        saved.save_to(&json).unwrap();
        let restore = || {
            let mut session = Session::load_from(&json).0.unwrap();
            session.repair(&folder);
            AppState::restore(
                egui::Context::default(),
                Arc::new(Router::new()),
                session,
                Settings::default(),
                folder.clone(),
            )
        };
        let mut state = restore();
        assert_eq!(Session::of(&state), saved);
        settle(&mut state, |s| !s.tab(0).loading);
        assert_eq!(names(&mut state), ["b.txt"]);

        std::fs::remove_file(&file).unwrap();
        let mut state = restore();
        settle(&mut state, |s| s.tab(0).dir == folder && !s.tab(0).loading);
        assert!(state.toasts.list[0].text.contains("opened its folder"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn tempfile_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn failed_navigation_returns_to_the_listed_folder() {
        let router = Router::new();
        router.register(Arc::new(Gone));
        let home = VPath::parse("gone://usb/").unwrap();
        let mut state = AppState::new(egui::Context::default(), Arc::new(router), home.clone());
        state.apply(Msg::Listed {
            dir: home.clone(),
            req: 100,
            gone: false,
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
            gone: false,
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
            gone: false,
            result: Err(anyhow::anyhow!("access denied")),
        });
        let tab = state.tab(0);
        assert_eq!(tab.dir, home);
        assert!(tab.future.is_empty());
    }
}
