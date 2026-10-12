//! Phase 14 Task 67, Finder-style browsing: Quick Look (`quicklook`), the path bar
//! (`path_bar`), spring-loaded folders (`spring`) and Go to Folder (`goto`). Their
//! commands are one `Action` variant, `Action::Finder`, run here; their settings are the
//! `[finder]` table of config.toml.

use crate::jobs::{self, ArchiveSrc, Source};
use crate::keys::Action;
use crate::pane::ViewMode;
use crate::state::AppState;
use crate::tab::Nav;
use keel_vfs::VPath;

/// `[finder]` in config.toml.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct FinderSettings {
    /// Space opens Quick Look; off, Space selects the cursor row as before (Ctrl+Space
    /// always does).
    pub space_quick_look: bool,
    /// The path bar under the file list.
    pub path_bar: bool,
    /// A drag resting on a folder opens it (`spring::DELAY`).
    pub spring_loaded: bool,
}

impl Default for FinderSettings {
    fn default() -> Self {
        Self {
            space_quick_look: true,
            path_bar: true,
            spring_loaded: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum FinderCmd {
    /// The Space key: Quick Look, the selection toggle (setting off, the media view), or
    /// opening the folder under a drag at once.
    Space,
    /// Open or close Quick Look (palette).
    QuickLook,
    /// Ctrl+Shift+G.
    GoToFolder,
    /// View → Show / Hide Path Bar.
    TogglePathBar,
    /// Items dragged from pane `pane` (showing `dir`) dropped on path bar crumb `dst`:
    /// moved, or copied (`copy`, Alt held).
    DropOnCrumb {
        pane: usize,
        dir: VPath,
        paths: Vec<VPath>,
        dst: VPath,
        copy: bool,
    },
    /// The path bar's Copy as Pathname.
    CopyPathname(VPath),
}

/// Quick Look and Go to Folder state.
#[derive(Default)]
pub struct Finder {
    pub quicklook: crate::quicklook::QuickLook,
    pub goto: crate::goto::GoTo,
}

/// First of `AppState::run`'s intercepts: the Space key, and while Quick Look is open,
/// Esc (closes it) and the arrow keys on an index sheet (step through the selected items).
pub fn intercept(s: &mut AppState, p: usize, action: Action) -> Option<Action> {
    let open = s.finder.quicklook.open;
    match action {
        Action::Finder(FinderCmd::Space) => {
            if egui::DragAndDrop::has_any_payload(&s.ctx) {
                crate::spring::space(&s.ctx);
                return None;
            }
            if open {
                s.finder.quicklook.close(&s.ctx);
                return None;
            }
            // The media view's Space opens its own viewer (`media_viewer::intercept`).
            if !s.settings.finder.space_quick_look || s.panes[p].view == ViewMode::Media {
                return Some(Action::ToggleSelect);
            }
            s.finder.quicklook.show();
            None
        }
        Action::ClearFilter if open => {
            s.finder.quicklook.close(&s.ctx);
            None
        }
        Action::Move(nav, false) if open => {
            let len = crate::quicklook::selection(s).len();
            if len == 0 {
                return Some(action);
            }
            let ql = &mut s.finder.quicklook;
            ql.item = match nav {
                Nav::Prev | Nav::Left | Nav::PageUp => crate::quicklook::step(ql.item, len, -1),
                Nav::Next | Nav::Right | Nav::PageDown => crate::quicklook::step(ql.item, len, 1),
                Nav::Home => Some(0),
                Nav::End => Some(len - 1),
            };
            None
        }
        _ => Some(action),
    }
}

pub fn run(s: &mut AppState, p: usize, cmd: FinderCmd) {
    match cmd {
        FinderCmd::Space | FinderCmd::QuickLook => {
            if s.finder.quicklook.open {
                s.finder.quicklook.close(&s.ctx);
            } else {
                s.finder.quicklook.show();
            }
        }
        FinderCmd::GoToFolder => {
            let base = s.panes[p].tabs[s.panes[p].active].dir.clone();
            s.finder.goto.show(base);
        }
        FinderCmd::TogglePathBar => s.settings.finder.path_bar = !s.settings.finder.path_bar,
        FinderCmd::CopyPathname(path) => {
            s.ctx.copy_text(path.display());
            s.toasts.info("Copied path");
        }
        FinderCmd::DropOnCrumb {
            pane,
            dir,
            paths,
            dst,
            copy,
        } => drop_on_crumb(s, p, pane, dir, paths, dst, copy),
    }
}

/// A path bar drop: a move (a copy with `copy`) into `dst`, refused like a drop on a
/// folder row (into the trash, into an archive). Entries dragged out of an archive
/// extract; library items go the way a row drop sends them.
fn drop_on_crumb(
    s: &mut AppState,
    p: usize,
    pane: usize,
    dir: VPath,
    paths: Vec<VPath>,
    dst: VPath,
    copy: bool,
) {
    if paths.is_empty() || dir == dst || paths.contains(&dst) {
        return;
    }
    if let Some(src) = ArchiveSrc::picked(&dir, &paths) {
        return s.run(p, Action::Extract { src, dst });
    }
    let drop = Action::Drop {
        paths: paths.clone(),
        from: Some((pane, dir)),
        dst: dst.clone(),
    };
    let library = keel_vfs::library::SCHEME;
    if dst.scheme == library || paths.iter().any(|x| x.scheme == library) {
        return s.run(p, drop);
    }
    if crate::trash_ui::refuses(&drop, false) {
        return s.toasts.error(format!(
            "The {} is read-only; restore items to work with them",
            keel_vfs::trashbin::label()
        ));
    }
    if let Some(why) = crate::state::archive_refusal(&dst) {
        return s.toasts.error(why);
    }
    if !jobs::spawn_plan(
        Source::Paths(paths, !copy),
        dst,
        s.router.clone(),
        s.tx.clone(),
        s.ctx.clone(),
    ) {
        s.toasts.error("Could not start the transfer");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finder::FinderCmd::*;
    use keel_vfs::Router;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn temp(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-finder-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            match f.strip_suffix('/') {
                Some(d) => std::fs::create_dir_all(dir.join(d)).unwrap(),
                None => std::fs::write(dir.join(f), b"text").unwrap(),
            }
        }
        dir
    }

    /// Worker answers applied until `done` (or 10 s).
    fn pump_until(s: &mut AppState, done: impl Fn(&mut AppState) -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done(s) && Instant::now() < until {
            s.tick();
            s.jobs.tick();
            if let Ok(msg) = s.rx.recv_timeout(Duration::from_millis(10)) {
                s.apply(msg);
            }
        }
        assert!(done(s), "timed out");
    }

    fn state(dir: &Path) -> AppState {
        let mut s = AppState::new(
            egui::Context::default(),
            Arc::new(Router::new()),
            VPath::local(dir),
        );
        s.dual = false;
        pump_until(&mut s, |s| !s.tab(0).loading);
        s.tab_mut(0).visible(false);
        s
    }

    fn shown(s: &mut AppState) -> Option<String> {
        crate::quicklook::target(s).map(|e| e.name)
    }

    fn frame(s: &mut AppState) -> Vec<(usize, Action)> {
        let ctx = s.ctx.clone();
        let mut out = Vec::new();
        let _ = ctx.run(Default::default(), |ctx| {
            crate::quicklook::ui(ctx, s, &mut out)
        });
        out
    }

    #[test]
    fn space_opens_quick_look_the_arrows_move_and_space_or_esc_close() {
        let dir = temp("ql", &["a.txt", "b.txt", "c.txt"]);
        let mut s = state(&dir);
        s.tab_mut(0).cursor = Some("a.txt".into());
        s.run(0, Action::Finder(Space));
        assert!(s.finder.quicklook.open);
        assert!(s.tab(0).selected.is_empty(), "Space no longer selects");
        assert_eq!(shown(&mut s).as_deref(), Some("a.txt"));
        frame(&mut s);
        s.run(0, Action::Move(Nav::Next, false));
        assert_eq!(s.tab(0).cursor.as_deref(), Some("b.txt"));
        assert_eq!(
            shown(&mut s).as_deref(),
            Some("b.txt"),
            "follows the cursor"
        );
        frame(&mut s);
        s.run(0, Action::Finder(Space));
        assert!(!s.finder.quicklook.open);
        s.run(0, Action::Finder(QuickLook));
        assert!(s.finder.quicklook.open);
        s.tab_mut(0).filter = "b".into();
        s.run(0, Action::ClearFilter);
        assert!(!s.finder.quicklook.open, "Esc closes");
        assert_eq!(s.tab(0).filter, "b", "and leaves the filter alone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_selects_as_before_with_the_setting_off_and_ctrl_space_always() {
        let dir = temp("ql-off", &["a.txt", "b.txt"]);
        let mut s = state(&dir);
        s.tab_mut(0).cursor = Some("a.txt".into());
        s.run(0, Action::ToggleSelect);
        assert!(s.tab(0).selected.contains("a.txt"), "Ctrl+Space");
        assert!(!s.finder.quicklook.open);
        s.settings.finder.space_quick_look = false;
        s.tab_mut(0).cursor = Some("b.txt".into());
        s.run(0, Action::Finder(Space));
        assert!(s.tab(0).selected.contains("b.txt"));
        assert!(!s.finder.quicklook.open);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn several_selected_open_an_index_sheet_the_arrows_step_through() {
        let dir = temp("ql-sheet", &["a.txt", "b.txt", "c.txt", "d.txt"]);
        let mut s = state(&dir);
        for n in ["a.txt", "b.txt", "c.txt"] {
            s.tab_mut(0).selected.insert(n.into());
        }
        s.tab_mut(0).cursor = Some("c.txt".into());
        s.run(0, Action::Finder(Space));
        assert_eq!(s.finder.quicklook.item, None, "the index sheet");
        assert!(frame(&mut s).is_empty());
        s.run(0, Action::Move(Nav::Right, false));
        assert_eq!(shown(&mut s).as_deref(), Some("a.txt"));
        s.run(0, Action::Move(Nav::Left, false));
        assert_eq!(shown(&mut s).as_deref(), Some("c.txt"), "wraps");
        assert_eq!(s.tab(0).selected.len(), 3, "the selection stays");
        assert_eq!(s.tab(0).cursor.as_deref(), Some("c.txt"));
        frame(&mut s);
        s.run(0, Action::Move(Nav::Home, false));
        assert_eq!(shown(&mut s).as_deref(), Some("a.txt"));
        // Shift+arrow still extends the selection in the list.
        s.run(0, Action::Move(Nav::Next, true));
        assert_eq!(s.tab(0).cursor.as_deref(), Some("d.txt"));
        frame(&mut s);
        s.run(0, Action::Finder(Space));
        assert!(!s.finder.quicklook.open && s.finder.quicklook.item.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn crumb_drop(s: &mut AppState, dir: &Path, name: &str, dst: &Path, copy: bool) {
        let cmd = DropOnCrumb {
            pane: 0,
            dir: VPath::local(dir),
            paths: vec![VPath::local(dir.join(name))],
            dst: VPath::local(dst),
            copy,
        };
        s.run(0, Action::Finder(cmd));
    }

    #[test]
    fn a_path_bar_drop_moves_and_alt_copies() {
        let root = temp("crumb", &["sub/", "moved.txt", "copied.txt"]);
        let sub = root.join("sub");
        let mut s = state(&root);
        // Onto the crumb of the folder the items are in: nothing to do.
        crumb_drop(&mut s, &root, "moved.txt", &root, false);
        assert!(s.jobs.list.is_empty());
        crumb_drop(&mut s, &root, "moved.txt", &sub, false);
        pump_until(&mut s, |_| {
            sub.join("moved.txt").exists() && !root.join("moved.txt").exists()
        });
        crumb_drop(&mut s, &root, "copied.txt", &sub, true);
        pump_until(&mut s, |_| sub.join("copied.txt").exists());
        assert!(root.join("copied.txt").exists(), "Alt copies");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_path_bar_drop_into_the_trash_is_refused() {
        let root = temp("crumb-trash", &["a.txt"]);
        let mut s = state(&root);
        let cmd = DropOnCrumb {
            pane: 0,
            dir: VPath::local(&root),
            paths: vec![VPath::local(root.join("a.txt"))],
            dst: keel_vfs::trashbin::root(),
            copy: false,
        };
        s.run(0, Action::Finder(cmd));
        assert!(s.jobs.list.is_empty());
        assert!(root.join("a.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn path_bar_and_go_to_folder_commands() {
        let root = temp("cmds", &["sub/"]);
        let mut s = state(&root);
        let shown = s.settings.finder.path_bar;
        s.run(0, Action::Finder(TogglePathBar));
        assert_eq!(s.settings.finder.path_bar, !shown);
        s.run(0, Action::Finder(GoToFolder));
        assert!(s.finder.goto.open);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn settings_round_trip_with_defaults_on() {
        let d = FinderSettings::default();
        assert!(d.space_quick_look && d.path_bar && d.spring_loaded);
        let mut s = crate::settings::Settings::default();
        s.finder.spring_loaded = false;
        let text = toml::to_string_pretty(&s).unwrap();
        assert!(text.contains("[finder]"));
        let (back, dropped) = crate::settings::parse_lenient(&text).unwrap();
        assert!(dropped.is_empty());
        assert_eq!(back.finder, s.finder);
        // An older config.toml without the table gets the defaults.
        let (old, _) = crate::settings::parse_lenient("theme = \"dark\"").unwrap();
        assert_eq!(old.finder, FinderSettings::default());
    }
}
