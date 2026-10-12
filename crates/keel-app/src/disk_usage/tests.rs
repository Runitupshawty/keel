use super::model::{Column, Model};
use keel_vfs::du::{self, Root, ScanHandle, ScanOptions};
use std::sync::Arc;

fn fixture() -> (tempfile::TempDir, Model) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    for (path, bytes) in [("small.TXT", 10), ("large.bin", 200), ("nested/other.txt", 30), ("README", 5)] {
        std::fs::write(dir.path().join(path), vec![0; bytes]).unwrap();
    }
    let tree = du::scan(&[Root::Path(dir.path().into())], &ScanOptions::default(), &ScanHandle::new()).unwrap();
    (dir, Model::new(Arc::new(tree)))
}

#[test]
fn tree_expansion_and_sibling_sort_preserve_hierarchy() {
    let (dir, mut model) = fixture();
    let root = model.tree.roots()[0].node;
    let nested = model.tree.find(&dir.path().join("nested")).unwrap();
    assert_eq!(model.rows.len(), 5);
    model.toggle(nested);
    assert_eq!(model.rows.len(), 6);
    model.sort = (Column::Name, true);
    model.rebuild();
    let pos = model.rows.iter().position(|&(id, _)| id == nested).unwrap();
    assert_eq!(&*model.tree.node(model.rows[pos + 1].0).name, "other.txt");
    model.toggle(root);
    assert_eq!(model.rows, vec![(root, 0)]);
}

#[test]
fn size_sort_and_all_columns_are_total_and_deterministic() {
    let (_, mut model) = fixture();
    assert_eq!(&*model.tree.node(model.rows[1].0).name, "large.bin");
    for column in Column::ALL {
        for ascending in [true, false] {
            model.sort = (column, ascending);
            model.rebuild();
            let before = model.rows.clone();
            model.rebuild();
            assert_eq!(before, model.rows);
        }
    }
}

#[test]
fn extensions_aggregate_bytes_and_group_only_unregistered() {
    let (_, model) = fixture();
    let types = model.file_types(false, &|ext| (ext == "txt").then(|| "Text document".into()));
    let txt = types.iter().find(|t| t.extensions == ["txt"]).unwrap();
    assert_eq!((txt.logical, txt.files), (40, 2));
    let grouped = model.file_types(true, &|ext| (ext == "txt").then(|| "Text document".into()));
    let unknown = grouped.iter().find(|t| t.label == "Unregistered").unwrap();
    assert_eq!((unknown.logical, unknown.files), (200, 1));
    assert!(grouped.iter().any(|t| t.label == "(no extension)"));
    assert_eq!(types.iter().map(|t| t.logical).sum::<u64>(), 245);
}

#[test]
fn selection_sum_does_not_double_count_selected_descendants() {
    let (dir, mut model) = fixture();
    let nested = model.tree.find(&dir.path().join("nested")).unwrap();
    let child = model.tree.find(&dir.path().join("nested/other.txt")).unwrap();
    model.select(nested, false, false);
    model.select(child, true, false);
    assert_eq!(model.selection_bytes(), 30);
    model.select(child, false, false);
    assert!(model.highlight_extensions.contains("txt"));
    model.filter_extensions(Some(["txt".to_owned()].into()));
    assert_eq!(model.rows.len(), 4); // root, folder, and two matching files
    assert!(model.rows.iter().any(|&(id, _)| id == child));
    model.filter_extensions(None);
    assert!(model.rows.iter().any(|&(id, _)| model.tree.node(id).name.as_ref() == "large.bin"));
}
