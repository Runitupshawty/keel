//! The File List Editor (Task 77): a window that holds one file list (EFU, TXT, CSV, ...)
//! being made or changed. **New** starts an empty list; files and folders dragged onto the
//! window (or picked with **Add files…** / **Add folder…**) are added, a folder with
//! everything below it; **Import…** merges other lists in; **Remove** drops the selected
//! rows and **Remove duplicates** the repeated paths; **Update** walks the listed folders
//! again; **Save** writes the list as EFU. Reading, walking and writing happen on workers
//! (`keel_search::filelist::edit`); the window only shows the list.

use crate::filelist_ui::{FileListMsg, OPEN_EXTENSIONS};
use crate::i18n::tr;
use crate::state::Msg;
use crate::worker;
use crossbeam_channel::Sender;
use egui_extras::{Column, TableBuilder};
use keel_search::filelist::edit::{self, Updated};
use keel_search::filelist::{format_iso, read_file, FileList, Limits};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const ROW_H: f32 = 20.0;

/// What a worker did to the list, for the note under the toolbar (made on the UI thread,
/// in the interface language).
#[derive(Clone, Debug, PartialEq)]
pub enum Done {
    /// A list was opened to edit.
    Opened(PathBuf),
    /// Files and folders added: how many entries.
    Added(usize),
    /// Lists merged in: how many entries.
    Imported(usize),
    Updated(Updated),
}

/// A worker's answer for the editor.
pub enum EditorMsg {
    /// The list after an operation, or why it failed. `gen`: the edit it started from.
    Changed {
        gen: u64,
        result: Result<(FileList, Done), String>,
    },
    /// Saved to this file, or why not.
    Saved {
        gen: u64,
        result: Result<PathBuf, String>,
    },
}

/// The File List Editor window's state.
#[derive(Default)]
pub struct FileListEditor {
    pub open: bool,
    list: FileList,
    /// The file the list was opened from or saved to.
    path: Option<PathBuf>,
    /// Changed since it was opened or saved.
    dirty: bool,
    selected: BTreeSet<usize>,
    /// The last row clicked (Shift+click selects from it).
    anchor: Option<usize>,
    /// What a worker is doing now (the buttons wait).
    busy: Option<&'static str>,
    /// The last outcome or error.
    note: Option<Result<String, String>>,
    /// Counts the list's versions: a worker's answer for an older one is dropped.
    gen: u64,
    /// **New** on a changed list asks first.
    confirm_new: bool,
    /// The window's place last frame: files dropped over it are added to the list.
    pub rect: Option<egui::Rect>,
}

impl FileListEditor {
    /// Whether files dropped at `pos` go to the editor.
    pub fn takes_drop(&self, pos: Option<egui::Pos2>) -> bool {
        self.open
            && self.busy.is_none()
            && pos.is_some_and(|p| self.rect.is_some_and(|r| r.contains(p)))
    }

    /// Opens the window (an empty list the first time).
    pub fn show(&mut self) {
        self.open = true;
    }

    fn changed(&mut self) {
        self.gen += 1;
        self.dirty = true;
        self.selected.clear();
        self.anchor = None;
    }

    /// Runs `op` on a copy of the list on a worker; its answer replaces the list.
    fn run(
        &mut self,
        what: &'static str,
        tx: &Sender<Msg>,
        ctx: &egui::Context,
        op: impl FnOnce(FileList) -> Result<(FileList, Done), String> + Send + 'static,
    ) {
        self.busy = Some(what);
        let (list, gen, tx, ctx) = (self.list.clone(), self.gen, tx.clone(), ctx.clone());
        let started = worker::spawn("keel-filelist-editor", move || {
            let result = op(list);
            let msg = EditorMsg::Changed { gen, result };
            worker::send(&tx, &ctx, Msg::FileList(FileListMsg::Editor(msg)));
        });
        if !started {
            self.busy = None;
        }
    }

    /// Adds files and folders (dropped, or picked).
    pub fn add(&mut self, paths: Vec<PathBuf>, tx: &Sender<Msg>, ctx: &egui::Context) {
        if paths.is_empty() || self.busy.is_some() {
            return;
        }
        self.open = true;
        self.run("filelist_editor.adding", tx, ctx, move |mut list| {
            let n = edit::add_paths(&mut list, &paths, &Limits::default())
                .map_err(|e| format!("{e:#}"))?;
            Ok((list, Done::Added(n)))
        });
    }

    /// A worker's answer.
    pub fn answered(&mut self, msg: EditorMsg) {
        self.busy = None;
        match msg {
            EditorMsg::Changed { gen, result } if gen == self.gen => match result {
                Ok((list, done)) => {
                    self.list = list;
                    self.changed();
                    if let Done::Opened(path) = &done {
                        self.path = Some(path.clone());
                        self.dirty = false;
                    }
                    self.note = Some(Ok(note(&done)));
                }
                Err(e) => self.note = Some(Err(e)),
            },
            EditorMsg::Saved { gen, result } => match result {
                Ok(path) => {
                    self.dirty &= gen != self.gen;
                    self.note =
                        Some(Ok(tr("filelist_editor.saved")
                            .replace("{path}", &path.display().to_string())));
                    self.path = Some(path);
                }
                Err(e) => self.note = Some(Err(e)),
            },
            // For a list that has changed since.
            EditorMsg::Changed { .. } => {}
        }
    }

    fn new_list(&mut self) {
        self.list = FileList::default();
        self.path = None;
        self.changed();
        self.dirty = false;
        self.note = None;
        self.confirm_new = false;
    }

    fn remove_selected(&mut self) {
        let mut i = 0;
        let selected = std::mem::take(&mut self.selected);
        self.list.entries.retain(|_| {
            let keep = !selected.contains(&i);
            i += 1;
            keep
        });
        let n = selected.len();
        self.changed();
        self.note = Some(Ok(
            tr("filelist_editor.removed").replace("{n}", &n.to_string())
        ));
    }

    fn remove_duplicates(&mut self) {
        let n = edit::remove_duplicates(&mut self.list);
        if n > 0 {
            self.changed();
        }
        self.note = Some(Ok(
            tr("filelist_editor.duplicates_removed").replace("{n}", &n.to_string())
        ));
    }

    /// The row `i` was clicked (with Ctrl: toggles it; with Shift: from the last click).
    fn click(&mut self, i: usize, modifiers: egui::Modifiers) {
        if modifiers.shift {
            let from = self.anchor.unwrap_or(i);
            self.selected = (from.min(i)..=from.max(i)).collect();
            return;
        }
        if modifiers.command {
            if !self.selected.remove(&i) {
                self.selected.insert(i);
            }
        } else {
            self.selected = BTreeSet::from([i]);
        }
        self.anchor = Some(i);
    }

    fn save(&mut self, to: Option<PathBuf>, tx: &Sender<Msg>, ctx: &egui::Context) {
        let (list, gen, tx, ctx) = (self.list.clone(), self.gen, tx.clone(), ctx.clone());
        let name = self.path.as_ref().and_then(|p| p.file_name()).map_or_else(
            || format!("{}.efu", tr("filelist_editor.untitled")),
            |n| Path::new(n).with_extension("efu").display().to_string(),
        );
        let dir = self
            .path
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf);
        let title = tr("filelist_editor.save_title").to_owned();
        self.busy = Some("filelist_editor.saving");
        let started = worker::spawn("keel-filelist-save", move || {
            let target = to.or_else(|| {
                crate::platform::file_dialog(&title, |mut d| {
                    d = d.set_file_name(&name).add_filter("EFU", &["efu"]);
                    if let Some(dir) = dir {
                        d = d.set_directory(dir);
                    }
                    d.save_file()
                })
            });
            let result = match target {
                Some(path) => crate::filelist_ui::save_efu(&list, &path)
                    .map(|()| path)
                    .map_err(|e| format!("{e:#}")),
                None => Err(String::new()),
            };
            let msg = EditorMsg::Saved { gen, result };
            worker::send(&tx, &ctx, Msg::FileList(FileListMsg::Editor(msg)));
        });
        if !started {
            self.busy = None;
        }
    }

    /// Picks files or a folder (`folder`) on a worker and adds them.
    fn pick_and_add(&mut self, folder: bool, tx: &Sender<Msg>, ctx: &egui::Context) {
        let title = tr(if folder {
            "filelist_editor.add_folder"
        } else {
            "filelist_editor.add_files"
        })
        .to_owned();
        self.run("filelist_editor.adding", tx, ctx, move |mut list| {
            let picked = crate::platform::file_dialog(&title, |d| match folder {
                true => d.pick_folder().map(|p| vec![p]),
                false => d.pick_files(),
            })
            .unwrap_or_default();
            if picked.is_empty() {
                return Err(String::new());
            }
            let n = edit::add_paths(&mut list, &picked, &Limits::default())
                .map_err(|e| format!("{e:#}"))?;
            Ok((list, Done::Added(n)))
        });
    }

    /// Picks lists on a worker: `import` merges them in, else the first replaces the list.
    fn pick_lists(&mut self, import: bool, tx: &Sender<Msg>, ctx: &egui::Context) {
        let title = tr(if import {
            "filelist_editor.import"
        } else {
            "filelist_editor.open"
        })
        .to_owned();
        let (lists, all) = (
            tr("filelist_editor.filter_lists").to_owned(),
            tr("filelist_editor.filter_all").to_owned(),
        );
        let what = if import {
            "filelist_editor.importing"
        } else {
            "filelist_editor.opening"
        };
        self.run(what, tx, ctx, move |mut list| {
            let picked = crate::platform::file_dialog(&title, |d| {
                let d = d
                    .add_filter(&lists, OPEN_EXTENSIONS)
                    .add_filter(&all, &["*"]);
                match import {
                    true => d.pick_files(),
                    false => d.pick_file().map(|p| vec![p]),
                }
            })
            .unwrap_or_default();
            if picked.is_empty() {
                return Err(String::new());
            }
            let limits = Limits::default();
            if !import {
                let path = picked[0].clone();
                let opened = read_file(&path, &limits).map_err(|e| format!("{e:#}"))?;
                return Ok((opened, Done::Opened(path)));
            }
            let mut n = 0;
            for path in &picked {
                let other = read_file(path, &limits).map_err(|e| format!("{e:#}"))?;
                n += other.entries.len();
                edit::merge(&mut list, other, &limits).map_err(|e| format!("{e:#}"))?;
            }
            Ok((list, Done::Imported(n)))
        });
    }

    fn update(&mut self, tx: &Sender<Msg>, ctx: &egui::Context) {
        self.run("filelist_editor.updating", tx, ctx, |list| {
            let (fresh, stats) =
                edit::update(&list, &Limits::default()).map_err(|e| format!("{e:#}"))?;
            Ok((fresh, Done::Updated(stats)))
        });
    }
}

/// The note for what a worker did.
fn note(done: &Done) -> String {
    match done {
        Done::Opened(path) => {
            tr("filelist_editor.opened").replace("{path}", &path.display().to_string())
        }
        Done::Added(n) => tr("filelist_editor.added").replace("{n}", &n.to_string()),
        Done::Imported(n) => tr("filelist_editor.imported").replace("{n}", &n.to_string()),
        Done::Updated(u) => tr("filelist_editor.updated")
            .replace("{folders}", &u.folders.to_string())
            .replace("{missing}", &u.missing.to_string()),
    }
}

/// The window title: the list's file name, with `*` while it has unsaved changes.
fn title(ed: &FileListEditor) -> String {
    let name = (ed.path.as_ref()).and_then(|p| p.file_name()).map_or_else(
        || tr("filelist_editor.untitled").to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    format!(
        "{}: {name}{}",
        tr("filelist_editor.title"),
        if ed.dirty { " *" } else { "" }
    )
}

/// The File List Editor window, while open. Returns a list to open as a search tab.
pub fn window(
    ctx: &egui::Context,
    ed: &mut FileListEditor,
    tx: &Sender<Msg>,
) -> Option<(PathBuf, FileList)> {
    if !ed.open {
        ed.rect = None;
        return None;
    }
    let mut open = true;
    let mut out = None;
    // A fixed id: the title changes with the list's name.
    let shown = egui::Window::new(title(ed))
        .id(egui::Id::new("keel-filelist-editor"))
        .open(&mut open)
        .default_size([760.0, 480.0])
        .min_size([420.0, 240.0])
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| out = body(ui, ed, tx));
    ed.rect = shown.map(|s| s.response.rect);
    ed.open &= open;
    out
}

fn body(
    ui: &mut egui::Ui,
    ed: &mut FileListEditor,
    tx: &Sender<Msg>,
) -> Option<(PathBuf, FileList)> {
    let ctx = ui.ctx().clone();
    let idle = ed.busy.is_none();
    let mut out = None;
    ui.horizontal_wrapped(|ui| {
        ui.add_enabled_ui(idle, |ui| {
            if ui
                .button(tr("filelist_editor.new"))
                .on_hover_text(tr("filelist_editor.new_tip"))
                .clicked()
            {
                if ed.dirty && !ed.list.entries.is_empty() {
                    ed.confirm_new = true;
                } else {
                    ed.new_list();
                }
            }
            if ui.button(tr("filelist_editor.open")).clicked() {
                ed.pick_lists(false, tx, &ctx);
            }
            if ui.button(tr("filelist_editor.add_files")).clicked() {
                ed.pick_and_add(false, tx, &ctx);
            }
            if ui.button(tr("filelist_editor.add_folder")).clicked() {
                ed.pick_and_add(true, tx, &ctx);
            }
            if ui
                .button(tr("filelist_editor.import"))
                .on_hover_text(tr("filelist_editor.import_tip"))
                .clicked()
            {
                ed.pick_lists(true, tx, &ctx);
            }
            ui.separator();
            if ui
                .add_enabled(
                    !ed.selected.is_empty(),
                    egui::Button::new(tr("filelist_editor.remove")),
                )
                .clicked()
            {
                ed.remove_selected();
            }
            if ui
                .add_enabled(
                    !ed.list.entries.is_empty(),
                    egui::Button::new(tr("filelist_editor.remove_duplicates")),
                )
                .clicked()
            {
                ed.remove_duplicates();
            }
            if ui
                .add_enabled(
                    !ed.list.entries.is_empty(),
                    egui::Button::new(tr("filelist_editor.update")),
                )
                .on_hover_text(tr("filelist_editor.update_tip"))
                .clicked()
            {
                ed.update(tx, &ctx);
            }
            ui.separator();
            let save_to = ed
                .path
                .clone()
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("efu")));
            if ui
                .add_enabled(
                    ed.dirty || save_to.is_none(),
                    egui::Button::new(tr("filelist_editor.save")),
                )
                .on_hover_text(tr("filelist_editor.save_tip"))
                .clicked()
            {
                ed.save(save_to, tx, &ctx);
            }
            if ui.button(tr("filelist_editor.save_as")).clicked() {
                ed.save(None, tx, &ctx);
            }
            if ui
                .add_enabled(
                    !ed.list.entries.is_empty(),
                    egui::Button::new(tr("filelist_editor.search")),
                )
                .on_hover_text(tr("filelist_editor.search_tip"))
                .clicked()
            {
                let path = ed
                    .path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(tr("filelist_editor.untitled")));
                out = Some((path, ed.list.clone()));
            }
        });
    });
    if ed.confirm_new {
        ui.horizontal(|ui| {
            ui.label(tr("filelist_editor.discard"));
            if ui.button(tr("filelist_editor.discard_yes")).clicked() {
                ed.new_list();
            }
            if ui.button(tr("dialogs.cancel")).clicked() {
                ed.confirm_new = false;
            }
        });
    }
    ui.horizontal(|ui| {
        if let Some(what) = ed.busy {
            crate::anim::spinner(ui);
            ui.label(tr(what));
        } else {
            match &ed.note {
                Some(Ok(text)) => {
                    ui.weak(text);
                }
                Some(Err(e)) if !e.is_empty() => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                _ => {}
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(
                tr("filelist_editor.count")
                    .replace("{n}", &ed.list.entries.len().to_string())
                    .replace("{selected}", &ed.selected.len().to_string()),
            );
        });
    });
    ui.separator();
    if ed.list.entries.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.weak(tr("filelist_editor.empty"));
        });
        return out;
    }
    let modifiers = ui.input(|i| i.modifiers);
    let mut clicked = None;
    let selected_fg = ui.visuals().strong_text_color();
    TableBuilder::new(ui)
        .id_salt("keel-filelist-editor-table")
        .striped(true)
        .resizable(true)
        .sense(egui::Sense::click())
        .auto_shrink([false, false])
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::remainder().at_least(160.0).clip(true))
        .column(Column::initial(90.0).at_least(40.0))
        .column(Column::initial(150.0).at_least(60.0).clip(true))
        .header(ROW_H, |mut h| {
            for key in [
                "filelist_editor.path",
                "filelist_editor.size",
                "filelist_editor.modified",
            ] {
                h.col(|ui| {
                    ui.strong(tr(key));
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, ed.list.entries.len(), |mut row| {
                let i = row.index();
                let e = &ed.list.entries[i];
                let selected = ed.selected.contains(&i);
                row.set_selected(selected);
                row.col(|ui| {
                    let icon = if e.is_dir { "📁 " } else { "" };
                    let text = egui::RichText::new(format!("{icon}{}", e.path));
                    let text = if selected {
                        text.color(selected_fg)
                    } else {
                        text
                    };
                    ui.add(egui::Label::new(text).truncate().selectable(false));
                });
                row.col(|ui| {
                    if let Some(size) = e.size.filter(|_| !e.is_dir) {
                        ui.label(humansize::format_size(size, humansize::DECIMAL));
                    }
                });
                row.col(|ui| {
                    if let Some(t) = e.modified {
                        ui.label(
                            format_iso(t)
                                .replace('T', " ")
                                .trim_end_matches('Z')
                                .to_owned(),
                        );
                    }
                });
                if row.response().clicked() {
                    clicked = Some(i);
                }
            });
        });
    if let Some(i) = clicked {
        ed.click(i, modifiers);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_search::filelist::Entry;

    fn editor(paths: &[&str]) -> FileListEditor {
        let mut ed = FileListEditor::default();
        for p in paths {
            ed.list.entries.push(Entry {
                path: (*p).to_owned(),
                ..Entry::default()
            });
        }
        ed
    }

    fn paths(ed: &FileListEditor) -> Vec<&str> {
        ed.list.entries.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn selecting_and_removing_rows() {
        let mut ed = editor(&["/a", "/b", "/c", "/d"]);
        let none = egui::Modifiers::NONE;
        ed.click(1, none);
        ed.click(3, egui::Modifiers::SHIFT);
        assert_eq!(ed.selected, BTreeSet::from([1, 2, 3]));
        ed.click(2, egui::Modifiers::COMMAND);
        assert_eq!(ed.selected, BTreeSet::from([1, 3]));
        ed.remove_selected();
        assert_eq!(paths(&ed), ["/a", "/c"]);
        assert!(ed.dirty && ed.selected.is_empty());
    }

    #[test]
    fn duplicates_and_new() {
        let mut ed = editor(&["/a", "/A", "/b", "/a/"]);
        ed.remove_duplicates();
        assert_eq!(paths(&ed), ["/a", "/b"]);
        ed.new_list();
        assert!(ed.list.entries.is_empty() && !ed.dirty && ed.path.is_none());
    }

    #[test]
    fn answers_for_an_older_list_are_dropped() {
        let mut ed = editor(&["/a"]);
        let gen = ed.gen;
        ed.remove_duplicates();
        ed.click(0, egui::Modifiers::NONE);
        ed.remove_selected();
        let stale = FileList {
            entries: vec![Entry {
                path: "/stale".into(),
                ..Entry::default()
            }],
            ..FileList::default()
        };
        ed.answered(EditorMsg::Changed {
            gen,
            result: Ok((stale, Done::Added(1))),
        });
        assert!(ed.list.entries.is_empty());
        let fresh = FileList {
            entries: vec![Entry {
                path: "/x".into(),
                ..Entry::default()
            }],
            ..FileList::default()
        };
        ed.answered(EditorMsg::Changed {
            gen: ed.gen,
            result: Ok((fresh, Done::Opened(PathBuf::from("/lists/x.efu")))),
        });
        assert_eq!(paths(&ed), ["/x"]);
        assert!(!ed.dirty, "a list just opened has no changes");
        assert!(title(&ed).contains("x.efu"));
        // Saved: no longer dirty, unless it changed meanwhile.
        ed.click(0, egui::Modifiers::NONE);
        ed.remove_selected();
        assert!(title(&ed).ends_with('*'));
        ed.answered(EditorMsg::Saved {
            gen: ed.gen,
            result: Ok(PathBuf::from("/lists/y.efu")),
        });
        assert!(!ed.dirty);
        assert!(title(&ed).contains("y.efu"));
    }

    #[test]
    fn drops_land_in_the_window_only_while_it_is_open() {
        let mut ed = FileListEditor {
            rect: Some(egui::Rect::from_min_size(
                egui::pos2(10.0, 10.0),
                egui::vec2(100.0, 100.0),
            )),
            ..FileListEditor::default()
        };
        let inside = Some(egui::pos2(50.0, 50.0));
        assert!(!ed.takes_drop(inside), "closed");
        ed.show();
        assert!(ed.takes_drop(inside));
        assert!(!ed.takes_drop(Some(egui::pos2(500.0, 50.0))));
        assert!(!ed.takes_drop(None));
    }
}
