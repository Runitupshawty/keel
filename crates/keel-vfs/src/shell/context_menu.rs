//! The Windows shell context menu (Task 43): the menu Explorer shows for files, with the
//! installed handlers, Send to, Open with and Share, as a native popup ([`ShellMenu::popup`])
//! or as a flat list for the command palette ([`ShellMenu::items`]).
//!
//! Call everything on the UI thread. The popup's owner window must belong to the calling
//! thread (a window subclass cannot cross threads), and handlers that finish their work
//! later post to that thread's message queue, which winit keeps pumping. Shell extensions
//! run inside this process: a crashing handler takes the window with it, a hanging one
//! blocks it, as in Explorer. So the menu is only built on request, never prefetched.
//!
//! The canonical verbs in [`FILTERED_VERBS`] are removed: Keel's own entries (preview,
//! trash, conflict prompts, the in-app Properties) stand for them.

use anyhow::{Context, Result};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR, PWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DefSubclassProc, IContextMenu, IContextMenu2, IContextMenu3, ILFindLastID, ILFree,
    IShellFolder, RemoveWindowSubclass, SHBindToParent, SHGetPathFromIDListEx, SHParseDisplayName,
    SetWindowSubclass, CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO,
    CMINVOKECOMMANDINFOEX, GCS_VERBW, GPFIDL_DEFAULT, SEE_MASK_UNICODE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DeleteMenu, DestroyMenu, EnumThreadWindows, GetCursorPos, GetMenuItemCount,
    GetMenuItemInfoW, GetWindow, IsWindowVisible, SetForegroundWindow, TrackPopupMenuEx, GW_OWNER,
    HMENU, MENUITEMINFOW, MFS_DISABLED, MFT_SEPARATOR, MF_BYPOSITION, MIIM_FTYPE, MIIM_ID,
    MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};

/// Canonical verbs left out of the menu: Keel's Delete (trash, with its prompt), Rename,
/// Cut, Copy, Paste (conflict prompts), Create shortcut and Properties stand for them.
pub const FILTERED_VERBS: &[&str] = &[
    "delete",
    "rename",
    "cut",
    "copy",
    "paste",
    "pastelink",
    "link",
    "properties",
];

/// Whether the canonical verb `verb` is one Keel removes.
pub fn filtered(verb: &str) -> bool {
    FILTERED_VERBS.iter().any(|v| v.eq_ignore_ascii_case(verb))
}

/// The command ids handed to the handlers.
const FIRST: u32 = 1;
const LAST: u32 = 0x7FFF;
const SUBCLASS_ID: usize = 0x4B43_4D31; // "KCM1"

/// One command of the menu, for the palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The labels from the top: `Send to > Desktop (create shortcut)`.
    pub label: String,
    /// What [`ShellMenu::invoke`] takes.
    pub id: u32,
}

/// The shell menu for files in one folder, built and filtered.
pub struct ShellMenu {
    cm: IContextMenu,
    menu: HMENU,
    /// The folder, NUL-terminated (`lpDirectoryW`).
    dir: Vec<u16>,
}

impl Drop for ShellMenu {
    fn drop(&mut self) {
        // SAFETY: `menu` came from CreatePopupMenu and is destroyed once.
        let _ = unsafe { DestroyMenu(self.menu) };
    }
}

/// A PIDL from `SHParseDisplayName`, freed on drop.
struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        // SAFETY: allocated by the shell; ILFree takes null too.
        unsafe { ILFree(Some(self.0)) };
    }
}

/// Whether a name in `p` ends with a dot or space: the shell trims those when it parses a
/// path, so `foo.` would name the sibling `foo`.
fn trimmed_by_shell(p: &Path) -> bool {
    p.components().any(|c| {
        matches!(c, std::path::Component::Normal(n) if n.to_string_lossy().ends_with(['.', ' ']))
    })
}

/// The file-system path `pidl` names, if it names one.
///
/// # Safety
/// `pidl` is a valid absolute PIDL.
unsafe fn pidl_path(pidl: *const ITEMIDLIST) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32_768];
    SHGetPathFromIDListEx(pidl, &mut buf, GPFIDL_DEFAULT)
        .as_bool()
        .then(|| {
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            PathBuf::from(String::from_utf16_lossy(&buf[..n]))
        })
}

/// Whether the shell's `got` is the `want` Keel asked for (NTFS names ignore case; a
/// folder given by its 8.3 short name, `RUNNER~1`, is the same folder as its long name).
fn same_path(got: &Path, want: &Path) -> bool {
    let norm = |p: &Path| {
        (long_form(p)
            .to_string_lossy()
            .trim_end_matches('\\')
            .to_lowercase())
        .replace('/', "\\")
    };
    norm(got) == norm(want)
}

/// `p` with its parent folder in long form (short 8.3 components expanded), the file name
/// kept exactly as given so a trailing dot or space still tells it apart from its sibling.
fn long_form(p: &Path) -> PathBuf {
    let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
        return p.to_path_buf();
    };
    match std::fs::canonicalize(parent) {
        Ok(long) => {
            let long = long.to_string_lossy();
            PathBuf::from(long.strip_prefix(r"\\?\").unwrap_or(&long)).join(name)
        }
        Err(_) => p.to_path_buf(),
    }
}

fn wide(p: &Path) -> Result<Vec<u16>> {
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    anyhow::ensure!(!w[..w.len() - 1].contains(&0), "path contains NUL");
    Ok(w)
}

impl ShellMenu {
    /// The menu for `paths`, which must all be in one local folder (UNC shares count).
    /// `extended`: the Shift+right-click verbs too (`CMF_EXTENDEDVERBS`). Refuses a name
    /// ending in a dot or space, and any path the shell resolves to a different file.
    pub fn new(paths: &[PathBuf], extended: bool) -> Result<Self> {
        let first = paths.first().context("nothing selected")?;
        anyhow::ensure!(
            paths.iter().all(|p| p.parent() == first.parent()),
            "the Windows menu works on files in one folder"
        );
        if let Some(p) = paths.iter().find(|p| trimmed_by_shell(p)) {
            anyhow::bail!(
                "{}: the Windows menu cannot act on a name ending in a dot or space",
                p.display()
            );
        }
        let dir = wide(first.parent().unwrap_or(first))?;
        // SAFETY: COM init on this thread (S_FALSE / RPC_E_CHANGED_MODE are harmless); each
        // PIDL lives until GetUIObjectOf returns, and `children` point into them.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let mut pidls = Vec::with_capacity(paths.len());
            for p in paths {
                let w = wide(p)?;
                let mut pidl = std::ptr::null_mut();
                SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None)
                    .with_context(|| p.display().to_string())?;
                let pidl = Pidl(pidl);
                // The handlers act on what the PIDL names: refuse anything else.
                anyhow::ensure!(
                    pidl_path(pidl.0).is_some_and(|got| same_path(&got, p)),
                    "{}: the shell resolved this to a different item",
                    p.display()
                );
                pidls.push(pidl);
            }
            let folder: IShellFolder = SHBindToParent(pidls[0].0, None)?;
            let children: Vec<*const ITEMIDLIST> = pidls
                .iter()
                .map(|p| ILFindLastID(p.0) as *const ITEMIDLIST)
                .collect();
            let cm: IContextMenu = folder.GetUIObjectOf(HWND::default(), &children, None)?;
            let me = Self {
                cm,
                menu: CreatePopupMenu()?,
                dir,
            };
            let flags = CMF_NORMAL | if extended { CMF_EXTENDEDVERBS } else { 0 };
            me.cm
                .QueryContextMenu(me.menu, 0, FIRST, LAST, flags)
                .context("QueryContextMenu")?;
            me.strip(me.menu);
            Ok(me)
        }
    }

    /// The canonical verb of command `id`, when its handler names one.
    fn verb(&self, id: u32) -> Option<String> {
        if !(FIRST..=LAST).contains(&id) {
            return None;
        }
        let mut buf = [0u16; 256];
        // SAFETY: GCS_VERBW writes at most `cchmax` UTF-16 units into `buf`.
        unsafe {
            self.cm.GetCommandString(
                (id - FIRST) as usize,
                GCS_VERBW,
                None,
                PSTR(buf.as_mut_ptr().cast()),
                buf.len() as u32,
            )
        }
        .ok()?;
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n])).filter(|v| !v.is_empty())
    }

    /// Removes the filtered verbs from `menu` (and its filled submenus), then the
    /// separators left doubled or at an end.
    fn strip(&self, menu: HMENU) {
        for i in (0..count(menu)).rev() {
            let Some(info) = item_info(menu, i, None) else {
                continue;
            };
            if !info.hSubMenu.is_invalid() {
                self.strip(info.hSubMenu);
            } else if self.verb(info.wID).is_some_and(|v| filtered(&v)) {
                // SAFETY: a valid position of a menu this object owns.
                let _ = unsafe { DeleteMenu(menu, i, MF_BYPOSITION) };
            }
        }
        tidy_separators(menu);
    }

    /// Lets the handler fill submenu `sub` (at `pos`), as a popup would on opening it, then
    /// filters it: Send to and Open with stay empty until then.
    fn open_submenu(&self, sub: HMENU, pos: u32) {
        let (w, l) = (WPARAM(sub.0 as usize), LPARAM(pos as isize));
        // SAFETY: the message the handler expects before its submenu shows.
        unsafe {
            if let Ok(cm3) = self.cm.cast::<IContextMenu3>() {
                let _ = cm3.HandleMenuMsg2(WM_INITMENUPOPUP, w, l, None);
            } else if let Ok(cm2) = self.cm.cast::<IContextMenu2>() {
                let _ = cm2.HandleMenuMsg(WM_INITMENUPOPUP, w, l);
            }
        }
        self.strip(sub);
    }

    fn nodes(&self) -> Vec<Node> {
        let mut nodes = Vec::new();
        walk(
            self.menu,
            "",
            &mut |s, p| self.open_submenu(s, p),
            &mut nodes,
        );
        nodes
    }

    /// Every command with a label, submenus filled, for the palette.
    pub fn items(&self) -> Vec<Item> {
        (self.nodes().into_iter())
            .filter(|n| !n.sub && (FIRST..=LAST).contains(&n.id))
            .map(|n| Item {
                label: n.label,
                id: n.id,
            })
            .collect()
    }

    /// The canonical verbs in the menu (submenus filled), for tests.
    pub fn verbs(&self) -> Vec<String> {
        (self.nodes().iter())
            .filter_map(|n| self.verb(n.id))
            .collect()
    }

    /// Shows the menu as Explorer does, at `at` (screen pixels) or else at the pointer, on
    /// this thread's main window, and returns the command picked, if any.
    pub fn popup(&self, at: Option<(i32, i32)>) -> Result<Option<u32>> {
        let hwnd = own_window().context("Keel's window was not found")?;
        let pt = match at {
            Some((x, y)) => POINT { x, y },
            None => cursor(),
        };
        let fwd = Forward {
            menu: self,
            cm3: self.cm.cast().ok(),
            cm2: self.cm.cast().ok(),
        };
        // SAFETY: `fwd` outlives the subclass, which is removed before it is dropped; the
        // window belongs to this thread (own_window).
        let cmd = unsafe {
            anyhow::ensure!(
                SetWindowSubclass(hwnd, Some(forward), SUBCLASS_ID, &fwd as *const _ as usize)
                    .as_bool(),
                "SetWindowSubclass failed"
            );
            let _ = SetForegroundWindow(hwnd);
            let flags = (TPM_RETURNCMD | TPM_RIGHTBUTTON).0;
            let cmd = TrackPopupMenuEx(self.menu, flags, pt.x, pt.y, hwnd, None).0 as u32;
            let _ = RemoveWindowSubclass(hwnd, Some(forward), SUBCLASS_ID);
            cmd
        };
        Ok((FIRST..=LAST).contains(&cmd).then_some(cmd))
    }

    /// Runs command `id` (from [`popup`](Self::popup) or [`items`](Self::items)).
    pub fn invoke(&self, id: u32) -> Result<()> {
        anyhow::ensure!((FIRST..=LAST).contains(&id), "no such command");
        if let Some(v) = self.verb(id).filter(|v| filtered(v)) {
            anyhow::bail!("\"{v}\" is Keel's own command");
        }
        if crate::headless::skip(format_args!("shell menu command {id}")) {
            return Ok(());
        }
        let offset = (id - FIRST) as usize;
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: SEE_MASK_UNICODE | CMIC_MASK_PTINVOKE,
            hwnd: own_window().unwrap_or_default(),
            // MAKEINTRESOURCE: a command offset, not a string.
            lpVerb: PCSTR(offset as *const u8),
            lpVerbW: PCWSTR(offset as *const u16),
            lpDirectoryW: PCWSTR(self.dir.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ptInvoke: cursor(),
            ..Default::default()
        };
        // SAFETY: `info` and `dir` outlive the call; an EX block starts like the plain one.
        unsafe {
            self.cm
                .InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO)
        }
        .context("InvokeCommand")
    }
}

fn cursor() -> POINT {
    let mut pt = POINT::default();
    // SAFETY: writes one POINT.
    let _ = unsafe { GetCursorPos(&mut pt) };
    pt
}

/// This thread's main window: visible, unowned, of winit's class.
fn own_window() -> Option<HWND> {
    unsafe extern "system" fn find(hwnd: HWND, found: LPARAM) -> BOOL {
        let main = IsWindowVisible(hwnd).as_bool()
            && GetWindow(hwnd, GW_OWNER).is_err()
            && crate::desktop::is_winit(hwnd);
        if main {
            *(found.0 as *mut Option<HWND>) = Some(hwnd);
        }
        BOOL::from(!main)
    }
    let mut found: Option<HWND> = None;
    // SAFETY: `found` outlives the enumeration.
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(find),
            LPARAM(&mut found as *mut _ as isize),
        );
    }
    found
}

/// What the subclass forwards the menu's messages to while it is open.
struct Forward<'a> {
    menu: &'a ShellMenu,
    cm3: Option<IContextMenu3>,
    cm2: Option<IContextMenu2>,
}

/// Owner-drawn items, lazily filled submenus (Send to, Open with) and their access keys
/// only work when the owner window hands these messages to the handler.
unsafe extern "system" fn forward(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let fwd = &*(data as *const Forward);
        let mut result = LRESULT(0);
        let handled = if let Some(cm3) = &fwd.cm3 {
            cm3.HandleMenuMsg2(msg, w, l, Some(&mut result)).is_ok()
        } else if let Some(cm2) = fwd.cm2.as_ref().filter(|_| msg != WM_MENUCHAR) {
            // Owner-draw messages answer TRUE when handled.
            result = LRESULT((msg != WM_INITMENUPOPUP) as isize);
            cm2.HandleMenuMsg(msg, w, l).is_ok()
        } else {
            false
        };
        if msg == WM_INITMENUPOPUP {
            fwd.menu.strip(HMENU(w.0 as *mut _));
        }
        if handled {
            return result;
        }
    }
    DefSubclassProc(hwnd, msg, w, l)
}

fn count(menu: HMENU) -> u32 {
    // SAFETY: -1 on an invalid menu, read as none.
    unsafe { GetMenuItemCount(menu) }.max(0) as u32
}

const INFO_MASK: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_MASK =
    windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_MASK(
        MIIM_FTYPE.0 | MIIM_STATE.0 | MIIM_ID.0 | MIIM_SUBMENU.0,
    );

/// Item `pos` of `menu`: type, state, id and submenu, plus its text into `text` if given
/// (sized by a first call).
fn item_info(menu: HMENU, pos: u32, text: Option<&mut Vec<u16>>) -> Option<MENUITEMINFOW> {
    let mut info = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: INFO_MASK,
        ..Default::default()
    };
    // SAFETY: `info` is sized; the third call's buffer holds `cch` + 1 units.
    unsafe {
        GetMenuItemInfoW(menu, pos, true, &mut info).ok()?;
        if let Some(text) = text {
            text.clear();
            let mut s = MENUITEMINFOW {
                cbSize: info.cbSize,
                fMask: MIIM_STRING,
                ..Default::default()
            };
            if GetMenuItemInfoW(menu, pos, true, &mut s).is_ok() && s.cch > 0 {
                text.resize(s.cch as usize + 1, 0);
                s.dwTypeData = PWSTR(text.as_mut_ptr());
                s.cch += 1;
                if GetMenuItemInfoW(menu, pos, true, &mut s).is_ok() {
                    text.truncate(s.cch as usize);
                } else {
                    text.clear();
                }
            }
        }
    }
    Some(info)
}

fn is_separator(menu: HMENU, pos: u32) -> bool {
    item_info(menu, pos, None).is_some_and(|i| i.fType.0 & MFT_SEPARATOR.0 != 0)
}

/// Deletes separators at either end and the second of each pair.
fn tidy_separators(menu: HMENU) {
    let mut after_sep = true; // the end counts as one
    for i in (0..count(menu)).rev() {
        let sep = is_separator(menu, i);
        if sep && after_sep {
            // SAFETY: a valid position, walked from the end.
            let _ = unsafe { DeleteMenu(menu, i, MF_BYPOSITION) };
            continue;
        }
        after_sep = sep;
    }
    if count(menu) > 0 && is_separator(menu, 0) {
        // SAFETY: as above.
        let _ = unsafe { DeleteMenu(menu, 0, MF_BYPOSITION) };
    }
}

/// A menu label as shown: `&` access keys dropped (`&&` is a literal `&`), and the
/// shortcut after a tab cut off.
fn clean(text: &str) -> String {
    let text = text.split('\t').next().unwrap_or("");
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '&' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_owned()
}

/// One usable item met by [`walk`].
#[derive(Debug, PartialEq, Eq)]
struct Node {
    label: String,
    id: u32,
    sub: bool,
}

/// Every item of `menu` a user could pick, depth first, labelled with its path from the
/// top: separators, disabled items and items without text are skipped (with their
/// submenus). `open` fills a submenu before it is walked.
fn walk(menu: HMENU, prefix: &str, open: &mut dyn FnMut(HMENU, u32), out: &mut Vec<Node>) {
    let mut text = Vec::new();
    for pos in 0..count(menu) {
        let Some(info) = item_info(menu, pos, Some(&mut text)) else {
            continue;
        };
        let label = clean(&String::from_utf16_lossy(&text));
        if info.fType.0 & MFT_SEPARATOR.0 != 0
            || info.fState.0 & MFS_DISABLED.0 != 0
            || label.is_empty()
        {
            continue;
        }
        let path = if prefix.is_empty() {
            label
        } else {
            format!("{prefix} > {label}")
        };
        let sub = !info.hSubMenu.is_invalid();
        if sub {
            open(info.hSubMenu, pos);
            walk(info.hSubMenu, &path, open, out);
        }
        out.push(Node {
            label: path,
            id: info.wID,
            sub,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    };

    #[test]
    fn filter_removes_keels_own_verbs() {
        for v in [
            "delete",
            "Delete",
            "RENAME",
            "cut",
            "copy",
            "paste",
            "pastelink",
            "link",
            "properties",
        ] {
            assert!(filtered(v), "{v}");
        }
        for v in ["open", "openas", "edit", "print", "runas", "copyaspath", ""] {
            assert!(!filtered(v), "{v}");
        }
    }

    #[test]
    fn labels_lose_access_keys_and_shortcuts() {
        assert_eq!(clean("&Open"), "Open");
        assert_eq!(clean("Save && &close\tCtrl+S"), "Save & close");
        assert_eq!(clean("Trailing&"), "Trailing");
        assert_eq!(clean("  \t"), "");
    }

    struct Menu(HMENU);
    impl Drop for Menu {
        fn drop(&mut self) {
            unsafe { DestroyMenu(self.0).unwrap() };
        }
    }

    fn labels(m: HMENU) -> Vec<String> {
        let mut text = Vec::new();
        (0..count(m))
            .map(|i| {
                let info = item_info(m, i, Some(&mut text)).unwrap();
                if info.fType.0 & MFT_SEPARATOR.0 != 0 {
                    "-".to_owned()
                } else {
                    String::from_utf16_lossy(&text)
                }
            })
            .collect()
    }

    /// A real HMENU: a submenu, a separator, a disabled item, an item without text.
    #[test]
    fn walk_lists_what_a_user_can_pick() {
        unsafe {
            let top = Menu(CreatePopupMenu().unwrap());
            let send = CreatePopupMenu().unwrap(); // owned by `top` once appended
            AppendMenuW(send, MF_STRING, 10, w!("&Desktop (create shortcut)")).unwrap();
            AppendMenuW(send, MF_GRAYED, 11, w!("Fax recipient")).unwrap();
            AppendMenuW(top.0, MF_STRING, 1, w!("&Open")).unwrap();
            AppendMenuW(top.0, MF_SEPARATOR, 0, PCWSTR::null()).unwrap();
            AppendMenuW(top.0, MF_POPUP, send.0 as usize, w!("Se&nd to")).unwrap();
            AppendMenuW(top.0, MF_GRAYED, 3, w!("Disabled")).unwrap();
            AppendMenuW(top.0, MF_STRING, 4, w!("")).unwrap();
            AppendMenuW(top.0, MF_STRING, 5, w!("Edit\tCtrl+E")).unwrap();
            let mut opened = Vec::new();
            let mut out = Vec::new();
            walk(top.0, "", &mut |s, p| opened.push((s.0, p)), &mut out);
            assert_eq!(
                opened,
                [(send.0, 2)],
                "the submenu is filled before its walk"
            );
            let got: Vec<(&str, bool)> = (out.iter()).map(|n| (n.label.as_str(), n.sub)).collect();
            assert_eq!(
                got,
                [
                    ("Open", false),
                    ("Send to > Desktop (create shortcut)", false),
                    ("Send to", true),
                    ("Edit", false),
                ]
            );
            let ids: Vec<u32> = out.iter().filter(|n| !n.sub).map(|n| n.id).collect();
            assert_eq!(ids, [1, 10, 5]);
        }
    }

    #[test]
    fn separators_are_tidied() {
        unsafe {
            let m = Menu(CreatePopupMenu().unwrap());
            for item in [None, Some(w!("a")), None, None, Some(w!("b")), None] {
                match item {
                    Some(text) => AppendMenuW(m.0, MF_STRING, 1, text).unwrap(),
                    None => AppendMenuW(m.0, MF_SEPARATOR, 0, PCWSTR::null()).unwrap(),
                }
            }
            tidy_separators(m.0);
            assert_eq!(labels(m.0), ["a", "-", "b"]);
        }
    }

    #[test]
    fn names_the_shell_would_trim_are_refused() {
        assert!(trimmed_by_shell(Path::new(r"C:\a\foo.")));
        assert!(trimmed_by_shell(Path::new(r"C:\a \foo")));
        assert!(!trimmed_by_shell(Path::new(r"C:\a\foo.txt")));
        assert!(same_path(Path::new(r"C:\A\Foo"), Path::new(r"c:\a\foo")));
        assert!(!same_path(Path::new(r"C:\a\foo"), Path::new(r"C:\a\foo.")));
    }

    /// `foo.` beside `foo`: SHParseDisplayName would resolve `foo.` to `foo`, so the menu is
    /// refused for it and still built for `foo`, whose PIDL round-trips. Nothing is invoked.
    #[test]
    fn trailing_dot_never_reaches_the_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("foo");
        std::fs::write(&plain, "plain").unwrap();
        let dotted = dir.path().join("foo.");
        let mut raw = std::ffi::OsString::from(r"\\?\");
        raw.push(dotted.as_os_str());
        std::fs::write(&raw, "dotted").unwrap();
        let err = ShellMenu::new(std::slice::from_ref(&dotted), false)
            .err()
            .expect("foo. refused");
        assert!(format!("{err:#}").contains("dot or space"), "{err:#}");
        // The round trip: the parsed PIDL of `foo` names `foo`; of `foo.` (asked of the
        // shell directly) names `foo`, which the check refuses.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            for (asked, ok) in [(&plain, true), (&dotted, false)] {
                let w = wide(asked).unwrap();
                let mut pidl = std::ptr::null_mut();
                SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None).unwrap();
                let pidl = Pidl(pidl);
                let got = pidl_path(pidl.0).unwrap();
                assert_eq!(
                    same_path(&got, asked),
                    ok,
                    "{} -> {}",
                    asked.display(),
                    got.display()
                );
            }
        }
    }

    /// The real shell menu of a temporary .txt file: Open with is there, Keel's own verbs
    /// are not. Verbs, never the localized labels. Nothing is invoked.
    #[test]
    #[ignore = "loads the installed shell extensions"]
    fn shell_menu_of_a_text_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let menu = ShellMenu::new(&[file], false).unwrap();
        let verbs = menu.verbs();
        let lower: Vec<String> = verbs.iter().map(|v| v.to_ascii_lowercase()).collect();
        assert!(lower.iter().any(|v| v == "openas"), "{verbs:?}");
        for gone in FILTERED_VERBS {
            assert!(!lower.iter().any(|v| v == gone), "{gone} in {verbs:?}");
        }
        assert!(!menu.items().is_empty());
    }
}
