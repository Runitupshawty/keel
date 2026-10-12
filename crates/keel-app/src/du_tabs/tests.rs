//! The analysis tabs drawn headless over scans of temp trees.

use super::*;
use keel_core::Op;
use keel_vfs::du::{scan, Root, ScanOptions};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

fn put(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn fixture() -> (tempfile::TempDir, Arc<Tree>) {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    put(&r.join("a/one.bin"), &[7; 5000]);
    put(&r.join("b/two.bin"), &[7; 5000]);
    put(&r.join("b/notes.txt"), b"hello");
    put(&r.join("photo.jpg"), &[1; 3000]);
    let t = scan(&[Root::Path(r.to_owned())], &ScanOptions::default(), &ScanHandle::new()).unwrap();
    (tmp, Arc::new(t))
}

/// Draws `f` in a panel `frames` times; returns the last frame's result.
fn draw<T>(frames: usize, mut f: impl FnMut(&mut egui::Ui) -> T) -> T {
    let ctx = egui::Context::default();
    let mut last = None;
    for _ in 0..frames {
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| last = Some(f(ui)));
        });
    }
    last.unwrap()
}

/// Draws `f` until it returns true (or 20 s pass).
fn draw_until(mut f: impl FnMut(&mut egui::Ui) -> bool) {
    let ctx = egui::Context::default();
    let start = Instant::now();
    let mut done = false;
    while !done {
        assert!(start.elapsed().as_secs() < 20, "timed out");
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| done = f(ui));
        });
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn size_fields_parse_or_explain() {
    let mut f = SizeFields::default();
    assert!(f.range().unwrap().is_any());
    f.min = "2 KB".into();
    assert_eq!(f.range().unwrap().min, 2000);
    f.max = "1 KB".into();
    assert!(f.range().is_err());
    f.max = "a lot".into();
    assert!(f.range().unwrap_err().contains("a lot"));
}

#[test]
fn a_new_scan_is_noticed_once() {
    let (_tmp, t) = fixture();
    let mut seen = None;
    assert!(changed(&mut seen, &t));
    assert!(!changed(&mut seen, &t));
    let refreshed = Arc::new((*t).clone());
    assert!(changed(&mut seen, &refreshed));
}

#[test]
fn largest_files_tab_lists_the_biggest() {
    let (_tmp, t) = fixture();
    let mut st = largest::State::default();
    let out = draw(2, |ui| largest::ui(ui, &t, &mut st));
    assert_eq!(out, None);
    let sizes: Vec<u64> = st.rows().iter().map(|r| r.bytes).collect();
    assert_eq!(sizes, [5000, 5000, 3000, 5]);
    st.top = 1;
    st.size.min = "4 KB".into();
    st.apply(&t);
    assert_eq!(st.rows().len(), 1);
    st.size.min = "nonsense".into();
    st.apply(&t);
    assert_eq!(st.rows().len(), 1, "a bad size keeps the last list");
}

#[test]
fn search_tab_by_name_and_extension() {
    let (_tmp, t) = fixture();
    let mut st = search::State::for_extension("BIN");
    draw(2, |ui| search::ui(ui, &t, &mut st));
    assert_eq!((st.results().files, st.results().bytes), (2, 10_000));
    st.name = "^two".into();
    st.regex = true;
    st.apply(&t);
    assert_eq!(st.results().files, 1);
    st.name = "(".into();
    st.apply(&t);
    assert!(st.results().nodes.is_empty());
}

#[test]
fn storage_tab_computes_on_a_worker() {
    let (_tmp, t) = fixture();
    let mut st = storage::State::default();
    st.tiers[0].cost_per_gb = 1.0;
    draw_until(|ui| {
        storage::ui(ui, &t, &mut st);
        st.result().is_some()
    });
    let a = st.result().unwrap();
    // Everything was just written: all hot.
    assert_eq!(a.tiers[0].bytes, 13_005);
    assert!((a.total.monthly_cost - 13_005.0 / 1e9).abs() < 1e-12);
    assert_eq!(storage::cost_text(0.0), "0.00");
    assert_eq!(storage::cost_text(0.00012), "0.0001");
    assert_eq!(storage::cost_text(1234.4), "1234");
}

#[test]
fn duplicates_tab_finds_and_plans_keep_one_and_hard_links() {
    let (tmp, t) = fixture();
    let mut st = duplicates::State {
        min_size: String::new(),
        ..duplicates::State::default()
    };
    let ctx = egui::Context::default();
    st.start(&ctx, &t);
    draw_until(|ui| {
        duplicates::ui(ui, &t, &mut st);
        st.report().is_some()
    });
    let report = st.report().unwrap();
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.groups[0].files.len(), 2);

    let r = tmp.path();
    let (keep, other) = (r.join("a/one.bin"), r.join("b/two.bin"));
    let ops = duplicates::ops(duplicates::Action::Keep, &keep, std::slice::from_ref(&other));
    assert_eq!(
        ops,
        [Op::Delete {
            paths: vec![keel_vfs::VPath::local(&other)]
        }]
    );
    let ops = duplicates::ops(duplicates::Action::HardLink, &keep, std::slice::from_ref(&other));
    assert_eq!(ops.len(), 2);
    assert_eq!(
        ops[1],
        Op::Link {
            target: keel_vfs::VPath::local(&keep),
            link: keel_vfs::VPath::local(&other),
            kind: keel_vfs::LinkKind::Hardlink,
        }
    );
    let first = Outcome::Plans(ops).first_action();
    assert!(matches!(
        first,
        Some(crate::keys::Action::Library(crate::library::LibCmd::Plan(Op::Delete { .. })))
    ));
    // Nothing on disk changed.
    assert!(keep.exists() && other.exists());
}

#[test]
fn permissions_tab_reads_folders() {
    let (_tmp, t) = fixture();
    let mut st = permissions::State::default();
    let ctx = egui::Context::default();
    let tree = t.clone();
    let job = Worker::spawn(&ctx, "test", move |h| Ok(keel_vfs::du::permissions::scan(&tree, false, &h)));
    let start = Instant::now();
    let report = loop {
        if let Some(r) = job.poll() {
            break r.unwrap();
        }
        assert!(start.elapsed().as_secs() < 20);
        std::thread::sleep(Duration::from_millis(10));
    };
    // The root, a and b.
    assert_eq!(report.rows.len(), 3);
    assert!(report.rows.iter().all(|r| r.permissions.is_some()));
    // The tab draws before and without a report.
    draw(2, |ui| permissions::ui(ui, &t, &mut st));
    st.show = permissions::Show::Flagged;
    draw(1, |ui| permissions::ui(ui, &t, &mut st));
}

#[test]
fn reports_are_saved_whole_or_not_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("report.csv");
    save(&path, |w| Ok(w.write_all(b"a,b\r\n")?)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"a,b\r\n");
    let err = save(&path, |w| {
        w.write_all(b"partial")?;
        anyhow::bail!("disk full")
    });
    assert!(err.is_err());
    // The old report is untouched and no temp file is left.
    assert_eq!(fs::read(&path).unwrap(), b"a,b\r\n");
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
}

#[test]
fn time_text_is_local_or_empty() {
    assert_eq!(time_text(None), "");
    assert_eq!(time_text(Some(0)).len(), "1970-01-01 00:00".len());
}
