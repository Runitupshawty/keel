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
use crate::settings::{ArchiveFormat, Settings};
use crate::sidebar::{Drive, Sidebar, DRIVES_REFRESH, DRIVES_TIMEOUT};
use crate::tab::{Listing, Tab, TabKind};
use crate::theme::{Theme, Themes};
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
/// Toast for Compress / Add to inside an archive (they work on files on this computer).
pub const READ_ONLY: &str = "Compress and Add to work on files outside archives";

/// Why nothing can be written in `dir` when it is inside an archive that cannot be
/// changed (7z, tar, RAR, an archive inside an archive or on another provider); None
/// outside archives and in zips on this computer. By name: no file is read.
pub fn archive_refusal(dir: &VPath) -> Option<String> {
    dir.split_archive()?;
    let why = format!("{:#}", keel_vfs::archive::editable(dir).err()?);
    let mut chars = why.chars();
    Some(match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => why,
    })
}
/// Why the newest operation cannot be undone, for the toast after "Can't undo: ".
pub const PERMANENT: &str = "deleted permanently";

fn restore_refusal() -> String {
    format!("restoring from the {}", keel_vfs::trashbin::label())
}

/// A listing that has not answered after this long stops its spinner and says so (a
/// late answer still fills the tab).
pub const LIST_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Phase 14: each drive's kind (removable ones get an eject button).
    DriveKinds(Vec<(String, keel_vfs::DriveKind)>),
    /// Task 51: the installed WSL distributions, the default first.
    WslDistros(Vec<String>),
    /// Task 51: distribution `name` opened (its root) or could not be.
    WslOpened {
        name: String,
        result: anyhow::Result<VPath>,
    },
    Thumb {
        key: PreviewKey,
        preview: keel_preview::Preview,
    },
    Toast(String),
    /// An info toast (Task 85: what a compression saved).
    Note(String),
    /// A bulk rename finished; `items`: what it renamed, for undo (empty when it cannot be
    /// undone).
    BulkDone {
        outcome: crate::bulk_rename::Outcome,
        total: usize,
        items: Vec<crate::undo::Item>,
        /// The undo journal's generation when it started.
        generation: u64,
    },
    /// An operation finished: what it left, for the undo journal; or why it cannot be undone.
    /// Tagged with the journal's generation when it started: after a profile switch it is
    /// dropped.
    Undoable(
        u64,
        Result<(crate::undo::Op, Vec<crate::undo::Item>), String>,
    ),
    /// The check before undoing entry `id`: Ok, or why it cannot be undone now.
    UndoChecked {
        id: u64,
        result: Result<(), crate::undo::Refusal>,
    },
    Preview {
        key: PreviewKey,
        preview: keel_preview::Preview,
    },
    /// Task 59: `library.inspect`'s answer to inspector request `req`.
    Inspected {
        req: u64,
        result: Box<Result<keel_api::types::Inspection, String>>,
    },
    JobProgress {
        id: u64,
        p: keel_vfs::Progress,
    },
    JobDone {
        id: u64,
        result: anyhow::Result<()>,
    },
    /// Task 46: folder totals a Calculate sizes job found (path, modified time, total).
    FolderSizes(Vec<(VPath, Option<std::time::SystemTime>, keel_core::Total)>),
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
    /// Trash items to move into the folder picked for them (macOS Restore to...).
    RestoreTrashTo(Vec<VPath>, std::path::PathBuf),
    /// Stashed items a Move here job moved away: they leave the drop zone.
    StashMoved(Vec<VPath>),
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
    /// Files for Ctrl+P: a batch of a list, `end` when it is complete (and whether it
    /// stopped at its cap).
    JumpFiles {
        origin: crate::jump::Origin,
        gen: u64,
        items: Vec<String>,
        end: Option<bool>,
    },
    Jump {
        id: u64,
        results: Vec<crate::jump::Found>,
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
    /// Path box answer: `path` exists as a file (open its folder, select it) or not.
    Opened {
        pane: usize,
        path: VPath,
        file: bool,
    },
    /// Task 67: what Get Info window `id` asked for with request `req`.
    Properties {
        id: u64,
        req: u64,
        info: Box<Result<crate::dialogs::properties::Props, String>>,
    },
    /// Open with…: the applications the system knows for `paths` (local copies).
    OpenWithApps {
        paths: Vec<std::path::PathBuf>,
        apps: Vec<(String, String)>,
    },
    /// Browse… answered: open `paths` with `app`.
    OpenWithChosen {
        paths: Vec<std::path::PathBuf>,
        app: String,
        remember: bool,
    },
    // --- Task 24 ---
    /// A later `keel` run handed over its command line (single instance).
    External(crate::cli::Request),
    // --- Task 29 ---
    Library(crate::library::LibMsg),
    /// Task 85: Delete permanently, hard-link dedup and the system cleanups.
    System(crate::system_cleanup::SysMsg),
    // --- Task 36 ---
    Devices(crate::devices::DevMsg),
    Mount(crate::mount_ui::MountMsg),
    /// Task 77: a file list read, an export written.
    FileList(crate::filelist_ui::FileListMsg),
    /// An account's storage quota (None: unknown).
    CloudQuota {
        id: String,
        quota: Option<keel_vfs::Quota>,
    },
    /// "Copy link" answered for `path`.
    CloudLink {
        path: VPath,
        result: anyhow::Result<keel_vfs::ShareLink>,
    },
}

/// The folder a watcher was requested for, and the live watcher (held for its `Drop`).
type WatchSlot = (VPath, Option<Box<dyn Any + Send>>);
/// Columns view: at most this many folders of one pane are watched (the tab's own folder
/// and the deepest columns).
const MAX_WATCHED: usize = 8;

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
    /// The floating preview of the file under the pointer.
    pub hover: crate::hover::Hover,
    /// Loaded on a worker at startup (`load_searcher`); None until then.
    pub searcher: Option<Arc<dyn Searcher>>,
    /// Why search does not work (banner text), None when it does.
    pub search_reason: Option<String>,
    pub jump: Jump,
    pub palette: Palette,
    /// The keys as bound now (`sync_keys`), and the keymap they were built from.
    pub keys: Arc<crate::keymap::Bindings>,
    keys_from: crate::keymap::Keymap,
    /// Settings → Keyboard.
    pub keyboard: crate::keymap::KeymapUi,
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
    /// The last finished file operations, for Ctrl+Z (`undo.rs`).
    pub undo: crate::undo::Journal,
    /// Theme, layout and preview options; saved by `settings::Persist`.
    pub settings: Settings,
    pub settings_open: bool,
    pub quick_requested: bool,
    pub search_window: Option<VPath>,
    /// Phase 17 Task 82: the Disk Usage window.
    pub disk_usage: crate::disk_usage::DiskUsage,
    /// SFTP hosts: providers, status, host-key prompts, host editor (`remotes.rs`).
    pub remotes: crate::remotes::Remotes,
    /// Cloud accounts: providers, status, the account wizard (`clouds.rs`).
    pub clouds: crate::clouds::Clouds,
    pub toasts: Toasts,
    pub theme: Theme,
    /// Every theme, read once at startup (and per profile switch).
    pub(crate) themes: Themes,
    /// Settings → Profiles (`profiles.rs`).
    pub profiles: crate::profiles::Profiles,
    /// Settings → Icons and the active icon theme (`icon_theme.rs`).
    pub icon_themes: crate::icon_theme::IconThemes,
    /// This process is the single instance (`single_instance`); None in tests.
    pub instance: Option<crate::single_instance::Server>,
    pub thumbs: Thumbs,
    search: worker::SearchWorker,
    pub tx: Sender<Msg>,
    pub rx: Receiver<Msg>,
    pub ctx: egui::Context,
    /// Per pane: its active tab's folder and, in the columns view, its columns' folders.
    watchers: [Vec<WatchSlot>; 2],
    pending_refresh: HashMap<VPath, Instant>,
    /// Folders being listed, with their request number and start; tabs on them share the
    /// answer.
    inflight: HashMap<VPath, (u64, Instant)>,
    /// `LIST_TIMEOUT` (shorter in tests).
    list_timeout: Duration,
    next_req: u64,
    /// The (tab, folder, view) each visible pane showed last frame; a change relists a
    /// tab that was in the background (it had no watcher).
    shown: [Option<(usize, VPath, crate::pane::ViewMode)>; 2],
    /// Task 66: per pane, the view it had before a folder with "Always open in" switched it
    /// (`view_prefs::view_on_enter`).
    view_before: [Option<crate::pane::ViewMode>; 2],
    /// The drop zone strip and its stash (Task 23).
    pub dropzone: crate::dropzone::DropZone,
    /// Task 29: the library (opened by the app, see `library.rs`).
    pub library: crate::library::LibraryUi,
    /// Task 36: paired devices, shares and Spacedrop (`devices.rs`).
    pub devices: crate::devices::Devices,
    /// Mounts on keel-daemon (`mount_ui.rs`).
    pub mount: crate::mount_ui::MountUi,
    // --- Task 32 ---
    /// Media view sidecar textures and their workers (`media.rs`).
    pub media: crate::media::Media,
    /// The full-window media viewer, while open.
    pub viewer: Option<crate::media_viewer::Viewer>,
    /// Task 46: folder totals walked this session (Calculate sizes).
    pub sizes: crate::sizes::Sizes,
    /// Task 47: the VS Code color theme installer (Settings → General).
    pub color_themes: crate::color_theme::ColorThemes,
    /// Task 67: the open Get Info windows (`dialogs::properties`).
    pub info: crate::dialogs::properties::InfoWindows,
    /// Phase 14 Task 67: Quick Look and Go to Folder.
    pub finder: crate::finder::Finder,
    /// Task 76: bookmarks, filters and search history.
    pub find: crate::bookmarks::Find,
    /// Settings → Search index: the page's edits and the offline roots it marks.
    pub search_page: crate::search_settings::SearchPage,
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
        let themes = Themes::load();
        let theme = themes.get(&settings.theme);
        let thumbs = Thumbs::new(tx.clone(), ctx.clone(), router.clone());
        let mut media = crate::media::Media::new(ctx.clone(), router.clone());
        media.tile = settings.media_tile;
        media.dates = settings.media_dates;
        // Remote hosts are registered before the restored tabs list (lazily connecting).
        let mut remotes = crate::remotes::Remotes::new(tx.clone(), ctx.clone());
        remotes.sync(&router, &settings.remotes);
        // Cloud accounts too; each reads the keychain on its first use, on a worker.
        #[cfg(not(test))]
        let secrets: Arc<dyn keel_vfs::SecretStore> = Arc::new(keel_vfs::cloud::KeyringStore);
        #[cfg(test)]
        let secrets: Arc<dyn keel_vfs::SecretStore> =
            Arc::new(keel_vfs::cloud::MemoryStore::default());
        let mut clouds = crate::clouds::Clouds::new(tx.clone(), ctx.clone(), secrets);
        clouds.sync(&router, &settings.clouds);
        let previewer = worker::spawn_previewer(router.clone(), tx.clone(), ctx.clone());
        let jump = Jump::new(tx.clone(), ctx.clone());
        let library = crate::library::LibraryUi::new(&router, tx.clone(), ctx.clone());
        let devices = crate::devices::Devices::new(tx.clone(), ctx.clone());
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
        let mut panes = [
            panes.next().expect("two panes"),
            panes.next().expect("two panes"),
        ];
        // --- Task 23 ---
        crate::view_columns::restore_session(&mut panes, &session.views, &session.columns);
        let mut preview = PreviewPanel::new(previewer);
        preview.router = Some(router.clone());
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
            hover: Default::default(),
            preview,
            searcher: None,
            search_reason: None,
            jump,
            palette: Palette::default(),
            keys: Arc::new(settings.keymap.bindings()),
            keys_from: settings.keymap.clone(),
            keyboard: Default::default(),
            jobs: Jobs::new(ctx.clone()),
            clipboard: Clipboard::with_writer(tx.clone(), ctx.clone()),
            paste_pending: false,
            cut_job: None,
            archive_clip: None,
            dialog: None,
            undo: Default::default(),
            settings,
            settings_open: false,
            quick_requested: false,
            search_window: None,
            disk_usage: Default::default(),
            remotes,
            clouds,
            toasts: Toasts::default(),
            theme,
            themes,
            profiles: crate::profiles::Profiles::new(ctx.clone()),
            icon_themes: crate::icon_theme::IconThemes::new(ctx.clone()),
            color_themes: crate::color_theme::ColorThemes::new(ctx.clone()),
            info: Default::default(),
            search_page: Default::default(),
            instance: None,
            thumbs,
            search: worker::SearchWorker::new(tx.clone(), ctx.clone()),
            tx,
            rx,
            ctx,
            watchers: Default::default(),
            pending_refresh: HashMap::new(),
            inflight: HashMap::new(),
            list_timeout: LIST_TIMEOUT,
            next_req: 0,
            shown: [None, None],
            view_before: [None, None],
            dropzone: Default::default(),
            library,
            devices,
            mount: Default::default(),
            media,
            viewer: None,
            sizes: Default::default(),
            finder: Default::default(),
            find: Default::default(),
        };
        state.dropzone.set_items(session.stash); // Task 23
        state.jobs.one_per_drive = state.settings.one_transfer_per_drive;
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
    /// Rebuilds the keys when the keymap changed (Settings → Keyboard, an import, a profile
    /// switch) and publishes them for the menus that show shortcuts.
    pub fn sync_keys(&mut self, ctx: &egui::Context) {
        if self.keys_from != self.settings.keymap {
            self.keys_from = self.settings.keymap.clone();
            self.keys = Arc::new(self.keys_from.bindings());
        }
        crate::keymap::publish(ctx, &self.keys);
    }

    pub fn settings_ui(&mut self, ctx: &egui::Context) {
        self.profiles_tick();
        self.mount_tick();
        self.icon_themes.tick(&mut self.settings, &mut self.toasts);
        self.icon_themes.license_modal(ctx);
        // Task 47: a color theme installed or removed: the picker lists what is on disk,
        // and an install switches to its first theme.
        let before = self.settings.theme.clone();
        if let Some(themes) = self.color_themes.tick(&mut self.settings, &mut self.toasts) {
            self.themes = themes;
            if before != self.settings.theme {
                self.theme = self.themes.get(&self.settings.theme);
                self.theme.apply(&self.ctx);
            }
        }
        self.color_themes.license_modal(ctx);
        let s = &mut self.settings;
        s.show_hidden = self.show_hidden;
        s.dual = self.dual;
        s.preview_open = self.preview.open;
        if !self.settings_open {
            self.keyboard.recording = None;
            return;
        }
        let mut find = Vec::new(); // Task 76
        let index_before = s.search_index.clone();
        self.search_page.offline =
            (self.searcher.as_ref()).map_or_else(Vec::new, |x| x.offline_roots());
        let theme_changed = crate::settings::window(
            ctx,
            &mut self.settings_open,
            s,
            &mut self.remotes,
            &mut self.clouds,
            &mut self.profiles,
            &mut self.icon_themes,
            &self.tx,
            self.searcher.as_ref().map(|x| x.name()), // Task 24
            &mut self.library,
            &self.devices,
            &self.themes.list(),
            &mut self.color_themes,
            &mut self.keyboard,
            &mut find,
            &mut self.search_page,
        );
        if s.search_index != index_before {
            crate::search_settings::push(self.searcher.clone(), &s.search_index);
        }
        self.show_hidden = s.show_hidden;
        self.preview.open = s.preview_open;
        self.jobs.one_per_drive = s.one_transfer_per_drive;
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
            self.theme = self.themes.get(&self.settings.theme);
            self.theme.apply(&self.ctx);
        }
        // --- Task 29 ---
        for cmd in std::mem::take(&mut self.library.pending) {
            self.library_cmd(self.active, cmd);
        }
        self.library.apply_hashing(&self.settings.library);
        for cmd in find {
            self.find_cmd(self.active, cmd);
        }
    }

    /// Loads `searcher`, else the platform searcher, off the UI thread (the Everything DLL
    /// load may block).
    pub fn load_searcher(&self, searcher: Option<Arc<dyn Searcher>>) {
        // Before any index opens: they journal what they change.
        keel_search::journal::configure(&self.settings.search_index.journal);
        worker::spawn_searcher(
            searcher,
            self.settings.search_index.clone(),
            self.tx.clone(),
            self.ctx.clone(),
        );
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
        if matches!(tab.kind, TabKind::Overview | TabKind::Journal) {
            if tab.kind == TabKind::Journal {
                // F5: read the journal again.
                tab.journal.changed();
            }
            return;
        }
        if let TabKind::Search { due, .. } = &mut tab.kind {
            *due = Some(Instant::now());
            self.ctx.request_repaint();
            return;
        }
        tab.refresh();
        let dir = tab.dir.clone();
        self.spawn_listing(dir);
    }

    /// Lists `dir` unless a listing of it is running. When that listing started at or
    /// before request `since` it may predate a change: another is queued for when it
    /// answers. `since: None` never queues (the running listing will do).
    fn want_listing(&mut self, dir: VPath, since: Option<u64>) {
        match self.inflight.get(&dir) {
            None => self.spawn_listing(dir),
            Some(&(req, _)) if since.is_some_and(|s| req <= s) => {
                self.pending_refresh.insert(dir, Instant::now());
                self.ctx.request_repaint();
            }
            Some(_) => {}
        }
    }

    /// Lists `dir` on a worker unless a listing of it is already in flight.
    fn spawn_listing(&mut self, dir: VPath) {
        if self.inflight.contains_key(&dir) {
            return;
        }
        self.next_req += 1;
        self.inflight
            .insert(dir.clone(), (self.next_req, Instant::now()));
        worker::spawn_list(
            self.router.clone(),
            dir,
            self.next_req,
            self.tx.clone(),
            self.ctx.clone(),
        );
    }

    // --- Task 23 ---
    /// Columns view, visible tabs only: lists columns that were just opened, and relists
    /// the columns on `changed`, a change seen when the last request was `since` (a
    /// listing of it that started at or before then is followed by another). One listing
    /// per folder, however many columns show it.
    fn list_columns(&mut self, changed: Option<(&VPath, u64)>) {
        let mut want: Vec<(VPath, bool)> = Vec::new();
        for p in 0..2 {
            let pane = &mut self.panes[p];
            if !(p == 0 || self.dual) || pane.view != crate::pane::ViewMode::Columns {
                continue;
            }
            for col in pane.tabs[pane.active].columns.cols.iter_mut() {
                let stale = changed.is_some_and(|(d, _)| *d == col.dir);
                if stale {
                    col.refresh();
                } else if !(col.loading && col.listed_dir.is_none() && col.error.is_none()) {
                    continue;
                }
                match want.iter_mut().find(|(d, _)| *d == col.dir) {
                    Some(w) => w.1 |= stale,
                    None => want.push((col.dir.clone(), stale)),
                }
            }
        }
        for (dir, stale) in want {
            let since = changed.filter(|_| stale).map(|(_, s)| s);
            self.want_listing(dir, since);
        }
    }

    /// Columns view: relists every column of pane `p`'s active tab (F5, a finished job)
    /// for a change seen at request `since`.
    fn relist_columns(&mut self, p: usize, since: u64) {
        let pane = &self.panes[p];
        if pane.view != crate::pane::ViewMode::Columns {
            return;
        }
        let dirs: Vec<VPath> = crate::view_columns::chain(&pane.tabs[pane.active]);
        for dir in dirs {
            self.list_columns(Some((&dir, since)));
        }
    }
    // --- end Task 23 ---

    /// Task 66: lists the folders just expanded in the shown tabs' list views.
    fn list_trees(&mut self) {
        let mut want = Vec::new();
        for p in 0..2 {
            if p == 0 || self.dual {
                let pane = &mut self.panes[p];
                want.extend(pane.tabs[pane.active].take_wanted());
            }
        }
        for dir in want {
            self.want_listing(dir, None);
        }
    }

    fn list_active(&mut self, p: usize) {
        let t = self.panes[p].active;
        self.list(p, t);
    }

    /// Like `list`, for when the folder's contents may have changed (F5, a rename, a
    /// finished job) when the last request was `since`: a listing in flight that started
    /// at or before then may predate the change, so another one is queued for when it
    /// answers.
    fn relist_since(&mut self, p: usize, t: usize, since: u64) {
        let dir = self.panes[p].tabs[t].dir.clone();
        if self.inflight.contains_key(&dir) && !self.panes[p].tabs[t].is_search() {
            self.panes[p].tabs[t].refresh();
            self.want_listing(dir, Some(since));
        } else {
            self.list(p, t);
        }
    }

    fn relist_active(&mut self, p: usize, since: u64) {
        let t = self.panes[p].active;
        self.relist_since(p, t, since);
        self.relist_columns(p, since); // Task 23
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
                if let Some(slot) = self.watchers[pane]
                    .iter_mut()
                    .find(|w| w.0 == dir && w.1.is_none())
                {
                    slot.1 = Some(watcher);
                }
            }
            Msg::Drives(drives) => {
                // A harness that set `drives_requested` into the future keeps its own list: the
                // boot's scan can land after the fixture under load (the CI runner's real
                // drives then broke the Tab walk).
                let frozen = (self.sidebar.drives_requested).is_some_and(|at| at > Instant::now());
                if !frozen {
                    self.sidebar.drives = drives;
                }
                self.sidebar.drives_pending = None;
                self.sidebar.drives_stuck = false;
            }
            Msg::DriveKinds(kinds) => self.sidebar.set_kinds(kinds),
            Msg::WslDistros(names) => {
                if !self.sidebar.wsl_frozen {
                    self.sidebar.wsl = names;
                }
            }
            Msg::WslOpened { name, result } => {
                if self.sidebar.wsl_starting.as_ref() == Some(&name) {
                    self.sidebar.wsl_starting = None;
                }
                match result {
                    Ok(root) => self.run(self.active, Action::Navigate(root)),
                    Err(e) => self.toasts.error(format!("Could not open {name}: {e:#}")),
                }
            }
            Msg::Thumb { key, preview } => self.thumbs.insert(&self.ctx, key, preview),
            Msg::Toast(text) => self.toasts.error(text),
            Msg::Note(text) => self.toasts.info(text),
            Msg::BulkDone {
                outcome,
                total,
                mut items,
                generation,
            } => {
                if generation != self.undo.generation() {
                    items.clear(); // started in another profile
                }
                let (text, ok) = crate::bulk_rename::summary(&outcome, total, "Renamed");
                let op = crate::undo::Op::Rename(outcome.done.clone());
                let id = self.undo.record(op, items).map(|e| e.id);
                match (ok, id) {
                    (true, Some(id)) => self.toasts.offer(text, "Undo", Action::UndoEntry(id)),
                    (true, None) => self.toasts.info(text),
                    (false, Some(id)) => {
                        self.toasts.with_action(text, "Undo", Action::UndoEntry(id))
                    }
                    (false, None) => self.toasts.error(text),
                }
                let dirs: std::collections::HashSet<VPath> = outcome
                    .done
                    .iter()
                    .flat_map(|(a, b)| [a.parent(), b.parent()])
                    .flatten()
                    .collect();
                for dir in dirs {
                    self.apply(Msg::Changed { dir });
                }
            }
            // Started in the profile before a switch: not this journal's.
            Msg::Undoable(generation, _) if generation != self.undo.generation() => {}
            Msg::Undoable(_, Err(why)) => self.undo.refuse(why),
            Msg::Undoable(_, Ok((op, items))) => {
                if let Some(e) = self.undo.record(op, items) {
                    let (text, id) = (e.done_text(), e.id);
                    self.toasts.offer(text, "Undo", Action::UndoEntry(id));
                }
            }
            Msg::UndoChecked { id, result } => {
                let Some((rename, preview)) = (self.undo.get(id))
                    .map(|e| (matches!(e.op, crate::undo::Op::Rename(_)), e.preview()))
                else {
                    return;
                };
                match result {
                    Err(r) => {
                        // Nothing was touched. What cannot come back leaves the journal;
                        // what may pass (a place taken, a share offline) stays for later.
                        if !r.transient {
                            self.undo.take(id);
                            self.toasts.drop_action(&Action::UndoEntry(id));
                        }
                        self.toasts.error(format!("Can't undo: {}", r.why));
                    }
                    Ok(()) if rename => {
                        let p = self.active;
                        self.run(p, Action::UndoRun(id));
                    }
                    Ok(()) if self.dialog.is_some() => self
                        .toasts
                        .error("Another dialog is open; nothing was undone"),
                    Ok(()) => {
                        self.dialog = Some(Dialog::Confirm {
                            text: preview,
                            yes: "Undo",
                            on_yes: Action::UndoRun(id),
                        })
                    }
                }
            }
            Msg::JobProgress { id, p } => self.jobs.progress(id, p),
            Msg::FolderSizes(totals) => self.sizes.got(totals),
            Msg::JobDone { id, result } => {
                self.sizes.job_done(id);
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
                self.stash_job_done(id); // Task 23
                self.jobs.finish(id, result);
                // Watchers usually beat us to it; network folders may not have one.
                let since = self.next_req;
                for p in 0..2 {
                    self.relist_active(p, since);
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
            Msg::StashMoved(paths) => self.dropzone.remove_all(&paths),
            Msg::RestoreTrashTo(paths, dst) => {
                self.undo.refuse(restore_refusal());
                self.jobs
                    .trash_op(jobs::TrashOp::RestoreTo(paths, dst), self.tx.clone());
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
                let since = self.next_req;
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
                            tab.reveal = true;
                            self.relist_since(p, t, since);
                        }
                    }
                }
                // --- Task 23 ---: the same in columns on `dir`.
                for col in self
                    .panes
                    .iter_mut()
                    .flat_map(|p| p.tabs.iter_mut())
                    .flat_map(|t| t.columns.cols.iter_mut())
                    .filter(|c| c.dir == dir)
                {
                    col.selected = [name.clone()].into();
                    col.cursor = Some(name.clone());
                    col.anchor = Some(name.clone());
                }
                self.list_columns(Some((&dir, since)));
            }
            Msg::Preview { key, preview } => self.preview.insert(&self.ctx, key, preview),
            Msg::Inspected { req, result } => self.preview.inspector.answered(req, *result),
            Msg::Search { id, result } => self.searched(id, result),
            Msg::Searcher { searcher, reason } => {
                crate::search_settings::push(Some(searcher.clone()), &self.settings.search_index);
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
            Msg::JumpFiles {
                origin,
                gen,
                items,
                end,
            } => self.jump.files(origin, gen, items, end),
            Msg::Jump { id, results } => self.jump.results(id, results),
            Msg::Remote(event) => self.remote_event(event),
            Msg::Download { name, done, total } => self.download_progress(name, done, total),
            Msg::Info(text) => self.toasts.info(text),
            Msg::Opened { pane, path, file } => {
                let pane = if self.dual { pane } else { 0 };
                match path.parent().filter(|_| file) {
                    Some(dir) => self.reveal_in(pane, dir, path.name().to_owned(), false),
                    None => self.run(pane, Action::Navigate(path)),
                }
            }
            Msg::Properties { id, req, info } => self.info_answer(id, req, *info),
            Msg::OpenWithApps { paths, apps } => {
                if self.dialog.is_none() {
                    let ext = ext_of(&paths[0]);
                    let recent = self.settings.open_with.recent(&ext).to_vec();
                    self.dialog = Some(Dialog::OpenWith {
                        paths,
                        recent,
                        apps,
                        remember: true,
                    });
                }
            }
            Msg::OpenWithChosen {
                paths,
                app,
                remember,
            } => self.open_with_app(paths, app, remember),
            // --- Task 24 ---
            Msg::External(req) => self.external(req),
            Msg::Library(msg) => self.library_msg(msg),
            Msg::System(msg) => self.system_msg(msg),
            Msg::Devices(msg) => self.devices_msg(msg),
            Msg::Mount(msg) => self.mount_msg(msg),
            Msg::FileList(msg) => self.filelist_msg(msg),
            Msg::CloudQuota { id, quota } => self.cloud_quota(id, quota),
            Msg::CloudLink { path, result } => self.cloud_link(path, result),
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
        // A remote or file list tab says nothing about the platform search backend.
        let remote = crate::remotes::is_network(&tab.dir) || tab.filelist.is_some();
        match result {
            Ok(hits) => {
                tab.snippets = (hits.iter())
                    .filter_map(|h| Some((h.path.clone(), h.snippet.clone()?)))
                    .collect();
                let mut listing = crate::tab::hits_listing(hits);
                if tab.collection().is_some() {
                    // A collection's own order is its Name order (sorting by another
                    // column still works; Name brings the hand order back).
                    listing.order = (0..listing.entries.len()).collect();
                }
                tab.set_listing(listing);
                tab.error = None;
                if !remote {
                    self.search_reason = None;
                }
            }
            Err(e) => {
                let text = crate::search_tab::banner(&format!("{e:#}"));
                // Only "not running" is global; a bad query stays on its tab.
                if text == crate::i18n::tr("search_tab.not_running") && !remote {
                    self.search_reason = Some(text.clone());
                }
                tab.error = Some(text);
            }
        }
    }

    /// The query of pane `p`'s search tab was edited (typing with the list focused): run
    /// it after the search delay (when searching as you type), from the first page.
    fn search_edited(&mut self, p: usize) {
        let delay = self.settings.search.delay();
        if let TabKind::Search { due, limit, .. } = &mut self.tab_mut(p).kind {
            *limit = 0;
            if let Some(delay) = delay {
                *due = Some(Instant::now() + delay);
                self.ctx.request_repaint_after(delay);
            }
        }
    }

    /// Phase 17 Task 82: the operations a Disk Usage tab asks for, one step at a time
    /// through the library's previews, or the window's own confirmation and runner when
    /// the library is not open (`disk_usage::steps`).
    fn disk_usage_plans(&mut self, p: usize, ops: Vec<keel_core::Op>) {
        if ops.is_empty() {
            return;
        }
        if !self.library.is_open() {
            self.disk_usage.fallback = Some(crate::disk_usage::steps::Fallback::new(ops));
            return;
        }
        if self.library.plan.is_some() || self.disk_usage.steps.active() {
            return self
                .toasts
                .error("Another preview is open: confirm or cancel it first");
        }
        if let Some(op) = self.disk_usage.steps.start(ops) {
            self.run(p, Action::Library(crate::library::LibCmd::Plan(op)));
        }
    }

    /// Every frame: follows the Disk Usage steps' previews and sends the next step.
    pub fn disk_usage_tick(&mut self) {
        let open = self.library.plan.is_some();
        if let Some(why) = self.disk_usage.steps.tick(open) {
            self.toasts.info(why);
        }
        if let Some(op) = self.disk_usage.next_plan.take() {
            if open {
                self.disk_usage.steps.stop();
                return self
                    .toasts
                    .error("Another preview is open: the next steps were not run");
            }
            self.run(
                self.active,
                Action::Library(crate::library::LibCmd::Plan(op)),
            );
        }
    }

    /// Runs the due query of search tab `t` in pane `p`.
    fn start_search(&mut self, p: usize, t: usize) {
        let settings = &self.settings.search;
        let tab = &mut self.panes[p].tabs[t];
        let TabKind::Search {
            query,
            due,
            req,
            toggles,
            limit,
            more,
            within,
        } = &mut tab.kind
        else {
            return;
        };
        // A remote or cloud tab searches names there (remotes.rs); Everything stays local.
        let remote = crate::remotes::is_network(&tab.dir) && !tab.library_search;
        let library = (tab.library_search)
            .then(|| self.library.backend.clone())
            .flatten()
            .map(|b| Arc::new(crate::library::LibSearch(b)) as Arc<dyn Searcher>);
        // Task 77: a file list tab searches the list's entries (an empty query lists them all).
        let filelist = (tab.filelist.clone()).map(|s| s as Arc<dyn Searcher>);
        let searcher = match self.searcher.clone() {
            _ if filelist.is_some() => filelist,
            _ if tab.library_search && library.is_none() => {
                tab.loading = false;
                tab.error = Some("The library is not open".into());
                *due = None;
                return;
            }
            _ if library.is_some() => library,
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
        let replaces = std::mem::replace(req, self.next_req);
        // Task 76: the tab's filter and match toggles; a filter alone also searches.
        use crate::search_filters::Target;
        let target = match &searcher {
            _ if tab.library_search => Target::Library,
            Some(s) => Target::system(s.name()),
            None => Target::Plain,
        };
        let applied = crate::search_filters::apply(
            query,
            &tab.search_filter,
            &self.settings.filters,
            target,
            crate::search_tab::match_toggles(*toggles),
        );
        if applied.text.is_empty() && tab.filelist.is_none() {
            tab.set_listing(Listing::new(Vec::new()));
            tab.loading = false;
            tab.highlight = None;
            return;
        }
        tab.loading = true;
        // Task 76: the toggles (the filter's own included), and the first page or what
        // Load more asked for.
        *limit = settings.limit(*limit);
        *more = *limit < settings.max();
        let mut q = settings.query(
            applied.text.clone(),
            crate::search_tab::toggles_of(applied.matching),
            *limit,
        );
        q.folders_only = applied.folders_only;
        q.within = within.as_ref().and_then(VPath::to_local_path);
        // Task 76: `no-highlight:` terms are found but not highlighted.
        tab.highlight = keel_search::Highlighter::new(&keel_search::Query {
            text: crate::search_view::without_no_highlight(&q.text, q.regex),
            ..q.clone()
        });
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        match searcher {
            Some(searcher) => self.search.search(
                searcher,
                q,
                applied,
                self.settings.omissions.filter(),
                self.next_req,
                replaces,
            ),
            None => crate::remotes::spawn_search(
                self.router.clone(),
                tab.dir.clone(),
                (applied.text, self.settings.omissions.filter()),
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
        // The walk (no search backend) covers the home folder: `home`, which tests set to
        // their fixture.
        let home = self.home.to_local_path();
        worker::spawn("keel-index", move || {
            let items = crate::jump::build_index(searcher.as_ref(), home.as_deref());
            worker::send(&tx, &ctx, Msg::JumpIndexed(items));
        });
    }

    /// Ctrl+P's file lists, each read on a worker when stale (`fresh`: also when not): the
    /// active folder's tree (local folders only) and the library's names.
    fn index_files(&mut self, fresh: bool) {
        let root = self.tab(self.active).dir.to_local_path();
        if fresh || self.jump.tree_stale(root.as_deref()) {
            self.jump.walk_tree(root);
        }
        let backend = self.library.backend.clone().filter(|_| !self.library.lost);
        if let Some(b) = backend.filter(|_| fresh || self.jump.lib_stale()) {
            self.jump.read_library(b);
        }
    }

    /// Opens a Ctrl+P result in pane `p`, or in the other pane (a new tab when single-pane).
    fn jump_to(&mut self, p: usize, path: VPath, file: bool, new_tab: bool, other_pane: bool) {
        let (q, new_tab) = match other_pane {
            true if self.dual => (1 - p, new_tab || self.tab(1 - p).is_search()),
            true => (p, true),
            false => (p, new_tab),
        };
        match path.parent().filter(|_| file) {
            Some(dir) => {
                self.reveal_in(q, dir, path.name().to_owned(), new_tab);
                self.preview.open = true;
            }
            None if new_tab => {
                self.open_tab(q, path);
                self.active = q;
            }
            None => {
                self.active = q;
                self.run(q, Action::Navigate(path));
            }
        }
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
        let stashed = self.dropzone.claim(&op, from_clipboard); // Task 23
                                                                // `Jobs::start` records the transfers that can be undone.
        if let Some(why) = crate::undo::transfer_refusal(&op, conflict) {
            self.undo.refuse(why);
        }
        let id = self
            .jobs
            .start(op, conflict, self.router.clone(), self.tx.clone());
        if let Some(src) = cut {
            self.cut_job = Some((id, src));
        }
        self.stash_job_started(id, stashed);
    }

    /// Task 85: the targets as local paths with whether each is a folder (the folder shown
    /// when nothing is selected); None when one is not on this computer.
    fn local_picked(&self, p: usize) -> Option<Vec<crate::cleanups::Picked>> {
        let tab = self.tab(p);
        match tab.targets() {
            t if t.is_empty() => Some(vec![(tab.dir.to_local_path()?, true)]),
            t => t
                .iter()
                .map(|e| Some((e.path.to_local_path()?, e.kind == keel_vfs::Kind::Dir)))
                .collect(),
        }
    }

    pub(crate) fn target_paths(&self, p: usize) -> Vec<VPath> {
        self.tab(p)
            .targets()
            .iter()
            .map(|e| e.path.clone())
            .collect()
    }

    /// Runs a provider call for `dir` off the UI thread; success selects `name` and, with
    /// `undo`, records what it did (a rename, or the new item at `name`).
    fn spawn_in_dir(
        &self,
        dir: VPath,
        name: String,
        undo: Option<crate::undo::Op>,
        f: impl FnOnce(&dyn keel_vfs::Provider, &VPath) -> anyhow::Result<()> + Send + 'static,
    ) {
        let (router, tx, ctx) = (self.router.clone(), self.tx.clone(), self.ctx.clone());
        let generation = self.undo.generation();
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
            if let (Msg::Select { .. }, Some(op)) = (&msg, undo) {
                let stat = crate::undo::stat_fn(&router);
                let items = match &op {
                    crate::undo::Op::Rename(steps) => crate::undo::renamed(steps, stat),
                    _ => crate::undo::created(&target, stat),
                };
                worker::send(&tx, &ctx, Msg::Undoable(generation, Ok((op, items))));
            }
            worker::send(&tx, &ctx, msg);
        });
    }

    /// Task 46: walks the subfolders of pane `p`'s folder for their totals, as a job (only
    /// those without one unless `fresh`).
    fn calculate_sizes(&mut self, p: usize, fresh: bool) {
        let tab = self.panes[p].tab();
        if tab.is_search() || tab.is_trash() || tab.kind != TabKind::Dir {
            if fresh {
                self.toasts.info("Calculate sizes works in a folder");
            }
            return;
        }
        let dir = tab.dir.clone();
        let folders = self.sizes.wanted(tab.entries(), fresh);
        if folders.is_empty() {
            self.sizes.walking(&dir, &[], 0);
            return;
        }
        let id =
            self.jobs
                .folder_sizes(&dir, folders.clone(), self.router.clone(), self.tx.clone());
        self.sizes.walking(&dir, &folders, id);
    }

    fn listed(&mut self, dir: VPath, req: u64, gone: bool, result: anyhow::Result<Listing>) {
        if self.inflight.get(&dir).map(|(r, _)| *r) == Some(req) {
            self.inflight.remove(&dir);
        }
        let still_loading = self.inflight.contains_key(&dir);
        // Task 66: folders expanded in a list view take their listing.
        crate::list_view::fill_trees(&mut self.panes, &dir, &result);
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
        // --- Task 23 ---: columns on `dir` take the listing too.
        let Some(result) = crate::view_columns::fill(
            &mut self.panes,
            &dir,
            req,
            still_loading,
            result,
            hits.is_empty(),
        ) else {
            return;
        };
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
                // Task 46: a local folder outside the library gets its folder sizes on its
                // own with the setting on.
                self.sizes.listed(&dir);
                // Task 67: the entries' Finder tags (macOS).
                let tab = &self.panes[last.0].tabs[last.1];
                crate::tag_dots::listed(&self.tx, &self.ctx, tab.entries());
                if crate::sizes::auto_start(&dir, &self.library, self.settings.folder_sizes_local) {
                    if let Some(&(p, _)) = hits.iter().find(|&&(p, t)| self.panes[p].active == t) {
                        self.calculate_sizes(p, false);
                    }
                }
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

    /// Listings past `list_timeout`: stop waiting (spinner off, say so). The in-flight
    /// slot is freed, so a refresh asks again; a late answer still fills the tab.
    fn expire_listings(&mut self, now: Instant) {
        let stuck: Vec<VPath> = self
            .inflight
            .iter()
            .filter(|(_, (_, at))| now.saturating_duration_since(*at) >= self.list_timeout)
            .map(|(d, _)| d.clone())
            .collect();
        for dir in stuck {
            self.inflight.remove(&dir);
            let stop = |tab: &mut Tab| {
                if tab.dir == dir && tab.loading && !tab.is_search() {
                    tab.loading = false;
                    tab.error = Some(format!("{} is not responding", dir.display()));
                }
            };
            for tab in self.panes.iter_mut().flat_map(|p| p.tabs.iter_mut()) {
                stop(tab);
                // Columns too: a column left loading would be asked again every tick.
                tab.columns.cols.iter_mut().for_each(stop);
            }
        }
        if let Some(next) = self.inflight.values().map(|(_, at)| *at).min() {
            self.ctx
                .request_repaint_after((next + self.list_timeout).saturating_duration_since(now));
        }
    }

    /// A tab brought to the front (tab click, dual pane switched back on) had no watcher
    /// while hidden: relist it.
    fn refresh_shown_tabs(&mut self) {
        for p in 0..2 {
            // The tab's own folder (Task 23: not its keyboard column).
            let t = self.panes[p].active;
            // Task 66: a folder shown anew opens in its "Always open in" view.
            let tab = &self.panes[p].tabs[t];
            let entered = (p == 0 || self.dual)
                && tab.kind == crate::tab::TabKind::Dir
                && self.shown[p]
                    .as_ref()
                    .is_none_or(|(st, sd, _)| *st != t || *sd != tab.dir);
            if entered {
                let remembered = self.settings.views.always_open(&tab.dir);
                let current = self.panes[p].view;
                self.panes[p].view =
                    crate::view_prefs::view_on_enter(remembered, current, &mut self.view_before[p]);
            }
            let view = self.panes[p].view;
            let now = (p == 0 || self.dual).then(|| (t, self.panes[p].tabs[t].dir.clone(), view));
            if now == self.shown[p] {
                continue;
            }
            let before = std::mem::replace(&mut self.shown[p], now.clone());
            let Some((t, dir, _)) = now else { continue };
            // Only the view changed: the tab itself was watched all along.
            let tab_shown = before.is_some_and(|(bt, bd, _)| bt == t && bd == dir);
            let since = self.next_req;
            let tab = &self.panes[p].tabs[t];
            if !tab_shown && !tab.loading && !tab.is_search() && tab.listed_dir.is_some() {
                self.relist_since(p, t, since);
            }
            // Task 23: its columns had no watcher either.
            self.relist_columns(p, since);
        }
    }

    fn tick_drives(&mut self, now: Instant) {
        let sidebar = &mut self.sidebar;
        match sidebar.drives_pending {
            Some(at) if now.saturating_duration_since(at) >= DRIVES_TIMEOUT => {
                sidebar.drives_stuck = true;
            }
            Some(at) => self
                .ctx
                .request_repaint_after((at + DRIVES_TIMEOUT).saturating_duration_since(now)),
            None if sidebar
                .drives_requested
                .is_none_or(|at| at.elapsed() >= DRIVES_REFRESH) =>
            {
                sidebar.drives_requested = Some(now);
                if worker::spawn_drives(self.tx.clone(), self.ctx.clone()) {
                    sidebar.drives_pending = Some(now);
                }
                self.ctx.request_repaint_after(DRIVES_REFRESH);
            }
            None => {}
        }
    }

    /// Per-frame housekeeping: due watcher refreshes, drive list, watchers.
    pub fn tick(&mut self) {
        self.media_tick(); // Task 32
        self.find.history.tick(&self.settings.search_history); // Task 76
        self.remote_tick();
        self.cloud_tick();
        self.devices_tick();
        let cwd = self.panes[self.active].tab().dir.to_local_path();
        self.terminal
            .follow(cwd, &self.settings, &self.tx, &self.ctx);
        let now = Instant::now();
        self.expire_listings(now);
        self.refresh_shown_tabs();
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
            let since = self.next_req;
            for p in 0..2 {
                for t in 0..self.panes[p].tabs.len() {
                    let tab = &self.panes[p].tabs[t];
                    if tab.dir == dir && !tab.is_search() {
                        self.list(p, t);
                    }
                }
            }
            self.list_columns(Some((&dir, since))); // Task 23
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
        self.tick_drives(now);
        self.list_columns(None); // Task 23
        self.list_trees(); // Task 66
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
        self.library_tick(); // Task 29
    }

    /// The local folders pane `p` shows: its active tab's and, in the columns view, its
    /// deepest columns' (at most `MAX_WATCHED` in all).
    fn watched_dirs(&self, p: usize) -> Vec<VPath> {
        let pane = &self.panes[p];
        let tab = &pane.tabs[pane.active];
        if !(p == 0 || self.dual) || tab.is_search() {
            return Vec::new();
        }
        let cols: &[Tab] = if pane.view == crate::pane::ViewMode::Columns {
            &tab.columns.cols
        } else {
            &[]
        };
        let deepest = &cols[cols.len().saturating_sub(MAX_WATCHED - 1)..];
        let mut dirs: Vec<VPath> = Vec::new();
        for dir in std::iter::once(&tab.dir).chain(deepest.iter().map(|c| &c.dir)) {
            if dir.to_local_path().is_some() && !dirs.contains(dir) {
                dirs.push(dir.clone());
            }
        }
        dirs
    }

    /// A watcher on every folder a visible pane shows (`watched_dirs`), plus one on each
    /// folder's parent so a rename or delete of the folder itself is noticed (the
    /// folder's own watcher does not report that). Created off-thread because opening a
    /// dead network folder can block.
    fn sync_watchers(&mut self) {
        for p in 0..2 {
            let wanted = self.watched_dirs(p);
            if self.watchers[p].len() == wanted.len()
                && self.watchers[p].iter().zip(&wanted).all(|(w, d)| w.0 == *d)
            {
                continue;
            }
            // Keep the live watchers still wanted; start the others.
            let mut old = std::mem::take(&mut self.watchers[p]);
            for dir in wanted {
                if let Some(i) = old.iter().position(|w| w.0 == dir) {
                    self.watchers[p].push(old.swap_remove(i));
                    continue;
                }
                self.watchers[p].push((dir.clone(), None));
                self.spawn_watcher(p, dir);
            }
        }
    }

    fn spawn_watcher(&self, p: usize, dir: VPath) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::Builder::new()
            .name("keel-watch".into())
            .spawn(move || {
                let local = dir.to_local_path().expect("filtered to local paths");
                let (wtx, wrx) = crossbeam_channel::bounded(1);
                let Ok(watcher) = keel_vfs::watch(&local, wtx) else {
                    return;
                };
                let (ptx, prx) = crossbeam_channel::bounded(1);
                let parent = local
                    .parent()
                    .and_then(|parent| keel_vfs::watch(parent, ptx).ok());
                let prx = if parent.is_some() {
                    prx
                } else {
                    crossbeam_channel::never()
                };
                let _ = tx.send(Msg::Watching {
                    pane: p,
                    dir: dir.clone(),
                    watcher: Box::new((watcher, parent)),
                });
                ctx.request_repaint();
                // Ends when the UI drops the watchers.
                loop {
                    crossbeam_channel::select! {
                        recv(wrx) -> event => if event.is_err() { break },
                        recv(prx) -> event => {
                            // Something next to the folder changed: only its own
                            // disappearance matters here.
                            if event.is_err() || local.try_exists().unwrap_or(true) {
                                continue;
                            }
                        }
                    }
                    let _ = tx.send(Msg::Changed { dir: dir.clone() });
                    ctx.request_repaint();
                }
            })
            .map_err(|e| tracing::error!("spawn watcher: {e}"))
            .ok();
    }

    pub fn run(&mut self, p: usize, action: Action) {
        let action = if action == Action::Delete
            && self.tab(p).is_search()
            && self.settings.omissions.mode
        {
            Action::OmitResults(true)
        } else {
            action
        };
        let p = if self.dual { p } else { 0 };
        // Targets come from the visible rows; make sure they match this listing.
        let show_hidden = self.show_hidden;
        self.tab_mut(p).visible(show_hidden);
        let in_trash = self.tab(p).is_trash();
        if crate::trash_ui::refuses(&action, in_trash) {
            return self.toasts.error(format!(
                "The {} is read-only; restore items to work with them",
                keel_vfs::trashbin::label()
            ));
        }
        if let Some(why) = self.archive_write_refusal(p, &action) {
            return self.toasts.error(why);
        }
        // Phase 14 Task 67: Space (Quick Look), and Quick Look's keys while it is open.
        let Some(action) = crate::finder::intercept(self, p, action) else {
            return;
        };
        // --- Task 32 ---: Space / Enter on a photo or video in the media view.
        let Some(action) = crate::media_viewer::intercept(self, p, action) else {
            return;
        };
        // --- Task 23 ---: columns view navigation.
        let Some(action) = crate::view_columns::intercept(self, p, action) else {
            return;
        };
        // --- Task 29 ---
        let Some(action) = self.library_intercept(p, action) else {
            return;
        };
        match action {
            Action::Backspace if self.tab(p).is_search() => {
                if let TabKind::Search { query, .. } = &mut self.tab_mut(p).kind {
                    // Backspace in a search tab edits the query (Alt+Up does not).
                    query.pop();
                }
                match &self.tab(p).kind {
                    // An emptied pane's search goes back to its folder, like its emptied box.
                    TabKind::Search {
                        query,
                        within: Some(_),
                        ..
                    } if query.is_empty() => self.run(p, Action::CloseSearch),
                    _ => self.search_edited(p),
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
            Action::Refresh => {
                let since = self.next_req;
                self.relist_active(p, since);
            }
            Action::Navigate(to) => {
                self.tab_mut(p).navigate(to);
                self.list_active(p);
            }
            Action::OpenPath(path) => {
                let (router, tx, ctx) = (self.router.clone(), self.tx.clone(), self.ctx.clone());
                worker::spawn("keel-stat", move || {
                    let file = router
                        .provider_for(&path)
                        .and_then(|provider| provider.stat(&path).ok())
                        .is_some_and(|e| e.kind != Kind::Dir);
                    worker::send(
                        &tx,
                        &ctx,
                        Msg::Opened {
                            pane: p,
                            path,
                            file,
                        },
                    );
                });
            }
            Action::NewTab => {
                let dir = self.tab(p).dir.clone();
                self.open_tab(p, dir);
            }
            Action::NewTabAt(dir) => self.open_tab(p, dir),
            Action::CloseTab => self.panes[p].close_tab(self.panes[p].active),
            // Pane 1 is relisted by `refresh_shown_tabs` when it shows again.
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
                if let TabKind::Search { query, .. } = &mut tab.kind {
                    // Typing in a search tab edits the query.
                    query.push_str(&text);
                    self.search_edited(p);
                } else {
                    tab.filter.push_str(&text);
                    tab.filter_open = true;
                }
                self.panes[p].focus_filter = true;
            }
            // Esc in a pane's search goes back to its folder.
            Action::ClearFilter
                if self.tab(p).search_within().is_some() && self.tab(p).renaming.is_none() =>
            {
                self.run(p, Action::CloseSearch)
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
            Action::ViewOptions => crate::list_view::toggle_options(&self.ctx),
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
                let paths: Vec<String> = match self.settings.quote_copied_paths {
                    true => paths.into_iter().map(|p| format!("\"{p}\"")).collect(),
                    false => paths,
                };
                self.ctx.copy_text(paths.join("\n"));
                self.toasts.info(match paths.len() {
                    1 => "Copied path".to_owned(),
                    n => format!("Copied {n} paths"),
                });
            }
            Action::CopyName => {
                let tab = self.tab(p);
                let names: Vec<String> = match tab.targets() {
                    t if t.is_empty() => vec![tab.dir.name().to_owned()],
                    t => t.iter().map(|e| tab.shown_name(e).to_owned()).collect(),
                };
                self.ctx.copy_text(names.join("\n"));
                self.toasts.info(match names.len() {
                    1 => "Copied name".to_owned(),
                    n => format!("Copied {n} names"),
                });
            }
            // --- Task 76 ---
            Action::SearchToggle(t) => {
                if let TabKind::Search {
                    toggles,
                    limit,
                    due,
                    ..
                } = &mut self.tab_mut(p).kind
                {
                    let on = t.of(toggles);
                    *on = !*on;
                    *limit = 0;
                    *due = Some(Instant::now());
                    self.ctx.request_repaint();
                }
            }
            Action::SearchMore => {
                let settings = self.settings.search.clone();
                if let TabKind::Search { limit, due, .. } = &mut self.tab_mut(p).kind {
                    *limit = settings.more(*limit);
                    *due = Some(Instant::now());
                    self.ctx.request_repaint();
                }
            }
            Action::CopyLink(path) => self.copy_link(p, &path),
            Action::OpenWith => {
                // Keel's picker: the system's apps for the first file (a worker lists them).
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                self.with_local_targets(p, move |paths| {
                    let apps = platform::apps_for(&paths[0]).unwrap_or_default();
                    worker::send(&tx, &ctx, Msg::OpenWithApps { paths, apps });
                });
            }
            Action::OpenWithApp {
                paths,
                app,
                remember,
            } => self.open_with_app(paths, app, remember),
            Action::OpenWithBrowse { paths, remember } => {
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                // The OS dialog blocks: never on the UI thread.
                worker::spawn("keel-pick", move || {
                    let picked = platform::file_dialog("Open with", |mut dialog| {
                        if cfg!(windows) {
                            dialog = dialog.add_filter("Programs", &["exe", "bat", "cmd", "com"]);
                        } else if cfg!(target_os = "macos") {
                            dialog = dialog.set_directory("/Applications");
                        }
                        dialog.pick_file()
                    });
                    if let Some(app) = picked {
                        let app = app.to_string_lossy().into_owned();
                        let msg = Msg::OpenWithChosen {
                            paths,
                            app,
                            remember,
                        };
                        worker::send(&tx, &ctx, msg);
                    }
                });
            }
            #[cfg(windows)]
            Action::OpenWithSystem(path) => {
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                worker::spawn("keel-launch", move || {
                    if let Err(e) = platform::open_with_chooser(&path) {
                        worker::send(&tx, &ctx, Msg::Toast(format!("Open with: {e}")));
                    }
                });
            }
            #[cfg(not(windows))]
            Action::OpenWithSystem(_) => {}
            Action::OpenWithRecent(app) => {
                if let Some(ext) = self.tab(p).targets().first().map(|e| e.ext.clone()) {
                    self.settings.open_with.remember(&ext, &app);
                }
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                self.with_local_targets(p, move |paths| {
                    if let Err(e) = platform::open_with(&app, &paths) {
                        let label = platform::app_label(&app);
                        worker::send(&tx, &ctx, Msg::Toast(format!("{label}: {e}")));
                    }
                });
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
            Action::CalculateSizes { fresh } => self.calculate_sizes(p, fresh),
            Action::View(mode) => self.panes[p].view = mode,
            // Task 66: "Always open in" the pane's view for its folder, or no longer.
            Action::ToggleAlwaysOpen => {
                let view = self.panes[p].view;
                let dir = self.panes[p].tabs[self.panes[p].active].dir.clone();
                let views = &mut self.settings.views;
                let on = views.always_open(&dir) != Some(view);
                views.set_always_open(&dir, on.then_some(view));
                let name = crate::view_prefs::view_name(view);
                self.toasts.info(if on {
                    format!("This folder always opens in the {name} view")
                } else {
                    format!("This folder no longer always opens in the {name} view")
                });
            }
            Action::ToggleTheme => {
                let name = if self.theme.dark { "light" } else { "dark" };
                self.settings.theme = name.to_owned();
                self.theme = self.themes.get(name);
                self.theme.apply(&self.ctx);
            }
            Action::Settings => self.settings_open = !self.settings_open,
            Action::NewWindow => {
                if let Err(e) = crate::single_instance::new_window() {
                    self.toasts
                        .error(format!("Could not start a new window: {e}"));
                }
            }
            // --- Task 85 ---
            Action::CleanupSettings => {
                self.settings_open = true;
                self.remotes.page = crate::settings::Page::Cleanups;
            }
            Action::RunCleanup(i) => {
                let Some(c) = self.settings.cleanups.get(i).cloned() else {
                    return;
                };
                let Some(picked) = self.local_picked(p) else {
                    return self
                        .toasts
                        .error("Cleanups run on files and folders on this computer only");
                };
                match crate::cleanups::prepare(i, &c, &picked) {
                    Err(why) => self.toasts.error(why),
                    // The expanded commands are shown before the first run, and every
                    // time with "Ask before running".
                    Ok(prep) if c.ask || !c.confirmed => {
                        self.dialog = Some(Dialog::Cleanup(Box::new(prep)));
                    }
                    Ok(prep) => self.run(p, Action::CleanupConfirmed(Box::new(prep))),
                }
            }
            Action::CleanupConfirmed(prep) => {
                if let Some(c) = (self.settings.cleanups.get_mut(prep.index))
                    .filter(|c| c.command == prep.cleanup.command)
                {
                    c.confirmed = true;
                }
                self.jobs.cleanup(*prep, self.tx.clone());
            }
            Action::Compress => {
                let Some(picked) = self.local_picked(p) else {
                    return self
                        .toasts
                        .error("Only files and folders on this computer can be compressed");
                };
                if let Some((first, _)) = picked
                    .first()
                    .filter(|(f, _)| !keel_vfs::compress::supported(f))
                {
                    return self.toasts.error(format!(
                        "{} is not on a drive with NTFS compression",
                        first.display()
                    ));
                }
                let (paths, dirs): (Vec<_>, Vec<_>) = picked.into_iter().unzip();
                let d = crate::compress_ui::CompressDialog::new(paths, &dirs, &self.ctx);
                self.dialog = Some(Dialog::Compress(Box::new(d)));
            }
            Action::CompressStart(start) => {
                self.jobs.compress(*start, self.tx.clone());
            }
            Action::DiskUsage(crate::disk_usage::DuCmd::ScanCurrent) => {
                match self.tab(p).dir.to_local_path() {
                    Some(dir) if !self.tab(p).is_search() => {
                        let root = keel_vfs::du::Root::Path(dir);
                        self.disk_usage.start(vec![root]);
                    }
                    _ => self
                        .toasts
                        .error("Disk usage scans local folders and drives only"),
                }
            }
            Action::DiskUsage(crate::disk_usage::DuCmd::Plans(ops)) => {
                self.disk_usage_plans(p, ops)
            }
            Action::DiskUsage(cmd) => self.disk_usage.command(cmd, &self.sidebar.drives),
            Action::SwitchProfile(name) => self.profiles.switch(&name),
            Action::Session(cmd) => self.session_cmd(cmd),
            Action::RenameTo { from, to } => {
                if let Some(why) = dialogs::invalid_name(&to) {
                    return self.toasts.error(why);
                }
                let Some(dir) = from.parent() else { return };
                let undo = match crate::undo::place_refusal(&dir, true) {
                    Some(why) => {
                        self.undo.refuse(why);
                        None
                    }
                    None => Some(crate::undo::Op::Rename(vec![(from.clone(), dir.join(&to))])),
                };
                self.spawn_in_dir(dir, to, undo, move |p, target| p.rename(&from, target));
            }
            Action::BulkRename => {
                let tab = self.tab(p);
                let targets = tab.targets();
                if targets.is_empty() {
                    return self.toasts.info("Select the items to rename");
                }
                let picked: std::collections::HashSet<&VPath> =
                    targets.iter().map(|e| &e.path).collect();
                let items: Vec<crate::bulk_rename::Item> = targets
                    .iter()
                    .map(|e| crate::bulk_rename::Item {
                        path: e.path.clone(),
                        name: tab.shown_name(e).to_owned(),
                        is_dir: e.kind == Kind::Dir,
                        modified: e.modified,
                    })
                    .collect();
                // Untouched siblings a new name could collide with (a search tab lists
                // many folders; a clash there is caught by the rename itself).
                let others: Vec<(VPath, String)> = tab
                    .entries()
                    .iter()
                    .filter(|e| !picked.contains(&e.path))
                    .filter_map(|e| Some((e.path.parent()?, tab.shown_name(e).to_owned())))
                    .collect();
                self.dialog = Some(Dialog::BulkRename(Box::new(
                    crate::bulk_rename::BulkRename::new(items, others),
                )));
            }
            Action::BulkRenameApply { renames } => {
                if let Some(why) = renames.iter().find_map(|(_, n)| dialogs::invalid_name(n)) {
                    return self.toasts.error(why);
                }
                let (router, tx, ctx) = (self.router.clone(), self.tx.clone(), self.ctx.clone());
                let total = renames.len();
                let refused = (renames.iter())
                    .find_map(|(f, _)| crate::undo::place_refusal(&f.parent()?, true));
                if let Some(why) = refused {
                    self.undo.refuse(why);
                }
                let generation = self.undo.generation();
                worker::spawn("keel-bulk-rename", move || {
                    let rename = |a: &VPath, b: &VPath| {
                        router
                            .provider_for(a)
                            .ok_or_else(|| anyhow::anyhow!("no provider for {}", a.display()))
                            .and_then(|p| p.rename(a, b))
                    };
                    let fold = cfg!(any(windows, target_os = "macos"));
                    let outcome = crate::bulk_rename::execute(&renames, fold, rename);
                    let items = match refused {
                        Some(_) => Vec::new(),
                        None => crate::undo::renamed(&outcome.done, crate::undo::stat_fn(&router)),
                    };
                    worker::send(
                        &tx,
                        &ctx,
                        Msg::BulkDone {
                            outcome,
                            total,
                            items,
                            generation,
                        },
                    );
                });
            }
            Action::EnableUpdateCheck => {
                self.settings.updates.check = true;
                self.toasts
                    .info("Keel checks for updates once a day (Settings → General)");
            }
            Action::Undo => match self.undo.next() {
                Ok(id) => self.run(p, Action::UndoEntry(id)),
                Err(text) => self.toasts.info(text),
            },
            Action::UndoEntry(id) => {
                let Some(e) = self.undo.get(id).cloned() else {
                    return self.toasts.info("That operation can no longer be undone");
                };
                // Stats may block (a sleeping disk): the check runs on a worker, the
                // preview (or a rename's undo) follows its answer.
                let (router, tx, ctx) = (self.router.clone(), self.tx.clone(), self.ctx.clone());
                worker::spawn("keel-undo-check", move || {
                    let result = e.check(crate::undo::stat_fn(&router));
                    worker::send(&tx, &ctx, Msg::UndoChecked { id, result });
                });
            }
            Action::UndoRun(id) => {
                self.toasts.drop_action(&Action::UndoEntry(id));
                if let Some(e) = self.undo.take(id) {
                    self.jobs.undo(e, self.router.clone(), self.tx.clone());
                }
            }
            Action::Delete => {
                let paths = self.target_paths(p);
                if paths
                    .first()
                    .is_some_and(|p| p.scheme == keel_vfs::trashbin::SCHEME)
                {
                    self.dialog = Some(Dialog::Confirm {
                        text: crate::trash_ui::delete_text(paths.len()),
                        yes: "Delete permanently",
                        on_yes: Action::PurgeTrash(paths),
                    });
                    return;
                }
                if let Some((archive, _)) = paths.first().and_then(VPath::split_archive) {
                    // Inside a zip: gone for good, and the archive is written again.
                    self.dialog = Some(Dialog::Confirm {
                        text: format!(
                            "Delete {} from {} for good? The archive is written again without them.",
                            jobs::items(paths.len()),
                            archive.name()
                        ),
                        yes: "Delete",
                        on_yes: Action::DeleteRemote(paths),
                    });
                    return;
                }
                let remote = paths
                    .first()
                    .filter(|p| keel_vfs::is_remote_scheme(&p.scheme));
                let cloud = paths.first().filter(|p| p.scheme == "cloud");
                let cloud = cloud.map(|p| (p, self.clouds.account(&p.authority)));
                if let Some((path, account)) = cloud {
                    // Never the local "trash" wording for a cloud the app no longer knows.
                    let Some(account) = account else {
                        let why = format!("{}: unknown cloud account", path.display());
                        return self.toasts.error(why);
                    };
                    // Each service deletes differently: say what this one does.
                    let kind = account.kind.remove_kind();
                    self.dialog = Some(Dialog::Confirm {
                        text: crate::clouds::delete_text(paths.len(), kind),
                        yes: crate::clouds::delete_button(kind),
                        on_yes: Action::DeleteRemote(paths),
                    });
                } else if let Some(host) = remote.map(|p| p.authority.clone()) {
                    // No trash on a remote host: say so, once per batch.
                    let label = self.remotes.label(&host).to_owned();
                    self.dialog = Some(Dialog::Confirm {
                        text: crate::remotes::delete_text(paths.len(), &label),
                        yes: "Delete",
                        on_yes: Action::DeleteRemote(paths),
                    });
                } else if let Some(distro) = crate::wsl::one_distro(&paths) {
                    // Task 51: WSL folders have no Recycle Bin; say the delete is for good.
                    self.dialog = Some(Dialog::Confirm {
                        text: crate::wsl::delete_text(paths.len(), &distro),
                        yes: "Delete",
                        on_yes: Action::DeleteRemote(paths),
                    });
                } else if !paths.is_empty() {
                    self.dialog = Some(Dialog::Confirm {
                        text: format!("Move {} to the trash?", jobs::items(paths.len())),
                        yes: "Move to trash",
                        on_yes: Action::Trash(paths),
                    });
                }
            }
            Action::RestoreTrash if !keel_vfs::trashbin::KNOWS_ORIGIN => {
                let paths = self.target_paths(p);
                if !paths.is_empty() {
                    // The Trash does not say where items came from: ask. The OS dialog
                    // blocks, so never on the UI thread.
                    let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                    let home = directories::UserDirs::new().map(|u| u.home_dir().to_path_buf());
                    worker::spawn("keel-pick", move || {
                        let picked = platform::file_dialog("Restore to", |mut dialog| {
                            if let Some(home) = &home {
                                dialog = dialog.set_directory(home);
                            }
                            dialog.pick_folder()
                        });
                        if let Some(dst) = picked {
                            worker::send(&tx, &ctx, Msg::RestoreTrashTo(paths, dst));
                        }
                    });
                }
            }
            Action::RestoreTrash => {
                let paths = self.target_paths(p);
                if !paths.is_empty() {
                    self.undo.refuse(restore_refusal());
                    self.jobs
                        .trash_op(jobs::TrashOp::Restore(paths), self.tx.clone());
                }
            }
            Action::PurgeTrash(paths) => {
                self.undo.refuse(PERMANENT);
                self.jobs
                    .trash_op(jobs::TrashOp::Purge(paths), self.tx.clone());
            }
            Action::EmptyTrash => {
                let n = self.tab(p).entries().len();
                if n == 0 {
                    self.toasts
                        .info(format!("The {} is empty", keel_vfs::trashbin::label()));
                } else {
                    self.dialog = Some(Dialog::Confirm {
                        text: crate::trash_ui::empty_text(n),
                        yes: "Empty",
                        on_yes: Action::EmptyTrashNow,
                    });
                }
            }
            Action::EmptyTrashNow => {
                self.undo.refuse(PERMANENT);
                self.jobs.trash_op(jobs::TrashOp::Empty, self.tx.clone());
            }
            Action::DeleteRemote(paths) => {
                let zip = paths.iter().any(|p| p.split_archive().is_some());
                self.undo.refuse(if zip {
                    "changes inside archives"
                } else {
                    PERMANENT
                });
                self.jobs
                    .delete(paths, self.router.clone(), self.tx.clone());
            }
            Action::OpenWsl(name) => {
                // One at a time: a second click while one starts waits for it.
                if self.sidebar.wsl_starting.is_none() {
                    let open = self.sidebar.wsl_open;
                    if worker::spawn_wsl(name.clone(), open, self.tx.clone(), self.ctx.clone()) {
                        self.sidebar.wsl_starting = Some(name);
                    }
                }
            }
            Action::Remote { host, cmd } => self.remote_cmd(host, cmd),
            Action::System(cmd) => self.system_cmd(p, cmd),
            Action::Cloud { id, cmd } => self.cloud_cmd(id, cmd),
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
                let mv = crate::pane::drop_moves(from.as_ref().map(|(pane, _)| *pane), p, shift);
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
                let undo = match crate::undo::place_refusal(&dir, false) {
                    Some(why) => {
                        self.undo.refuse(why);
                        None
                    }
                    None => Some(crate::undo::Op::Create(folder)),
                };
                // Both fail when the name exists; nothing is ever truncated.
                self.spawn_in_dir(dir, name, undo, move |p, target| {
                    if folder {
                        p.mkdir(target)
                    } else {
                        // Providers that place files on flush (SFTP, zips) need it.
                        p.create_new(target).and_then(|mut w| Ok(w.flush()?))
                    }
                });
            }
            Action::Properties => self.open_info(p, false),
            Action::InfoFollow => self.open_info(p, true),
            Action::SetLocked(path, on) => self.set_locked(path, on),
            Action::ShellProperties(path) => {
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                worker::spawn("keel-shell-props", move || {
                    #[cfg(windows)]
                    let result = keel_vfs::shell::properties(&path);
                    #[cfg(not(windows))]
                    let result: anyhow::Result<()> = {
                        let _ = &path;
                        Err(anyhow::anyhow!("only on Windows"))
                    };
                    if let Err(e) = result {
                        worker::send(&tx, &ctx, Msg::Toast(format!("Properties: {e:#}")));
                    }
                });
            }
            Action::QuickSearch => self.quick_requested = true,
            Action::ToggleOnTop => self.settings.always_on_top = !self.settings.always_on_top,
            Action::ManageOmissions => self.settings.omissions.manage = true,
            Action::ShowOmitted => {
                self.settings.omissions.show_omitted = !self.settings.omissions.show_omitted;
                self.refresh_omissions();
            }
            Action::OmitResults(temporary) => {
                if self.tab(p).is_search() {
                    let paths = self
                        .tab(p)
                        .targets()
                        .iter()
                        .map(|e| e.path.clone())
                        .collect();
                    self.settings.omissions.add(paths, temporary);
                    self.refresh_omissions();
                }
            }
            Action::Search
                if self.settings.searches_in_windows && !self.tab(p).is_open_search() =>
            {
                self.search_window = Some(self.tab(p).dir.clone());
            }
            Action::Search => {
                if !self.tab(p).is_open_search() {
                    let mut tab = Tab::search(self.tab(p).dir.clone());
                    if let TabKind::Search { toggles, .. } = &mut tab.kind {
                        *toggles = self.settings.search.toggles;
                    }
                    if !crate::remotes::is_network(&tab.dir) {
                        tab.error = self.search_reason.clone();
                    }
                    let pane = &mut self.panes[p];
                    pane.tabs.push(tab);
                    pane.active = pane.tabs.len() - 1;
                }
                self.panes[p].focus_filter = true;
            }
            Action::SearchFor(text) => {
                let mut tab = Tab::search(self.tab(p).dir.clone());
                if !crate::remotes::is_network(&tab.dir) {
                    tab.error = self.search_reason.clone();
                }
                if let TabKind::Search { query, due, .. } = &mut tab.kind {
                    *query = text;
                    *due = Some(Instant::now());
                }
                let pane = &mut self.panes[p];
                pane.tabs.push(tab);
                pane.active = pane.tabs.len() - 1;
                self.ctx.request_repaint();
            }
            Action::SearchIn(text) => {
                let dir = self.tab(p).dir.clone();
                let mut tab = Tab::search(dir.clone());
                if !crate::remotes::is_network(&dir) {
                    tab.error = self.search_reason.clone();
                }
                if let TabKind::Search {
                    query,
                    toggles,
                    within,
                    ..
                } = &mut tab.kind
                {
                    *query = text;
                    *toggles = self.settings.search.toggles;
                    *within = Some(dir);
                }
                let pane = &mut self.panes[p];
                pane.active += 1;
                pane.tabs.insert(pane.active, tab);
                pane.focus_filter = true;
                self.search_edited(p);
            }
            Action::CloseSearch => {
                if let Some(dir) = self.tab(p).search_within().cloned() {
                    let pane = &mut self.panes[p];
                    let i = pane.active;
                    let folder = |t: &Tab| t.kind == TabKind::Dir && t.dir == dir;
                    match i.checked_sub(1).filter(|&f| folder(&pane.tabs[f])) {
                        Some(f) => {
                            pane.close_tab(i);
                            pane.active = f;
                        }
                        // Its folder tab was closed, moved or went elsewhere: this tab
                        // shows the folder.
                        None => self.run(p, Action::Navigate(dir)),
                    }
                }
            }
            Action::OpenLocation => self.open_location(p),
            Action::OpenJournal => {
                let pane = &mut self.panes[p];
                match pane.tabs.iter().position(|t| t.kind == TabKind::Journal) {
                    Some(t) => pane.active = t,
                    None => {
                        pane.tabs.push(Tab::new(crate::tab::journal_path()));
                        pane.active = pane.tabs.len() - 1;
                    }
                }
                self.active = p;
            }
            Action::JumpFolder => {
                self.jump.filter = self.settings.jump_filter;
                self.jump.show();
                if self.jump.stale() {
                    self.index_folders();
                }
                self.index_files(false);
            }
            Action::JumpTo {
                path,
                file,
                new_tab,
                other_pane,
            } => self.jump_to(p, path, file, new_tab, other_pane),
            Action::ReindexFolders => {
                self.index_folders();
                self.index_files(true);
            }
            Action::Palette => {
                let selection = !self.tab(p).targets().is_empty();
                self.palette.undo = self.undo.label();
                self.palette.search = self.tab(p).is_search();
                self.palette.cleanups = (self.settings.cleanups.iter())
                    .map(|c| c.title.clone())
                    .collect();
                self.palette.collections =
                    (self.library.is_open()).then(|| self.library.collections.clone());
                // The "Shell: …" entries follow on the first "shell" typed (palette_shell).
                self.palette.show(selection, &self.keys);
            }
            #[cfg(windows)]
            Action::ShellMenu { extended, keyboard } => self.shell_menu(p, extended, keyboard),
            #[cfg(windows)]
            Action::ShellCommand(id) => self.shell_command(id),
            Action::TogglePreview => self.preview.toggle(crate::inspector::PanelTab::Preview),
            Action::ToggleInspector => self.preview.toggle(crate::inspector::PanelTab::Inspector),
            Action::ExtractHere
            | Action::ExtractToFolder
            | Action::ExtractTo
            | Action::ExtractToOther => self.extract_targets(p, action),
            Action::Extract { src, dst } => {
                // A drag that ends where it started (inside the archive): nothing to do.
                if dst.split_archive().is_some() {
                    return;
                }
                self.plan_extract(src, dst);
            }
            Action::ConvertImage => {
                let paths = self.target_paths(p);
                if paths.is_empty()
                    || self
                        .tab(p)
                        .targets()
                        .iter()
                        .any(|e| e.kind == keel_vfs::Kind::Dir)
                    || !paths.iter().all(crate::image_convert::supported)
                {
                    return self
                        .toasts
                        .error("Select local images supported by the image decoder");
                }
                let src = paths.iter().filter_map(VPath::to_local_path).collect();
                self.dialog = Some(Dialog::ConvertImage(
                    crate::image_convert::ConvertDialog::new(src),
                ));
            }
            Action::ConvertImages { src, format, max } => {
                self.undo.refuse("image conversion");
                self.jobs.convert_images(src, format, max, self.tx.clone());
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
                    let fmt = self.settings.archive.default_format;
                    self.dialog = Some(Dialog::ZipName {
                        dir,
                        src,
                        text: fmt.apply(&name, ArchiveFormat::Zip),
                        focus: true,
                        format: fmt,
                        edited: false,
                    });
                }
            }
            Action::ZipTo { zip, src } => {
                let name = zip.file_name().map(|n| n.to_string_lossy().into_owned());
                if let Some(why) = dialogs::invalid_name(&name.unwrap_or_default()) {
                    return self.toasts.error(why);
                }
                self.undo.refuse("adding to an archive");
                self.jobs.add_to_zip(zip, src, self.tx.clone());
            }
            Action::CompressTo { zip, src, format } => {
                self.settings.archive.default_format = format;
                self.run(p, Action::ZipTo { zip, src });
            }
            Action::AddToArchive => {
                let targets = self.tab(p).targets();
                let Some((archive, others)) = jobs::add_target(&targets) else {
                    return self
                        .toasts
                        .error("Select one zip, 7z, tar or tar.gz and the items to add beside it");
                };
                let (archive, src) = (
                    archive.path.clone(),
                    others.iter().map(|e| e.path.clone()).collect(),
                );
                self.confirm_add(archive, src);
            }
            Action::AddTo { archive, src } => self.confirm_add(archive, src),
            // --- Task 23 ---
            Action::ToggleDropZone
            | Action::StashSelection
            | Action::Stash(_)
            | Action::Unstash(_)
            | Action::StashPaste { .. }
            | Action::ClearStash => crate::dropzone::run(self, p, action),
            // Handled by `library_intercept`.
            Action::Library(cmd) => self.library_cmd(p, cmd),
            Action::Devices(cmd) => self.devices_cmd(p, cmd),
            Action::Sidebar(cmd) => self.sidebar_cmd(p, cmd),
            Action::Mount(cmd) => self.mount_cmd(cmd),
            Action::FileList(cmd) => self.filelist_cmd(p, cmd),
            Action::Finder(cmd) => crate::finder::run(self, p, cmd),
            Action::Find(cmd) => self.find_cmd(p, cmd), // Task 76
            Action::Results(cmd) => self.results_cmd(p, cmd), // Task 76
            // --- Task 65 ---
            Action::ToggleSidebar => self.settings.show_sidebar = !self.settings.show_sidebar,
            Action::CustomizeToolbar => crate::toolbar::open_customize(&self.ctx),
            Action::ToolbarSearch(text) => {
                self.run(p, Action::Search);
                let pane = &mut self.panes[p];
                let active = pane.active;
                if let TabKind::Search { query, due, .. } = &mut pane.tabs[active].kind {
                    *query = text;
                    *due = Some(Instant::now());
                }
            }
            Action::FocusTab { pane, tab } => {
                if (pane == 0 || (pane == 1 && self.dual)) && tab < self.panes[pane].tabs.len() {
                    self.active = pane;
                    self.panes[pane].active = tab;
                }
            }
        }
    }

    /// Asks before adding `src` to the local archive `archive`; yes runs `ZipTo`.
    fn confirm_add(&mut self, archive: VPath, src: Vec<VPath>) {
        let (Some(zip), Some(src)) = (
            archive.to_local_path(),
            src.iter()
                .map(VPath::to_local_path)
                .collect::<Option<Vec<_>>>(),
        ) else {
            return self
                .toasts
                .error("Only local files can be added to an archive for now");
        };
        if src.is_empty() {
            return;
        }
        self.dialog = Some(Dialog::Confirm {
            text: format!("Add {} to \"{}\"?", jobs::items(src.len()), archive.name()),
            yes: "Add",
            on_yes: Action::ZipTo { zip, src },
        });
    }

    /// Why a write inside an archive is refused (only zips on this computer can be
    /// changed; Compress and Add to never work inside one). A drag out of an archive that
    /// ends back in its own folder is not a write.
    fn archive_write_refusal(&self, p: usize, action: &Action) -> Option<String> {
        let inside = |dir: &VPath| dir.split_archive().is_some();
        let here = &self.tab(p).dir;
        match action {
            Action::BulkRenameApply { renames } => renames
                .iter()
                .any(|(f, _)| inside(f))
                .then(|| "Bulk rename does not work inside archives: rename one at a time".into()),
            Action::AddToZip | Action::AddToArchive | Action::CompressToZip => {
                inside(here).then(|| READ_ONLY.into())
            }
            Action::Rename
            | Action::Delete
            | Action::Cut
            | Action::Paste
            | Action::NewFolder
            | Action::NewFile => archive_refusal(here),
            Action::RenameTo { from: dir, .. }
            | Action::Create { dir, .. }
            | Action::Drop { dst: dir, .. } => archive_refusal(dir),
            Action::Trash(paths) => paths.iter().find_map(archive_refusal),
            Action::Extract { src, dst }
                if *dst != VPath::join_archive(&src.archive, &src.base) =>
            {
                archive_refusal(dst)
            }
            _ => None,
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
        if action == Action::ExtractToOther {
            let other = 1 - p;
            if !self.dual || self.tab(other).is_search() || self.tab(other).is_trash() {
                return self
                    .toasts
                    .error("Open the folder to extract into in the other pane (Ctrl+Shift+D)");
            }
            let dst = self.tab(other).dir.clone();
            if dst.split_archive().is_some() {
                return self
                    .toasts
                    .error("Extract into a folder outside archives (paste copies into a zip)");
            }
            for archive in archives {
                self.plan_extract(ArchiveSrc::whole(archive), dst.clone());
            }
            return;
        }
        // Next to each archive (search results live in many folders; any provider).
        let Some(folders) = archives
            .iter()
            .map(|a| a.parent().filter(|d| d.split_archive().is_none()))
            .collect::<Option<Vec<_>>>()
        else {
            return self.toasts.error(
                "Extract here works for archives outside archives: use Extract to…, or copy the archive out first",
            );
        };
        if action == Action::ExtractTo {
            let (tx, ctx, router) = (self.tx.clone(), self.ctx.clone(), self.router.clone());
            let start = folders[0]
                .to_local_path()
                .or_else(|| directories::UserDirs::new().map(|u| u.home_dir().to_path_buf()));
            // The OS dialog blocks: never on the UI thread.
            worker::spawn("keel-pick", move || {
                let picked = platform::file_dialog("Extract to", |mut dialog| {
                    if let Some(start) = &start {
                        dialog = dialog.set_directory(start);
                    }
                    dialog.pick_folder()
                });
                let Some(dst) = picked else {
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
            self.plan_extract(ArchiveSrc::whole(archive), dst);
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
        let new_tab = !self.dual || self.tab(q).is_search();
        self.reveal_in(q, dir, e.path.name().to_owned(), new_tab);
    }

    /// Shows folder `dir` in pane `q` (a new tab, or the active one) with `name` selected
    /// and scrolled into view. A folder the tab already shows is not left: its filter is
    /// cleared and the row revealed at once.
    pub(crate) fn reveal_in(&mut self, q: usize, dir: VPath, name: String, new_tab: bool) {
        let here = !new_tab && self.tab(q).dir == dir && !self.tab(q).is_search();
        if new_tab {
            self.open_tab(q, dir);
        } else if !here {
            let t = self.panes[q].active;
            crate::view_columns::collapse(&mut self.panes[q].tabs[t]); // Task 23
            self.tab_mut(q).navigate(dir);
            self.list_active(q);
        }
        let show_hidden = self.show_hidden;
        let tab = self.tab_mut(q);
        tab.selected = [name.clone()].into();
        tab.cursor = Some(name.clone());
        tab.anchor = Some(name);
        tab.reveal = true;
        if here {
            tab.filter.clear();
            tab.filter_open = false;
            tab.visible(show_hidden);
            tab.reveal_cursor();
        }
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
        } else if e.kind == Kind::Dir && e.path.scheme == keel_vfs::trashbin::SCHEME {
            self.toasts
                .info(format!("Restore {} to open it", e.path.name()));
        } else if e.kind == Kind::Dir {
            self.run(p, Action::Navigate(e.path));
        } else if jobs::is_archive_file(&e) {
            self.run(p, Action::Navigate(VPath::join_archive(&e.path, "")));
        } else if let Some(list) = (e.ext == "efu").then(|| e.path.to_local_path()).flatten() {
            // Task 77: an EFU file list opens as a file list tab.
            self.filelist_cmd(p, crate::filelist_ui::FileListCmd::Open(Some(list)));
        } else {
            self.launch(e.path, platform::open);
        }
    }

    /// Runs `f` on a worker with local copies of the selected files (folders are skipped).
    /// Nothing happens when no file is selected.
    fn with_local_targets(
        &self,
        p: usize,
        f: impl FnOnce(Vec<std::path::PathBuf>) + Send + 'static,
    ) {
        let files: Vec<VPath> = (self.tab(p).targets().iter())
            .filter(|e| e.kind != Kind::Dir)
            .map(|e| e.path.clone())
            .collect();
        if files.is_empty() {
            return;
        }
        let (tx, ctx, router) = (self.tx.clone(), self.ctx.clone(), self.router.clone());
        worker::spawn("keel-open-with", move || {
            let mut local = Vec::new();
            for path in &files {
                match crate::remotes::materialise(&router, path, &tx, &ctx) {
                    Ok(l) => local.push(l),
                    Err(e) => {
                        let msg = Msg::Toast(format!("{}: {e:#}", path.display()));
                        return worker::send(&tx, &ctx, msg);
                    }
                }
            }
            f(local);
        });
    }

    /// Opens `paths` with `app` on a worker; `remember` files it under their extension.
    fn open_with_app(&mut self, paths: Vec<std::path::PathBuf>, app: String, remember: bool) {
        let Some(first) = paths.first() else { return };
        if remember {
            self.settings.open_with.remember(&ext_of(first), &app);
        }
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        worker::spawn("keel-launch", move || {
            if let Err(e) = platform::open_with(&app, &paths) {
                let label = platform::app_label(&app);
                worker::send(&tx, &ctx, Msg::Toast(format!("{label}: {e}")));
            }
        });
    }

    pub(crate) fn launch(&self, path: VPath, f: fn(&std::path::Path) -> std::io::Result<()>) {
        worker::spawn_local(
            self.router.clone(),
            path,
            self.tx.clone(),
            self.ctx.clone(),
            f,
        );
    }
}

/// Lowercase extension without the dot ("" when none), as `Entry::ext`.
fn ext_of(path: &std::path::Path) -> String {
    path.extension()
        .map_or_else(String::new, |e| e.to_string_lossy().to_lowercase())
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
        fn list_complete(&self, dir: &VPath) -> anyhow::Result<Vec<Entry>> {
            self.list(dir)
        }
        fn remove_kind(&self) -> keel_vfs::RemoveKind {
            keel_vfs::RemoveKind::Permanent
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
            TabKind::Dir | TabKind::Overview | TabKind::Journal => None,
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

        // Compress and Add to never work inside an archive; a delete asks first.
        state.tab_mut(0).cursor = Some("b.txt".into());
        for action in [Action::AddToZip, Action::CompressToZip] {
            state.toasts.list.clear();
            state.run(0, action.clone());
            let texts: Vec<&str> = state.toasts.list.iter().map(|t| t.text.as_str()).collect();
            assert_eq!(texts, [READ_ONLY], "{action:?}");
        }
        assert!(state.dialog.is_none());
        state.run(0, Action::Delete);
        match state.dialog.take() {
            Some(Dialog::Confirm {
                text,
                yes,
                on_yes: Action::DeleteRemote(paths),
            }) => {
                assert!(text.contains("from demo.zip for good"), "{text}");
                assert_eq!(yes, "Delete");
                assert_eq!(paths, [state.tab(0).dir.join("b.txt")]);
            }
            _ => panic!("delete confirmation"),
        }
        // Inside an archive inside an archive (or a 7z), every write is refused.
        let nested = VPath::join_archive(&root.join("inner.zip"), "");
        state.toasts.list.clear();
        state.run(
            0,
            Action::Create {
                dir: nested,
                name: "x".into(),
                folder: true,
            },
        );
        assert!(
            state.toasts.list[0].text.contains("inside another archive"),
            "{}",
            state.toasts.list[0].text
        );

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
    fn zip_entries_are_deleted_created_renamed_and_pasted() {
        use std::io::Read;
        let tmp = tempfile_dir("keel-archive-edit");
        let file = demo_zip(&tmp);
        std::fs::write(tmp.join("pasted.txt"), "pasted").unwrap();
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            VPath::local(&tmp),
        );
        settle(&mut state, |s| !s.tab(0).loading);
        let root = VPath::join_archive(&VPath::local(&file), "");
        state.run(0, Action::Navigate(root.clone()));
        settle(&mut state, |s| !s.tab(0).loading);
        let done =
            |s: &AppState, n: usize| s.jobs.list.len() == n && s.jobs.list[n - 1].done.is_some();
        // Delete (confirmed): one job, progress by bytes, the listing follows.
        state.run(0, Action::DeleteRemote(vec![root.join("a.txt")]));
        assert_eq!(state.jobs.list[0].title, "Deleting 1 item from demo.zip");
        settle(&mut state, |s| done(s, 1));
        assert!(state.jobs.list[0].done.as_ref().unwrap().is_ok());
        settle(&mut state, |s| {
            !s.tab(0).loading && s.tab(0).entries().len() == 1
        });
        // A new folder and a new (empty) file, then a rename.
        for (name, folder) in [("made", true), ("note.txt", false)] {
            state.run(
                0,
                Action::Create {
                    dir: root.clone(),
                    name: name.into(),
                    folder,
                },
            );
            settle(&mut state, |s| s.tab(0).cursor.as_deref() == Some(name));
        }
        state.run(
            0,
            Action::RenameTo {
                from: root.join("note.txt"),
                to: "renamed.txt".into(),
            },
        );
        settle(&mut state, |s| {
            s.tab(0).cursor.as_deref() == Some("renamed.txt")
        });
        // A file from outside pasted into a folder of the archive.
        jobs::plan(
            Source::Paths(vec![VPath::local(tmp.join("pasted.txt"))], false),
            root.join("made"),
            &state.router,
            &state.tx,
            &state.ctx,
        );
        settle(&mut state, |s| s.jobs.list.len() == 2);
        settle(&mut state, |s| done(s, 2));
        assert!(state.jobs.list[1].done.as_ref().unwrap().is_ok());
        let provider = state.router.provider_for(&root).unwrap();
        let mut body = String::new();
        provider
            .read(&root.join("made/pasted.txt"))
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "pasted");
        let names: Vec<String> = provider
            .list(&root)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["dir", "made", "renamed.txt"]);
        // Entries cut inside the archive cannot be moved out of it.
        state.toasts.list.clear();
        jobs::plan(
            Source::Paths(vec![root.join("renamed.txt")], true),
            VPath::local(&tmp),
            &state.router,
            &state.tx,
            &state.ctx,
        );
        settle(&mut state, |s| !s.toasts.list.is_empty());
        assert!(state.toasts.list[0]
            .text
            .contains("Moving out of an archive"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn archives_extract_into_a_folder_on_another_provider() {
        use keel_vfs::Provider;
        let tmp = tempfile_dir("keel-archive-remote");
        demo_zip(&tmp);
        let router = Arc::new(Router::new());
        let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
        router.register(mem.clone());
        let remote = VPath::parse("memory://host/inbox").unwrap();
        mem.mkdir(&remote).unwrap();
        mem.put("/inbox/a.txt", "mine");
        let mut state = AppState::new(egui::Context::default(), router, VPath::local(&tmp));
        settle(&mut state, |s| !s.tab(0).loading);
        // The other pane shows the remote folder: Extract to the other pane.
        state.dual = true;
        state.run(1, Action::Navigate(remote.clone()));
        state.tab_mut(0).cursor = Some("demo.zip".into());
        state.run(0, Action::ExtractToOther);
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
                from_clipboard: false,
            },
        );
        settle(&mut state, |s| {
            s.jobs.list.first().is_some_and(|j| j.done.is_some())
        });
        assert!(state.jobs.list[0].done.as_ref().unwrap().is_ok());
        let files: Vec<(String, Vec<u8>)> = mem.files();
        assert_eq!(
            files,
            [
                ("/inbox/a (2).txt".to_owned(), b"alpha".to_vec()),
                ("/inbox/a.txt".to_owned(), b"mine".to_vec()),
                ("/inbox/dir/b.txt".to_owned(), b"beta".to_vec()),
            ]
        );
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
            ..Session::single(folder.clone())
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

    /// A provider whose listing never comes back (a dead network share).
    struct Hang;
    impl Provider for Hang {
        fn scheme(&self) -> &'static str {
            "hang"
        }
        fn caps(&self) -> Caps {
            Caps::default()
        }
        fn list(&self, _: &VPath) -> anyhow::Result<Vec<Entry>> {
            std::thread::sleep(Duration::from_secs(3));
            Ok(Vec::new())
        }
        fn stat(&self, _: &VPath) -> anyhow::Result<Entry> {
            anyhow::bail!("hang")
        }
        fn read(&self, _: &VPath) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
            anyhow::bail!("hang")
        }
        fn write(&self, _: &VPath) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
            anyhow::bail!("hang")
        }
        fn mkdir(&self, _: &VPath) -> anyhow::Result<()> {
            anyhow::bail!("hang")
        }
        fn rename(&self, _: &VPath, _: &VPath) -> anyhow::Result<()> {
            anyhow::bail!("hang")
        }
        fn remove(&self, _: &VPath) -> anyhow::Result<()> {
            anyhow::bail!("hang")
        }
        fn list_complete(&self, dir: &VPath) -> anyhow::Result<Vec<Entry>> {
            self.list(dir)
        }
        fn remove_kind(&self) -> keel_vfs::RemoveKind {
            keel_vfs::RemoveKind::Permanent
        }
        fn local_copy(&self, _: &VPath) -> anyhow::Result<std::path::PathBuf> {
            anyhow::bail!("hang")
        }
    }

    /// Polish backlog: a listing that never returns stops the spinner and says so.
    #[test]
    fn hung_listing_times_out() {
        let router = Router::new();
        router.register(Arc::new(Hang));
        let dir = VPath::parse("hang://nas/share").unwrap();
        let mut state = AppState::new(egui::Context::default(), Arc::new(router), dir);
        assert!(state.tab(0).loading);
        state.list_timeout = Duration::from_millis(50);
        std::thread::sleep(Duration::from_millis(100));
        state.tick();
        let tab = state.tab(0);
        assert!(!tab.loading, "spinner stopped");
        assert!(tab.error.as_deref().unwrap().contains("not responding"));
        assert!(state.inflight.is_empty(), "a refresh may ask again");
    }

    /// Polish backlog: F5 while the folder is being listed queues another listing.
    #[test]
    fn refresh_during_a_listing_lists_again_after_it() {
        let tmp = tempfile_dir("keel-relist");
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            VPath::local(&tmp),
        );
        assert!(state.inflight.contains_key(&VPath::local(&tmp)));
        state.run(0, Action::Refresh);
        assert!(state.pending_refresh.contains_key(&VPath::local(&tmp)));
        // Created after the first listing read the folder: the second one sees it.
        settle(&mut state, |s| {
            !s.inflight.contains_key(&VPath::local(&tmp))
        });
        std::fs::write(tmp.join("late.txt"), "x").unwrap();
        state.tick();
        settle(&mut state, |s| {
            s.tab(0).entries().iter().any(|e| e.name == "late.txt")
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Polish backlog: a background tab (no watcher) is relisted when it comes to the
    /// front, and pane 1 when dual mode comes back.
    #[test]
    fn tabs_brought_to_the_front_are_relisted() {
        let tmp = tempfile_dir("keel-front");
        std::fs::create_dir_all(tmp.join("other")).unwrap();
        let home = VPath::local(&tmp);
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            home.clone(),
        );
        state.dual = false;
        state.run(0, Action::NewTabAt(home.join("other")));
        settle(&mut state, |s| s.panes[0].tabs.iter().all(|t| !t.loading));
        state.tick();
        settle(&mut state, |s| s.inflight.is_empty());
        std::fs::write(tmp.join("new.txt"), "x").unwrap();
        state.panes[0].active = 0; // tab click
        state.tick();
        assert!(state.tab(0).loading, "relisting");
        settle(&mut state, |s| {
            s.tab(0).entries().iter().any(|e| e.name == "new.txt")
        });

        std::fs::write(tmp.join("newer.txt"), "x").unwrap();
        state.run(0, Action::ToggleDual);
        state.tick();
        settle(&mut state, |s| {
            s.tab(1).entries().iter().any(|e| e.name == "newer.txt")
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Polish backlog: renaming the open folder itself is noticed (parent watcher).
    #[test]
    fn renaming_the_open_folder_is_noticed() {
        let tmp = tempfile_dir("keel-parent-watch");
        std::fs::create_dir_all(tmp.join("open")).unwrap();
        let dir = VPath::local(tmp.join("open"));
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            dir.clone(),
        );
        state.dual = false;
        state.tick();
        settle(&mut state, |s| {
            !s.watchers[0].is_empty()
                && s.watchers[0].iter().all(|w| w.1.is_some())
                && !s.tab(0).loading
        });
        std::thread::sleep(Duration::from_millis(200));
        std::fs::rename(tmp.join("open"), tmp.join("renamed")).unwrap();
        settle(&mut state, |s| s.pending_refresh.contains_key(&dir));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Polish backlog: Open location into the folder the other pane already shows
    /// clears its filter and scrolls to the file at once (no relist wait).
    #[test]
    fn open_location_in_the_shown_folder_reveals_at_once() {
        let dir = VPath::parse("gone://usb/").unwrap();
        let router = Router::new();
        router.register(Arc::new(Gone));
        let mut state = AppState::new(egui::Context::default(), Arc::new(router), dir.clone());
        let entries: Vec<Entry> = (0..50)
            .map(|i| test_entry(&dir, &format!("f{i:02}.txt"), Kind::File, 1))
            .collect();
        state.panes[1].tabs[0].set_entries(entries);
        state.panes[1].tabs[0].filter = "zzz".into();
        state.panes[1].tabs[0].filter_open = true;
        // Pane 0: a search tab whose cursor is on f40.txt.
        state.run(0, Action::Search);
        state
            .tab_mut(0)
            .set_listing(crate::tab::hits_listing(vec![keel_search::Hit {
                path: dir.join("f40.txt"),
                is_dir: false,
                size: 1,
                modified: None,
                snippet: None,
            }]));
        state.tab_mut(0).visible(false);
        state.tab_mut(0).cursor = Some(dir.join("f40.txt").display());
        state.run(0, Action::OpenLocation);
        let tab = state.tab(1);
        assert_eq!(state.active, 1);
        assert!(tab.filter.is_empty() && !tab.filter_open);
        assert_eq!(tab.cursor.as_deref(), Some("f40.txt"));
        assert_eq!(
            tab.scroll_to,
            Some(40),
            "scrolled without waiting for a listing"
        );
    }

    /// Polish backlog: a file path typed in the path box opens its folder with the
    /// file selected; a folder path navigates.
    #[test]
    fn typed_file_path_opens_its_folder_and_selects_it() {
        let tmp = tempfile_dir("keel-path-box");
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("sub").join("pick.txt"), "x").unwrap();
        let home = VPath::local(&tmp);
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            home.clone(),
        );
        settle(&mut state, |s| !s.tab(0).loading);
        let typed = crate::pane::parse_path("sub/pick.txt", &home).unwrap();
        state.run(0, Action::OpenPath(typed));
        settle(&mut state, |s| {
            s.tab(0).dir == home.join("sub") && !s.tab(0).loading
        });
        assert_eq!(state.tab(0).cursor.as_deref(), Some("pick.txt"));
        state.run(
            0,
            Action::OpenPath(crate::pane::parse_path("..", &home.join("sub")).unwrap()),
        );
        settle(&mut state, |s| s.tab(0).dir == home);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Polish backlog: a drive list that never answers is shown as stuck and is not
    /// asked again until it answers.
    #[test]
    fn stuck_drive_list_is_reported_once() {
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(egui::Context::default(), Arc::new(Router::new()), dir);
        let long_ago = Instant::now() - DRIVES_TIMEOUT - Duration::from_secs(1);
        state.sidebar.drives_pending = Some(long_ago);
        state.sidebar.drives_requested = Some(long_ago - DRIVES_REFRESH);
        state.tick();
        assert!(state.sidebar.drives_stuck);
        assert_eq!(
            state.sidebar.drives_pending,
            Some(long_ago),
            "no second request"
        );
        state.apply(Msg::Drives(Vec::new()));
        assert!(!state.sidebar.drives_stuck && state.sidebar.drives_pending.is_none());
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

    // --- fixH: columns listing ---
    /// Ticks and applies worker messages for `dur`.
    fn pump(s: &mut AppState, dur: Duration) {
        let until = Instant::now() + dur;
        while Instant::now() < until {
            s.tick();
            if let Ok(msg) = s.rx.recv_timeout(Duration::from_millis(10)) {
                s.apply(msg);
            }
        }
    }

    /// Ticks until `done` (or 10 s).
    fn pump_until(s: &mut AppState, done: impl Fn(&AppState) -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done(s) && Instant::now() < until {
            pump(s, Duration::from_millis(20));
        }
        assert!(done(s), "timed out");
    }

    fn idle(s: &AppState) -> bool {
        s.inflight.is_empty()
            && s.pending_refresh.is_empty()
            && s.panes.iter().all(|p| {
                p.tabs
                    .iter()
                    .flat_map(|t| std::iter::once(t).chain(&t.columns.cols))
                    .all(|t| t.listed_dir.is_some() && !t.loading)
            })
    }

    /// The real watcher on the test folder may still report the files the test just wrote
    /// (FSEvents delivers late): let those listings happen before counting.
    fn drain_watcher(s: &mut AppState) {
        if cfg!(target_os = "macos") {
            pump(s, Duration::from_millis(2500));
            pump_until(s, idle);
        }
    }

    #[test]
    fn a_change_lists_a_folder_once_however_many_columns_show_it() {
        use crate::pane::ViewMode;
        let tmp = tempfile_dir("keel-columns-once");
        std::fs::create_dir_all(tmp.join("a")).unwrap();
        std::fs::write(tmp.join("a").join("f.txt"), "x").unwrap();
        let x = VPath::local(&tmp);
        let a = x.join("a");
        let new = || AppState::new(egui::Context::default(), Arc::new(Router::new()), x.clone());

        // Pane 1's tab is on X\a and pane 0 shows X\a as a column.
        let mut s = new();
        s.dual = true;
        s.panes[0].view = ViewMode::Columns;
        crate::view_columns::restore(&mut s.panes[0].tabs[0], std::slice::from_ref(&a));
        s.run(1, Action::Navigate(a.clone()));
        pump_until(&mut s, idle);
        drain_watcher(&mut s);
        let before = s.next_req;
        s.apply(Msg::Changed { dir: a.clone() });
        pump(&mut s, Duration::from_millis(1200));
        assert!(idle(&s));
        assert_eq!(s.next_req - before, 1, "one listing per change");
        let before = s.next_req;
        s.apply(Msg::Select {
            dir: a.clone(),
            name: "f.txt".into(),
        });
        pump(&mut s, Duration::from_millis(600));
        assert!(idle(&s));
        assert_eq!(s.next_req - before, 1, "one listing per rename");

        // Two columns on one folder, no tab on it.
        let mut s = new();
        s.dual = true;
        for p in 0..2 {
            s.panes[p].view = ViewMode::Columns;
            crate::view_columns::restore(&mut s.panes[p].tabs[0], std::slice::from_ref(&a));
        }
        pump_until(&mut s, idle);
        drain_watcher(&mut s);
        let before = s.next_req;
        s.apply(Msg::Changed { dir: a.clone() });
        pump(&mut s, Duration::from_millis(1200));
        assert!(idle(&s));
        assert_eq!(s.next_req - before, 1, "one listing for both columns");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_hung_column_listing_is_not_asked_again() {
        use crate::pane::ViewMode;
        let root = VPath::local(std::env::temp_dir());
        let hung = VPath::parse("mem://hung/x").unwrap();
        let mut s = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            root.clone(),
        );
        s.dual = false;
        s.panes[0].view = ViewMode::Columns;
        s.shown[0] = Some((0, root, ViewMode::Columns));
        s.panes[0].tabs[0].columns.cols.push(Tab::new(hung.clone()));
        s.list_timeout = Duration::from_secs(1);
        // Its listing started long ago and never answered.
        s.inflight.insert(
            hung.clone(),
            (s.next_req, Instant::now() - Duration::from_secs(60)),
        );
        let before = s.next_req;
        s.tick();
        s.tick();
        let col = &s.panes[0].tabs[0].columns.cols[0];
        assert!(!col.loading);
        assert_eq!(col.error.as_deref(), Some("mem://hung/x is not responding"));
        assert_eq!(s.next_req, before, "no second request");
    }

    #[test]
    fn an_outside_change_in_column_0_shows_while_column_2_has_focus() {
        use crate::pane::ViewMode;
        let tmp = tempfile_dir("keel-columns-watch");
        std::fs::create_dir_all(tmp.join("a").join("b")).unwrap();
        let x = VPath::local(&tmp);
        let mut s = AppState::new(egui::Context::default(), Arc::new(Router::new()), x.clone());
        s.dual = false;
        s.panes[0].view = ViewMode::Columns;
        let chain = [x.join("a"), x.join("a").join("b")];
        crate::view_columns::restore(&mut s.panes[0].tabs[0], &chain);
        assert_eq!(s.panes[0].tabs[0].columns.focus, 2);
        pump_until(&mut s, |s| {
            idle(s) && s.watchers[0].len() == 3 && s.watchers[0].iter().all(|w| w.1.is_some())
        });
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(tmp.join("outside.txt"), "x").unwrap();
        pump_until(&mut s, |s| {
            s.panes[0].tabs[0]
                .entries()
                .iter()
                .any(|e| e.name == "outside.txt")
        });
        assert_eq!(s.panes[0].tabs[0].columns.focus, 2, "focus untouched");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Task 66: "Always open in": a folder switches the pane to its view; leaving it for a
    /// folder without one brings the view back; the palette toggles it for the folder.
    #[test]
    fn a_folder_opens_in_its_always_open_view() {
        use crate::pane::ViewMode;
        let tmp = tempfile_dir("keel-always-open");
        for d in ["photos", "docs"] {
            std::fs::create_dir_all(tmp.join(d)).unwrap();
        }
        let x = VPath::local(&tmp);
        let mut s = AppState::new(egui::Context::default(), Arc::new(Router::new()), x.clone());
        s.settings.views = Default::default();
        let photos = x.join("photos");
        let docs = x.join("docs");
        s.settings
            .views
            .set_always_open(&photos, Some(ViewMode::Gallery));
        s.tick();
        assert_eq!(s.panes[0].view, ViewMode::Details);
        s.run(0, Action::Navigate(photos.clone()));
        s.tick();
        assert_eq!(s.panes[0].view, ViewMode::Gallery);
        s.run(0, Action::Navigate(docs.clone()));
        s.tick();
        assert_eq!(s.panes[0].view, ViewMode::Details, "the view from before");
        s.panes[0].view = ViewMode::Grid;
        s.run(0, Action::ToggleAlwaysOpen);
        assert_eq!(s.settings.views.always_open(&docs), Some(ViewMode::Grid));
        s.run(0, Action::ToggleAlwaysOpen);
        assert_eq!(s.settings.views.always_open(&docs), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
