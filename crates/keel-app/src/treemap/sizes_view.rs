//! The Sizes view's charts: a **Table / Treemap / Sunburst / Flame graph** switch above the
//! view, and the chart of the shown folder built from the totals the view already has: its
//! files and folders, and below them every folder total known (walked by Calculate sizes,
//! or indexed in the library, which is asked for a few levels on demand). What a folder
//! holds besides its known subfolders shows as "Files in …".

use super::panel::options_ui;
use super::tree::{NodeId, Tree};
use super::widget::{Chart, TreemapView};
use super::Options;
use crate::library::LibraryUi;
use crate::sizes::Sizes;
use crate::tab::Tab;
use keel_core::Total;
use keel_vfs::{Kind, VPath};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the Sizes view shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Show {
    #[default]
    Table,
    Chart(Chart),
}

/// A folder (known total, contents shown only as far as totals below it are known).
pub const FOLDER: &str = "/folder";
/// The files of a folder besides its subfolders.
pub const FILES: &str = "/files";
/// While totals keep arriving (a walk), the chart is rebuilt at most this often.
const REBUILD_EVERY: Duration = Duration::from_millis(700);
const MAX_DEPTH: u16 = 64;
/// Library folders this many levels down whose children are not known yet are asked for.
const ASK_DEPTH: u16 = 3;

#[derive(Default)]
struct State {
    view: TreemapView,
    /// The folder and the generations the tree was built from.
    built: Option<(VPath, [u64; 4])>,
    built_at: Option<Instant>,
    /// The tab's cursor last synced into the chart.
    cursor: Option<String>,
}

/// A node id for a path.
pub fn id_of(p: &VPath) -> NodeId {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// Whether `p` is below `root` (not `root` itself).
fn below(p: &VPath, root: &VPath) -> bool {
    let r = root.path.trim_end_matches('/');
    p.scheme == root.scheme
        && p.authority == root.authority
        && p.path.len() > r.len() + 1
        && p.path.starts_with(r)
        && p.path.as_bytes()[r.len()] == b'/'
}

/// The known totals below `root`, by parent folder, each list by name.
fn children_map<'a>(
    totals: impl Iterator<Item = (&'a VPath, Total)>,
    root: &VPath,
) -> HashMap<VPath, Vec<(VPath, Total)>> {
    let mut map: HashMap<VPath, Vec<(VPath, Total)>> = HashMap::new();
    for (p, t) in totals.filter(|(p, _)| below(p, root)) {
        if let Some(parent) = p.parent() {
            map.entry(parent).or_default().push((p.clone(), t));
        }
    }
    for v in map.values_mut() {
        v.sort_by(|a, b| a.0.name().cmp(b.0.name()));
    }
    map
}

/// Adds the known subfolders of `at` (and theirs) under `node`, then the rest of its
/// `bytes` as "Files in …". `ask` gets the folders whose children are not known.
fn descend(
    tree: &mut Tree,
    map: &HashMap<VPath, Vec<(VPath, Total)>>,
    node: u32,
    at: &VPath,
    bytes: u64,
    depth: u16,
    ask: &mut dyn FnMut(&VPath, u16, u64),
) {
    let Some(kids) = map.get(at) else {
        ask(at, depth, bytes);
        return;
    };
    let mut sum = 0u64;
    for (p, t) in kids.iter().filter(|(_, t)| t.bytes > 0) {
        let c = tree.add(node, id_of(p), t.bytes, FOLDER, p.name());
        sum = sum.saturating_add(t.bytes);
        if depth < MAX_DEPTH {
            descend(tree, map, c, p, t.bytes, depth + 1, ask);
        }
    }
    if bytes > sum {
        let label = format!("Files in {}", at.name());
        // Same id as the folder: picking it picks the folder.
        tree.add(node, id_of(at), bytes - sum, FILES, &label);
    }
}

fn label_of(dir: &VPath) -> String {
    match dir.name() {
        "" => dir.display(),
        n => n.to_owned(),
    }
}

/// The chart's tree of the tab's shown entries, with every total known below them.
pub fn build(tab: &Tab, lib: &LibraryUi, sizes: &Sizes) -> Tree {
    let dir = &tab.dir;
    let mut tree = Tree::new(id_of(dir), &label_of(dir));
    let walked = children_map(sizes.walked(), dir);
    let real_dir = lib
        .is_open()
        .then(|| crate::library::real_of(&lib.sources, dir))
        .flatten();
    let indexed = match &real_dir {
        Some(r) => children_map(lib.sizes.iter().map(|(p, t)| (p, *t)), r),
        None => HashMap::new(),
    };
    let entries: Vec<_> = (tab.visible_cached().iter())
        .filter_map(|&i| tab.entries().get(i))
        .filter(|e| !(e.kind == Kind::Dir && e.is_link))
        .map(|e| (e, crate::sizes::bytes_of(lib, sizes, e)))
        .collect();
    let total: u64 = entries.iter().filter_map(|(_, b)| *b).sum();
    for (e, bytes) in entries {
        let Some(bytes) = bytes.filter(|b| *b > 0) else {
            continue;
        };
        if e.kind != Kind::Dir {
            tree.add(0, id_of(&e.path), bytes, &e.ext, &e.name);
            continue;
        }
        let node = tree.add(0, id_of(&e.path), bytes, FOLDER, &e.name);
        let real = real_dir.as_ref().map(|r| r.join(&e.name));
        match real.filter(|r| lib.sizes.contains_key(r)) {
            Some(real) => {
                let mut ask = |p: &VPath, depth: u16, b: u64| {
                    if depth <= ASK_DEPTH && b.saturating_mul(100) >= total {
                        lib.want_children(p);
                    }
                };
                descend(&mut tree, &indexed, node, &real, bytes, 1, &mut ask);
            }
            None => descend(
                &mut tree,
                &walked,
                node,
                &e.path,
                bytes,
                1,
                &mut |_, _, _| {},
            ),
        }
    }
    tree.finish();
    tree
}

/// The Sizes view's switch, and with a chart chosen the chart below it (Task 83). True
/// when a chart took the place of the table.
pub fn ui(ui: &mut egui::Ui, pane: usize, tab: &mut Tab, lib: &LibraryUi, sizes: &Sizes) -> bool {
    let show_id = egui::Id::new(("keel-sizes-show", pane));
    let opts_id = egui::Id::new("keel-treemap-options");
    let saved: Show = ui
        .data_mut(|d| d.get_persisted(show_id))
        .unwrap_or_default();
    let mut show = saved;
    let mut opts: Options = ui
        .data_mut(|d| d.get_persisted(opts_id))
        .unwrap_or_default();
    let state: Arc<Mutex<State>> = ui.data_mut(|d| {
        d.get_temp_mut_or_insert_with(egui::Id::new(("keel-sizes-chart", pane)), || {
            Arc::new(Mutex::new(State::default()))
        })
        .clone()
    });
    let mut st = state.lock();
    let mut opts_changed = false;
    ui.horizontal(|ui| {
        if ui
            .selectable_label(show == Show::Table, "Table")
            .on_hover_text("The list, biggest first")
            .clicked()
        {
            show = Show::Table;
        }
        for (c, tip) in [
            (
                Chart::Treemap,
                "Each file a rectangle by its size, inside its folder's (double-click zooms in, Backspace out)",
            ),
            (Chart::Sunburst, "Folders as rings around this folder"),
            (Chart::Flame, "Folders as rows below this folder"),
        ] {
            if ui
                .selectable_label(show == Show::Chart(c), c.name())
                .on_hover_text(tip)
                .clicked()
            {
                show = Show::Chart(c);
            }
        }
        if show == Show::Table {
            return;
        }
        ui.separator();
        ui.menu_button("Options", |ui| opts_changed |= options_ui(ui, &mut opts));
        if st.view.root != 0 {
            if ui
                .button("⏶ Zoom out")
                .on_hover_text("Backspace or the mouse's back button")
                .clicked()
            {
                st.view.zoom_out();
            }
            let root = st.view.root;
            ui.weak(st.view.tree().path(root));
        }
    });
    if show != saved {
        ui.data_mut(|d| d.insert_persisted(show_id, show));
    }
    if opts_changed {
        ui.data_mut(|d| d.insert_persisted(opts_id, opts.clone()));
    }
    let Show::Chart(chart) = show else {
        return false;
    };
    rebuild(ui.ctx(), &mut st, tab, lib, sizes);
    // The table's cursor shows in the chart.
    if st.cursor != tab.cursor {
        st.cursor.clone_from(&tab.cursor);
        let entry = (tab.cursor.as_deref())
            .and_then(|c| tab.entries().iter().find(|e| e.name == c))
            .map(|e| id_of(&e.path));
        if let Some(id) = entry {
            let tree = st.view.tree();
            let inside = (st.view.selected)
                .zip(tree.find(id))
                .is_some_and(|(s, e)| tree.contains(e, s));
            if !inside {
                st.view.select(id);
            }
        }
    }
    let name = format!("{} of {}", chart.name(), tab.title());
    let r = st.view.ui(ui, chart, &opts, &name);
    // A pick selects its entry of this folder in the table.
    if r.picked.is_some() {
        let tree = st.view.tree();
        let top = (st.view.selected).and_then(|s| {
            if tree.node(s).parent == Some(0) {
                Some(s)
            } else {
                tree.child_toward(0, s)
            }
        });
        if let Some(top) = top {
            let name = tree.node(top).label.clone();
            if tab.entries().iter().any(|e| e.name == name) {
                tab.click(&name, false, false);
                tab.reveal = true;
                st.cursor = Some(name);
            }
        }
    }
    true
}

/// Builds the tree again when the folder, its listing or the totals changed (at most every
/// [`REBUILD_EVERY`] in the same folder).
fn rebuild(ctx: &egui::Context, st: &mut State, tab: &Tab, lib: &LibraryUi, sizes: &Sizes) {
    let key = [
        tab.generation(),
        tab.visible_gen(),
        lib.sizes_gen,
        sizes.gen,
    ];
    let same_dir = st.built.as_ref().is_some_and(|(d, _)| *d == tab.dir);
    if same_dir && st.built.as_ref().is_some_and(|(_, k)| *k == key) {
        return;
    }
    if same_dir && st.built_at.is_some_and(|t| t.elapsed() < REBUILD_EVERY) {
        ctx.request_repaint_after(REBUILD_EVERY);
        return;
    }
    let tree = build(tab, lib, sizes);
    st.view.set_tree(tree);
    if !same_dir {
        st.view.root = 0;
        st.view.selected = None;
        st.cursor = None;
    }
    st.built = Some((tab.dir.clone(), key));
    st.built_at = Some(Instant::now());
}
