//! Accessibility: the AccessKit tree a screen reader reads, on the screenshot fixture. Nothing here speaks or plays sound: the tree
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
/// not refresh this one's listing.
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
    h.state_mut().state.settings_open = false;
    h.run_steps(2);
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

/// The selected row says so; the folder and its item count are a live region that follows
/// navigation; toasts are live regions.
#[test]
fn rows_and_the_folder_are_announced() {
    let mut h = app("announced");
    press(&mut h, Modifiers::NONE, Key::ArrowDown);
    let cursor = h.state().state.tab(0).cursor.clone().unwrap();
    let prefix = format!("{cursor}, ");
    let row = (nodes(&h).into_iter())
        .find(|n| n.role() == Role::ListItem && n.label().is_some_and(|l| l.starts_with(&prefix)))
        .expect("cursor row");
    assert_eq!(row.is_selected(), Some(true), "the cursor row is selected");
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
    let root = format!("keel-a11y-{}-announced, ", std::process::id());
    assert!(status(&h).starts_with(&root), "{}", status(&h));
    h.state_mut().state.toasts.error("Disk full");
    h.run_steps(2);
    let toast = h.get_by_label("Disk full");
    assert_eq!(toast.live(), egui::accesskit::Live::Assertive);
}
