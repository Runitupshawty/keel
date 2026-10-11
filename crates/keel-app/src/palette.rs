//! Ctrl+Shift+P: every action by name, plus the context actions for the selection,
//! fuzzy filtered. A few dozen labels match instantly, so this runs on the UI thread.

use crate::keys::Action;
use crate::library::LibCmd;
use egui::{Id, Key, Modal, Modifiers};
use keel_search::Fuzzy;

pub struct Item {
    pub label: String,
    pub shortcut: &'static str,
    pub action: Action,
}

#[derive(Default)]
pub struct Palette {
    pub open: bool,
    pub text: String,
    pub items: Vec<Item>,
    /// Indices into `items` matching `text`, best first.
    pub shown: Vec<usize>,
    pub cursor: usize,
    /// Other profiles: "Switch profile: <name>" entries (kept by `AppState::profiles_tick`).
    pub profiles: Vec<String>,
    /// "Undo <what>" for the newest file operation, set before `show`.
    pub undo: Option<String>,
    focus: bool,
    fuzzy: Option<Fuzzy>,
    /// The Windows shell menu its "Shell: …" entries came from (`shell_menu`); dropped when
    /// the palette closes, unless for the Shell command picked.
    #[cfg(windows)]
    pub shell: Option<keel_vfs::shell::context_menu::ShellMenu>,
    /// The text began with "shell": `AppState` adds the "Shell: …" entries now. Loading the
    /// shell extensions waits for that, never the palette's opening.
    #[cfg(windows)]
    pub shell_asked: bool,
    #[cfg(windows)]
    shell_built: bool,
}

/// Whether `text` asks for the Windows menu's "Shell: …" entries.
#[cfg_attr(not(windows), allow(dead_code))]
fn wants_shell(text: &str) -> bool {
    let t = text.trim_start();
    t.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("shell"))
}

/// Actions on the selection (shown first when something is selected).
fn context_items() -> Vec<Item> {
    use Action::*;
    [
        ("Open", "Enter", Enter),
        ("Open with…", "Ctrl+Shift+O", OpenWith),
        ("Open location", "Ctrl+Enter", OpenLocation),
        ("Copy", "Ctrl+C", Copy),
        ("Cut", "Ctrl+X", Cut),
        ("Rename", "F2", Rename),
        ("Bulk rename…", "Ctrl+F2", BulkRename),
        ("Move to trash", "Del", Delete),
        ("Properties", "", Properties),
        ("Extract here", "", ExtractHere),
        ("Extract to folder", "", ExtractToFolder),
        ("Extract to…", "", ExtractTo),
        ("Extract to the other pane", "", ExtractToOther),
        ("Add to zip", "", AddToZip),
        ("Compress to zip…", "", CompressToZip),
        ("Tag…", "Ctrl+Shift+T", Library(LibCmd::TagPicker)),
        ("Toggle favorite", "Ctrl+D", Library(LibCmd::ToggleFavorite)),
    ]
    .map(|(label, shortcut, action)| Item {
        label: label.to_owned(),
        shortcut,
        action,
    })
    .into()
}

fn global_items() -> Vec<Item> {
    use Action::*;
    [
        ("Go to parent folder", "Alt+Up", Up),
        ("Back", "Alt+Left", Back),
        ("Forward", "Alt+Right", Forward),
        ("Refresh", "F5", Refresh),
        ("New tab", "Ctrl+T", NewTab),
        ("Close tab", "Ctrl+W", CloseTab),
        ("Toggle dual pane", "Ctrl+Shift+D", ToggleDual),
        ("Switch pane", "F6", SwitchPane),
        ("Toggle preview panel", "F3", TogglePreview),
        ("Toggle drop zone", "Ctrl+Shift+Z", ToggleDropZone),
        (
            "Stash selection in the drop zone",
            "Ctrl+Shift+S",
            StashSelection,
        ),
        ("Paste the drop zone here", "", StashPaste { mv: false }),
        ("Move the drop zone here", "", StashPaste { mv: true }),
        ("Clear the drop zone", "", ClearStash),
        (
            "Toggle hidden files",
            crate::keys::HIDDEN_LABEL,
            ToggleHidden,
        ),
        ("Filter this folder", "Ctrl+E", FocusFilter),
        ("Edit path", "Ctrl+L", FocusPath),
        // Not "with Everything": Windows uses Keel's own index when Everything is not running.
        ("Search files", "Ctrl+F", Search),
        ("Jump to folder or file", "Ctrl+P", JumpFolder),
        ("Rebuild folder and file lists", "", ReindexFolders),
        ("Select all", "Ctrl+A", SelectAll),
        ("Invert selection", "", InvertSelection),
        ("Paste", "Ctrl+V", Paste),
        ("Copy path", "", CopyPath),
        ("New folder", "Ctrl+Shift+N", NewFolder),
        ("New file", "", NewFile),
        ("Show in system file manager", "", RevealInSystem),
        ("Open terminal here", "", OpenTerminal),
        ("Toggle terminal", "Ctrl+`", ToggleTerminal),
        ("Toggle dark / light theme", "", ToggleTheme),
        // --- Task 46 ---
        ("Calculate sizes", "", CalculateSizes { fresh: true }),
        ("Sizes view", "", View(crate::pane::ViewMode::Sizes)),
        ("Details view", "", View(crate::pane::ViewMode::Details)),
        ("Settings", "Ctrl+,", Settings),
        // --- Task 29 ---
        ("Library overview", "", Library(LibCmd::Overview)),
        ("Add library source…", "", Library(LibCmd::AddSource)),
        ("Find duplicates", "", Library(LibCmd::Duplicates)),
        // --- Task 53 ---
        ("Back up library…", "", Library(LibCmd::Backup)),
        ("Restore library…", "", Library(LibCmd::Restore)),
        (
            "Favorites",
            "",
            Library(LibCmd::Query(crate::library::FAVORITES_QUERY.into())),
        ),
        (
            "Recents",
            "",
            Library(LibCmd::Query(crate::library::RECENTS_QUERY.into())),
        ),
    ]
    .map(|(label, shortcut, action)| Item {
        label: label.to_owned(),
        shortcut,
        action,
    })
    .into()
}

impl Palette {
    /// Opens with the global actions, plus the context ones when something is selected.
    pub fn show(&mut self, selection: bool) {
        let mut items = if selection {
            context_items()
        } else {
            Vec::new()
        };
        items.extend(self.undo.clone().map(|label| Item {
            label,
            shortcut: "Ctrl+Z",
            action: Action::Undo,
        }));
        items.extend(global_items());
        items.extend(self.profiles.iter().map(|name| Item {
            label: format!("Switch profile: {name}"),
            shortcut: "",
            action: Action::SwitchProfile(name.clone()),
        }));
        self.items = items;
        self.text.clear();
        self.open = true;
        self.focus = true;
        #[cfg(windows)]
        {
            self.shell = None;
            self.shell_asked = false;
            self.shell_built = false;
        }
        self.reindex();
    }

    /// Adds entries to the open palette (the Windows menu's "Shell: …" ones).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn add(&mut self, extra: Vec<Item>) {
        if !extra.is_empty() {
            self.items.extend(extra);
            self.reindex();
        }
    }

    fn reindex(&mut self) {
        self.fuzzy = Some(Fuzzy::new(
            self.items.iter().map(|i| i.label.clone()).collect(),
        ));
        self.filter();
    }

    pub fn filter(&mut self) {
        self.cursor = 0;
        let pat = self.text.trim();
        self.shown = match self.fuzzy.as_mut() {
            Some(f) if !pat.is_empty() => f
                .search(pat, self.items.len())
                .into_iter()
                .filter_map(|(_, label)| self.items.iter().position(|i| i.label == label))
                .collect(),
            _ => (0..self.items.len()).collect(),
        };
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Option<Action> {
        let mut out = None;
        let modal = Modal::new(Id::new("keel-palette")).show(ctx, |ui| {
            ui.set_width(520.0);
            let (up, down, enter) = ui.input_mut(|i| {
                (
                    i.count_and_consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.count_and_consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::Enter),
                )
            });
            self.cursor = (self.cursor + down)
                .saturating_sub(up)
                .min(self.shown.len().saturating_sub(1));
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.text)
                    .hint_text("Type a command")
                    .desired_width(f32::INFINITY),
            );
            if std::mem::take(&mut self.focus) || !r.has_focus() {
                r.request_focus();
            }
            if r.changed() {
                self.filter();
                #[cfg(windows)]
                if !self.shell_built && wants_shell(&self.text) {
                    self.shell_built = true;
                    self.shell_asked = true;
                }
            }
            if enter {
                out = self
                    .shown
                    .get(self.cursor)
                    .map(|&i| self.items[i].action.clone());
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (row, &i) in self.shown.iter().enumerate() {
                        let item = &self.items[i];
                        let button = egui::Button::new(item.label.as_str())
                            .shortcut_text(crate::keys::shortcut_label(item.shortcut))
                            .selected(row == self.cursor)
                            .frame(row == self.cursor)
                            .min_size(egui::vec2(ui.available_width(), 0.0));
                        let r = ui.add(button);
                        if row == self.cursor && (up + down) > 0 {
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            out = Some(item.action.clone());
                        }
                    }
                });
        });
        if modal.should_close() || out.is_some() {
            self.open = false;
            // The menu stays only for the Shell command that runs on it.
            #[cfg(windows)]
            if !matches!(out, Some(Action::ShellCommand(_))) {
                self.shell = None;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shell extensions load only once "shell" is typed; their entries join the open
    /// palette and are found like the others.
    #[test]
    fn shell_entries_wait_for_shell_typed() {
        assert!(wants_shell("Shell: Open") && wants_shell("  shell"));
        assert!(!wants_shell("she") && !wants_shell("copy") && !wants_shell(""));
        let mut p = Palette::default();
        p.show(false);
        let n = p.items.len();
        p.add(vec![Item {
            label: "Shell: Open with Notepad".into(),
            shortcut: "",
            action: Action::Undo,
        }]);
        assert_eq!(p.items.len(), n + 1);
        p.text = "shell: notepad".into();
        p.filter();
        assert_eq!(p.items[p.shown[0]].label, "Shell: Open with Notepad");
        p.show(false);
        assert_eq!(p.items.len(), n, "a new opening starts without them");
    }

    #[test]
    fn filter_dual_finds_toggle_dual() {
        let mut p = Palette::default();
        p.show(false);
        assert!(p.shown.len() > 20, "every action listed");
        p.text = "dual".into();
        p.filter();
        let actions: Vec<&Action> = p.shown.iter().map(|&i| &p.items[i].action).collect();
        assert_eq!(actions.first(), Some(&&Action::ToggleDual));
    }

    /// QA walkthrough 2026-10-10: on Windows the entry said "Search with Everything", also
    /// when Everything was not running and Keel's own index answered.
    #[test]
    fn search_is_named_for_every_backend() {
        let mut p = Palette::default();
        p.show(false);
        let search = p.items.iter().find(|i| i.action == Action::Search).unwrap();
        assert_eq!(search.label, "Search files");
    }

    #[test]
    fn other_profiles_are_listed() {
        let mut p = Palette {
            profiles: vec!["work".into()],
            ..Palette::default()
        };
        p.show(false);
        p.text = "switch profile".into();
        p.filter();
        assert_eq!(
            p.items[p.shown[0]].action,
            Action::SwitchProfile("work".into())
        );
        assert_eq!(p.items[p.shown[0]].label, "Switch profile: work");
    }

    #[test]
    fn selection_adds_context_actions() {
        let mut p = Palette::default();
        p.show(true);
        p.text = "open location".into();
        p.filter();
        assert_eq!(p.items[p.shown[0]].action, Action::OpenLocation);
    }
}
