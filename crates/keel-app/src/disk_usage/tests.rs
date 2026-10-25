use super::model::{attributes, Column, Model, NO_EXTENSION, UNREGISTERED};
use super::{DiskUsage, DuCmd, DuSettings, Layout};
use crate::keys::Action;
use keel_vfs::du::{self, NodeId, Root, ScanHandle, ScanOptions};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

/// root/
///   small.TXT (10), large.bin (200), README (5)
///   nested/ other.txt (30), deeper/ a.log (1)
fn fixture() -> (tempfile::TempDir, Model) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("nested/deeper")).unwrap();
    for (path, bytes) in [
        ("small.TXT", 10),
        ("large.bin", 200),
        ("nested/other.txt", 30),
        ("nested/deeper/a.log", 1),
        ("README", 5),
    ] {
        std::fs::write(dir.path().join(path), vec![0; bytes]).unwrap();
    }
    let roots = [Root::Path(dir.path().into())];
    let tree = du::scan(&roots, &ScanOptions::default(), &ScanHandle::new()).unwrap();
    (dir, Model::new(Arc::new(tree)))
}

fn id(model: &Model, root: &Path, rel: &str) -> NodeId {
    model.tree.find(&root.join(rel)).unwrap()
}

fn names(model: &Model) -> Vec<String> {
    (model.rows.iter())
        .map(|&(id, _)| model.tree.node(id).name.to_string())
        .collect()
}

#[test]
fn tree_expansion_and_sibling_sort_preserve_hierarchy() {
    let (dir, mut model) = fixture();
    let root = model.tree.roots()[0].node;
    let nested = id(&model, dir.path(), "nested");
    // The root is open: itself and its four children.
    assert_eq!(model.rows.len(), 5);
    assert_eq!(model.rows[0], (root, 0));
    model.toggle(nested);
    assert_eq!(model.rows.len(), 7);
    model.set_sort(Column::Name);
    assert_eq!(model.sort, (Column::Name, true), "names sort A to Z first");
    let pos = model.row_of(nested).unwrap();
    assert_eq!(&*model.tree.node(model.rows[pos + 1].0).name, "deeper");
    assert_eq!(model.rows[pos + 1].1, 2, "children are one level deeper");
    assert_eq!(&*model.tree.node(model.rows[pos + 2].0).name, "other.txt");
    // Natural, case-insensitive order among the root's children.
    let top: Vec<String> = (model.rows.iter())
        .filter(|r| r.1 == 1)
        .map(|r| model.tree.node(r.0).name.to_string())
        .collect();
    assert_eq!(top, ["large.bin", "nested", "README", "small.TXT"]);
    model.toggle(root);
    assert_eq!(model.rows, vec![(root, 0)]);
}

#[test]
fn size_sort_is_biggest_first_and_every_column_sorts_deterministically() {
    let (_, mut model) = fixture();
    assert_eq!(
        names(&model)[1..],
        ["large.bin", "nested", "small.TXT", "README"]
    );
    // Size ascending on the second click.
    model.set_sort(Column::Size);
    assert_eq!(model.sort, (Column::Size, true));
    assert_eq!(
        names(&model)[1..],
        ["README", "small.TXT", "nested", "large.bin"]
    );
    for column in Column::ALL {
        for ascending in [true, false] {
            model.sort = (column, ascending);
            model.rebuild();
            let before = model.rows.clone();
            model.rebuild();
            assert_eq!(before, model.rows, "{column:?}");
            assert_eq!(model.rows.len(), 5);
        }
    }
    // Numeric columns start biggest first, text columns A to Z.
    model.set_sort(Column::Files);
    assert_eq!(model.sort, (Column::Files, false));
    model.set_sort(Column::Attributes);
    assert_eq!(model.sort, (Column::Attributes, true));
}

#[test]
fn shares_follow_the_size_kind_and_percentage_mode() {
    let (dir, mut model) = fixture();
    let root = model.tree.roots()[0].node;
    let nested = id(&model, dir.path(), "nested");
    let other = id(&model, dir.path(), "nested/other.txt");
    assert_eq!(model.bytes(root), 246);
    assert!((model.fraction(other, true) - 30.0 / 31.0).abs() < 1e-9);
    assert!((model.fraction(other, false) - 30.0 / 246.0).abs() < 1e-9);
    assert!((model.fraction(nested, true) - model.fraction(nested, false)).abs() < 1e-9);
    model.set_physical(true);
    let n = model.tree.node(other);
    assert_eq!(model.bytes(other), n.allocated);
    model.set_relative(true);
    assert!(model.relative);
}

#[test]
fn extensions_aggregate_bytes_and_group_only_unregistered() {
    let (_, model) = fixture();
    let describe = |ext: &str| (ext == "txt").then(|| "Text document".to_owned());
    let types = model.file_types(false, &describe);
    let txt = types.iter().find(|t| t.label == ".txt").unwrap();
    assert_eq!((txt.logical, txt.files), (40, 2), "small.TXT and other.txt");
    assert_eq!(txt.description, "Text document");
    assert_eq!(types[0].label, ".bin", "biggest first");
    assert_eq!(types[0].description, "BIN file");
    assert_eq!(types.iter().map(|t| t.logical).sum::<u64>(), 246);
    assert_eq!(types.iter().map(|t| t.files).sum::<u64>(), 5);

    let grouped = model.file_types(true, &describe);
    let unknown = grouped.iter().find(|t| t.label == UNREGISTERED).unwrap();
    assert_eq!(
        (unknown.logical, unknown.files),
        (201, 2),
        "large.bin and a.log"
    );
    let exts: Vec<&str> = unknown.extensions.iter().map(String::as_str).collect();
    assert_eq!(exts, ["bin", "log"]);
    let none = grouped.iter().find(|t| t.label == NO_EXTENSION).unwrap();
    assert_eq!(
        (none.logical, none.files),
        (5, 1),
        "README is never grouped"
    );
    assert_eq!(grouped.len(), 3);
    assert_eq!(grouped.iter().map(|t| t.logical).sum::<u64>(), 246);
    let mut exts: Vec<&str> = model.extensions().collect();
    exts.sort_unstable();
    assert_eq!(exts, ["", "bin", "log", "txt"]);
}

#[test]
fn selection_sum_does_not_double_count_selected_descendants() {
    let (dir, mut model) = fixture();
    let nested = id(&model, dir.path(), "nested");
    let child = id(&model, dir.path(), "nested/other.txt");
    model.toggle(nested);
    model.select(nested, false, false);
    model.select(child, true, false);
    assert_eq!(model.selected.len(), 2);
    assert_eq!(model.selection_bytes(), 31);
    // Ctrl+click again unselects.
    model.select(child, true, false);
    assert_eq!(model.selected.len(), 1);
    model.select(child, false, false);
    assert!(model.highlight_extensions.contains("txt"));
    assert_eq!(model.selection_bytes(), 30);
    // Shift+click: the range from the anchor.
    let large = id(&model, dir.path(), "large.bin");
    model.select(large, false, true);
    let a = model.row_of(large).unwrap();
    let b = model.row_of(child).unwrap();
    assert_eq!(model.selected.len(), b - a + 1);
    assert!(model.highlight_extensions.contains("bin"));
}

#[test]
fn an_extension_filter_lists_matches_and_their_folders_only() {
    let (dir, mut model) = fixture();
    let child = id(&model, dir.path(), "nested/other.txt");
    model.filter_extensions(Some(["txt".to_owned()].into()));
    // root, nested (opened by the filter), other.txt, small.TXT
    assert_eq!(model.rows.len(), 4);
    assert!(model.row_of(child).is_some());
    assert!(names(&model)
        .iter()
        .all(|n| n != "large.bin" && n != "deeper"));
    model.filter_extensions(None);
    assert!(names(&model).iter().any(|n| n == "large.bin"));
    assert!(model.row_of(child).is_none(), "nested is closed again");
}

#[test]
fn reveal_opens_the_folders_above_and_selects() {
    let (dir, mut model) = fixture();
    let log = id(&model, dir.path(), "nested/deeper/a.log");
    assert!(model.row_of(log).is_none());
    model.reveal(log);
    assert!(model.row_of(log).is_some());
    assert_eq!(model.cursor, Some(log));
    assert_eq!(model.selected, BTreeSet::from([log]));
    assert_eq!(
        model.highlight_extensions,
        BTreeSet::from(["log".to_owned()])
    );
    // A pick outside the filter drops the filter.
    model.filter_extensions(Some(["txt".to_owned()].into()));
    model.reveal(log);
    assert!(model.filter.is_none());
    assert!(model.row_of(log).is_some());
}

#[test]
fn keyboard_moves_the_cursor_and_opens_and_closes_folders() {
    let (dir, mut model) = fixture();
    let root = model.tree.roots()[0].node;
    let nested = id(&model, dir.path(), "nested");
    model.move_cursor(1, false);
    assert_eq!(model.cursor, Some(root), "first press: the first row");
    model.move_cursor(1, false);
    model.move_cursor(1, false);
    assert_eq!(model.cursor, Some(nested));
    model.right();
    assert!(model.expanded.contains(&nested));
    model.right();
    assert_eq!(model.tree.node(model.cursor.unwrap()).parent, nested);
    model.left();
    assert_eq!(model.cursor, Some(nested), "left goes to the parent");
    model.left();
    assert!(!model.expanded.contains(&nested), "then closes it");
    model.move_cursor(-100, false);
    assert_eq!(model.cursor, Some(root));
    model.move_cursor(100, true);
    assert_eq!(
        model.selected.len(),
        model.rows.len(),
        "Shift+End selects to the end"
    );
    model.select_all();
    assert_eq!(model.selected.len(), model.rows.len());
}

#[test]
fn attribute_letters() {
    use keel_vfs::du::attr;
    assert_eq!(attributes(0), "");
    assert_eq!(
        attributes(attr::READONLY | attr::HIDDEN | attr::ARCHIVE),
        "RHA"
    );
    assert_eq!(
        attributes(attr::COMPRESSED | attr::SPARSE | attr::OFFLINE),
        "CPO"
    );
}

#[test]
fn settings_columns_keep_name_first_and_their_order() {
    let mut s = DuSettings::default();
    assert_eq!(s.shown_columns()[0], Column::Name);
    s.toggle_column(Column::Name);
    assert_eq!(s.shown_columns()[0], Column::Name, "Name cannot be hidden");
    s.toggle_column(Column::Size);
    assert!(!s.shown_columns().contains(&Column::Size));
    s.toggle_column(Column::Owner);
    s.toggle_column(Column::Size);
    let shown = s.shown_columns();
    let at = |c| shown.iter().position(|x| *x == c).unwrap();
    assert!(at(Column::Size) < at(Column::Owner), "Column::ALL order");
    // Saved and read back as TOML.
    s.layout = Layout::TreemapLeft;
    let text = toml::to_string(&s).unwrap();
    let back: DuSettings = toml::from_str(&text).unwrap();
    assert_eq!(back, s);
    let empty: DuSettings = toml::from_str("").unwrap();
    assert_eq!(empty, DuSettings::default());
}

#[test]
fn tree_only_layout_hides_types_and_treemap() {
    assert!(!Layout::TreeOnly.shows_types());
    assert!(!Layout::TreeOnly.shows_treemap());
    assert!(Layout::ListsOnly.shows_types());
    assert!(!Layout::ListsOnly.shows_treemap());
    assert!(Layout::Classic.shows_types());
    assert!(Layout::Classic.shows_treemap());
    assert_eq!(Layout::ALL.len(), 4);
}

/// Opening with nothing scanned asks for drives; a scan replaces the picker, finishes on its
/// worker and selects the root; the file types are built with ranks.
#[test]
fn a_scan_runs_on_a_worker_and_fills_the_window() {
    let (dir, _) = fixture();
    let mut du = DiskUsage::default();
    du.command(DuCmd::Open, &[]);
    assert!(du.open && du.picker.is_some());
    du.start(vec![Root::Path(dir.path().into())]);
    assert!(du.picker.is_none());
    let ctx = egui::Context::default();
    let mut settings = DuSettings {
        physical: true,
        ..Default::default()
    };
    for _ in 0..500 {
        du.poll(&ctx, &mut settings);
        if du.model.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let model = du.model.as_ref().expect("scan finished");
    assert!(model.physical, "the size kind of the settings");
    assert_eq!(model.cursor, Some(model.tree.roots()[0].node));
    assert!(du.scanning().is_none());
    let types = du.types(false).to_vec();
    assert_eq!(types.len(), 4);
    assert_eq!(du.ranks().len(), 4);
    assert_eq!(du.ranks()[types[0].extensions.iter().next().unwrap()], 0);
}

/// The window draws a scan and its keys move the tree's cursor; Esc closes it.
#[test]
fn the_window_draws_the_tree_and_takes_its_keys() {
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;
    let (dir, _) = fixture();
    let theme: crate::theme::Theme = toml::from_str(crate::theme::BUILTIN[0].1).unwrap();
    let mut du = DiskUsage::default();
    du.start(vec![Root::Path(dir.path().into())]);
    type State = (DiskUsage, DuSettings, Vec<Action>);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1280.0, 800.0))
        .build_state(
            move |ctx, (du, settings, out): &mut State| {
                super::ui::window(ctx, du, settings, &[], &theme, out)
            },
            (du, DuSettings::default(), Vec::new()),
        );
    for _ in 0..500 {
        h.step();
        if h.state().0.model.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.run_steps(3);
    h.get_by_label("large.bin");
    h.get_by_label("Subtree %");
    h.get_by_label("Treemap");
    h.get_by_label(".bin");
    let root = h.state().0.model.as_ref().unwrap().tree.roots()[0].node;
    assert_eq!(h.state().0.model.as_ref().unwrap().cursor, Some(root));
    h.press_key(egui::Key::ArrowDown);
    h.run_steps(2);
    let m = h.state().0.model.as_ref().unwrap();
    assert_eq!(&*m.tree.node(m.cursor.unwrap()).name, "large.bin");
    assert_eq!(m.highlight_extensions, BTreeSet::from(["bin".to_owned()]));
    h.press_key(egui::Key::Escape);
    h.run_steps(2);
    assert!(!h.state().0.open);
}

fn delete(paths: &[&Path]) -> keel_core::Op {
    keel_core::Op::Delete {
        paths: paths.iter().map(keel_vfs::VPath::local).collect(),
    }
}

fn link(target: &Path, at: &Path) -> keel_core::Op {
    keel_core::Op::Link {
        target: keel_vfs::VPath::local(target),
        link: keel_vfs::VPath::local(at),
        kind: keel_vfs::LinkKind::Hardlink,
    }
}

/// Through the library, a step is previewed only after the previous one's job is done;
/// a cancelled preview or a failed job drops the rest.
#[test]
fn steps_wait_for_each_job_before_the_next_preview() {
    use super::steps::{Phase, Steps};
    use keel_core::JobStatus;
    let (a, b) = (Path::new("/k/a"), Path::new("/k/b"));
    let ops = vec![delete(&[b]), link(a, b)];
    let mut s = Steps::default();
    assert_eq!(s.start(ops.clone()), Some(ops[0].clone()));
    assert_eq!(s.phase, Phase::Preview { shown: false });
    assert_eq!(s.tick(false), None, "the preview is not up yet");
    s.tick(true);
    assert_eq!(s.phase, Phase::Preview { shown: true });
    s.executing();
    assert_eq!(s.tick(false), None, "confirmed: the dialog closed");
    s.started(7);
    assert_eq!(s.phase, Phase::Running(7));
    assert_eq!(s.finished(8, JobStatus::Done), Ok(None), "another job");
    assert_eq!(s.status().as_deref(), Some("Step 1 of 2: running"));
    assert_eq!(s.finished(7, JobStatus::Done), Ok(Some(ops[1].clone())));
    assert_eq!(s.current, 2);
    s.tick(true);
    s.executing();
    s.started(9);
    assert_eq!(s.finished(9, JobStatus::Done), Ok(None));
    assert!(!s.active(), "all steps ran");

    // A cancelled preview drops the steps after it.
    s.start(ops.clone());
    s.tick(true);
    assert!(s.tick(false).is_some());
    assert!(!s.active() && s.queue.is_empty());
    // A failed job too.
    s.start(ops.clone());
    s.tick(true);
    s.executing();
    s.started(3);
    assert!(s.finished(3, JobStatus::Failed).is_err());
    assert!(!s.active());
    // A failed preview too.
    s.start(ops);
    assert!(s.failed().is_some());
    assert!(!s.active());
}

/// Without the library: the confirmation lists every path, and the runner stops at the
/// first failure, so a file that was not trashed is never replaced by a link.
#[test]
fn steps_without_the_library_stop_at_the_first_failure() {
    use super::steps::{describe, run, runs_without_library};
    let dir = tempfile::tempdir().unwrap();
    let kept = dir.path().join("kept.bin");
    std::fs::write(&kept, b"same").unwrap();
    let missing = dir.path().join("missing.bin");
    let ops = vec![delete(&[&missing]), link(&kept, &missing)];
    assert!(runs_without_library(&ops));
    let lines = describe(&ops);
    assert!(lines[0].starts_with("Move 1 item(s)"), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("missing.bin")));
    assert!(lines.last().unwrap().starts_with("Make the hard link"));
    let err = run(&ops, &ScanHandle::new()).unwrap_err();
    assert!(format!("{err:#}").contains("0 link(s) made"), "{err:#}");
    assert!(!missing.exists(), "no link after the failed trash step");
    // A link step alone runs.
    let twin = dir.path().join("twin.bin");
    let done = run(&[link(&kept, &twin)], &ScanHandle::new()).unwrap();
    assert!(done.contains("1 link(s) made"), "{done}");
    assert_eq!(std::fs::read(&twin).unwrap(), b"same");
    // Other operations need the library.
    let other = keel_core::Op::Rename {
        path: keel_vfs::VPath::local(&kept),
        new_name: "x".into(),
    };
    assert!(!runs_without_library(&[other]));
}

fn ranks_of(model: &Model) -> std::collections::HashMap<String, usize> {
    let types = model.file_types(false, &|_: &str| None);
    (types.iter().enumerate())
        .flat_map(|(rank, t)| t.extensions.iter().map(move |e| (e.clone(), rank)))
        .collect()
}

/// The chart's tree is the scan: same total, same names, a file's extension as its color
/// key and the rank the file types list gives it, folders neutral.
#[test]
fn the_chart_tree_is_the_scan_with_the_file_types_colors() {
    let (dir, model) = fixture();
    let ranks = ranks_of(&model);
    let tree = super::chart::build(&model.tree, false, true, true, &ranks);
    let top = model.tree.node(keel_vfs::du::TOP);
    assert_eq!(tree.node(0).total, top.size);
    let large = id(&model, dir.path(), "large.bin");
    let n = tree.find(u64::from(large)).expect("large.bin is a tile");
    assert_eq!(tree.node(n).total, 200);
    assert_eq!(tree.node(n).label, "large.bin");
    assert_eq!(tree.key_of(n), "bin");
    let nested = tree
        .find(u64::from(id(&model, dir.path(), "nested")))
        .unwrap();
    assert_eq!(tree.node(nested).total, 31);
    assert!(tree.key_of(nested).starts_with('/'));
    // The biggest type is rank 0 whatever part is shown; folders have no rank.
    let colors = tree.key_ranks(nested);
    assert_eq!(colors[tree.node(n).key as usize], ranks["bin"] as u32);
    assert_eq!(ranks["bin"], 0);
    assert_eq!(colors[tree.node(nested).key as usize], u32::MAX);
    // Sizes on disk are the weights when asked.
    let physical = super::chart::build(&model.tree, true, true, true, &ranks);
    assert_eq!(physical.node(0).total, top.allocated);
}

/// A folder keeps its big children and shows the rest as one "Other" tile; the tile counts
/// toward the folder's total.
#[test]
fn the_chart_tree_keeps_a_folders_big_children_and_pools_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.bin"), vec![0; 4_000_000]).unwrap();
    for i in 0..40 {
        std::fs::write(dir.path().join(format!("tiny{i}.dat")), [0u8; 10]).unwrap();
    }
    let roots = [Root::Path(dir.path().into())];
    let scan = du::scan(&roots, &ScanOptions::default(), &ScanHandle::new()).unwrap();
    let tree = super::chart::build(&scan, false, true, true, &Default::default());
    let top = scan.node(keel_vfs::du::TOP).size;
    assert_eq!(top, 4_000_400);
    assert_eq!(tree.node(0).total, top);
    let labels: Vec<&str> = tree.nodes.iter().map(|n| n.label.as_str()).collect();
    assert!(labels.contains(&"big.bin"));
    assert!(!labels.iter().any(|l| l.starts_with("tiny")), "{labels:?}");
    let other: u64 = (tree.nodes.iter())
        .filter(|n| n.synthetic)
        .map(|n| n.total)
        .sum();
    assert_eq!(other, 400, "what was left out is one tile");
    // The pooled tile stands for its folder: picking it picks the folder.
    let at = tree.nodes.iter().position(|n| n.synthetic).unwrap();
    assert_eq!(
        tree.nodes[at].id,
        tree.nodes[tree.nodes[at].parent.unwrap() as usize].id
    );
}

/// The chart is built on a worker, follows the tree list's cursor (zooming out to show
/// it) and a pick in it is what `Model::reveal` shows in the list.
#[test]
fn the_chart_follows_the_tree_lists_selection() {
    use egui_kittest::Harness;
    let (dir, _) = fixture();
    let theme: crate::theme::Theme = toml::from_str(crate::theme::BUILTIN[0].1).unwrap();
    let mut du = DiskUsage::default();
    du.start(vec![Root::Path(dir.path().into())]);
    type State = (DiskUsage, DuSettings, Vec<Action>);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1280.0, 800.0))
        .build_state(
            move |ctx, (du, settings, out): &mut State| {
                super::ui::window(ctx, du, settings, &[], &theme, out)
            },
            (du, DuSettings::default(), Vec::new()),
        );
    for _ in 0..500 {
        h.step();
        if h.state().0.chart.tree().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.run_steps(3);
    let root = h.state().0.model.as_ref().unwrap().tree.roots()[0].node;
    assert_eq!(h.state().0.chart.selected_item(), Some(root));
    h.press_key(egui::Key::ArrowDown);
    h.run_steps(3);
    let large = h.state().0.model.as_ref().unwrap().cursor.unwrap();
    assert_eq!(h.state().0.chart.selected_item(), Some(large));
    // The file's type is outlined in the chart too.
    assert_eq!(
        h.state().0.chart.highlighted(),
        ["bin".to_owned()].as_slice()
    );
}

/// The disk usage window's text is translated (en/es key parity is checked in i18n::tests).
#[test]
fn disk_usage_window_strings_in_spanish() {
    use crate::i18n::tr;
    crate::i18n::set("es");
    assert_eq!(tr("disk_usage.select_drives"), "Elegir unidades…");
    assert_eq!(tr("disk_usage.rescan"), "Volver a analizar");
    assert_eq!(tr("disk_usage.stop"), "Detener");
    assert_eq!(tr("disk_usage.pause"), "Pausar");
    assert_eq!(tr("disk_usage.resume"), "Reanudar");
    assert_eq!(tr("disk_usage.paused"), "En pausa");
    assert_eq!(tr("disk_usage.on_disk"), "en disco");
    assert_eq!(tr("disk_usage.nothing_selected"), "Nada seleccionado");
    assert_eq!(tr("disk_usage.grid"), "Líneas de cuadrícula");
    assert_eq!(tr("disk_usage.collapse"), "Contraer");
    assert_eq!(tr("disk_usage.expand"), "Expandir");
    assert_eq!(tr("disk_usage.dismiss"), "Descartar");
    // Template keys interpolate their placeholders (a mismatch would leave a "{x}").
    assert_eq!(
        tr("disk_usage.scanning")
            .replace("{what}", "un dispositivo portátil")
            .replace("{files}", "42")
            .replace("{size}", "1.0 MB")
            .replace("{rate}", "7")
            .replace("{time}", "0:12"),
        "Analizando un dispositivo portátil: 42 archivos, 1.0 MB (7 archivos/s), 0:12"
    );
    assert_eq!(
        tr("disk_usage.items_selected")
            .replace("{n}", "3")
            .replace("{size}", "12 KB")
            .replace("{what}", "en disco"),
        "3 elementos seleccionados: 12 KB en disco"
    );
    assert_eq!(
        tr("disk_usage.unreadable").replace("{n}", "2"),
        ", 2 carpetas no se pudieron leer"
    );
    assert_eq!(
        tr("disk_usage.sorted_ascending").replace("{label}", "Tamaño"),
        "Tamaño, orden ascendente"
    );
    assert_eq!(
        tr("disk_usage.row_hover_parent")
            .replace("{pct}", "12.5")
            .replace("{name}", "nested")
            .replace("{bytes}", "234 B"),
        "\n12.5 % de nested (234 B)"
    );
    assert_eq!(
        tr("disk_usage.row_hover_drive").replace("{pct}", "99.0"),
        ", 99.0 % de la unidad"
    );
    assert_eq!(
        tr("disk_usage.rescan_note").replace("{d}", "Copia terminada"),
        "Copia terminada. Vuelve a analizar para ver el cambio."
    );
    crate::i18n::set("en");
}

/// The scan priority, the File Watcher switch and the previous scans are settings.
#[test]
fn priority_watch_and_history_round_trip_through_the_settings() {
    let mut s = DuSettings::default();
    assert_eq!(s.priority, du::Priority::Normal);
    assert!(s.watch, "watching is on by default");
    s.priority = du::Priority::Background;
    s.watch = false;
    super::history::remember(
        &mut s.history,
        super::history::Entry {
            roots: vec!["/data".into()],
            scanned: 1_700_000_000,
            bytes: 10,
            files: 2,
        },
    );
    let text = toml::to_string(&s).unwrap();
    assert!(text.contains("priority = \"background\""), "{text}");
    let back: DuSettings = toml::from_str(&text).unwrap();
    assert_eq!(back, s);
}

/// A newer version of the tree (the File Watcher's) keeps what is open and selected,
/// through the id map; items that are gone drop out.
#[test]
fn a_replaced_tree_keeps_the_selection_and_what_is_open() {
    let (dir, mut model) = fixture();
    let nested = id(&model, dir.path(), "nested");
    let other = id(&model, dir.path(), "nested/other.txt");
    let small = id(&model, dir.path(), "small.TXT");
    model.toggle(nested);
    model.select(other, false, false);
    model.select(small, true, false);
    std::fs::remove_file(dir.path().join("small.TXT")).unwrap();
    std::fs::write(dir.path().join("nested/new.bin"), vec![0; 50]).unwrap();
    let mut tree = (*model.tree).clone();
    let applied = tree
        .apply_changes(&[
            dir.path().join("small.TXT"),
            dir.path().join("nested/new.bin"),
        ])
        .unwrap();
    model.replace_tree(Arc::new(tree), applied.remap.as_deref());
    let nested = id(&model, dir.path(), "nested");
    let other = id(&model, dir.path(), "nested/other.txt");
    assert!(model.expanded.contains(&nested));
    assert_eq!(
        model.selected,
        BTreeSet::from([other]),
        "the removed file dropped out"
    );
    assert_eq!(model.cursor, None, "the cursor was on the removed file");
    let new = id(&model, dir.path(), "nested/new.bin");
    assert!(model.row_of(new).is_some(), "shown in the open folder");
    assert_eq!(model.bytes(nested), 81);
    assert!(model.extensions().any(|e| e == "bin"));
}

/// A pane's Disk usage view takes the folder from the window's scan when it holds it,
/// and scans the folder itself otherwise.
#[test]
fn the_pane_view_reuses_the_windows_scan_or_scans_the_folder() {
    let (dir, model) = fixture();
    let nested = dir.path().join("nested");
    let shared =
        super::view::Shared::from_settings(&DuSettings::default(), Some((7, model.tree.clone())));
    let mut view = super::view::PaneView::default();
    view.follow(&nested, &shared);
    assert!(!view.scanning(), "no scan of its own");
    let m = view.model.as_ref().expect("the window's tree");
    assert!(Arc::ptr_eq(&m.tree, &model.tree));
    let names: Vec<String> = (m.rows.iter())
        .map(|r| m.tree.node(r.0).name.to_string())
        .collect();
    assert_eq!(
        names,
        ["other.txt", "deeper"],
        "the folder's children, biggest first"
    );
    // A folder the window did not scan: scanned here.
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("x.dat"), vec![0; 7]).unwrap();
    view.follow(elsewhere.path(), &shared);
    assert!(view.scanning() && view.model.is_none());
    let ctx = egui::Context::default();
    for _ in 0..500 {
        view.poll(&ctx, &shared);
        if view.model.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let m = view.model.as_ref().expect("scanned");
    assert_eq!(m.rows.len(), 1);
    assert_eq!(m.bytes(m.base), 7);
    assert_eq!(view.dir(), Some(elsewhere.path()));
}

/// Owners are read for the rows asked for, and named once per account.
#[test]
fn owners_are_read_lazily_and_named_once() {
    let (dir, model) = fixture();
    let mut reader = super::MetaReader::default();
    let rows: Vec<NodeId> = model.rows.iter().map(|r| r.0).collect();
    let mut model = model;
    reader.want(&model, rows.clone());
    let ctx = egui::Context::default();
    for _ in 0..500 {
        reader.poll(&ctx, Some(&mut model));
        if !model.metadata.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let small = id(&model, dir.path(), "small.TXT");
    let owner = (model.metadata.get(&small)).and_then(|m| m.owner.clone());
    if cfg!(any(windows, unix)) {
        assert!(owner.is_some_and(|o| !o.is_empty()));
        assert_eq!(reader.owner_lookups(), 1, "one account, one lookup");
    }
}

/// The scan's progress line names a device storage the way the Select drives dialog did.
#[test]
fn the_progress_line_names_device_storages() {
    let phone = Root::Mtp {
        device_id: "phone".to_owned(),
        storage_id: "internal".to_owned(),
    };
    let camera = Root::Mtp {
        device_id: "camera".to_owned(),
        storage_id: "card".to_owned(),
    };
    let labels = vec![(phone.clone(), "Phone: Internal".to_owned())];
    assert_eq!(
        super::ui::roots_text(&[phone, camera], &labels),
        "Phone: Internal, a portable device"
    );
}

#[test]
fn selected_rows_are_tab_separated() {
    let (dir, mut model) = fixture();
    let small = id(&model, dir.path(), "small.TXT");
    model.selected.clear();
    let text = model.selected_tsv(small);
    assert!(text.starts_with("Path\tSize\tSize on disk\tItems\tFiles\tFolders\n"));
    let line = text.lines().nth(1).unwrap();
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(cols.len(), 6);
    assert!(cols[0].ends_with("small.TXT"));
    assert_eq!(cols[1], "10");
    assert_eq!(text.lines().count(), 2);
}

/// The scan options follow the window's scan switches.
#[test]
fn scan_options_follow_window_settings() {
    let du = DiskUsage {
        follow_junctions: true,
        follow_mount_points: true,
        exclude_hidden: true,
        exclude_system: true,
        ..DiskUsage::default()
    };
    let o = du.scan_options();
    assert!(o.follow_junctions && o.follow_mount_points);
    assert!(o.filters.exclude_hidden && o.filters.exclude_system);
    let plain = DiskUsage::default().scan_options();
    assert!(!plain.follow_junctions && !plain.filters.exclude_hidden);
}
