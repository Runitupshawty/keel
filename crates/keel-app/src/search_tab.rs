//! Search tab: a query box over the active pane's results table. Typing is debounced
//! (`DEBOUNCE`); `AppState::tick` sends due queries to `worker::spawn_search`.

use crate::keys::Action;
use crate::pane::Pane;
use crate::tab::{Nav, TabKind};
use egui::{Key, Modifiers};
use keel_search::{Query, Searcher};
use std::time::{Duration, Instant};

pub const DEBOUNCE: Duration = Duration::from_millis(150);
pub const MAX_HITS: u32 = 500;

pub const NOT_RUNNING: &str =
    "Everything is not running. Start Everything.exe (voidtools) to search.";

#[cfg(windows)]
const HINT: &str = "Everything syntax: ext:pdf dm:today \"exact phrase\" folder: regex:";
#[cfg(not(windows))]
const HINT: &str = "Search file names";

/// What the user is told when a query fails.
pub fn banner(err: &str) -> String {
    if err.contains("Everything is not running") {
        NOT_RUNNING.into()
    } else {
        err.into()
    }
}

/// Why search does not work right now, or None when it does. Asks the backend to
/// check again first (Everything may have started since). May block on IPC or a
/// child process: worker threads only.
pub fn probe(searcher: &dyn Searcher) -> Option<String> {
    if searcher.probe() {
        return None;
    }
    let one = Query {
        max: 1,
        ..Query::default()
    };
    searcher
        .query(&one)
        .err()
        .map(|e| banner(&format!("{e:#}")))
}

/// The query box. Editing schedules the query `DEBOUNCE` later; Enter runs it now;
/// Up/Down move the result cursor while typing.
pub fn bar(ui: &mut egui::Ui, pane: &mut Pane, out: &mut Vec<Action>) {
    let focus = std::mem::take(&mut pane.focus_filter);
    let tab = pane.tab_mut();
    let (count, busy) = (tab.entries().len(), tab.loading);
    let TabKind::Search { query, due, .. } = &mut tab.kind else {
        return;
    };
    ui.horizontal(|ui| {
        let id = ui.id().with("search");
        if ui.memory(|m| m.has_focus(id)) {
            ui.input_mut(|i| {
                for (key, nav) in [(Key::ArrowUp, Nav::Prev), (Key::ArrowDown, Nav::Next)] {
                    if i.consume_key(Modifiers::NONE, key) {
                        out.push(Action::Move(nav, i.modifiers.shift));
                    }
                }
                if i.consume_key(Modifiers::COMMAND, Key::Enter) {
                    out.push(Action::OpenLocation);
                }
            });
        }
        let r = ui.add(
            egui::TextEdit::singleline(query)
                .id(id)
                .desired_width((ui.available_width() - 110.0).max(120.0))
                .hint_text(HINT),
        );
        if focus {
            r.request_focus();
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                let end = egui::text::CCursor::new(query.chars().count());
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                state.store(ui.ctx(), id);
            }
        }
        if r.changed() {
            *due = Some(Instant::now() + DEBOUNCE);
            ui.ctx().request_repaint_after(DEBOUNCE);
        }
        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            *due = Some(Instant::now());
            ui.ctx().request_repaint();
        }
        if busy {
            ui.spinner();
        }
        let n = count;
        ui.weak(match n {
            1 => "1 result".to_owned(),
            n if n as u32 >= MAX_HITS => format!("first {n} results"),
            n => format!("{n} results"),
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, Msg};
    use keel_search::Unavailable;
    use keel_vfs::{Router, VPath};
    use std::sync::Arc;

    /// Review Focus 3: no search backend; the search tab says why, the app keeps working.
    #[test]
    fn unavailable_searcher_shows_banner_in_search_tab() {
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(egui::Context::default(), Arc::new(Router::new()), dir);
        let searcher: Arc<dyn Searcher> = Arc::new(Unavailable::new("Everything is not running"));
        let reason = probe(searcher.as_ref());
        state.apply(Msg::Searcher { searcher, reason });

        state.run(0, Action::Search);
        let tab = state.tab(0);
        assert!(tab.is_search());
        assert_eq!(tab.error.as_deref(), Some(NOT_RUNNING));
        assert_eq!(state.panes[0].tabs.len(), 2, "folder tab still there");

        // A query still runs (Everything may have started since) and fails softly.
        if let TabKind::Search { query, due, .. } = &mut state.tab_mut(0).kind {
            *query = "ext:toml".into();
            *due = Some(Instant::now());
        }
        state.tick();
        let msg = loop {
            match state.rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                m @ Msg::Search { .. } => break m,
                other => state.apply(other),
            }
        };
        state.apply(msg);
        assert_eq!(state.tab(0).error.as_deref(), Some(NOT_RUNNING));
        assert!(!state.tab(0).loading);
    }

    /// Hits with the same file name stay distinct rows; Open location selects the file in
    /// the other pane.
    #[test]
    fn hits_become_rows_and_open_location_selects_the_file() {
        let root = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(egui::Context::default(), Arc::new(Router::new()), root);
        state.run(0, Action::Search);
        let hit = |dir: &str| keel_search::Hit {
            path: VPath::local(std::env::temp_dir().join(dir).join("Cargo.toml")),
            is_dir: false,
            size: 7,
            modified: None,
        };
        if let TabKind::Search { req, .. } = &mut state.tab_mut(0).kind {
            *req = 99;
        }
        state.apply(Msg::Search {
            id: 99,
            result: Ok(vec![hit("a"), hit("b")]),
        });
        let tab = state.tab_mut(0);
        assert_eq!(tab.visible(false).len(), 2);
        let first = tab.entries()[tab.visible_cached()[0]].clone();
        assert_eq!(tab.shown_name(&first), "Cargo.toml");
        assert_eq!(first.ext, "toml");
        tab.click(&first.name, false, false);

        state.run(0, Action::OpenLocation);
        assert_eq!(state.active, 1);
        let other = state.tab(1);
        assert_eq!(Some(other.dir.clone()), first.path.parent());
        assert_eq!(other.cursor.as_deref(), Some("Cargo.toml"));
        assert!(other.reveal);
    }
}
