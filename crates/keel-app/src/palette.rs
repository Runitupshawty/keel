//! Ctrl+Shift+P: every action by name, plus the context actions for the selection,
//! fuzzy filtered. A few dozen labels match instantly, so this runs on the UI thread.

use crate::keymap::{Bindings, Pal};
use crate::keys::Action;
use crate::profiles::SessionCmd;
use egui::{Id, Key, Modal, Modifiers};
use keel_search::Fuzzy;

pub struct Item {
    pub label: String,
    /// Its keys as bound now ("" = none).
    pub shortcut: String,
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
    /// The profile's named sessions: "Open session: <name>", "Delete session: <name>".
    pub sessions: Vec<String>,
    /// "Undo <what>" for the newest file operation, set before `show`.
    pub undo: Option<String>,
    /// The active tab is a search tab: its toggles and Load more are offered (set before
    /// `show`).
    pub search: bool,
    /// Task 85: the user-defined cleanups' titles ("Run cleanup: <title>"), set before
    /// `show`.
    pub cleanups: Vec<String>,
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

/// The palette's entries from the command table (`keymap::commands`): the selection's
/// first when there is one, then a search tab's (its toggles and Load more, in a search
/// tab only), then the global ones, each with its keys as bound now.
fn entries(selection: bool, search: bool, keys: &Bindings) -> [Vec<Item>; 3] {
    let (mut context, mut searching, mut global) = (Vec::new(), Vec::new(), Vec::new());
    for c in crate::keymap::commands() {
        let list = match c.palette {
            Pal::Context if selection => &mut context,
            Pal::Search if search => &mut searching,
            Pal::Global => &mut global,
            _ => continue,
        };
        list.push(Item {
            label: crate::i18n::command(c.id, c.label).to_owned(),
            shortcut: keys.label(&c.action),
            action: c.action,
        });
    }
    [context, searching, global]
}

impl Palette {
    /// Opens with the global actions, plus the context ones when something is selected.
    pub fn show(&mut self, selection: bool, keys: &Bindings) {
        let [mut items, searching, global] = entries(selection, self.search, keys);
        items.extend(self.undo.clone().map(|label| Item {
            label,
            shortcut: keys.label(&Action::Undo),
            action: Action::Undo,
        }));
        items.extend(searching);
        items.extend(global);
        // Task 85: Delete permanently (with a selection), system cleanups, launchers.
        items.extend(crate::system_cleanup::palette_items(selection));
        items.extend(self.cleanups.iter().enumerate().map(|(i, title)| Item {
            label: crate::i18n::tr("palette.run_cleanup").replace("{title}", title),
            shortcut: String::new(),
            action: Action::RunCleanup(i),
        }));
        items.extend(self.profiles.iter().map(|name| Item {
            label: crate::i18n::tr("palette.switch_profile").replace("{name}", name),
            shortcut: String::new(),
            action: Action::SwitchProfile(name.clone()),
        }));
        let tr = crate::i18n::tr;
        items.push(Item {
            label: tr("palette.save_session").to_owned(),
            shortcut: String::new(),
            action: Action::Session(SessionCmd::SaveAs),
        });
        for name in &self.sessions {
            items.extend([
                Item {
                    label: tr("palette.open_session").replace("{name}", name),
                    shortcut: String::new(),
                    action: Action::Session(SessionCmd::Open(name.clone())),
                },
                Item {
                    label: tr("palette.delete_session").replace("{name}", name),
                    shortcut: String::new(),
                    action: Action::Session(SessionCmd::Delete(name.clone())),
                },
            ]);
        }
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
                    .hint_text(crate::i18n::tr("palette.hint"))
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
                            .shortcut_text(item.shortcut.as_str())
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
        p.show(false, &Bindings::default());
        let n = p.items.len();
        p.add(vec![Item {
            label: "Shell: Open with Notepad".into(),
            shortcut: String::new(),
            action: Action::Undo,
        }]);
        assert_eq!(p.items.len(), n + 1);
        p.text = "shell: notepad".into();
        p.filter();
        assert_eq!(p.items[p.shown[0]].label, "Shell: Open with Notepad");
        p.show(false, &Bindings::default());
        assert_eq!(p.items.len(), n, "a new opening starts without them");
    }

    #[test]
    fn filter_dual_finds_toggle_dual() {
        let mut p = Palette::default();
        p.show(false, &Bindings::default());
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
        p.show(false, &Bindings::default());
        let search = p.items.iter().find(|i| i.action == Action::Search).unwrap();
        assert_eq!(search.label, "Search files");
    }

    /// The palette shows the keys as bound now, not the defaults.
    #[test]
    fn shortcuts_follow_the_keymap() {
        let mut p = Palette::default();
        p.show(false, &Bindings::default());
        let refresh = |p: &Palette| {
            let i = p
                .items
                .iter()
                .find(|i| i.action == Action::Refresh)
                .unwrap();
            i.shortcut.clone()
        };
        assert_eq!(refresh(&p), "F5");
        let mut map = crate::keymap::Keymap::default();
        let f9 = crate::keymap::Chord::parse("F9").unwrap();
        map.bind("view.refresh", f9, false).unwrap();
        p.show(false, &map.bindings());
        assert_eq!(refresh(&p), "F5, F9");
        let paste = p.items.iter().find(|i| i.action == Action::Paste).unwrap();
        assert_eq!(paste.shortcut, crate::keys::shortcut_label("Ctrl+V"));
    }

    #[test]
    fn other_profiles_are_listed() {
        let mut p = Palette {
            profiles: vec!["work".into()],
            ..Palette::default()
        };
        p.show(false, &Bindings::default());
        p.text = "switch profile".into();
        p.filter();
        assert_eq!(
            p.items[p.shown[0]].action,
            Action::SwitchProfile("work".into())
        );
        assert_eq!(p.items[p.shown[0]].label, "Switch profile: work");
    }

    #[test]
    fn named_sessions_are_listed() {
        let mut p = Palette {
            sessions: vec!["photos".into()],
            ..Palette::default()
        };
        p.show(false, &Bindings::default());
        for (text, action) in [
            ("save session", SessionCmd::SaveAs),
            ("open session", SessionCmd::Open("photos".into())),
            ("delete session", SessionCmd::Delete("photos".into())),
        ] {
            p.text = text.into();
            p.filter();
            assert_eq!(
                p.items[p.shown[0]].action,
                Action::Session(action),
                "{text}"
            );
        }
    }

    #[test]
    fn selection_adds_context_actions() {
        let mut p = Palette::default();
        p.show(true, &Bindings::default());
        p.text = "open location".into();
        p.filter();
        assert_eq!(p.items[p.shown[0]].action, Action::OpenLocation);
    }
}
