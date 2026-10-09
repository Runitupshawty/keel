//! Ctrl+Shift+P: every action by name, plus the context actions for the selection,
//! fuzzy filtered. A few dozen labels match instantly, so this runs on the UI thread.

use crate::keys::Action;
use egui::{Id, Key, Modal, Modifiers};
use keel_search::Fuzzy;

pub struct Item {
    pub label: &'static str,
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
    focus: bool,
    fuzzy: Option<Fuzzy>,
}

const SEARCH_LABEL: &str = if cfg!(windows) {
    "Search with Everything"
} else {
    "Search files"
};

/// Actions on the selection (shown first when something is selected).
fn context_items() -> Vec<Item> {
    use Action::*;
    [
        ("Open", "Enter", Enter),
        ("Open with…", "", OpenWith),
        ("Open location", "Ctrl+Enter", OpenLocation),
        ("Copy", "Ctrl+C", Copy),
        ("Cut", "Ctrl+X", Cut),
        ("Rename", "F2", Rename),
        ("Move to trash", "Del", Delete),
        ("Properties", "", Properties),
    ]
    .map(|(label, shortcut, action)| Item {
        label,
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
        ("Toggle hidden files", "Ctrl+H", ToggleHidden),
        ("Filter this folder", "Ctrl+E", FocusFilter),
        ("Edit path", "Ctrl+L", FocusPath),
        (SEARCH_LABEL, "Ctrl+F", Search),
        ("Jump to folder", "Ctrl+P", JumpFolder),
        ("Rebuild folder index", "", ReindexFolders),
        ("Select all", "Ctrl+A", SelectAll),
        ("Invert selection", "", InvertSelection),
        ("Paste", "Ctrl+V", Paste),
        ("Copy path", "", CopyPath),
        ("New folder", "Ctrl+Shift+N", NewFolder),
        ("New file", "", NewFile),
        ("Show in system file manager", "", RevealInSystem),
        ("Open terminal here", "", OpenTerminal),
        ("Toggle dark / light theme", "", ToggleTheme),
        ("Settings", "Ctrl+,", Settings),
    ]
    .map(|(label, shortcut, action)| Item {
        label,
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
        items.extend(global_items());
        self.fuzzy = Some(Fuzzy::new(
            items.iter().map(|i| i.label.to_owned()).collect(),
        ));
        self.items = items;
        self.text.clear();
        self.open = true;
        self.focus = true;
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
                        let button = egui::Button::new(item.label)
                            .shortcut_text(item.shortcut)
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
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn selection_adds_context_actions() {
        let mut p = Palette::default();
        p.show(true);
        p.text = "open location".into();
        p.filter();
        assert_eq!(p.items[p.shown[0]].action, Action::OpenLocation);
    }
}
