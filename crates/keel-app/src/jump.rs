//! Ctrl+P: fuzzy jump to a folder. The folder list comes from the searcher (Everything
//! `folder:`) or a walk of the home folder, built on a worker; nucleo matching runs on
//! its own thread because draining 200k paths per keystroke would stall the UI.

use crate::keys::Action;
use crate::state::Msg;
use crate::worker::{send, spawn};
use crossbeam_channel::Sender;
use egui::{Id, Key, Modal, Modifiers};
use keel_search::{Fuzzy, Searcher};
use keel_vfs::VPath;
use std::path::Path;
use std::time::{Duration, Instant};

pub const MAX_RESULTS: usize = 50;
/// The index is rebuilt on open when older than this (F5 in the popup forces it).
pub const REINDEX_AFTER: Duration = Duration::from_secs(600);
/// Folders kept from the home-folder walk when no searcher works.
pub const WALK_CAP: usize = 50_000;

enum Cmd {
    Items(Vec<String>),
    Search(u64, String),
}

pub struct Jump {
    pub open: bool,
    pub text: String,
    pub results: Vec<String>,
    pub cursor: usize,
    focus: bool,
    /// The matcher thread, which owns the `Fuzzy`.
    matcher: Sender<Cmd>,
    req: u64,
    /// The request `results` answer.
    answered: u64,
    /// Enter (or Ctrl+Enter: true) pressed before the results caught up with the text.
    pending_enter: Option<bool>,
    pub indexed: usize,
    pub indexed_at: Option<Instant>,
    pub indexing: bool,
    /// An index was asked for before the searcher finished loading.
    pub waiting: bool,
}

impl Jump {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context) -> Self {
        let (matcher, rx) = crossbeam_channel::unbounded::<Cmd>();
        spawn("keel-jump", move || {
            let mut fuzzy: Option<Fuzzy> = None;
            while let Ok(first) = rx.recv() {
                // Only the newest search of a burst is worth matching.
                let mut search = None;
                for cmd in std::iter::once(first).chain(rx.try_iter()) {
                    match cmd {
                        Cmd::Items(items) => fuzzy = Some(Fuzzy::new(items)),
                        Cmd::Search(id, pat) => search = Some((id, pat)),
                    }
                }
                if let (Some((id, pat)), Some(f)) = (search, fuzzy.as_mut()) {
                    let results = f
                        .search(&pat, MAX_RESULTS)
                        .into_iter()
                        .map(|(_, s)| s)
                        .collect();
                    send(&tx, &ctx, Msg::Jump { id, results });
                }
            }
        });
        Self {
            open: false,
            text: String::new(),
            results: Vec::new(),
            cursor: 0,
            focus: false,
            matcher,
            req: 0,
            answered: 0,
            pending_enter: None,
            indexed: 0,
            indexed_at: None,
            indexing: false,
            waiting: false,
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

    pub fn stale(&self) -> bool {
        self.indexed_at
            .is_none_or(|at| at.elapsed() >= REINDEX_AFTER)
    }

    fn search(&mut self) {
        self.req += 1;
        let _ = self.matcher.send(Cmd::Search(self.req, self.text.clone()));
    }

    pub fn results(&mut self, id: u64, results: Vec<String>) {
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
        let _ = self.matcher.send(Cmd::Items(items));
        self.search();
    }

    /// The popup. Enter navigates the active pane, Ctrl+Enter opens a new tab, F5 reindexes.
    pub fn ui(&mut self, ctx: &egui::Context) -> Option<Action> {
        let mut out = None;
        let modal = Modal::new(Id::new("keel-jump")).show(ctx, |ui| {
            ui.set_width(640.0);
            let (up, down, new_tab, enter, f5) = ui.input_mut(|i| {
                (
                    i.count_and_consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.count_and_consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::COMMAND, Key::Enter),
                    i.consume_key(Modifiers::NONE, Key::Enter),
                    i.consume_key(Modifiers::NONE, Key::F5),
                )
            });
            self.cursor = (self.cursor + down)
                .saturating_sub(up)
                .min(self.results.len().saturating_sub(1));
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.text)
                    .hint_text("Jump to folder")
                    .desired_width(f32::INFINITY),
            );
            if std::mem::take(&mut self.focus) || !r.has_focus() {
                r.request_focus();
            }
            if r.changed() {
                self.cursor = 0;
                self.search();
            }
            let mut go = |path: &str, new_tab: bool| {
                let to = VPath::local(path);
                out = Some(if new_tab {
                    Action::NewTabAt(to)
                } else {
                    Action::Navigate(to)
                });
            };
            // Enter acts on results for the text as typed, never on stale ones.
            if enter || new_tab {
                self.pending_enter = Some(new_tab);
            }
            if self.answered == self.req {
                if let Some(new_tab) = self.pending_enter.take() {
                    if let Some(path) = self.results.get(self.cursor) {
                        go(path, new_tab);
                    }
                }
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for (i, path) in self.results.iter().enumerate() {
                        let r = ui.selectable_label(i == self.cursor, path);
                        if i == self.cursor && (up + down) > 0 {
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            go(path, ui.input(|i| i.modifiers.command));
                        }
                    }
                });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if self.indexing {
                    ui.spinner();
                    ui.weak("Indexing folders…");
                } else {
                    ui.weak(format!("{} folders", self.indexed));
                }
                ui.weak("·  Enter open  ·  Ctrl+Enter new tab  ·  F5 reindex  ·  Esc close");
            });
            if f5 {
                out = Some(Action::ReindexFolders);
            }
        });
        if modal.should_close() || matches!(out, Some(Action::Navigate(_) | Action::NewTabAt(_))) {
            self.open = false;
        }
        out
    }
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
        assert_eq!(jump.results[0], r"C:\Users\x\Obsidian");
    }
}
