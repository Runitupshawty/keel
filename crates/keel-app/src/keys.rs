//! Keyboard map: one pass over the frame's input producing `Action`s.

use crate::jobs::{ArchiveSrc, Transfer};
use crate::tab::Nav;
use egui::{Event, Key, Modifiers};
use keel_vfs::{Conflict, VPath};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Parent folder (Alt+Up, toolbar).
    Up,
    /// Parent folder, or in a search tab: delete the last query character.
    Backspace,
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
    /// Confirmed: delete these on their remote host (no trash there).
    DeleteRemote(Vec<VPath>),
    /// Sidebar command on a configured remote host (by id).
    Remote {
        host: String,
        cmd: crate::remotes::RemoteCmd,
    },
    /// A transfer whose conflict policy is decided.
    StartTransfer {
        op: Transfer,
        conflict: Conflict,
        from_clipboard: bool,
    },
    /// Files dropped on `dst`: from another app (`from` None) or dragged from a pane
    /// (`from` = that pane and its folder).
    Drop {
        paths: Vec<VPath>,
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
    ToggleTerminal,
    LeaveTerminal,
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
    /// Settings window (Ctrl+,).
    Settings,
    /// Archive targets: extract next to the archive.
    ExtractHere,
    /// Archive targets: extract into a new `<name>/` folder next to each archive.
    ExtractToFolder,
    /// Archive targets: extract into a folder picked with the OS dialog.
    ExtractTo,
    /// Extract entries dragged out of an archive onto `dst`.
    Extract {
        src: ArchiveSrc,
        dst: VPath,
    },
    /// Targets: add to `<name>.zip` in this folder.
    AddToZip,
    /// Targets: ask for a zip name, then `ZipTo`.
    CompressToZip,
    ZipTo {
        zip: PathBuf,
        src: Vec<PathBuf>,
    },
    /// Show tab `tab` of pane `pane` (sidebar "Open archives").
    FocusTab {
        pane: usize,
        tab: usize,
    },
}

const CMD: Modifiers = Modifiers::COMMAND;
const CMD_SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::COMMAND
};

/// Shortcut table; more specific modifier sets come first because egui ignores
/// unrequested Shift/Alt when matching.
const SHORTCUTS: &[(Modifiers, Key, Action)] = &[
    (CMD, Key::Backtick, Action::ToggleTerminal),
    (CMD_SHIFT, Key::P, Action::Palette),
    (CMD_SHIFT, Key::D, Action::ToggleDual),
    (CMD_SHIFT, Key::N, Action::NewFolder),
    (CMD, Key::P, Action::JumpFolder),
    (CMD, Key::Enter, Action::OpenLocation),
    (CMD, Key::F, Action::Search),
    (CMD, Key::E, Action::FocusFilter),
    (CMD, Key::L, Action::FocusPath),
    (CMD, Key::T, Action::NewTab),
    (CMD, Key::W, Action::CloseTab),
    (CMD, Key::A, Action::SelectAll),
    (CMD, Key::H, Action::ToggleHidden),
    (CMD, Key::Comma, Action::Settings),
    (Modifiers::ALT, Key::ArrowUp, Action::Up),
    (Modifiers::ALT, Key::ArrowLeft, Action::Back),
    (Modifiers::ALT, Key::ArrowRight, Action::Forward),
    (Modifiers::NONE, Key::Backspace, Action::Backspace),
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
    Action::ToggleTerminal,
    Action::Palette,
    Action::JumpFolder,
    Action::Search,
    Action::ToggleDual,
    Action::TogglePreview,
    Action::NewTab,
    Action::CloseTab,
    Action::Settings,
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

pub fn actions_with_terminal(ctx: &egui::Context, enabled: bool, terminal: bool) -> Vec<Action> {
    if !terminal {
        return actions(ctx, enabled);
    }
    // Track swallowed clipboard presses even while the terminal owns the events.
    // Their later releases must never become a file paste after focus leaves.
    let _ = paste_pressed(ctx);
    if !enabled {
        return Vec::new();
    }
    ctx.input_mut(|i| {
        if i.consume_key(CMD, Key::Backtick) {
            return vec![Action::ToggleTerminal];
        }
        // Plain Esc belongs to the shell's programs (vim, less, fzf, PSReadLine).
        if i.consume_key(Modifiers::NONE, Key::F6) || i.consume_key(Modifiers::SHIFT, Key::Escape) {
            // Discard this frame's text/key events on focus transfer.
            i.events.retain(|e| !crate::term_pane::keyboard_event(e));
            return vec![Action::LeaveTerminal];
        }
        Vec::new()
    })
}

/// Actions for this frame. Empty while a text field has focus (rename, filter, path box);
/// a focused button (after Tab) does not block the key map. Call every frame: with
/// `enabled` false (a modal is open) nothing is returned, but V presses are still tracked
/// so a V held while the modal closes cannot turn into a paste.
pub fn actions(ctx: &egui::Context, enabled: bool) -> Vec<Action> {
    // Tracked even while typing, so a key released inside a text box cannot confuse it.
    let paste = paste_pressed(ctx);
    if !enabled {
        return Vec::new();
    }
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
    let mut out = ctx.input_mut(|i| {
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
                // egui-winit turns Shift+Delete into Cut on Windows; Phase 1 trashes.
                Event::Cut if i.modifiers.shift && !i.modifiers.command => out.push(Action::Delete),
                Event::Cut => out.push(Action::Cut),
                // Space toggles selection; it never starts a filter.
                Event::Text(t) if typing_allowed && !t.trim().is_empty() => {
                    out.push(Action::Type(t.clone()))
                }
                _ => {}
            }
        }
        out
    });
    if paste {
        out.push(Action::Paste);
    }
    out
}

/// Ctrl+V (or Shift+Insert with text on the clipboard), once per key press. egui-winit
/// swallows the press of a paste shortcut and only sends `Event::Paste`, and only when the
/// clipboard holds text. So a V release whose press never arrived as a key event was a
/// paste press, whatever the modifiers are by the time of the release. egui-winit treats
/// Ctrl+Shift+V as a paste too: a V release or `Event::Paste` with Shift and Command held
/// is ignored (Shift+Insert, Shift without Command, still pastes). The swallowed press
/// carries no modifiers, so Command+Shift seen in any frame since Command went down (and
/// since the last V release) marks the coming V release as Ctrl+Shift+V even when Shift is
/// released first. Known ceiling: Ctrl+Shift held, Shift released, then V does not paste.
fn paste_pressed(ctx: &egui::Context) -> bool {
    let id = egui::Id::new("keel-paste-keys");
    // (a V/Insert press arrived as a plain key event, an Event::Paste already fired,
    // Command+Shift was held since Command went down)
    let (mut press_seen, mut pasted, mut shift_armed) = ctx
        .data(|d| d.get_temp::<(bool, bool, bool)>(id))
        .unwrap_or_default();
    let mut paste = false;
    ctx.input(|i| {
        let shift_cmd = i.modifiers.shift && i.modifiers.command;
        shift_armed = i.modifiers.command && (shift_armed || shift_cmd);
        for event in &i.events {
            match event {
                Event::Paste(_) if !pasted => {
                    pasted = true;
                    paste = !shift_cmd;
                }
                Event::Key {
                    key: Key::V,
                    pressed,
                    modifiers,
                    ..
                } => {
                    if *pressed {
                        press_seen = true;
                    } else {
                        paste |= !press_seen && !pasted && !modifiers.shift && !shift_armed;
                        (press_seen, pasted, shift_armed) = (false, false, false);
                    }
                }
                // Shift+Insert pastes only through `Event::Paste` (text clipboards); an
                // unseen Insert press may also be Ctrl+Insert (copy), so it never pastes.
                Event::Key {
                    key: Key::Insert,
                    pressed: false,
                    ..
                } => pasted = false,
                _ => {}
            }
        }
    });
    ctx.data_mut(|d| d.insert_temp(id, (press_seen, pasted, shift_armed)));
    paste
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_focus_suppresses_file_shortcuts_and_clipboard_release() {
        let ctx = egui::Context::default();
        for events in [
            vec![Event::Copy, Event::Cut, Event::Paste("text".into())],
            vec![v(false, CMD)],
        ] {
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    assert!(actions_with_terminal(ctx, true, true).is_empty());
                },
            );
        }
    }

    /// M7: only F6 / Shift+Esc leave the terminal; plain Esc stays for the PTY.
    #[test]
    fn terminal_keeps_plain_escape() {
        let ctx = egui::Context::default();
        let esc = |modifiers| Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let run = |event: Event| {
            let (mut actions, mut left) = (Vec::new(), 0);
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![event],
                    ..Default::default()
                },
                |ctx| {
                    actions = actions_with_terminal(ctx, true, true);
                    left = ctx.input(|i| i.events.len());
                },
            );
            (actions, left)
        };
        assert_eq!(run(esc(Modifiers::NONE)), (vec![], 1));
        assert_eq!(run(esc(Modifiers::SHIFT)), (vec![Action::LeaveTerminal], 0));
        let f6 = Event::Key {
            key: Key::F6,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(run(f6), (vec![Action::LeaveTerminal], 0));
    }

    fn frame(ctx: &egui::Context, events: Vec<Event>) -> bool {
        frame_with(ctx, Modifiers::NONE, events)
    }

    /// One frame with `modifiers` held (egui-winit's modifier state for that frame).
    fn frame_with(ctx: &egui::Context, modifiers: Modifiers, events: Vec<Event>) -> bool {
        let mut paste = false;
        let _ = ctx.run(
            egui::RawInput {
                events,
                modifiers,
                ..Default::default()
            },
            |ctx| paste = paste_pressed(ctx),
        );
        paste
    }

    fn v(pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key {
            key: Key::V,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn swallowed_ctrl_v_pastes_once_on_release() {
        let ctx = egui::Context::default();
        // Files on the clipboard: egui-winit swallows the press, only the release arrives,
        // and Ctrl may already be up by then.
        assert!(frame(&ctx, vec![v(false, Modifiers::NONE)]));
        // Text on the clipboard: Event::Paste on press, the release adds nothing.
        assert!(frame(&ctx, vec![Event::Paste("x".into())]));
        assert!(!frame(&ctx, vec![v(false, Modifiers::COMMAND)]));
        // Typing a plain "v" is not a paste, and leaves no state behind.
        assert!(!frame(&ctx, vec![v(true, Modifiers::NONE)]));
        assert!(!frame(&ctx, vec![v(false, Modifiers::NONE)]));
        assert!(frame(&ctx, vec![v(false, Modifiers::COMMAND)]));
        // Ctrl+Shift+V (swallowed by egui-winit like Ctrl+V) never pastes.
        let cmd_shift = Modifiers {
            shift: true,
            ..Modifiers::COMMAND
        };
        assert!(!frame(&ctx, vec![v(false, cmd_shift)]));

        // Ctrl+Shift+V with Shift released before V: the press frame saw Command+Shift.
        assert!(!frame_with(&ctx, cmd_shift, vec![]));
        assert!(!frame_with(&ctx, CMD, vec![]));
        assert!(!frame_with(&ctx, CMD, vec![v(false, CMD)]));
        // Ctrl still held: the next Ctrl+V pastes again.
        assert!(frame_with(&ctx, CMD, vec![v(false, CMD)]));
        // Command released in between clears it too.
        assert!(!frame_with(&ctx, cmd_shift, vec![]));
        assert!(!frame_with(&ctx, Modifiers::NONE, vec![]));
        assert!(!frame_with(&ctx, CMD, vec![]));
        assert!(frame_with(&ctx, CMD, vec![v(false, CMD)]));
    }

    #[test]
    fn v_typed_in_a_modal_does_not_paste_after_it_closes() {
        let ctx = egui::Context::default();
        let run = |events: Vec<Event>, enabled: bool| {
            let mut out = Vec::new();
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| out = actions(ctx, enabled),
            );
            out
        };
        // "v" pressed while Ctrl+P is open; Enter closes it; the release arrives after.
        assert!(run(vec![v(true, Modifiers::NONE)], false).is_empty());
        assert!(!run(vec![v(false, Modifiers::NONE)], true).contains(&Action::Paste));
    }
}
