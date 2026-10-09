//! Columns view (Finder's Miller columns): column 0 is the tab's own folder; a single
//! selected folder in column n is listed in column n + 1, a selected file is previewed in
//! a last column. The keyboard column (`Columns::focus`) is what `Pane::tab()` returns in
//! this view, so every file action works on it.

use crate::keys::Action;
use crate::pane::{context_menu, drag_and_drop, handle_click, Pane, ViewCx, ViewMode};
use crate::state::AppState;
use crate::tab::{Listing, Nav, Tab};
use crate::view_details::{date_text, size_text, ROW_H};
use egui::{vec2, Key, Rect, Sense, UiBuilder};
use keel_vfs::{Entry, Kind, VPath};

pub const DEFAULT_WIDTH: f32 = 230.0;
const PREVIEW_WIDTH: f32 = 340.0;
const MIN_WIDTH: f32 = 120.0;

/// The columns right of a tab's own folder.
#[derive(Default)]
pub struct Columns {
    /// Column `i + 1`, each a folder tab of its own (listing, selection, filter).
    pub cols: Vec<Tab>,
    /// Keyboard column: 0 is the tab itself.
    pub focus: usize,
    /// The file selected in the deepest column (the preview column).
    pub file: Option<Entry>,
    /// Right entered a column still loading: its first row gets the cursor on arrival.
    home_pending: bool,
}

/// Column `i` (0 = `root`).
pub fn col_mut(root: &mut Tab, i: usize) -> Option<&mut Tab> {
    match i {
        0 => Some(root),
        i => root.columns.cols.get_mut(i - 1),
    }
}

fn count(root: &Tab) -> usize {
    1 + root.columns.cols.len()
}

/// The keyboard column of `root` (`root` itself when focus is 0 or out of range).
pub fn focused(root: &Tab) -> &Tab {
    root.columns
        .focus
        .checked_sub(1)
        .and_then(|i| root.columns.cols.get(i))
        .unwrap_or(root)
}

/// Index into `cols` of the keyboard column, when it is not the root.
pub fn focused_index(root: &Tab) -> Option<usize> {
    root.columns
        .focus
        .checked_sub(1)
        .filter(|&i| i < root.columns.cols.len())
}

/// Keeps columns `0..=keep`, drops the preview.
fn truncate(root: &mut Tab, keep: usize) {
    root.columns.cols.truncate(keep);
    root.columns.focus = root.columns.focus.min(keep);
    root.columns.file = None;
}

/// Back to the tab's own folder only.
pub fn collapse(root: &mut Tab) {
    truncate(root, 0);
    root.columns.home_pending = false;
}

fn select_only(col: &mut Tab, name: &str) {
    col.selected = [name.to_owned()].into();
    col.cursor = Some(name.to_owned());
    col.anchor = Some(name.to_owned());
}

fn clear_selection(col: &mut Tab) {
    col.selected.clear();
    col.cursor = None;
    col.anchor = None;
}

/// Shows folder `dir` (a child of column `i`) as column `i + 1` and focuses it.
fn open_child(root: &mut Tab, i: usize, dir: VPath) {
    if let Some(col) = col_mut(root, i) {
        select_only(col, dir.name());
    }
    truncate(root, i);
    root.columns.cols.push(Tab::new(dir));
    root.columns.focus = i + 1;
}

/// The keyboard column without a cursor: its first row (now, or once it is listed).
fn home(root: &mut Tab, show_hidden: bool) {
    let focus = root.columns.focus;
    let Some(col) = col_mut(root, focus) else {
        return;
    };
    let waiting = col.listed_dir.is_none();
    if col.cursor.is_none() && !waiting {
        col.visible(show_hidden);
        col.move_cursor(Nav::Home, false);
    }
    root.columns.home_pending = waiting;
}

/// Makes the columns follow each column's selection: one selected folder opens the next
/// column (closing the ones after it), one selected file becomes the preview, anything
/// else closes everything right of it. Cheap when nothing changed (no listing scan).
pub fn sync(root: &mut Tab) {
    let mut i = 0;
    while i < count(root) {
        let next = root.columns.cols.get(i).map(|t| t.dir.clone());
        let deepest = next.is_none();
        let file = root.columns.file.as_ref().map(|e| e.path.clone());
        let Some(col) = col_mut(root, i) else { return };
        let child = col.cursor.clone().filter(|c| {
            col.selected.is_empty() || (col.selected.len() == 1 && col.selected.contains(c))
        });
        let Some(name) = child else {
            truncate(root, i);
            return;
        };
        let path = col.dir.join(&name);
        if next.as_ref() == Some(&path) {
            i += 1;
            continue;
        }
        if deepest && file.as_ref() == Some(&path) {
            return;
        }
        // The selection changed: one scan of this column's listing.
        let Some(e) = col.entries().iter().find(|e| e.name == name).cloned() else {
            // Not listed (yet): keep what is shown.
            return;
        };
        truncate(root, i);
        if e.kind == Kind::Dir && !e.encrypted {
            root.columns.cols.push(Tab::new(e.path));
        } else {
            root.columns.file = Some(e);
        }
        return;
    }
}

/// The folder of every column right of the tab (saved in the session).
pub fn chain(root: &Tab) -> Vec<VPath> {
    root.columns.cols.iter().map(|t| t.dir.clone()).collect()
}

/// Reopens a saved chain: each folder must be a child of the one before it (else the
/// chain stops there); the deepest column gets the keyboard.
pub fn restore(root: &mut Tab, chain: &[VPath]) {
    collapse(root);
    for dir in chain {
        let i = count(root) - 1;
        let Some(col) = col_mut(root, i) else { return };
        if dir.parent().as_ref() != Some(&col.dir) {
            break;
        }
        select_only(col, dir.name());
        root.columns.cols.push(Tab::new(dir.clone()));
    }
    root.columns.focus = count(root) - 1;
}

/// Columns view handling of navigation and Left/Right; other actions (and every action
/// in the other views) are returned to run as usual on `Pane::tab()`.
pub fn intercept(s: &mut AppState, p: usize, action: Action) -> Option<Action> {
    let show_hidden = s.show_hidden;
    let pane = &mut s.panes[p];
    let active = pane.active;
    if pane.view != ViewMode::Columns || pane.tabs[active].is_search() {
        return Some(action);
    }
    let root = &mut pane.tabs[active];
    act(root, action, show_hidden)
}

/// `intercept` on one tab (tests drive this directly).
pub fn act(root: &mut Tab, action: Action, show_hidden: bool) -> Option<Action> {
    let focus = root.columns.focus.min(count(root) - 1);
    match action {
        Action::Move(Nav::Left, _) => {
            root.columns.focus = focus.saturating_sub(1);
            root.columns.home_pending = false;
            None
        }
        Action::Move(Nav::Right, _) => {
            if focus + 1 < count(root) {
                root.columns.focus = focus + 1;
                home(root, show_hidden);
            }
            None
        }
        // Back pops the column path first, then walks the tab's history.
        Action::Back if !root.columns.cols.is_empty() => {
            root.columns.cols.pop();
            let last = count(root) - 1;
            if let Some(col) = col_mut(root, last) {
                clear_selection(col);
            }
            root.columns.focus = focus.min(last);
            root.columns.file = None;
            None
        }
        // The tab's own folder goes up; the folder it came from opens as column 1.
        Action::Up | Action::Backspace => {
            collapse(root);
            Some(action)
        }
        Action::Enter => {
            let col = col_mut(root, focus)?;
            match col.targets().first().map(|e| (*e).clone()) {
                Some(e) if e.kind == Kind::Dir && !e.encrypted => {
                    open_child(root, focus, e.path);
                    None
                }
                _ => Some(Action::Enter),
            }
        }
        Action::Navigate(to) => {
            // A shown column: close what is right of it.
            for i in 0..count(root) {
                let col = col_mut(root, i)?;
                if col.dir == to {
                    clear_selection(col);
                    truncate(root, i);
                    root.columns.focus = i;
                    return None;
                }
            }
            // A subfolder of a shown column: open it as the next column.
            for i in (0..count(root)).rev() {
                if col_mut(root, i).is_some_and(|c| to.parent().as_ref() == Some(&c.dir)) {
                    open_child(root, i, to);
                    return None;
                }
            }
            collapse(root);
            Some(Action::Navigate(to))
        }
        other => Some(other),
    }
}

/// `AppState::listed`: columns on `dir` waiting for request `req` take its answer; the
/// last one takes the listing itself (no copy of 100k entries) when `no_tab` needs it.
/// None when the listing was used up.
pub fn fill(
    panes: &mut [Pane; 2],
    dir: &VPath,
    req: u64,
    still_loading: bool,
    result: anyhow::Result<Listing>,
    no_tab: bool,
) -> Option<anyhow::Result<Listing>> {
    let mut cols: Vec<&mut Tab> = panes
        .iter_mut()
        .flat_map(|p| p.tabs.iter_mut())
        .flat_map(|t| t.columns.cols.iter_mut())
        .filter(|c| c.dir == *dir && req > c.listed_req)
        .collect();
    if cols.is_empty() {
        return Some(result);
    }
    let listing = match result {
        Ok(listing) => listing,
        Err(e) => {
            for c in cols {
                c.loading = still_loading;
                c.listed_req = req;
                c.error = Some(format!("{e:#}"));
            }
            return Some(Err(e));
        }
    };
    let set = |c: &mut Tab, listing: Listing| {
        c.set_listing(listing);
        c.listed_dir = Some(dir.clone());
        c.listed_req = req;
        c.loading = still_loading;
        c.error = None;
    };
    let last = if no_tab { cols.pop() } else { None };
    for c in cols {
        set(c, listing.clone());
    }
    match last {
        Some(c) => {
            set(c, listing);
            None
        }
        None => Some(Ok(listing)),
    }
}

/// Session restore: pane views and each tab's column chain.
pub fn restore_session(panes: &mut [Pane; 2], views: &[ViewMode; 2], chains: &[Vec<Vec<VPath>>]) {
    for (p, pane) in panes.iter_mut().enumerate() {
        pane.view = views[p];
        for (t, tab) in pane.tabs.iter_mut().enumerate() {
            if let Some(chain) = chains.get(p).and_then(|c| c.get(t)) {
                restore(tab, chain);
            }
        }
    }
}

/// Session save: per pane, per tab, `chain` (empty when no tab has columns).
pub fn session_chains(panes: &[Pane; 2]) -> Vec<Vec<Vec<VPath>>> {
    if panes
        .iter()
        .flat_map(|p| &p.tabs)
        .all(|t| t.columns.cols.is_empty())
    {
        return Vec::new();
    }
    panes
        .iter()
        .map(|p| p.tabs.iter().map(chain).collect())
        .collect()
}

/// The active pane shows a file in its preview column (the preview must follow it even
/// with the preview panel closed).
pub fn wants_preview(pane: &Pane) -> bool {
    pane.view == ViewMode::Columns && pane.tabs[pane.active].columns.file.is_some()
}

fn width(widths: &[f32], i: usize, default: f32) -> f32 {
    widths
        .get(i)
        .copied()
        .filter(|w| *w >= MIN_WIDTH)
        .unwrap_or(default)
}

pub fn ui(
    ui: &mut egui::Ui,
    id: (usize, usize),
    root: &mut Tab,
    cx: &mut ViewCx,
    out: &mut Vec<Action>,
) {
    if root.columns.home_pending {
        home(root, cx.show_hidden);
    }
    sync(root);
    let n = count(root);
    root.columns.focus = root.columns.focus.min(n - 1);
    let focus = root.columns.focus;
    let height = ui.available_height();
    // Scroll the keyboard column into view when it changes.
    let seen_id = ui.id().with(("columns-focus", id));
    let moved = ui.data(|d| d.get_temp::<(usize, usize)>(seen_id)) != Some((focus, n));
    ui.data_mut(|d| d.insert_temp(seen_id, (focus, n)));
    let mut clicked = None;
    let mut resized = None;
    egui::ScrollArea::horizontal()
        .id_salt(("columns", id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                for i in 0..n {
                    let w = width(cx.column_widths, i, DEFAULT_WIDTH);
                    let (rect, _) = ui.allocate_exact_size(vec2(w, height), Sense::hover());
                    if moved && i == focus {
                        ui.scroll_to_rect_animation(
                            rect,
                            None,
                            egui::style::ScrollAnimation::none(),
                        );
                    }
                    let mut child = ui.new_child(UiBuilder::new().max_rect(rect).id_salt(i));
                    child.set_clip_rect(rect.intersect(ui.clip_rect()));
                    let col = col_mut(root, i).expect("column in range");
                    if column(&mut child, (id.0, id.1, i), col, i == focus, cx, out) {
                        clicked = Some(i);
                    }
                    if let Some(dx) = handle(ui, height) {
                        resized = Some((i, (w + dx).max(MIN_WIDTH)));
                    }
                }
                if let Some(file) = root.columns.file.clone() {
                    let w = width(cx.column_widths, n, PREVIEW_WIDTH);
                    let (rect, _) = ui.allocate_exact_size(vec2(w, height), Sense::hover());
                    let mut child = ui.new_child(
                        UiBuilder::new()
                            .max_rect(rect.shrink2(vec2(6.0, 2.0)))
                            .id_salt("preview"),
                    );
                    child.set_clip_rect(rect.intersect(ui.clip_rect()));
                    preview(&mut child, &file, cx);
                    if let Some(dx) = handle(ui, height) {
                        resized = Some((n, (w + dx).max(MIN_WIDTH)));
                    }
                }
            });
        });
    if let Some(i) = clicked {
        root.columns.focus = i;
    }
    if let Some((i, w)) = resized {
        // Unset widths are saved as 0 (= the default).
        let widths = &mut *cx.column_widths;
        if widths.len() <= i {
            widths.resize(i + 1, 0.0);
        }
        widths[i] = w.round();
    }
}

/// The drag handle right of column `i`; the width change this frame.
fn handle(ui: &mut egui::Ui, height: f32) -> Option<f32> {
    let (rect, r) = ui.allocate_exact_size(vec2(5.0, height), Sense::drag());
    let r = r.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    let stroke = if r.hovered() || r.dragged() {
        ui.visuals().widgets.hovered.fg_stroke
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    ui.painter().vline(rect.center().x, rect.y_range(), stroke);
    r.dragged()
        .then(|| r.drag_delta().x)
        .filter(|dx| *dx != 0.0)
}

/// One column: virtual rows, like the details view. True when it was clicked (it takes
/// the keyboard).
fn column(
    ui: &mut egui::Ui,
    id: (usize, usize, usize),
    col: &mut Tab,
    focused: bool,
    cx: &mut ViewCx,
    out: &mut Vec<Action>,
) -> bool {
    let rect = ui.max_rect();
    if focused && cx.active {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().faint_bg_color.gamma_multiply(1.5));
    }
    // Empty space: click clears the selection (closing the columns right of it).
    let bg = ui.interact(rect, ui.id().with("bg"), Sense::click());
    let mut clicked = bg.clicked() || bg.secondary_clicked();
    if clicked {
        clear_selection(col);
    }
    bg.context_menu(|ui| context_menu(ui, col, None, out));
    if let Some(err) = col.error.clone() {
        ui.colored_label(ui.visuals().error_fg_color, err);
    }
    if col.loading && col.listed_dir.is_none() {
        ui.centered_and_justified(|ui| ui.spinner());
        return clicked;
    }
    col.visible(cx.show_hidden);
    col.row_step = 1;
    col.page_rows = ((ui.available_height() / ROW_H) as usize)
        .saturating_sub(1)
        .max(1);
    let n = col.visible_cached().len();
    let mut area = egui::ScrollArea::vertical()
        .id_salt(("column", id))
        .auto_shrink([false, false]);
    if let Some(pos) = col.scroll_to.take() {
        let (offset, viewport) = col.grid_scroll;
        let top = pos as f32 * ROW_H;
        if top < offset {
            area = area.vertical_scroll_offset(top);
        } else if top + ROW_H > offset + viewport {
            area = area.vertical_scroll_offset(top + ROW_H - viewport);
        }
    }
    let selection = ui.visuals().selection.bg_fill;
    let hover = ui.visuals().widgets.hovered.weak_bg_fill;
    let muted = cx.theme.muted();
    let fade = 1.0;
    let mut renaming = col.renaming.take();
    let mut rename_done = false;
    let mut rows: Vec<(egui::Response, Entry)> = Vec::new();
    let output = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            area.show_rows(ui, ROW_H, n, |ui, range| {
                for pos in range {
                    let e = &col.entries()[col.visible_cached()[pos]];
                    let (rect, r) = ui.allocate_exact_size(
                        vec2(ui.available_width(), ROW_H),
                        Sense::click_and_drag(),
                    );
                    if col.selected.contains(&e.name) {
                        let t = if col.cursor.as_deref() == Some(&e.name) {
                            fade
                        } else {
                            1.0
                        };
                        ui.painter()
                            .rect_filled(rect, 2.0, selection.gamma_multiply(t));
                    } else if r.hovered() {
                        ui.painter().rect_filled(rect, 2.0, hover);
                    }
                    let icon = Rect::from_center_size(
                        egui::pos2(rect.left() + 14.0, rect.center().y),
                        vec2(16.0, 16.0),
                    );
                    egui::Image::new(crate::icons::icon_for(e)).paint_at(ui, icon);
                    let text = Rect::from_min_max(
                        egui::pos2(icon.right() + 6.0, rect.top()),
                        egui::pos2(rect.right() - 16.0, rect.bottom()),
                    );
                    match &mut renaming {
                        Some((name, edit)) if *name == e.name => {
                            let mut child = ui.new_child(
                                UiBuilder::new()
                                    .max_rect(text)
                                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                            );
                            let te = child
                                .add(egui::TextEdit::singleline(edit).desired_width(text.width()));
                            if te.lost_focus() {
                                rename_done = true;
                                if ui.input(|i| i.key_pressed(Key::Enter))
                                    && !edit.trim().is_empty()
                                    && edit.trim() != e.name
                                {
                                    out.push(Action::RenameTo {
                                        from: e.path.clone(),
                                        to: edit.trim().to_owned(),
                                    });
                                }
                            } else {
                                te.request_focus();
                            }
                        }
                        _ => {
                            let color = if e.hidden {
                                muted
                            } else {
                                ui.visuals().text_color()
                            };
                            let galley = egui::WidgetText::from(e.name.as_str())
                                .color(color)
                                .into_galley(
                                    ui,
                                    Some(egui::TextWrapMode::Truncate),
                                    text.width(),
                                    egui::TextStyle::Body,
                                );
                            let at =
                                egui::pos2(text.left(), text.center().y - galley.size().y / 2.0);
                            ui.painter().galley(at, galley, color);
                        }
                    }
                    if e.kind == Kind::Dir {
                        ui.painter().text(
                            egui::pos2(rect.right() - 8.0, rect.center().y),
                            egui::Align2::CENTER_CENTER,
                            "›",
                            egui::TextStyle::Body.resolve(ui.style()),
                            muted,
                        );
                    }
                    r.context_menu(|ui| context_menu(ui, col, Some(e), out));
                    rows.push((r, e.clone()));
                }
            })
        })
        .inner;
    col.grid_scroll = (output.state.offset.y, output.inner_rect.height());
    for (r, e) in rows {
        clicked |= r.clicked() || r.secondary_clicked() || r.double_clicked() || r.drag_started();
        handle_click(&r, col, &e, out);
        drag_and_drop(&r, id.0, col, &e, out);
    }
    if !rename_done {
        col.renaming = renaming;
    }
    clicked
}

/// The preview column: the preview panel's rendering when this pane has it (the panel
/// itself closed), else name, size and date.
fn preview(ui: &mut egui::Ui, file: &Entry, cx: &mut ViewCx) {
    let shows_file = |p: &crate::preview_panel::PreviewPanel| {
        p.key.as_ref().is_some_and(|k| k.path == file.path)
    };
    match cx.preview.as_deref_mut() {
        Some(panel) if shows_file(panel) => {
            panel.width_px = (ui.available_width() * ui.ctx().pixels_per_point()).round() as u32;
            panel.ui(ui, cx.theme.muted());
        }
        _ => {
            ui.add_space(6.0);
            ui.add(egui::Label::new(egui::RichText::new(&file.name).strong()).truncate());
            let meta: Vec<String> = [size_text(file), date_text(file)]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect();
            ui.weak(meta.join("  ·  "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tab::test_entry;

    fn dir(p: &str) -> VPath {
        VPath::parse(&format!("mem://t{p}")).unwrap()
    }

    fn listed(path: &str, names: &[(&str, Kind)]) -> Tab {
        let d = dir(path);
        let mut t = Tab::new(d.clone());
        t.set_entries(
            names
                .iter()
                .map(|(n, k)| test_entry(&d, n, k.clone(), 1))
                .collect(),
        );
        t.loading = false;
        t.listed_dir = Some(d);
        t.visible(false);
        t
    }

    fn root() -> Tab {
        listed(
            "/r",
            &[("a", Kind::Dir), ("b", Kind::Dir), ("f.txt", Kind::File)],
        )
    }

    fn fill(root: &mut Tab, i: usize, names: &[(&str, Kind)]) {
        let d = col_mut(root, i).unwrap().dir.clone();
        let t = listed(&d.path, names);
        let col = col_mut(root, i).unwrap();
        col.set_listing(crate::tab::Listing::new(t.entries().to_vec()));
        col.loading = false;
        col.listed_dir = Some(d);
        col.visible(false);
    }

    #[test]
    fn selecting_in_column_n_closes_columns_after_n_plus_one() {
        let mut r = root();
        r.click("a", false, false);
        sync(&mut r);
        assert_eq!(chain(&r), [dir("/r/a")]);
        fill(&mut r, 1, &[("x", Kind::Dir), ("y.md", Kind::File)]);
        col_mut(&mut r, 1).unwrap().click("x", false, false);
        sync(&mut r);
        assert_eq!(chain(&r), [dir("/r/a"), dir("/r/a/x")]);
        fill(&mut r, 2, &[("deep", Kind::Dir)]);
        col_mut(&mut r, 2).unwrap().click("deep", false, false);
        sync(&mut r);
        assert_eq!(chain(&r).len(), 3);
        // Another folder in column 0: columns 2 and 3 close, column 1 shows it.
        r.click("b", false, false);
        sync(&mut r);
        assert_eq!(chain(&r), [dir("/r/b")]);
        // A file: no folder column, a preview instead.
        r.click("f.txt", false, false);
        sync(&mut r);
        assert!(chain(&r).is_empty());
        assert_eq!(r.columns.file.as_ref().unwrap().name, "f.txt");
        // Two selected: nothing right of column 0.
        r.click("a", true, false);
        sync(&mut r);
        assert!(chain(&r).is_empty() && r.columns.file.is_none());
        // A cursor not listed yet keeps the columns (restored session).
        let mut r = root();
        restore(&mut r, &[dir("/r/a")]);
        select_only(col_mut(&mut r, 1).unwrap(), "not-listed-yet");
        sync(&mut r);
        assert_eq!(chain(&r), [dir("/r/a")]);
        assert!(r.columns.file.is_none());
    }

    #[test]
    fn path_chain_round_trips_and_stops_at_a_break() {
        let mut r = root();
        let saved = vec![dir("/r/a"), dir("/r/a/x"), dir("/r/a/x/y")];
        restore(&mut r, &saved);
        assert_eq!(chain(&r), saved);
        assert_eq!(r.columns.focus, 3, "deepest column has the keyboard");
        assert_eq!(r.cursor.as_deref(), Some("a"));
        assert_eq!(col_mut(&mut r, 2).unwrap().cursor.as_deref(), Some("y"));
        let mut again = root();
        restore(&mut again, &chain(&r));
        assert_eq!(chain(&again), saved);
        // Not a child of the column before it: the chain stops there.
        restore(&mut r, &[dir("/r/a"), dir("/elsewhere/z")]);
        assert_eq!(chain(&r), [dir("/r/a")]);
    }

    #[test]
    fn keyboard_moves_between_columns_and_back_pops_the_path() {
        let mut r = root();
        r.move_cursor(Nav::Home, false);
        assert_eq!(r.cursor.as_deref(), Some("a"));
        sync(&mut r);
        fill(&mut r, 1, &[("x", Kind::Dir), ("y.md", Kind::File)]);
        // Right enters column 1 on its first row; Down moves within it.
        assert_eq!(act(&mut r, Action::Move(Nav::Right, false), false), None);
        assert_eq!(r.columns.focus, 1);
        assert_eq!(focused(&r).cursor.as_deref(), Some("x"));
        sync(&mut r);
        assert_eq!(chain(&r).len(), 2);
        col_mut(&mut r, 1).unwrap().move_cursor(Nav::Next, false);
        sync(&mut r);
        assert_eq!(chain(&r).len(), 1, "a file closes the folder column");
        assert_eq!(r.columns.file.as_ref().unwrap().name, "y.md");
        // Left goes back to column 0; Right past the last column does nothing.
        act(&mut r, Action::Move(Nav::Left, false), false);
        assert_eq!(r.columns.focus, 0);
        act(&mut r, Action::Move(Nav::Right, false), false);
        act(&mut r, Action::Move(Nav::Right, false), false);
        assert_eq!(r.columns.focus, 1);
        // Enter on a folder opens it as the next column and focuses it.
        col_mut(&mut r, 1).unwrap().move_cursor(Nav::Home, false);
        assert_eq!(act(&mut r, Action::Enter, false), None);
        assert_eq!((chain(&r).len(), r.columns.focus), (2, 2));
        // Enter on a file is a normal open.
        col_mut(&mut r, 1).unwrap().move_cursor(Nav::End, false);
        sync(&mut r);
        r.columns.focus = 1;
        assert_eq!(act(&mut r, Action::Enter, false), Some(Action::Enter));
        // Back pops the deepest column; then the tab's own history.
        sync(&mut r);
        r.columns.cols.push(Tab::new(dir("/r/a/x")));
        assert_eq!(act(&mut r, Action::Back, false), None);
        assert_eq!(chain(&r), [dir("/r/a")]);
        assert_eq!(col_mut(&mut r, 1).unwrap().cursor, None);
        assert_eq!(act(&mut r, Action::Back, false), None);
        assert!(chain(&r).is_empty());
        assert_eq!(act(&mut r, Action::Back, false), Some(Action::Back));
        // Navigate: a shown column truncates, a child opens, elsewhere collapses.
        restore(&mut r, &[dir("/r/a"), dir("/r/a/x")]);
        assert_eq!(act(&mut r, Action::Navigate(dir("/r/a")), false), None);
        assert_eq!((chain(&r).len(), r.columns.focus), (1, 1));
        assert_eq!(act(&mut r, Action::Navigate(dir("/r/a/q")), false), None);
        assert_eq!(chain(&r), [dir("/r/a"), dir("/r/a/q")]);
        let far = Action::Navigate(dir("/other"));
        assert_eq!(act(&mut r, far.clone(), false), Some(far));
        assert!(chain(&r).is_empty());
        // Up collapses, then runs on the tab.
        restore(&mut r, &[dir("/r/a")]);
        assert_eq!(act(&mut r, Action::Up, false), Some(Action::Up));
        assert!(chain(&r).is_empty());
    }

    /// Through `AppState`: a column is listed by a worker, actions work on the keyboard
    /// column, Back pops it, and the session keeps the chain.
    #[test]
    fn columns_list_through_the_app_state_and_survive_the_session() {
        use std::time::{Duration, Instant};
        let tmp = std::env::temp_dir().join(format!("keel-columns-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("a").join("deeper")).unwrap();
        std::fs::write(tmp.join("a").join("note.txt"), "hi").unwrap();
        let start = VPath::local(&tmp);
        let mut s = AppState::new(
            egui::Context::default(),
            std::sync::Arc::new(keel_vfs::Router::new()),
            start.clone(),
        );
        let settle = |s: &mut AppState, done: &dyn Fn(&AppState) -> bool| {
            let until = Instant::now() + Duration::from_secs(10);
            while !done(s) && Instant::now() < until {
                s.tick();
                if let Ok(msg) = s.rx.recv_timeout(Duration::from_millis(50)) {
                    s.apply(msg);
                }
            }
            assert!(done(s), "timed out");
        };
        settle(&mut s, &|s| !s.tab(0).loading);
        s.panes[0].view = ViewMode::Columns;
        s.run(0, Action::Move(Nav::Home, false));
        assert_eq!(s.tab(0).cursor.as_deref(), Some("a"));
        sync(&mut s.panes[0].tabs[0]);
        settle(&mut s, &|s| {
            s.panes[0].tabs[0]
                .columns
                .cols
                .first()
                .is_some_and(|c| c.entries().len() == 2)
        });
        s.run(0, Action::Move(Nav::Right, false));
        assert_eq!(s.tab(0).dir, start.join("a"), "actions go to column 1");
        assert_eq!(s.tab(0).cursor.as_deref(), Some("deeper"));
        let saved = crate::session::Session::of(&s);
        assert_eq!(saved.views[0], ViewMode::Columns);
        assert_eq!(saved.columns[0][0], [start.join("a")]);
        s.run(0, Action::Back);
        assert!(s.panes[0].tabs[0].columns.cols.is_empty());
        assert_eq!(s.tab(0).dir, start);
        let s = AppState::restore(
            egui::Context::default(),
            std::sync::Arc::new(keel_vfs::Router::new()),
            saved,
            crate::settings::Settings::default(),
            start.clone(),
        );
        assert_eq!(s.panes[0].view, ViewMode::Columns);
        assert_eq!(s.tab(0).dir, start.join("a"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The view renders (both panes in columns, a file preview column) and keyboard
    /// navigation drives it.
    #[test]
    fn columns_view_renders_and_follows_the_keyboard() {
        use crate::app::{App, Boot};
        use egui_kittest::Harness;
        let tmp = std::env::temp_dir().join(format!("keel-columns-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("a").join("b")).unwrap();
        std::fs::write(tmp.join("a").join("b").join("c.txt"), "hello").unwrap();
        let start = VPath::local(&tmp);
        let mut h = Harness::builder()
            .with_size(egui::vec2(1280.0, 720.0))
            .build_eframe(|cc| App::new(cc, Boot::at(start.clone())));
        let wait = |h: &mut Harness<App>, done: &dyn Fn(&App) -> bool| {
            for _ in 0..250 {
                h.step();
                if done(h.state()) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let s = &h.state().state;
            panic!(
                "timed out: {} {:?}",
                s.tab(0).dir.display(),
                chain(&s.panes[0].tabs[0])
            );
        };
        h.state_mut().state.panes[0].view = ViewMode::Columns;
        h.state_mut().state.panes[1].view = ViewMode::Columns;
        wait(&mut h, &|a| !a.state.tab(0).loading);
        for key in [Key::ArrowDown, Key::ArrowRight] {
            h.press_key(key);
            wait(&mut h, &|a| {
                !a.state.tab(0).loading && a.state.tab(0).entries().len() == 1
            });
        }
        assert_eq!(h.state().state.tab(0).dir, start.join("a"));
        h.press_key(Key::ArrowRight);
        wait(&mut h, &|a| a.state.tab(0).dir == start.join("a").join("b"));
        wait(&mut h, &|a| a.state.panes[0].tabs[0].columns.file.is_some());
        h.run_steps(3);
        assert_eq!(chain(&h.state().state.panes[0].tabs[0]).len(), 2);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Pane::tab() is the keyboard column in this view, the tab itself in the others.
    #[test]
    fn pane_tab_is_the_focused_column() {
        let mut pane = Pane::new(dir("/r"));
        restore(&mut pane.tabs[0], &[dir("/r/a")]);
        assert_eq!(pane.tab().dir, dir("/r"));
        pane.view = ViewMode::Columns;
        assert_eq!(pane.tab().dir, dir("/r/a"));
        assert_eq!(pane.tab_mut().dir, dir("/r/a"));
        pane.tabs[0].columns.focus = 0;
        assert_eq!(pane.tab().dir, dir("/r"));
    }
}
