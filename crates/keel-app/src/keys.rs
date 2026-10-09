//! Keyboard map: one pass over the frame's input producing `Action`s.

use crate::tab::Nav;
use egui::{Event, Key, Modifiers};
use keel_vfs::VPath;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Parent folder.
    Up,
    Back,
    Forward,
    Refresh,
    NewTab,
    CloseTab,
    ToggleDual,
    ToggleHidden,
    FocusFilter,
    /// Esc: close the filter box / stop renaming.
    ClearFilter,
    /// Filter-on-typing: a printable character typed with the list focused.
    Type(String),
    FocusPath,
    /// Open the cursor entry (dir: navigate, file: OS default app).
    Enter,
    Move(Nav, bool),
    ToggleSelect,
    SelectAll,
    InvertSelection,
    SwitchPane,
    /// Start inline rename (F2).
    Rename,
    /// Commit an inline rename. Task 6 executes it.
    RenameTo {
        from: VPath,
        to: String,
    },
    Delete,
    Copy,
    Cut,
    Paste,
    NewFolder,
    NewFile,
    CopyPath,
    OpenWith,
    Properties,
    RevealInSystem,
    OpenTerminal,
    ToggleTheme,
    Navigate(VPath),
    NewTabAt(VPath),
    /// Task 7.
    Search,
    JumpFolder,
    Palette,
    TogglePreview,
}

const CMD: Modifiers = Modifiers::COMMAND;
const CMD_SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::COMMAND
};

/// Shortcut table; more specific modifier sets come first because egui ignores
/// unrequested Shift/Alt when matching.
const SHORTCUTS: &[(Modifiers, Key, Action)] = &[
    (CMD_SHIFT, Key::P, Action::Palette),
    (CMD_SHIFT, Key::D, Action::ToggleDual),
    (CMD_SHIFT, Key::N, Action::NewFolder),
    (CMD_SHIFT, Key::V, Action::TogglePreview),
    (CMD, Key::P, Action::JumpFolder),
    (CMD, Key::F, Action::Search),
    (CMD, Key::E, Action::FocusFilter),
    (CMD, Key::L, Action::FocusPath),
    (CMD, Key::T, Action::NewTab),
    (CMD, Key::W, Action::CloseTab),
    (CMD, Key::A, Action::SelectAll),
    (CMD, Key::H, Action::ToggleHidden),
    (Modifiers::ALT, Key::ArrowUp, Action::Up),
    (Modifiers::ALT, Key::ArrowLeft, Action::Back),
    (Modifiers::ALT, Key::ArrowRight, Action::Forward),
    (Modifiers::NONE, Key::Backspace, Action::Up),
    (Modifiers::NONE, Key::F2, Action::Rename),
    (Modifiers::NONE, Key::F3, Action::TogglePreview),
    (Modifiers::NONE, Key::F5, Action::Refresh),
    (Modifiers::NONE, Key::F6, Action::SwitchPane),
    (Modifiers::NONE, Key::Delete, Action::Delete),
    (Modifiers::NONE, Key::Enter, Action::Enter),
    (Modifiers::NONE, Key::Space, Action::ToggleSelect),
    (Modifiers::NONE, Key::Escape, Action::ClearFilter),
];

const MOVES: &[(Key, Nav)] = &[
    (Key::ArrowUp, Nav::Prev),
    (Key::ArrowDown, Nav::Next),
    (Key::ArrowLeft, Nav::Left),
    (Key::ArrowRight, Nav::Right),
    (Key::PageUp, Nav::PageUp),
    (Key::PageDown, Nav::PageDown),
    (Key::Home, Nav::Home),
    (Key::End, Nav::End),
];

/// Actions for this frame. Empty while a text field has focus (rename, filter, path box).
pub fn actions(ctx: &egui::Context) -> Vec<Action> {
    if ctx.wants_keyboard_input() {
        return Vec::new();
    }
    ctx.input_mut(|i| {
        let mut out = Vec::new();
        for (mods, key, action) in SHORTCUTS {
            if i.consume_key(*mods, *key) {
                out.push(action.clone());
            }
        }
        for (key, nav) in MOVES {
            let extend = i.modifiers.shift;
            // Counted: a slow frame can carry several key repeats.
            for _ in 0..i.count_and_consume_key(Modifiers::NONE, *key) {
                out.push(Action::Move(*nav, extend));
            }
        }
        let typing_allowed = !i.modifiers.command && !i.modifiers.alt;
        for event in &i.events {
            match event {
                Event::Copy => out.push(Action::Copy),
                Event::Cut => out.push(Action::Cut),
                Event::Paste(_) => out.push(Action::Paste),
                // Space toggles selection; it never starts a filter.
                Event::Text(t) if typing_allowed && !t.trim().is_empty() => {
                    out.push(Action::Type(t.clone()))
                }
                _ => {}
            }
        }
        out
    })
}
