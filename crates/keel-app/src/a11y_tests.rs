//! Accessibility: the AccessKit tree a screen reader reads, keyboard-only use (Tab walks,
//! Esc leaves), on the screenshot fixture. Nothing here speaks or plays sound: the tree
//! is read from the harness.

use crate::app::{App, Boot};
use crate::screenshots::{listed, neutral_sidebar, wait, write_fixture};
use crate::session::Session;
use egui::accesskit::Role;
use egui::{Key, Modifiers};
use egui_kittest::kittest::{by, Node, Queryable};
use egui_kittest::Harness;
use keel_vfs::VPath;
use std::path::PathBuf;

type H = Harness<'static, App>;

/// A fixture of its own per test: another test's views (media sidecars, thumbnails) must
/// not refresh this one's listing in the middle of a Tab walk.
fn demo(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("keel-a11y-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    write_fixture(&root);
    root
}

fn app(test: &str) -> H {
    let root = demo(test);
    let root = root.as_path();
    let (l, r) = (
        VPath::local(root.join("Documents")),
        VPath::local(root.join("Pictures")),
    );
    let boot = Boot {
        session: Session {
            panes: vec![vec![l.clone()], vec![r]],
            ..Session::single(l.clone())
        },
        ..Boot::at(l)
    };
    let mut h = Harness::builder()
        .with_size(egui::vec2(1280.0, 800.0))
        .build_eframe(move |cc| App::new(cc, boot));
    neutral_sidebar(&mut h.state_mut().state, root);
    assert!(wait(&mut h, 30, &listed), "both panes listed");
    h
}

/// Every node of the tree, depth first.
fn nodes(h: &H) -> Vec<Node<'_>> {
    h.query_all(by()).collect()
}

fn interactive(n: &Node<'_>) -> bool {
    n.is_clickable() || n.is_focusable()
}

/// Interactive nodes without an accessible name, as text.
fn unnamed(h: &H) -> Vec<String> {
    nodes(h)
        .into_iter()
        .filter(|n| interactive(n) && !n.is_hidden())
        .filter(|n| n.label().is_none_or(|l| l.trim().is_empty()) && n.value().is_none())
        .map(|n| format!("{:?} at {:?}", n.role(), n.raw_bounds()))
        .collect()
}

fn dump(h: &H) -> String {
    nodes(h)
        .iter()
        .filter(|n| interactive(n) || n.label().is_some() || n.value().is_some())
        .map(|n| {
            format!(
                "{:?} {:?} {:?} focus={} {:?}\n",
                n.role(),
                n.label(),
                n.value(),
                n.is_focused(),
                n.raw_bounds().map(|b| (b.x0 as i32, b.y0 as i32))
            )
        })
        .collect()
}

fn press(h: &mut H, mods: Modifiers, key: Key) {
    if mods == Modifiers::NONE {
        h.press_key(key);
    } else {
        h.press_key_modifiers(mods, key);
    }
    h.run_steps(3);
}

/// The node holding the keyboard focus (AccessKit's tree focus), as (role, name).
fn focus(h: &H) -> Option<(Role, String)> {
    let n = nodes(h).into_iter().find(|n| n.is_focused())?;
    Some((n.role(), n.label().unwrap_or_default()))
}

/// Interactive nodes a screen reader would announce badly: no name, or no role.
fn badly_named(h: &H, state: &str) -> Vec<String> {
    let mut bad: Vec<String> = (unnamed(h).into_iter())
        .map(|u| format!("{state}: {u}"))
        .collect();
    bad.extend(
        nodes(h)
            .into_iter()
            .filter(|n| interactive(n) && !n.is_hidden() && n.role() == Role::Unknown)
            .map(|n| format!("{state}: no role {:?} {:?}", n.label(), n.raw_bounds())),
    );
    bad
}

fn set_view(h: &mut H, view: crate::pane::ViewMode) {
    h.state_mut().state.panes[0].view = view;
    h.run_steps(4);
}

/// The tree as text (what a screen reader reads), for checking by eye:
/// `cargo test -p keel-app --bin keel a11y_tests::dump_tree -- --ignored --nocapture`.
#[test]
#[ignore]
fn dump_tree() {
    let mut h = app("dump_tree");
    press(&mut h, Modifiers::NONE, Key::ArrowDown);
    println!("{}", dump(&h));
}

/// Every interactive control of the main window, the four views, the preview panel, each
/// Settings page, the palette, the folder jump, the search tab and the dialogs has an
/// accessible name and a role.
#[test]
fn every_control_has_a_name_and_a_role() {
    use crate::pane::ViewMode;
    let mut h = app("every_control_has_a_name_and_a_role");
    press(&mut h, Modifiers::NONE, Key::ArrowDown);
    let mut bad = badly_named(&h, "main");
    for view in [
        ViewMode::Grid,
        ViewMode::Columns,
        ViewMode::Media,
        ViewMode::Details,
    ] {
        set_view(&mut h, view);
        bad.extend(badly_named(&h, &format!("{view:?}")));
    }
    press(&mut h, Modifiers::NONE, Key::F3);
    bad.extend(badly_named(&h, "preview"));
    press(&mut h, Modifiers::NONE, Key::F3);
    press(&mut h, Modifiers::COMMAND, Key::Comma);
    for page in [
        "General", "Remotes", "Cloud", "Profiles", "Icons", "Library", "Devices",
    ] {
        h.query_all_by_label(page).last().unwrap().click();
        h.run_steps(3);
        bad.extend(badly_named(&h, &format!("Settings {page}")));
    }
    press(&mut h, Modifiers::NONE, Key::Escape);
    assert!(!h.state().state.settings_open, "Esc closes Settings");
    for (mods, key, state) in [
        (Modifiers::COMMAND | Modifiers::SHIFT, Key::P, "palette"),
        (Modifiers::COMMAND, Key::P, "folder jump"),
        (Modifiers::COMMAND | Modifiers::SHIFT, Key::N, "new folder"),
        (Modifiers::NONE, Key::Delete, "confirm"),
    ] {
        press(&mut h, mods, key);
        bad.extend(badly_named(&h, state));
        press(&mut h, Modifiers::NONE, Key::Escape);
    }
    h.state_mut().state.run(0, crate::keys::Action::Properties);
    h.run_steps(5);
    bad.extend(badly_named(&h, "properties"));
    press(&mut h, Modifiers::NONE, Key::Escape);
    press(&mut h, Modifiers::COMMAND, Key::F);
    bad.extend(badly_named(&h, "search tab"));
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// A file row reads as its name, size and modified date; a folder as "folder".
#[test]
fn file_rows_read_name_size_and_modified() {
    let h = app("file_rows_read_name_size_and_modified");
    let rows: Vec<String> = nodes(&h)
        .into_iter()
        .filter(|n| n.role() == Role::ListItem)
        .filter_map(|n| n.label())
        .collect();
    let notes = (rows.iter())
        .find(|l| l.starts_with("notes.md, "))
        .expect("notes.md row");
    let parts: Vec<&str> = notes.split(", ").collect();
    assert_eq!(parts.len(), 3, "{notes}");
    assert!(parts[1].ends_with(" B"), "size: {notes}");
    assert_eq!(
        parts[2].len(),
        "2026-10-03 14:00".len(),
        "modified: {notes}"
    );
    assert!(
        rows.iter().any(|l| l.starts_with("Projects, folder, ")),
        "{rows:?}"
    );
}

/// The cursor row holds the keyboard focus as it moves (a screen reader reads each row),
/// selected; the folder and its item count are a live region that follows navigation;
/// toasts are live regions.
#[test]
fn cursor_row_has_the_focus_and_the_folder_is_announced() {
    let mut h = app("cursor_row_has_the_focus_and_the_folder_is_announced");
    press(&mut h, Modifiers::NONE, Key::ArrowDown);
    let (role, first) = focus(&h).expect("focus");
    assert_eq!(role, Role::ListItem);
    let cursor = h.state().state.tab(0).cursor.clone().unwrap();
    assert!(
        first.starts_with(&format!("{cursor}, ")),
        "{first} / {cursor}"
    );
    let node = nodes(&h).into_iter().find(|n| n.is_focused()).unwrap();
    assert_eq!(node.is_selected(), Some(true), "the cursor row is selected");
    press(&mut h, Modifiers::NONE, Key::ArrowDown);
    let (_, second) = focus(&h).unwrap();
    assert_ne!(first, second, "the focus follows the cursor");
    let status = |h: &H| {
        let n = (nodes(h).into_iter())
            .find(|n| n.role() == Role::Status)
            .expect("live region");
        assert_eq!(n.live(), egui::accesskit::Live::Polite);
        n.label().unwrap()
    };
    assert_eq!(status(&h), "Documents, 11 items");
    press(&mut h, Modifiers::ALT, Key::ArrowUp);
    assert!(wait(&mut h, 10, &listed));
    let root = format!(
        "keel-a11y-{}-cursor_row_has_the_focus_and_the_folder_is_announced, ",
        std::process::id()
    );
    assert!(status(&h).starts_with(&root), "{}", status(&h));
    h.state_mut().state.toasts.error("Disk full");
    h.run_steps(2);
    let toast = h.get_by_label("Disk full");
    assert_eq!(toast.live(), egui::accesskit::Live::Assertive);
}

/// Dialogs open with the focus on their primary action or first field; Tab stays inside
/// (a modal traps on purpose); Esc cancels and gives the focus back to the list.
#[test]
fn dialogs_focus_their_primary_action_and_esc_cancels() {
    let mut h = app("dialogs_focus_their_primary_action_and_esc_cancels");
    press(&mut h, Modifiers::NONE, Key::ArrowDown);
    press(&mut h, Modifiers::COMMAND | Modifiers::SHIFT, Key::N);
    assert_eq!(focus(&h).map(|f| f.0), Some(Role::TextInput), "new folder");
    press(&mut h, Modifiers::NONE, Key::Escape);
    assert!(h.state().state.dialog.is_none());
    assert_eq!(
        focus(&h).map(|f| f.0),
        Some(Role::ListItem),
        "back on the list"
    );
    press(&mut h, Modifiers::NONE, Key::Delete);
    let trash = (Role::Button, "Move to trash".to_owned());
    assert_eq!(focus(&h).as_ref(), Some(&trash));
    press(&mut h, Modifiers::NONE, Key::Tab);
    assert_eq!(focus(&h), Some((Role::Button, "Cancel".into())));
    press(&mut h, Modifiers::NONE, Key::Tab);
    assert_eq!(focus(&h).as_ref(), Some(&trash), "Tab stays in the dialog");
    press(&mut h, Modifiers::NONE, Key::Escape);
    assert!(h.state().state.dialog.is_none(), "Esc cancels");
    // Task 67: Get Info is a window, not a modal: the list keeps the keyboard and Esc
    // closes the window in front.
    h.state_mut().state.run(0, crate::keys::Action::Properties);
    let read = |h: &H| (h.state().state.info.windows.first()).is_some_and(|w| w.info.is_some());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !read(&h) && std::time::Instant::now() < until {
        h.run_steps(1);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(read(&h), "Get Info read the item");
    h.run_steps(2);
    assert_eq!(
        focus(&h).map(|f| f.0),
        Some(Role::ListItem),
        "the list keeps the keyboard"
    );
    press(&mut h, Modifiers::NONE, Key::Escape);
    h.run_steps(3);
    assert!(
        h.state().state.info.windows.is_empty(),
        "Esc closes Get Info"
    );
    assert_eq!(focus(&h).map(|f| f.0), Some(Role::ListItem));
}

/// Keyboard only: Tab from the list visits every control once and comes back (never
/// trapped), Shift+Tab walks the same stops backwards; the sidebar is one stop that the
/// arrow keys move inside; pointer-only handles are not stops; Enter on a focused button
/// presses it instead of opening the cursor row; the focus ring is drawn.
#[test]
fn tab_walks_every_control_and_comes_back() {
    let mut h = app("tab_walks_every_control_and_comes_back");
    press(&mut h, Modifiers::NONE, Key::ArrowDown);
    let start = focus(&h).unwrap();
    let mut walk = Vec::new();
    for _ in 0..200 {
        press(&mut h, Modifiers::NONE, Key::Tab);
        let f = focus(&h).expect("something has the focus after Tab");
        if f == start {
            break;
        }
        walk.push(f);
    }
    assert_eq!(
        focus(&h).as_ref(),
        Some(&start),
        "back to the list: {walk:#?}"
    );
    assert!(
        walk.iter()
            .all(|(r, n)| *r != Role::Unknown && !n.trim().is_empty()),
        "unnamed stop in {walk:#?}"
    );
    let names: Vec<&str> = walk.iter().map(|(_, n)| n.as_str()).collect();
    let at = |name: &str| {
        (names.iter().position(|n| *n == name))
            .unwrap_or_else(|| panic!("{name} not in {names:#?}"))
    };
    // A pane in reading order: tabs, toolbar, path, column headers (Back and Forward are
    // disabled here: no history).
    assert!(at("New tab (Ctrl+T)") < at("Refresh (F5)"));
    assert!(at("Refresh (F5)") < at("Type a path (Ctrl+L)"));
    assert!(at("Type a path (Ctrl+L)") < at("Name, sorted ascending"));
    assert!(at("Name, sorted ascending") < at("Modified"));
    // The sidebar is one stop (Home); its other items are reached with the arrow keys.
    // The drive is "C:" on Windows and "/" elsewhere.
    let sidebar = ["Home", "Documents", "Pictures", "Backup", "C:", "/", "Add…"];
    assert_eq!(
        names.iter().filter(|n| **n == "Home").count(),
        1,
        "{names:#?}"
    );
    for item in ["Backup", "C:"] {
        assert!(!names.contains(&item), "{item}: {names:#?}");
    }
    // Each pane's tab and current path part; the sidebar's Documents and Pictures are not.
    for item in ["Documents", "Pictures"] {
        let n = names.iter().filter(|n| **n == item).count();
        assert_eq!(n, 2, "{item}: {names:#?}");
    }
    // Every other focusable control of the window was a stop.
    let stops: std::collections::HashSet<&str> = names.iter().copied().collect();
    let missing: Vec<String> = nodes(&h)
        .into_iter()
        .filter(|n| n.is_focusable() && !n.is_hidden() && !n.is_disabled())
        .filter(|n| n.role() != Role::ListItem)
        .filter_map(|n| n.label())
        .filter(|l| !stops.contains(l.as_str()) && !sidebar.contains(&l.as_str()))
        .collect();
    assert!(missing.is_empty(), "never reached with Tab: {missing:?}");
    // Shift+Tab walks the same stops (the sidebar is entered at its last item) and comes
    // back too.
    let mut back = Vec::new();
    for _ in 0..200 {
        press(&mut h, Modifiers::SHIFT, Key::Tab);
        let f = focus(&h).expect("something has the focus after Shift+Tab");
        if f == start {
            break;
        }
        back.push(f);
    }
    assert_eq!(
        focus(&h).as_ref(),
        Some(&start),
        "Shift+Tab came back: {back:#?}"
    );
    let norm = |w: &[(Role, String)]| -> std::collections::BTreeSet<String> {
        (w.iter())
            .map(|(_, n)| {
                if sidebar.contains(&n.as_str()) && n != "Documents" && n != "Pictures" {
                    "<sidebar>".to_owned()
                } else {
                    n.clone()
                }
            })
            .collect()
    };
    assert_eq!(norm(&back), norm(&walk), "Shift+Tab reaches the same stops");
    assert_eq!(back.len(), walk.len(), "{back:#?}");
    // Into the sidebar: Down moves inside it, Tab leaves it in one press.
    while focus(&h).map(|f| f.1) != Some("Home".into()) {
        press(&mut h, Modifiers::NONE, Key::Tab);
    }
    let cursor = h.state().state.tab(0).cursor.clone();
    press(&mut h, Modifiers::NONE, Key::ArrowDown);
    assert_eq!(
        focus(&h).map(|f| f.1),
        Some("Documents".into()),
        "Down in the sidebar"
    );
    assert_eq!(
        h.state().state.tab(0).cursor,
        cursor,
        "the list cursor stays"
    );
    press(&mut h, Modifiers::NONE, Key::Tab);
    let after = focus(&h).unwrap().1;
    assert!(
        !sidebar[2..].contains(&after.as_str()),
        "left the sidebar: {after}"
    );
    // Enter on a focused button presses it; it does not open the cursor row.
    while focus(&h).map(|f| f.1) != Some("Up (Alt+Up)".into()) {
        press(&mut h, Modifiers::NONE, Key::Tab);
    }
    let ring = crate::a11y::ring_color(&h.ctx.style().visuals);
    let drawn = h.output().shapes.iter().any(|s| match &s.shape {
        egui::Shape::Rect(r) => r.stroke.color == ring && r.stroke.width >= 2.0,
        _ => false,
    });
    assert!(drawn, "focus ring painted");
    let dir = h.state().state.tab(0).dir.clone();
    press(&mut h, Modifiers::NONE, Key::Enter);
    assert!(wait(&mut h, 10, &listed));
    assert_eq!(
        h.state().state.tab(0).dir,
        dir.parent().unwrap(),
        "Enter pressed Up"
    );
    press(&mut h, Modifiers::NONE, Key::Escape);
    assert_eq!(
        focus(&h).map(|f| f.0),
        Some(Role::ListItem),
        "Esc: back to the list"
    );
}

/// Ctrl+= / Ctrl+- / Ctrl+0 scale the UI and the scale is a setting (saved like the
/// others); the setting scales the UI too. Reduce motion stops every transition and the
/// spinners.
#[test]
fn ui_scale_and_reduce_motion() {
    let mut h = app("ui_scale_and_reduce_motion");
    let scale = |h: &H| (h.ctx.zoom_factor(), h.state().state.settings.ui_scale);
    press(&mut h, Modifiers::COMMAND, Key::Equals);
    assert_eq!(scale(&h), (1.1, 1.1));
    press(&mut h, Modifiers::COMMAND, Key::Equals);
    press(&mut h, Modifiers::COMMAND, Key::Minus);
    assert_eq!(scale(&h), (1.1, 1.1));
    press(&mut h, Modifiers::COMMAND, Key::Num0);
    assert_eq!(scale(&h), (1.0, 1.0));
    h.state_mut().state.settings.ui_scale = 1.5;
    h.run_steps(3);
    assert_eq!(scale(&h), (1.5, 1.5), "the setting scales the UI");
    h.state_mut().state.settings.ui_scale = 99.0;
    h.run_steps(3);
    assert_eq!(scale(&h).1, crate::settings::MAX_SCALE, "kept in range");
    let s = crate::settings::Settings {
        ui_scale: 1.3,
        reduce_motion: true,
        ..Default::default()
    };
    let text = toml::to_string(&s).unwrap();
    let back: crate::settings::Settings = toml::from_str(&text).unwrap();
    assert_eq!((back.ui_scale, back.reduce_motion), (1.3, true));
    let old: crate::settings::Settings = toml::from_str("theme = \"dark\"").unwrap();
    assert_eq!(
        (old.ui_scale, old.reduce_motion),
        (1.0, false),
        "older files"
    );
    h.state_mut().state.settings.reduce_motion = true;
    h.run_steps(2);
    assert_eq!(h.ctx.style().animation_time, 0.0);
    // A spinning spinner asks for the next frame at once; the still one does not.
    let repaint = |reduce: bool| {
        let ctx = egui::Context::default();
        let frame = |ctx: &egui::Context| {
            crate::anim::apply(ctx, reduce);
            egui::CentralPanel::default().show(ctx, crate::anim::spinner);
        };
        let _ = ctx.run(egui::RawInput::default(), frame);
        let out = ctx.run(egui::RawInput::default(), frame);
        out.viewport_output[&egui::ViewportId::ROOT].repaint_delay
    };
    assert_eq!(repaint(false), std::time::Duration::ZERO);
    assert!(
        repaint(true) > std::time::Duration::from_secs(1),
        "a still spinner"
    );
    h.state_mut().state.settings.reduce_motion = false;
    h.run_steps(2);
    assert_eq!(h.ctx.style().animation_time, crate::anim::DURATION);
}
