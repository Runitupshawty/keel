//! Keyboard map: one pass over the frame's input producing `Action`s.

use crate::jobs::Transfer;
use crate::tab::Nav;
use egui::{Event, Key, Modifiers};
use keel_vfs::{Conflict, VPath};
use std::path::PathBuf;

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
    /// Commit an inline rename.
    RenameTo {
        from: VPath,
        to: String,
    },
    /// Asks before trashing the targets.
    Delete,
    /// Confirmed: send these to the OS trash.
    Trash(Vec<VPath>),
    /// A transfer whose conflict policy is decided.
    StartTransfer {
        op: Transfer,
        conflict: Conflict,
        from_clipboard: bool,
    },
    /// Files dropped on `dst`: from another app (`from` None) or dragged from a pane
    /// (`from` = that pane and its folder).
    Drop {
        paths: Vec<PathBuf>,
        from: Option<(usize, VPath)>,
        dst: VPath,
    },
    /// New folder / file dialog answered.
    Create {
        dir: VPath,
        name: String,
        folder: bool,
    },
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
    /// Open the cursor entry's folder with the entry selected (other pane, or a new tab
    /// when single-pane).
    OpenLocation,
    Search,
    JumpFolder,
    /// Rebuild the Ctrl+P folder index (F5 in the popup).
    ReindexFolders,
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
    (CMD, Key::Enter, Action::OpenLocation),
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

/// Shortcuts that never mean anything to a focused text box.
const WHILE_TYPING: &[Action] = &[
    Action::Palette,
    Action::JumpFolder,
    Action::Search,
    Action::ToggleDual,
    Action::TogglePreview,
    Action::NewTab,
    Action::CloseTab,
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

/// Actions for this frame. Empty while a text field has focus (rename, filter, path box);
/// a focused button (after Tab) does not block the key map.
pub fn actions(ctx: &egui::Context) -> Vec<Action> {
    let typing = ctx
        .memory(|m| m.focused())
        .is_some_and(|id| egui::TextEdit::load_state(ctx, id).is_some());
    if typing {
        // Window-level shortcuts still work from a text box (e.g. Ctrl+P from the search box).
        return ctx.input_mut(|i| {
            SHORTCUTS
                .iter()
                .filter(|(_, _, a)| WHILE_TYPING.contains(a))
                .filter(|(mods, key, _)| i.consume_key(*mods, *key))
                .map(|(_, _, a)| a.clone())
                .collect()
        });
    }
    let (mut out, paste_event, v_released) = ctx.input_mut(|i| {
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
        let (mut paste_event, mut v_released) = (false, false);
        for event in &i.events {
            match event {
                Event::Copy => out.push(Action::Copy),
                // egui-winit turns Shift+Delete into Cut on Windows; Phase 1 trashes.
                Event::Cut if i.modifiers.shift && !i.modifiers.command => out.push(Action::Delete),
                Event::Cut => out.push(Action::Cut),
                Event::Paste(_) => paste_event = true,
                Event::Key {
                    key: Key::V,
                    pressed: false,
                    modifiers,
                    ..
                } if modifiers.command => v_released = true,
                // Space toggles selection; it never starts a filter.
                Event::Text(t) if typing_allowed && !t.trim().is_empty() => {
                    out.push(Action::Type(t.clone()))
                }
                _ => {}
            }
        }
        (out, paste_event, v_released)
    });
    // egui-winit only sends `Event::Paste` when the clipboard holds text, so files on the
    // clipboard show up only as the Ctrl+V key release. Paste once per press either way.
    let seen = egui::Id::new("keel-paste-event");
    if paste_event {
        ctx.data_mut(|d| d.insert_temp(seen, true));
        out.push(Action::Paste);
    }
    if v_released
        && !ctx
            .data_mut(|d| d.remove_temp::<bool>(seen))
            .unwrap_or(false)
    {
        out.push(Action::Paste);
    }
    out
}
