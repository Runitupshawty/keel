//! Ctrl+P: fuzzy jump to a folder or a file. Folders come from the searcher (Everything
//! `folder:`) or a walk of the home folder; files from a walk of the active folder's tree
//! (at most `WALK_CAP`) and from the library index (at most `keel_core::FILE_NAMES_MAX`).
//! Every list is read on a worker (the walk hands its files over in batches); nucleo
//! matching runs on its own thread, because scoring 200k paths per keystroke would stall
//! the UI.

use crate::a11y::Named;
use crate::backend::LibraryBackend;
use crate::keys::Action;
use crate::state::Msg;
use crate::worker::{send, spawn};
use crossbeam_channel::Sender;
use egui::{Id, Key, Modal, Modifiers};
use keel_search::{Fuzzy, Searcher};
use keel_vfs::VPath;
use std::cmp::Reverse;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub const MAX_RESULTS: usize = 50;
/// The lists are read again on open when older than this (F5 in the popup forces it).
pub const REINDEX_AFTER: Duration = Duration::from_secs(600);
/// Folders kept from the home-folder walk when no searcher works, and files kept from the
/// walk of the active folder's tree.
pub const WALK_CAP: usize = 50_000;
/// The tree walk hands its files over in batches of this many, or whatever it has after
/// `BATCH_AFTER` (a slow network folder).
const BATCH: usize = 2_000;
const BATCH_AFTER: Duration = Duration::from_millis(250);

/// What Ctrl+P lists; Tab cycles through them, the last one is kept per profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum JumpFilter {
    Folders,
    Files,
    #[default]
    Both,
}

impl JumpFilter {
    pub const ALL: [JumpFilter; 3] = [JumpFilter::Folders, JumpFilter::Files, JumpFilter::Both];

    pub fn next(self) -> Self {
        match self {
            JumpFilter::Folders => JumpFilter::Files,
            JumpFilter::Files => JumpFilter::Both,
            JumpFilter::Both => JumpFilter::Folders,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            JumpFilter::Folders => "Folders",
            JumpFilter::Files => "Files",
            JumpFilter::Both => "Both",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            JumpFilter::Folders => "Jump to folder",
            JumpFilter::Files => "Find a file",
            JumpFilter::Both => "Find a folder or file",
        }
    }

    fn wants(self, origin: Origin) -> bool {
        match self {
            JumpFilter::Folders => origin == Origin::Folders,
            JumpFilter::Files => origin != Origin::Folders,
            JumpFilter::Both => true,
        }
    }
}

/// Where a result comes from. On equal scores and lengths the earlier one ranks first: the
/// active folder's tree before the library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    Tree,
    Library,
    Folders,
}

/// One result: a display path, and whether it is a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    pub file: bool,
}

impl Found {
    /// The name, and the folder it is in (empty for a root).
    pub fn split(&self) -> (&str, &str) {
        let path = self.path.trim_end_matches(['/', '\\']);
        match path.rsplit_once(['/', '\\']) {
            Some((dir, name)) if !name.is_empty() => (name, dir),
            _ => (&self.path, ""),
        }
    }

    pub fn vpath(&self) -> VPath {
        match self.path.contains("://") {
            true => VPath::parse(&self.path).unwrap_or_else(|_| VPath::local(&self.path)),
            false => VPath::local(&self.path),
        }
    }
}

/// A file list for Ctrl+P (the tree walk, the library's names) and how far it got.
#[derive(Clone, Debug, Default)]
pub struct FileList {
    /// Files received so far.
    pub count: usize,
    /// Being read now.
    pub busy: bool,
    /// It stopped at its cap.
    pub capped: bool,
    /// When it was last read in full.
    pub at: Option<Instant>,
    /// The tree walked (the tree list only).
    pub root: Option<PathBuf>,
    gen: u64,
}

enum Cmd {
    /// Replaces a list.
    Set(Origin, Vec<String>),
    /// Adds to a list.
    Add(Origin, Vec<String>),
    Search(u64, String, JumpFilter),
}

/// Best first: the higher score, then the shorter path, then `Origin` order (each list's
/// own order after that); a path in two lists (a library file in the walked tree) once.
pub fn rank(mut hits: Vec<(u32, Origin, String)>, max: usize) -> Vec<Found> {
    hits.sort_by_cached_key(|(score, origin, path)| {
        (Reverse(*score), path.chars().count(), *origin)
    });
    let mut seen = HashSet::new();
    hits.into_iter()
        .filter(|(_, _, path)| seen.insert(path.clone()))
        .take(max)
        .map(|(_, origin, path)| Found {
            path,
            file: origin != Origin::Folders,
        })
        .collect()
}

/// How Enter opens a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Open {
    Here,
    NewTab,
    OtherPane,
}

pub struct Jump {
    pub open: bool,
    pub text: String,
    pub results: Vec<Found>,
    pub cursor: usize,
    pub filter: JumpFilter,
    focus: bool,
    /// The matcher thread, which owns the `Fuzzy` lists.
    matcher: Sender<Cmd>,
    tx: Sender<Msg>,
    ctx: egui::Context,
    req: u64,
    /// The request `results` answer.
    answered: u64,
    /// Enter pressed before the results caught up with the text.
    pending_enter: Option<Open>,
    /// Folders: their count, when indexed, being indexed.
    pub indexed: usize,
    pub indexed_at: Option<Instant>,
    pub indexing: bool,
    /// An index was asked for before the searcher finished loading.
    pub waiting: bool,
    /// Files of the active folder's tree, and of the library.
    pub tree: FileList,
    pub lib: FileList,
    /// The tree walk's cap (`WALK_CAP`; tests lower it).
    pub tree_cap: usize,
    /// The running tree walk stops when this moves past its generation.
    walking: Arc<AtomicU64>,
}

impl Jump {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context) -> Self {
        let (matcher, rx) = crossbeam_channel::unbounded::<Cmd>();
        let (out, repaint) = (tx.clone(), ctx.clone());
        spawn("keel-jump", move || {
            let mut lists: [Option<Fuzzy>; 3] = [None, None, None];
            while let Ok(first) = rx.recv() {
                // Only the newest search of a burst is worth matching.
                let mut search = None;
                for cmd in std::iter::once(first).chain(rx.try_iter()) {
                    match cmd {
                        Cmd::Set(o, items) => lists[o as usize] = Some(Fuzzy::new(items)),
                        Cmd::Add(o, items) => match &mut lists[o as usize] {
                            Some(f) => f.push(items),
                            none => *none = Some(Fuzzy::new(items)),
                        },
                        Cmd::Search(id, pat, filter) => search = Some((id, pat, filter)),
                    }
                }
                let Some((id, pat, filter)) = search else {
                    continue;
                };
                let mut hits = Vec::new();
                let origins = [Origin::Tree, Origin::Library, Origin::Folders];
                for (origin, list) in origins.into_iter().zip(&mut lists) {
                    if let Some(f) = list.as_mut().filter(|_| filter.wants(origin)) {
                        let found = f.search(&pat, MAX_RESULTS).into_iter();
                        hits.extend(found.map(|(score, path)| (score, origin, path)));
                    }
                }
                let results = rank(hits, MAX_RESULTS);
                send(&out, &repaint, Msg::Jump { id, results });
            }
        });
        Self {
            open: false,
            text: String::new(),
            results: Vec::new(),
            cursor: 0,
            filter: JumpFilter::default(),
            focus: false,
            matcher,
            tx,
            ctx,
            req: 0,
            answered: 0,
            pending_enter: None,
            indexed: 0,
            indexed_at: None,
            indexing: false,
            waiting: false,
            tree: FileList::default(),
            lib: FileList::default(),
            tree_cap: WALK_CAP,
            walking: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn show(&mut self) {
        self.open = true;
        self.focus = true;
        self.text.clear();
        self.results.clear();
        self.cursor = 0;
        self.pending_enter = None;
        self.search();
    }

    /// The folder list is due for a rebuild.
    pub fn stale(&self) -> bool {
        self.indexed_at
            .is_none_or(|at| at.elapsed() >= REINDEX_AFTER)
    }

    fn search(&mut self) {
        self.req += 1;
        let cmd = Cmd::Search(self.req, self.text.clone(), self.filter);
        let _ = self.matcher.send(cmd);
    }

    pub fn results(&mut self, id: u64, results: Vec<Found>) {
        if id == self.req {
            self.answered = id;
            self.results = results;
            self.cursor = self.cursor.min(self.results.len().saturating_sub(1));
        }
    }

    pub fn indexed(&mut self, items: Vec<String>) {
        self.indexing = false;
        self.indexed = items.len();
        self.indexed_at = Some(Instant::now());
        let _ = self.matcher.send(Cmd::Set(Origin::Folders, items));
        self.search();
    }

    /// The tree list is not of `root`, or due for a rebuild (a walk of `root` running now
    /// is not).
    pub fn tree_stale(&self, root: Option<&Path>) -> bool {
        let t = &self.tree;
        t.root.as_deref() != root
            || (root.is_some() && !t.busy && t.at.is_none_or(|at| at.elapsed() >= REINDEX_AFTER))
    }

    pub fn lib_stale(&self) -> bool {
        !self.lib.busy && self.lib.at.is_none_or(|at| at.elapsed() >= REINDEX_AFTER)
    }

    /// Walks `root` (None: no tree, as in a remote folder) for files on a worker, at most
    /// `tree_cap`; a walk still running stops.
    pub fn walk_tree(&mut self, root: Option<PathBuf>) {
        let gen = self.walking.fetch_add(1, Ordering::Relaxed) + 1;
        self.tree = FileList {
            busy: root.is_some(),
            root: root.clone(),
            gen,
            ..FileList::default()
        };
        let _ = self.matcher.send(Cmd::Set(Origin::Tree, Vec::new()));
        let Some(root) = root else { return };
        let (walking, cap) = (self.walking.clone(), self.tree_cap);
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        spawn("keel-jump-walk", move || {
            let alive = || walking.load(Ordering::Relaxed) == gen;
            let msg = |items, end| Msg::JumpFiles {
                origin: Origin::Tree,
                gen,
                items,
                end,
            };
            let capped = walk_files(&root, cap, &alive, &mut |items| {
                send(&tx, &ctx, msg(items, None))
            });
            if alive() {
                send(&tx, &ctx, msg(Vec::new(), Some(capped)));
            }
        });
    }

    /// Reads the library's file names on a worker; the list read before stays until then.
    pub fn read_library(&mut self, backend: LibraryBackend) {
        self.lib.gen += 1;
        self.lib.busy = true;
        let gen = self.lib.gen;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        spawn("keel-jump-library", move || {
            let (items, capped) = backend
                .file_names(keel_core::FILE_NAMES_MAX)
                .unwrap_or_else(|e| {
                    tracing::warn!("Ctrl+P library files: {e:#}");
                    (Vec::new(), false)
                });
            let msg = Msg::JumpFiles {
                origin: Origin::Library,
                gen,
                items,
                end: Some(capped),
            };
            send(&tx, &ctx, msg);
        });
    }

    /// A batch of files arrived (`end`: the list is complete, and whether it stopped at its
    /// cap). The library's one batch replaces its list; the walk's add up.
    pub fn files(&mut self, origin: Origin, gen: u64, items: Vec<String>, end: Option<bool>) {
        let list = match origin {
            Origin::Tree => &mut self.tree,
            Origin::Library => &mut self.lib,
            Origin::Folders => return,
        };
        if list.gen != gen {
            return;
        }
        let cmd = match origin {
            Origin::Library => {
                list.count = items.len();
                Cmd::Set(origin, items)
            }
            _ => {
                list.count += items.len();
                Cmd::Add(origin, items)
            }
        };
        if let Some(capped) = end {
            list.busy = false;
            list.capped = capped;
            list.at = Some(Instant::now());
        }
        let _ = self.matcher.send(cmd);
        if self.open {
            self.search();
        }
    }

    /// The footer: what is listed, what is still being read, and the caps.
    pub fn status(&self) -> String {
        let mut parts = vec![match self.indexing {
            true if self.indexed_at.is_none() => "indexing folders…".to_owned(),
            true => format!("{} folders, indexing…", count(self.indexed)),
            false => format!("{} folders", count(self.indexed)),
        }];
        let t = &self.tree;
        if t.root.is_some() {
            parts.push(match (t.busy, t.capped) {
                (true, _) => format!("indexing files here… {}", count(t.count)),
                (false, true) => format!("first {} files here", count(self.tree_cap)),
                (false, false) => format!("{} files here", count(t.count)),
            });
        }
        let l = &self.lib;
        if l.busy && l.at.is_none() {
            parts.push("reading the library…".to_owned());
        } else if l.at.is_some() {
            parts.push(match l.capped {
                true => format!("first {} library files", count(keel_core::FILE_NAMES_MAX)),
                false => format!("{} library files", count(l.count)),
            });
        }
        let text = parts.join("  ·  ");
        let mut chars = text.chars();
        chars
            .next()
            .map(|c| c.to_uppercase().chain(chars).collect())
            .unwrap_or_default()
    }

    fn busy(&self) -> bool {
        self.indexing || self.tree.busy || self.lib.busy
    }

    /// The popup. Enter opens the result (a file: its folder, with it selected and
    /// previewed), Ctrl+Enter in a new tab, Shift+Enter in the other pane; Tab switches
    /// between folders, files and both; F5 reads every list again.
    pub fn ui(&mut self, ctx: &egui::Context) -> Option<Action> {
        let mut out = None;
        let modal = Modal::new(Id::new("keel-jump")).show(ctx, |ui| {
            ui.set_width(640.0);
            let (up, down, new_tab, other, enter, f5, tab) = ui.input_mut(|i| {
                (
                    i.count_and_consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.count_and_consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::COMMAND, Key::Enter),
                    i.consume_key(Modifiers::SHIFT, Key::Enter),
                    i.consume_key(Modifiers::NONE, Key::Enter),
                    i.consume_key(Modifiers::NONE, Key::F5),
                    i.consume_key(Modifiers::NONE, Key::Tab),
                )
            });
            let before = self.filter;
            if tab {
                self.filter = self.filter.next();
            }
            ui.horizontal(|ui| {
                for f in JumpFilter::ALL {
                    ui.selectable_value(&mut self.filter, f, f.label());
                }
                ui.weak("(Tab)");
            });
            self.cursor = (self.cursor + down)
                .saturating_sub(up)
                .min(self.results.len().saturating_sub(1));
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.text)
                    .hint_text(self.filter.hint())
                    .lock_focus(true)
                    .desired_width(f32::INFINITY),
            );
            if std::mem::take(&mut self.focus) || !r.has_focus() {
                r.request_focus();
            }
            if r.changed() || self.filter != before {
                self.cursor = 0;
                self.search();
            }
            let mut go = |found: &Found, how: Open| {
                out = Some(Action::JumpTo {
                    path: found.vpath(),
                    file: found.file,
                    new_tab: how == Open::NewTab,
                    other_pane: how == Open::OtherPane,
                });
            };
            // Enter acts on results for the text as typed, never on stale ones.
            let pressed = [
                (enter, Open::Here),
                (new_tab, Open::NewTab),
                (other, Open::OtherPane),
            ];
            if let Some(how) = pressed.into_iter().find_map(|(p, how)| p.then_some(how)) {
                self.pending_enter = Some(how);
            }
            if self.answered == self.req {
                if let Some(how) = self.pending_enter.take() {
                    if let Some(found) = self.results.get(self.cursor) {
                        go(found, how);
                    }
                }
            }
            ui.add_space(4.0);
            let weak = ui.visuals().weak_text_color();
            let strong = ui.visuals().text_color();
            let font = egui::TextStyle::Body.resolve(ui.style());
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for (i, found) in self.results.iter().enumerate() {
                        let (name, dir) = found.split();
                        let fmt = |color| egui::TextFormat::simple(font.clone(), color);
                        let mut job = egui::text::LayoutJob::default();
                        job.append(name, 0.0, fmt(strong));
                        job.append(dir, 12.0, fmt(weak));
                        // Task 56: a folder's color and icon from the library.
                        let style = (!found.file)
                            .then(|| crate::folder_style::published(ui.ctx(), &found.vpath()))
                            .flatten();
                        let icon = match found.file {
                            true => crate::icons::file_icon(name),
                            false => crate::icons::folder(),
                        };
                        let image = egui::Image::new(icon.clone()).fit_to_exact_size([16.0, 16.0].into());
                        // Styled: the icon slot is drawn over (an emoji, a tint).
                        let image = if style.is_some() {
                            image.tint(egui::Color32::TRANSPARENT)
                        } else {
                            image
                        };
                        let r = ui.add(
                            egui::Button::image_and_text(image, job)
                                .selected(i == self.cursor)
                                .frame(i == self.cursor),
                        );
                        if let Some(style) = &style {
                            let label = format!("{name} {dir}, {}", crate::folder_style::describe(style));
                            r.clone().named(&label);
                            let at = egui::Rect::from_center_size(
                                egui::pos2(r.rect.left() + ui.spacing().button_padding.x + 8.0, r.rect.center().y),
                                egui::vec2(16.0, 16.0),
                            );
                            crate::folder_style::paint(ui, at, icon, Some(style), false);
                        }
                        if i == self.cursor && (up + down) > 0 {
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            let how = ui.input(|i| match (i.modifiers.command, i.modifiers.shift) {
                                (true, _) => Open::NewTab,
                                (_, true) => Open::OtherPane,
                                _ => Open::Here,
                            });
                            go(found, how);
                        }
                    }
                });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if self.busy() {
                    crate::anim::spinner(ui);
                }
                ui.weak(self.status());
            });
            ui.weak(
                "Enter open  ·  Ctrl+Enter new tab  ·  Shift+Enter other pane  ·  F5 reindex  ·  Esc close",
            );
            if f5 {
                out = Some(Action::ReindexFolders);
            }
        });
        if modal.should_close() || matches!(out, Some(Action::JumpTo { .. })) {
            self.open = false;
        }
        out
    }
}

/// Thousands separated by commas.
fn count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Folder display paths: the searcher's folder index, else a walk of the home folder.
/// Blocks for seconds: worker threads only.
/// The searcher's folders, else up to `WALK_CAP` folders under `home` (the app's home
/// folder; a test's fixture).
pub fn build_index(searcher: &dyn Searcher, home: Option<&Path>) -> Vec<String> {
    // Probe again: Everything may have started after Keel.
    if crate::search_tab::probe(searcher).is_none() {
        match keel_search::folder_index(searcher) {
            Ok(items) if !items.is_empty() => return items,
            Ok(_) => {}
            Err(e) => tracing::warn!("folder index: {e:#}; walking the home folder"),
        }
    }
    home.map(|h| walk_folders(h, WALK_CAP)).unwrap_or_default()
}

/// Up to `cap` folders under `root` (itself included), skipping hidden and git-ignored ones.
pub fn walk_folders(root: &Path, cap: usize) -> Vec<String> {
    ignore::WalkBuilder::new(root)
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_dir()))
        .take(cap)
        .map(|e| VPath::local(e.path()).display())
        .collect()
}

/// Up to `cap` files under `root`, skipping hidden and git-ignored ones like
/// `walk_folders`, handed to `batch` while `alive()` (each batch newest first). Returns
/// whether there were more than `cap`. Blocks: worker threads only.
pub fn walk_files(
    root: &Path,
    cap: usize,
    alive: &dyn Fn() -> bool,
    batch: &mut dyn FnMut(Vec<String>),
) -> bool {
    let mut buf: Vec<(Option<SystemTime>, String)> = Vec::new();
    let mut flush = |buf: &mut Vec<(Option<SystemTime>, String)>| {
        if !buf.is_empty() {
            buf.sort_by_key(|b| Reverse(b.0));
            batch(buf.drain(..).map(|(_, p)| p).collect());
        }
    };
    let (mut n, mut since) = (0, Instant::now());
    for e in ignore::WalkBuilder::new(root).build().flatten() {
        if !alive() {
            return false;
        }
        if e.file_type().is_none_or(|t| t.is_dir()) {
            continue;
        }
        if n == cap {
            flush(&mut buf);
            return true;
        }
        let modified = e.metadata().ok().and_then(|m| m.modified().ok());
        buf.push((modified, VPath::local(e.path()).display()));
        n += 1;
        if buf.len() >= BATCH || since.elapsed() >= BATCH_AFTER {
            flush(&mut buf);
            since = Instant::now();
        }
    }
    flush(&mut buf);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QA walkthrough 2026-10-10: without a search backend Ctrl+P walked the real home
    /// folder, also in tests; it walks the app's home (a test's fixture) now.
    #[test]
    fn without_a_backend_ctrl_p_walks_the_apps_home() {
        let root = std::env::temp_dir().join(format!("keel-jump-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("only-here")).unwrap();
        let none = keel_search::Unavailable::new("no search in tests");
        let items = build_index(&none, Some(&root));
        assert_eq!(items.len(), 2, "{items:?}");
        assert!(items
            .iter()
            .all(|i| i.starts_with(&root.display().to_string())));
        assert!(build_index(&none, None).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn walk_lists_folders_only_and_stops_at_the_cap() {
        let root = std::env::temp_dir().join(format!("keel-jump-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a").join("deep")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a").join("file.txt"), b"x").unwrap();
        let all = walk_folders(&root, usize::MAX);
        assert_eq!(all.len(), 4, "{all:?}");
        assert!(all.iter().any(|p| p.ends_with("deep")));
        assert_eq!(walk_folders(&root, 2).len(), 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn matcher_thread_answers_the_latest_search() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut jump = Jump::new(tx, egui::Context::default());
        jump.indexed(vec![
            r"D:\Work\home\filemgr".into(),
            r"C:\Users\x\Obsidian".into(),
        ]);
        jump.text = "obs".into();
        jump.search();
        let latest = jump.req;
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Msg::Jump { id, results } if id == latest => {
                    jump.results(id, results);
                    break;
                }
                Msg::Jump { id, results } => jump.results(id, results),
                _ => {}
            }
        }
        assert_eq!(jump.results[0].path, r"C:\Users\x\Obsidian");
    }

    /// Waits for the answer to the latest search.
    fn answer(jump: &mut Jump, rx: &crossbeam_channel::Receiver<Msg>) {
        let latest = jump.req;
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Msg::Jump { id, results } => {
                    jump.results(id, results);
                    if id == latest {
                        return;
                    }
                }
                Msg::JumpFiles {
                    origin,
                    gen,
                    items,
                    end,
                } => jump.files(origin, gen, items, end),
                _ => {}
            }
        }
    }

    #[test]
    fn ranking_prefers_better_scores_then_shorter_paths_then_the_tree() {
        let hits = vec![
            (10, Origin::Library, r"D:\lib\report.txt".to_owned()),
            (10, Origin::Tree, r"C:\cur\report.txt".to_owned()),
            (10, Origin::Folders, r"C:\cur\report".to_owned()),
            (10, Origin::Tree, r"C:\cur\deeper\report.txt".to_owned()),
            (12, Origin::Library, r"D:\lib\a\b\c\report.txt".to_owned()),
            // The same file from the library and the tree: listed once.
            (10, Origin::Library, r"C:\cur\report.txt".to_owned()),
        ];
        let paths: Vec<(String, bool)> = rank(hits, 10)
            .into_iter()
            .map(|f| (f.path, f.file))
            .collect();
        assert_eq!(
            paths,
            [
                (r"D:\lib\a\b\c\report.txt".to_owned(), true),
                (r"C:\cur\report".to_owned(), false),
                (r"C:\cur\report.txt".to_owned(), true),
                (r"D:\lib\report.txt".to_owned(), true),
                (r"C:\cur\deeper\report.txt".to_owned(), true),
            ]
        );
        assert_eq!(rank(vec![(1, Origin::Tree, "a".into())], 0), []);
        let f = Found {
            path: r"C:\cur\deeper\report.txt".into(),
            file: true,
        };
        assert_eq!(f.split(), ("report.txt", r"C:\cur\deeper"));
        let root = Found {
            path: "/".into(),
            file: false,
        };
        assert_eq!(root.split(), ("/", ""));
    }

    #[test]
    fn the_footer_names_the_caps() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut jump = Jump::new(tx, egui::Context::default());
        jump.indexing = true;
        assert_eq!(jump.status(), "Indexing folders…");
        jump.indexing = false;
        jump.indexed = 1_234_567;
        jump.tree = FileList {
            root: Some(PathBuf::from("here")),
            busy: true,
            count: 4_000,
            ..FileList::default()
        };
        assert_eq!(
            jump.status(),
            "1,234,567 folders  ·  indexing files here… 4,000"
        );
        jump.tree.busy = false;
        jump.tree.capped = true;
        jump.lib = FileList {
            capped: true,
            at: Some(Instant::now()),
            count: keel_core::FILE_NAMES_MAX,
            ..FileList::default()
        };
        assert_eq!(
            jump.status(),
            "1,234,567 folders  ·  first 50,000 files here  ·  first 100,000 library files"
        );
        jump.tree.capped = false;
        jump.lib.capped = false;
        jump.lib.count = 12;
        assert_eq!(
            jump.status(),
            "1,234,567 folders  ·  4,000 files here  ·  12 library files"
        );
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
    }

    /// A tree with more files than the cap: the walk stops there, says so, and the finder
    /// still answers from what it read.
    #[test]
    fn a_tree_over_the_cap_reports_the_cap_and_still_answers() {
        let root = std::env::temp_dir().join(format!("keel-jump-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for i in 0..30 {
            std::fs::write(root.join("sub").join(format!("file{i:02}.txt")), b"x").unwrap();
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut jump = Jump::new(tx, egui::Context::default());
        jump.tree_cap = 20;
        jump.filter = JumpFilter::Files;
        assert!(jump.tree_stale(Some(&root)));
        jump.walk_tree(Some(root.clone()));
        assert!(!jump.tree_stale(Some(&root)), "a walk is running");
        let end = Instant::now() + Duration::from_secs(10);
        while jump.tree.busy {
            assert!(Instant::now() < end, "the walk ends");
            if let Ok(Msg::JumpFiles {
                origin,
                gen,
                items,
                end,
            }) = rx.recv_timeout(Duration::from_secs(1))
            {
                jump.files(origin, gen, items, end);
            }
        }
        assert_eq!((jump.tree.count, jump.tree.capped), (20, true));
        assert!(
            jump.status().contains("first 20 files here"),
            "{}",
            jump.status()
        );
        assert!(!jump.tree_stale(Some(&root)), "read now");
        assert!(jump.tree_stale(Some(&root.join("sub"))), "another folder");
        jump.text = "file".into();
        jump.search();
        answer(&mut jump, &rx);
        assert_eq!(jump.results.len(), 20, "{:?}", jump.results);
        assert!(jump.results.iter().all(|f| f.file));
        // Folders only: the files are gone.
        jump.filter = JumpFilter::Folders;
        jump.search();
        answer(&mut jump, &rx);
        assert!(jump.results.is_empty(), "{:?}", jump.results);
        // No tree (a remote folder): the walked files leave.
        jump.walk_tree(None);
        jump.filter = JumpFilter::Both;
        jump.search();
        answer(&mut jump, &rx);
        assert!(jump.results.is_empty(), "{:?}", jump.results);
        assert!(!jump.tree_stale(None));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn walk_files_lists_files_only_and_stops_at_the_cap() {
        let root = std::env::temp_dir().join(format!("keel-jump-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a").join("deep")).unwrap();
        std::fs::write(root.join("a").join("deep").join("one.txt"), b"x").unwrap();
        std::fs::write(root.join("two.txt"), b"x").unwrap();
        let mut all = Vec::new();
        assert!(!walk_files(&root, 10, &|| true, &mut |b| all.extend(b)));
        all.sort();
        assert_eq!(all.len(), 2, "{all:?}");
        assert!(all[0].ends_with("one.txt") && all[1].ends_with("two.txt"));
        let mut one = Vec::new();
        assert!(walk_files(&root, 1, &|| true, &mut |b| one.extend(b)));
        assert_eq!(one.len(), 1);
        let mut none = Vec::new();
        assert!(!walk_files(&root, 10, &|| false, &mut |b| none.extend(b)));
        assert!(none.is_empty(), "a stopped walk hands nothing over");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Live (`KEEL_LIVE_JUMP_DIR`: a folder with many files, e.g. 20,000 generated ones):
    /// the walk hands its first batch over quickly, and each keystroke's search answers
    /// within a second. Skipped without the variable; the timing is not asserted under CI.
    #[test]
    fn live_a_big_tree_answers_each_keystroke_within_a_second() {
        let Some(root) = std::env::var_os("KEEL_LIVE_JUMP_DIR").map(PathBuf::from) else {
            return;
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut jump = Jump::new(tx, egui::Context::default());
        jump.open = true;
        jump.filter = JumpFilter::Files;
        let start = Instant::now();
        jump.walk_tree(Some(root));
        let mut first = None;
        let mut keystrokes = Vec::new();
        let word = "file1234";
        let mut key = |jump: &mut Jump, rx: &crossbeam_channel::Receiver<Msg>| {
            jump.text = word[..=keystrokes.len()].to_owned();
            let typed = Instant::now();
            jump.search();
            answer(jump, rx);
            keystrokes.push(typed.elapsed());
            keystrokes.len() < word.len()
        };
        // One letter after each batch while the walk runs, then the rest.
        let mut more = true;
        while jump.tree.busy {
            match rx
                .recv_timeout(Duration::from_secs(30))
                .expect("the walk answers")
            {
                Msg::JumpFiles {
                    origin,
                    gen,
                    items,
                    end,
                } => {
                    first.get_or_insert(start.elapsed());
                    jump.files(origin, gen, items, end);
                }
                Msg::Jump { id, results } => jump.results(id, results),
                _ => {}
            }
            if first.is_some() && more {
                more = key(&mut jump, &rx);
            }
        }
        let walk = start.elapsed();
        while more {
            more = key(&mut jump, &rx);
        }
        let slowest = keystrokes.iter().max().copied().unwrap_or_default();
        eprintln!(
            "{} files, first batch after {:?}, walk {:?}, slowest keystroke {slowest:?}, {} results for {word:?}",
            jump.tree.count,
            first.unwrap_or_default(),
            walk,
            jump.results.len()
        );
        assert!(!jump.results.is_empty());
        if std::env::var_os("CI").is_none() {
            assert!(slowest < Duration::from_secs(1), "{keystrokes:?}");
        }
    }
}
