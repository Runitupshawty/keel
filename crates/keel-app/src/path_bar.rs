//! The path bar (Phase 14 Task 67): a strip above the status bar with the active folder's
//! crumbs, icon and name each, from the root down, and the selected item after them.
//! Double-click (or Enter on a focused crumb) opens one; files dragged onto a folder
//! crumb move there (Alt / Option copies) through the usual transfer jobs; a drag resting
//! on a crumb opens it (`spring`). Right-click: Copy as Pathname, Open, Open in New Tab.
//! Shown or hidden from the palette or Settings → General.

use crate::finder::FinderCmd;
use crate::keys::Action;
use crate::pane::DragPayload;
use crate::state::AppState;
use egui::{vec2, Id, Rect, Sense};
use keel_vfs::{Entry, Kind, VPath};

/// One crumb: where it goes and what it says.
#[derive(Clone, Debug, PartialEq)]
pub struct Crumb {
    pub path: VPath,
    pub label: String,
    /// A folder (the selected item may be a file).
    pub dir: bool,
}

/// The crumbs of `dir` from its root down, plus `item` (the selected entry) at the end.
pub fn crumbs(dir: &VPath, item: Option<&Entry>) -> Vec<Crumb> {
    let mut dirs = Vec::new();
    let mut cur = Some(dir.clone());
    while let Some(d) = cur {
        cur = d.parent();
        dirs.push(d);
    }
    dirs.reverse();
    let mut out: Vec<Crumb> = (dirs.into_iter().enumerate())
        .map(|(i, path)| Crumb {
            label: crate::pane::crumb_label(&path, i),
            path,
            dir: true,
        })
        .collect();
    if let Some(e) = item.filter(|e| e.path.parent().as_ref() == Some(dir)) {
        out.push(Crumb {
            path: e.path.clone(),
            label: e.name.clone(),
            dir: e.kind == Kind::Dir,
        });
    }
    out
}

/// What dropping a drag on a crumb does: "Move" or "Copy" (Alt held), for the drag's
/// tooltip. None when the pointer is on no folder crumb.
pub fn drag_verb(ctx: &egui::Context) -> Option<&'static str> {
    let over = ctx.data(|d| d.get_temp::<bool>(hover_id()).unwrap_or(false));
    over.then(|| {
        if ctx.input(|i| i.modifiers.alt) {
            "Copy"
        } else {
            "Move"
        }
    })
}

fn hover_id() -> Id {
    Id::new("keel-pathbar-drop")
}

/// The bar's contents for the active pane.
pub fn ui(ui: &mut egui::Ui, s: &mut AppState, out: &mut Vec<(usize, Action)>) {
    let p = s.active;
    let show_hidden = s.show_hidden;
    s.tab_mut(p).visible(show_hidden);
    let tab = s.tab(p);
    let item = (tab.selected.len() <= 1)
        .then(|| tab.targets().first().map(|e| (*e).clone()))
        .flatten();
    let list = crumbs(&tab.dir.clone(), item.as_ref());
    ui.ctx().data_mut(|d| d.insert_temp(hover_id(), false));
    egui::ScrollArea::horizontal()
        .stick_to_right(true)
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (i, c) in list.iter().enumerate() {
                    if i > 0 {
                        ui.weak("›");
                    }
                    crumb(ui, p, c, out);
                }
            });
        });
}

fn crumb(ui: &mut egui::Ui, p: usize, c: &Crumb, out: &mut Vec<(usize, Action)>) {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let color = ui.visuals().text_color();
    let galley = ui.painter().layout_no_wrap(c.label.clone(), font, color);
    let pad = 4.0;
    let icon = 16.0;
    let h = galley.size().y.max(icon) + 4.0;
    let size = vec2(pad + icon + 4.0 + galley.size().x + pad, h);
    let (rect, r) = ui.allocate_exact_size(size, Sense::click());
    let r = crate::a11y::Named::named(r, &format!("{}: {}", c.label, c.path.display()));
    let ctx = ui.ctx().clone();
    let drag = r
        .dnd_hover_payload::<DragPayload>()
        .filter(|d| c.dir && !d.paths.contains(&c.path) && d.dir != c.path);
    let visuals = ui.visuals();
    if drag.is_some() {
        ui.painter()
            .rect_filled(rect, 4.0, visuals.selection.bg_fill.gamma_multiply(0.5));
        ui.painter().rect_stroke(
            rect,
            4.0,
            visuals.selection.stroke,
            egui::StrokeKind::Inside,
        );
        ctx.data_mut(|d| d.insert_temp(hover_id(), true));
        crate::spring::hovered(&ctx, p, &c.path);
    } else if r.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let icon_rect = Rect::from_min_size(
        rect.left_center() + vec2(pad, -icon / 2.0),
        vec2(icon, icon),
    );
    let style = crate::folder_style::published(&ctx, &c.path);
    let base = if c.dir {
        crate::icons::folder()
    } else {
        crate::icons::file_icon(&c.label)
    };
    crate::folder_style::paint(ui, icon_rect, base, style.as_ref(), c.dir, false);
    let text_at = egui::pos2(
        icon_rect.right() + 4.0,
        rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(text_at, galley, color);
    let r = r.on_hover_text(c.path.display());
    // Enter or Space on a focused crumb (a click without the pointer) opens it too.
    let keyboard = r.clicked() && !ctx.input(|i| i.pointer.any_released());
    if r.clicked() && !keyboard {
        // A pointer click leaves the keys with the file list.
        r.surrender_focus();
    }
    if r.double_clicked() || keyboard {
        out.push((
            p,
            if c.dir {
                Action::Navigate(c.path.clone())
            } else {
                Action::Enter
            },
        ));
    }
    if let Some(d) = r.dnd_release_payload::<DragPayload>() {
        if c.dir && !d.paths.contains(&c.path) && d.dir != c.path {
            let copy = ctx.input(|i| i.modifiers.alt);
            out.push((
                p,
                Action::Finder(FinderCmd::DropOnCrumb {
                    pane: d.pane,
                    dir: d.dir.clone(),
                    paths: d.paths.clone(),
                    dst: c.path.clone(),
                    copy,
                }),
            ));
        }
    }
    r.context_menu(|ui| {
        if ui.button("Copy as Pathname").clicked() {
            out.push((p, Action::Finder(FinderCmd::CopyPathname(c.path.clone()))));
            ui.close_menu();
        }
        if c.dir {
            if ui.button("Open").clicked() {
                out.push((p, Action::Navigate(c.path.clone())));
                ui.close_menu();
            }
            if ui.button("Open in New Tab").clicked() {
                out.push((p, Action::NewTabAt(c.path.clone())));
                ui.close_menu();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &VPath, kind: Kind) -> Entry {
        Entry {
            path: path.clone(),
            name: path.name().to_owned(),
            kind,
            size: 1,
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: String::new(),
        }
    }

    #[test]
    fn crumbs_run_from_the_root_down_to_the_selected_item() {
        let dir = VPath::parse("mem://t/a/b").unwrap();
        let file = entry(&dir.join("notes"), Kind::File);
        let list = crumbs(&dir, Some(&file));
        let labels: Vec<&str> = list.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels.last(), Some(&"notes"));
        assert_eq!(&labels[labels.len() - 3..labels.len() - 1], ["a", "b"]);
        assert!(list[..list.len() - 1].iter().all(|c| c.dir));
        assert!(!list.last().unwrap().dir);
        assert_eq!(list[list.len() - 2].path, dir);
        // Each crumb is its follower's parent.
        for w in list.windows(2) {
            assert_eq!(w[1].path.parent().as_ref(), Some(&w[0].path));
        }
        // No item, or one from another folder (a search result): the folders only.
        assert_eq!(crumbs(&dir, None).len(), list.len() - 1);
        let elsewhere = entry(&VPath::parse("mem://t/x/y").unwrap(), Kind::File);
        assert_eq!(crumbs(&dir, Some(&elsewhere)).len(), list.len() - 1);
    }

    #[test]
    fn a_local_folder_starts_at_its_drive_or_root() {
        let tmp = std::env::temp_dir().join("keel-crumbs").join("inner");
        let list = crumbs(&VPath::local(&tmp), None);
        assert_eq!(list.last().unwrap().label, "inner");
        assert_eq!(list[list.len() - 2].label, "keel-crumbs");
        assert!(list[0].path.parent().is_none(), "the first crumb is a root");
        let trash = crumbs(&keel_vfs::trashbin::root(), None);
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].label, keel_vfs::trashbin::label());
    }
}
