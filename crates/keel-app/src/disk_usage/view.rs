//! The **Disk usage** view mode of a pane: the Disk Usage window's tree list for the
//! folder the pane shows (its subfolders and files biggest first, with the window's
//! columns). When the window's scan holds that folder its tree is used as it is (and
//! follows the window's File Watcher); otherwise the folder is scanned on a worker, at the
//! window's scan priority. Moving to another folder shows that one.

use super::model::{Column, Model};
use super::ui::{self, ListStyle};
use super::{DuSettings, MetaReader};
use crate::i18n::tr;
use crate::keys::Action;
use crate::pane::ViewCx;
use crate::tab::Tab;
use egui::Ui;
use keel_vfs::du::{self, Priority, Root, ScanHandle, ScanOptions, Tree};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

/// What panes take from the Disk Usage window each frame (`share`).
#[derive(Clone)]
pub struct Shared {
    pub columns: Vec<Column>,
    pub stripes: bool,
    pub grid: bool,
    pub seconds: bool,
    pub physical: bool,
    pub priority: Priority,
    pub palette: Vec<[u8; 3]>,
    /// The window's tree and its generation.
    pub tree: Option<(u64, Arc<Tree>)>,
}

impl Default for Shared {
    fn default() -> Self {
        Self::from_settings(&DuSettings::default(), None)
    }
}

impl Shared {
    pub fn from_settings(s: &DuSettings, tree: Option<(u64, Arc<Tree>)>) -> Self {
        Shared {
            columns: s.shown_columns(),
            stripes: s.stripes,
            grid: s.grid,
            seconds: s.seconds,
            physical: s.physical,
            priority: s.priority,
            palette: s.chart_options.palette.clone(),
            tree,
        }
    }
}

fn shared_id() -> egui::Id {
    egui::Id::new("keel-du-shared")
}

/// Leaves the window's settings and tree where panes find them this frame.
pub fn share(ctx: &egui::Context, shared: Shared) {
    ctx.data_mut(|d| d.insert_temp(shared_id(), shared));
}

fn shared(ctx: &egui::Context) -> Shared {
    ctx.data(|d| d.get_temp::<Shared>(shared_id()))
        .unwrap_or_default()
}

/// Where the rows come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// The window's scan of this generation.
    Window(u64),
    /// A scan of the folder by this view.
    Own,
}

/// One tab's Disk usage view.
#[derive(Default)]
pub struct PaneView {
    dir: Option<PathBuf>,
    source: Option<Source>,
    scan: Option<(ScanHandle, Receiver<anyhow::Result<Tree>>)>,
    pub model: Option<Model>,
    error: Option<String>,
    meta: MetaReader,
    generation: u64,
    ranks: HashMap<String, usize>,
    scroll: bool,
}

impl PaneView {
    /// The folder shown (None before the first frame).
    #[cfg(test)]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    #[cfg(test)]
    pub fn scanning(&self) -> bool {
        self.scan.is_some()
    }

    /// Shows `dir`: from the window's tree when it holds the folder, else from a scan of
    /// it (started here, or kept when it is of this folder already).
    pub fn follow(&mut self, dir: &Path, shared: &Shared) {
        if self.dir.as_deref() != Some(dir) {
            self.reset();
            self.dir = Some(dir.to_owned());
        }
        if let Some((generation, tree)) = &shared.tree {
            let node = tree.find(dir).filter(|&n| tree.node(n).is_container());
            if let Some(node) = node {
                if self.source != Some(Source::Window(*generation)) {
                    self.cancel();
                    let sort = self.model.as_ref().map(|m| m.sort);
                    self.show(
                        Model::below(tree.clone(), node),
                        shared,
                        Source::Window(*generation),
                    );
                    if let (Some(sort), Some(m)) = (sort, &mut self.model) {
                        m.sort = sort;
                        m.rebuild();
                    }
                }
                return;
            }
        }
        if matches!(self.source, Some(Source::Window(_))) {
            // The window's scan no longer holds this folder.
            self.reset();
            self.dir = Some(dir.to_owned());
        }
        if self.model.is_none() && self.scan.is_none() && self.error.is_none() {
            self.start(dir, shared.priority);
        }
    }

    /// Scans the folder again.
    pub fn rescan(&mut self, priority: Priority) {
        if let Some(dir) = self.dir.clone() {
            self.cancel();
            self.model = None;
            self.error = None;
            self.source = None;
            self.start(&dir, priority);
        }
    }

    fn start(&mut self, dir: &Path, priority: Priority) {
        let handle = ScanHandle::new();
        let (tx, rx) = channel();
        let (h, roots) = (handle.clone(), vec![Root::Path(dir.to_owned())]);
        let options = ScanOptions {
            priority,
            // A folder of a drive: free and unknown space belong to the drive's view.
            free_and_unknown: false,
            ..ScanOptions::default()
        };
        crate::worker::spawn("keel-du-pane", move || {
            let _ = tx.send(du::scan(&roots, &options, &h));
        });
        self.scan = Some((handle, rx));
    }

    fn cancel(&mut self) {
        if let Some((h, _)) = self.scan.take() {
            h.cancel();
        }
    }

    fn reset(&mut self) {
        self.cancel();
        let generation = self.generation;
        *self = PaneView::default();
        self.generation = generation;
    }

    fn show(&mut self, mut model: Model, shared: &Shared, source: Source) {
        model.physical = shared.physical;
        model.rebuild();
        if let Some(&(first, _)) = model.rows.first() {
            model.select(first, false, false);
        }
        let types = model.file_types(false, &|_: &str| None);
        self.ranks = (types.iter().enumerate())
            .flat_map(|(rank, t)| t.extensions.iter().map(move |e| (e.clone(), rank)))
            .collect();
        self.model = Some(model);
        self.source = Some(source);
        self.generation += 1;
        self.meta.reset(self.generation);
        self.scroll = true;
    }

    /// Takes in a finished scan of the folder and finished metadata reads.
    pub fn poll(&mut self, ctx: &egui::Context, shared: &Shared) {
        if let Some((_, rx)) = &self.scan {
            match rx.try_recv() {
                Ok(Ok(tree)) => {
                    self.scan = None;
                    let tree = Arc::new(tree);
                    let base = tree.roots().first().map_or(du::TOP, |r| r.node);
                    self.show(Model::below(tree, base), shared, Source::Own);
                }
                Ok(Err(e)) => {
                    self.scan = None;
                    self.error = Some(format!("{e:#}"));
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
                Err(TryRecvError::Disconnected) => self.scan = None,
            }
        }
        if let Some(m) = &mut self.model {
            m.set_physical(shared.physical);
        }
        self.meta.poll(ctx, self.model.as_mut());
    }
}

impl Drop for PaneView {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The view in a pane: a line about the folder, then its tree list.
pub fn ui(ui: &mut Ui, id: (usize, usize), tab: &mut Tab, cx: &mut ViewCx, out: &mut Vec<Action>) {
    let Some(dir) = tab.dir.to_local_path() else {
        ui.weak(tr("disk_usage.view_local_only"));
        return;
    };
    let shared = shared(ui.ctx());
    let view = tab.du_view.get_or_insert_with(Default::default);
    view.follow(&dir, &shared);
    view.poll(ui.ctx(), &shared);
    let mut rescan = false;
    ui.horizontal(|ui| {
        if let Some((h, _)) = &view.scan {
            crate::anim::spinner(ui);
            let p = h.progress();
            ui.label(
                tr("disk_usage.view_scanning")
                    .replace("{files}", &p.files.to_string())
                    .replace("{size}", &ui::size_text(p.bytes)),
            );
        } else if let Some(m) = &view.model {
            let n = m.tree.node(m.base);
            let bytes = m.bytes(m.base);
            ui.label(
                tr("disk_usage.view_summary")
                    .replace("{size}", &ui::size_text(bytes))
                    .replace("{files}", &n.files.to_string())
                    .replace("{folders}", &n.folders.to_string()),
            );
            if matches!(view.source, Some(Source::Window(_))) {
                ui.weak(tr("disk_usage.view_from_window"));
            }
        }
        if ui
            .add_enabled(
                view.scan.is_none(),
                egui::Button::new(tr("disk_usage.rescan")),
            )
            .on_hover_text(tr("disk_usage.view_rescan_tip"))
            .clicked()
        {
            rescan = true;
        }
        if ui
            .button(tr("disk_usage.view_open_window"))
            .on_hover_text(tr("disk_usage.view_open_window_tip"))
            .clicked()
        {
            let cmd = super::DuCmd::Scan(vec![Root::Path(dir.clone())]);
            out.push(Action::DiskUsage(cmd));
        }
    });
    if rescan {
        view.rescan(shared.priority);
    }
    if let Some(e) = &view.error {
        ui.colored_label(ui.visuals().error_fg_color, e);
        return;
    }
    let salt = egui::Id::new(("keel-du-pane", id.0, id.1));
    let page = ui::page_rows(ui);
    let PaneView {
        model,
        meta,
        ranks,
        scroll,
        ..
    } = &mut **view;
    let Some(model) = model.as_mut() else {
        return;
    };
    if model.rows.is_empty() {
        ui.weak(tr("disk_usage.view_empty"));
        return;
    }
    if cx.keys && !ui.ctx().wants_keyboard_input() && ui::tree_keys(ui.ctx(), model, page) {
        *scroll = true;
    }
    if shared.columns.iter().any(|c| c.metadata()) {
        meta.want(model, ui::visible_rows(ui, salt, model, page as usize));
    }
    let style = ListStyle {
        salt,
        columns: shared.columns.clone(),
        stripes: shared.stripes,
        grid: shared.grid,
        seconds: shared.seconds,
        ranks,
        palette: &shared.palette,
        scroll: std::mem::take(scroll),
    };
    // A click in the pane makes it the active one already.
    let _ = ui::tree_list(ui, model, &style, cx.theme, out);
}
