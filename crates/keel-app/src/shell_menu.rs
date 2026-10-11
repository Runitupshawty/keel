//! "More options" (Task 43): the Windows shell menu for the selection, from the context
//! menu, Shift+right-click and Shift+F10, and its commands as palette "Shell: …" entries.
//! The shell calls live in `keel_vfs::shell::context_menu`; this is the glue. Local files
//! in one folder only: a handler would act on a temporary copy of anything else.

use crate::tab::Tab;
#[cfg(windows)]
use std::path::PathBuf;

/// Why the entry is greyed for remote, cloud, archive, library and multi-folder targets.
#[cfg(windows)]
pub const NOT_LOCAL: &str = "The Windows menu works on files in one folder on this computer";

/// The local paths the Windows menu acts on (the targets), or why it cannot.
#[cfg(windows)]
pub fn targets(tab: &Tab) -> Result<Vec<PathBuf>, &'static str> {
    let picked = tab.targets();
    if picked.is_empty() {
        return Err("Select files or folders first");
    }
    let paths: Vec<PathBuf> = (picked.iter())
        .map(|e| e.path.to_local_path())
        .collect::<Option<_>>()
        .ok_or(NOT_LOCAL)?;
    if paths.iter().any(|p| p.parent() != paths[0].parent()) {
        return Err(NOT_LOCAL);
    }
    Ok(paths)
}

/// The temp-data key of the cursor row's rect in `tab` (kept by `pane::a11y_item`).
pub fn row_id(tab: &Tab) -> egui::Id {
    egui::Id::new(("keel-shell-row", &tab.dir))
}

#[cfg(windows)]
pub(crate) mod win {
    use super::*;
    use crate::keys::Action;
    use crate::palette::Item;
    use crate::state::AppState;
    use keel_vfs::shell::context_menu::ShellMenu;

    /// Screen pixels just below the cursor row of `tab`, if it was drawn.
    fn row_point(ctx: &egui::Context, tab: &Tab) -> Option<(i32, i32)> {
        let rect: egui::Rect = ctx.data(|d| d.get_temp(row_id(tab)))?;
        let window = ctx.input(|i| i.viewport().inner_rect)?;
        let at = (window.min + rect.left_bottom().to_vec2()) * ctx.pixels_per_point();
        Some((at.x as i32, at.y as i32))
    }

    /// The palette's entries for the menu of `tab`'s targets, kept in `menu` for
    /// `ShellCommand`. None when the targets are not local or the shell failed. A label
    /// met twice (two apps of one name) keeps its first entry: the palette finds entries by
    /// label.
    #[cfg_attr(test, allow(dead_code))] // tests never build the shell menu
    pub fn palette_items(tab: &Tab, menu: &mut Option<ShellMenu>) -> Vec<Item> {
        *menu = targets(tab).ok().and_then(|paths| {
            ShellMenu::new(&paths, false)
                .map_err(|e| tracing::warn!("shell menu: {e:#}"))
                .ok()
        });
        let items = menu.as_ref().map(ShellMenu::items).unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        (items.into_iter())
            .filter(|i| seen.insert(i.label.clone()))
            .map(|i| Item {
                label: format!("Shell: {}", i.label),
                shortcut: "",
                action: Action::ShellCommand(i.id),
            })
            .collect()
    }

    impl AppState {
        /// Adds the palette's "Shell: …" entries for pane `p`'s targets (asked for by the
        /// first "shell" typed). Never in tests: a walkthrough runs every palette entry.
        pub(crate) fn palette_shell(&mut self, p: usize) {
            #[cfg(not(test))]
            {
                let extra = palette_items(self.panes[p].tab(), &mut self.palette.shell);
                self.palette.add(extra);
            }
            #[cfg(test)]
            let _ = p;
        }

        /// Builds the menu for pane `p`'s targets, shows it, and runs what was picked.
        pub(crate) fn shell_menu(&mut self, p: usize, extended: bool, keyboard: bool) {
            let tab = self.tab(p);
            let at = keyboard.then(|| row_point(&self.ctx, tab)).flatten();
            let shown = targets(tab).map_err(str::to_owned).and_then(|paths| {
                let menu = ShellMenu::new(&paths, extended).map_err(|e| format!("{e:#}"))?;
                match menu.popup(at).map_err(|e| format!("{e:#}"))? {
                    Some(id) => menu.invoke(id).map_err(|e| format!("{e:#}")),
                    None => Ok(()),
                }
            });
            if let Err(e) = shown {
                self.toasts.error(format!("More options: {e}"));
            }
        }

        /// Runs a palette "Shell: …" entry on the menu the palette built.
        pub(crate) fn shell_command(&mut self, id: u32) {
            let Some(menu) = self.palette.shell.take() else {
                return;
            };
            if let Err(e) = menu.invoke(id) {
                self.toasts.error(format!("More options: {e:#}"));
            }
        }
    }
}
