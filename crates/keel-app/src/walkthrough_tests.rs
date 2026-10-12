//! The QA walkthrough: what a user does with the window, driven through egui_kittest on
//! the screenshot fixture (`screenshots::write_fixture`), each flow asserting what the
//! user sees; `docs/qa/2026-10-10-walkthrough.md` lists what the first run found. The fast
//! flows run with the normal tests; the slow ones (library, devices, media, PDF and video,
//! the Recycle Bin, window sizes) are `#[ignore]`d:
//!
//! ```sh
//! scripts/walkthrough.sh            # every flow, with a PNG per flow
//! ```
//!
//! With `KEEL_WALKTHROUGH_SHOTS=1` (the script sets it) each flow renders a 1280x800 PNG to
//! `target/walkthrough/<flow>.png` (needs a GPU); without it nothing is rendered. Settings,
//! data and the search index stay in temp folders; no flow touches the keychain, the home
//! folder or the system's sound. The palette and keyboard flows use the system clipboard
//! (holding `SYSTEM_CLIPBOARD`, like the clipboard tests); the Windows Recycle Bin flow
//! trashes, restores and purges one file of its own.

use crate::app::{App, Boot};
use crate::keys::Action;
use crate::pane::ViewMode;
use crate::screenshots::{listed, neutral_sidebar, previewed, wait, write_fixture};
use crate::session::Session;
use crate::state::AppState;
use crate::tab::SortKey;
use egui::{Event, Key, Modifiers, PointerButton, Pos2};
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use keel_vfs::VPath;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

type H = Harness<'static, App>;

const SIZE: egui::Vec2 = egui::vec2(1280.0, 800.0);

/// Rendering is opt-in: it needs a GPU.
fn shots_on() -> bool {
    std::env::var_os("KEEL_WALKTHROUGH_SHOTS").is_some_and(|v| !v.is_empty() && v != "0")
}

/// The shared read-only fixture (Documents, Pictures, Backup, Inbox), written once per
/// test process.
fn demo() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("keel-walk-{}", std::process::id()));
        write_fixture(&root);
        root
    })
}

/// A fresh writable folder for one flow, with a few files from the fixture's Documents.
fn scratch(flow: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("keel-walk-{}-{flow}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let docs = demo().join("Documents");
    for sub in ["a", "b"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for name in [
        "notes.md",
        "budget.csv",
        "todo.txt",
        "garden.rs",
        "site.zip",
    ] {
        std::fs::copy(docs.join(name), dir.join("a").join(name)).unwrap();
    }
    std::fs::write(dir.join("b").join("budget.csv"), "old budget\n").unwrap();
    dir
}

fn boot(left: &Path, right: &Path, settings: crate::settings::Settings) -> Boot {
    let (l, r) = (VPath::local(left), VPath::local(right));
    Boot {
        settings,
        session: Session {
            panes: vec![vec![l.clone()], vec![r]],
            ..Session::single(l.clone())
        },
        ..Boot::at(l)
    }
}

fn app_with(boot: Boot, size: egui::Vec2) -> H {
    build(boot, size, shots_on())
}

/// `wgpu`: a GPU device from the start (else one is made on the first `shot`).
fn build(boot: Boot, size: egui::Vec2, wgpu: bool) -> H {
    let mut b = Harness::builder().with_size(size);
    if wgpu {
        b = b.wgpu();
    }
    let mut h = b.build_eframe(move |cc| App::new(cc, boot));
    h.input_mut().max_texture_side = Some(8192);
    // The fixture's quick-access folders, never the real ones.
    neutral_sidebar(&mut h.state_mut().state, demo());
    assert!(wait(&mut h, 30, &listed), "both panes listed");
    h
}

/// The window in the Classic style: the position checks here (tab strip, path bar, rows)
/// are measured in it. `appearance_style_finder_and_classic` boots the Finder default.
fn app(left: &Path, right: &Path) -> H {
    let classic = crate::settings::Settings {
        style: crate::design::Style::Classic,
        ..Default::default()
    };
    app_with(boot(left, right, classic), SIZE)
}

/// Renders the window to `target/walkthrough/<name>.png` (with `KEEL_WALKTHROUGH_SHOTS`).
fn shot(h: &mut H, name: &str) {
    h.run_steps(3);
    if !shots_on() {
        return;
    }
    neutral_sidebar(&mut h.state_mut().state, demo());
    h.run_steps(3);
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/walkthrough");
    std::fs::create_dir_all(&dir).unwrap();
    let img = h.render().expect("render");
    img.save(dir.join(format!("{name}.png"))).unwrap();
}

fn s(h: &H) -> &AppState {
    &h.state().state
}

fn sm(h: &mut H) -> &mut AppState {
    &mut h.state_mut().state
}

fn key(h: &mut H, mods: Modifiers, k: Key) {
    h.press_key_modifiers(mods, k);
    h.run_steps(2);
}

fn type_text(h: &mut H, text: &str) {
    h.input_mut().events.push(Event::Text(text.into()));
    h.run_steps(2);
}

const CMD: Modifiers = Modifiers::COMMAND;
const ALT: Modifiers = Modifiers::ALT;
const NONE: Modifiers = Modifiers::NONE;
const CMD_SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::COMMAND
};

fn center_of(node: &egui_kittest::kittest::Node<'_>) -> Pos2 {
    let b = node.raw_bounds().expect("bounds");
    egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32)
}

/// The center of the `n`th widget labelled exactly `label` (left to right, top to bottom
/// as egui lists them).
fn pos(h: &H, label: &str, n: usize) -> Pos2 {
    let nodes: Vec<_> = h.query_all_by_label(label).collect();
    assert!(nodes.len() > n, "no {label:?} #{n} ({} found)", nodes.len());
    center_of(&nodes[n])
}

/// Nodes labelled `label` right of the sidebar, below `top`, left to right.
fn in_panes(h: &H, label: &str, top: f32) -> Vec<Pos2> {
    // Sidebar labels are centred well left of this; the panes start at its edge.
    let left = 150.0;
    let mut at: Vec<Pos2> = h
        .query_all_by_label(label)
        .map(|n| center_of(&n))
        .filter(|p| p.x > left && p.y > top)
        .collect();
    at.sort_by(|a, b| a.x.total_cmp(&b.x));
    at
}

/// A file row in pane `pane` (the leftmost or rightmost match below the headers; the
/// sidebar, tabs and path bar repeat names).
fn row(h: &H, name: &str, pane: usize) -> Pos2 {
    let mut rows = in_panes(h, name, 50.0);
    if rows.is_empty() {
        // A row painted without a label of its own (columns view): its AccessKit name.
        let prefix = format!("{name}, ");
        rows = (h.query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::ListItem)))
            .filter(|n| n.label().is_some_and(|l| l.starts_with(&prefix)))
            .map(|n| center_of(&n))
            .filter(|p| p.x > 150.0)
            .collect();
        rows.sort_by(|a, b| a.x.total_cmp(&b.x));
    }
    if rows.is_empty() {
        let all: Vec<_> = h
            .query_all_by_label_contains(name)
            .map(|n| (n.role(), n.label(), n.raw_bounds()))
            .collect();
        panic!("no row {name:?}: {all:?}");
    }
    if pane == 0 {
        rows[0]
    } else {
        *rows.last().unwrap()
    }
}

/// A tab title in the tab strip (pane 0: leftmost).
fn tab_pos(h: &H, title: &str) -> Pos2 {
    let tabs: Vec<Pos2> = in_panes(h, title, 0.0)
        .into_iter()
        .filter(|p| p.y < 24.0)
        .collect();
    *tabs.first().unwrap_or_else(|| panic!("no tab {title:?}"))
}

/// A part of the path bar (pane 0: leftmost).
fn crumb(h: &H, name: &str) -> Pos2 {
    let crumbs: Vec<Pos2> = in_panes(h, name, 24.0)
        .into_iter()
        .filter(|p| p.y < 50.0)
        .collect();
    *crumbs
        .first()
        .unwrap_or_else(|| panic!("no crumb {name:?}"))
}

fn click_at(h: &mut H, at: Pos2, mods: Modifiers, button: PointerButton) {
    h.input_mut().modifiers = mods;
    // The pointer arrives first, as a real mouse does: a press in the frame the pointer
    // jumps would drag what is under the old position (a window).
    h.input_mut().events.push(Event::PointerMoved(at));
    h.step();
    for pressed in [true, false] {
        h.input_mut().events.push(Event::PointerButton {
            pos: at,
            button,
            pressed,
            modifiers: mods,
        });
        h.step();
    }
    h.input_mut().modifiers = NONE;
    h.run_steps(2);
}

fn click_row(h: &mut H, name: &str, pane: usize, mods: Modifiers) {
    let at = row(h, name, pane);
    click_at(h, at, mods, PointerButton::Primary);
}

fn click_nth(h: &mut H, label: &str, n: usize) {
    let at = pos(h, label, n);
    click_at(h, at, NONE, PointerButton::Primary);
}

fn right_click_row(h: &mut H, name: &str) {
    let at = row(h, name, 0);
    click_at(h, at, NONE, PointerButton::Secondary);
}

fn click(h: &mut H, label: &str) {
    let at = pos(h, label, 0);
    click_at(h, at, NONE, PointerButton::Primary);
}

/// The last widget labelled `label`: windows and popups come after the panels (the
/// sidebar has Remotes, Cloud, Library and Devices too).
fn click_last(h: &mut H, label: &str) {
    let found: Vec<Pos2> = h.query_all_by_label(label).map(|n| center_of(&n)).collect();
    let at = *found.last().unwrap_or_else(|| panic!("no {label:?}"));
    click_at(h, at, NONE, PointerButton::Primary);
}

fn dump(h: &H, part: &str) -> String {
    let all: Vec<_> = h
        .query_all_by_label_contains(part)
        .map(|n| format!("{:?} {:?} {:?}", n.role(), n.label(), n.raw_bounds()))
        .collect();
    all.join(
        "
",
    )
}

fn has(h: &H, label: &str) -> bool {
    h.query_all_by_label(label).next().is_some()
}

fn has_contains(h: &H, part: &str) -> bool {
    h.query_all_by_label_contains(part).next().is_some()
}

/// Drags with the primary button from `from` to `to`, a few frames per leg.
fn drag(h: &mut H, from: Pos2, to: Pos2) {
    h.input_mut().events.push(Event::PointerMoved(from));
    h.step();
    h.input_mut().events.push(Event::PointerButton {
        pos: from,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: NONE,
    });
    h.step();
    for i in 1..=6 {
        let t = i as f32 / 6.0;
        h.input_mut()
            .events
            .push(Event::PointerMoved(from + (to - from) * t));
        h.step();
    }
    h.input_mut().events.push(Event::PointerButton {
        pos: to,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: NONE,
    });
    h.run_steps(3);
}

fn names(s: &AppState, p: usize) -> Vec<String> {
    let tab = s.tab(p);
    // Search and library tabs key entries by full path; the row shows the name.
    tab.visible_cached()
        .iter()
        .map(|&i| tab.shown_name(&tab.entries()[i]).to_owned())
        .collect()
}

fn dir_is(s: &AppState, p: usize, dir: &Path) -> bool {
    s.tab(p).dir == VPath::local(dir) && !s.tab(p).loading
}

fn toast_texts(s: &AppState) -> Vec<String> {
    s.toasts.list.iter().map(|t| t.text.clone()).collect()
}

// ---------------------------------------------------------------------------------------
// Navigation

/// Ctrl+L, type a path, Enter: the pane opens it.
#[test]
fn open_folder_by_typing_a_path() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    key(&mut h, CMD, Key::L);
    assert!(s(&h).panes[0].path_edit.is_some(), "path box open");
    // The shown path is selected: typing replaces it.
    type_text(&mut h, &root.join("Backup").display().to_string());
    key(&mut h, NONE, Key::Enter);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &root.join("Backup"))));
    assert!(s(&h).panes[0].path_edit.is_none());
    assert_eq!(
        names(s(&h), 0),
        ["budget.csv", "notes-2025.md", "report.pdf"]
    );
    // A file path opens its folder with the file selected.
    key(&mut h, CMD, Key::L);
    type_text(
        &mut h,
        &root
            .join("Documents")
            .join("todo.txt")
            .display()
            .to_string(),
    );
    key(&mut h, NONE, Key::Enter);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &root.join("Documents"))));
    assert!(wait(&mut h, 5, &|s| s.tab(0).cursor.as_deref()
        == Some("todo.txt")));
    shot(&mut h, "open-folder-by-typing-a-path");
}

/// A click on a part of the path goes there.
#[test]
fn breadcrumb_click() {
    let root = demo();
    let shed = root.join("Documents").join("Projects").join("shed");
    let mut h = app(&shed, &root.join("Pictures"));
    // The left pane's crumbs come first.
    let at = crumb(&h, "Documents");
    click_at(&mut h, at, NONE, PointerButton::Primary);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &root.join("Documents"))));
    shot(&mut h, "breadcrumb-click");
}

/// Alt+Left / Alt+Right / Alt+Up and Backspace, and the arrow buttons.
#[test]
fn back_forward_up() {
    let root = demo();
    let docs = root.join("Documents");
    let mut h = app(&docs, &root.join("Pictures"));
    sm(&mut h).run(0, Action::Navigate(VPath::local(docs.join("Projects"))));
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &docs.join("Projects"))));
    key(&mut h, ALT, Key::ArrowLeft);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &docs)), "back");
    key(&mut h, ALT, Key::ArrowRight);
    assert!(
        wait(&mut h, 10, &|s| dir_is(s, 0, &docs.join("Projects"))),
        "forward"
    );
    key(&mut h, ALT, Key::ArrowUp);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &docs)), "up");
    // Up selects the folder it came from.
    assert_eq!(s(&h).tab(0).cursor.as_deref(), Some("Projects"));
    key(&mut h, NONE, Key::Backspace);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, root)), "backspace");
    // The toolbar arrows.
    click(&mut h, "Back (Alt+Left)");
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &docs)), "back button");
    click(&mut h, "Forward (Alt+Right)");
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, root)), "forward button");
    click(&mut h, "Up (Alt+Up)");
    assert!(wait(&mut h, 10, &|s| s.tab(0).dir
        == VPath::local(root.parent().unwrap())));
    shot(&mut h, "back-forward-up");
}

/// Ctrl+T, Ctrl+W, the + and × buttons, and a tab dragged to another place.
#[test]
fn tabs_new_close_move() {
    let root = demo();
    let docs = root.join("Documents");
    let mut h = app(&docs, &root.join("Pictures"));
    key(&mut h, CMD, Key::T);
    assert_eq!(s(&h).panes[0].tabs.len(), 2);
    assert_eq!(s(&h).panes[0].active, 1, "the new tab is active");
    sm(&mut h).run(0, Action::Navigate(VPath::local(docs.join("Reports"))));
    assert!(wait(&mut h, 10, &listed));
    click(&mut h, "New tab (Ctrl+T)");
    assert_eq!(s(&h).panes[0].tabs.len(), 3);
    sm(&mut h).run(0, Action::Navigate(VPath::local(docs.join("Projects"))));
    assert!(wait(&mut h, 10, &listed));
    let titles = |h: &H| -> Vec<String> { s(h).panes[0].tabs.iter().map(|t| t.title()).collect() };
    assert_eq!(titles(&h), ["Documents", "Reports", "Projects"]);
    // Drag "Projects" onto "Documents".
    let (from, to) = (tab_pos(&h, "Projects"), tab_pos(&h, "Documents"));
    drag(&mut h, from, to);
    assert_eq!(
        titles(&h),
        ["Projects", "Documents", "Reports"],
        "dragged first"
    );
    assert_eq!(
        s(&h).tab(0).dir,
        VPath::local(docs.join("Projects")),
        "still active"
    );
    shot(&mut h, "tabs-new-close-move");
    key(&mut h, CMD, Key::W);
    assert_eq!(titles(&h), ["Documents", "Reports"]);
    // × closes the tab it belongs to.
    click(&mut h, "Close tab Documents");
    assert_eq!(s(&h).panes[0].tabs.len(), 1);
    // The last tab never closes.
    key(&mut h, CMD, Key::W);
    assert_eq!(s(&h).panes[0].tabs.len(), 1);
}

/// Ctrl+Shift+D: one pane / two panes; F6 and a click switch the active pane.
#[test]
fn dual_pane_toggle_and_switch() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Backup"));
    assert!(s(&h).dual);
    key(&mut h, NONE, Key::F6);
    assert_eq!(s(&h).active, 1);
    // A click into the left pane's rows makes it active.
    click_row(&mut h, "notes.md", 0, NONE);
    assert_eq!(s(&h).active, 0);
    key(&mut h, CMD_SHIFT, Key::D);
    assert!(!s(&h).dual);
    assert!(
        h.query_all_by_label("notes-2025.md").next().is_none(),
        "right pane hidden"
    );
    shot(&mut h, "dual-pane-off");
    key(&mut h, CMD_SHIFT, Key::D);
    assert!(s(&h).dual);
    assert!(wait(&mut h, 10, &listed));
    assert!(
        h.query_all_by_label("notes-2025.md").next().is_some(),
        "right pane back"
    );
    shot(&mut h, "dual-pane-on");
}

/// Click, Ctrl+click and Shift+click select; Shift+Down extends; the status bar counts.
#[test]
fn select_with_shift_and_ctrl() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    click_row(&mut h, "budget.csv", 0, NONE);
    assert_eq!(s(&h).tab(0).selected.len(), 1);
    click_row(&mut h, "notes.md", 0, CMD);
    let sel: Vec<_> = s(&h).tab(0).selected.iter().cloned().collect();
    assert_eq!(sel, ["budget.csv", "notes.md"]);
    // Shift+click: the range from the last clicked row (notes.md), as in Explorer.
    click_row(&mut h, "lake.jpg", 0, Modifiers::SHIFT);
    let sel: Vec<_> = s(&h).tab(0).selected.iter().cloned().collect();
    assert_eq!(sel, ["lake.jpg", "letter.docx", "notes.md"]);
    assert!(has(&h, "3 selected"), "status bar");
    // Shift+Down moves the range's end from lake.jpg to letter.docx.
    key(&mut h, Modifiers::SHIFT, Key::ArrowDown);
    let sel: Vec<_> = s(&h).tab(0).selected.iter().cloned().collect();
    assert_eq!(sel, ["letter.docx", "notes.md"]);
    shot(&mut h, "select-with-shift-and-ctrl");
    // Esc clears the selection.
    key(&mut h, NONE, Key::Escape);
    assert!(s(&h).tab(0).selected.is_empty());
    // Ctrl+A selects all.
    key(&mut h, CMD, Key::A);
    assert_eq!(s(&h).tab(0).selected.len(), names(s(&h), 0).len());
}

/// Typing filters the list; Esc clears it.
#[test]
fn filter_as_you_type() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    type_text(&mut h, "n");
    type_text(&mut h, "o");
    assert!(s(&h).tab(0).filter_open);
    assert_eq!(s(&h).tab(0).filter, "no");
    assert_eq!(names(s(&h), 0), ["notes.md"]);
    assert!(has(&h, "filter: no"), "status bar shows the filter");
    shot(&mut h, "filter-as-you-type");
    key(&mut h, NONE, Key::Escape);
    assert!(!s(&h).tab(0).filter_open);
    assert!(names(s(&h), 0).len() > 5);
}

/// A click on each column header sorts by it; a second click reverses.
#[test]
fn sort_by_every_column() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Backup"));
    let files = |h: &H| -> Vec<String> {
        let tab = s(h).tab(0);
        tab.visible_cached()
            .iter()
            .map(|&i| &tab.entries()[i])
            .filter(|e| e.kind != keel_vfs::Kind::Dir)
            .map(|e| e.name.clone())
            .collect()
    };
    for (label, key_, first) in [
        ("Size", SortKey::Size, "todo.txt"),
        ("Modified", SortKey::Modified, "letter.docx"),
        ("Ext", SortKey::Ext, "budget.csv"),
        ("Name", SortKey::Name, "budget.csv"),
    ] {
        click_nth(&mut h, label, 0);
        assert_eq!(s(&h).tab(0).sort, (key_, true), "{label}");
        assert_eq!(files(&h)[0], first, "{label} ascending");
        let arrow = format!("{label}, sorted ascending");
        click_nth(&mut h, &arrow, 0);
        assert_eq!(s(&h).tab(0).sort, (key_, false), "{label} again");
        assert_ne!(files(&h)[0], first, "{label} descending");
        let name = format!("sort-by-{}", label.to_lowercase());
        shot(&mut h, &name);
    }
}

/// Columns: the buttons switch the view; Right opens the folder in the next column.
#[test]
fn column_view() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    click_nth(&mut h, "Columns", 0);
    assert_eq!(s(&h).panes[0].view, ViewMode::Columns);
    click_row(&mut h, "Projects", 0, NONE);
    assert!(wait(&mut h, 10, &|s| s.panes[0].tabs[0].columns.cols.len() == 1));
    assert!(wait(&mut h, 10, &|s| !s.tab(0).loading));
    // The next column lists on its own worker.
    assert!(wait(&mut h, 10, &|s| {
        s.panes[0].tabs[0].columns.cols.iter().all(|c| !c.loading)
    }));
    assert!(
        h.query_all_by_label_contains("shed, folder, ")
            .next()
            .is_some(),
        "next column"
    );
    shot(&mut h, "column-view");
    click_nth(&mut h, "Details", 0);
    assert_eq!(s(&h).panes[0].view, ViewMode::Details);
}

/// Grid: thumbnails for the photos.
#[test]
fn grid_view() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    // The right pane's Grid button.
    click_nth(&mut h, "Grid", 1);
    assert_eq!(s(&h).panes[1].view, ViewMode::Grid);
    assert!(wait(&mut h, 60, &|s| s.thumbs.len() >= 6), "thumbnails");
    shot(&mut h, "grid-view");
}

/// Task 46: the Sizes view walks a folder outside the library on its own, shows its
/// children biggest first with share bars, Enter goes down (the walk already knows the
/// totals below) and Backspace up; the Size column and the status bar show folder totals;
/// Calculate sizes can be cancelled.
#[test]
fn sizes_view_and_calculate_sizes() {
    let dir = std::env::temp_dir().join(format!("keel-walk-{}-sizes", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let put = |rel: &str, n: usize| {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![b'x'; n]).unwrap();
    };
    put("big/one.bin", 2_000);
    put("big/inner/two.bin", 1_000);
    put("small/s.txt", 10);
    put("mid.bin", 500);
    let mut h = app(&dir, &demo().join("Documents"));
    click_nth(&mut h, "Sizes", 0);
    assert_eq!(s(&h).panes[0].view, ViewMode::Sizes);
    assert!(
        wait(&mut h, 20, &|s| job_done(s) && s.sizes.gen > 1),
        "walked"
    );
    h.run_steps(3);
    assert_eq!(
        names(s(&h), 0),
        ["big", "mid.bin", "small"],
        "biggest first"
    );
    assert!(has(&h, "Share") && has(&h, "Of used"), "the Sizes columns");
    assert!(has_contains(&h, "3 kB"), "big's total in the Size column");
    shot(&mut h, "sizes-view");
    sm(&mut h).tab_mut(0).click("big", false, false);
    key(&mut h, NONE, Key::Enter);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &dir.join("big")) && listed(s)));
    h.run_steps(3);
    assert_eq!(names(s(&h), 0), ["one.bin", "inner"]);
    let inner = s(&h)
        .tab(0)
        .entries()
        .iter()
        .find(|e| e.name == "inner")
        .unwrap()
        .clone();
    assert!(
        matches!(s(&h).sizes.get(&inner), Some(crate::sizes::Size::Known(t)) if t.bytes == 1_000),
        "known from the walk above"
    );
    key(&mut h, NONE, Key::Backspace);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &dir) && listed(s)));
    // Back in Details, the status bar counts a selected folder's total.
    click_nth(&mut h, "Details", 0);
    sm(&mut h).tab_mut(0).click("big", false, false);
    h.run_steps(3);
    assert!(has_contains(&h, "3 kB"), "the selected folder's total");

    // Calculate sizes over many folders, cancelled at once.
    for i in 0..400 {
        put(&format!("many/d{i}/f.bin"), 10);
    }
    sm(&mut h).run(0, Action::Navigate(VPath::local(dir.join("many"))));
    assert!(wait(&mut h, 10, &listed));
    sm(&mut h).run(0, Action::CalculateSizes { fresh: true });
    let job = s(&h).jobs.list.last().unwrap();
    assert!(job.title.starts_with("Calculating sizes"), "{}", job.title);
    job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(wait(&mut h, 20, &job_done));
    let cancelled = s(&h).jobs.list.last().unwrap();
    assert!(
        matches!(&cancelled.done, Some(Err(e)) if crate::jobs::is_cancel(e)),
        "cancelled"
    );
    let tab = s(&h).tab(0);
    let pending = (tab.entries().iter())
        .filter(|e| s(&h).sizes.get(e) == Some(crate::sizes::Size::Pending))
        .count();
    assert_eq!(pending, 0, "nothing left computing");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The status bar counts items with the right plural.
#[test]
fn status_bar_counts() {
    let root = demo();
    let shed = root.join("Documents").join("Projects");
    let mut h = app(&shed, &root.join("Pictures"));
    assert_eq!(names(s(&h), 0), ["shed"]);
    assert!(has(&h, "1 item"), "one folder: \"1 item\"");
    sm(&mut h).run(0, Action::Navigate(VPath::local(shed.join("shed"))));
    assert!(wait(&mut h, 10, &listed));
    assert!(has(&h, "2 items"));
    key(&mut h, CMD, Key::A);
    assert!(has(&h, "2 selected"));
    shot(&mut h, "status-bar-counts");
}

/// The text box on the same row as `label`, as a click target.
fn field(h: &H, label: &str) -> Pos2 {
    // A field labelled by `label` (AccessKit `labelled_by`) is found by it.
    let own = h
        .query_all_by_label(label)
        .find(|n| n.role() == egui::accesskit::Role::TextInput);
    if let Some(n) = own {
        return center_of(&n);
    }
    let at = pos(h, label, 0);
    let mut fields: Vec<Pos2> = h
        .query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::TextInput))
        .map(|n| center_of(&n))
        .filter(|p| (p.y - at.y).abs() < 10.0 && p.x > at.x)
        .collect();
    fields.sort_by(|a, b| a.x.total_cmp(&b.x));
    *fields
        .first()
        .unwrap_or_else(|| panic!("no field right of {label:?}"))
}

/// Clicks into a text box, selects what it holds and types `text` over it.
fn fill(h: &mut H, at: Pos2, text: &str) {
    click_at(h, at, NONE, PointerButton::Primary);
    key(h, CMD, Key::A);
    type_text(h, text);
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn job_done(s: &AppState) -> bool {
    !s.jobs.list.is_empty() && s.jobs.list.iter().all(|j| j.done.is_some())
}

// ---------------------------------------------------------------------------------------
// Previews

/// F3 opens the preview panel; each fixture type shows its kind of preview.
#[test]
fn preview_each_file_type() {
    use keel_preview::Preview;
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    key(&mut h, NONE, Key::F3);
    assert!(s(&h).preview.open);
    type Check = fn(&Preview) -> bool;
    let cases: [(&str, &str, Check); 7] = [
        ("garden.rs", "DRY", |p| matches!(p, Preview::Text { .. })),
        ("todo.txt", "Call the plumber", |p| {
            matches!(p, Preview::Text { .. })
        }),
        ("notes.md", "Garden notes", |p| {
            matches!(p, Preview::Markdown(_))
        }),
        ("budget.csv", "Seeds", |p| {
            matches!(p, Preview::Table { .. })
        }),
        ("lake.jpg", "", |p| matches!(p, Preview::Image(_))),
        ("letter.docx", "", |p| matches!(p, Preview::Doc { .. })),
        ("plants.xlsx", "", |p| matches!(p, Preview::Table { .. })),
    ];
    for (file, text, check) in cases {
        click_row(&mut h, file, 0, NONE);
        assert!(wait(&mut h, 30, &previewed), "{file} previewed");
        let p = s(&h).preview.current.as_ref().unwrap();
        assert!(check(p), "{file}: {}", preview_kind(p));
        if !text.is_empty() {
            assert!(has_contains(&h, text), "{file} shows {text:?}");
        }
        let stem = file.replace('.', "-");
        shot(&mut h, &format!("preview-{stem}"));
    }
    // A file inside a zip, opened like a folder.
    click_row(&mut h, "site.zip", 0, NONE);
    key(&mut h, NONE, Key::Enter);
    assert!(wait(&mut h, 10, &|s| s
        .tab(0)
        .dir
        .split_archive()
        .is_some()
        && !s.tab(0).loading));
    assert!(names(s(&h), 0).contains(&"recipes".to_owned()));
    let zip = VPath::local(root.join("Documents").join("site.zip"));
    sm(&mut h).run(0, Action::Navigate(VPath::join_archive(&zip, "recipes")));
    assert!(wait(&mut h, 10, &listed));
    click_row(&mut h, "bread.md", 0, NONE);
    assert!(wait(&mut h, 30, &previewed), "zip entry previewed");
    assert!(matches!(s(&h).preview.current, Some(Preview::Markdown(_))));
    assert!(has_contains(&h, "500 g flour"));
    shot(&mut h, "preview-zip-entry");
    // Alt+Up leaves the archive, back to its folder.
    key(&mut h, ALT, Key::ArrowUp);
    assert!(wait(&mut h, 10, &listed));
    key(&mut h, ALT, Key::ArrowUp);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &root.join("Documents"))));
    assert_eq!(s(&h).tab(0).cursor.as_deref(), Some("site.zip"));
    key(&mut h, NONE, Key::F3);
    assert!(!s(&h).preview.open, "F3 again hides it");
}

fn preview_kind(p: &keel_preview::Preview) -> String {
    use keel_preview::Preview::*;
    match p {
        Text { .. } => "text".into(),
        Markdown(_) => "markdown".into(),
        Image(_) => "image".into(),
        Table { .. } => "table".into(),
        Pdf { .. } => "pdf".into(),
        Doc { .. } => "doc".into(),
        Video { .. } => "video".into(),
        Audio { .. } => "audio".into(),
        Font { .. } => "font".into(),
        Hex { .. } => "hex".into(),
        TooLarge(n) => format!("too large {n}"),
        Unsupported => "unsupported".into(),
        Missing(m) => format!("missing: {m}"),
        Error(e) => format!("error: {e}"),
    }
}

/// PDF pages (pdfium from `target/deps`, `scripts/fetch-deps`) and a video still (ffmpeg on
/// the PATH); each part is skipped, and says so, when its helper is missing.
#[test]
#[ignore]
fn preview_pdf_and_video() {
    use keel_preview::Preview;
    let root = demo();
    let pdfium = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deps");
    keel_preview::init_pdfium(&pdfium);
    let dir = scratch("video");
    let clip = keel_core::find_tool("ffmpeg").map(|ffmpeg| {
        let out = dir.join("a").join("clip.mp4");
        let ok = std::process::Command::new(ffmpeg)
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=1.5:size=320x240:rate=25")
            .args(["-c:v", "mpeg4"])
            .arg(&out)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ffmpeg made the clip");
        out
    });
    let mut h = app(&root.join("Documents"), &dir.join("a"));
    key(&mut h, NONE, Key::F3);
    click_row(&mut h, "report.pdf", 0, NONE);
    assert!(wait(&mut h, 30, &previewed));
    match s(&h).preview.current.as_ref().unwrap() {
        Preview::Pdf { pages, .. } => {
            assert_eq!(*pages, 1);
            shot(&mut h, "preview-report-pdf");
        }
        other => println!("preview-report-pdf skipped: {}", preview_kind(other)),
    }
    if clip.is_some() {
        key(&mut h, NONE, Key::F6);
        click_row(&mut h, "clip.mp4", 1, NONE);
        assert!(wait(&mut h, 30, &previewed));
        let p = s(&h).preview.current.as_ref().unwrap();
        assert!(matches!(p, Preview::Video { .. }), "{}", preview_kind(p));
        shot(&mut h, "preview-video-still");
    } else {
        println!("preview-video-still skipped: ffmpeg is not installed");
    }
}

// ---------------------------------------------------------------------------------------
// Copy, move, delete

/// A row dragged onto the other pane copies; the job shows in the jobs panel and the file
/// lands.
#[test]
fn drag_copy_and_the_jobs_panel() {
    let dir = scratch("dragcopy");
    let mut h = app(&dir.join("a"), &dir.join("b"));
    let from = row(&h, "todo.txt", 0);
    let to = row(&h, "budget.csv", 1) + egui::vec2(0.0, 60.0);
    drag(&mut h, from, to);
    assert!(wait(&mut h, 10, &|s| !s.jobs.list.is_empty()), "a job");
    let title = s(&h).jobs.list[0].title.clone();
    assert!(title.starts_with("Copying"), "{title}");
    assert!(wait(&mut h, 20, &job_done));
    assert!(has(&h, "Done"), "the jobs panel says Done");
    shot(&mut h, "jobs-panel-after-copy");
    let (a, b) = (
        dir.join("a").join("todo.txt"),
        dir.join("b").join("todo.txt"),
    );
    assert_eq!(read(&b), read(&a));
    assert!(a.exists(), "a copy, not a move");
    let todo = "todo.txt".to_owned();
    assert!(wait(&mut h, 10, &|s| names(s, 1).contains(&todo)), "listed");
}

/// Copying onto a name that exists asks once: Skip, Overwrite, Keep both, Cancel.
#[test]
fn copy_conflicts() {
    let dir = scratch("conflicts");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let mut h = app(&a, &b);
    let drop = |h: &mut H| {
        let paths = vec![VPath::local(a.join("budget.csv"))];
        sm(h).run(
            1,
            Action::Drop {
                paths,
                from: Some((0, VPath::local(&a))),
                dst: VPath::local(&b),
            },
        );
        let asked =
            |s: &AppState| matches!(s.dialog, Some(crate::dialogs::Dialog::Conflict { .. }));
        assert!(wait(h, 10, &asked), "conflict asked");
        assert!(has(h, "An item with this name already exists:"));
    };
    drop(&mut h);
    shot(&mut h, "copy-conflict");
    click(&mut h, "Cancel");
    assert!(s(&h).dialog.is_none());
    h.run_steps(5);
    assert!(s(&h).jobs.list.is_empty(), "cancel starts nothing");

    drop(&mut h);
    click(&mut h, "Skip");
    assert!(wait(&mut h, 20, &job_done));
    assert_eq!(read(&b.join("budget.csv")), "old budget\n", "skipped");
    assert!(
        has_contains(&h, "skipped 1 item"),
        "the job says what it skipped"
    );
    sm(&mut h).jobs.list.clear();

    drop(&mut h);
    click(&mut h, "Keep both");
    assert!(wait(&mut h, 20, &job_done));
    let both: Vec<String> = std::fs::read_dir(&b)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(both.len(), 2, "{both:?}");
    assert_eq!(read(&b.join("budget.csv")), "old budget\n");
    sm(&mut h).jobs.list.clear();

    drop(&mut h);
    click(&mut h, "Overwrite");
    assert!(wait(&mut h, 20, &job_done));
    assert_eq!(read(&b.join("budget.csv")), read(&a.join("budget.csv")));
    shot(&mut h, "copy-conflict-done");
}

/// Shift+drop moves; Del asks before the trash and Cancel keeps the file.
#[test]
fn move_and_delete_confirmation() {
    let dir = scratch("move");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let mut h = app(&a, &b);
    let paths = vec![VPath::local(a.join("todo.txt"))];
    h.input_mut().modifiers = Modifiers::SHIFT;
    h.step();
    let from = Some((0, VPath::local(&a)));
    let dst = VPath::local(&b);
    sm(&mut h).run(1, Action::Drop { paths, from, dst });
    h.input_mut().modifiers = NONE;
    assert!(wait(&mut h, 20, &job_done));
    let title = s(&h).jobs.list[0].title.clone();
    assert!(title.starts_with("Moving"), "{title}");
    assert!(!a.join("todo.txt").exists() && b.join("todo.txt").exists());

    // Del on notes.md: the question, then Cancel.
    let todo = "todo.txt".to_owned();
    assert!(wait(&mut h, 10, &|s| !names(s, 0).contains(&todo)));
    click_row(&mut h, "notes.md", 0, NONE);
    key(&mut h, NONE, Key::Delete);
    assert!(has(&h, "Move 1 item to the trash?"));
    assert!(has(&h, "Move to trash"));
    shot(&mut h, "delete-confirmation");
    click(&mut h, "Cancel");
    assert!(s(&h).dialog.is_none());
    h.run_steps(5);
    assert!(a.join("notes.md").exists(), "still there");
}

// ---------------------------------------------------------------------------------------
// Rename, bulk rename, new items

/// F2 renames in place: Enter saves, Esc cancels, an invalid name is refused.
#[test]
fn rename_in_place() {
    let dir = scratch("rename");
    let a = dir.join("a");
    let mut h = app(&a, &dir.join("b"));
    click_row(&mut h, "todo.txt", 0, NONE);
    key(&mut h, NONE, Key::F2);
    let editing = |s: &AppState| s.tab(0).renaming.as_ref().map(|r| r.1.clone());
    assert_eq!(editing(s(&h)).as_deref(), Some("todo.txt"));
    // Typing replaces the name's stem (the extension stays), as in Explorer and Finder.
    type_text(&mut h, "chores");
    assert_eq!(
        editing(s(&h)).as_deref(),
        Some("chores.txt"),
        "stem selected"
    );
    shot(&mut h, "rename-in-place");
    key(&mut h, NONE, Key::Escape);
    assert!(s(&h).tab(0).renaming.is_none(), "Esc cancels");
    h.run_steps(3);
    assert!(a.join("todo.txt").exists());

    key(&mut h, NONE, Key::F2);
    type_text(&mut h, "chores");
    key(&mut h, NONE, Key::Enter);
    let chores = "chores.txt".to_owned();
    assert!(wait(&mut h, 10, &|s| names(s, 0).contains(&chores)));
    assert!(a.join("chores.txt").exists() && !a.join("todo.txt").exists());

    // An invalid name: refused with a toast, nothing renamed.
    click_row(&mut h, "notes.md", 0, NONE);
    key(&mut h, NONE, Key::F2);
    key(&mut h, CMD, Key::A);
    type_text(&mut h, if cfg!(windows) { "a:b" } else { "a/b" });
    key(&mut h, NONE, Key::Enter);
    h.run_steps(3);
    let toasts = toast_texts(s(&h));
    assert!(
        toasts
            .iter()
            .any(|t| t.starts_with("A name cannot contain")),
        "{toasts:?}"
    );
    assert!(a.join("notes.md").exists());
    shot(&mut h, "rename-invalid-name");
}

/// Ctrl+F2: the bulk rename dialog previews, Apply renames, the palette's Undo reverts.
#[test]
fn bulk_rename_and_undo() {
    let dir = scratch("bulk");
    let a = dir.join("a");
    let mut h = app(&a, &dir.join("b"));
    click_row(&mut h, "budget.csv", 0, NONE);
    click_row(&mut h, "todo.txt", 0, CMD);
    key(&mut h, CMD, Key::F2);
    assert!(has(&h, "Rename 2 items"));
    let at = field(&h, "Pattern");
    fill(&mut h, at, "{name}-{n:3}.{ext}");
    assert!(
        has(&h, "budget-001.csv") && has(&h, "todo-002.txt"),
        "preview rows"
    );
    shot(&mut h, "bulk-rename");
    click(&mut h, "Apply");
    let renamed = |a: &Path| a.join("budget-001.csv").exists() && a.join("todo-002.txt").exists();
    assert!(wait(&mut h, 10, &|_| renamed(&a)));
    let todo = "todo-002.txt".to_owned();
    assert!(wait(&mut h, 10, &|s| names(s, 0).contains(&todo)));
    // Undo from the command palette.
    key(&mut h, CMD_SHIFT, Key::P);
    type_text(&mut h, "undo rename");
    key(&mut h, NONE, Key::Enter);
    let back = |a: &Path| a.join("budget.csv").exists() && a.join("todo.txt").exists();
    assert!(wait(&mut h, 10, &|_| back(&a)));
}

/// Ctrl+Z: F2 rename then Ctrl+Z brings the name back (Ctrl+Z while typing a name stays
/// the text box's own); a move's toast has an Undo button whose preview moves it back.
#[test]
fn undo_rename_and_move() {
    let dir = scratch("undo");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let mut h = app(&a, &b);
    key(&mut h, CMD, Key::Z);
    assert!(wait(&mut h, 5, &|s| toast_texts(s)
        .contains(&"Nothing to undo".to_owned())));
    click_row(&mut h, "todo.txt", 0, NONE);
    key(&mut h, NONE, Key::F2);
    type_text(&mut h, "chores");
    key(&mut h, NONE, Key::Enter);
    let chores = "chores.txt".to_owned();
    assert!(wait(&mut h, 10, &|s| names(s, 0).contains(&chores)));
    assert!(wait(&mut h, 10, &|s| s.undo.label().is_some()), "recorded");
    let renamed = format!("Renamed todo.txt to {chores}");
    assert!(toast_texts(s(&h)).contains(&renamed), "the toast says so");
    // In the rename box Ctrl+Z is the text's undo: no file comes back.
    click_row(&mut h, "notes.md", 0, NONE);
    key(&mut h, NONE, Key::F2);
    type_text(&mut h, "x");
    key(&mut h, CMD, Key::Z);
    key(&mut h, NONE, Key::Escape);
    h.run_steps(5);
    assert!(a.join("chores.txt").exists() && a.join("notes.md").exists());
    // In the list it is file undo: the old name is back, with no question asked.
    key(&mut h, CMD, Key::Z);
    assert!(wait(&mut h, 10, &|_| a.join("todo.txt").exists()));
    assert!(!a.join("chores.txt").exists());
    shot(&mut h, "undo-rename");

    // A move, then its toast's Undo: the preview, then back.
    let paths = vec![VPath::local(a.join("todo.txt"))];
    h.input_mut().modifiers = Modifiers::SHIFT;
    h.step();
    let (from, dst) = (Some((0, VPath::local(&a))), VPath::local(&b));
    sm(&mut h).run(1, Action::Drop { paths, from, dst });
    h.input_mut().modifiers = NONE;
    assert!(wait(&mut h, 20, &|_| b.join("todo.txt").exists()));
    let moved = format!("Moved 1 item to {}", VPath::local(&b).display());
    assert!(wait(&mut h, 10, &|s| toast_texts(s).contains(&moved)));
    // The toast slides in first.
    std::thread::sleep(std::time::Duration::from_millis(500));
    h.run_steps(3);
    click(&mut h, "Undo");
    let question = format!("Undo move: 1 item back to {}?", VPath::local(&a).display());
    let asked = wait(&mut h, 10, &|s| {
        matches!(
            &s.dialog,
            Some(crate::dialogs::Dialog::Confirm { text, .. }) if *text == question
        )
    });
    assert!(asked, "{:?}", toast_texts(s(&h)));
    shot(&mut h, "undo-move-preview");
    click_last(&mut h, "Undo");
    assert!(wait(&mut h, 20, &|_| a.join("todo.txt").exists()));
    assert!(!b.join("todo.txt").exists());
    assert_eq!(
        read(&a.join("todo.txt")),
        read(&demo().join("Documents/todo.txt"))
    );
}

/// Ctrl+Shift+N and the palette's New file: the dialog, Create, the new row.
#[test]
fn new_folder_and_new_file() {
    let dir = scratch("new");
    let a = dir.join("a");
    let mut h = app(&a, &dir.join("b"));
    key(&mut h, CMD_SHIFT, Key::N);
    assert!(has(&h, "New folder name"));
    shot(&mut h, "new-folder");
    key(&mut h, NONE, Key::Enter);
    assert!(wait(&mut h, 10, &|_| a.join("New folder").is_dir()));
    let folder = "New folder".to_owned();
    assert!(wait(&mut h, 10, &|s| names(s, 0).contains(&folder)));
    key(&mut h, CMD_SHIFT, Key::P);
    type_text(&mut h, "new file");
    key(&mut h, NONE, Key::Enter);
    assert!(has(&h, "New file name"));
    click(&mut h, "Create");
    assert!(wait(&mut h, 10, &|_| a.join("New file.txt").is_file()));
    // A name that exists is refused, nothing is overwritten.
    std::fs::write(a.join("New file.txt"), "keep").unwrap();
    key(&mut h, CMD_SHIFT, Key::P);
    type_text(&mut h, "new file");
    key(&mut h, NONE, Key::Enter);
    click(&mut h, "Create");
    assert!(
        wait(&mut h, 10, &|s| !s.toasts.list.is_empty()),
        "an error toast"
    );
    assert_eq!(read(&a.join("New file.txt")), "keep");
}

/// Ctrl+Shift+O lists apps for the file (nothing is launched); Properties shows the facts.
#[test]
fn open_with_and_properties() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    click_row(&mut h, "todo.txt", 0, NONE);
    key(&mut h, CMD_SHIFT, Key::O);
    let open_with =
        |s: &AppState| matches!(s.dialog, Some(crate::dialogs::Dialog::OpenWith { .. }));
    assert!(wait(&mut h, 20, &open_with));
    assert!(has(&h, "Open \"todo.txt\" with"));
    assert!(has(&h, "Remember for .txt files"));
    shot(&mut h, "open-with");
    click(&mut h, "Cancel");
    assert!(s(&h).dialog.is_none());

    // Properties from the context menu of a folder.
    right_click_row(&mut h, "Projects");
    click(&mut h, "Properties");
    assert!(wait(&mut h, 20, &|s| matches!(
        &s.dialog,
        Some(crate::dialogs::Dialog::Properties {
            info: Some(Ok(_)),
            ..
        })
    )));
    h.run_steps(2);
    assert!(has(&h, "Folder"));
    assert!(
        has(&h, "2 files, 1 folder"),
        "Contains, with the right plural"
    );
    shot(&mut h, "properties");
    click(&mut h, "Close");
    assert!(s(&h).dialog.is_none());
}

// ---------------------------------------------------------------------------------------
// Archives

/// Add to zip, Compress (dialog), Extract here, Extract to folder; Extract to… opens the
/// system folder picker, so it is only checked to be offered.
#[test]
fn zip_compress_and_extract() {
    let dir = scratch("zip");
    let a = dir.join("a");
    let mut h = app(&a, &dir.join("b"));
    // Add to "todo.zip" from the context menu.
    right_click_row(&mut h, "todo.txt");
    click(&mut h, "Add to \"todo.zip\"");
    assert!(wait(&mut h, 20, &job_done));
    assert!(a.join("todo.zip").is_file());
    sm(&mut h).jobs.list.clear();

    // Compress two files: the dialog's name and formats.
    click_row(&mut h, "budget.csv", 0, NONE);
    click_row(&mut h, "notes.md", 0, CMD);
    right_click_row(&mut h, "notes.md");
    click(&mut h, "Compress to zip…");
    assert!(has(&h, "Compress 2 items to"));
    shot(&mut h, "compress-dialog");
    click(&mut h, "7z");
    let name = match &s(&h).dialog {
        Some(crate::dialogs::Dialog::ZipName { text, .. }) => text.clone(),
        _ => panic!("compress dialog"),
    };
    assert_eq!(name, "a.7z");
    click(&mut h, "Compress");
    assert!(wait(&mut h, 20, &job_done));
    assert!(a.join("a.7z").is_file());
    sm(&mut h).jobs.list.clear();

    // site.zip: the archive items of its context menu.
    right_click_row(&mut h, "site.zip");
    for item in [
        "Extract here",
        "Extract to folder \"site\"",
        "Extract to…",
        "Extract to the other pane",
    ] {
        assert!(has(&h, item), "{item}");
    }
    shot(&mut h, "archive-context-menu");
    click(&mut h, "Extract to folder \"site\"");
    assert!(wait(&mut h, 20, &job_done));
    assert!(a.join("site").join("recipes").join("bread.md").is_file());
    sm(&mut h).jobs.list.clear();
    right_click_row(&mut h, "site.zip");
    click(&mut h, "Extract here");
    assert!(wait(&mut h, 20, &job_done));
    assert!(a.join("README.md").is_file() && a.join("css").join("style.css").is_file());

    // Inside the zip no zip can be made: neither zip item is offered there.
    let zip = VPath::local(a.join("site.zip"));
    sm(&mut h).run(0, Action::Navigate(VPath::join_archive(&zip, "recipes")));
    assert!(wait(&mut h, 10, &listed));
    right_click_row(&mut h, "bread.md");
    assert!(has(&h, "Copy path"), "the item's menu is open");
    assert!(!has_contains(&h, "Add to \"") && !has(&h, "Compress to zip…"));
}

/// More options (the Windows shell menu, Task 43): offered on local files, greyed with its
/// reason inside an archive. The Windows menu itself is never opened here.
#[cfg(windows)]
#[test]
fn more_options_only_on_local_files() {
    let dir = scratch("shell");
    let a = dir.join("a");
    let mut h = app(&a, &dir.join("b"));
    let disabled = |h: &H| {
        h.query_all_by_label("More options")
            .map(|n| n.is_disabled())
            .collect::<Vec<_>>()
    };
    right_click_row(&mut h, "todo.txt");
    assert_eq!(disabled(&h), [false], "offered on a local file");
    key(&mut h, NONE, Key::Escape);

    let zip = VPath::local(a.join("site.zip"));
    sm(&mut h).run(0, Action::Navigate(VPath::join_archive(&zip, "recipes")));
    assert!(wait(&mut h, 10, &listed));
    right_click_row(&mut h, "bread.md");
    assert_eq!(disabled(&h), [true], "greyed inside an archive");
    assert!(has(&h, crate::shell_menu::NOT_LOCAL), "with its reason");
    shot(&mut h, "more-options-greyed");
}

// ---------------------------------------------------------------------------------------
// Command palette and keys

/// What a user can see change: tabs, folders, panes, dialogs, panels, jobs, toasts.
fn fingerprint(s: &AppState) -> String {
    let panes: Vec<_> = (0..2)
        .map(|p| {
            let pane = &s.panes[p];
            let tabs: Vec<_> = (pane.tabs.iter())
                .map(|t| {
                    (
                        t.dir.display(),
                        t.filter_open,
                        t.selected.len(),
                        t.renaming.is_some(),
                        t.listed_req,
                        t.is_search(),
                    )
                })
                .collect();
            (pane.active, pane.view, pane.path_edit.is_some(), tabs)
        })
        .collect();
    format!(
        "{panes:?} {:?}",
        (
            s.active,
            s.dual,
            s.show_hidden,
            (s.preview.open, s.preview.tab),
            s.dialog.is_some(),
            s.settings_open,
            s.jump.open,
            s.dropzone.open,
            s.dropzone.items().len(),
            s.jobs.list.len(),
            toast_texts(s),
            s.theme.dark,
        )
    )
}

/// Every command in the palette (with a selection: the context ones too), picked by a
/// click after typing its name, changes something the user can see. Three launch other
/// programs and are only checked to be bound: Show in system file manager, Open terminal
/// here, Toggle terminal; Extract to… opens the system folder picker, Back up library… and
/// Restore library… the system file dialogs.
#[test]
fn command_palette_every_command() {
    let _clip = crate::clipboard::SYSTEM_CLIPBOARD.lock();
    let dir = scratch("palette");
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::create_dir_all(a.join("sub")).unwrap();
    let mut h = app(&a, &b);
    click_row(&mut h, "todo.txt", 0, NONE);
    key(&mut h, CMD_SHIFT, Key::P);
    assert!(s(&h).palette.open);
    let items: Vec<(String, Action)> = (s(&h).palette.items.iter())
        .map(|i| (i.label.clone(), i.action.clone()))
        .collect();
    assert!(items.len() > 50, "{} commands", items.len());
    shot(&mut h, "command-palette");
    key(&mut h, NONE, Key::Escape);
    assert!(!s(&h).palette.open, "Esc closes it");
    let launches = [
        Action::RevealInSystem,
        Action::OpenTerminal,
        Action::ToggleTerminal,
        Action::ExtractTo,
        Action::Library(crate::library::LibCmd::Backup),
        Action::Library(crate::library::LibCmd::Restore),
    ];
    let mut ran = 0;
    for (label, action) in items {
        if launches.contains(&action) {
            continue;
        }
        // A clean start: one pane on `a` (except Back / Forward: the history), two panes,
        // nothing open.
        {
            let st = sm(&mut h);
            st.dual = true;
            st.active = 0;
            st.dialog = None;
            st.settings_open = false;
            st.jump.open = false;
            st.panes[0].path_edit = None;
            st.tab_mut(0).renaming = None;
        }
        let ready = wait(&mut h, 20, &|s| !s.jump.indexing && listed(s));
        assert!(
            ready,
            "{label}: {} {:?}",
            s(&h).jump.indexing,
            (
                s(&h).tab(0).loading,
                s(&h).tab(1).loading,
                s(&h).tab(1).dir.display()
            )
        );
        if !matches!(action, Action::Back | Action::Forward) && s(&h).tab(0).dir != VPath::local(&a)
        {
            sm(&mut h).run(0, Action::Navigate(VPath::local(&a)));
            assert!(wait(&mut h, 10, &listed));
        }
        let pick = match action {
            Action::Enter => "sub",
            Action::ExtractHere | Action::ExtractToFolder | Action::ExtractToOther => "site.zip",
            _ => "todo.txt",
        };
        if !matches!(action, Action::Back | Action::Forward) {
            sm(&mut h).tab_mut(0).click(pick, false, false);
        }
        sm(&mut h).toasts.list.clear();
        h.run_steps(2);
        let before = fingerprint(s(&h));
        key(&mut h, CMD_SHIFT, Key::P);
        type_text(&mut h, &label);
        // The palette lists a selection's commands once the listing has settled: poll.
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let found: Vec<Pos2> = loop {
            let found: Vec<Pos2> = h
                .query_all_by_label(&label)
                .map(|n| center_of(&n))
                .collect();
            if !found.is_empty() || std::time::Instant::now() > end {
                break found;
            }
            h.step();
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let at = *found.last().unwrap_or_else(|| panic!("{label:?} listed"));
        click_at(&mut h, at, NONE, PointerButton::Primary);
        assert!(!s(&h).palette.open, "{label}: the palette closes");
        let changed = wait(&mut h, 10, &|s| fingerprint(s) != before);
        assert!(changed, "{label:?} did nothing visible");
        ran += 1;
    }
    assert!(ran > 45, "{ran} commands run");
}

/// The keys of the guide's "most useful keys" table, each doing what the guide says.
/// Ctrl+` (a real shell) and the global hotkey (a system-wide grab) are not pressed.
#[test]
fn keyboard_shortcuts_from_the_guide() {
    let _clip = crate::clipboard::SYSTEM_CLIPBOARD.lock();
    let dir = scratch("keys");
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::create_dir_all(a.join("sub")).unwrap();
    let mut h = app(&a, &b);
    // Enter opens a folder; Alt+Up and Backspace go to the parent.
    sm(&mut h).tab_mut(0).click("sub", false, false);
    key(&mut h, NONE, Key::Enter);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &a.join("sub"))), "Enter");
    key(&mut h, ALT, Key::ArrowUp);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &a)), "Alt+Up");
    sm(&mut h).run(0, Action::Navigate(VPath::local(a.join("sub"))));
    assert!(wait(&mut h, 10, &listed));
    key(&mut h, NONE, Key::Backspace);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &a)), "Backspace");
    key(&mut h, ALT, Key::ArrowLeft);
    assert!(
        wait(&mut h, 10, &|s| dir_is(s, 0, &a.join("sub"))),
        "Alt+Left"
    );
    key(&mut h, ALT, Key::ArrowRight);
    assert!(wait(&mut h, 10, &|s| dir_is(s, 0, &a)), "Alt+Right");
    key(&mut h, NONE, Key::F6);
    assert_eq!(s(&h).active, 1, "F6");
    key(&mut h, NONE, Key::F6);
    key(&mut h, CMD, Key::T);
    assert_eq!(s(&h).panes[0].tabs.len(), 2, "Ctrl+T");
    key(&mut h, CMD, Key::W);
    assert_eq!(s(&h).panes[0].tabs.len(), 1, "Ctrl+W");
    key(&mut h, CMD, Key::L);
    assert!(s(&h).panes[0].path_edit.is_some(), "Ctrl+L");
    key(&mut h, NONE, Key::Escape);
    h.run_steps(2);
    key(&mut h, CMD, Key::F);
    assert!(s(&h).tab(0).is_search(), "Ctrl+F");
    key(&mut h, CMD, Key::W);
    key(&mut h, CMD, Key::P);
    assert!(s(&h).jump.open, "Ctrl+P");
    shot(&mut h, "jump-to-folder");
    key(&mut h, NONE, Key::Escape);
    assert!(!s(&h).jump.open);
    key(&mut h, CMD_SHIFT, Key::P);
    assert!(s(&h).palette.open, "Ctrl+Shift+P");
    key(&mut h, NONE, Key::Escape);
    key(&mut h, NONE, Key::F3);
    assert!(s(&h).preview.open, "F3");
    key(&mut h, NONE, Key::F3);
    sm(&mut h).tab_mut(0).click("todo.txt", false, false);
    key(&mut h, NONE, Key::F2);
    assert!(s(&h).tab(0).renaming.is_some(), "F2");
    key(&mut h, NONE, Key::Escape);
    key(&mut h, CMD, Key::F2);
    assert!(
        matches!(s(&h).dialog, Some(crate::dialogs::Dialog::BulkRename(_))),
        "Ctrl+F2"
    );
    sm(&mut h).dialog = None;
    h.run_steps(2);
    // Ctrl+C / Ctrl+X arrive as clipboard events, Ctrl+V as a V release (egui-winit).
    h.input_mut().events.push(Event::Copy);
    h.run_steps(2);
    assert!(
        toast_texts(s(&h)).contains(&"Copied 1 item".to_owned()),
        "Ctrl+C"
    );
    h.input_mut().events.push(Event::Cut);
    h.run_steps(2);
    assert!(
        toast_texts(s(&h)).contains(&"Cut 1 item".to_owned()),
        "Ctrl+X"
    );
    h.input_mut().events.push(Event::Copy);
    h.run_steps(2);
    key(&mut h, NONE, Key::F6);
    h.input_mut().events.push(Event::Key {
        key: Key::V,
        physical_key: None,
        pressed: false,
        repeat: false,
        modifiers: CMD,
    });
    h.run_steps(2);
    assert!(
        wait(&mut h, 10, &|_| b.join("todo.txt").is_file()),
        "Ctrl+V"
    );
    key(&mut h, NONE, Key::F6);
    sm(&mut h).tab_mut(0).click("todo.txt", false, false);
    key(&mut h, NONE, Key::Delete);
    assert!(has(&h, "Move 1 item to the trash?"), "Del");
    click(&mut h, "Cancel");
    key(&mut h, CMD_SHIFT, Key::N);
    assert!(has(&h, "New folder name"), "Ctrl+Shift+N");
    click(&mut h, "Cancel");
    #[cfg(not(target_os = "macos"))]
    key(&mut h, CMD, Key::H);
    #[cfg(target_os = "macos")]
    key(&mut h, CMD_SHIFT, Key::Period);
    assert!(s(&h).show_hidden, "Ctrl+H");
    key(&mut h, CMD, Key::Comma);
    assert!(s(&h).settings_open, "Ctrl+,");
    shot(&mut h, "settings-general");
}

// ---------------------------------------------------------------------------------------
// Settings

fn checked(h: &H, label: &str) -> bool {
    let node = h.get_by_label(label);
    node.toggled() == Some(egui::accesskit::Toggled::True)
}

/// Every page of the Settings window renders its controls.
#[test]
fn settings_every_page_renders() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    key(&mut h, CMD, Key::Comma);
    for (page, shows) in [
        ("General", "Max preview size"),
        ("Remotes", "Add host…"),
        ("Cloud", "Add account…"),
        ("Profiles", "New"),
        ("Icons", "Install from VS Code Marketplace…"),
        ("Library", "Library (index sources, tags, duplicates)"),
        ("Devices", "Devices (pairing, shares, Spacedrop)"),
    ] {
        click_last(&mut h, page);
        assert!(has(&h, shows), "{page} shows {shows:?}");
        shot(&mut h, &format!("settings-{}", page.to_lowercase()));
    }
}

/// Each control of Settings → General (and the remote thumbnails switch) changes its
/// setting, the setting is written as the app writes it on exit, and a new window reads
/// it back.
#[test]
fn settings_controls_persist_after_reopening() {
    let _env = crate::settings::TEST_ENV.lock();
    let _restore = crate::settings::EnvRestore::save(&["KEEL_CONFIG_DIR"]);
    let cfg = std::env::temp_dir().join(format!("keel-walk-{}-settings", std::process::id()));
    let _ = std::fs::remove_dir_all(&cfg);
    std::env::set_var("KEEL_CONFIG_DIR", &cfg);
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    key(&mut h, CMD, Key::Comma);
    for label in [
        "Show",
        "Dual pane",
        "Preview panel",
        "Reduce motion",
        "One at a time per drive",
        "Reuse the running window",
    ] {
        let was = checked(&h, label);
        click(&mut h, label);
        assert_ne!(checked(&h, label), was, "{label} toggles");
    }
    // Theme: the combo box, then "light".
    let combo = h
        .query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::ComboBox))
        .next()
        .map(|n| center_of(&n))
        .expect("theme combo box");
    click_at(&mut h, combo, NONE, PointerButton::Primary);
    click(&mut h, "light");
    assert_eq!(s(&h).settings.theme, "light");
    assert!(!s(&h).theme.dark, "applied at once");
    // The size slider: a click near its left end.
    let max = s(&h).settings.max_preview_mb;
    let slider = (h.query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::Slider)))
        .find(|n| n.label().as_deref() == Some("Max preview size"))
        .expect("slider")
        .raw_bounds()
        .unwrap();
    let left = egui::pos2(
        slider.x0 as f32 + 8.0,
        ((slider.y0 + slider.y1) / 2.0) as f32,
    );
    click_at(&mut h, left, NONE, PointerButton::Primary);
    assert!(s(&h).settings.max_preview_mb < max, "slider moved");
    // The hotkey box: typed, then committed when it loses focus.
    let at = field(&h, "Global hotkey");
    fill(&mut h, at, "Ctrl+Shift+Alt+J");
    click(&mut h, "Transfers");
    assert_eq!(s(&h).settings.hotkey, "Ctrl+Shift+Alt+J");
    click_last(&mut h, "Remotes");
    click(&mut h, "Thumbnails for remote and cloud files");
    assert!(s(&h).settings.remote_thumbnails);
    shot(&mut h, "settings-changed");
    let changed = s(&h).settings.clone();
    assert_ne!(changed, crate::settings::Settings::default());
    // Closing the app writes them (Persist::finish), a new window reads them.
    let mut persist = crate::settings::Persist::new(Default::default(), None);
    persist.finish(&changed, None);
    drop(h);
    let (loaded, notice) = crate::settings::Settings::load();
    assert_eq!(notice, None);
    assert_eq!(loaded, changed, "written and read back");
    let mut h = app_with(
        boot(&root.join("Documents"), &root.join("Pictures"), loaded),
        SIZE,
    );
    assert!(!s(&h).dual && s(&h).show_hidden && s(&h).preview.open && !s(&h).theme.dark);
    key(&mut h, CMD, Key::Comma);
    for label in [
        "Show",
        "Preview panel",
        "Reduce motion",
        "One at a time per drive",
    ] {
        assert!(checked(&h, label), "{label} reopened checked");
    }
    assert!(!checked(&h, "Dual pane") && !checked(&h, "Reuse the running window"));
    shot(&mut h, "settings-reopened");
    drop(h);
    let _ = std::fs::remove_dir_all(&cfg);
}

/// Theme in the status bar switches dark / light; three screens in the light theme.
#[test]
fn theme_switch_and_light_screens() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    click(&mut h, "Theme: dark");
    assert!(!s(&h).theme.dark);
    assert_eq!(s(&h).settings.theme, "light");
    assert!(!h.ctx.style().visuals.dark_mode, "egui visuals follow");
    assert!(has(&h, "Theme: light"));
    sm(&mut h).panes[1].view = ViewMode::Grid;
    shot(&mut h, "light-main-window");
    key(&mut h, CMD_SHIFT, Key::P);
    type_text(&mut h, "pre");
    shot(&mut h, "light-command-palette");
    key(&mut h, NONE, Key::Escape);
    click_row(&mut h, "notes.md", 0, NONE);
    key(&mut h, NONE, Key::F3);
    assert!(wait(&mut h, 30, &previewed));
    shot(&mut h, "light-preview-markdown");
    key(&mut h, CMD, Key::Comma);
    shot(&mut h, "light-settings");
    click(&mut h, "Theme: light");
    assert!(s(&h).theme.dark && h.ctx.style().visuals.dark_mode);
}

/// Settings → Profiles: New makes a profile, Switch moves to it (its own settings), the
/// palette's "Switch profile: default" goes back.
#[test]
fn profile_switch() {
    let _env = crate::settings::TEST_ENV.lock();
    let _restore = crate::settings::EnvRestore::save(&["KEEL_CONFIG_DIR"]);
    let cfg = std::env::temp_dir().join(format!("keel-walk-{}-profiles", std::process::id()));
    let _ = std::fs::remove_dir_all(&cfg);
    std::env::set_var("KEEL_CONFIG_DIR", &cfg);
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    key(&mut h, CMD, Key::Comma);
    click_last(&mut h, "Profiles");
    let boxes: Vec<Pos2> = h
        .query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::TextInput))
        .map(|n| center_of(&n))
        .collect();
    fill(&mut h, boxes[0], "work");
    click(&mut h, "New");
    assert!(wait(&mut h, 10, &|s| s
        .profiles
        .names
        .contains(&"work".to_owned())));
    h.run_steps(3);
    shot(&mut h, "settings-profiles");
    // The current profile's Switch is disabled; "work" is the second row.
    click_last(&mut h, "Switch");
    assert!(wait(&mut h, 10, &|_| crate::profiles::current() == "work"));
    assert!(has(&h, "work  (current)"));
    // The palette offers the way back.
    key(&mut h, CMD, Key::Comma);
    key(&mut h, CMD_SHIFT, Key::P);
    type_text(&mut h, "switch profile");
    assert!(has(&h, "Switch profile: default"));
    key(&mut h, NONE, Key::Enter);
    assert!(wait(&mut h, 10, &|_| crate::profiles::current() == "default"));
    drop(h);
    crate::profiles::set_current("default");
    let _ = std::fs::remove_dir_all(&cfg);
}

/// A small VS Code icon theme package: coloured circles for files, folders and .md.
fn icon_package() -> Vec<u8> {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    let mut add = |name: &str, body: &str| {
        zip.start_file(name, opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    };
    add("extension.vsixmanifest", "<PackageManifest/>");
    add(
        "extension/package.json",
        r#"{"name":"walk","displayName":"Walk Icons","license":"MIT","contributes":
           {"iconThemes":[{"id":"walk","label":"Walk Icons","path":"./theme.json"}]}}"#,
    );
    add("extension/LICENSE.md", "MIT License\n\nA test package.");
    add(
        "extension/theme.json",
        r#"{"iconDefinitions":{
             "file":{"iconPath":"./icons/file.svg"},
             "folder":{"iconPath":"./icons/folder.svg"},
             "folder-open":{"iconPath":"./icons/folder-open.svg"},
             "md":{"iconPath":"./icons/md.svg"}},
           "file":"file","folder":"folder","folderExpanded":"folder-open",
           "fileExtensions":{"md":"md"}}"#,
    );
    for (name, colour) in [
        ("file", "#8a8f98"),
        ("folder", "#e5a50a"),
        ("folder-open", "#f6d32d"),
        ("md", "#3584e4"),
    ] {
        add(
            &format!("extension/icons/{name}.svg"),
            &format!(
                "<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16' \
                 viewBox='0 0 16 16'><circle cx='8' cy='8' r='7' fill='{colour}'/></svg>"
            ),
        );
    }
    zip.finish().unwrap().into_inner()
}

/// Settings → Icons: a package (a fixture .vsix) shows its license; I accept installs
/// and selects it; Built-in switches back; Remove deletes it.
#[test]
fn icon_theme_install_from_a_fixture_package() {
    let _env = crate::settings::TEST_ENV.lock();
    let _restore = crate::settings::EnvRestore::save(&["KEEL_CONFIG_DIR"]);
    let cfg = std::env::temp_dir().join(format!("keel-walk-{}-icons", std::process::id()));
    let _ = std::fs::remove_dir_all(&cfg);
    std::env::set_var("KEEL_CONFIG_DIR", &cfg);
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    key(&mut h, CMD, Key::Comma);
    click_last(&mut h, "Icons");
    let v = crate::icon_theme::inspect("walk.icons", icon_package()).unwrap();
    sm(&mut h).icon_themes.offer(v);
    h.run_steps(2);
    assert!(has(&h, "License: Walk Icons"));
    shot(&mut h, "icon-theme-license");
    click(&mut h, "I accept");
    assert!(wait(&mut h, 20, &|s| s.settings.icon_theme == "walk.icons"));
    // (The active theme itself is process-wide, shared with the tests running beside this
    // one: not asserted here; icon_theme's own tests cover loading it.)
    assert!(cfg
        .join("icons")
        .join("walk.icons")
        .join("theme.json")
        .is_file());
    assert!(wait(&mut h, 10, &|s| !s.icon_themes.installed.is_empty()));
    assert!(has(&h, "Walk Icons"));
    let toasts = toast_texts(s(&h));
    let installed = |t: &String| t.starts_with("Icon theme walk.icons installed");
    assert!(toasts.iter().any(installed), "{toasts:?}");
    assert!(
        !toasts.iter().any(|t| t.contains("built-in icons")),
        "{toasts:?}"
    );
    shot(&mut h, "icon-theme-installed");
    click(&mut h, "Built-in (Material Icon Theme subset)");
    assert_eq!(s(&h).settings.icon_theme, "default");
    click(&mut h, "Remove");
    assert!(wait(&mut h, 10, &|s| s.icon_themes.installed.is_empty()));
    assert!(!cfg.join("icons").join("walk.icons").exists());
    drop(h);
    let _ = std::fs::remove_dir_all(&cfg);
}

/// A color theme package: two themes, one including a base file, and a license.
fn color_package() -> Vec<u8> {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    let mut add = |name: &str, body: &str| {
        zip.start_file(name, opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    };
    add(
        "extension/package.json",
        r#"{"name":"walk","displayName":"Walk Colors","license":"MIT","contributes":{"themes":[
             {"label":"Walk Night","uiTheme":"vs-dark","path":"./themes/night.json"},
             {"label":"Walk Day","uiTheme":"vs","path":"./themes/day.json"}]}}"#,
    );
    add(
        "extension/LICENSE",
        "MIT License

A test package.",
    );
    add(
        "extension/themes/base.json",
        r##"{ "colors": { "focusBorder": "#3399ff", } }"##,
    );
    add(
        "extension/themes/night.json",
        r##"{ "include": "./base.json", // comment
             "colors": { "editor.background": "#14181f", "foreground": "#dde3ea" } }"##,
    );
    add(
        "extension/themes/day.json",
        r##"{ "colors": { "editor.background": "#fbfbf8", "foreground": "#1d1d1d" } }"##,
    );
    zip.finish().unwrap().into_inner()
}

/// Task 47: a VS Code color theme package through the license dialog (I accept), the new
/// theme applied, the picker switching to its second theme, then Remove.
#[test]
fn color_theme_install_switch_and_remove() {
    let _env = crate::settings::TEST_ENV.lock();
    let was = std::env::var_os("KEEL_CONFIG_DIR");
    let cfg = std::env::temp_dir().join(format!("keel-walk-{}-colors", std::process::id()));
    let _ = std::fs::remove_dir_all(&cfg);
    std::env::set_var("KEEL_CONFIG_DIR", &cfg);
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    key(&mut h, CMD, Key::Comma);
    let v = crate::color_theme::inspect("walk.colors", color_package()).unwrap();
    assert_eq!(v.themes.len(), 2);
    sm(&mut h).color_themes.offer(v);
    h.run_steps(2);
    assert!(has(&h, "License: Walk Colors"));
    click(&mut h, "I accept");
    assert!(wait(&mut h, 20, &|s| s.settings.theme == "walk.colors~0"));
    assert_eq!(s(&h).theme.name, "Walk Night");
    assert_eq!(s(&h).theme.bg, [0x14, 0x18, 0x1f]);
    assert_eq!(
        s(&h).theme.accent,
        [0x33, 0x99, 0xff],
        "from the included file"
    );
    for n in 0..2 {
        assert!(cfg
            .join("themes")
            .join(format!("walk.colors~{n}.toml"))
            .is_file());
    }
    shot(&mut h, "color-theme-installed");
    click_last(&mut h, "Theme");
    click_last(&mut h, "Walk Day");
    assert_eq!(s(&h).settings.theme, "walk.colors~1");
    assert!(!s(&h).theme.dark, "the light theme");
    click(&mut h, "Remove");
    assert!(wait(&mut h, 10, &|s| s.color_themes.installed.is_empty()));
    assert_eq!(s(&h).settings.theme, "dark", "back to dark");
    assert!(!cfg.join("themes").join("walk.colors~0.toml").exists());
    drop(h);
    match was {
        Some(v) => std::env::set_var("KEEL_CONFIG_DIR", v),
        None => std::env::remove_var("KEEL_CONFIG_DIR"),
    }
    let _ = std::fs::remove_dir_all(&cfg);
}

/// Settings → Remotes: Add host…, Edit…, Remove (twice, it asks); nothing connects.
#[test]
fn remotes_add_edit_remove() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    key(&mut h, CMD, Key::Comma);
    click_last(&mut h, "Remotes");
    assert!(has(&h, "No remote hosts yet."));
    click(&mut h, "Add host…");
    assert!(has(&h, "Add remote host"));
    // No ~/.ssh/config lookups from a test.
    if checked(&h, "Use ~/.ssh/config") {
        click(&mut h, "Use ~/.ssh/config");
    }
    let at = field(&h, "Label");
    fill(&mut h, at, "NAS");
    let at = field(&h, "Host");
    fill(&mut h, at, "nas.example.com");
    // The host's summary line appears below it a moment later (from a worker) and moves
    // the rows down: the next field is found after it is shown.
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !has_contains(&h, "~/.ssh/config is not used") {
        assert!(std::time::Instant::now() < end, "the host's summary line");
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.run_steps(2);
    let at = field(&h, "User");
    fill(&mut h, at, "demo");
    click(&mut h, "SSH agent");
    shot(&mut h, "remote-add");
    click(&mut h, "Save");
    assert_eq!(s(&h).settings.remotes.len(), 1);
    let host = s(&h).settings.remotes[0].clone();
    assert_eq!((host.label.as_str(), host.port), ("NAS", 22));
    assert!(has(&h, "SFTP demo@nas.example.com:22") && has(&h, "SSH agent"));
    // The host is in the sidebar's Remotes section, not connected.
    assert!(
        h.query_all_by_label("NAS").count() >= 2,
        "listed in the sidebar too"
    );
    click(&mut h, "Edit…");
    assert!(has(&h, "Edit remote host"));
    let at = field(&h, "Label");
    fill(&mut h, at, "Home NAS");
    click(&mut h, "Save");
    assert_eq!(s(&h).settings.remotes.len(), 1, "edited, not added");
    let edited = &s(&h).settings.remotes[0];
    assert_eq!((edited.label.as_str(), &edited.id), ("Home NAS", &host.id));
    assert!(has(&h, "Home NAS"));
    shot(&mut h, "remote-edited");
    click(&mut h, "Remove");
    assert!(has(&h, "Really remove?"));
    assert_eq!(s(&h).settings.remotes.len(), 1, "asks first");
    click(&mut h, "Really remove?");
    assert!(s(&h).settings.remotes.is_empty());
    assert!(has(&h, "No remote hosts yet."));
}

/// Settings → Remotes: an FTP host shows the clear-text warning where it is added, then
/// lists with its protocol; FTPS drops the warning. Nothing connects.
#[test]
fn ftp_remote_warns_about_clear_text() {
    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    key(&mut h, CMD, Key::Comma);
    click_last(&mut h, "Remotes");
    click(&mut h, "Add host…");
    assert!(!has(&h, keel_vfs::FTP_CLEAR_TEXT));
    click(&mut h, "FTP");
    h.run_steps(2);
    assert!(
        has(&h, keel_vfs::FTP_CLEAR_TEXT),
        "the warning next to the choice"
    );
    assert!(!has(&h, "Use ~/.ssh/config"), "no SSH settings for FTP");
    let at = field(&h, "Label");
    fill(&mut h, at, "Camera");
    let at = field(&h, "Host");
    fill(&mut h, at, "camera.example.com");
    // Anonymous: no password goes anywhere.
    click(&mut h, "Anonymously");
    shot(&mut h, "remote-add-ftp");
    click(&mut h, "Save");
    let host = s(&h).settings.remotes[0].clone();
    assert_eq!(
        (host.protocol, host.port, host.user.as_str()),
        (keel_vfs::RemoteProtocol::Ftp, 21, "anonymous")
    );
    assert!(has(&h, "FTP anonymous@camera.example.com:21") && has(&h, "Anonymous"));
    let row = &s(&h).sidebar.remotes[0];
    assert_eq!(
        row.warning,
        Some(keel_vfs::FTP_CLEAR_TEXT),
        "repeated on the sidebar"
    );
    assert_eq!(
        row.home,
        VPath::parse(&format!("ftp://{}/", host.id)).unwrap()
    );
    click(&mut h, "Edit…");
    click(&mut h, "FTPS");
    h.run_steps(2);
    assert!(!has(&h, keel_vfs::FTP_CLEAR_TEXT));
    click(&mut h, "Save");
    assert_eq!(
        s(&h).settings.remotes[0].protocol,
        keel_vfs::RemoteProtocol::Ftps
    );
    assert!(has(&h, "FTPS anonymous@camera.example.com:21"));
    assert_eq!(s(&h).sidebar.remotes[0].warning, None);
}

/// Live, against a real FTP server (`KEEL_FTP_TEST=user@host:port`, `KEEL_FTP_TEST_PASS`):
/// the host is added in Settings → Remotes like a user would, clicked in the sidebar, and
/// a folder made on the server is listed and opened in the pane. The password reaches the
/// provider through the in-memory store (`KEEL_NET_SECRET=memory`), never the keychain.
#[test]
#[ignore = "needs a live FTP server (KEEL_FTP_TEST)"]
fn live_ftp_remote_in_the_window() {
    let Ok(target) = std::env::var("KEEL_FTP_TEST") else {
        return;
    };
    let (user, address) = target.split_once('@').expect("user@host:port");
    let (server, port) = address.rsplit_once(':').expect("host:port");
    std::env::set_var("KEEL_NET_SECRET", "memory");
    let password = std::env::var("KEEL_FTP_TEST_PASS").unwrap_or_default();
    keel_vfs::sftp::auth::store_secret("live-ftp", "password", &password).unwrap();
    // A folder with a file on the server, made through the provider itself.
    let host = keel_vfs::RemoteHost {
        id: "live-ftp".into(),
        label: "Live FTP".into(),
        host: server.into(),
        port: port.parse().unwrap(),
        user: user.into(),
        auth: keel_vfs::RemoteAuth::PasswordInKeyring,
        home: None,
        bookmarks: Vec::new(),
        use_ssh_config: false,
        protocol: keel_vfs::RemoteProtocol::Ftp,
    };
    let provider = keel_vfs::FtpProvider::new(host, crossbeam_channel::unbounded().0);
    let folder = format!("keel-window-{}", std::process::id());
    let dir = VPath::parse(&format!("ftp://live-ftp/{folder}")).unwrap();
    use keel_vfs::Provider;
    provider.mkdir(&dir).unwrap();
    let mut w = provider.write(&dir.join("seen in the window.txt")).unwrap();
    std::io::Write::write_all(&mut w, b"hello").unwrap();
    std::io::Write::flush(&mut w).unwrap();
    drop(w);

    let root = demo();
    let mut h = app(&root.join("Documents"), &root.join("Pictures"));
    key(&mut h, CMD, Key::Comma);
    click_last(&mut h, "Remotes");
    click(&mut h, "Add host…");
    click(&mut h, "FTP");
    h.run_steps(2);
    assert!(has(&h, keel_vfs::FTP_CLEAR_TEXT));
    for (label, text) in [
        ("Label", "Live FTP"),
        ("Host", server),
        ("Port", port),
        ("User", user),
    ] {
        let at = field(&h, label);
        fill(&mut h, at, text);
    }
    let secret = h
        .query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::PasswordInput))
        .map(|n| center_of(&n))
        .next()
        .expect("the password field");
    fill(&mut h, secret, "typed, not stored by tests");
    click(&mut h, "Save");
    assert_eq!(s(&h).settings.remotes[0].id, "live-ftp");
    key(&mut h, NONE, Key::Escape);
    h.run_steps(3);
    // The sidebar row opens the host's root.
    click(&mut h, "Live FTP");
    let opened = |s: &AppState| {
        s.tab(0).dir.scheme == "ftp" && !s.tab(0).loading && names(s, 0).contains(&folder)
    };
    assert!(
        wait(&mut h, 30, &opened),
        "{:?} {:?}",
        s(&h).tab(0).error,
        names(s(&h), 0)
    );
    assert_eq!(
        s(&h).remotes.state("live-ftp"),
        Some(keel_vfs::ConnStatus::Connected)
    );
    sm(&mut h).run(0, Action::Navigate(dir.clone()));
    let inside = |s: &AppState| !s.tab(0).loading && names(s, 0) == ["seen in the window.txt"];
    assert!(wait(&mut h, 30, &inside), "{:?}", names(s(&h), 0));
    shot(&mut h, "remote-ftp-live");
    provider.remove(&dir).unwrap();
}

// ---------------------------------------------------------------------------------------
// Window sizes

fn inside(h: &H, label: &str, size: egui::Vec2) {
    let nodes: Vec<_> = h.query_all_by_label(label).collect();
    assert!(!nodes.is_empty(), "{label} shown at {size:?}");
    for n in nodes {
        let b = n.raw_bounds().unwrap();
        assert!(
            b.x0 >= -0.5
                && b.y0 >= -0.5
                && b.x1 <= size.x as f64 + 0.5
                && b.y1 <= size.y as f64 + 0.5,
            "{label} at {b:?} is outside {size:?}"
        );
    }
}

/// 800x600 and 2560x1440: the panes' toolbars, the status bar and the sidebar stay in the
/// window, and the left pane's view buttons stay left of the right pane.
#[test]
#[ignore]
fn window_sizes() {
    let root = demo();
    for (w, hgt) in [(800.0, 600.0), (2560.0, 1440.0)] {
        let size = egui::vec2(w, hgt);
        let mut h = app_with(
            boot(
                &root.join("Documents"),
                &root.join("Pictures"),
                Default::default(),
            ),
            size,
        );
        h.run_steps(4);
        for label in [
            "Details",
            "Grid",
            "Columns",
            "Media",
            "Back (Alt+Left)",
            "New tab (Ctrl+T)",
            "Theme: dark",
        ] {
            inside(&h, label, size);
        }
        inside(&h, "Pictures", size);
        let media: Vec<Pos2> = h
            .query_all_by_label("Media")
            .map(|n| center_of(&n))
            .collect();
        let back: Vec<Pos2> = h
            .query_all_by_label("Back (Alt+Left)")
            .map(|n| center_of(&n))
            .collect();
        assert_eq!((media.len(), back.len()), (2, 2));
        let (left_media, right_back) = (
            media.iter().map(|p| p.x).fold(f32::MAX, f32::min),
            back.iter().map(|p| p.x).fold(f32::MIN, f32::max),
        );
        assert!(left_media < right_back, "pane 0's buttons overlap pane 1");
        shot(&mut h, &format!("window-{}x{}", w as u32, hgt as u32));
    }
}

// ---------------------------------------------------------------------------------------
// Media view

/// Media: the Media button, Dates on (day headers), L tiles; Space opens the viewer, Esc
/// closes it.
#[test]
#[ignore]
fn media_view_with_dates() {
    let root = demo();
    let mut h = app(&root.join("Pictures"), &root.join("Documents"));
    key(&mut h, CMD_SHIFT, Key::D);
    click_nth(&mut h, "Media", 0);
    assert_eq!(s(&h).panes[0].view, ViewMode::Media);
    click_nth(&mut h, "Dates", 0);
    assert!(s(&h).media.dates, "Dates on");
    assert!(s(&h).settings.media_dates, "remembered as a setting");
    click_nth(&mut h, "L", 0);
    assert_eq!(s(&h).media.tile, crate::media::TileSize::L);
    let tiles = wait(&mut h, 90, &|s| s.media.len() >= 18);
    assert!(tiles, "{} tiles", s(&h).media.len());
    // Day headers: the fixture photos were modified on three days.
    shot(&mut h, "media-view-dates");
    sm(&mut h).tab_mut(0).click("IMG_2041.jpg", false, false);
    key(&mut h, NONE, Key::Space);
    assert!(s(&h).viewer.is_some(), "Space opens the viewer");
    // The full-size decode: a second or two.
    let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < end {
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    shot(&mut h, "media-viewer");
    // Dates on: newest first, so the oldest photo is the last and Left goes to the one
    // taken after it.
    key(&mut h, NONE, Key::ArrowLeft);
    key(&mut h, NONE, Key::Escape);
    assert!(s(&h).viewer.is_none(), "Esc closes it");
    let cursor = s(&h).tab(0).cursor.clone();
    assert_eq!(
        cursor.as_deref(),
        Some("IMG_2042.jpg"),
        "the grid follows the viewer"
    );
}

// ---------------------------------------------------------------------------------------
// The library

/// A window with the library open on a fresh data folder, and the fixture-only search
/// index (`screenshots::harness` does the same).
fn with_library(
    flow: &str,
    left: &Path,
    right: &Path,
) -> (H, PathBuf, std::sync::Arc<keel_core::Library>) {
    let base = std::env::temp_dir().join(format!("keel-walk-{}-{flow}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let index = base.join("index");
    let walk = keel_search::WalkIndexSearcher::open(index, vec![demo().to_owned()]).unwrap();
    let boot = Boot {
        searcher: Some(std::sync::Arc::new(walk)),
        ..boot(left, right, Default::default())
    };
    let mut h = app_with(boot, SIZE);
    let lib = keel_core::Library::open(&base.join("data"), "main").unwrap();
    let lib = std::sync::Arc::new(lib);
    lib.set_router(s(&h).router.clone());
    sm(&mut h).library.set_open(lib.clone());
    h.run_steps(3);
    (h, base, lib)
}

/// Sidebar → Add source…: the dialog starts on the active folder; Add and index.
fn add_source(h: &mut H, folder: &Path) {
    let n = s(h).library.sources.len();
    sm(h).run(0, Action::Navigate(VPath::local(folder)));
    assert!(wait(h, 10, &listed));
    click(h, "Add source…");
    assert!(has(h, "Add a library source"));
    click(h, "Add and index");
    let indexed = |s: &AppState| {
        s.library.sources.len() == n + 1
            && !s.library.jobs.values().any(|j| j.active())
            && s.library.stats.files > 0
    };
    assert!(wait(h, 120, &indexed), "indexed");
}

fn close_library(h: &mut H) {
    sm(h).library.close_now();
}

/// Overview after adding Documents: the counts, the source, the cards.
#[test]
#[ignore]
fn library_overview_after_adding_a_source() {
    let root = demo();
    let docs = root.join("Documents");
    let (mut h, _base, _lib) = with_library("overview", &docs, &root.join("Backup"));
    click(&mut h, "Add source…");
    assert!(has(&h, "Add a library source"));
    shot(&mut h, "library-add-source");
    click(&mut h, "Add and index");
    let indexed = |s: &AppState| {
        s.library.sources.len() == 1
            && s.library.jobs.values().any(|j| j.kind == "hash")
            && !s.library.jobs.values().any(|j| j.active())
            && s.library.stats.files > 0
    };
    assert!(wait(&mut h, 120, &indexed), "indexed and hashed");
    click(&mut h, "Overview");
    assert!(wait(&mut h, 30, &|s| s.library.dup_summary.is_some()));
    let files = s(&h).library.stats.files;
    assert!(files >= 13, "{files} files");
    assert!(has(&h, "Library overview"));
    assert!(
        has(&h, &crate::library::count(files)),
        "the Files indexed card"
    );
    assert!(has(&h, "1 source, 0 offline"), "{}", dump(&h, "source"));
    shot(&mut h, "library-overview");
    close_library(&mut h);
}

/// Ctrl+D marks a favorite (★ Favorites lists it); Ctrl+Shift+T creates and sets a tag
/// (the sidebar chip lists the file).
#[test]
#[ignore]
fn library_favorites_and_tags() {
    let root = demo();
    let (mut h, _base, _lib) = with_library("tags", &root.join("Documents"), &root.join("Backup"));
    add_source(&mut h, &root.join("Documents"));
    click_row(&mut h, "notes.md", 0, NONE);
    key(&mut h, CMD, Key::D);
    let notes = VPath::local(root.join("Documents").join("notes.md"));
    let tags = |s: &AppState| s.library.tagged.get(&notes).map_or(0, |t| t.len());
    assert!(wait(&mut h, 10, &|s| tags(s) == 1), "favorite");
    key(&mut h, CMD_SHIFT, Key::T);
    assert!(has(&h, "For 1 item"), "the tag picker");
    let at = h
        .query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::TextInput))
        .map(|n| center_of(&n))
        .next_back()
        .expect("New tag box");
    fill(&mut h, at, "garden");
    click_last(&mut h, "Create");
    let made = |s: &AppState| s.library.tags.iter().any(|t| t.name == "garden");
    assert!(wait(&mut h, 10, &made));
    assert!(wait(&mut h, 10, &|s| tags(s) == 2), "tagged");
    shot(&mut h, "library-tag-picker");
    sm(&mut h).library.picker = None;
    h.run_steps(3);
    // ★ Favorites lists notes.md.
    click(&mut h, "★ Favorites");
    let notes_md = "notes.md".to_owned();
    let listed_notes = |s: &AppState| !s.tab(0).loading && names(s, 0).contains(&notes_md);
    assert!(wait(&mut h, 20, &listed_notes));
    shot(&mut h, "library-favorites");
    // The tag chip in the sidebar lists it too.
    let chips: Vec<Pos2> = h
        .query_all_by_label("garden")
        .map(|n| center_of(&n))
        .collect();
    let chip = *chips
        .iter()
        .min_by(|a, b| a.x.total_cmp(&b.x))
        .expect("chip");
    click_at(&mut h, chip, NONE, PointerButton::Primary);
    let only_notes = |s: &AppState| !s.tab(0).loading && names(s, 0) == ["notes.md"];
    assert!(wait(&mut h, 20, &only_notes));
    shot(&mut h, "library-tag");
    close_library(&mut h);
}

/// Task 59: Ctrl+I shows the inspector; a file's Copies row opens the copy (its folder,
/// the copy selected); a folder shows its totals.
#[test]
#[ignore]
fn inspector_copies_and_folder_totals() {
    let dir = scratch("inspector");
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::copy(a.join("todo.txt"), b.join("todo copy.txt")).unwrap();
    let (mut h, _base, lib) = with_library("inspector-lib", &a, &b);
    add_source(&mut h, &a);
    add_source(&mut h, &b);
    // Both hashed: the library knows the copy.
    let todo = VPath::local(a.join("todo.txt"));
    let copy = VPath::local(b.join("todo copy.txt"));
    let known = |s: &AppState| {
        !s.library.jobs.values().any(|j| j.active())
            && lib.inspect(&todo, 50).unwrap().is_some_and(|i| {
                i.redundancy
                    .is_some_and(|r| r.locations.iter().any(|l| l.path == copy))
            })
    };
    assert!(wait(&mut h, 120, &known), "hashed");
    sm(&mut h).run(0, Action::Navigate(VPath::local(&a)));
    assert!(wait(&mut h, 10, &listed));
    click_row(&mut h, "todo.txt", 0, NONE);
    key(&mut h, CMD, Key::I);
    let inspector = crate::inspector::PanelTab::Inspector;
    assert!(s(&h).preview.open && s(&h).preview.tab == inspector);
    let answered =
        |s: &AppState| matches!(&s.preview.inspector.answer, Some(Ok(i)) if i.library.is_some());
    assert!(wait(&mut h, 30, &answered), "inspected");
    for label in ["Overview", "Library", "Copies", "History", "Tags", "Tags…"] {
        assert!(has(&h, label), "{label}");
    }
    assert!(
        has(&h, "2 copies in 1 failure domain"),
        "{}",
        dump(&h, "cop")
    );
    shot(&mut h, "inspector-file");
    click(&mut h, &copy.display());
    let opened = |s: &AppState| {
        dir_is(s, 0, &b) && !s.tab(0).loading && s.tab(0).cursor.as_deref() == Some("todo copy.txt")
    };
    assert!(
        wait(&mut h, 20, &opened),
        "the copy is selected in its folder"
    );
    // A folder: its totals.
    sm(&mut h).run(0, Action::Navigate(VPath::local(&dir)));
    assert!(wait(&mut h, 10, &listed));
    click_row(&mut h, "a", 0, NONE);
    let totals = |s: &AppState| match &s.preview.inspector.answer {
        Some(Ok(i)) => {
            i.entry.name == "a" && i.library.as_ref().is_some_and(|l| l.folder.is_some())
        }
        _ => false,
    };
    assert!(wait(&mut h, 30, &totals), "folder inspected");
    let lib_part = s(&h)
        .preview
        .inspector
        .answer
        .clone()
        .unwrap()
        .unwrap()
        .library
        .unwrap();
    let folder = lib_part.folder.unwrap();
    assert_eq!((folder.files, folder.folders), (Some(5), 0));
    assert!(has(&h, "Folders") && has(&h, "Biggest"));
    shot(&mut h, "inspector-folder");
    // Ctrl+I again hides the panel; F3 shows the preview tab.
    key(&mut h, CMD, Key::I);
    assert!(!s(&h).preview.open);
    key(&mut h, NONE, Key::F3);
    assert!(s(&h).preview.open && s(&h).preview.tab == crate::inspector::PanelTab::Preview);
    close_library(&mut h);
}

/// Task 56: right-click a folder → Folder color → Red: the dot shows after its name;
/// Folder icon → 📷: the row shows the emoji; Clear both. Outside the library the menu
/// says why it cannot style.
#[test]
#[ignore]
fn library_folder_color_and_icon() {
    let root = demo();
    let docs = root.join("Documents");
    let (mut h, _base, _lib) = with_library("styles", &docs, &root.join("Backup"));
    // Not in a source yet: the menu says what to do.
    sm(&mut h).run(0, Action::Navigate(VPath::local(&docs)));
    assert!(wait(&mut h, 10, &listed));
    right_click_row(&mut h, "Projects");
    click(&mut h, "Folder color");
    assert!(
        has(&h, crate::folder_style::NOT_IN_LIBRARY),
        "{}",
        dump(&h, "library")
    );
    key(&mut h, NONE, Key::Escape);
    key(&mut h, NONE, Key::Escape);
    add_source(&mut h, &docs);
    sm(&mut h).run(0, Action::Navigate(VPath::local(&docs)));
    assert!(wait(&mut h, 10, &listed));
    let projects = VPath::local(docs.join("Projects"));
    let style = |s: &AppState| s.library.style_of(&projects).cloned().unwrap_or_default();
    right_click_row(&mut h, "Projects");
    click(&mut h, "Folder color");
    click_last(&mut h, "Red");
    let red = |s: &AppState| style(s).color.as_deref() == Some("#e5484d");
    assert!(wait(&mut h, 10, &red), "colored");
    h.run_steps(3);
    assert!(
        has(&h, "Folder color: Red"),
        "the dot: {}",
        dump(&h, "Folder")
    );
    right_click_row(&mut h, "Projects");
    click(&mut h, "Folder icon");
    click_last(&mut h, "📷");
    let camera = |s: &AppState| style(s).emoji() == Some("📷") && style(s).color.is_some();
    assert!(wait(&mut h, 10, &camera), "the icon, the color kept");
    h.run_steps(3);
    assert!(
        has(&h, "Folder icon: 📷"),
        "the row's icon: {}",
        dump(&h, "Folder")
    );
    shot(&mut h, "library-folder-style");
    // Ctrl+P shows the style on the folder's result (the search index's folders).
    key(&mut h, CMD, Key::P);
    type_text(&mut h, "Projects");
    let found = |h: &H| has_contains(h, "Folder icon: 📷, Folder color: Red");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !found(&h) && std::time::Instant::now() < end {
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(found(&h), "{:?}", s(&h).jump.results);
    key(&mut h, NONE, Key::Escape);
    // Type an emoji…: one emoji only.
    right_click_row(&mut h, "Projects");
    click(&mut h, "Folder icon");
    click_last(&mut h, "Type an emoji…");
    assert!(matches!(
        s(&h).dialog,
        Some(crate::dialogs::Dialog::FolderIcon { .. })
    ));
    type_text(&mut h, "ab");
    click_last(&mut h, "Set icon");
    assert!(has(&h, "Type one emoji"));
    key(&mut h, NONE, Key::Escape);
    // Clear both.
    right_click_row(&mut h, "Projects");
    click(&mut h, "Folder color");
    click_last(&mut h, "Clear color");
    right_click_row(&mut h, "Projects");
    click(&mut h, "Folder icon");
    click_last(&mut h, "Clear icon");
    assert!(
        wait(&mut h, 10, &|s| s.library.style_of(&projects).is_none()),
        "cleared"
    );
    h.run_steps(3);
    assert!(!has_contains(&h, "Folder color:") && !has_contains(&h, "Folder icon:"));
    close_library(&mut h);
}

/// Task 58: Ctrl+Shift+A enters tag assign mode; 1 tags the two selected files with the
/// first tag (the bar's count follows), 1 again untags them, Esc leaves; another folder
/// leaves the mode too.
#[test]
#[ignore]
fn library_tag_assign_mode() {
    let root = demo();
    let docs = root.join("Documents");
    let (mut h, _base, lib) = with_library("assign", &docs, &root.join("Backup"));
    add_source(&mut h, &docs);
    for (name, color) in [("receipts", "#e5484d"), ("taxes", "#3b82f6")] {
        lib.create_tag(name, Some(color), None).unwrap();
    }
    sm(&mut h).library.refresh_meta();
    assert!(wait(&mut h, 10, &|s| s.library.tags.len() == 2), "tags");
    click_row(&mut h, "notes.md", 0, NONE);
    click_row(&mut h, "todo.txt", 0, CMD);
    key(&mut h, CMD_SHIFT, Key::A);
    assert!(s(&h).library.assign.is_some(), "assign mode");
    assert!(has(&h, "Assign tags to 2 items:"), "{}", dump(&h, "Assign"));
    assert!(has(&h, "1 receipts (0)"), "{}", dump(&h, "receipts"));
    assert!(has(&h, "2 taxes (0)"));
    assert!(has(&h, "0 or Esc leaves"));
    let both = [docs.join("notes.md"), docs.join("todo.txt")].map(VPath::local);
    let carry = |s: &AppState, on: bool| {
        let tag = s.library.tags[0].id;
        both.iter()
            .all(|p| on == s.library.tagged.get(p).is_some_and(|t| t.contains(&tag)))
    };
    // As a keyboard sends it: the key, then its text (which must not start the filter).
    for e in [
        Event::Key {
            key: Key::Num1,
            physical_key: Some(Key::Num1),
            pressed: true,
            repeat: false,
            modifiers: NONE,
        },
        Event::Text("1".into()),
        Event::Key {
            key: Key::Num1,
            physical_key: Some(Key::Num1),
            pressed: false,
            repeat: false,
            modifiers: NONE,
        },
    ] {
        h.input_mut().events.push(e);
    }
    h.run_steps(2);
    assert!(has(&h, "1 receipts (2)"), "{}", dump(&h, "receipts"));
    assert!(has(&h, "2 taxes (0)"));
    // What the library holds, read back (not the bar's own update).
    assert!(wait(&mut h, 10, &|s| !s
        .library
        .jobs
        .values()
        .any(|j| j.active())));
    sm(&mut h).library.tagged.clear();
    sm(&mut h).library.refresh_meta();
    assert!(
        wait(&mut h, 10, &|s| carry(s, true)),
        "tagged in the library"
    );
    assert!(
        s(&h).tab(0).filter.is_empty(),
        "the digit never reached the filter"
    );
    shot(&mut h, "library-tag-assign");
    key(&mut h, NONE, Key::Num1);
    assert!(has(&h, "1 receipts (0)"));
    sm(&mut h).library.tagged.clear();
    sm(&mut h).library.refresh_meta();
    h.run_steps(3);
    assert!(
        wait(&mut h, 10, &|s| carry(s, false)),
        "untagged in the library"
    );
    key(&mut h, NONE, Key::Escape);
    assert!(s(&h).library.assign.is_none(), "Esc leaves");
    assert!(!has(&h, "0 or Esc leaves"));
    // Another folder ends the mode.
    key(&mut h, CMD_SHIFT, Key::A);
    assert!(s(&h).library.assign.is_some());
    sm(&mut h).run(0, Action::Navigate(VPath::local(root.join("Backup"))));
    assert!(wait(&mut h, 10, &listed));
    assert!(s(&h).library.assign.is_none(), "navigation leaves");
    close_library(&mut h);
}

/// Ctrl+F, the backend menu → Library, then `ext:`, `in:` and `tag:` queries.
#[test]
#[ignore]
fn search_tab_with_library_filters() {
    let root = demo();
    let (mut h, _base, _lib) =
        with_library("search", &root.join("Documents"), &root.join("Backup"));
    add_source(&mut h, &root.join("Documents"));
    add_source(&mut h, &root.join("Backup"));
    let create = crate::library::LibCmd::CreateTag {
        name: "taxes".into(),
        color: "#e5484d".into(),
    };
    sm(&mut h).run(0, Action::Library(create));
    // System search first (the fixture's own index).
    key(&mut h, CMD, Key::F);
    assert!(s(&h).tab(0).is_search());
    type_text(&mut h, "budget");
    let two = |s: &AppState| !s.tab(0).loading && s.tab(0).entries().len() == 2;
    assert!(wait(&mut h, 30, &two), "{:?}", names(s(&h), 0));
    shot(&mut h, "search-system");
    // The backend menu: Library.
    let combo = h
        .query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::ComboBox))
        .map(|n| center_of(&n))
        .min_by(|a, b| a.x.total_cmp(&b.x))
        .expect("backend menu");
    click_at(&mut h, combo, NONE, PointerButton::Primary);
    click_last(&mut h, "Library");
    assert!(s(&h).tab(0).library_search);
    let query = |h: &mut H, q: &str| {
        let at = search_box(h);
        fill(h, at, q);
        key(h, NONE, Key::Enter);
        assert!(wait(h, 30, &|s| !s.tab(0).loading), "{q}");
        h.run_steps(5);
        let mut found = names(s(h), 0);
        found.sort();
        found
    };
    let pdfs = query(&mut h, "ext:pdf");
    let want = ["2026-q1.pdf", "2026-q2.pdf", "report.pdf", "report.pdf"];
    assert_eq!(pdfs, want);
    shot(&mut h, "search-library-ext");
    let backup = query(&mut h, "ext:md in:Backup");
    assert_eq!(backup, ["notes-2025.md"]);
    // Tag notes.md "taxes", then tag:taxes finds it. The tag was created on a worker at the
    // start; under machine load its list refresh can land after the searches above.
    let notes = VPath::local(root.join("Documents").join("notes.md"));
    assert!(
        wait(&mut h, 10, &|s| s
            .library
            .tags
            .iter()
            .any(|t| t.name == "taxes")),
        "tag made"
    );
    let tag = s(&h)
        .library
        .tags
        .iter()
        .find(|t| t.name == "taxes")
        .map(|t| t.id);
    let tag = tag.expect("tag made");
    sm(&mut h).run(1, Action::Navigate(VPath::local(root.join("Documents"))));
    assert!(wait(&mut h, 10, &listed));
    sm(&mut h).tab_mut(1).click("notes.md", false, false);
    sm(&mut h).run(1, Action::Library(crate::library::LibCmd::TagPicker));
    let set = crate::library::LibCmd::SetTag { tag, on: true };
    sm(&mut h).run(1, Action::Library(set));
    sm(&mut h).library.picker = None;
    assert!(wait(&mut h, 10, &|s| s.library.tagged.contains_key(&notes)));
    assert_eq!(query(&mut h, "tag:taxes"), ["notes.md"]);
    shot(&mut h, "search-library-tag");
    close_library(&mut h);
}

/// The search tab's query box: the topmost text box (the chip row's extension box is
/// below it).
fn search_box(h: &H) -> Pos2 {
    h.query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::TextInput))
        .map(|n| center_of(&n))
        .filter(|p| p.x > 150.0)
        .min_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)))
        .expect("search box")
}

fn search_text(s: &AppState) -> String {
    match &s.tab(0).kind {
        crate::tab::TabKind::Search { query, .. } => query.clone(),
        _ => String::new(),
    }
}

/// Task 57: the filter chips build a library query without the syntax. Kind → Document
/// and Tag → taxes write `kind:document tag:taxes` and the results follow; a term typed by
/// hand lights its chip; Tab from the query box reaches the chips and Space toggles one.
#[test]
#[ignore]
fn search_chips_build_the_query() {
    let root = demo();
    let docs = root.join("Documents");
    let (mut h, _base, _lib) = with_library("chips", &docs, &root.join("Backup"));
    add_source(&mut h, &docs);
    let create = crate::library::LibCmd::CreateTag {
        name: "taxes".into(),
        color: "#e5484d".into(),
    };
    sm(&mut h).run(0, Action::Library(create));
    let made = |s: &AppState| s.library.tags.iter().any(|t| t.name == "taxes");
    assert!(wait(&mut h, 10, &made), "tag made");
    let tag = (s(&h).library.tags.iter())
        .find(|t| t.name == "taxes")
        .map(|t| t.id)
        .unwrap();
    sm(&mut h).run(1, Action::Navigate(VPath::local(&docs)));
    assert!(wait(&mut h, 10, &listed));
    sm(&mut h).tab_mut(1).click("notes.md", false, false);
    sm(&mut h).run(1, Action::Library(crate::library::LibCmd::TagPicker));
    sm(&mut h).run(
        1,
        Action::Library(crate::library::LibCmd::SetTag { tag, on: true }),
    );
    sm(&mut h).library.picker = None;
    let notes = VPath::local(docs.join("notes.md"));
    assert!(wait(&mut h, 10, &|s| s.library.tagged.contains_key(&notes)));
    // Ctrl+F, then Library: the chip row shows under the box (not for the system search).
    key(&mut h, CMD, Key::F);
    assert!(s(&h).tab(0).is_search());
    assert!(!has(&h, "Kind: Document"), "no chips for the system search");
    let combo = h
        .query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::ComboBox))
        .map(|n| center_of(&n))
        .min_by(|a, b| a.x.total_cmp(&b.x))
        .expect("backend menu");
    click_at(&mut h, combo, NONE, PointerButton::Primary);
    click_last(&mut h, "Library");
    assert!(s(&h).tab(0).library_search);
    h.run_steps(3);
    for chip in [
        "Kind: Document",
        "Size: Under 1 MB",
        "Modified: This week",
        "Only favorites",
    ] {
        assert!(has(&h, chip), "{chip}");
    }
    assert!(has(&h, "Type: another extension"));
    // The Type chips come from the index's most common extensions.
    assert!(wait(&mut h, 10, &|s| s
        .library
        .top_ext
        .first()
        .is_some_and(|e| e == "pdf")));
    h.run_steps(3);
    assert!(has(&h, "Type: pdf"), "{:?}", s(&h).library.top_ext);
    let settled = |h: &mut H| {
        assert!(wait(h, 30, &|s| !s.tab(0).loading && s.tab(0).error.is_none()));
        h.run_steps(5);
        let mut found = names(s(h), 0);
        found.sort();
        found
    };
    click(&mut h, "Kind: Document");
    assert_eq!(search_text(s(&h)), "kind:document");
    let documents = [
        "2026-q1.pdf",
        "2026-q2.pdf",
        "budget.csv",
        "letter.docx",
        "materials.csv",
        "notes.md",
        "plan.md",
        "plants.xlsx",
        "report.pdf",
        "todo.txt",
    ];
    assert_eq!(settled(&mut h), documents);
    assert!(checked(&h, "Kind: Document"));
    click(&mut h, "Tag: taxes");
    assert_eq!(search_text(s(&h)), "kind:document tag:taxes");
    assert_eq!(settled(&mut h), ["notes.md"]);
    assert!(checked(&h, "Tag: taxes") && checked(&h, "Kind: Document"));
    shot(&mut h, "search-chips");
    // Clicking a lit chip takes its term out again.
    click(&mut h, "Kind: Document");
    assert_eq!(search_text(s(&h)), "tag:taxes");
    assert!(!checked(&h, "Kind: Document"));
    // Typed by hand: the chips light from the text.
    let at = search_box(&h);
    fill(&mut h, at, "TAG:taxes size:<1MB ext:.MD");
    key(&mut h, NONE, Key::Enter);
    assert_eq!(settled(&mut h), ["notes.md"]);
    for chip in ["Tag: taxes", "Size: Under 1 MB", "Type: md"] {
        assert!(checked(&h, chip), "{chip}");
    }
    assert!(!checked(&h, "Kind: Document") && !checked(&h, "Type: pdf"));
    // The keyboard: Tab from the query box reaches the first chip, Space toggles it.
    click_at(&mut h, at, NONE, PointerButton::Primary);
    key(&mut h, NONE, Key::Tab);
    key(&mut h, NONE, Key::Space);
    assert_eq!(search_text(s(&h)), "TAG:taxes size:<1MB ext:.MD kind:image");
    assert!(settled(&mut h).is_empty());
    assert!(checked(&h, "Kind: Image"));
    close_library(&mut h);
}

/// Task 62: text inside documents. Documents is indexed, the text job reads its documents;
/// a word only inside todo.txt finds it with a snippet line under the name; the Contents
/// chip off (`contents:off`) hides it again.
#[test]
#[ignore]
fn search_finds_words_inside_documents() {
    let root = demo();
    let docs = root.join("Documents");
    let (mut h, _base, lib) = with_library("contents", &docs, &root.join("Backup"));
    lib.set_pause_on_battery(false);
    lib.set_text_extractor(keel_preview::text_extractor());
    add_source(&mut h, &docs);
    // The walk started the text job: wait for its end in the library itself (the window's
    // job list can lag), and for Overview's stats to count the documents read.
    let text_ended = || {
        let list = lib.jobs().list().unwrap_or_default();
        let mut text = list.iter().filter(|j| j.kind == "text").peekable();
        text.peek().is_some()
            && text.all(|j| {
                matches!(
                    j.status,
                    keel_core::JobStatus::Done
                        | keel_core::JobStatus::Failed
                        | keel_core::JobStatus::Cancelled
                )
            })
    };
    let read = |s: &AppState| s.library.stats.text.documents >= 5 && text_ended();
    let jobs = || {
        let list = lib.jobs().list().unwrap_or_default();
        let list: Vec<_> = list.iter().map(|j| (&j.kind, j.status, &j.log)).collect();
        format!("{list:?}")
    };
    assert!(
        wait(&mut h, 120, &read),
        "{:?} {}",
        s(&h).library.stats.text,
        jobs()
    );
    key(&mut h, CMD, Key::F);
    let combo = h
        .query_all(egui_kittest::kittest::by().role(egui::accesskit::Role::ComboBox))
        .map(|n| center_of(&n))
        .min_by(|a, b| a.x.total_cmp(&b.x))
        .expect("backend menu");
    click_at(&mut h, combo, NONE, PointerButton::Primary);
    click_last(&mut h, "Library");
    assert!(s(&h).tab(0).library_search);
    h.run_steps(3);
    assert!(checked(&h, "Search file contents"), "on by default");
    let settled = |h: &mut H| {
        assert!(wait(h, 30, &|s| !s.tab(0).loading && s.tab(0).error.is_none()));
        h.run_steps(5);
        names(s(h), 0)
    };
    // "plumber" is only inside todo.txt.
    let at = search_box(&h);
    fill(&mut h, at, "plumber");
    key(&mut h, NONE, Key::Enter);
    assert_eq!(settled(&mut h), ["todo.txt"]);
    let snippet = s(&h)
        .tab(0)
        .snippets
        .values()
        .next()
        .cloned()
        .expect("a snippet");
    assert!(snippet.text.contains("Call the plumber"), "{snippet:?}");
    assert!(has(&h, &snippet.text), "the snippet line under the name");
    shot(&mut h, "search-contents");
    // The Contents chip off: names only.
    click(&mut h, "Search file contents");
    assert_eq!(search_text(s(&h)), "plumber contents:off");
    assert!(settled(&mut h).is_empty());
    assert!(!checked(&h, "Search file contents"));
    // On again; `content:` finds the word inside notes.md (a Markdown list item).
    click(&mut h, "Search file contents");
    assert_eq!(search_text(s(&h)), "plumber");
    let at = search_box(&h);
    fill(&mut h, at, "content:drip");
    key(&mut h, NONE, Key::Enter);
    assert_eq!(settled(&mut h), ["notes.md"]);
    close_library(&mut h);
}

// ---------------------------------------------------------------------------------------
// Devices

/// Sidebar → Devices → Pair… → Show code: a short code and the QR code, on an offline
/// node (loopback only, the identity in memory).
#[test]
#[ignore]
fn devices_pair_dialog_shows_a_code() {
    let _env = crate::settings::TEST_ENV.lock();
    let _restore =
        crate::settings::EnvRestore::save(&["KEEL_CONFIG_DIR", "KEEL_DATA_DIR", "KEEL_NET_SECRET"]);
    let root = demo();
    let (mut h, base, lib) = with_library("devices", &root.join("Documents"), &root.join("Backup"));
    std::env::set_var("KEEL_CONFIG_DIR", base.join("config"));
    std::env::set_var("KEEL_DATA_DIR", base.join("data"));
    std::env::set_var("KEEL_NET_SECRET", "memory");
    let rt = std::sync::Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    );
    let handler = std::sync::Arc::new(keel_net::LibraryHandler::new(lib.clone()));
    let node = rt
        .block_on(keel_net::Node::open_with_options(
            std::sync::Arc::new(keel_vfs::cloud::MemoryStore::default()),
            &base.join("data"),
            handler.clone(),
            keel_net::NodeOptions::offline(),
        ))
        .unwrap();
    node.set_label("Desktop");
    {
        let st = sm(&mut h);
        st.settings.devices.enabled = true;
        st.devices.set_open(lib, node, handler, rt.clone());
    }
    assert!(wait(&mut h, 30, &|s| s.sidebar.devices.is_some()));
    click(&mut h, "Pair…");
    assert!(has(&h, "Pair a device"));
    click(&mut h, "Show code");
    let showing =
        |s: &AppState| matches!(s.devices.pair, Some(crate::devices::Pair::Showing { .. }));
    assert!(wait(&mut h, 30, &showing), "a code");
    let short = match &s(&h).devices.pair {
        Some(crate::devices::Pair::Showing { short, .. }) => short.clone(),
        _ => unreachable!(),
    };
    assert!(short.len() >= 6, "{short}");
    assert!(has(&h, &short), "the short code is shown");
    shot(&mut h, "devices-pair-code");
    sm(&mut h).devices.pair = None;
    sm(&mut h).devices.close_now();
    close_library(&mut h);
}

// ---------------------------------------------------------------------------------------
// The Recycle Bin

/// Dispatches this thread's pending window messages, as the app's event loop does.
#[cfg(windows)]
fn pump_messages() {
    #[repr(C)]
    struct Msg {
        hwnd: isize,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        pt: [i32; 2],
        private: u32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn PeekMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32, remove: u32) -> i32;
        fn TranslateMessage(msg: *const Msg) -> i32;
        fn DispatchMessageW(msg: *const Msg) -> isize;
    }
    const PM_REMOVE: u32 = 1;
    let mut msg = std::mem::MaybeUninit::<Msg>::zeroed();
    // SAFETY: plain Win32 calls on this thread's queue with a valid MSG buffer.
    unsafe {
        while PeekMessageW(msg.as_mut_ptr(), 0, 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(msg.as_ptr());
            DispatchMessageW(msg.as_ptr());
        }
    }
}

/// Del moves a file to the Recycle Bin; the Recycle Bin tab (filtered to this test's own
/// file: the user's other items are never touched or shown) restores it; deleted again,
/// Delete permanently removes it for good.
#[cfg(windows)]
#[test]
#[ignore]
fn recycle_bin_restore_and_purge() {
    let dir = scratch("trash");
    let a = dir.join("a");
    let name = format!("keel-walkthrough-{}.txt", std::process::id());
    std::fs::write(a.join(&name), "trash me").unwrap();
    // No GPU device until the one shot at the end: with one, the shell's listing of the
    // Recycle Bin can wait forever on a thread that pumps no messages.
    let mut h = build(boot(&a, &dir.join("b"), Default::default()), SIZE, false);
    let to_trash = |h: &mut H| {
        sm(h).run(0, Action::Navigate(VPath::local(&a)));
        assert!(wait(h, 10, &listed));
        sm(h).tab_mut(0).click(&name, false, false);
        key(h, NONE, Key::Delete);
        assert!(has(h, "Move 1 item to the trash?"));
        click(h, "Move to trash");
        assert!(wait(h, 30, &job_done));
        assert!(!a.join(&name).exists(), "in the Recycle Bin");
        sm(h).jobs.list.clear();
    };
    // False when the Recycle Bin did not answer (see docs/qa/2026-10-10-walkthrough.md:
    // after a delete in the same process the shell's listing sometimes stalls here).
    let open_bin = |h: &mut H| -> bool {
        sm(h).run(0, Action::Navigate(keel_vfs::trashbin::root()));
        let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !listed(s(h)) && std::time::Instant::now() < end {
            pump_messages();
            h.step();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !listed(s(h)) {
            return false;
        }
        // Only this test's file in view.
        type_text(h, &name);
        assert!(wait(h, 10, &|s| names(s, 0) == [name.clone()]));
        sm(h).tab_mut(0).click(&name, false, false);
        h.run_steps(2);
        true
    };
    // The file back where it was, when the listing stalled.
    let skip = |what: &str| {
        use keel_vfs::Provider;
        let bin = keel_vfs::TrashProvider;
        let mine: Vec<VPath> = (bin.list(&keel_vfs::trashbin::root()).unwrap().into_iter())
            .filter(|e| e.name == name)
            .map(|e| e.path)
            .collect();
        bin.restore_paths(&mine).unwrap();
        println!("recycle-bin {what} skipped: the Recycle Bin did not answer in 60 s");
    };
    to_trash(&mut h);
    if !open_bin(&mut h) {
        return skip("restore");
    }
    assert!(has(&h, "Original location") && has(&h, "Deleted on"));
    right_click_row(&mut h, &name);
    assert!(has(&h, "Restore") && has(&h, "Delete permanently"));
    click(&mut h, "Restore");
    assert!(wait(&mut h, 30, &|_| a.join(&name).is_file()), "restored");
    assert_eq!(read(&a.join(&name)), "trash me");
    sm(&mut h).jobs.list.clear();
    to_trash(&mut h);
    if !open_bin(&mut h) {
        return skip("purge");
    }
    // The filter box has the keys: a click on the row gives them back to the list.
    click_row(&mut h, &name, 0, NONE);
    key(&mut h, NONE, Key::Delete);
    let asked = "Permanently delete 1 item from the Recycle Bin?";
    assert!(has_contains(&h, asked));
    shot(&mut h, "recycle-bin");
    click(&mut h, "Delete permanently");
    assert!(wait(&mut h, 30, &job_done));
    let done = s(&h).jobs.list[0].done.as_ref().map(|r| r.is_ok());
    assert_eq!(done, Some(true), "purged");
    assert!(!a.join(&name).exists());
}

/// The live check of undo, in the harness: rename, move between panes, copy, new folder
/// and delete, then Ctrl+Z five times. Each comes back, and the Recycle Bin / Trash no
/// longer holds the deleted file (the copy and the new folder go there; they are purged
/// at the end). Ignored: it uses the real Recycle Bin.
#[test]
#[ignore]
fn undo_five_operations() {
    let dir = scratch("undo5");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let doomed = format!("keel-trash-test-walk-{}.txt", std::process::id());
    std::fs::write(a.join(&doomed), "bring me back").unwrap();
    let before = (tree_of(&a), tree_of(&b));
    let mut h = build(boot(&a, &b, Default::default()), SIZE, false);
    let settle = |h: &mut H| {
        assert!(wait(h, 30, &job_done));
        sm(h).jobs.list.clear();
        sm(h).toasts.list.clear();
        sm(h).active = 0;
        h.run_steps(3);
    };
    // 1. F2 rename.
    click_row(&mut h, "notes.md", 0, NONE);
    key(&mut h, NONE, Key::F2);
    type_text(&mut h, "renamed");
    key(&mut h, NONE, Key::Enter);
    assert!(wait(&mut h, 10, &|_| a.join("renamed.md").exists()));
    assert!(wait(&mut h, 10, &|s| s.undo.label().is_some()));
    // 2. Move to the other pane, 3. copy there.
    for (name, mv) in [("todo.txt", true), ("garden.rs", false)] {
        let paths = vec![VPath::local(a.join(name))];
        h.input_mut().modifiers = if mv { Modifiers::SHIFT } else { NONE };
        h.step();
        let (from, dst) = (Some((0, VPath::local(&a))), VPath::local(&b));
        sm(&mut h).run(1, Action::Drop { paths, from, dst });
        h.input_mut().modifiers = NONE;
        settle(&mut h);
        assert!(b.join(name).exists() && a.join(name).exists() != mv);
    }
    // 4. New folder, 5. delete.
    key(&mut h, CMD_SHIFT, Key::N);
    key(&mut h, NONE, Key::Enter);
    assert!(wait(&mut h, 10, &|_| a.join("New folder").is_dir()));
    assert!(wait(&mut h, 10, &|s| names(s, 0).contains(&doomed)));
    sm(&mut h).tab_mut(0).click(&doomed, false, false);
    key(&mut h, NONE, Key::Delete);
    click(&mut h, "Move to trash");
    settle(&mut h);
    assert!(!a.join(&doomed).exists());
    assert_eq!(s(&h).undo.label().as_deref(), Some("Undo delete"));
    // Ctrl+Z five times: each preview confirmed, the rename at once.
    let since = std::time::SystemTime::now();
    for what in ["delete", "new folder", "copy", "move", "rename"] {
        assert_eq!(s(&h).undo.label(), Some(format!("Undo {what}")));
        key(&mut h, CMD, Key::Z);
        if what != "rename" {
            let asked = |s: &AppState| {
                matches!(&s.dialog, Some(crate::dialogs::Dialog::Confirm { text, .. })
                    if text.starts_with(&format!("Undo {what}: ")))
            };
            assert!(wait(&mut h, 30, &asked), "{what}: {:?}", toast_texts(s(&h)));
            click_last(&mut h, "Undo");
        }
        settle(&mut h);
        let done = s(&h).jobs.list.is_empty();
        assert!(done, "{what}");
    }
    assert_eq!((tree_of(&a), tree_of(&b)), before, "everything is back");
    assert_eq!(read(&a.join(&doomed)), "bring me back");
    // The deleted file left the Recycle Bin; the copy and the new folder went there.
    let bin = |p: &Path| keel_vfs::trashbin::find_trashed(&[p.to_path_buf()], since).unwrap();
    assert_eq!(bin(&a.join(&doomed)), [None]);
    // (macOS: the Trash does not say where its items came from.)
    for p in [b.join("garden.rs"), a.join("New folder")]
        .iter()
        .filter(|_| keel_vfs::trashbin::KNOWS_ORIGIN)
    {
        let found = bin(p);
        let [Some(item)] = found.as_slice() else {
            panic!("{} is not in the bin", p.display());
        };
        keel_vfs::TrashProvider
            .purge_paths(std::slice::from_ref(item))
            .unwrap();
    }
}

/// Names and bytes under `dir`, sorted.
fn tree_of(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .map(|p| {
            let bytes = if p.is_file() {
                std::fs::read(&p).unwrap()
            } else {
                Vec::new()
            };
            (p.file_name().unwrap().to_string_lossy().into_owned(), bytes)
        })
        .collect();
    out.sort();
    out
}

/// An operation still running when the profile is switched reports into the old journal's
/// generation: it is dropped, and the new profile's own operations are recorded.
#[test]
fn undo_reports_from_before_a_profile_switch_are_dropped() {
    use crate::undo::{Item, Op};
    let dir = scratch("undogen");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let mut h = build(boot(&a, &b, Default::default()), SIZE, false);
    let old = s(&h).undo.generation();
    // What the profile switch does to the journal and the jobs.
    sm(&mut h).undo.clear();
    sm(&mut h).jobs.undo_generation = s(&h).undo.generation();
    let made = || {
        let item = Item {
            now: VPath::local(b.join("notes.md")),
            was: None,
            stamp: None,
        };
        Ok((Op::Copy, vec![item]))
    };
    sm(&mut h).apply(crate::state::Msg::Undoable(old, made()));
    sm(&mut h).apply(crate::state::Msg::Undoable(old, Err("refused".into())));
    assert_eq!(s(&h).undo.label(), None, "the old profile's operation");
    let now = s(&h).undo.generation();
    sm(&mut h).apply(crate::state::Msg::Undoable(now, made()));
    assert_eq!(s(&h).undo.label().as_deref(), Some("Undo copy"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Ctrl+P finds files: part of a file's name, Enter, and the file is selected in its folder
/// with the preview open. Tab switches to Folders (kept for the next Ctrl+P), and the file
/// leaves the results.
#[test]
fn ctrl_p_finds_a_file_selects_and_previews_it() {
    use crate::jump::JumpFilter;
    let dir = scratch("jump-file");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let deep = a.join("sub").join("deep");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("quarterly-ledger.txt"), "ledger\n").unwrap();
    let mut h = app(&a, &b);
    let first_file = |s: &AppState| {
        (s.jump.results.first()).is_some_and(|f| f.file && f.path.ends_with("quarterly-ledger.txt"))
    };
    key(&mut h, CMD, Key::P);
    assert!(s(&h).jump.open, "Ctrl+P");
    assert_eq!(
        s(&h).jump.filter,
        JumpFilter::Both,
        "folders and files by default"
    );
    type_text(&mut h, "qledger");
    assert!(wait(&mut h, 20, &first_file), "{:?}", s(&h).jump.results);
    assert!(!s(&h).jump.tree.busy, "{}", s(&h).jump.status());
    shot(&mut h, "find-a-file");
    key(&mut h, NONE, Key::Tab);
    assert_eq!(s(&h).jump.filter, JumpFilter::Folders, "Tab");
    assert!(
        wait(&mut h, 10, &|s| s.jump.results.iter().all(|f| !f.file)),
        "folders only: {:?}",
        s(&h).jump.results
    );
    key(&mut h, NONE, Key::Escape);
    assert!(!s(&h).jump.open);
    assert_eq!(
        s(&h).settings.jump_filter,
        JumpFilter::Folders,
        "remembered"
    );
    key(&mut h, CMD, Key::P);
    assert_eq!(s(&h).jump.filter, JumpFilter::Folders, "kept");
    key(&mut h, NONE, Key::Tab);
    key(&mut h, NONE, Key::Tab);
    assert_eq!(s(&h).jump.filter, JumpFilter::Both);
    type_text(&mut h, "qledger");
    assert!(wait(&mut h, 10, &first_file), "{:?}", s(&h).jump.results);
    key(&mut h, NONE, Key::Enter);
    assert!(!s(&h).jump.open, "Enter closes it");
    let shown = |s: &AppState| {
        dir_is(s, 0, &deep) && s.tab(0).cursor.as_deref() == Some("quarterly-ledger.txt")
    };
    assert!(wait(&mut h, 10, &shown), "{}", s(&h).tab(0).dir.display());
    assert!(s(&h).tab(0).selected.contains("quarterly-ledger.txt"));
    assert!(s(&h).preview.open, "the preview opens");
    assert!(wait(&mut h, 10, &previewed), "and shows the file");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------
// WSL (Task 51)

/// Stands in for `wsl::open`: "Ubuntu" is stopped (it takes a moment to start) and then
/// opens on the fixture's Documents, as a started distribution's root would; "Broken"
/// fails as an unreachable share does.
fn fake_wsl_open(name: &str) -> anyhow::Result<VPath> {
    match name {
        "Ubuntu" => {
            std::thread::sleep(std::time::Duration::from_millis(600));
            Ok(VPath::local(demo().join("Documents")))
        }
        _ => anyhow::bail!("The network name cannot be found. (os error 67)"),
    }
}

/// The sidebar's WSL section with a fake distribution list (default first): a click shows
/// "starting…" while the distribution starts, then opens it in the active pane through the
/// navigation path (the provider call is stubbed); a failure is an error toast; without
/// distributions there is no section.
#[test]
fn wsl_distributions_in_the_sidebar() {
    let mut h = app(demo(), demo());
    // The drive and WSL refresh has answered (it would replace the fake list).
    assert!(wait(&mut h, 30, &|s| s.sidebar.drives_pending.is_none()));
    assert!(!has(&h, "WSL"), "no section without distributions");
    sm(&mut h).sidebar.wsl_frozen = true;
    sm(&mut h).sidebar.wsl = vec!["Ubuntu".into(), "Broken".into()];
    sm(&mut h).sidebar.wsl_open = fake_wsl_open;
    h.run_steps(2);
    assert!(has(&h, "WSL"));
    assert!(
        pos(&h, "Ubuntu", 0).y < pos(&h, "Broken", 0).y,
        "default first"
    );
    click(&mut h, "Ubuntu");
    assert_eq!(s(&h).sidebar.wsl_starting.as_deref(), Some("Ubuntu"));
    assert!(has_contains(&h, "starting…"), "shown while it starts");
    // A second click while it starts does nothing.
    click(&mut h, "Broken");
    let docs = demo().join("Documents");
    assert!(wait(&mut h, 10, &|s| dir_is(s, s.active, &docs)
        && s.sidebar.wsl_starting.is_none()));
    assert!(!has_contains(&h, "starting…"));
    assert!(toast_texts(s(&h)).is_empty(), "{:?}", toast_texts(s(&h)));
    shot(&mut h, "wsl-sidebar");
    click(&mut h, "Broken");
    assert!(wait(&mut h, 10, &|s| toast_texts(s).iter().any(
        |t| t.starts_with("Could not open Broken: The network name")
    )));
    assert!(dir_is(s(&h), s(&h).active, &docs), "the pane stays");
    sm(&mut h).sidebar.wsl.clear();
    h.run_steps(2);
    assert!(!has(&h, "WSL"));
}

/// Task 60: Organize by… on a folder: the dialog, the preview listing the groups it makes,
/// Enter runs it, Ctrl+Z puts every file back and the group folders go.
#[test]
fn organize_by_type_then_undo() {
    use crate::folder_ops::Kind;
    use crate::library::LibCmd;
    let dir = scratch("organize");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let before = tree_of(&a);
    let (mut h, _base, _lib) = with_library("organize-lib", &a, &b);
    sm(&mut h).run(0, Action::Library(LibCmd::FolderOp(Kind::Organize)));
    h.run_steps(3);
    assert!(has(&h, "Organize by"), "the dialog");
    assert!(has(&h, "Year and month"));
    key(&mut h, NONE, Key::Enter);
    assert!(
        wait(&mut h, 20, &|s| s.library.plan.is_some()),
        "the preview"
    );
    h.run_steps(3);
    let headline = format!(
        "Organize {} by type: 5 files into 3 folders",
        VPath::local(&a).display()
    );
    assert!(has(&h, &headline), "{}", dump(&h, "Organize"));
    for group in ["Documents", "Other", "Archives"] {
        let made = format!("Make folder {}", VPath::local(a.join(group)).display());
        assert!(has(&h, &made), "{}", dump(&h, "Make folder"));
    }
    shot(&mut h, "organize-preview");
    key(&mut h, NONE, Key::Enter);
    assert!(wait(&mut h, 30, &|_| a.join("Other/garden.rs").exists()));
    assert!(a.join("Documents/notes.md").exists() && a.join("Archives/site.zip").exists());
    assert!(wait(&mut h, 20, &|s| s.undo.label().as_deref()
        == Some("Undo organize")));
    key(&mut h, CMD, Key::Z);
    let asked = |s: &AppState| {
        matches!(&s.dialog, Some(crate::dialogs::Dialog::Confirm { text, .. })
            if text.starts_with("Undo organize: move 5 items back"))
    };
    assert!(wait(&mut h, 20, &asked), "{:?}", toast_texts(s(&h)));
    shot(&mut h, "organize-undo-preview");
    // Enter presses the focused button, which starts on Undo.
    key(&mut h, NONE, Key::Enter);
    let back = wait(&mut h, 30, &|_| tree_of(&a) == before);
    assert!(back, "everything is back: {:?}", tree_of(&a));
    close_library(&mut h);
}

/// Task 64: a new profile opens in Finder-style (Finder's spacing and type, the Finder
/// palette on the stock dark theme); Settings → Appearance → Classic switches back to the
/// look before at once. One screenshot per style.
#[test]
fn appearance_style_finder_and_classic() {
    use crate::design::{Style, CLASSIC, FINDER};
    let root = demo();
    let new_profile = boot(
        &root.join("Documents"),
        &root.join("Pictures"),
        Default::default(),
    );
    let mut h = app_with(new_profile, SIZE);
    h.run_steps(2);
    let ctx = s(&h).ctx.clone();
    assert_eq!(s(&h).settings.style, Style::Finder, "the default");
    assert_eq!(crate::design::look(&ctx).style, Style::Finder);
    assert_eq!(ctx.style().spacing.interact_size.y, FINDER.interact_height);
    assert_eq!(
        ctx.style().visuals.extreme_bg_color,
        egui::Color32::from_rgb(0x1e, 0x1e, 0x1e)
    );
    shot(&mut h, "style-finder");
    key(&mut h, CMD, Key::Comma);
    click_last(&mut h, "Appearance");
    click(&mut h, "Classic");
    h.run_steps(2);
    assert_eq!(s(&h).settings.style, Style::Classic);
    assert_eq!(crate::design::look(&ctx).style, Style::Classic);
    assert_eq!(ctx.style().spacing.interact_size.y, CLASSIC.interact_height);
    let bg = crate::theme::rgb(s(&h).theme.bg);
    assert_eq!(ctx.style().visuals.extreme_bg_color, bg, "the theme's own");
    shot(&mut h, "style-classic");
    click(&mut h, "Finder-style");
    h.run_steps(2);
    assert_eq!(ctx.style().spacing.interact_size.y, FINDER.interact_height);
}
