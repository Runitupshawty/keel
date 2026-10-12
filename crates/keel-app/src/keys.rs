//! Keyboard map: one pass over the frame's input producing `Action`s.

use crate::jobs::{ArchiveSrc, Transfer};
use crate::keymap::Bindings;
use crate::tab::Nav;
use egui::{Event, Key};
use keel_vfs::{Conflict, VPath};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Parent folder (Alt+Up, toolbar).
    Up,
    /// Task 66: show or hide the View Options panel (Ctrl+J).
    ViewOptions,
    /// Turn the daily update check on (the first-start question).
    EnableUpdateCheck,
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
    /// Bulk rename dialog for the selection (Ctrl+F2).
    BulkRename,
    /// The dialog answered: `(item, new name)` pairs, already checked.
    BulkRenameApply {
        renames: Vec<(VPath, String)>,
    },
    /// Ctrl+Z, the palette's "Undo <what>": undo the newest file operation (`undo.rs`).
    Undo,
    /// A toast's Undo: undo this operation (`undo::Entry::id`), checked first.
    UndoEntry(u64),
    /// The undo's preview was confirmed (or a rename's check passed): run it.
    UndoRun(u64),
    /// Asks before trashing the targets.
    Delete,
    /// Confirmed: send these to the OS trash.
    Trash(Vec<VPath>),
    /// Confirmed: delete these for good (a remote host, a cloud, a WSL folder: no trash
    /// there).
    DeleteRemote(Vec<VPath>),
    /// Task 85: Delete permanently, hard-link dedup, system cleanups and launchers
    /// (`system_cleanup`). Never on a key.
    System(crate::system_cleanup::SysCmd),
    /// Sidebar: open this WSL distribution (started first when stopped), on a worker.
    OpenWsl(String),
    /// Sidebar command on a configured remote host (by id).
    Remote {
        host: String,
        cmd: crate::remotes::RemoteCmd,
    },
    /// Sidebar, settings or toast command on a cloud account (by id).
    Cloud {
        id: String,
        cmd: crate::clouds::CloudCmd,
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
    /// Copy the targets' names (a search tab: the file names, not the full paths).
    CopyName,
    /// A search toggle (Match Case, Whole Words, Path, Diacritics, Regex) in a search tab:
    /// flip it and search again.
    SearchToggle(crate::search_tab::Toggle),
    /// A search tab: ask for more results (up to the Most results setting).
    SearchMore,
    /// A share link for this cloud item (the one right-clicked), on the clipboard
    /// (`clouds::can_link`).
    CopyLink(VPath),
    /// Open with… (Ctrl+Shift+O): Keel's picker for the targets.
    OpenWith,
    /// Open `paths` with `app`; `remember` puts it in the recent list for their extension.
    OpenWithApp {
        paths: Vec<PathBuf>,
        app: String,
        remember: bool,
    },
    /// The picker's Browse…: choose an executable or .app, then as `OpenWithApp`.
    OpenWithBrowse {
        paths: Vec<PathBuf>,
        remember: bool,
    },
    /// Windows: the system "Open with" chooser for the first of `paths`.
    OpenWithSystem(PathBuf),
    /// A recent app from the context menu, on the current targets.
    OpenWithRecent(String),
    /// Get Info (Task 67): an info window for the selection (several can be open).
    Properties,
    /// Task 67: the info window that follows the selection (Alt+Ctrl+I), on or off.
    InfoFollow,
    /// Task 67: Get Info's Locked box: read-only on or off for a local item.
    SetLocked(PathBuf, bool),
    /// Explorer's own Properties sheet (Windows).
    #[cfg_attr(not(windows), allow(dead_code))]
    ShellProperties(PathBuf),
    /// "More options": the Windows shell menu for the targets (`shell_menu`). `extended`:
    /// the Shift verbs too; `keyboard`: at the cursor row instead of the pointer.
    #[cfg(windows)]
    ShellMenu {
        extended: bool,
        keyboard: bool,
    },
    /// A palette "Shell: …" entry: command `id` of the menu the palette built.
    #[cfg(windows)]
    ShellCommand(u32),
    RevealInSystem,
    OpenTerminal,
    ToggleTerminal,
    LeaveTerminal,
    ToggleTheme,
    /// Task 46: walk the shown folder's subfolders for their totals (a job). `fresh`:
    /// asked by the user (totals walked before are walked again); else only folders without
    /// a total, once per listing.
    CalculateSizes {
        fresh: bool,
    },
    /// Switch the pane's view (palette).
    View(crate::pane::ViewMode),
    Navigate(VPath),
    /// Path box: a folder navigates; a file opens its folder with the file selected.
    OpenPath(VPath),
    NewTabAt(VPath),
    /// Open the cursor entry's folder with the entry selected (other pane, or a new tab
    /// when single-pane).
    OpenLocation,
    Search,
    QuickSearch,
    OmitResults(bool),
    ManageOmissions,
    ShowOmitted,
    ToggleOnTop,
    /// A new search tab in the pane running this query (the Disk Usage window's
    /// "Search for .ext").
    SearchFor(String),
    JumpFolder,
    /// A Ctrl+P result: a folder opens; a file opens its folder with it selected and the
    /// preview shown. In a new tab, or in the other pane (a new tab when single-pane).
    JumpTo {
        path: VPath,
        file: bool,
        new_tab: bool,
        other_pane: bool,
    },
    /// Read Ctrl+P's folder and file lists again (F5 in the popup).
    ReindexFolders,
    /// Open the index journal in a tab (Phase 15).
    OpenJournal,
    Palette,
    TogglePreview,
    /// Task 59: the preview panel's Inspector tab (Ctrl+I).
    ToggleInspector,
    /// Settings window (Ctrl+,).
    Settings,
    /// Start another Keel window (`keel --new-window`, same profile).
    NewWindow,
    // --- Task 85 ---
    /// Settings → Cleanups.
    CleanupSettings,
    /// Run user-defined cleanup `n` (of the profile's list) on the targets.
    RunCleanup(usize),
    /// A cleanup whose expanded commands were shown and confirmed.
    CleanupConfirmed(Box<crate::cleanups::Prepared>),
    /// Compress (NTFS)… on the targets (Windows).
    Compress,
    /// The Compress dialog's answer: start the job.
    CompressStart(Box<crate::compress_ui::CompressStart>),
    /// Palette "Switch profile: <name>".
    SwitchProfile(String),
    /// Archive targets: extract next to the archive.
    ExtractHere,
    /// Archive targets: extract into a new `<name>/` folder next to each archive.
    ExtractToFolder,
    /// Archive targets: extract into a folder picked with the OS dialog.
    ExtractTo,
    /// Archive targets: extract into the other pane's folder (any provider: SFTP, cloud).
    ExtractToOther,
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
    /// The Compress dialog's answer: `ZipTo`, and `format` becomes the default.
    CompressTo {
        zip: PathBuf,
        src: Vec<PathBuf>,
        format: crate::settings::ArchiveFormat,
    },
    /// Targets: one existing archive plus items beside it; confirm, then `ZipTo`.
    AddToArchive,
    /// Items dropped on an archive row: confirm, then `ZipTo`.
    AddTo {
        archive: VPath,
        src: Vec<VPath>,
    },
    /// Show tab `tab` of pane `pane` (sidebar "Open archives").
    FocusTab {
        pane: usize,
        tab: usize,
    },
    // --- Task 23: drop zone ---
    /// Show / hide the drop zone strip (Ctrl+Shift+Z).
    ToggleDropZone,
    /// Stash the targets in the drop zone (Ctrl+Shift+S).
    StashSelection,
    /// Rows dragged onto the drop zone.
    Stash(Vec<VPath>),
    Unstash(VPath),
    /// Copy (or move) the stash into the active folder.
    StashPaste {
        mv: bool,
    },
    ClearStash,
    // --- Task 29 ---
    /// Library sidebar, overview, dialogs, tags (`library.rs`).
    Library(crate::library::LibCmd),
    /// Phase 17 Task 82: the Disk Usage window (Ctrl+Shift+U, a drive's context menu).
    DiskUsage(crate::disk_usage::DuCmd),
    /// Recycle Bin / Trash tab: put the targets back where they were deleted from.
    RestoreTrash,
    /// Confirmed: delete these trash items for good.
    PurgeTrash(Vec<VPath>),
    /// Recycle Bin / Trash tab: asks, then `EmptyTrashNow`.
    EmptyTrash,
    /// Confirmed: delete everything in the trash for good.
    EmptyTrashNow,
    // --- Task 36 ---
    /// Devices sidebar, pair / shares dialogs, Spacedrop (`devices.rs`).
    Devices(crate::devices::DevCmd),
    /// Phase 14: the sidebar's Favorites, section headers and eject (`sidebar.rs`).
    Sidebar(crate::sidebar_prefs::SidebarCmd),
    /// Mount… on a library source (`mount_ui.rs`).
    Mount(crate::mount_ui::MountCmd),
    /// Task 77: open a file list as a tab, export results (`filelist_ui.rs`).
    FileList(crate::filelist_ui::FileListCmd),
    /// Phase 14 Task 67: Quick Look, the path bar, Go to Folder (`finder.rs`).
    Finder(crate::finder::FinderCmd),
    // --- Task 76 ---
    /// Bookmarks, filters and search history (`bookmarks.rs`).
    Find(crate::bookmarks::FindCmd),
    // --- Task 65 ---
    /// Show / hide the sidebar (Ctrl+Alt+S, the toolbar's sidebar button).
    ToggleSidebar,
    /// The Customize Toolbar dialog (`toolbar.rs`).
    CustomizeToolbar,
    /// The toolbar's search field: a search tab for this query, run now.
    ToolbarSearch(String),
}

/// A shortcut as the user's OS writes it: "Ctrl+" is "Cmd+" on macOS.
pub fn shortcut_label(text: &str) -> String {
    label_for(text, cfg!(target_os = "macos"))
}

fn label_for(text: &str, mac: bool) -> String {
    if mac {
        text.replace("Ctrl+", "Cmd+")
    } else {
        text.to_owned()
    }
}

/// Task 76: whether Keel itself uses exactly this key by default (a bookmark may not take
/// it): the default keymap's keys, the moving keys included.
pub fn is_builtin(mods: egui::Modifiers, key: Key) -> bool {
    crate::keymap::Keymap::default()
        .bindings()
        .main()
        .any(|b| b.chord.key == key && b.mods() == mods)
}

/// A search tab's keys (`keymap::Scope::Search`; by default Everything's toggles Ctrl+I,
/// Ctrl+B, Ctrl+U, Ctrl+M and Ctrl+R), also while typing in its query box. Call before
/// [`actions`] so Ctrl+I is Match Case there, not the Inspector.
pub fn search_toggles(ctx: &egui::Context, keys: &Bindings) -> Vec<Action> {
    ctx.input_mut(|i| {
        keys.search()
            .filter(|b| i.consume_key(b.mods(), b.chord.key))
            .map(|b| b.action.clone())
            .collect()
    })
}

pub fn actions_with_terminal(
    ctx: &egui::Context,
    enabled: bool,
    terminal: bool,
    keys: &Bindings,
) -> Vec<Action> {
    if !terminal {
        return actions(ctx, enabled, keys);
    }
    // Track swallowed clipboard presses even while the terminal owns the events.
    // Their later releases must never become a file paste after focus leaves.
    let _ = paste_pressed(ctx);
    if !enabled {
        return Vec::new();
    }
    ctx.input_mut(|i| {
        // Plain Esc belongs to the shell's programs (vim, less, fzf, PSReadLine): the
        // keymap never gives it to Leave the terminal.
        for b in keys.terminal() {
            if i.consume_key(b.mods(), b.chord.key) {
                if b.action == Action::LeaveTerminal {
                    // Discard this frame's text/key events on focus transfer.
                    i.events.retain(|e| !crate::term_pane::keyboard_event(e));
                }
                return vec![b.action.clone()];
            }
        }
        Vec::new()
    })
}

/// Actions while another window has the keys (the Disk Usage window): only the
/// window-level shortcuts that also work from a text box (palette, Settings, Ctrl+P…).
pub fn window_actions(ctx: &egui::Context, keys: &Bindings) -> Vec<Action> {
    let _ = paste_pressed(ctx);
    ctx.input_mut(|i| {
        keys.main()
            .filter(|b| b.typing)
            .filter(|b| i.consume_key(b.mods(), b.chord.key))
            .map(|b| b.action.clone())
            .collect()
    })
}

/// Actions for this frame. Empty while a text field has focus (rename, filter, path box);
/// a focused button, checkbox or sidebar item (after Tab) keeps Enter, Space and the
/// arrow keys for itself (`a11y::list_has_keys`), every other shortcut still works. Call every frame: with
/// `enabled` false (a modal is open) nothing is returned, but V presses are still tracked
/// so a V held while the modal closes cannot turn into a paste.
pub fn actions(ctx: &egui::Context, enabled: bool, keys: &Bindings) -> Vec<Action> {
    // Tracked even while typing, so a key released inside a text box cannot confuse it.
    let paste = paste_pressed(ctx);
    if !enabled {
        return Vec::new();
    }
    if typing(ctx) {
        // Window-level shortcuts still work from a text box (e.g. Ctrl+P from the search box).
        return ctx.input_mut(|i| {
            keys.main()
                .filter(|b| b.typing)
                .filter(|b| i.consume_key(b.mods(), b.chord.key))
                .map(|b| b.action.clone())
                .collect()
        });
    }
    let list = crate::a11y::list_has_keys(ctx);
    // Task 83: a focused chart zooms out with Backspace.
    let chart = crate::treemap::keys_owned(ctx);
    let mut out = ctx.input_mut(|i| {
        let mut out = Vec::new();
        for b in keys.main() {
            if !list && b.needs_list() {
                continue;
            }
            if chart && b.mods() == egui::Modifiers::NONE && b.chord.key == Key::Backspace {
                continue;
            }
            if let Action::Move(nav, _) = b.action {
                let extend = i.modifiers.shift;
                // Counted: a slow frame can carry several key repeats.
                for _ in 0..i.count_and_consume_key(b.mods(), b.chord.key) {
                    out.push(Action::Move(nav, extend));
                }
            } else if i.consume_key(b.mods(), b.chord.key) {
                out.push(b.action.clone());
            }
        }
        // AltGr arrives as Ctrl+Alt (Windows): its characters (@, €, \ on many layouts)
        // start the filter like any other text.
        let altgr = i.modifiers.ctrl && i.modifiers.alt && !i.modifiers.mac_cmd;
        let typing_allowed = altgr || (!i.modifiers.command && !i.modifiers.alt);
        for event in &i.events {
            match event {
                Event::Copy => out.push(Action::Copy),
                // egui-winit turns Shift+Delete into Cut on Windows; Phase 1 trashes.
                Event::Cut if i.modifiers.shift && !i.modifiers.command => out.push(Action::Delete),
                Event::Cut => out.push(Action::Cut),
                // Space never starts a filter.
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

/// A text box (rename, filter, path box) has the keyboard.
pub fn typing(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.focused())
        .is_some_and(|id| egui::TextEdit::load_state(ctx, id).is_some())
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
    use egui::Modifiers;

    const CMD: Modifiers = Modifiers::COMMAND;

    #[test]
    fn terminal_focus_suppresses_file_shortcuts_and_clipboard_release() {
        let ctx = egui::Context::default();
        for events in [
            vec![Event::Copy, Event::Cut, Event::Paste("text".into())],
            vec![v(false, CMD)],
        ] {
            let _ =
                ctx.run(
                    egui::RawInput {
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        assert!(
                            actions_with_terminal(ctx, true, true, &Bindings::default()).is_empty()
                        );
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
                    actions = actions_with_terminal(ctx, true, true, &Bindings::default());
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

    fn typed(modifiers: Modifiers, text: &str) -> Vec<Action> {
        let ctx = egui::Context::default();
        let mut out = Vec::new();
        let _ = ctx.run(
            egui::RawInput {
                events: vec![Event::Text(text.into())],
                modifiers,
                ..Default::default()
            },
            |ctx| out = actions(ctx, true, &Bindings::default()),
        );
        out
    }

    #[test]
    fn altgr_text_starts_the_filter_but_ctrl_and_alt_alone_do_not() {
        let ctrl_alt = Modifiers {
            alt: true,
            ..Modifiers::CTRL
        };
        assert_eq!(typed(ctrl_alt, "@"), [Action::Type("@".into())]);
        assert_eq!(typed(Modifiers::NONE, "a"), [Action::Type("a".into())]);
        assert!(typed(Modifiers::ALT, "x").is_empty());
        if !cfg!(target_os = "macos") {
            assert!(typed(Modifiers::COMMAND, "x").is_empty());
        }
    }

    #[test]
    fn labels_follow_the_os() {
        assert_eq!(label_for("Ctrl+Shift+P", true), "Cmd+Shift+P");
        assert_eq!(label_for("Ctrl+Shift+P", false), "Ctrl+Shift+P");
        assert_eq!(label_for("F5", true), "F5");
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
                |ctx| out = actions(ctx, enabled, &Bindings::default()),
            );
            out
        };
        // "v" pressed while Ctrl+P is open; Enter closes it; the release arrives after.
        assert!(run(vec![v(true, Modifiers::NONE)], false).is_empty());
        assert!(!run(vec![v(false, Modifiers::NONE)], true).contains(&Action::Paste));
    }
}
