//! Search tab: a query box over the active pane's results table, with Everything's search
//! toggles (Match Case Ctrl+I, Match Whole Words Ctrl+B, Match Path Ctrl+U, Match
//! Diacritics Ctrl+M, Regex Ctrl+R) beside it and a Search menu. Typing is debounced
//! (Settings: search as you type and its delay); `AppState::tick` sends due queries to
//! `worker::SearchWorker`. The first query asks for `PAGE` results; Load more asks for
//! more, up to the Most results setting.

use crate::a11y::Named;
use crate::bookmarks::FindCmd;
use crate::keys::Action;
use crate::pane::Pane;
use crate::search_history::SUGGESTIONS;
use crate::tab::{Nav, TabKind};
use egui::{Key, Modifiers};
use keel_search::{Query, Searcher};
use std::time::{Duration, Instant};

/// The default delay of search as you type.
pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// A remote (SFTP, cloud) name search walks the host: it stops after this many.
pub const MAX_HITS: u32 = 500;
/// The first query of a search asks for this many results.
pub const PAGE: u32 = 1_000;
/// The Most results setting's range.
pub const MAX_RESULTS: std::ops::RangeInclusive<u32> = 1_000..=1_000_000;

#[cfg(test)]
pub const NOT_RUNNING: &str =
    "Everything is not running. Start Everything.exe (voidtools) to search.";

/// The query box's hint for the backend that answers it (the System backend's `name`):
/// Everything takes its own syntax; Keel's index takes names, wildcards, quotes,
/// `folder:`, `in:` and `regex:` (not `ext:` or `dm:`); the library its own terms.
pub fn hint(backend: Option<&str>, library: bool) -> String {
    match backend {
        _ if library => crate::i18n::tr("search_tab.hint_library").to_owned(),
        Some("Everything") => crate::i18n::tr("search_tab.hint_everything").to_owned(),
        Some("Keel index") => crate::i18n::tr("search_tab.hint_keel_index").to_owned(),
        _ => crate::i18n::tr("search_tab.hint_default").to_owned(),
    }
}

/// Everything's search toggles (its Search menu), per search tab.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Toggles {
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub match_diacritics: bool,
    pub regex: bool,
}

/// The toggles as a bookmark or filter keeps them (`keel_core::MatchToggles`).
pub fn match_toggles(t: Toggles) -> keel_core::MatchToggles {
    keel_core::MatchToggles {
        case: t.match_case,
        whole_words: t.whole_word,
        path: t.match_path,
        diacritics: t.match_diacritics,
        regex: t.regex,
        ..Default::default()
    }
}

/// The tab's toggles for a bookmark's or filter's (`prefix`, `suffix`, `ignore_punctuation`
/// and `ignore_whitespace` are the HTTP search's and not the search tab's).
pub fn toggles_of(m: keel_core::MatchToggles) -> Toggles {
    Toggles {
        match_case: m.case,
        whole_word: m.whole_words,
        match_path: m.path,
        match_diacritics: m.diacritics,
        regex: m.regex,
    }
}

/// One of the [`Toggles`].

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toggle {
    Case,
    WholeWord,
    Path,
    Diacritics,
    Regex,
}

impl Toggle {
    pub const ALL: [Toggle; 5] = [
        Toggle::Case,
        Toggle::WholeWord,
        Toggle::Path,
        Toggle::Diacritics,
        Toggle::Regex,
    ];

    pub fn label(self) -> String {
        match self {
            Toggle::Case => crate::i18n::tr("bookmarks.match_case").to_owned(),
            Toggle::WholeWord => crate::i18n::tr("search_tab.match_whole_words").to_owned(),
            Toggle::Path => crate::i18n::tr("search_tab.match_path").to_owned(),
            Toggle::Diacritics => crate::i18n::tr("search_tab.match_diacritics").to_owned(),
            Toggle::Regex => crate::i18n::tr("bookmarks.regex").to_owned(),
        }
    }

    /// The button's text in the search bar.
    pub fn icon(self) -> &'static str {
        match self {
            Toggle::Case => "Aa",
            Toggle::WholeWord => "ab",
            Toggle::Path => "\\",
            Toggle::Diacritics => "\u{e9}",
            Toggle::Regex => ".*",
        }
    }

    /// Everything's shortcuts, the default keys (`keymap::commands`; in a search tab,
    /// Ctrl+I is the Inspector elsewhere).
    pub fn shortcut(self) -> &'static str {
        match self {
            Toggle::Case => "Ctrl+I",
            Toggle::WholeWord => "Ctrl+B",
            Toggle::Path => "Ctrl+U",
            Toggle::Diacritics => "Ctrl+M",
            Toggle::Regex => "Ctrl+R",
        }
    }

    pub fn of(self, t: &mut Toggles) -> &mut bool {
        match self {
            Toggle::Case => &mut t.match_case,
            Toggle::WholeWord => &mut t.whole_word,
            Toggle::Path => &mut t.match_path,
            Toggle::Diacritics => &mut t.match_diacritics,
            Toggle::Regex => &mut t.regex,
        }
    }

    pub fn get(self, mut t: Toggles) -> bool {
        *self.of(&mut t)
    }
}

/// `[search]` in the profile's settings (Everything's Home and Search options).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SearchSettings {
    /// The toggles a new search tab starts with.
    pub toggles: Toggles,
    /// Run the query while typing; off, Enter runs it.
    pub as_you_type: bool,
    /// How long typing has to pause before the query runs.
    pub delay_ms: u64,
    /// A `*` or `?` term matches the whole name (`*.pdf`); off, anywhere in it.
    pub whole_filename_wildcards: bool,
    /// Load more stops at this many results.
    pub max_results: u32,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            toggles: Toggles::default(),
            as_you_type: true,
            delay_ms: DEBOUNCE.as_millis() as u64,
            whole_filename_wildcards: true,
            max_results: 10_000,
        }
    }
}

impl SearchSettings {
    /// The wait after an edit before the query runs; None when only Enter runs it.
    pub fn delay(&self) -> Option<Duration> {
        self.as_you_type
            .then_some(Duration::from_millis(self.delay_ms.min(2_000)))
    }

    pub fn max(&self) -> u32 {
        self.max_results
            .clamp(*MAX_RESULTS.start(), *MAX_RESULTS.end())
    }

    /// The limit of a query: the first page for a new one (`limit` 0), else `limit`.
    pub fn limit(&self, limit: u32) -> u32 {
        match limit {
            0 => PAGE.min(self.max()),
            n => n.min(self.max()),
        }
    }

    /// What Load more asks for after `limit`.
    pub fn more(&self, limit: u32) -> u32 {
        self.limit(limit).saturating_mul(4).min(self.max())
    }

    /// The backend query for `text` with these toggles, asking for `max` results.
    pub fn query(&self, text: String, t: Toggles, max: u32) -> Query {
        Query {
            text,
            max,
            regex: t.regex,
            match_case: t.match_case,
            whole_word: t.whole_word,
            match_path: t.match_path,
            match_diacritics: t.match_diacritics,
            whole_filename_wildcards: self.whole_filename_wildcards,
            ..Query::default()
        }
    }
}

/// Where the app leaves the search settings for the search bar each frame.
pub fn settings_id() -> egui::Id {
    egui::Id::new("keel-search-settings")
}

/// What the user is told when a query fails.
pub fn banner(err: &str) -> String {
    if err.contains("Everything is not running") {
        crate::i18n::tr("search_tab.not_running").to_owned()
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

/// `text` with the `marks` byte ranges (from [`keel_search::Highlighter`]) drawn in the
/// strong text colour on a light selection tint.
pub fn highlighted(
    ui: &egui::Ui,
    text: &str,
    marks: &[(usize, usize)],
    color: Option<egui::Color32>,
) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let font = egui::TextStyle::Body.resolve(ui.style());
    let plain = TextFormat::simple(font.clone(), color.unwrap_or(ui.visuals().text_color()));
    let mark = TextFormat {
        background: ui.visuals().selection.bg_fill.gamma_multiply(0.5),
        ..TextFormat::simple(font, ui.visuals().strong_text_color())
    };
    let mut job = LayoutJob::default();
    let mut at = 0;
    for &(a, b) in marks {
        let fits = at <= a && a < b && b <= text.len();
        if !fits || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
            continue;
        }
        job.append(&text[at..a], 0.0, plain.clone());
        job.append(&text[a..b], 0.0, mark.clone());
        at = b;
    }
    job.append(&text[at..], 0.0, plain);
    job
}

/// The Search menu: the toggles, Load more, Open path and the copy commands.
fn search_menu(ui: &mut egui::Ui, toggles: Toggles, more: bool, out: &mut Vec<Action>) {
    let mut item = |ui: &mut egui::Ui, button: egui::Button<'_>, action: Action| {
        if ui.add(button).clicked() {
            out.push(action);
            ui.close_menu();
        }
    };
    for t in Toggle::ALL {
        let button = egui::Button::new(t.label())
            .selected(t.get(toggles))
            .shortcut_text(crate::keymap::shortcut(
                ui.ctx(),
                &Action::SearchToggle(t),
                t.shortcut(),
            ));
        item(ui, button, Action::SearchToggle(t));
    }
    ui.separator();
    if more {
        item(
            ui,
            egui::Button::new(crate::i18n::tr("search_tab.load_more_results")),
            Action::SearchMore,
        );
    }
    let open = egui::Button::new(crate::i18n::tr("search_tab.open_path"))
        .shortcut_text(crate::keys::shortcut_label("Ctrl+Enter"));
    item(ui, open, Action::OpenLocation);
    item(
        ui,
        egui::Button::new(crate::i18n::tr("menu.copy_name")),
        Action::CopyName,
    );
    item(
        ui,
        egui::Button::new(crate::i18n::tr("search_tab.copy_full_path")),
        Action::CopyPath,
    );
    ui.separator();
    item(
        ui,
        egui::Button::new(crate::i18n::tr("search_tab.search_settings")),
        Action::Settings,
    );
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

/// The query box. Editing schedules the query after the search delay (when searching as
/// you type); Enter runs it now; Up/Down move the result cursor while typing. The toggles
/// and the Search menu follow; with the Library backend the filter chips (`chip_row`)
/// follow too; a chip runs the query now.
pub fn bar(
    ui: &mut egui::Ui,
    pane: &mut Pane,
    platform: Option<&str>,
    lib: &crate::library::LibraryUi,
    out: &mut Vec<Action>,
) {
    let library = lib.is_open();
    let settings: SearchSettings = ui
        .ctx()
        .data(|d| d.get_temp(settings_id()))
        .unwrap_or_default();
    let focus = std::mem::take(&mut pane.focus_filter);
    let tab = pane.tab_mut();
    let (count, busy) = (tab.entries().len(), tab.loading);
    let TabKind::Search {
        query,
        due,
        toggles,
        limit,
        more,
        within,
        ..
    } = &mut tab.kind
    else {
        return;
    };
    let scoped = within.as_ref().map(|d| match d.name() {
        "" => d.display(),
        name => name.to_owned(),
    });
    let remote_scope = within.as_ref().is_some_and(|d| d.to_local_path().is_none());
    // Task 77: a file list tab searches its list: (shown, entries in the list).
    let list = (tab.filelist.as_ref()).map(|s| (s.label().to_owned(), s.len()));
    let shared = crate::bookmarks::published(ui.ctx()); // Task 76
    ui.horizontal(|ui| {
        if let Some((label, _)) = &list {
            ui.label(crate::i18n::tr("search_tab.file_list").replace("{label}", label));
        }
        // Task 29: the backend (the platform's, or the library index).
        let name = platform;
        let platform = platform.unwrap_or("System");
        let before = tab.library_search;
        let backends = list.is_none();
        if backends {
            let combo = egui::ComboBox::from_id_salt("search-backend")
                .width(110.0)
                .selected_text(if tab.library_search {
                    crate::i18n::tr("tab.library")
                } else {
                    platform
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut tab.library_search, false, platform);
                    // The library keeps to local folders only (`LibSearch`).
                    ui.add_enabled_ui(library && !remote_scope, |ui| {
                        ui.selectable_value(
                            &mut tab.library_search,
                            true,
                            crate::i18n::tr("tab.library"),
                        )
                        .on_disabled_hover_text(match remote_scope {
                            true => crate::i18n::tr("search_tab.library_remote_tip"),
                            false => crate::i18n::tr("search_tab.library_off_tip"),
                        });
                    });
                });
            let _ =
                crate::a11y::Named::named(combo.response, crate::i18n::tr("bookmarks.search_with"));
        }
        if tab.library_search != before {
            // Run the query again on the other backend now.
            *limit = 0;
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
        let sugg: Vec<String> = match focused || shown {
            true => crate::bookmarks::suggestions(&shared.history, query, SUGGESTIONS),
            false => Vec::new(),
        };
        if focused && ui.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowDown)) {
            st.forced = true;
            st.dismissed = None;
        }
        let open = focused
            && !sugg.is_empty()
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
                        let last = sugg.len() - 1;
                        st.highlight = Some(st.highlight.map_or(0, |h| (h + 1).min(last)));
                    }
                    if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        st.highlight = st.highlight.and_then(|h| h.checked_sub(1));
                    }
                    if let Some(h) = st.highlight {
                        if i.consume_key(Modifiers::NONE, Key::Enter) {
                            picked = sugg.get(h).cloned();
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
        if let Some(name) = &scoped {
            ui.weak(crate::i18n::tr("search_tab.in_scope").replace("{name}", name));
        }
        let hint_text = if list.is_some() {
            crate::i18n::tr("search_tab.list_hint").to_owned()
        } else {
            hint(name, tab.library_search)
        };
        let r = ui.add(
            egui::TextEdit::singleline(query)
                .id(id)
                .desired_width((ui.available_width() - 520.0).max(120.0))
                .hint_text(&hint_text),
        );
        // A query Everything's search language rejects shows its problem beside the box. Only for
        // the Everything backend: Keel's index, locate and the library read their own terms
        // (`in:` is a source there) until keel-query is wired into them.
        let everything_syntax = name == Some("Everything") && !tab.library_search && !toggles.regex;
        if list.is_none() && everything_syntax && !query.is_empty() {
            let cache_id = id.with("query_problem");
            let problem = ui.ctx().data_mut(|d| {
                let cache = d.get_temp_mut_or_default::<(String, Option<String>)>(cache_id);
                if cache.0 != *query {
                    cache.0 = query.clone();
                    cache.1 = crate::query_check::problem(query);
                }
                cache.1.clone()
            });
            if let Some(p) = problem {
                ui.colored_label(ui.visuals().warn_fg_color, &p)
                    .on_hover_text(crate::i18n::tr("search_tab.query_rejected"));
            }
        }
        // A pane's search goes back to its folder on Esc, on ×, or when the box is emptied.
        if scoped.is_some()
            && ((r.changed() && query.is_empty())
                || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)))
                || (ui.small_button("×"))
                    .tip(crate::i18n::tr("search_tab.back_to_folder"))
                    .clicked())
        {
            out.push(Action::CloseSearch);
        }
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
            *limit = 0;
            if let Some(delay) = settings.delay() {
                *due = Some(Instant::now() + delay);
                ui.ctx().request_repaint_after(delay);
            }
        }
        let enter = ui.input(|i| i.key_pressed(Key::Enter));
        if r.lost_focus() && enter {
            *limit = 0;
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
                for (i, text) in sugg.iter().enumerate() {
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
            *limit = 0;
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
        // The toggles (Everything's Search menu); the library has its own terms.
        ui.add_enabled_ui(!tab.library_search, |ui| {
            for t in Toggle::ALL {
                let text = egui::RichText::new(t.icon()).monospace();
                let keys =
                    crate::keymap::shortcut(ui.ctx(), &Action::SearchToggle(t), t.shortcut());
                let tip = match keys.is_empty() {
                    true => t.label(),
                    false => format!("{} ({keys})", t.label()),
                };
                let r = ui
                    .selectable_label(t.get(*toggles), text)
                    .on_hover_text(&tip)
                    .on_disabled_hover_text(crate::i18n::tr("search_tab.library_own_terms"));
                if crate::a11y::Named::named(r, &t.label()).clicked() {
                    out.push(Action::SearchToggle(t));
                }
            }
        });
        let capped = *limit > 0 && count as u32 >= *limit;
        let can_more = capped && *more && !tab.library_search;
        ui.menu_button(crate::i18n::tr("tab.search"), |ui| {
            search_menu(ui, *toggles, can_more, out)
        });
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
                if ui
                    .button(crate::i18n::tr("bookmarks.edit_filters"))
                    .clicked()
                {
                    out.push(Action::Find(FindCmd::EditFilters));
                }
            });
        let _ = crate::a11y::Named::named(combo.response, crate::i18n::tr("pane.filter"));
        if tab.search_filter != before {
            *limit = 0;
            *due = Some(Instant::now());
            ui.ctx().request_repaint();
        }
        crate::bookmarks::menu(ui, out);
        if busy {
            crate::anim::spinner(ui);
        }
        ui.weak(match (count, &list) {
            (n, Some((_, total))) => crate::i18n::tr("search_tab.of_total")
                .replace("{n}", &n.to_string())
                .replace("{total}", &total.to_string()),
            (1, None) => crate::i18n::tr("search_tab.one_result").to_owned(),
            (n, None) if capped => {
                crate::i18n::tr("search_tab.first_n_results").replace("{n}", &n.to_string())
            }
            (n, None) => crate::i18n::tr("search_tab.n_results").replace("{n}", &n.to_string()),
        });
        if can_more
            && ui
                .small_button(crate::i18n::tr("search_tab.load_more"))
                .on_hover_text(
                    crate::i18n::tr("search_tab.load_more_tip")
                        .replace("{n}", &settings.more(*limit).to_string())
                        .replace("{max}", &settings.max().to_string()),
                )
                .clicked()
        {
            out.push(Action::SearchMore);
        }
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
            *limit = 0;
            *due = Some(Instant::now());
            ui.ctx().request_repaint();
        }
    }
}

/// Settings rows (the General page's grid): search as you type and its delay, whole
/// filename wildcards, Most results, the toggles new searches start with, and quoted
/// copied paths.
pub fn settings_rows(ui: &mut egui::Ui, s: &mut SearchSettings, quote_paths: &mut bool) {
    ui.label(crate::i18n::tr("tab.search"));
    ui.horizontal(|ui| {
        ui.checkbox(
            &mut s.as_you_type,
            crate::i18n::tr("search_tab.search_as_you_type"),
        );
        ui.add_enabled(
            s.as_you_type,
            egui::DragValue::new(&mut s.delay_ms)
                .range(0..=2_000)
                .suffix(" ms"),
        )
        .on_hover_text(crate::i18n::tr("search_tab.delay_tip"));
    });
    ui.end_row();
    ui.label("");
    ui.checkbox(
        &mut s.whole_filename_wildcards,
        crate::i18n::tr("search_tab.whole_filename_wildcards"),
    )
    .on_hover_text(crate::i18n::tr("search_tab.whole_filename_wildcards_tip"));
    ui.end_row();
    let label = ui.label(crate::i18n::tr("search_tab.most_results"));
    let r = ui
        .add(
            egui::DragValue::new(&mut s.max_results)
                .range(MAX_RESULTS)
                .speed(100.0),
        )
        .on_hover_text(crate::i18n::tr("search_tab.most_results_tip"));
    let _ = r.labelled_by(label.id);
    ui.end_row();
    ui.label(crate::i18n::tr("search_tab.new_searches"));
    ui.horizontal_wrapped(|ui| {
        for t in Toggle::ALL {
            ui.checkbox(t.of(&mut s.toggles), t.label());
        }
    });
    ui.end_row();
    ui.label(crate::i18n::tr("menu.copy_path"));
    ui.checkbox(quote_paths, crate::i18n::tr("search_tab.in_double_quotes"))
        .on_hover_text(crate::i18n::tr("search_tab.in_double_quotes_tip"));
    ui.end_row();
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

    /// A temp folder, Keel's walk index over it as the System backend, and a search tab.
    struct Fixture {
        base: std::path::PathBuf,
        root: std::path::PathBuf,
        state: AppState,
    }

    impl Fixture {
        fn new(tag: &str, files: &[&str]) -> Self {
            let base =
                std::env::temp_dir().join(format!("keel-search-tab-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let root = base.join("tree");
            for f in files {
                let path = root.join(f);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, b"12345").unwrap();
            }
            let index =
                keel_search::WalkIndexSearcher::open(base.join("index"), vec![root.clone()])
                    .unwrap();
            assert!(index.wait_ready(Duration::from_secs(60)));
            let mut state = AppState::new(
                egui::Context::default(),
                Arc::new(Router::new()),
                VPath::local(&root),
            );
            let searcher: Arc<dyn Searcher> = Arc::new(index);
            state.apply(Msg::Searcher {
                searcher,
                reason: None,
            });
            state.run(0, Action::Search);
            assert!(state.tab(0).is_search());
            Self { base, root, state }
        }

        /// Runs `text` with `toggles` through the search worker; the result names, sorted.
        fn search(&mut self, text: &str, toggles: Toggles) -> Vec<String> {
            if let TabKind::Search {
                query,
                due,
                toggles: t,
                limit,
                ..
            } = &mut self.state.tab_mut(0).kind
            {
                *query = text.into();
                *t = toggles;
                *limit = 0;
                *due = Some(Instant::now());
            }
            self.finish()
        }

        /// Runs the due query and applies its answer.
        fn finish(&mut self) -> Vec<String> {
            self.state.tick();
            let msg = loop {
                match self.state.rx.recv_timeout(Duration::from_secs(30)).unwrap() {
                    m @ Msg::Search { .. } => break m,
                    other => self.state.apply(other),
                }
            };
            self.state.apply(msg);
            let tab = self.state.tab(0);
            assert_eq!(tab.error, None);
            let mut names: Vec<String> = (tab.entries().iter())
                .map(|e| tab.shown_name(e).to_owned())
                .collect();
            names.sort();
            names
        }

        fn toggles(&self) -> Toggles {
            match &self.state.tab(0).kind {
                TabKind::Search { toggles, .. } => *toggles,
                _ => panic!("not a search tab"),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    const FILES: &[&str] = &[
        "docs/Report.pdf",
        "docs/report notes.txt",
        "docs/oldies/report.pdf",
        "café menu.txt",
        "reportage.md",
    ];

    /// Task 76: each toggle changes what the worker finds (Keel's walk index on a temp
    /// folder), and the toggle actions flip the tab's toggles and search again.
    #[test]
    fn each_toggle_through_the_worker() {
        let mut f = Fixture::new("toggles", FILES);
        let off = Toggles::default();
        assert_eq!(
            f.search("report", off),
            [
                "Report.pdf",
                "report notes.txt",
                "report.pdf",
                "reportage.md"
            ]
        );
        // Match Case, from its action (Ctrl+I).
        f.search("Report", off);
        f.state.run(0, Action::SearchToggle(Toggle::Case));
        assert!(f.toggles().match_case);
        assert_eq!(f.finish(), ["Report.pdf"]);
        f.state.run(0, Action::SearchToggle(Toggle::Case));
        assert!(!f.toggles().match_case);
        assert_eq!(f.finish().len(), 4);
        // Match Whole Words.
        let words = Toggles {
            whole_word: true,
            ..off
        };
        assert_eq!(
            f.search("report", words),
            ["Report.pdf", "report notes.txt", "report.pdf"]
        );
        // Match Path.
        assert_eq!(f.search("oldies", off), ["oldies"]);
        let path = Toggles {
            match_path: true,
            ..off
        };
        assert_eq!(f.search("oldies", path), ["oldies", "report.pdf"]);
        // Match Diacritics.
        assert_eq!(f.search("cafe", off), ["café menu.txt"]);
        let marks = Toggles {
            match_diacritics: true,
            ..off
        };
        assert!(f.search("cafe", marks).is_empty());
        assert_eq!(f.search("café", marks), ["café menu.txt"]);
        // Regex.
        assert!(f.search(r"^report\.pdf$", off).is_empty(), "literal");
        let regex = Toggles { regex: true, ..off };
        assert_eq!(
            f.search(r"^report\.pdf$", regex),
            ["Report.pdf", "report.pdf"]
        );
        // Match whole filename when using wildcards (a setting).
        assert!(f.search("*.pd", off).is_empty());
        f.state.settings.search.whole_filename_wildcards = false;
        assert_eq!(f.search("*.pd", off), ["Report.pdf", "report.pdf"]);
        // New search tabs start with the profile's toggles.
        f.state.settings.search.toggles.whole_word = true;
        f.state.run(0, Action::NewTab);
        f.state.run(0, Action::Search);
        assert!(f.toggles().whole_word);
    }

    /// Task 76: the matched parts of names are highlighted; with Match Path, of folders.
    #[test]
    fn matched_parts_are_highlighted() {
        let mut f = Fixture::new("marks", FILES);
        f.search("rep", Toggles::default());
        let h = f.state.tab(0).highlight.clone().expect("highlighter");
        assert_eq!(h.marks("My Report.pdf"), [(3, 6)]);
        assert!(!h.path);
        let path = Toggles {
            match_path: true,
            ..Toggles::default()
        };
        f.search("oldies", path);
        let h = f.state.tab(0).highlight.clone().expect("highlighter");
        assert!(h.path);
        let folder = f.root.join("docs").join("oldies").display().to_string();
        assert!(!h.marks(&folder).is_empty());
        // An empty query clears the results and the highlight (no query runs).
        if let TabKind::Search { query, due, .. } = &mut f.state.tab_mut(0).kind {
            query.clear();
            *due = Some(Instant::now());
        }
        f.state.tick();
        assert!(f.state.tab(0).highlight.is_none());
        assert!(f.state.tab(0).entries().is_empty());
    }

    /// Task 76: no 500 cap. The first query shows the first page; Load more asks for more,
    /// up to the Most results setting; the status bar's numbers follow.
    #[test]
    fn load_more_goes_past_the_first_page() {
        let names: Vec<String> = (0..PAGE + 50).map(|i| format!("item{i:05}.txt")).collect();
        let files: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut f = Fixture::new("more", &files);
        assert_eq!(f.search("item", Toggles::default()).len(), PAGE as usize);
        let TabKind::Search { limit, more, .. } = f.state.tab(0).kind.clone() else {
            unreachable!()
        };
        assert_eq!((limit, more), (PAGE, true));
        f.state.run(0, Action::SearchMore);
        assert_eq!(f.finish().len(), PAGE as usize + 50);
        let TabKind::Search { limit, .. } = f.state.tab(0).kind.clone() else {
            unreachable!()
        };
        assert_eq!(limit, 4 * PAGE);
        // Sizes and dates came along (the walk index has them).
        let tab = f.state.tab(0);
        assert!(tab
            .entries()
            .iter()
            .all(|e| e.size == 5 && e.modified.is_some()));
        // Editing the query starts from the first page again.
        f.state.run(0, Action::Type("0".into()));
        assert!(matches!(
            f.state.tab(0).kind,
            TabKind::Search { limit: 0, .. }
        ));
    }

    #[test]
    fn limits_and_settings() {
        let s = SearchSettings::default();
        assert_eq!(s.limit(0), PAGE);
        assert_eq!(s.more(PAGE), 4 * PAGE);
        assert_eq!(s.more(4 * PAGE), 10_000);
        assert_eq!(s.more(10_000), 10_000);
        let few = SearchSettings {
            max_results: 5,
            ..s.clone()
        };
        assert_eq!(few.max(), *MAX_RESULTS.start(), "clamped");
        assert_eq!(s.delay(), Some(DEBOUNCE));
        let enter = SearchSettings {
            as_you_type: false,
            ..s.clone()
        };
        assert_eq!(enter.delay(), None);
        let q = s.query(
            "x".into(),
            Toggles {
                match_case: true,
                whole_word: true,
                match_path: true,
                match_diacritics: true,
                regex: true,
            },
            7,
        );
        assert!(q.match_case && q.whole_word && q.match_path && q.match_diacritics && q.regex);
        assert!(q.whole_filename_wildcards);
        assert_eq!(q.max, 7);
        // Old settings files without [search] load with the defaults.
        let old: crate::settings::Settings = toml::from_str("theme = 'light'").unwrap();
        assert_eq!(old.search, SearchSettings::default());
        assert!(!old.quote_copied_paths);
    }

    /// The hint names only what the backend understands (Keel's index has no `ext:`).
    #[test]
    fn hint_matches_the_backend() {
        assert!(hint(Some("Everything"), false).contains("ext:"));
        let native = hint(Some("Keel index"), false);
        assert!(!native.contains("ext:") && !native.contains("dm:"));
        assert!(native.contains("in:") && native.contains("regex:"));
        assert!(hint(Some("Library"), true).contains("tag:"));
        assert_eq!(hint(None, false), "Search file names");
    }

    /// Everything's toggle shortcuts reach a search tab, also from its query box.
    #[test]
    fn toggle_shortcuts() {
        let ctx = egui::Context::default();
        let keys = crate::keymap::Bindings::default();
        for t in Toggle::ALL {
            let chord = crate::keymap::Chord::parse(t.shortcut()).unwrap();
            let mut out = Vec::new();
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key: chord.key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers::COMMAND,
                    }],
                    modifiers: Modifiers::COMMAND,
                    ..Default::default()
                },
                |ctx| out = crate::keys::search_toggles(ctx, &keys),
            );
            assert_eq!(out, [Action::SearchToggle(t)]);
        }
    }

    /// Copy name copies file names; Copy path can quote each path.
    #[test]
    fn copy_name_and_quoted_paths() {
        let mut f = Fixture::new("copy", FILES);
        f.search("report.pdf", Toggles::default());
        let first = f.state.tab(0).entries()[0].clone();
        f.state.tab_mut(0).click(&first.name, false, false);
        let copied = |state: &AppState| -> Vec<String> {
            state
                .ctx
                .output(|o| o.commands.clone())
                .into_iter()
                .filter_map(|c| match c {
                    egui::OutputCommand::CopyText(t) => Some(t),
                    _ => None,
                })
                .collect()
        };
        f.state.run(0, Action::CopyName);
        assert_eq!(copied(&f.state).last().unwrap(), first.path.name());
        f.state.settings.quote_copied_paths = true;
        f.state.run(0, Action::CopyPath);
        assert_eq!(
            copied(&f.state).last().unwrap(),
            &format!("\"{}\"", first.path.display())
        );
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

    /// A pane's search: Esc, Backspace to empty and the × go back to its folder tab, or show
    /// the folder in the search tab when that tab is gone or moved; Ctrl+F, the toolbar's
    /// search and a bookmark open their own tab; it is not bookmarked.
    #[test]
    fn a_pane_search_goes_back_to_its_folder_and_is_not_reused() {
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            dir.clone(),
        );
        let tabs = |s: &AppState| s.panes[0].tabs.len();
        state.run(0, Action::SearchIn("ab".into()));
        assert_eq!((tabs(&state), state.panes[0].active), (2, 1));
        assert_eq!(state.tab(0).search_within(), Some(&dir));
        // Ctrl+F, the toolbar and a bookmark each open a search of their own.
        state.run(0, Action::Search);
        assert_eq!(tabs(&state), 3);
        assert!(state.tab(0).is_open_search());
        state.panes[0].active = 1;
        state.run(0, Action::ToolbarSearch("x".into()));
        assert_eq!(tabs(&state), 4);
        assert_eq!(state.tab(0).search_within(), None);
        state.panes[0].active = 1;
        let view = keel_core::View {
            query: "y".into(),
            backend: keel_core::ViewBackend::System,
            ..keel_core::View::default()
        };
        state.open_bookmark(0, &view);
        assert_eq!(tabs(&state), 5);
        let scoped = &state.panes[0].tabs[1];
        assert_eq!(scoped.search_within(), Some(&dir), "kept its folder");
        // Not bookmarked.
        state.panes[0].active = 1;
        state.find_cmd(0, FindCmd::AddBookmark);
        assert!(state.find.editor.is_none());
        // Backspace to empty: back to the folder tab before it.
        state.run(0, Action::Backspace);
        assert!(state.tab(0).search_within().is_some(), "\"a\" is left");
        state.run(0, Action::Backspace);
        assert_eq!((tabs(&state), state.panes[0].active), (4, 0));
        assert!(!state.tab(0).is_search());
        // Its folder tab moved away: the search tab shows the folder.
        state.run(0, Action::SearchIn("c".into()));
        state.panes[0].move_tab(0, 4);
        assert_eq!(state.panes[0].active, 0);
        state.run(0, Action::CloseSearch);
        assert_eq!(tabs(&state), 5);
        assert!(!state.tab(0).is_search());
        assert_eq!(state.tab(0).dir, dir);
    }
}
