//! Task 76: the rest of the search window. The Filters sidebar and the Folders sidebar
//! (Everything 1.5) at the left of a search tab's results; **Search current results**,
//! which turns the results shown into the list the next searches look in; Ctrl+Space in
//! the query box, which completes the word before the cursor from the search history and
//! the names in the results; and `no-highlight:`, whose terms are left out of the result
//! highlighting.

use crate::search_columns::Refine;
use crate::state::AppState;
use crate::tab::{Tab, TabKind};
use keel_search::filelist::{filetime_from_system, FileList};
use keel_search::FileListSearcher;
use keel_vfs::{Entry, Kind, VPath};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Names of the results Ctrl+Space looks through at most.
pub const COMPLETE_NAMES: usize = 20_000;
/// Results the Folders sidebar builds its tree from at most.
pub const TREE_RESULTS: usize = 200_000;

/// A command of the search window (`Action::Results`).
#[derive(Clone, Debug, PartialEq)]
pub enum ResultsCmd {
    ToggleFiltersSidebar,
    ToggleFoldersSidebar,
    /// Search current results on (the results shown become the list searched) or off
    /// (back to the search before).
    SearchCurrentResults,
}

impl AppState {
    pub fn results_cmd(&mut self, p: usize, cmd: ResultsCmd) {
        let opts = &mut self.settings.views.search;
        match cmd {
            ResultsCmd::ToggleFiltersSidebar => opts.filters_sidebar = !opts.filters_sidebar,
            ResultsCmd::ToggleFoldersSidebar => opts.folders_sidebar = !opts.folders_sidebar,
            ResultsCmd::SearchCurrentResults => {
                if !self.tab(p).is_search() {
                    return;
                }
                if !toggle_refine(self.tab_mut(p)) {
                    self.toasts
                        .info(crate::i18n::tr("search_results.nothing_to_refine"));
                }
            }
        }
        self.ctx.request_repaint();
    }
}

/// The results of `tab` as a file list (local results only: the list keeps paths).
pub fn snapshot(tab: &Tab) -> FileList {
    let entries = (tab.visible_cached().iter())
        .filter_map(|&i| tab.entries().get(i))
        .filter_map(|e| {
            let path = e.path.to_local_path()?;
            let created = tab.results.meta.by_name.get(&e.name).and_then(|m| m.created);
            Some(keel_search::filelist::Entry {
                path: path.to_string_lossy().into_owned(),
                is_dir: e.kind == Kind::Dir,
                size: (e.kind != Kind::Dir).then_some(e.size),
                modified: e.modified.and_then(filetime_from_system),
                created: created.and_then(filetime_from_system),
                attributes: None,
                extra: Vec::new(),
            })
        })
        .collect();
    FileList {
        columns: Vec::new(),
        entries,
    }
}

/// Search current results on or off for a search tab; false when there is nothing to
/// search in (no local results).
pub fn toggle_refine(tab: &mut Tab) -> bool {
    let TabKind::Search {
        query, limit, due, ..
    } = &mut tab.kind
    else {
        return false;
    };
    if let Some(r) = tab.results.refine.take() {
        tab.filelist = r.base;
        *query = r.query;
        *limit = 0;
        *due = Some(Instant::now());
        return true;
    }
    let list = snapshot(tab);
    if list.entries.is_empty() {
        return false;
    }
    let TabKind::Search {
        query, limit, due, ..
    } = &mut tab.kind
    else {
        return false;
    };
    let searcher = FileListSearcher::new(crate::i18n::tr("search_results.current_results"), list)
        .with_cap(crate::filelist_ui::LIST_HITS);
    tab.results.refine = Some(Refine {
        base: tab.filelist.replace(Arc::new(searcher)),
        query: std::mem::take(query),
    });
    *limit = 0;
    *due = Some(Instant::now());
    true
}

/// Leaves Search current results without running anything (a bookmark or a new search
/// takes the tab over).
pub fn end_refine(tab: &mut Tab) {
    if let Some(r) = tab.results.refine.take() {
        tab.filelist = r.base;
    }
}

/// The Search menu's entries of this module.
pub fn menu_items(
    ui: &mut egui::Ui,
    refining: bool,
    opts: &crate::search_columns::ResultsOptions,
    out: &mut Vec<crate::keys::Action>,
) {
    use crate::keys::Action;
    let mut item = |ui: &mut egui::Ui, on: bool, key: &str, cmd: ResultsCmd| {
        let label = crate::i18n::tr(key);
        let button = egui::Button::new(label).selected(on).shortcut_text(
            crate::keymap::shortcut(ui.ctx(), &Action::Results(cmd.clone()), ""),
        );
        if ui.add(button).clicked() {
            out.push(Action::Results(cmd));
            ui.close_menu();
        }
    };
    item(
        ui,
        refining,
        "search_results.search_current_results",
        ResultsCmd::SearchCurrentResults,
    );
    item(
        ui,
        opts.filters_sidebar,
        "search_results.filters_sidebar",
        ResultsCmd::ToggleFiltersSidebar,
    );
    item(
        ui,
        opts.folders_sidebar,
        "search_results.folders_sidebar",
        ResultsCmd::ToggleFoldersSidebar,
    );
}

// ---------------------------------------------------------------------------------------
// The Folders sidebar's tree
// ---------------------------------------------------------------------------------------

/// The folders the results are in, with how many results each holds (all below it).
#[derive(Default, Debug)]
pub struct FolderTree {
    pub nodes: Vec<Node>,
    /// Folders without a parent in the tree, by name.
    pub roots: Vec<usize>,
}

#[derive(Debug)]
pub struct Node {
    pub path: VPath,
    /// Results at or below this folder.
    pub hits: usize,
    /// Results directly in it.
    pub own: usize,
    /// By name.
    pub children: Vec<usize>,
}

impl FolderTree {
    /// The tree of the folders of `entries` (their parents and every folder above).
    pub fn build<'a>(entries: impl Iterator<Item = &'a Entry>) -> FolderTree {
        let mut at: HashMap<VPath, usize> = HashMap::new();
        let mut nodes: Vec<Node> = Vec::new();
        for e in entries.take(TREE_RESULTS) {
            let mut dir = e.path.parent();
            let mut first = true;
            while let Some(d) = dir {
                let i = *at.entry(d.clone()).or_insert_with(|| {
                    nodes.push(Node {
                        path: d.clone(),
                        hits: 0,
                        own: 0,
                        children: Vec::new(),
                    });
                    nodes.len() - 1
                });
                nodes[i].hits += 1;
                if first {
                    nodes[i].own += 1;
                    first = false;
                }
                dir = d.parent();
            }
        }
        let mut roots = Vec::new();
        for i in 0..nodes.len() {
            match nodes[i].path.parent().and_then(|p| at.get(&p).copied()) {
                Some(parent) => nodes[parent].children.push(i),
                None => roots.push(i),
            }
        }
        let key = |nodes: &[Node], i: usize| nodes[i].path.name().to_lowercase();
        let mut sorted = |list: &mut Vec<usize>| {
            list.sort_by(|&a, &b| natord::compare(&key(&nodes, a), &key(&nodes, b)))
        };
        sorted(&mut roots);
        let mut kids: Vec<Vec<usize>> = nodes.iter().map(|n| n.children.clone()).collect();
        for k in &mut kids {
            sorted(k);
        }
        for (n, k) in nodes.iter_mut().zip(kids) {
            n.children = k;
        }
        FolderTree { nodes, roots }
    }

    /// From `i`, the deepest folder of its chain of folders with one subfolder and no
    /// results of their own (shown as one row: `D:\Projects\site\src`).
    pub fn chain_end(&self, mut i: usize) -> usize {
        while let [only] = self.nodes[i].children[..] {
            if self.nodes[i].own > 0 {
                break;
            }
            i = only;
        }
        i
    }
}

/// `tab`'s Folders sidebar tree for its current results (built again when they change).
fn tree_of(tab: &mut Tab) -> Arc<FolderTree> {
    let key = tab.generation();
    if let Some((k, t)) = &tab.results.tree {
        if *k == key {
            return t.clone();
        }
    }
    let tree = Arc::new(FolderTree::build(tab.entries().iter()));
    tab.results.tree = Some((key, tree.clone()));
    tree
}

/// The label of a folder row: its name, or its whole path for a chain or a root.
fn folder_label(path: &VPath, whole: bool) -> String {
    match (whole, path.name()) {
        (true, _) | (_, "") => path.display(),
        (false, name) => name.to_owned(),
    }
}

fn folder_rows(
    ui: &mut egui::Ui,
    tree: &FolderTree,
    i: usize,
    whole: bool,
    tab: &mut Tab,
    depth: usize,
) {
    let end = tree.chain_end(i);
    let node = &tree.nodes[end];
    let label = folder_label(&node.path, whole || end != i);
    let open = (depth == 0) != tab.results.toggled.contains(&node.path);
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 12.0);
        if node.children.is_empty() {
            ui.add_space(16.0);
        } else {
            let arrow = egui::Button::new(if open { "⏷" } else { "⏵" }).frame(false);
            let name = match open {
                true => crate::i18n::tr("search_results.collapse_folder"),
                false => crate::i18n::tr("search_results.expand_folder"),
            };
            if crate::a11y::Named::named(ui.add(arrow), &format!("{name} {label}")).clicked() {
                if !tab.results.toggled.remove(&node.path) {
                    tab.results.toggled.insert(node.path.clone());
                }
            }
        }
        let selected = tab.results.folder.as_ref() == Some(&node.path);
        let text = format!("{label}  ({})", node.hits);
        if ui
            .selectable_label(selected, text)
            .on_hover_text(node.path.display())
            .clicked()
        {
            tab.results.folder = (!selected).then(|| node.path.clone());
        }
    });
    if open {
        for &c in &node.children {
            folder_rows(ui, tree, c, false, tab, depth + 1);
        }
    }
}

/// The Filters and Folders sidebars of a search tab, when on (`[views.search]`), at the
/// left of `ui`.
pub fn sidebars(
    ui: &mut egui::Ui,
    pane: usize,
    tab: &mut Tab,
    opts: &crate::search_columns::ResultsOptions,
    out: &mut Vec<crate::keys::Action>,
) {
    // A folder picked in the sidebar narrows the results only while the sidebar shows.
    if !opts.folders_sidebar {
        tab.results.folder = None;
    }
    if !opts.filters_sidebar && !opts.folders_sidebar {
        return;
    }
    egui::SidePanel::left(egui::Id::new(("keel-search-sidebar", pane)))
        .resizable(true)
        .default_width(200.0)
        .width_range(120.0..=480.0)
        .show_inside(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if opts.filters_sidebar {
                        filters_section(ui, tab, out);
                    }
                    if opts.folders_sidebar {
                        if opts.filters_sidebar {
                            ui.separator();
                        }
                        folders_section(ui, tab);
                    }
                });
        });
}

fn filters_section(ui: &mut egui::Ui, tab: &mut Tab, out: &mut Vec<crate::keys::Action>) {
    ui.label(egui::RichText::new(crate::i18n::tr("search_results.filters")).strong());
    let shared = crate::bookmarks::published(ui.ctx());
    let names = match shared.filters.is_empty() {
        true => crate::search_filters::names(&[]),
        false => shared.filters.clone(),
    };
    for name in &names {
        let value = match name.as_str() {
            "Everything" => String::new(),
            n => n.to_owned(),
        };
        let on = tab.search_filter == value;
        if ui
            .selectable_label(on, crate::search_filters::shown(name))
            .clicked()
            && !on
        {
            tab.search_filter = value;
            if let TabKind::Search { limit, due, .. } = &mut tab.kind {
                *limit = 0;
                *due = Some(Instant::now());
            }
            ui.ctx().request_repaint();
        }
    }
    if ui
        .small_button(crate::i18n::tr("bookmarks.edit_filters"))
        .clicked()
    {
        out.push(crate::keys::Action::Find(
            crate::bookmarks::FindCmd::EditFilters,
        ));
    }
}

fn folders_section(ui: &mut egui::Ui, tab: &mut Tab) {
    ui.label(egui::RichText::new(crate::i18n::tr("search_results.folders")).strong());
    let tree = tree_of(tab);
    let all = tab.results.folder.is_none();
    let text =
        crate::i18n::tr("search_results.all_results").replace("{n}", &tab.entries().len().to_string());
    if ui.selectable_label(all, text).clicked() {
        tab.results.folder = None;
    }
    if tab.entries().len() > TREE_RESULTS {
        ui.weak(
            crate::i18n::tr("search_results.tree_first_n").replace("{n}", &TREE_RESULTS.to_string()),
        );
    }
    for &r in &tree.roots {
        folder_rows(ui, &tree, r, true, tab, 0);
    }
}

// ---------------------------------------------------------------------------------------
// Ctrl+Space: complete the word
// ---------------------------------------------------------------------------------------

/// Characters that end the word Ctrl+Space completes (besides white-space): a quote,
/// a function's colon, `<`, `>`, `|` and `!`.
fn breaks_word(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | ':' | '<' | '>' | '|' | '!')
}

/// The words Ctrl+Space offers: the history's (functions left out), then the result
/// names, each once (any case), in that order.
pub fn candidates<'a>(
    history: &'a [String],
    names: impl Iterator<Item = &'a str>,
) -> Vec<&'a str> {
    let mut seen = std::collections::HashSet::new();
    let words = (history.iter())
        .flat_map(|h| h.split_whitespace())
        .map(|w| w.trim_matches('"'))
        .filter(|w| !w.is_empty() && !w.contains(':'));
    words
        .chain(names.take(COMPLETE_NAMES))
        .filter(|w| seen.insert(w.to_lowercase()))
        .collect()
}

/// `text` with the word before `cursor` (in characters) completed by the `skip`-th of
/// `words` that starts with it (any case) and is longer; the new text and cursor. A
/// completion with white-space is put in quotes.
pub fn complete_word(
    text: &str,
    cursor: usize,
    words: &[&str],
    skip: usize,
) -> Option<(String, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let start = (chars[..cursor].iter())
        .rposition(|&c| breaks_word(c))
        .map_or(0, |i| i + 1);
    let word: String = chars[start..cursor].iter().collect();
    if word.is_empty() {
        return None;
    }
    let lower = word.to_lowercase();
    let quoted = start > 0 && chars[start - 1] == '"';
    let found = (words.iter())
        .filter(|w| w.chars().count() > word.chars().count() && w.to_lowercase().starts_with(&lower))
        .nth(skip)?;
    let insert = match found.contains(char::is_whitespace) && !quoted {
        true => format!("\"{found}\""),
        false => (*found).to_owned(),
    };
    let head: String = chars[..start].iter().collect();
    let tail: String = chars[cursor..].iter().collect();
    let at = start + insert.chars().count();
    Some((format!("{head}{insert}{tail}"), at))
}

/// Where Ctrl+Space left the box, to offer the next word on the next press.
#[derive(Clone, Default)]
struct Completing {
    /// The text and cursor before the first press.
    original: String,
    cursor: usize,
    /// The text after the last press, and which word it took.
    shown: String,
    skip: usize,
}

/// The cursor of the text box `id` (in characters), else its end.
fn cursor_of(ctx: &egui::Context, id: egui::Id, text: &str) -> usize {
    (egui::TextEdit::load_state(ctx, id))
        .and_then(|s| s.cursor.char_range())
        .map_or(text.chars().count(), |r| r.primary.index)
}

/// Ctrl+Space in the focused query box `id`: completes the word before the cursor from
/// `words`; pressed again, offers the next word (round). True when `query` changed.
pub fn complete_key(ui: &egui::Ui, id: egui::Id, query: &mut String, words: &[&str]) -> bool {
    let sid = id.with("complete-word");
    let mut st: Completing = ui.data(|d| d.get_temp(sid)).unwrap_or_default();
    let again = st.shown == *query && !query.is_empty();
    let (text, cursor, skip) = match again {
        true => (st.original.clone(), st.cursor, st.skip + 1),
        false => (query.clone(), cursor_of(ui.ctx(), id, query), 0),
    };
    let done = complete_word(&text, cursor, words, skip)
        .map(|r| (r, skip))
        .or_else(|| match again {
            // Past the last word: the first again.
            true => complete_word(&text, cursor, words, 0).map(|r| (r, 0)),
            false => None,
        });
    let Some(((new, at), skip)) = done else {
        return false;
    };
    st = Completing {
        original: text,
        cursor,
        shown: new.clone(),
        skip,
    };
    ui.data_mut(|d| d.insert_temp(sid, st));
    *query = new;
    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
        let c = egui::text::CCursor::new(at);
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(c)));
        state.store(ui.ctx(), id);
    }
    true
}

/// Whether Ctrl+Space was pressed this frame (consumed: no other handler sees it).
pub fn complete_pressed(ui: &egui::Ui) -> bool {
    ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Space))
}

/// The names of `tab`'s results Ctrl+Space offers (after the history's words).
pub fn result_names(tab: &Tab) -> Vec<String> {
    (tab.entries().iter())
        .take(COMPLETE_NAMES)
        .map(|e| e.path.name().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------
// no-highlight:
// ---------------------------------------------------------------------------------------

/// `text` with every term that has `no-highlight:` blanked out, for the highlighter
/// (unchanged when it does not parse, or for a regular expression).
pub fn without_no_highlight(text: &str, regex: bool) -> String {
    if regex || !text.to_ascii_lowercase().contains("no-highlight:") {
        return text.to_owned();
    }
    let Ok(ast) = keel_query::parse_ast(text, &keel_query::Options::default()) else {
        return text.to_owned();
    };
    fn collect(e: &keel_query::Expr, out: &mut Vec<std::ops::Range<usize>>) {
        match e {
            keel_query::Expr::And(xs) | keel_query::Expr::Or(xs) => {
                xs.iter().for_each(|x| collect(x, out))
            }
            keel_query::Expr::Not(x) => collect(x, out),
            keel_query::Expr::Term(t) => {
                if t.modifiers.contains(&keel_query::Modifier::NoHighlight) {
                    out.push(t.span.clone());
                }
            }
        }
    }
    let mut spans = Vec::new();
    collect(&ast.expr, &mut spans);
    let mut out = text.to_owned();
    for r in spans {
        let fits = r.start <= r.end
            && r.end <= out.len()
            && out.is_char_boundary(r.start)
            && out.is_char_boundary(r.end);
        if fits {
            // Same length in bytes: the other spans stay where they are.
            let blank = " ".repeat(r.len());
            out.replace_range(r, &blank);
        }
    }
    out
}

#[cfg(test)]
#[path = "search_view_tests.rs"]
mod tests;
