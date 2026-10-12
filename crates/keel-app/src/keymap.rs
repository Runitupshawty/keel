//! Configurable keys (Phase 16 Task 76): every command by a stable id with its default
//! chords (the shortcut table Keel had before), the profile's own bindings (`[keymap]` in
//! config.toml, only what differs from the defaults), conflict checks, the guard that keeps
//! the essential commands reachable, import and export, and the Settings → Keyboard page.

use crate::a11y::Named;
use crate::keys::Action;
use crate::library::LibCmd;
use crate::tab::Nav;
use egui::{Event, Key, Modifiers};
use std::collections::BTreeMap;
use std::sync::Arc;

/// One key with its modifiers. "Ctrl" is the Command key on macOS (egui's `command`), as in
/// every shortcut label Keel shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub command: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: Key,
}

/// Keys whose names are symbols (egui's own names differ between versions for some).
const SYMBOLS: &[(Key, &str)] = &[
    (Key::Backtick, "`"),
    (Key::Comma, ","),
    (Key::Period, "."),
    (Key::Minus, "-"),
    (Key::Equals, "="),
    (Key::Plus, "+"),
    (Key::Slash, "/"),
    (Key::Backslash, "\\"),
    (Key::Semicolon, ";"),
    (Key::Colon, ":"),
    (Key::OpenBracket, "["),
    (Key::CloseBracket, "]"),
    (Key::Pipe, "|"),
    (Key::Questionmark, "?"),
    (Key::ArrowUp, "Up"),
    (Key::ArrowDown, "Down"),
    (Key::ArrowLeft, "Left"),
    (Key::ArrowRight, "Right"),
];

/// Other spellings accepted when reading a chord.
const ALIASES: &[(&str, Key)] = &[
    ("esc", Key::Escape),
    ("del", Key::Delete),
    ("ins", Key::Insert),
    ("return", Key::Enter),
    ("pgup", Key::PageUp),
    ("pgdn", Key::PageDown),
    ("arrowup", Key::ArrowUp),
    ("arrowdown", Key::ArrowDown),
    ("arrowleft", Key::ArrowLeft),
    ("arrowright", Key::ArrowRight),
    ("backtick", Key::Backtick),
    ("grave", Key::Backtick),
    ("comma", Key::Comma),
    ("period", Key::Period),
    ("minus", Key::Minus),
    ("equals", Key::Equals),
    ("plus", Key::Plus),
    ("slash", Key::Slash),
    ("backslash", Key::Backslash),
    ("semicolon", Key::Semicolon),
];

pub fn key_name(key: Key) -> &'static str {
    SYMBOLS
        .iter()
        .find(|(k, _)| *k == key)
        .map_or_else(|| key.name(), |(_, n)| n)
}

fn key_of(name: &str) -> Option<Key> {
    let n = name.trim();
    if let Some((k, _)) = SYMBOLS.iter().find(|(_, s)| s.eq_ignore_ascii_case(n)) {
        return Some(*k);
    }
    if let Some((_, k)) = ALIASES.iter().find(|(a, _)| a.eq_ignore_ascii_case(n)) {
        return Some(*k);
    }
    Key::ALL
        .iter()
        .copied()
        .find(|k| k.name().eq_ignore_ascii_case(n) || format!("{k:?}").eq_ignore_ascii_case(n))
}

impl Chord {
    pub const fn new(mods: Modifiers, key: Key) -> Self {
        Self {
            command: mods.command,
            shift: mods.shift,
            alt: mods.alt,
            key,
        }
    }

    /// The pattern `consume_key` matches with (the same values as the old table's).
    pub fn modifiers(self) -> Modifiers {
        Modifiers {
            alt: self.alt,
            shift: self.shift,
            command: self.command,
            ..Modifiers::NONE
        }
    }

    fn count(self) -> usize {
        usize::from(self.command) + usize::from(self.shift) + usize::from(self.alt)
    }

    /// "Ctrl+Shift+P", "Alt+Up", "F5" (reads back with `parse`).
    pub fn text(self) -> String {
        let mut s = String::new();
        for (on, name) in [
            (self.command, "Ctrl+"),
            (self.shift, "Shift+"),
            (self.alt, "Alt+"),
        ] {
            if on {
                s.push_str(name);
            }
        }
        s.push_str(key_name(self.key));
        s
    }

    /// As the user's OS writes it (Cmd on macOS).
    pub fn label(self) -> String {
        crate::keys::shortcut_label(&self.text())
    }

    /// "Ctrl+Shift+P", "ctrl + p", "Cmd+,", "Ctrl++", "Esc".
    pub fn parse(text: &str) -> Result<Chord, String> {
        let t = text.trim();
        let (mods, key) = if t == "+" {
            ("", "+")
        } else if let Some(m) = t.strip_suffix("++") {
            (m, "+")
        } else {
            match t.rsplit_once('+') {
                Some((m, k)) => (m, k),
                None => ("", t),
            }
        };
        let bad = || format!("\"{t}\" is not a key");
        if key.trim().is_empty() {
            return Err(bad());
        }
        let mut c = Chord {
            command: false,
            shift: false,
            alt: false,
            key: key_of(key).ok_or_else(bad)?,
        };
        for m in mods.split('+').map(str::trim).filter(|m| !m.is_empty()) {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "cmd" | "command" => c.command = true,
                "shift" => c.shift = true,
                "alt" | "option" | "opt" => c.alt = true,
                _ => return Err(format!("\"{t}\": unknown modifier \"{m}\"")),
            }
        }
        Ok(c)
    }

    /// Plain Enter, Space and the moving keys belong to a focused button or sidebar item
    /// (`a11y::list_has_keys`): they reach the list only when it has the keys.
    fn needs_list(self) -> bool {
        !self.command
            && !self.alt
            && matches!(
                self.key,
                Key::Enter
                    | Key::Space
                    | Key::ArrowUp
                    | Key::ArrowDown
                    | Key::ArrowLeft
                    | Key::ArrowRight
                    | Key::PageUp
                    | Key::PageDown
                    | Key::Home
                    | Key::End
            )
    }

    /// Why this chord can never be bound to `id`, if so.
    pub fn reserved(self, id: &str) -> Option<&'static str> {
        let plain = !self.command && !self.alt;
        // egui-winit turns these into clipboard events (with any extra modifier): the key
        // itself never arrives.
        if self.command && matches!(self.key, Key::C | Key::X | Key::V) {
            return Some("it is the clipboard's (Copy, Cut, Paste)");
        }
        if (self.key == Key::Insert && (self.command || self.shift))
            || (self.key == Key::Delete && self.shift)
        {
            return Some("it is the clipboard's (Ctrl+Insert, Shift+Insert, Shift+Delete)");
        }
        if self.command && matches!(self.key, Key::Equals | Key::Plus | Key::Minus | Key::Num0) {
            return Some("it zooms the window (Ctrl+= / Ctrl+- / Ctrl+0)");
        }
        if plain && self.key == Key::Tab {
            return Some("Tab moves the keyboard focus");
        }
        if plain && !self.shift && self.key == Key::Escape && id != ESCAPE_ID {
            return Some("plain Esc closes dialogs, menus and the filter");
        }
        if plain && types_text(self.key) {
            return Some("it types text (filter-on-typing); add Ctrl or Alt");
        }
        None
    }
}

/// Keys that type a character (letters, digits, punctuation); Space is not one here (it
/// selects).
fn types_text(key: Key) -> bool {
    key_name(key).chars().count() == 1
}

impl std::fmt::Display for Chord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

/// Where a command's keys work: in the file panes, in the terminal, both, or only in a
/// search tab (also from its query box; there they come before the file panes' keys, so
/// Ctrl+I is Match case in a search tab and the Inspector elsewhere).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Main,
    Terminal,
    Both,
    Search,
}

impl Scope {
    fn overlaps(self, other: Scope) -> bool {
        if self == Scope::Search || other == Scope::Search {
            return self == other;
        }
        self == other || self == Scope::Both || other == Scope::Both
    }
    fn main(self) -> bool {
        matches!(self, Scope::Main | Scope::Both)
    }
    fn terminal(self) -> bool {
        matches!(self, Scope::Terminal | Scope::Both)
    }
    fn search(self) -> bool {
        self == Scope::Search
    }
}

/// In the command palette: with a selection only, always, in a search tab only, or not
/// at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pal {
    None,
    Context,
    Global,
    Search,
}

pub struct Command {
    /// Stable: the key in `[keymap]`.
    pub id: &'static str,
    pub label: &'static str,
    pub action: Action,
    pub defaults: &'static [&'static str],
    pub palette: Pal,
    /// Also works while a text box has the keyboard (window-level commands).
    pub typing: bool,
    pub scope: Scope,
    /// Must keep at least one key (the guard).
    pub essential: bool,
    /// A key the system handles (the clipboard's): shown, never removable.
    pub fixed: &'static str,
}

/// Plain Esc: closes the filter, a rename, dialogs and menus. Only this command has it.
pub const ESCAPE_ID: &str = "cancel";
pub const PALETTE_ID: &str = "palette";

fn cmd(
    id: &'static str,
    label: &'static str,
    action: Action,
    defaults: &'static [&'static str],
) -> Command {
    Command {
        id,
        label,
        action,
        defaults,
        palette: Pal::None,
        typing: false,
        scope: Scope::Main,
        essential: false,
        fixed: "",
    }
}

impl Command {
    fn ctx(mut self) -> Self {
        self.palette = Pal::Context;
        self
    }
    fn global(mut self) -> Self {
        self.palette = Pal::Global;
        self
    }
    /// A search tab's command: listed in the palette there, its keys work only there.
    fn search(mut self) -> Self {
        self.palette = Pal::Search;
        self.scope = Scope::Search;
        self.typing = true;
        self
    }
    fn typing(mut self) -> Self {
        self.typing = true;
        self
    }
    fn essential(mut self) -> Self {
        self.essential = true;
        self
    }
    fn scope(mut self, scope: Scope) -> Self {
        self.scope = scope;
        self
    }
    fn fixed(mut self, key: &'static str) -> Self {
        self.fixed = key;
        self
    }
}

/// Show / hide hidden files: Finder's Cmd+Shift+. on macOS (Cmd+H hides the app there).
const HIDDEN: &[&str] = if cfg!(target_os = "macos") {
    &["Ctrl+Shift+."]
} else {
    &["Ctrl+H"]
};

/// Every command, in the palette's order (context ones first, then the global ones), then
/// the keys the palette does not list.
pub fn commands() -> Vec<Command> {
    use crate::folder_ops::Kind;
    use crate::search_tab::Toggle;
    use Action::*;
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut v = vec![
        // --- the palette, with a selection ---
        cmd("file.open", "Open", Enter, &["Enter"]).ctx(),
        cmd("file.open_with", "Open with…", OpenWith, &["Ctrl+Shift+O"]).ctx(),
        cmd(
            "file.open_location",
            "Open location",
            OpenLocation,
            &["Ctrl+Enter"],
        )
        .ctx(),
        cmd("edit.copy", "Copy", Copy, &[]).ctx().fixed("Ctrl+C"),
        cmd("edit.cut", "Cut", Cut, &[]).ctx().fixed("Ctrl+X"),
        cmd("file.rename", "Rename", Rename, &["F2"]).ctx(),
        cmd("file.bulk_rename", "Bulk rename…", BulkRename, &["Ctrl+F2"]).ctx(),
        cmd("file.trash", "Move to trash", Delete, &["Delete"]).ctx(),
        cmd("file.properties", "Properties", Properties, &[]).ctx(),
        cmd("archive.extract_here", "Extract here", ExtractHere, &[]).ctx(),
        cmd(
            "archive.extract_folder",
            "Extract to folder",
            ExtractToFolder,
            &[],
        )
        .ctx(),
        cmd("archive.extract_to", "Extract to…", ExtractTo, &[]).ctx(),
        cmd(
            "archive.extract_other",
            "Extract to the other pane",
            ExtractToOther,
            &[],
        )
        .ctx(),
        cmd("archive.add_zip", "Add to zip", AddToZip, &[]).ctx(),
        cmd("archive.compress", "Compress to zip…", CompressToZip, &[]).ctx(),
        cmd(
            "library.tag",
            "Tag…",
            Library(LibCmd::TagPicker),
            &["Ctrl+Shift+T"],
        )
        .ctx(),
        cmd(
            "library.favorite",
            "Toggle favorite",
            Library(LibCmd::ToggleFavorite),
            &["Ctrl+D"],
        )
        .ctx(),
        cmd(
            "folder.merge",
            "Merge into…",
            Library(LibCmd::FolderOp(Kind::Merge)),
            &[],
        )
        .ctx(),
        cmd(
            "folder.flatten",
            "Flatten…",
            Library(LibCmd::FolderOp(Kind::Flatten)),
            &[],
        )
        .ctx(),
        cmd(
            "folder.organize",
            "Organize by…",
            Library(LibCmd::FolderOp(Kind::Organize)),
            &[],
        )
        .ctx(),
        cmd(
            "folder.attributes",
            "Attributes…",
            Library(LibCmd::FolderOp(Kind::Attributes)),
            &[],
        )
        .ctx(),
        cmd(
            "folder.link",
            "Create link…",
            Library(LibCmd::FolderOp(Kind::Link)),
            &[],
        )
        .ctx(),
        // --- the palette, in a search tab (Everything's Search menu and its keys) ---
        cmd(
            "search.match_case",
            "Search: Match case",
            SearchToggle(Toggle::Case),
            &["Ctrl+I"],
        )
        .search(),
        cmd(
            "search.whole_words",
            "Search: Match whole words",
            SearchToggle(Toggle::WholeWord),
            &["Ctrl+B"],
        )
        .search(),
        cmd(
            "search.match_path",
            "Search: Match path",
            SearchToggle(Toggle::Path),
            &["Ctrl+U"],
        )
        .search(),
        cmd(
            "search.diacritics",
            "Search: Match diacritics",
            SearchToggle(Toggle::Diacritics),
            &["Ctrl+M"],
        )
        .search(),
        cmd(
            "search.regex",
            "Search: Regular expression",
            SearchToggle(Toggle::Regex),
            &["Ctrl+R"],
        )
        .search(),
        cmd("search.more", "Search: Load more results", SearchMore, &[]).search(),
        // --- the palette, always ---
        cmd("go.up", "Go to parent folder", Up, &["Alt+Up"]).global(),
        cmd("go.back", "Back", Back, &["Alt+Left"]).global(),
        cmd("go.forward", "Forward", Forward, &["Alt+Right"]).global(),
        cmd("view.refresh", "Refresh", Refresh, &["F5"]).global(),
        cmd("tab.new", "New tab", NewTab, &["Ctrl+T"])
            .global()
            .typing(),
        cmd("tab.close", "Close tab", CloseTab, &["Ctrl+W"])
            .global()
            .typing(),
        cmd(
            "pane.dual",
            "Toggle dual pane",
            ToggleDual,
            &["Ctrl+Shift+D"],
        )
        .global()
        .typing(),
        cmd("pane.switch", "Switch pane", SwitchPane, &["F6"]).global(),
        cmd(
            "view.preview",
            "Toggle preview panel",
            TogglePreview,
            &["F3"],
        )
        .global()
        .typing(),
        cmd(
            "view.inspector",
            "Show inspector",
            ToggleInspector,
            &["Ctrl+I"],
        )
        .global()
        .typing(),
        cmd(
            "dropzone.toggle",
            "Toggle drop zone",
            ToggleDropZone,
            &["Ctrl+Shift+Z"],
        )
        .global(),
        cmd(
            "dropzone.stash",
            "Stash selection in the drop zone",
            StashSelection,
            &["Ctrl+Shift+S"],
        )
        .global(),
        cmd(
            "dropzone.paste",
            "Paste the drop zone here",
            StashPaste { mv: false },
            &[],
        )
        .global(),
        cmd(
            "dropzone.move",
            "Move the drop zone here",
            StashPaste { mv: true },
            &[],
        )
        .global(),
        cmd("dropzone.clear", "Clear the drop zone", ClearStash, &[]).global(),
        cmd("view.hidden", "Toggle hidden files", ToggleHidden, HIDDEN).global(),
        cmd("filter", "Filter this folder", FocusFilter, &["Ctrl+E"]).global(),
        cmd("go.path", "Edit path", FocusPath, &["Ctrl+L"]).global(),
        // Not "with Everything": Windows uses Keel's own index when Everything is not running.
        cmd("search", "Search files", Search, &["Ctrl+F"])
            .global()
            .typing(),
        cmd("go.jump", "Jump to folder or file", JumpFolder, &["Ctrl+P"])
            .global()
            .typing(),
        cmd(
            "go.reindex",
            "Rebuild folder and file lists",
            ReindexFolders,
            &[],
        )
        .global(),
        cmd("select.all", "Select all", SelectAll, &["Ctrl+A"]).global(),
        cmd("select.invert", "Invert selection", InvertSelection, &[]).global(),
        cmd("edit.paste", "Paste", Paste, &[])
            .global()
            .fixed("Ctrl+V"),
        cmd("edit.copy_path", "Copy path", CopyPath, &[]).global(),
        cmd("edit.copy_name", "Copy name", CopyName, &[]).global(),
        cmd(
            "file.new_folder",
            "New folder",
            NewFolder,
            &["Ctrl+Shift+N"],
        )
        .global(),
        cmd("file.new_file", "New file", NewFile, &[]).global(),
        cmd(
            "file.reveal",
            "Show in system file manager",
            RevealInSystem,
            &[],
        )
        .global(),
        cmd("terminal.open", "Open terminal here", OpenTerminal, &[]).global(),
        cmd(
            "terminal.toggle",
            "Toggle terminal",
            ToggleTerminal,
            &["Ctrl+`"],
        )
        .global()
        .typing()
        .scope(Scope::Both),
        cmd("view.theme", "Toggle dark / light theme", ToggleTheme, &[]).global(),
        cmd(
            "view.sizes_calc",
            "Calculate sizes",
            CalculateSizes { fresh: true },
            &[],
        )
        .global(),
        cmd(
            "view.sizes",
            "Sizes view",
            View(crate::pane::ViewMode::Sizes),
            &[],
        )
        .global(),
        cmd(
            "view.details",
            "Details view",
            View(crate::pane::ViewMode::Details),
            &[],
        )
        .global(),
        cmd("settings", "Settings", Settings, &["Ctrl+,"])
            .global()
            .typing(),
        cmd("window.new", "New window", NewWindow, &[]).global(),
        cmd(
            "library.overview",
            "Library overview",
            Library(LibCmd::Overview),
            &[],
        )
        .global(),
        cmd(
            "library.add_source",
            "Add library source…",
            Library(LibCmd::AddSource),
            &[],
        )
        .global(),
        cmd(
            "library.duplicates",
            "Find duplicates",
            Library(LibCmd::Duplicates),
            &[],
        )
        .global(),
        cmd(
            "library.assign",
            "Assign tags with number keys",
            Library(LibCmd::AssignMode),
            &["Ctrl+Shift+A"],
        )
        .global(),
        cmd(
            "library.backup",
            "Back up library…",
            Library(LibCmd::Backup),
            &[],
        )
        .global(),
        cmd(
            "library.restore",
            "Restore library…",
            Library(LibCmd::Restore),
            &[],
        )
        .global(),
        cmd(
            "library.favorites",
            "Favorites",
            Library(LibCmd::Query(crate::library::FAVORITES_QUERY.into())),
            &[],
        )
        .global(),
        cmd(
            "library.recents",
            "Recents",
            Library(LibCmd::Query(crate::library::RECENTS_QUERY.into())),
            &[],
        )
        .global(),
        // --- Task 77 ---
        cmd(
            "filelist.open",
            "Open file list…",
            FileList(crate::filelist_ui::FileListCmd::Open(None)),
            &[],
        )
        .global(),
        cmd(
            "filelist.export",
            "Export results…",
            FileList(crate::filelist_ui::FileListCmd::Export),
            &[],
        )
        .global(),
        // --- keys the palette does not list ---
        cmd(PALETTE_ID, "Command palette", Palette, &["Ctrl+Shift+P"])
            .typing()
            .essential(),
        cmd("edit.undo", "Undo", Undo, &["Ctrl+Z"]),
        cmd(
            "go.backspace",
            "Parent folder (in a search: delete a character)",
            Backspace,
            &["Backspace"],
        ),
        cmd(
            "select.toggle",
            "Select or deselect the cursor item",
            ToggleSelect,
            &["Space"],
        ),
        cmd(
            ESCAPE_ID,
            "Cancel: close the filter or a rename",
            ClearFilter,
            &["Escape"],
        )
        .essential(),
        cmd(
            "terminal.leave",
            "Leave the terminal",
            LeaveTerminal,
            &["F6", "Shift+Escape"],
        )
        .scope(Scope::Terminal)
        .essential(),
        cmd("cursor.up", "Cursor up", Move(Nav::Prev, false), &["Up"]),
        cmd(
            "cursor.down",
            "Cursor down",
            Move(Nav::Next, false),
            &["Down"],
        ),
        cmd(
            "cursor.left",
            "Cursor left",
            Move(Nav::Left, false),
            &["Left"],
        ),
        cmd(
            "cursor.right",
            "Cursor right",
            Move(Nav::Right, false),
            &["Right"],
        ),
        cmd(
            "cursor.page_up",
            "Cursor one page up",
            Move(Nav::PageUp, false),
            &["PageUp"],
        ),
        cmd(
            "cursor.page_down",
            "Cursor one page down",
            Move(Nav::PageDown, false),
            &["PageDown"],
        ),
        cmd(
            "cursor.home",
            "Cursor to the first item",
            Move(Nav::Home, false),
            &["Home"],
        ),
        cmd(
            "cursor.end",
            "Cursor to the last item",
            Move(Nav::End, false),
            &["End"],
        ),
    ];
    #[cfg(windows)]
    v.push(cmd(
        "file.shell_menu",
        "More options (the Windows menu)",
        ShellMenu {
            extended: true,
            keyboard: true,
        },
        &["Shift+F10"],
    ));
    v
}

/// The profile's bindings: only commands whose keys differ from the defaults, id → chords
/// (an empty list: no key). Saved as `[keymap]` in config.toml.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(transparent)]
pub struct Keymap(pub BTreeMap<String, Vec<String>>);

/// A recorded chord another command has.
#[derive(Clone, Debug, PartialEq)]
pub enum Bind {
    /// Added.
    Done,
    /// Already this command's.
    Already,
    /// Other commands have it: ask before taking it from them (`replace`).
    Conflict(Vec<&'static str>),
}

fn find(id: &str) -> Option<Command> {
    commands().into_iter().find(|c| c.id == id)
}

fn parse_all(texts: &[impl AsRef<str>]) -> Vec<Chord> {
    let mut out: Vec<Chord> = Vec::new();
    for t in texts {
        if let Ok(c) = Chord::parse(t.as_ref()) {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

impl Keymap {
    /// `cmd`'s chords now: the profile's, else the defaults.
    pub fn chords(&self, cmd: &Command) -> Vec<Chord> {
        match self.0.get(cmd.id) {
            Some(texts) => parse_all(texts.as_slice()),
            None => parse_all(cmd.defaults),
        }
    }

    pub fn is_default(&self, id: &str) -> bool {
        !self.0.contains_key(id)
    }

    /// Sets `id`'s chords; the defaults again remove the entry.
    fn set(&mut self, cmd: &Command, chords: &[Chord]) {
        if chords == parse_all(cmd.defaults).as_slice() {
            self.0.remove(cmd.id);
        } else {
            let texts = chords.iter().map(|c| c.text()).collect();
            self.0.insert(cmd.id.to_owned(), texts);
        }
    }

    pub fn reset(&mut self, id: &str) {
        self.0.remove(id);
    }

    pub fn reset_all(&mut self) {
        self.0.clear();
    }

    /// The commands (other than `id`) whose keys share a scope with `id` and include
    /// `chord`.
    pub fn conflicts(&self, id: &str, chord: Chord) -> Vec<&'static str> {
        let all = commands();
        let Some(me) = all.iter().find(|c| c.id == id) else {
            return Vec::new();
        };
        all.iter()
            .filter(|c| c.id != id && c.scope.overlaps(me.scope))
            .filter(|c| self.chords(c).contains(&chord))
            .map(|c| c.id)
            .collect()
    }

    /// Adds `chord` to `id`. With `replace` the chord is taken from the commands that have
    /// it; without, those are returned to ask about. Refused: a reserved chord, and taking
    /// the last key of an essential command (the guard).
    pub fn bind(&mut self, id: &str, chord: Chord, replace: bool) -> Result<Bind, String> {
        let all = commands();
        let me = all.iter().find(|c| c.id == id).ok_or("no such command")?;
        if let Some(why) = chord.reserved(id) {
            return Err(format!("{} cannot be bound: {why}", chord.label()));
        }
        let mine = self.chords(me);
        if mine.contains(&chord) {
            return Ok(Bind::Already);
        }
        let others = self.conflicts(id, chord);
        if !others.is_empty() && !replace {
            return Ok(Bind::Conflict(others));
        }
        for other in &others {
            let c = all.iter().find(|c| c.id == *other).expect("listed");
            let left: Vec<Chord> = self.chords(c).into_iter().filter(|x| *x != chord).collect();
            if c.essential && left.is_empty() {
                return Err(format!(
                    "{} is the only key of \"{}\", which must keep one; give it another key first",
                    chord.label(),
                    c.label
                ));
            }
        }
        for other in others {
            let c = all.iter().find(|c| c.id == other).expect("listed");
            let left: Vec<Chord> = self.chords(c).into_iter().filter(|x| *x != chord).collect();
            self.set(c, &left);
        }
        let mut chords = mine;
        chords.push(chord);
        self.set(me, &chords);
        Ok(Bind::Done)
    }

    /// Removes `chord` from `id`; refused when it is an essential command's last key.
    pub fn remove(&mut self, id: &str, chord: Chord) -> Result<(), String> {
        let me = find(id).ok_or("no such command")?;
        let left: Vec<Chord> = self
            .chords(&me)
            .into_iter()
            .filter(|c| *c != chord)
            .collect();
        if me.essential && left.is_empty() {
            return Err(format!(
                "\"{}\" must keep a key: add another one before removing {}",
                me.label,
                chord.label()
            ));
        }
        self.set(&me, &left);
        Ok(())
    }

    /// Puts a hand-edited or imported keymap right: unknown commands, unreadable or
    /// reserved keys are dropped and an essential command without a key gets its defaults
    /// back. Returns what was changed, for a notice.
    pub fn sanitize(&mut self) -> Vec<String> {
        let all = commands();
        let mut notes = Vec::new();
        let ids: Vec<String> = self.0.keys().cloned().collect();
        for id in ids {
            let Some(c) = all.iter().find(|c| c.id == id) else {
                self.0.remove(&id);
                notes.push(format!("unknown command \"{id}\""));
                continue;
            };
            let texts = self.0.get(&id).cloned().unwrap_or_default();
            let mut keep: Vec<Chord> = Vec::new();
            for t in &texts {
                match Chord::parse(t) {
                    Ok(chord) if chord.reserved(&id).is_some() => {
                        notes.push(format!("{id}: {t} is reserved"))
                    }
                    Ok(chord) => {
                        if !keep.contains(&chord) {
                            keep.push(chord);
                        }
                    }
                    Err(e) => notes.push(format!("{id}: {e}")),
                }
            }
            if c.essential && keep.is_empty() {
                notes.push(format!("{id}: must keep a key, defaults restored"));
                self.0.remove(&id);
                continue;
            }
            self.set(c, &keep);
        }
        notes
    }

    /// The keys to read every frame (`keys::actions`).
    pub fn bindings(&self) -> Bindings {
        let mut keys = Vec::new();
        let mut labels = Vec::new();
        for c in commands() {
            let chords = self.chords(&c);
            let mut text: Vec<String> = Vec::new();
            if !c.fixed.is_empty() {
                text.push(crate::keys::shortcut_label(c.fixed));
            }
            text.extend(chords.iter().map(|k| k.label()));
            labels.push((c.action.clone(), text.join(", ")));
            for chord in chords {
                keys.push(Binding {
                    chord,
                    action: c.action.clone(),
                    typing: c.typing,
                    scope: c.scope,
                });
            }
        }
        // More modifiers first: egui ignores an unrequested Shift or Alt when matching, so
        // Ctrl+Shift+Z must be tried before Ctrl+Z. Stable: the table's order otherwise.
        keys.sort_by_key(|b| std::cmp::Reverse(b.chord.count()));
        Bindings { keys, labels }
    }
}

pub struct Binding {
    pub chord: Chord,
    pub action: Action,
    pub typing: bool,
    pub scope: Scope,
}

impl Binding {
    pub fn mods(&self) -> Modifiers {
        self.chord.modifiers()
    }
    pub fn needs_list(&self) -> bool {
        self.chord.needs_list()
    }
}

/// A keymap resolved for reading keys and showing them.
pub struct Bindings {
    pub keys: Vec<Binding>,
    labels: Vec<(Action, String)>,
}

impl Default for Bindings {
    fn default() -> Self {
        Keymap::default().bindings()
    }
}

impl Bindings {
    /// The file panes' keys.
    pub fn main(&self) -> impl Iterator<Item = &Binding> {
        self.keys.iter().filter(|b| b.scope.main())
    }

    /// The keys read while the terminal has the keyboard.
    pub fn terminal(&self) -> impl Iterator<Item = &Binding> {
        self.keys.iter().filter(|b| b.scope.terminal())
    }

    /// A search tab's keys (`keys::search_toggles`), read before the file panes' keys.
    pub fn search(&self) -> impl Iterator<Item = &Binding> {
        self.keys.iter().filter(|b| b.scope.search())
    }

    /// "Ctrl+Shift+P" (every key of `action`, comma separated; "" = none).
    pub fn label(&self, action: &Action) -> String {
        self.labels
            .iter()
            .find(|(a, _)| a == action)
            .map(|(_, l)| l.clone())
            .unwrap_or_default()
    }
}

fn published_id() -> egui::Id {
    egui::Id::new("keel-keymap")
}

/// For menus that show shortcuts (`shortcut`), once per frame.
pub fn publish(ctx: &egui::Context, keys: &Arc<Bindings>) {
    let keys = keys.clone();
    ctx.data_mut(|d| d.insert_temp(published_id(), keys));
}

/// `action`'s keys as the user bound them; `fallback` (the default's text) before the
/// first frame published them.
pub fn shortcut(ctx: &egui::Context, action: &Action, fallback: &str) -> String {
    // The menu's "More options" opens at the pointer; its key opens at the cursor row.
    #[cfg(windows)]
    let keyed = Action::ShellMenu {
        extended: true,
        keyboard: true,
    };
    #[cfg(windows)]
    let action = match action {
        Action::ShellMenu { .. } => &keyed,
        _ => action,
    };
    match ctx.data(|d| d.get_temp::<Arc<Bindings>>(published_id())) {
        Some(keys) => keys.label(action),
        None => crate::keys::shortcut_label(fallback),
    }
}

// --- import / export ---

/// Largest keymap file read.
const MAX_FILE: u64 = 1024 * 1024;

/// Every command's keys, for sharing or a backup (`[keymap]`, as in config.toml).
pub fn export_text(map: &Keymap) -> String {
    let mut table = BTreeMap::new();
    for c in commands() {
        table.insert(
            c.id.to_owned(),
            map.chords(&c).iter().map(|k| k.text()).collect::<Vec<_>>(),
        );
    }
    #[derive(serde::Serialize)]
    struct File {
        keymap: BTreeMap<String, Vec<String>>,
    }
    let body = toml::to_string(&File { keymap: table }).unwrap_or_default();
    format!(
        "# Keel keymap: command id = its keys (\"Ctrl\" is Cmd on macOS; [] = no key).\n\
         # Import it in Settings → Keyboard.\n\n{body}"
    )
}

/// A keymap file read back: commands it leaves out keep their defaults. Returns the keymap
/// and what was dropped or put right.
pub fn import_text(text: &str) -> Result<(Keymap, Vec<String>), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let inner = match table.remove("keymap") {
        Some(toml::Value::Table(t)) => t,
        Some(_) => return Err("[keymap] is not a table".into()),
        None => table,
    };
    let mut map = Keymap::default();
    let mut notes = Vec::new();
    for (id, value) in inner {
        let texts: Vec<String> = match value {
            toml::Value::Array(items) => items
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            toml::Value::String(s) => vec![s],
            _ => {
                notes.push(format!("{id}: not a list of keys"));
                continue;
            }
        };
        map.0.insert(id, texts);
    }
    notes.extend(map.sanitize());
    Ok((map, notes))
}

pub fn import_file(path: &std::path::Path) -> Result<(Keymap, Vec<String>), String> {
    let len = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if len > MAX_FILE {
        return Err("the file is too large for a keymap".into());
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    import_text(&text)
}

// --- Settings → Keyboard ---

#[derive(Default)]
pub struct KeymapUi {
    pub search: String,
    /// Recording a key for this command (`capture`).
    pub recording: Option<&'static str>,
    /// A recorded key other commands have: (command, key, those commands).
    conflict: Option<(&'static str, Chord, Vec<&'static str>)>,
    /// The last refusal or result: (text, is an error).
    notice: Option<(String, bool)>,
}

impl KeymapUi {
    /// While recording: the next key press (Esc cancels). Call before anything else reads
    /// the frame's keys; consumes every key event.
    pub fn capture(&mut self, ctx: &egui::Context, map: &mut Keymap) {
        let Some(id) = self.recording else { return };
        let pressed = ctx.input_mut(|i| {
            let mut got = None;
            for e in &i.events {
                match e {
                    Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } if got.is_none() => got = Some(Ok((*key, *modifiers))),
                    Event::Copy | Event::Cut | Event::Paste(_) if got.is_none() => {
                        got = Some(Err(()));
                    }
                    _ => {}
                }
            }
            i.events.retain(|e| {
                !matches!(
                    e,
                    Event::Key { .. } | Event::Text(_) | Event::Copy | Event::Cut | Event::Paste(_)
                )
            });
            got
        });
        match pressed {
            None => {}
            Some(Err(())) => {
                self.recording = None;
                self.notice = Some(("Ctrl+C, Ctrl+X and Ctrl+V are the clipboard's".into(), true));
            }
            Some(Ok((Key::Escape, m))) if !m.any() => {
                self.recording = None;
                self.notice = None;
            }
            Some(Ok((key, m))) => {
                self.recording = None;
                if m.ctrl && !m.command {
                    self.notice = Some(("Use Cmd instead of Control".into(), true));
                    return;
                }
                self.offer(map, id, Chord::new(m, key));
            }
        }
    }

    fn offer(&mut self, map: &mut Keymap, id: &'static str, chord: Chord) {
        self.conflict = None;
        self.notice = match map.bind(id, chord, false) {
            Ok(Bind::Done) => None,
            Ok(Bind::Already) => Some((
                format!("{} is already this command's", chord.label()),
                false,
            )),
            Ok(Bind::Conflict(others)) => {
                self.conflict = Some((id, chord, others));
                None
            }
            Err(e) => Some((e, true)),
        };
    }

    pub fn page(&mut self, ui: &mut egui::Ui, map: &mut Keymap) {
        let all = commands();
        let label_of = |id: &str| all.iter().find(|c| c.id == id).map_or("?", |c| c.label);
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search commands or keys")
                    .desired_width(220.0),
            );
            let _ = r.named("Search commands");
            if ui.button("Import…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Import keymap")
                    .add_filter("Keymap", &["toml"])
                    .pick_file()
                {
                    self.notice = Some(match import_file(&path) {
                        Ok((imported, notes)) => {
                            *map = imported;
                            if notes.is_empty() {
                                ("Keymap imported".into(), false)
                            } else {
                                (
                                    format!("Keymap imported; skipped: {}", notes.join("; ")),
                                    true,
                                )
                            }
                        }
                        Err(e) => (format!("Could not import {}: {e}", path.display()), true),
                    });
                }
            }
            if ui.button("Export…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Export keymap")
                    .set_file_name("keel-keymap.toml")
                    .add_filter("Keymap", &["toml"])
                    .save_file()
                {
                    self.notice = Some(
                        match crate::settings::write_atomic(&path, export_text(map).as_bytes()) {
                            Ok(()) => (format!("Exported to {}", path.display()), false),
                            Err(e) => (format!("Could not export: {e:#}"), true),
                        },
                    );
                }
            }
            if ui
                .add_enabled(!map.0.is_empty(), egui::Button::new("Reset all"))
                .on_hover_text("Every command back to its default keys")
                .clicked()
            {
                map.reset_all();
                self.conflict = None;
                self.notice = Some(("Every key is back to its default".into(), false));
            }
        });
        if let Some((text, error)) = &self.notice {
            if *error {
                ui.colored_label(ui.visuals().error_fg_color, text);
            } else {
                ui.label(text);
            }
        }
        if let Some((id, chord, others)) = self.conflict.clone() {
            ui.horizontal_wrapped(|ui| {
                let names: Vec<&str> = others.iter().map(|o| label_of(o)).collect();
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("{} is already \"{}\".", chord.label(), names.join("\", \"")),
                );
                if ui
                    .button(format!("Use it for \"{}\"", label_of(id)))
                    .clicked()
                {
                    self.conflict = None;
                    if let Err(e) = map.bind(id, chord, true) {
                        self.notice = Some((e, true));
                    }
                }
                if ui.button("Cancel").clicked() {
                    self.conflict = None;
                }
            });
        }
        ui.weak(
            "Record adds a key; click a key to remove it. Global hotkeys are on the General page.",
        );
        let pat = self.search.trim().to_lowercase();
        egui::ScrollArea::vertical()
            .max_height(420.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("keymap-grid")
                    .num_columns(3)
                    .striped(true)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        for c in &all {
                            let chords = map.chords(c);
                            let texts: Vec<String> = chords.iter().map(|k| k.label()).collect();
                            let hit = pat.is_empty()
                                || c.label.to_lowercase().contains(&pat)
                                || c.id.contains(&pat)
                                || c.fixed.to_lowercase().contains(&pat)
                                || texts.iter().any(|t| t.to_lowercase().contains(&pat));
                            if !hit {
                                continue;
                            }
                            let name = if map.is_default(c.id) {
                                egui::RichText::new(c.label)
                            } else {
                                egui::RichText::new(c.label).strong()
                            };
                            ui.label(name).on_hover_text(c.id);
                            ui.horizontal_wrapped(|ui| {
                                if !c.fixed.is_empty() {
                                    ui.weak(crate::keys::shortcut_label(c.fixed))
                                        .on_hover_text("The system's clipboard key (always on)");
                                }
                                for (chord, text) in chords.iter().zip(&texts) {
                                    let r = ui
                                        .small_button(format!("{text} \u{d7}"))
                                        .on_hover_text(format!("Remove {text}"));
                                    if r.clicked() {
                                        self.conflict = None;
                                        self.notice =
                                            map.remove(c.id, *chord).err().map(|e| (e, true));
                                    }
                                }
                                if chords.is_empty() && c.fixed.is_empty() {
                                    ui.weak("none");
                                }
                            });
                            ui.horizontal(|ui| {
                                if self.recording == Some(c.id) {
                                    ui.colored_label(
                                        ui.visuals().warn_fg_color,
                                        "Press a key… (Esc cancels)",
                                    );
                                } else if ui.small_button("Record").clicked() {
                                    self.recording = Some(c.id);
                                    self.conflict = None;
                                    self.notice = None;
                                    ui.memory_mut(|m| {
                                        if let Some(f) = m.focused() {
                                            m.surrender_focus(f);
                                        }
                                    });
                                }
                                if ui
                                    .add_enabled(
                                        !map.is_default(c.id),
                                        egui::Button::new("Reset").small(),
                                    )
                                    .on_hover_text("Back to the default keys")
                                    .clicked()
                                {
                                    map.reset(c.id);
                                    self.conflict = None;
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: Modifiers = Modifiers::COMMAND;
    const CMD_SHIFT: Modifiers = Modifiers {
        shift: true,
        ..Modifiers::COMMAND
    };
    #[cfg(target_os = "macos")]
    const HIDDEN_OLD: (Modifiers, Key) = (CMD_SHIFT, Key::Period);
    #[cfg(not(target_os = "macos"))]
    const HIDDEN_OLD: (Modifiers, Key) = (CMD, Key::H);

    /// The compile-time table `keys.rs` had before keys were configurable, verbatim.
    fn old_table() -> Vec<(Modifiers, Key, Action)> {
        vec![
            (CMD, Key::Backtick, Action::ToggleTerminal),
            (CMD_SHIFT, Key::P, Action::Palette),
            (CMD_SHIFT, Key::D, Action::ToggleDual),
            (CMD_SHIFT, Key::N, Action::NewFolder),
            (CMD_SHIFT, Key::O, Action::OpenWith),
            (CMD_SHIFT, Key::Z, Action::ToggleDropZone),
            (CMD, Key::Z, Action::Undo),
            (CMD_SHIFT, Key::S, Action::StashSelection),
            (CMD_SHIFT, Key::T, Action::Library(LibCmd::TagPicker)),
            (CMD_SHIFT, Key::A, Action::Library(LibCmd::AssignMode)),
            (HIDDEN_OLD.0, HIDDEN_OLD.1, Action::ToggleHidden),
            (CMD, Key::P, Action::JumpFolder),
            (CMD, Key::Enter, Action::OpenLocation),
            (CMD, Key::F, Action::Search),
            (CMD, Key::E, Action::FocusFilter),
            (CMD, Key::I, Action::ToggleInspector),
            (CMD, Key::L, Action::FocusPath),
            (CMD, Key::T, Action::NewTab),
            (CMD, Key::W, Action::CloseTab),
            (CMD, Key::A, Action::SelectAll),
            (CMD, Key::D, Action::Library(LibCmd::ToggleFavorite)),
            (CMD, Key::Comma, Action::Settings),
            (Modifiers::ALT, Key::ArrowUp, Action::Up),
            (Modifiers::ALT, Key::ArrowLeft, Action::Back),
            (Modifiers::ALT, Key::ArrowRight, Action::Forward),
            (Modifiers::NONE, Key::Backspace, Action::Backspace),
            (CMD, Key::F2, Action::BulkRename),
            (Modifiers::NONE, Key::F2, Action::Rename),
            (Modifiers::NONE, Key::F3, Action::TogglePreview),
            (Modifiers::NONE, Key::F5, Action::Refresh),
            (Modifiers::NONE, Key::F6, Action::SwitchPane),
            #[cfg(windows)]
            (
                Modifiers::SHIFT,
                Key::F10,
                Action::ShellMenu {
                    extended: true,
                    keyboard: true,
                },
            ),
            (Modifiers::NONE, Key::Delete, Action::Delete),
            (Modifiers::NONE, Key::Enter, Action::Enter),
            (Modifiers::NONE, Key::Space, Action::ToggleSelect),
            (Modifiers::NONE, Key::Escape, Action::ClearFilter),
        ]
    }

    const OLD_MOVES: &[(Key, Nav)] = &[
        (Key::ArrowUp, Nav::Prev),
        (Key::ArrowDown, Nav::Next),
        (Key::ArrowLeft, Nav::Left),
        (Key::ArrowRight, Nav::Right),
        (Key::PageUp, Nav::PageUp),
        (Key::PageDown, Nav::PageDown),
        (Key::Home, Nav::Home),
        (Key::End, Nav::End),
    ];

    const OLD_WHILE_TYPING: &[Action] = &[
        Action::ToggleTerminal,
        Action::Palette,
        Action::JumpFolder,
        Action::Search,
        Action::ToggleDual,
        Action::TogglePreview,
        Action::ToggleInspector,
        Action::NewTab,
        Action::CloseTab,
        Action::Settings,
    ];

    #[test]
    fn default_keymap_equals_the_old_table() {
        let b = Keymap::default().bindings();
        let main: Vec<(Modifiers, Key, Action)> = b
            .main()
            .filter(|k| !matches!(k.action, Action::Move(..)))
            .map(|k| (k.mods(), k.chord.key, k.action.clone()))
            .collect();
        let old = old_table();
        assert_eq!(main.len(), old.len(), "{main:?}");
        for entry in &old {
            assert!(main.contains(entry), "missing {entry:?}");
        }
        // The moves.
        let moves: Vec<(Modifiers, Key, Action)> = b
            .main()
            .filter(|k| matches!(k.action, Action::Move(..)))
            .map(|k| (k.mods(), k.chord.key, k.action.clone()))
            .collect();
        assert_eq!(moves.len(), OLD_MOVES.len());
        for (key, nav) in OLD_MOVES {
            assert!(moves.contains(&(Modifiers::NONE, *key, Action::Move(*nav, false))));
        }
        // The same window-level keys work from a text box.
        let mut typing: Vec<Action> = b
            .main()
            .filter(|k| k.typing)
            .map(|k| k.action.clone())
            .collect();
        typing.dedup();
        assert_eq!(typing.len(), OLD_WHILE_TYPING.len());
        for a in OLD_WHILE_TYPING {
            assert!(typing.contains(a), "{a:?}");
        }
        // The terminal's keys: Ctrl+` and F6 / Shift+Esc.
        let term: Vec<(Modifiers, Key, Action)> = b
            .terminal()
            .map(|k| (k.mods(), k.chord.key, k.action.clone()))
            .collect();
        assert_eq!(
            term,
            [
                (CMD, Key::Backtick, Action::ToggleTerminal),
                (Modifiers::SHIFT, Key::Escape, Action::LeaveTerminal),
                (Modifiers::NONE, Key::F6, Action::LeaveTerminal),
            ]
        );
        // A search tab's keys: Everything's toggles, only there (Ctrl+I stays the
        // Inspector elsewhere, so it is no conflict).
        let search: Vec<(Modifiers, Key, Action)> = b
            .search()
            .map(|k| (k.mods(), k.chord.key, k.action.clone()))
            .collect();
        assert_eq!(search.len(), crate::search_tab::Toggle::ALL.len());
        for t in crate::search_tab::Toggle::ALL {
            let chord = Chord::parse(t.shortcut()).unwrap();
            assert!(search.contains(&(CMD, chord.key, Action::SearchToggle(t))));
        }
        assert!(Keymap::default()
            .conflicts("search.match_case", Chord::new(CMD, Key::I))
            .is_empty());
        // Ctrl+Shift+Z before Ctrl+Z (and every such pair): more modifiers come first.
        for (i, x) in b.keys.iter().enumerate() {
            for y in &b.keys[i + 1..] {
                if x.chord.key == y.chord.key {
                    assert!(
                        x.chord.count() >= y.chord.count(),
                        "{:?} / {:?}",
                        x.chord,
                        y.chord
                    );
                }
            }
        }
        // No default is refused by the rules a user's key goes through.
        assert!(Keymap::default().sanitize().is_empty());
        for c in commands() {
            for d in c.defaults {
                let chord = Chord::parse(d).unwrap();
                assert_eq!(chord.reserved(c.id), None, "{}: {d}", c.id);
                assert_eq!(chord.text(), *d, "defaults are written as they print");
            }
        }
    }

    #[test]
    fn ids_are_unique_and_every_palette_action_is_a_command() {
        let all = commands();
        for (i, c) in all.iter().enumerate() {
            assert!(all[i + 1..].iter().all(|o| o.id != c.id), "{}", c.id);
            assert!(
                all[i + 1..].iter().all(|o| o.action != c.action),
                "{}",
                c.id
            );
        }
        assert!(all.iter().filter(|c| c.palette != Pal::None).count() > 50);
    }

    #[test]
    fn chords_parse_and_print() {
        let p = |t| Chord::parse(t).unwrap();
        assert_eq!(p("Ctrl+Shift+P"), Chord::new(CMD_SHIFT, Key::P));
        assert_eq!(p(" ctrl + shift + p "), Chord::new(CMD_SHIFT, Key::P));
        assert_eq!(p("Cmd+,"), Chord::new(CMD, Key::Comma));
        assert_eq!(p("Ctrl++"), Chord::new(CMD, Key::Plus));
        assert_eq!(p("+"), Chord::new(Modifiers::NONE, Key::Plus));
        assert_eq!(p("Esc"), Chord::new(Modifiers::NONE, Key::Escape));
        assert_eq!(p("Alt+ArrowUp"), Chord::new(Modifiers::ALT, Key::ArrowUp));
        assert_eq!(p("Alt+Up").text(), "Alt+Up");
        assert_eq!(p("Shift+Ctrl+Alt+k").text(), "Ctrl+Shift+Alt+K");
        for bad in ["", "Ctrl+", "Hyper+K", "Ctrl+Nope"] {
            assert!(Chord::parse(bad).is_err(), "{bad:?}");
        }
        // Every key egui knows prints and reads back, with every modifier set.
        for &key in Key::ALL {
            for mods in [Modifiers::NONE, CMD, CMD_SHIFT, Modifiers::ALT] {
                let c = Chord::new(mods, key);
                assert_eq!(Chord::parse(&c.text()), Ok(c), "{}", c.text());
            }
        }
    }

    #[test]
    fn keymap_round_trips_through_config_and_export() {
        let mut map = Keymap::default();
        assert_eq!(
            map.bind("view.theme", Chord::new(CMD, Key::K), false),
            Ok(Bind::Done)
        );
        assert_eq!(
            map.remove("view.refresh", Chord::new(Modifiers::NONE, Key::F5)),
            Ok(())
        );
        // Only differences are stored; an emptied command is an empty list.
        assert_eq!(map.0.len(), 2);
        assert_eq!(map.0["view.theme"], ["Ctrl+K"]);
        assert!(map.0["view.refresh"].is_empty());
        let s = crate::settings::Settings {
            keymap: map.clone(),
            ..Default::default()
        };
        let text = toml::to_string_pretty(&s).unwrap();
        assert!(text.contains("[keymap]"), "{text}");
        let back: crate::settings::Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.keymap, map);
        // A config without [keymap] has the defaults.
        let old: crate::settings::Settings = toml::from_str("theme = 'dark'").unwrap();
        assert_eq!(old.keymap, Keymap::default());
        // Export lists every command; importing it gives the same keymap back.
        let exported = export_text(&map);
        assert!(exported.contains("Ctrl+Shift+P"), "{exported}");
        let (imported, notes) = import_text(&exported).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(imported, map);
        let b = imported.bindings();
        assert_eq!(
            b.label(&Action::ToggleTheme),
            Chord::new(CMD, Key::K).label()
        );
        assert_eq!(b.label(&Action::Refresh), "");
        // A partial file: the rest keeps its defaults; unknown and bad entries are reported.
        let (partial, notes) = import_text(
            "[keymap]\n\"view.theme\" = [\"Ctrl+K\", \"Ctrl+Nope\"]\nfuture = [\"F9\"]\n",
        )
        .unwrap();
        assert_eq!(partial.0.len(), 1);
        assert_eq!(partial.0["view.theme"], ["Ctrl+K"]);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(import_text("keymap = 3").is_err());
    }

    #[test]
    fn conflicts_are_found_and_replaced_only_when_asked() {
        let mut map = Keymap::default();
        let ctrl_e = Chord::new(CMD, Key::E);
        // Ctrl+E is "Filter this folder": asked about, nothing changed yet.
        assert_eq!(
            map.bind("view.theme", ctrl_e, false),
            Ok(Bind::Conflict(vec!["filter"]))
        );
        assert!(map.0.is_empty());
        assert_eq!(map.conflicts("view.theme", ctrl_e), ["filter"]);
        // Replace: the key moves.
        assert_eq!(map.bind("view.theme", ctrl_e, true), Ok(Bind::Done));
        let b = map.bindings();
        assert_eq!(b.label(&Action::FocusFilter), "");
        assert_eq!(b.label(&Action::ToggleTheme), ctrl_e.label());
        assert!(map.conflicts("filter", ctrl_e) == ["view.theme"]);
        // Binding a command's own key again changes nothing.
        assert_eq!(map.bind("view.theme", ctrl_e, false), Ok(Bind::Already));
        // F6 is Switch pane in the panes and Leave the terminal in the terminal: no conflict.
        let f6 = Chord::new(Modifiers::NONE, Key::F6);
        assert!(map.conflicts("terminal.leave", f6).is_empty());
        // Ctrl+` works in both, so it meets both.
        assert_eq!(
            map.conflicts("terminal.leave", Chord::new(CMD, Key::Backtick)),
            ["terminal.toggle"]
        );
        assert_eq!(
            map.conflicts("pane.switch", Chord::new(CMD, Key::Backtick)),
            ["terminal.toggle"]
        );
        // Reset puts it back.
        map.reset("view.theme");
        map.reset("filter");
        assert_eq!(map, Keymap::default());
    }

    #[test]
    fn essential_commands_never_become_unreachable() {
        let mut map = Keymap::default();
        let palette = Chord::new(CMD_SHIFT, Key::P);
        // The palette's only key cannot be removed or taken.
        assert!(map.remove(PALETTE_ID, palette).is_err());
        assert!(map.bind("view.theme", palette, true).is_err());
        assert_eq!(map.bindings().label(&Action::Palette), palette.label());
        // With a second key, the first can go.
        let f1 = Chord::new(Modifiers::NONE, Key::F1);
        assert_eq!(map.bind(PALETTE_ID, f1, false), Ok(Bind::Done));
        assert_eq!(map.remove(PALETTE_ID, palette), Ok(()));
        assert!(map.remove(PALETTE_ID, f1).is_err(), "the last one stays");
        // Plain Esc is the cancel key's alone, and the cancel key keeps it.
        let esc = Chord::new(Modifiers::NONE, Key::Escape);
        assert!(map.bind("view.theme", esc, true).is_err());
        assert!(map.remove(ESCAPE_ID, esc).is_err());
        // Leaving the terminal keeps a key too.
        let mut t = Keymap::default();
        t.remove("terminal.leave", f6()).unwrap();
        assert!(t
            .remove("terminal.leave", Chord::new(Modifiers::SHIFT, Key::Escape))
            .is_err());
        // Reserved: the clipboard's, the zoom keys, Tab, and keys that type text.
        for bad in [
            "Ctrl+C",
            "Ctrl+Shift+V",
            "Ctrl+X",
            "Shift+Insert",
            "Shift+Delete",
            "Ctrl+=",
            "Ctrl+0",
            "Tab",
            "K",
            "Shift+K",
            "5",
            "/",
        ] {
            let c = Chord::parse(bad).unwrap();
            assert!(map.bind("view.theme", c, true).is_err(), "{bad}");
        }
        for good in ["Ctrl+K", "Alt+K", "F9", "Ctrl+Shift+K"] {
            let c = Chord::parse(good).unwrap();
            assert_eq!(map.bind("view.theme", c, false), Ok(Bind::Done), "{good}");
        }
        // A hand-edited file that unbinds the palette gets its default back.
        let mut edited = Keymap::default();
        edited.0.insert(PALETTE_ID.into(), vec![]);
        edited
            .0
            .insert("view.theme".into(), vec!["Escape".into(), "Ctrl+K".into()]);
        let notes = edited.sanitize();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(edited.is_default(PALETTE_ID));
        assert_eq!(edited.0["view.theme"], ["Ctrl+K"]);
    }

    fn f6() -> Chord {
        Chord::new(Modifiers::NONE, Key::F6)
    }

    /// Recording: the next key press binds, Esc cancels, nothing reaches anything else.
    #[test]
    fn recording_binds_the_next_key() {
        let ctx = egui::Context::default();
        let mut ui = KeymapUi {
            recording: Some("view.theme"),
            ..Default::default()
        };
        let mut map = Keymap::default();
        let press = |key, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let mut left = usize::MAX;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![press(Key::J, CMD), Event::Text("j".into())],
                ..Default::default()
            },
            |ctx| {
                ui.capture(ctx, &mut map);
                left = ctx.input(|i| i.events.len());
            },
        );
        assert_eq!(left, 0);
        assert_eq!(ui.recording, None);
        assert_eq!(map.0["view.theme"], ["Ctrl+J"]);
        // A conflicting key waits for the answer.
        ui.recording = Some("view.theme");
        let _ = ctx.run(
            egui::RawInput {
                events: vec![press(Key::E, CMD)],
                ..Default::default()
            },
            |ctx| ui.capture(ctx, &mut map),
        );
        assert_eq!(
            ui.conflict.as_ref().map(|c| c.2.clone()),
            Some(vec!["filter"])
        );
        assert_eq!(map.0["view.theme"], ["Ctrl+J"]);
        // Esc cancels.
        ui.recording = Some("view.theme");
        let _ = ctx.run(
            egui::RawInput {
                events: vec![press(Key::Escape, Modifiers::NONE)],
                ..Default::default()
            },
            |ctx| ui.capture(ctx, &mut map),
        );
        assert_eq!(ui.recording, None);
        assert_eq!(map.0["view.theme"], ["Ctrl+J"]);
    }
}
