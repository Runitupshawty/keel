//! The Recycle Bin / Trash as a folder (`trash://`, keel-vfs `trashbin`): its context menu,
//! confirmations, and the column text that differs from an ordinary folder.

use crate::keys::Action;
use keel_vfs::{trashbin, Entry};

/// The context menu of a trash tab as `(text, shortcut, action)`; an empty text is a
/// separator. Rows offer Restore / Delete permanently; the empty area only Empty.
pub fn menu(on_item: bool) -> Vec<(String, &'static str, Action)> {
    let mut items = Vec::new();
    if on_item {
        items.push(("Restore".to_owned(), "", Action::RestoreTrash));
        items.push(("Delete permanently".to_owned(), "Del", Action::Delete));
        items.push((String::new(), "", Action::Refresh));
    }
    items.push((
        format!("Empty {}", trashbin::label()),
        "",
        Action::EmptyTrash,
    ));
    items.push((String::new(), "", Action::Refresh));
    items.push(("Select all".to_owned(), "Ctrl+A", Action::SelectAll));
    items.push(("Invert selection".to_owned(), "", Action::InvertSelection));
    items
}

pub fn context_menu(ui: &mut egui::Ui, entry: Option<&Entry>, out: &mut Vec<Action>) {
    for (text, shortcut, action) in menu(entry.is_some()) {
        if text.is_empty() {
            ui.separator();
            continue;
        }
        let button = egui::Button::new(text).shortcut_text(crate::keys::shortcut_label(shortcut));
        if ui.add(button).clicked() {
            out.push(action);
            ui.close_menu();
        }
    }
}

/// Delete in a trash tab: gone for good, so say so.
pub fn delete_text(n: usize) -> String {
    format!(
        "Permanently delete {} from the {}? This cannot be undone.",
        crate::jobs::items(n),
        trashbin::label()
    )
}

pub fn empty_text(n: usize) -> String {
    format!(
        "Permanently delete {} in the {}? This cannot be undone.",
        crate::jobs::items(n),
        trashbin::label()
    )
}

/// The "Original location" cell: the folder the item was deleted from.
pub fn original_location(e: &Entry) -> String {
    trashbin::info(&e.path)
        .and_then(|i| i.original.parent().map(|p| p.display().to_string()))
        .unwrap_or_default()
}

/// Actions a trash tab refuses: it is not a place to put or change files.
pub fn refuses(action: &Action, from_trash_dir: bool) -> bool {
    match action {
        Action::Rename
        | Action::RenameTo { .. }
        | Action::Cut
        | Action::Paste
        | Action::NewFolder
        | Action::NewFile
        | Action::AddToZip
        | Action::CompressToZip
        | Action::Create { .. } => from_trash_dir,
        Action::Drop { paths, dst, .. } => {
            dst.scheme == trashbin::SCHEME || paths.iter().any(|p| p.scheme == trashbin::SCHEME)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tab::Tab;
    use keel_vfs::VPath;

    #[test]
    fn a_trash_tab_gets_the_trash_menu() {
        assert!(Tab::new(trashbin::root()).is_trash());
        assert!(!Tab::new(VPath::local("/")).is_trash());
        assert_eq!(Tab::new(trashbin::root()).title(), trashbin::label());

        let actions = |on_item| -> Vec<Action> {
            menu(on_item)
                .into_iter()
                .filter(|(t, ..)| !t.is_empty())
                .map(|(.., a)| a)
                .collect()
        };
        let row = actions(true);
        assert!(row.contains(&Action::RestoreTrash));
        assert!(row.contains(&Action::Delete));
        assert!(row.contains(&Action::EmptyTrash));
        for no in [Action::Copy, Action::Cut, Action::Paste, Action::Rename] {
            assert!(!row.contains(&no), "{no:?}");
        }
        let blank = actions(false);
        assert!(blank.contains(&Action::EmptyTrash));
        assert!(!blank.contains(&Action::RestoreTrash) && !blank.contains(&Action::Delete));
        assert!(menu(true)
            .iter()
            .any(|(t, ..)| t == &format!("Empty {}", trashbin::label())));
    }

    #[test]
    fn a_trash_tab_refuses_edits_and_drops() {
        assert!(refuses(&Action::Paste, true));
        assert!(!refuses(&Action::Paste, false));
        assert!(!refuses(&Action::RestoreTrash, true));
        let drop = |from: VPath, dst: VPath| Action::Drop {
            paths: vec![from],
            from: None,
            dst,
        };
        let file = VPath::local("/a");
        let bin_item = trashbin::root().join("x");
        assert!(refuses(&drop(file.clone(), trashbin::root()), false));
        assert!(refuses(&drop(bin_item, file.clone()), false));
        assert!(!refuses(&drop(file.clone(), file), false));
    }

    #[test]
    fn confirmations_name_the_count_and_the_platform_bin() {
        assert!(delete_text(2).contains("2 items"));
        assert!(empty_text(1).contains("1 item "));
        assert!(empty_text(5).contains(trashbin::label()));
    }
}
