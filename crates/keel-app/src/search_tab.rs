//! Search tab: a query box over the active pane's results table. Typing is debounced
//! (`DEBOUNCE`); `AppState::tick` sends due queries to `worker::spawn_search`.

use crate::bookmarks::FindCmd;
use crate::keys::Action;
use crate::pane::Pane;
use crate::search_history::SUGGESTIONS;
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

/// Task 76: the suggestions dropdown of one search field.
#[derive(Clone, Default)]
struct Suggest {
    /// The row Up/Down moved to (Enter picks it).
    highlight: Option<usize>,
    /// Esc (or a pick) closed it for this text; typing opens it again.
    dismissed: Option<String>,
    /// Alt+Down opened it although suggestions are off.
    forced: bool,
    /// The text last put in the history from this field.
    remembered: Option<String>,
}

/// The query box. Editing schedules the query `DEBOUNCE` later; Enter runs it now;
/// Up/Down move the result cursor while typing. With the Library backend the filter chips
/// (`chip_row`) follow; a chip runs the query now.
pub fn bar(
    ui: &mut egui::Ui,
    pane: &mut Pane,
    platform: Option<&str>,
    lib: &crate::library::LibraryUi,
    out: &mut Vec<Action>,
) {
    let library = lib.is_open();
    let focus = std::mem::take(&mut pane.focus_filter);
    let tab = pane.tab_mut();
    let (count, busy) = (tab.entries().len(), tab.loading);
    let TabKind::Search { query, due, .. } = &mut tab.kind else {
        return;
    };
    let shared = crate::bookmarks::published(ui.ctx()); // Task 76
    ui.horizontal(|ui| {
        // Task 29: the backend (the platform's, or the library index).
        let platform = platform.unwrap_or("System");
        let before = tab.library_search;
        let combo = egui::ComboBox::from_id_salt("search-backend")
            .width(110.0)
            .selected_text(if tab.library_search {
                "Library"
            } else {
                platform
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut tab.library_search, false, platform);
                ui.add_enabled_ui(library, |ui| {
                    ui.selectable_value(&mut tab.library_search, true, "Library")
                        .on_disabled_hover_text("The library is off or still opening");
                });
            });
        let _ = crate::a11y::Named::named(combo.response, "Search with");
        if tab.library_search != before {
            // Run the query again on the other backend now.
            *due = Some(Instant::now());
            ui.ctx().request_repaint();
        }
        let id = ui.id().with("search");
        // Task 76: earlier searches in a dropdown under the field (`Suggest`).
        let sid = id.with("suggest");
        let popup = id.with("suggest-popup");
        let mut st: Suggest = ui.data(|d| d.get_temp(sid)).unwrap_or_default();
        if st.dismissed.as_deref().is_some_and(|d| d != query.as_str()) {
            st.dismissed = None;
            st.forced = false;
        }
        let focused = ui.memory(|m| m.has_focus(id));
        let shown = ui.memory(|m| m.is_popup_open(popup));
        let list: Vec<String> = match focused || shown {
            true => crate::bookmarks::suggestions(&shared.history, query, SUGGESTIONS),
            false => Vec::new(),
        };
        if focused && ui.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowDown)) {
            st.forced = true;
            st.dismissed = None;
        }
        let open = focused
            && !list.is_empty()
            && st.dismissed.is_none()
            && (shared.suggestions || st.forced);
        if !open {
            st.highlight = None;
        }
        let mut picked: Option<String> = None;
        if focused {
            ui.input_mut(|i| {
                if open {
                    if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                        let last = list.len() - 1;
                        st.highlight = Some(st.highlight.map_or(0, |h| (h + 1).min(last)));
                    }
                    if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        st.highlight = st.highlight.and_then(|h| h.checked_sub(1));
                    }
                    if let Some(h) = st.highlight {
                        if i.consume_key(Modifiers::NONE, Key::Enter) {
                            picked = list.get(h).cloned();
                        }
                    }
                    if i.consume_key(Modifiers::NONE, Key::Escape) {
                        st.dismissed = Some(query.clone());
                    }
                }
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
                .desired_width((ui.available_width() - 250.0).max(120.0))
                .hint_text(HINT),
        );
        let to_end = |ui: &egui::Ui, query: &str| {
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                let end = egui::text::CCursor::new(query.chars().count());
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                state.store(ui.ctx(), id);
            }
        };
        if focus {
            r.request_focus();
            to_end(ui, query);
        }
        if r.changed() {
            *due = Some(Instant::now() + DEBOUNCE);
            ui.ctx().request_repaint_after(DEBOUNCE);
        }
        let enter = ui.input(|i| i.key_pressed(Key::Enter));
        if r.lost_focus() && enter {
            *due = Some(Instant::now());
            ui.ctx().request_repaint();
        }
        // Task 76: a search run with Enter, or left for its results, goes into the history
        // (once per text; not while a suggestion is being clicked).
        let text = query.trim();
        if r.lost_focus()
            && !text.is_empty()
            && !shown
            && (enter || st.remembered.as_deref() != Some(text))
        {
            out.push(Action::Find(FindCmd::Remember(text.to_owned())));
            st.remembered = Some(text.to_owned());
        }
        if open {
            ui.memory_mut(|m| m.open_popup(popup));
        } else if shown && !ui.input(|i| i.pointer.any_down() || i.pointer.any_released()) {
            ui.memory_mut(|m| m.close_popup());
        }
        egui::popup_below_widget(
            ui,
            popup,
            &r,
            egui::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                ui.set_min_width(r.rect.width());
                for (i, text) in list.iter().enumerate() {
                    if ui
                        .selectable_label(st.highlight == Some(i), text.as_str())
                        .clicked()
                    {
                        picked = Some(text.clone());
                    }
                }
            },
        );
        if let Some(text) = picked {
            query.clone_from(&text);
            *due = Some(Instant::now());
            ui.ctx().request_repaint();
            out.push(Action::Find(FindCmd::Remember(text.clone())));
            st.dismissed = Some(text.clone());
            st.remembered = Some(text);
            st.highlight = None;
            ui.memory_mut(|m| m.close_popup());
            r.request_focus();
            to_end(ui, query);
        }
        ui.data_mut(|d| d.insert_temp(sid, st));
        // Task 76: the filter (Everything, Audio, …, the profile's own) and bookmarks.
        let before = tab.search_filter.clone();
        let names = match shared.filters.is_empty() {
            true => crate::search_filters::names(&[]),
            false => shared.filters.clone(),
        };
        let combo = egui::ComboBox::from_id_salt("search-filter")
            .width(100.0)
            .selected_text(crate::search_filters::shown(&tab.search_filter))
            .show_ui(ui, |ui| {
                for name in &names {
                    let value = match name.as_str() {
                        "Everything" => String::new(),
                        n => n.to_owned(),
                    };
                    ui.selectable_value(&mut tab.search_filter, value, name.as_str());
                }
                ui.separator();
                if ui.button("Edit filters…").clicked() {
                    out.push(Action::Find(FindCmd::EditFilters));
                }
            });
        let _ = crate::a11y::Named::named(combo.response, "Filter");
        if tab.search_filter != before {
            *due = Some(Instant::now());
            ui.ctx().request_repaint();
        }
        crate::bookmarks::menu(ui, out);
        if busy {
            crate::anim::spinner(ui);
        }
        let n = count;
        ui.weak(match n {
            1 => "1 result".to_owned(),
            n if n as u32 >= MAX_HITS => format!("first {n} results"),
            n => format!("{n} results"),
        });
    });
    if tab.library_search && library {
        let tags: Vec<String> = lib.tags.iter().map(|t| t.name.clone()).collect();
        let sources: Vec<String> = lib.sources.iter().map(|s| s.label.clone()).collect();
        let offer = crate::chip_row::Offer {
            tags: &tags,
            sources: &sources,
            types: &lib.top_ext,
        };
        if crate::chip_row::chip_row(ui, query, &offer) {
            *due = Some(Instant::now());
            ui.ctx().request_repaint();
        }
    }
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
            snippet: None,
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
