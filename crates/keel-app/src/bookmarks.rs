//! Bookmarks, filters and search history in the window (Task 76).
//!
//! A bookmark is a saved search: its text, backend (System or Library), filter, match
//! toggles, sort, layout and columns, with a place in the Bookmarks list (folder, position)
//! and an optional shortcut key. Bookmarks of library searches are the library's saved
//! views (`keel_core::View`, also `views.*` in the API); bookmarks of System-backend
//! searches are kept in the profile (`[[bookmarks]]`, the same record), or in the library
//! when the user picks it. Ctrl+D in a search tab adds one; the search bar's ☆ menu and the
//! sidebar's Bookmarks section open them; Organize bookmarks moves, edits and deletes them.
//! Filters are `search_filters`, the history `search_history`.

use crate::keys::Action;
use crate::search_filters::CustomFilter;
use crate::search_history::HistoryStore;
use crate::state::AppState;
use crate::tab::{SortKey, TabKind};
use egui::{Event, Id, Key, Modal, Modifiers, RichText};
use keel_core::{MatchToggles, View, ViewBackend};
use std::sync::Arc;
use std::time::Instant;

/// Where a bookmark is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Store {
    /// A library saved view.
    Library,
    /// The profile's `[[bookmarks]]`.
    Profile,
}

/// A bookmark, filter or history command (`Action::Find`).
#[derive(Clone, Debug, PartialEq)]
pub enum FindCmd {
    /// Ctrl+D, the ☆ menu: the Add bookmark dialog for the active search tab.
    AddBookmark,
    Edit(Store, i64),
    /// The bookmark dialog's answer: a new bookmark in `store`, or `replaces` it.
    Save {
        store: Store,
        view: Box<View>,
        replaces: Option<i64>,
    },
    /// Asks first (`Delete`).
    AskDelete(Store, i64),
    Delete(Store, i64),
    /// One place up (-1) or down (+1) inside its folder.
    Move(Store, i64, i64),
    Open(Store, i64),
    Organize,
    EditFilters,
    /// A search worth keeping in the history (run with Enter, picked, or left for a result).
    Remember(String),
    ClearHistory,
}

/// The bookmark dialog.
pub struct Editor {
    pub view: View,
    pub store: Store,
    /// Editing this bookmark (its store is fixed then).
    pub replaces: Option<i64>,
    pub error: Option<String>,
    /// Waiting for the shortcut key to record.
    pub recording: bool,
}

/// The filter editor: a copy of the profile's filters, saved on OK.
pub struct FiltersEditor {
    pub list: Vec<CustomFilter>,
    pub selected: Option<usize>,
    pub error: Option<String>,
}

/// What the search bar and the sidebar draw, published once per change (`publish`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Published {
    /// Every filter name for the combo.
    pub filters: Vec<String>,
    /// Earlier searches in the chosen order.
    pub history: Vec<String>,
    /// Always show suggestions.
    pub suggestions: bool,
    /// Every bookmark, in list order.
    pub bookmarks: Vec<(Store, View)>,
}

#[derive(Default)]
pub struct Find {
    pub editor: Option<Editor>,
    pub organize: bool,
    pub filters: Option<FiltersEditor>,
    pub history: HistoryStore,
    published: Arc<Published>,
}

const PUBLISHED: &str = "keel-find";

/// What `publish` handed out last (an empty one before).
pub fn published(ctx: &egui::Context) -> Arc<Published> {
    ctx.data(|d| d.get_temp::<Arc<Published>>(Id::new(PUBLISHED)))
        .unwrap_or_default()
}

/// Every bookmark, in list order: by folder (top level first), position, then name.
pub fn all(profile: &[View], library: Option<&[View]>) -> Vec<(Store, View)> {
    let mut out: Vec<(Store, View)> = (library.unwrap_or_default().iter())
        .map(|v| (Store::Library, v.clone()))
        .chain(profile.iter().map(|v| (Store::Profile, v.clone())))
        .collect();
    out.sort_by(|(_, a), (_, b)| {
        (a.folder.as_str(), a.position, a.name.to_lowercase()).cmp(&(
            b.folder.as_str(),
            b.position,
            b.name.to_lowercase(),
        ))
    });
    out
}

/// Up to `n` of the earlier searches `history` (in the chosen order) that hold `typed` in
/// any case, without the text exactly as typed.
pub fn suggestions(history: &[String], typed: &str, n: usize) -> Vec<String> {
    let typed = typed.trim();
    let needle = typed.to_lowercase();
    (history.iter())
        .filter(|h| h.as_str() != typed && h.to_lowercase().contains(&needle))
        .take(n)
        .cloned()
        .collect()
}

// --- Sort and layout as stored in a bookmark ---

fn sort_word(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
        SortKey::Ext => "extension",
    }
}

/// A tab's sort as a bookmark stores it ("" for the default, Name ascending).
pub fn sort_text(sort: (SortKey, bool)) -> String {
    match sort {
        (SortKey::Name, true) => String::new(),
        (key, true) => sort_word(key).to_owned(),
        (key, false) => format!("{}:desc", sort_word(key)),
    }
}

/// A bookmark's sort for a tab: its first key this window can sort by (the others wait
/// for more columns); None for the default.
pub fn parse_sort(text: &str) -> Option<(SortKey, bool)> {
    text.split(',').find_map(|k| {
        let (col, dir) = k.split_once(':').unwrap_or((k, "asc"));
        let key = match col {
            "name" => SortKey::Name,
            "size" => SortKey::Size,
            "modified" | "date_modified" => SortKey::Modified,
            "extension" | "ext" => SortKey::Ext,
            _ => return None,
        };
        Some((key, dir != "desc"))
    })
}

fn layout_of(view: crate::pane::ViewMode) -> &'static str {
    match view {
        crate::pane::ViewMode::Grid => "grid",
        _ => "details",
    }
}

// --- Shortcut keys ---

/// `Ctrl+Shift+1` → the modifiers and key; Ctrl means Cmd on macOS.
pub fn parse_shortcut(text: &str) -> Option<(Modifiers, Key)> {
    let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let key = Key::from_name(parts.pop()?)?;
    let mut mods = Modifiers::NONE;
    for p in parts {
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "cmd" | "command" => mods.command = true,
            "alt" | "option" => mods.alt = true,
            "shift" => mods.shift = true,
            _ => return None,
        }
    }
    Some((mods, key))
}

pub fn shortcut_text(mods: Modifiers, key: Key) -> String {
    let mut s = String::new();
    if mods.command || mods.ctrl || mods.mac_cmd {
        s.push_str("Ctrl+");
    }
    if mods.alt {
        s.push_str("Alt+");
    }
    if mods.shift {
        s.push_str("Shift+");
    }
    s.push_str(key.name());
    s
}

fn function_key(key: Key) -> bool {
    key.name()
        .strip_prefix('F')
        .is_some_and(|n| n.parse::<u8>().is_ok())
}

/// Why `text` cannot be bookmark `me`'s shortcut, given every bookmark: a key that types
/// (it needs Ctrl or Alt, or must be a function key), Keel's own shortcuts (Ctrl+D adds a
/// bookmark) and another bookmark's.
pub fn shortcut_problem(
    text: &str,
    me: Option<(Store, i64)>,
    all: &[(Store, View)],
) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let Some((mods, key)) = parse_shortcut(text) else {
        return Some(crate::i18n::tr("bookmarks.not_a_key").replace("{text}", text));
    };
    if !(mods.command || mods.alt || function_key(key)) {
        return Some(crate::i18n::tr("bookmarks.use_ctrl_or_alt").to_owned());
    }
    if crate::keys::is_builtin(mods, key) || (mods == Modifiers::COMMAND && key == Key::D) {
        return Some(crate::i18n::tr("bookmarks.keel_shortcut").replace("{text}", text));
    }
    let clash = all
        .iter()
        .find(|(s, v)| Some((*s, v.id)) != me && parse_shortcut(&v.shortcut) == Some((mods, key)));
    clash.map(|(_, v)| {
        crate::i18n::tr("bookmarks.opens_bookmark")
            .replace("{text}", text)
            .replace("{name}", &v.name)
    })
}

impl Find {
    /// Hands the search bar and sidebar what they draw, when it changed.
    pub fn publish(
        &mut self,
        ctx: &egui::Context,
        settings: &crate::settings::Settings,
        library: Option<&[View]>,
    ) {
        let order = settings.search_history.order;
        let fresh = Published {
            filters: crate::search_filters::names(&settings.filters),
            history: (self.history.history.ordered(order).into_iter())
                .map(|e| e.text.clone())
                .collect(),
            suggestions: settings.search_history.suggestions,
            bookmarks: all(&settings.bookmarks, library),
        };
        if *self.published != fresh {
            self.published = Arc::new(fresh);
            let p = self.published.clone();
            ctx.data_mut(|d| d.insert_temp(Id::new(PUBLISHED), p));
        }
    }
}

impl AppState {
    fn library_views(&self) -> Option<&[View]> {
        self.library
            .is_open()
            .then_some(self.library.views.as_slice())
    }

    fn bookmarks(&self) -> Vec<(Store, View)> {
        all(&self.settings.bookmarks, self.library_views())
    }

    fn bookmark(&self, store: Store, id: i64) -> Option<View> {
        (self.bookmarks().into_iter())
            .find(|(s, v)| *s == store && v.id == id)
            .map(|(_, v)| v)
    }

    /// Every frame before the shortcut table (app.rs): Ctrl+D in a search tab adds a
    /// bookmark (instead of Favorites), and a bookmark's shortcut opens it, also from the
    /// search field. Exact modifiers: Ctrl+1 never fires on Ctrl+Shift+1.
    pub fn bookmark_keys(&mut self, ctx: &egui::Context, on: bool) {
        if !on {
            return;
        }
        let search = self.tab(self.active).is_search();
        let keys: Vec<(Modifiers, Key, Store, i64)> = (self.bookmarks().into_iter())
            .filter_map(|(s, v)| parse_shortcut(&v.shortcut).map(|(m, k)| (m, k, s, v.id)))
            .collect();
        let mut add = false;
        let mut open = None;
        ctx.input_mut(|i| {
            i.events.retain(|e| {
                let Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = e
                else {
                    return true;
                };
                if search && *key == Key::D && modifiers.matches_exact(Modifiers::COMMAND) {
                    add = true;
                    return false;
                }
                match keys
                    .iter()
                    .find(|(m, k, ..)| k == key && modifiers.matches_exact(*m))
                {
                    Some((.., s, id)) => {
                        open = Some((*s, *id));
                        false
                    }
                    None => true,
                }
            });
        });
        if add {
            self.find_cmd(self.active, FindCmd::AddBookmark);
        }
        if let Some((s, id)) = open {
            self.find_cmd(self.active, FindCmd::Open(s, id));
        }
    }

    /// The search of tab `p`'s active tab as a new bookmark (None: not a search tab).
    fn bookmark_of_tab(&self, p: usize) -> Option<(View, Store)> {
        let pane = &self.panes[p];
        let tab = pane.tab();
        let TabKind::Search { query, .. } = &tab.kind else {
            return None;
        };
        let name: String = match query.trim() {
            "" => crate::i18n::tr("bookmarks.new_bookmark").to_owned(),
            q => q.chars().take(60).collect(),
        };
        let backend = match tab.library_search {
            true => ViewBackend::Library,
            false => ViewBackend::System,
        };
        let view = View {
            name: name.trim().to_owned(),
            query: query.trim().to_owned(),
            layout: layout_of(pane.view).to_owned(),
            backend,
            sort: sort_text(tab.sort),
            filter: tab.search_filter.clone(),
            matching: tab.match_toggles(),
            ..View::default()
        };
        // A System search lives in the profile unless the user picks the library.
        let store = match backend {
            ViewBackend::Library => Store::Library,
            ViewBackend::System => Store::Profile,
        };
        Some((view, store))
    }

    pub fn find_cmd(&mut self, p: usize, cmd: FindCmd) {
        match cmd {
            // ponytail: a bookmark has no folder to keep to; store one in View if wanted.
            FindCmd::AddBookmark if self.tab(p).search_within().is_some() => self
                .toasts
                .info(crate::i18n::tr("bookmarks.folder_search_not_yet").to_owned()),
            FindCmd::AddBookmark => match self.bookmark_of_tab(p) {
                Some((view, store)) => {
                    self.find.editor = Some(Editor {
                        view,
                        store,
                        replaces: None,
                        error: None,
                        recording: false,
                    });
                }
                None => self
                    .toasts
                    .info(crate::i18n::tr("bookmarks.open_search_to_bookmark").to_owned()),
            },
            FindCmd::Edit(store, id) => {
                if let Some(view) = self.bookmark(store, id) {
                    self.find.editor = Some(Editor {
                        view,
                        store,
                        replaces: Some(id),
                        error: None,
                        recording: false,
                    });
                }
            }
            FindCmd::Save {
                store,
                view,
                replaces,
            } => self.save_bookmark(store, *view, replaces),
            FindCmd::AskDelete(store, id) => {
                if let Some(v) = self.bookmark(store, id) {
                    self.dialog = Some(crate::dialogs::Dialog::Confirm {
                        text: crate::i18n::tr("bookmarks.delete_bookmark_confirm")
                            .replace("{name}", &v.name),
                        yes: crate::i18n::tr("menu.delete"),
                        on_yes: Action::Find(FindCmd::Delete(store, id)),
                    });
                }
            }
            FindCmd::Delete(store, id) => match store {
                Store::Profile => self.settings.bookmarks.retain(|v| v.id != id),
                Store::Library => {
                    self.library
                        .spawn_try("keel-bookmark", "Delete bookmark", move |b| {
                            b.delete_view(id)?;
                            Ok(None)
                        });
                    self.library.refresh_meta_soon();
                }
            },
            FindCmd::Move(store, id, by) => self.move_bookmark(store, id, by),
            FindCmd::Open(store, id) => match self.bookmark(store, id) {
                Some(view) => self.open_bookmark(p, &view),
                None => self
                    .toasts
                    .error(crate::i18n::tr("bookmarks.bookmark_gone").to_owned()),
            },
            FindCmd::Organize => self.find.organize = true,
            FindCmd::EditFilters => {
                self.find.filters = Some(FiltersEditor {
                    list: self.settings.filters.clone(),
                    selected: None,
                    error: None,
                });
            }
            FindCmd::Remember(text) => {
                self.find
                    .history
                    .record(&text, &self.settings.search_history);
            }
            FindCmd::ClearHistory => {
                self.find.history.clear();
                self.toasts
                    .info(crate::i18n::tr("bookmarks.history_cleared").to_owned());
            }
        }
    }

    fn save_bookmark(&mut self, store: Store, mut view: View, replaces: Option<i64>) {
        match store {
            Store::Profile => {
                let list = &mut self.settings.bookmarks;
                match replaces.and_then(|id| list.iter_mut().find(|v| v.id == id)) {
                    Some(old) => {
                        view.id = old.id;
                        *old = view;
                    }
                    None => {
                        view.id = list.iter().map(|v| v.id).max().unwrap_or(0) + 1;
                        if view.position == 0 {
                            view.position = (list.iter())
                                .filter(|v| v.folder == view.folder)
                                .map(|v| v.position)
                                .max()
                                .unwrap_or(0)
                                + 1;
                        }
                        list.push(view);
                    }
                }
            }
            Store::Library => {
                self.library
                    .spawn_try("keel-bookmark", "Save bookmark", move |b| {
                        match replaces {
                            Some(id) => {
                                view.id = id;
                                b.update_view(&view)?;
                            }
                            None => b.add_view(&view)?,
                        }
                        Ok(None)
                    });
                self.library.refresh_meta_soon();
            }
        }
    }

    /// Moves a bookmark one place inside its folder; the folder is numbered 1..n again.
    fn move_bookmark(&mut self, store: Store, id: i64, by: i64) {
        let Some(me) = self.bookmark(store, id) else {
            return;
        };
        let mut folder: Vec<(Store, View)> = (self.bookmarks().into_iter())
            .filter(|(_, v)| v.folder == me.folder)
            .collect();
        let Some(i) = folder.iter().position(|(s, v)| *s == store && v.id == id) else {
            return;
        };
        let j = i as i64 + by;
        if j < 0 || j >= folder.len() as i64 {
            return;
        }
        folder.swap(i, j as usize);
        let mut library = Vec::new();
        for (n, (s, mut v)) in folder.into_iter().enumerate() {
            let position = n as i64 + 1;
            if v.position == position {
                continue;
            }
            v.position = position;
            match s {
                Store::Profile => {
                    if let Some(b) = self.settings.bookmarks.iter_mut().find(|b| b.id == v.id) {
                        b.position = position;
                    }
                }
                Store::Library => library.push(v),
            }
        }
        if !library.is_empty() {
            // Shown in the new order at once; the library confirms with its next read.
            for v in &library {
                if let Some(old) = self.library.views.iter_mut().find(|o| o.id == v.id) {
                    old.position = v.position;
                }
            }
            self.library
                .spawn_try("keel-bookmark", "Move bookmark", move |b| {
                    for v in &library {
                        b.update_view(v)?;
                    }
                    Ok(None)
                });
            self.library.refresh_meta_soon();
        }
    }

    /// Runs a bookmark in pane `p`: in its search tab when one is active, else a new one.
    pub fn open_bookmark(&mut self, p: usize, view: &View) {
        let library = view.backend == ViewBackend::Library;
        if library && !self.library.is_open() {
            return self.toasts.error(
                crate::i18n::tr("bookmarks.library_not_open").replace("{name}", &view.name),
            );
        }
        let p = if self.dual { p } else { 0 };
        if !self.tab(p).is_open_search() {
            let tab = crate::tab::Tab::search(self.tab(p).dir.clone());
            let pane = &mut self.panes[p];
            pane.tabs.push(tab);
            pane.active = pane.tabs.len() - 1;
        }
        let pane = &mut self.panes[p];
        match view.layout.as_str() {
            "grid" => pane.view = crate::pane::ViewMode::Grid,
            "details" => pane.view = crate::pane::ViewMode::Details,
            _ => {}
        }
        let tab = pane.tab_mut();
        tab.library_search = library;
        tab.search_filter = view.filter.clone();
        tab.set_match_toggles(view.matching);
        tab.sort = parse_sort(&view.sort).unwrap_or((SortKey::Name, true));
        if let TabKind::Search { query, due, .. } = &mut tab.kind {
            query.clone_from(&view.query);
            *due = Some(Instant::now());
        }
        self.active = p;
        self.find
            .history
            .record(&view.query, &self.settings.search_history);
        self.ctx.request_repaint();
    }
}

// --- Drawing ---

/// The search bar's ☆ menu: add, organize, and every bookmark (folders as submenus).
pub fn menu(ui: &mut egui::Ui, out: &mut Vec<Action>) {
    let list = published(ui.ctx()).bookmarks.clone();
    ui.menu_button("☆", |ui| {
        let add = crate::keys::shortcut_label("Ctrl+D");
        if ui
            .add(egui::Button::new(crate::i18n::tr("bookmarks.add_bookmark")).shortcut_text(add))
            .clicked()
        {
            out.push(Action::Find(FindCmd::AddBookmark));
            ui.close_menu();
        }
        if ui
            .button(crate::i18n::tr("bookmarks.organize_bookmarks"))
            .clicked()
        {
            out.push(Action::Find(FindCmd::Organize));
            ui.close_menu();
        }
        if !list.is_empty() {
            ui.separator();
        }
        let mut i = 0;
        while i < list.len() {
            let folder = list[i].1.folder.clone();
            let n = list[i..]
                .iter()
                .take_while(|(_, v)| v.folder == folder)
                .count();
            let group = &list[i..i + n];
            let rows = |ui: &mut egui::Ui, out: &mut Vec<Action>| {
                for (s, v) in group {
                    let mut b = egui::Button::new(v.name.as_str());
                    if !v.shortcut.is_empty() {
                        b = b.shortcut_text(crate::keys::shortcut_label(&v.shortcut));
                    }
                    if ui.add(b).on_hover_text(v.query.as_str()).clicked() {
                        out.push(Action::Find(FindCmd::Open(*s, v.id)));
                        ui.close_menu();
                    }
                }
            };
            match folder.as_str() {
                "" => rows(ui, out),
                f => {
                    ui.menu_button(format!("🗀 {f}"), |ui| rows(ui, out));
                }
            }
            i += n;
        }
    })
    .response
    .on_hover_text(crate::i18n::tr("sidebar.bookmarks"));
}

/// The sidebar's Bookmarks section's rows (`sidebar::Sidebar::ui` draws the header and
/// shows the section only while there are bookmarks).
pub fn sidebar(ui: &mut egui::Ui, out: &mut Vec<Action>) {
    use crate::sidebar::row;
    use crate::sidebar_glyphs::Glyph;
    let list = published(ui.ctx()).bookmarks.clone();
    let row = |ui: &mut egui::Ui, s: Store, v: &View, out: &mut Vec<Action>| {
        let mut tip = v.query.clone();
        if !v.shortcut.is_empty() {
            tip.push_str(&format!(
                "
{}",
                crate::keys::shortcut_label(&v.shortcut)
            ));
        }
        let id = egui::Id::new(("sidebar-bookmark", v.id, &v.folder, &v.name));
        let r = row(
            ui,
            id,
            Glyph::View,
            &v.name,
            false,
            egui::Sense::click(),
            |_| (),
        )
        .on_hover_text(tip);
        if r.clicked() {
            out.push(Action::Find(FindCmd::Open(s, v.id)));
        }
        r.context_menu(|ui| {
            for (text, cmd) in [
                (crate::i18n::tr("menu.open"), FindCmd::Open(s, v.id)),
                (
                    crate::i18n::tr("bookmarks.edit_bookmark"),
                    FindCmd::Edit(s, v.id),
                ),
                (
                    crate::i18n::tr("bookmarks.delete_bookmark"),
                    FindCmd::AskDelete(s, v.id),
                ),
                (
                    crate::i18n::tr("bookmarks.organize_bookmarks"),
                    FindCmd::Organize,
                ),
            ] {
                if ui.button(text).clicked() {
                    out.push(Action::Find(cmd));
                    ui.close_menu();
                }
            }
        });
    };
    let mut folders: Vec<&str> = Vec::new();
    for (s, v) in list.iter().filter(|(_, v)| v.folder.is_empty()) {
        row(ui, *s, v, out);
    }
    for (_, v) in &list {
        if !v.folder.is_empty() && !folders.contains(&v.folder.as_str()) {
            folders.push(&v.folder);
        }
    }
    for f in folders {
        egui::CollapsingHeader::new(f)
            .id_salt(("bookmark-folder", f))
            .default_open(true)
            .show(ui, |ui| {
                for (s, v) in list.iter().filter(|(_, v)| v.folder == f) {
                    row(ui, *s, v, out);
                }
            });
    }
}

/// The bookmark dialog, Organize bookmarks and the filter editor.
pub fn windows(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    editor(ctx, s, out);
    organize(ctx, s, out);
    filters(ctx, s);
}

fn toggles_ui(ui: &mut egui::Ui, m: &mut MatchToggles) {
    ui.checkbox(&mut m.case, crate::i18n::tr("bookmarks.match_case"));
    ui.checkbox(&mut m.regex, crate::i18n::tr("bookmarks.regex"));
}

fn editor(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let all = s.bookmarks();
    let library_open = s.library.is_open();
    let filter_names = crate::search_filters::names(&s.settings.filters);
    let Some(ed) = s.find.editor.as_mut() else {
        return;
    };
    // Recording a shortcut: the next key pressed (Esc cancels).
    if ed.recording {
        ctx.input_mut(|i| {
            i.events.retain(|e| match e {
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if *key != Key::Escape {
                        ed.view.shortcut = shortcut_text(*modifiers, *key);
                    }
                    ed.recording = false;
                    false
                }
                _ => true,
            })
        });
    }
    let mut answer: Option<bool> = None;
    let title = match ed.replaces {
        Some(_) => crate::i18n::tr("bookmarks.edit_title"),
        None => crate::i18n::tr("bookmarks.add_title"),
    };
    let folders: Vec<String> = {
        let mut f: Vec<String> = (all.iter())
            .map(|(_, v)| v.folder.clone())
            .filter(|f| !f.is_empty())
            .collect();
        f.sort();
        f.dedup();
        f
    };
    let modal = Modal::new(Id::new("keel-bookmark")).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.heading(title);
        egui::Grid::new("bookmark-grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label(crate::i18n::tr("toolbar.name"));
                ui.add(egui::TextEdit::singleline(&mut ed.view.name).desired_width(300.0));
                ui.end_row();
                ui.label(crate::i18n::tr("tab.search"));
                ui.add(egui::TextEdit::singleline(&mut ed.view.query).desired_width(300.0));
                ui.end_row();
                ui.label(crate::i18n::tr("bookmarks.search_with"));
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut ed.view.backend,
                        ViewBackend::System,
                        crate::i18n::tr("bookmarks.system"),
                    );
                    ui.add_enabled_ui(
                        library_open || ed.view.backend == ViewBackend::Library,
                        |ui| {
                            ui.selectable_value(
                                &mut ed.view.backend,
                                ViewBackend::Library,
                                crate::i18n::tr("tab.library"),
                            );
                        },
                    );
                });
                ui.end_row();
                ui.label(crate::i18n::tr("pane.filter"));
                egui::ComboBox::from_id_salt("bookmark-filter")
                    .selected_text(crate::search_filters::shown(&ed.view.filter))
                    .show_ui(ui, |ui| {
                        for name in &filter_names {
                            ui.selectable_value(
                                &mut ed.view.filter,
                                crate::search_filters::value(name),
                                name.as_str(),
                            );
                        }
                    });
                ui.end_row();
                ui.label(crate::i18n::tr("bookmarks.match"));
                ui.horizontal(|ui| toggles_ui(ui, &mut ed.view.matching));
                ui.end_row();
                ui.label(crate::i18n::tr("bookmarks.sort"));
                let mut sort = parse_sort(&ed.view.sort).unwrap_or((SortKey::Name, true));
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("bookmark-sort")
                        .selected_text(match sort.0 {
                            SortKey::Name => crate::i18n::tr("toolbar.name"),
                            SortKey::Size => crate::i18n::tr("toolbar.size"),
                            SortKey::Modified => crate::i18n::tr("bookmarks.date_modified"),
                            SortKey::Ext => crate::i18n::tr("bookmarks.extension"),
                        })
                        .show_ui(ui, |ui| {
                            for (key, label) in [
                                (SortKey::Name, crate::i18n::tr("toolbar.name")),
                                (SortKey::Size, crate::i18n::tr("toolbar.size")),
                                (
                                    SortKey::Modified,
                                    crate::i18n::tr("bookmarks.date_modified"),
                                ),
                                (SortKey::Ext, crate::i18n::tr("bookmarks.extension")),
                            ] {
                                ui.selectable_value(&mut sort.0, key, label);
                            }
                        });
                    let mut desc = !sort.1;
                    ui.checkbox(&mut desc, crate::i18n::tr("toolbar.descending"));
                    sort.1 = !desc;
                });
                ed.view.sort = sort_text(sort);
                ui.end_row();
                ui.label(crate::i18n::tr("inspector.folder"));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.view.folder)
                            .hint_text(crate::i18n::tr("bookmarks.folder_hint"))
                            .desired_width(240.0),
                    );
                    if !folders.is_empty() {
                        ui.menu_button("▾", |ui| {
                            if ui.button(crate::i18n::tr("bookmarks.top_level")).clicked() {
                                ed.view.folder.clear();
                                ui.close_menu();
                            }
                            for f in &folders {
                                if ui.button(f.as_str()).clicked() {
                                    ed.view.folder.clone_from(f);
                                    ui.close_menu();
                                }
                            }
                        });
                    }
                });
                ui.end_row();
                ui.label(crate::i18n::tr("bookmarks.shortcut"));
                ui.horizontal(|ui| {
                    let shown = match (ed.recording, ed.view.shortcut.as_str()) {
                        (true, _) => crate::i18n::tr("bookmarks.press_key").to_owned(),
                        (false, "") => crate::i18n::tr("toolbar.none").to_owned(),
                        (false, k) => crate::keys::shortcut_label(k),
                    };
                    ui.label(RichText::new(shown).monospace());
                    if ui.button(crate::i18n::tr("bookmarks.record")).clicked() {
                        ed.recording = true;
                    }
                    if !ed.view.shortcut.is_empty()
                        && ui.button(crate::i18n::tr("bookmarks.clear")).clicked()
                    {
                        ed.view.shortcut.clear();
                    }
                });
                ui.end_row();
                if ed.replaces.is_none() && ed.view.backend == ViewBackend::System {
                    ui.label(crate::i18n::tr("bookmarks.keep_in"));
                    ui.horizontal(|ui| {
                        ui.selectable_value(
                            &mut ed.store,
                            Store::Profile,
                            crate::i18n::tr("bookmarks.this_profile"),
                        );
                        ui.add_enabled_ui(library_open, |ui| {
                            ui.selectable_value(
                                &mut ed.store,
                                Store::Library,
                                crate::i18n::tr("tab.library"),
                            );
                        })
                        .response
                        .on_hover_text(crate::i18n::tr("bookmarks.library_bookmarks_tip"));
                    });
                    ui.end_row();
                }
            });
        if ed.replaces.is_none() && ed.view.backend == ViewBackend::Library {
            ed.store = Store::Library;
        }
        if let Some(e) = &ed.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button(crate::i18n::tr("dialogs.save")).clicked() {
                answer = Some(true);
            }
            if ui.button(crate::i18n::tr("dialogs.cancel")).clicked() {
                answer = Some(false);
            }
        });
    });
    if answer.is_none() && modal.should_close() && !ed.recording {
        answer = Some(false);
    }
    match answer {
        Some(true) => {
            ed.view.name = ed.view.name.trim().to_owned();
            ed.view.folder = (ed.view.folder.split('/'))
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("/");
            let me = ed.replaces.map(|id| (ed.store, id));
            let problem = keel_core::check_view(&ed.view)
                .err()
                .map(|e| format!("{e:#}"))
                .or_else(|| shortcut_problem(&ed.view.shortcut, me, &all))
                .or_else(|| {
                    (ed.store == Store::Profile && ed.view.backend == ViewBackend::Library)
                        .then(|| crate::i18n::tr("bookmarks.library_searches_kept").to_owned())
                });
            match problem {
                Some(p) => ed.error = Some(p),
                None => {
                    let ed = s.find.editor.take().expect("open");
                    out.push(Action::Find(FindCmd::Save {
                        store: ed.store,
                        view: Box::new(ed.view),
                        replaces: ed.replaces,
                    }));
                }
            }
        }
        Some(false) => s.find.editor = None,
        None => {}
    }
}

fn organize(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    if !s.find.organize || s.find.editor.is_some() || s.dialog.is_some() {
        return;
    }
    let list = s.bookmarks();
    let mut close = false;
    let modal = Modal::new(Id::new("keel-bookmarks")).show(ctx, |ui| {
        ui.set_width(560.0);
        ui.heading(crate::i18n::tr("bookmarks.organize_title"));
        if list.is_empty() {
            ui.weak(crate::i18n::tr("bookmarks.no_bookmarks"));
        }
        egui::ScrollArea::vertical()
            .max_height(420.0)
            .show(ui, |ui| {
                let mut last_folder: Option<&str> = None;
                for (i, (st, v)) in list.iter().enumerate() {
                    if last_folder != Some(v.folder.as_str()) {
                        last_folder = Some(v.folder.as_str());
                        if !v.folder.is_empty() {
                            ui.add_space(4.0);
                            ui.strong(format!("🗀 {}", v.folder));
                        }
                    }
                    let same = |j: usize| list.get(j).is_some_and(|(_, o)| o.folder == v.folder);
                    ui.horizontal(|ui| {
                        if !v.folder.is_empty() {
                            ui.add_space(16.0);
                        }
                        let up = i > 0 && same(i - 1);
                        if ui.add_enabled(up, egui::Button::new("↑").small()).clicked() {
                            out.push(Action::Find(FindCmd::Move(*st, v.id, -1)));
                        }
                        let down = same(i + 1);
                        if ui
                            .add_enabled(down, egui::Button::new("↓").small())
                            .clicked()
                        {
                            out.push(Action::Find(FindCmd::Move(*st, v.id, 1)));
                        }
                        ui.label(v.name.as_str()).on_hover_text(v.query.as_str());
                        ui.weak(match (st, v.backend) {
                            (Store::Library, ViewBackend::Library) => {
                                crate::i18n::tr("tab.library")
                            }
                            (Store::Library, ViewBackend::System) => {
                                crate::i18n::tr("bookmarks.system_in_library")
                            }
                            (Store::Profile, _) => crate::i18n::tr("bookmarks.system"),
                        });
                        if !v.shortcut.is_empty() {
                            ui.weak(crate::keys::shortcut_label(&v.shortcut));
                        }
                        if ui
                            .small_button(crate::i18n::tr("bookmarks.edit_bookmark"))
                            .clicked()
                        {
                            out.push(Action::Find(FindCmd::Edit(*st, v.id)));
                        }
                        if ui
                            .small_button(crate::i18n::tr("bookmarks.delete_bookmark"))
                            .clicked()
                        {
                            out.push(Action::Find(FindCmd::AskDelete(*st, v.id)));
                        }
                    });
                }
            });
        ui.add_space(6.0);
        ui.weak(crate::i18n::tr("bookmarks.organize_hint"));
        if ui.button(crate::i18n::tr("toolbar.close")).clicked() {
            close = true;
        }
    });
    if close || modal.should_close() {
        s.find.organize = false;
    }
}

fn filters(ctx: &egui::Context, s: &mut AppState) {
    let Some(ed) = s.find.filters.as_mut() else {
        return;
    };
    let mut answer: Option<bool> = None;
    let modal = Modal::new(Id::new("keel-filters")).show(ctx, |ui| {
        ui.set_width(520.0);
        ui.heading(crate::i18n::tr("bookmarks.filters_title"));
        ui.weak(crate::i18n::tr("bookmarks.filters_note"));
        ui.add_space(4.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(160.0);
                for (i, f) in ed.list.iter().enumerate() {
                    if ui
                        .selectable_label(ed.selected == Some(i), f.name.as_str())
                        .clicked()
                    {
                        ed.selected = Some(i);
                    }
                }
                if ui.button(crate::i18n::tr("bookmarks.new_filter")).clicked() {
                    let n = (1..).find(|n| {
                        let name =
                            crate::i18n::tr("bookmarks.filter_n").replace("{n}", &n.to_string());
                        !ed.list.iter().any(|f| f.name == name)
                    });
                    ed.list.push(CustomFilter {
                        name: crate::i18n::tr("bookmarks.filter_n")
                            .replace("{n}", &n.unwrap_or(1).to_string()),
                        ..CustomFilter::default()
                    });
                    ed.selected = Some(ed.list.len() - 1);
                }
            });
            ui.separator();
            ui.vertical(|ui| match ed.selected.filter(|i| *i < ed.list.len()) {
                Some(i) => {
                    let f = &mut ed.list[i];
                    ui.label(crate::i18n::tr("toolbar.name"));
                    ui.text_edit_singleline(&mut f.name);
                    ui.label(crate::i18n::tr("tab.search"));
                    ui.add(
                        egui::TextEdit::singleline(&mut f.search)
                            .hint_text("ext:psd;ai dm:thisyear"),
                    );
                    toggles_ui(ui, &mut f.matching);
                    if ui
                        .button(crate::i18n::tr("bookmarks.delete_filter"))
                        .clicked()
                    {
                        ed.list.remove(i);
                        ed.selected = None;
                    }
                }
                None => {
                    ui.weak(crate::i18n::tr("bookmarks.pick_filter"));
                }
            });
        });
        if let Some(e) = &ed.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button(crate::i18n::tr("dialogs.save")).clicked() {
                answer = Some(true);
            }
            if ui.button(crate::i18n::tr("dialogs.cancel")).clicked() {
                answer = Some(false);
            }
        });
    });
    if answer.is_none() && modal.should_close() {
        answer = Some(false);
    }
    match answer {
        Some(true) => {
            for f in &mut ed.list {
                f.name = f.name.trim().to_owned();
            }
            let problem = (0..ed.list.len()).find_map(|i| {
                let others: Vec<CustomFilter> = (ed.list.iter().enumerate())
                    .filter(|(j, _)| *j != i)
                    .map(|(_, f)| f.clone())
                    .collect();
                crate::search_filters::check(&ed.list[i], &others)
                    .err()
                    .map(|e| (i, e))
            });
            match problem {
                Some((i, e)) => {
                    ed.selected = Some(i);
                    ed.error = Some(e);
                }
                None => {
                    let list = s.find.filters.take().expect("open").list;
                    // Tabs on a filter that is gone go back to Everything.
                    for pane in &mut s.panes {
                        for tab in &mut pane.tabs {
                            let gone = !tab.search_filter.is_empty()
                                && !crate::search_filters::BUILTIN
                                    .contains(&tab.search_filter.as_str())
                                && !list.iter().any(|f| f.name == tab.search_filter);
                            if gone {
                                tab.search_filter.clear();
                            }
                        }
                    }
                    s.settings.filters = list;
                }
            }
        }
        Some(false) => s.find.filters = None,
        None => {}
    }
}

/// Settings → Search: history, filters and bookmarks.
pub fn settings_page(
    ui: &mut egui::Ui,
    s: &mut crate::settings::Settings,
    pending: &mut Vec<FindCmd>,
) {
    use crate::search_history::{Order, MAX_LIMIT};
    let h = &mut s.search_history;
    ui.strong(crate::i18n::tr("bookmarks.search_history"));
    let mut off = !h.save;
    if ui
        .checkbox(&mut off, crate::i18n::tr("bookmarks.do_not_save_history"))
        .on_hover_text(crate::i18n::tr("bookmarks.do_not_save_history_tip"))
        .changed()
    {
        h.save = !off;
    }
    ui.add_enabled_ui(h.save, |ui| {
        ui.horizontal(|ui| {
            ui.label(crate::i18n::tr("bookmarks.keep_at_most"));
            ui.add(egui::DragValue::new(&mut h.max).range(1..=MAX_LIMIT));
            ui.label(crate::i18n::tr("bookmarks.searches"));
        });
        ui.checkbox(
            &mut h.suggestions,
            crate::i18n::tr("bookmarks.always_suggest"),
        )
        .on_hover_text(crate::i18n::tr("bookmarks.always_suggest_tip"));
        ui.horizontal(|ui| {
            ui.label(crate::i18n::tr("bookmarks.suggest"));
            ui.selectable_value(
                &mut h.order,
                Order::MostRecent,
                crate::i18n::tr("bookmarks.most_recent"),
            );
            ui.selectable_value(
                &mut h.order,
                Order::MostUsed,
                crate::i18n::tr("bookmarks.most_used"),
            );
        });
    });
    if ui
        .button(crate::i18n::tr("bookmarks.clear_history"))
        .clicked()
    {
        pending.push(FindCmd::ClearHistory);
    }
    ui.add_space(8.0);
    ui.strong(crate::i18n::tr("bookmarks.filters_and_bookmarks"));
    ui.horizontal(|ui| {
        if ui
            .button(crate::i18n::tr("bookmarks.edit_filters"))
            .clicked()
        {
            pending.push(FindCmd::EditFilters);
        }
        if ui
            .button(crate::i18n::tr("bookmarks.organize_bookmarks"))
            .clicked()
        {
            pending.push(FindCmd::Organize);
        }
    });
    ui.weak(
        crate::i18n::tr("bookmarks.counts")
            .replace("{f}", &s.filters.len().to_string())
            .replace("{b}", &s.bookmarks.len().to_string()),
    );
}

#[cfg(test)]
#[path = "bookmarks_tests.rs"]
mod tests;
