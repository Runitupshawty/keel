//! Task 85: replace confirmed duplicates with hard links. The duplicate finder's groups
//! (library BLAKE3 content ids) are planned on a worker: local, online copies only, and
//! within a group only the copies on the same volume as the one kept (the first such
//! copy); copies that already are that file (one hard link) are left out. The preview
//! lists every replacement; the job compares each pair byte by byte right before linking
//! it, makes the link under a temporary name next to the duplicate and renames it over
//! the duplicate (the same volume, so the duplicate is never missing). Undo (Ctrl+Z, the
//! toast) gives each path its own copy again with its old modified time.

use crate::keys::Action;
use crate::library::DupGroup;
use crate::system_cleanup::SysCmd;
use humansize::{format_size, DECIMAL};
use keel_vfs::ops::Stamp;
use keel_vfs::VPath;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

/// One replacement: `dup` becomes a hard link to `keep`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    pub keep: PathBuf,
    pub dup: PathBuf,
    pub size: u64,
}

/// Why a copy is left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// On a remote host, a cloud or another device.
    NotLocal,
    /// Its source is offline.
    Offline,
    /// Not on the volume of the copy kept: a hard link cannot cross volumes.
    OtherVolume,
    /// Already a hard link to the copy kept.
    AlreadyLinked,
    /// Gone, not a plain file, or its size is not the group's any more.
    Changed,
}

impl Skip {
    pub fn text(self) -> &'static str {
        match self {
            Skip::NotLocal => "not on this computer",
            Skip::Offline => "offline",
            Skip::OtherVolume => "on another volume",
            Skip::AlreadyLinked => "already hard links",
            Skip::Changed => "changed or gone",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DedupPlan {
    pub pairs: Vec<Pair>,
    pub skipped: Vec<(VPath, Skip)>,
}

impl DedupPlan {
    /// Bytes freed when every pair is linked.
    pub fn bytes(&self) -> u64 {
        self.pairs.iter().map(|p| p.size).sum()
    }

    /// "5 duplicates (1.2 GB) become hard links; 2 left alone (on another volume)".
    pub fn summary(&self) -> String {
        let n = match self.pairs.len() {
            1 => "1 duplicate".to_owned(),
            n => format!("{n} duplicates"),
        };
        let mut text = format!(
            "{n} ({}) become hard links to the copy kept",
            format_size(self.bytes(), DECIMAL)
        );
        if !self.skipped.is_empty() {
            let mut why: Vec<&str> = self.skipped.iter().map(|(_, s)| s.text()).collect();
            why.sort_unstable();
            why.dedup();
            text += &format!("; {} left alone ({})", self.skipped.len(), why.join(", "));
        }
        text
    }
}

/// The volume of a local path (see `jobs::volume_key`); None when it cannot be told.
fn volume(path: &Path) -> Option<String> {
    crate::jobs::volume_key(&VPath::local(path))
}

/// Whether `a` and `b` are on one volume (both readable).
pub fn same_volume(a: &Path, b: &Path) -> bool {
    matches!((volume(a), volume(b)), (Some(x), Some(y)) if x == y)
}

/// The file's identity (volume and file index or device and inode), None when unreadable.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    Stamp::new(0, None).with_file_id(path).id
}

/// Whether `a` and `b` are one file already (hard links to each other).
pub fn same_file(a: &Path, b: &Path) -> bool {
    matches!((file_id(a), file_id(b)), (Some(x), Some(y)) if x == y)
}

/// A plain file of `size` bytes (not a link, not a folder).
fn plain_file(path: &Path, size: u64) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file() && m.len() == size)
}

/// The replacements for `groups` (worker: reads the disk).
pub fn plan(groups: &[DupGroup]) -> DedupPlan {
    plan_by(groups, volume)
}

/// [`plan`] with the volume of each path from `vol`.
fn plan_by(groups: &[DupGroup], vol: impl Fn(&Path) -> Option<String>) -> DedupPlan {
    let mut out = DedupPlan::default();
    for g in groups {
        // (shown path, local path, volume) of the usable copies, in the finder's order.
        let mut usable = Vec::new();
        for r in &g.records {
            match r.path.to_local_path() {
                _ if r.offline => out.skipped.push((r.path.clone(), Skip::Offline)),
                None => out.skipped.push((r.path.clone(), Skip::NotLocal)),
                Some(l) if !plain_file(&l, g.size) => {
                    out.skipped.push((r.path.clone(), Skip::Changed))
                }
                Some(l) => {
                    let v = vol(&l);
                    usable.push((r.path.clone(), l, v));
                }
            }
        }
        // The volume holding most copies (the first such on a tie); a hard link cannot
        // cross volumes, and one volume per group keeps the preview readable.
        let mut best: Option<(&String, usize)> = None;
        for (_, _, v) in &usable {
            let Some(v) = v else { continue };
            let n = usable.iter().filter(|(_, _, u)| u.as_ref() == Some(v)).count();
            if best.is_none_or(|(_, m)| n > m) {
                best = Some((v, n));
            }
        }
        let best = best.map(|(v, _)| v.clone());
        let mut on = Vec::new();
        for (shown, local, v) in usable {
            if v.is_some() && v == best {
                on.push(local);
            } else {
                out.skipped.push((shown, Skip::OtherVolume));
            }
        }
        // The first copy on that volume is kept; a lone copy has nothing to link to.
        let Some((keep, dups)) = on.split_first() else {
            continue;
        };
        for dup in dups {
            if same_file(keep, dup) {
                out.skipped.push((VPath::local(dup), Skip::AlreadyLinked));
            } else {
                out.pairs.push(Pair {
                    keep: keep.clone(),
                    dup: dup.clone(),
                    size: g.size,
                });
            }
        }
    }
    out
}

/// Whether the two files hold the same bytes (read side by side, 1 MiB at a time).
pub fn same_content(a: &Path, b: &Path, cancel: &AtomicBool) -> anyhow::Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    loop {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
        let n = read_full(&mut fa, &mut ba)?;
        let m = read_full(&mut fb, &mut bb)?;
        if n != m || ba[..n] != bb[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Fills `buf` unless the file ends first; the bytes read.
fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// A free name next to `path` for a temporary item (`.<name>.keel-<what>[-n]`).
fn temp_beside(path: &Path, what: &str) -> anyhow::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} has no name", path.display()))?
        .to_string_lossy();
    for n in 0..100 {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let tmp = path.with_file_name(format!(".{name}.keel-{what}{suffix}"));
        if fs::symlink_metadata(&tmp).is_err() {
            return Ok(tmp);
        }
    }
    anyhow::bail!("no free temporary name next to {}", path.display())
}

/// What a link replaced: the path and its modified time before (restored by the undo).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Linked {
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
}

/// Makes `pair.dup` a hard link to `pair.keep` after checking that both are plain files
/// of the planned size on one volume, not one file already, the duplicate is not
/// read-only, and their bytes are equal. The duplicate is replaced in one rename.
pub fn link(pair: &Pair, cancel: &AtomicBool) -> anyhow::Result<Linked> {
    let (keep, dup) = (&pair.keep, &pair.dup);
    anyhow::ensure!(
        plain_file(keep, pair.size) && plain_file(dup, pair.size),
        "{} or {} changed since the preview",
        keep.display(),
        dup.display()
    );
    anyhow::ensure!(
        same_volume(keep, dup),
        "{} is not on the volume of {}",
        dup.display(),
        keep.display()
    );
    anyhow::ensure!(
        !same_file(keep, dup),
        "{} already is a hard link to {}",
        dup.display(),
        keep.display()
    );
    let before = fs::symlink_metadata(dup)?;
    anyhow::ensure!(
        !before.permissions().readonly(),
        "{} is read-only",
        dup.display()
    );
    anyhow::ensure!(
        same_content(keep, dup, cancel)?,
        "{} and {} are not the same bytes",
        keep.display(),
        dup.display()
    );
    let tmp = temp_beside(dup, "link")?;
    fs::hard_link(keep, &tmp)
        .map_err(|e| anyhow::anyhow!("linking {} to {}: {e}", dup.display(), keep.display()))?;
    // Changed while it was compared: leave it.
    let now = fs::symlink_metadata(dup).ok();
    let unchanged = now.is_some_and(|m| {
        m.len() == before.len() && m.modified().ok() == before.modified().ok()
    });
    if !unchanged {
        let _ = fs::remove_file(&tmp);
        anyhow::bail!("{} changed while it was compared", dup.display());
    }
    if let Err(e) = fs::rename(&tmp, dup) {
        let _ = fs::remove_file(&tmp);
        anyhow::bail!("replacing {}: {e}", dup.display());
    }
    Ok(Linked {
        path: dup.clone(),
        mtime: before.modified().ok(),
    })
}

/// The undo of [`link`]: `path` gets its own copy of the bytes again (copied next to it,
/// its old modified time set, renamed over the link).
pub fn unlink(path: &Path, mtime: Option<SystemTime>) -> anyhow::Result<()> {
    let tmp = temp_beside(path, "copy")?;
    let copied = (|| -> anyhow::Result<()> {
        fs::copy(path, &tmp)?;
        if let Some(t) = mtime {
            File::options().write(true).open(&tmp)?.set_modified(t)?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(e) = copied {
        let _ = fs::remove_file(&tmp);
        anyhow::bail!("copying {} back: {e:#}", path.display());
    }
    Ok(())
}

/// The job: links each pair; pairs that fail a check are skipped and counted. Returns what
/// was linked (also when cancelled partway) and why each skipped pair was.
pub fn run(
    plan: &DedupPlan,
    report: &dyn Fn(keel_vfs::Progress),
    cancel: &AtomicBool,
) -> (Vec<Linked>, Vec<String>) {
    let mut p = keel_vfs::Progress {
        total_items: plan.pairs.len(),
        total_bytes: plan.bytes(),
        ..keel_vfs::Progress::default()
    };
    let (mut linked, mut failed) = (Vec::new(), Vec::new());
    for pair in &plan.pairs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        p.current = pair.dup.display().to_string();
        report(p.clone());
        match link(pair, cancel) {
            Ok(l) => linked.push(l),
            Err(e) => {
                p.skipped += 1;
                failed.push(format!("{e:#}"));
            }
        }
        p.done_items += 1;
        p.done_bytes += pair.size;
    }
    report(p);
    (linked, failed)
}

/// The preview dialog's state.
#[derive(Debug)]
pub struct Preview {
    pub plan: DedupPlan,
}

/// The preview (inside `dialogs::show`'s modal): the summary, the replacements (the first
/// 200), the copies left alone, and the button.
pub fn ui(ui: &mut egui::Ui, d: &mut Preview, cancel: &mut bool) -> Option<Action> {
    let mut out = None;
    ui.strong("Replace duplicates with hard links");
    if d.plan.pairs.is_empty() {
        ui.label(if d.plan.skipped.is_empty() {
            "No duplicates to replace.".to_owned()
        } else {
            format!("Nothing to replace: {}", d.plan.summary())
        });
    } else {
        ui.label(d.plan.summary());
        ui.weak(
            "Each pair is compared byte by byte right before it is linked. Linked files \
             share their bytes: a change to one shows in all of them. Undo gives each its \
             own copy again.",
        );
    }
    egui::ScrollArea::vertical()
        .max_height(220.0)
        .show(ui, |ui| {
            for pair in d.plan.pairs.iter().take(200) {
                ui.add(egui::Label::new(format!("{}", pair.dup.display())).truncate());
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("  → {}", pair.keep.display())).weak(),
                    )
                    .truncate(),
                );
            }
            if d.plan.pairs.len() > 200 {
                ui.weak(format!("and {} more", d.plan.pairs.len() - 200));
            }
            for (path, why) in d.plan.skipped.iter().take(50) {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("{} ({})", path.display(), why.text())).weak(),
                    )
                    .truncate(),
                );
            }
        });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let n = d.plan.pairs.len();
        if n > 0 {
            let label = match n {
                1 => "Replace 1 file".to_owned(),
                n => format!("Replace {n} files"),
            };
            let b = ui.button(label);
            crate::a11y::first_focus(&b);
            if b.clicked() {
                out = Some(Action::System(SysCmd::DedupRun(Box::new(d.plan.clone()))));
            }
        }
        *cancel |= ui.button("Cancel").clicked();
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::DupRecord;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("keel-dedup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn group(size: u64, paths: &[&Path]) -> DupGroup {
        DupGroup {
            size,
            records: paths
                .iter()
                .map(|p| DupRecord {
                    path: VPath::local(p),
                    source: "src".into(),
                    offline: false,
                })
                .collect(),
        }
    }

    #[test]
    fn plans_same_volume_copies_and_skips_the_rest() {
        let d = tmp("plan");
        let (a, b, c) = (d.join("a.bin"), d.join("b.bin"), d.join("c.bin"));
        for p in [&a, &b] {
            fs::write(p, b"same bytes").unwrap();
        }
        fs::hard_link(&a, &c).unwrap(); // already one file with a
        let mut g = group(10, &[&a, &b, &c]);
        g.records.push(DupRecord {
            path: VPath::parse("sftp://host/x/a.bin").unwrap(),
            source: "host".into(),
            offline: false,
        });
        g.records.push(DupRecord {
            path: VPath::local(d.join("gone.bin")),
            source: "src".into(),
            offline: false,
        });
        g.records.push(DupRecord {
            path: VPath::local(d.join("off.bin")),
            source: "usb".into(),
            offline: true,
        });
        let plan = plan(&[g]);
        assert_eq!(
            plan.pairs,
            [Pair {
                keep: a.clone(),
                dup: b.clone(),
                size: 10
            }]
        );
        let why: Vec<Skip> = plan.skipped.iter().map(|(_, s)| *s).collect();
        assert_eq!(
            why,
            [Skip::NotLocal, Skip::Changed, Skip::Offline, Skip::AlreadyLinked]
        );
        assert_eq!(plan.bytes(), 10);
        let s = plan.summary();
        assert!(s.starts_with("1 duplicate (10 B) become hard links"), "{s}");
        assert!(s.contains("4 left alone"), "{s}");
        assert!(same_volume(&a, &b));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn byte_compare_finds_any_difference() {
        let d = tmp("cmp");
        let cancel = AtomicBool::new(false);
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(d.join("x"), &big).unwrap();
        fs::write(d.join("y"), &big).unwrap();
        assert!(same_content(&d.join("x"), &d.join("y"), &cancel).unwrap());
        let mut last = big.clone();
        *last.last_mut().unwrap() ^= 1; // past the first 1 MiB block
        fs::write(d.join("z"), &last).unwrap();
        assert!(!same_content(&d.join("x"), &d.join("z"), &cancel).unwrap());
        fs::write(d.join("short"), &big[..10]).unwrap();
        assert!(!same_content(&d.join("x"), &d.join("short"), &cancel).unwrap());
        fs::write(d.join("e1"), b"").unwrap();
        fs::write(d.join("e2"), b"").unwrap();
        assert!(same_content(&d.join("e1"), &d.join("e2"), &cancel).unwrap());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn links_after_a_byte_compare_and_undo_copies_back() {
        let d = tmp("link");
        let (keep, dup) = (d.join("keep.txt"), d.join("dup.txt"));
        fs::write(&keep, b"identical content").unwrap();
        fs::write(&dup, b"identical content").unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        File::options()
            .write(true)
            .open(&dup)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let pair = Pair {
            keep: keep.clone(),
            dup: dup.clone(),
            size: 17,
        };
        let cancel = AtomicBool::new(false);
        let linked = link(&pair, &cancel).unwrap();
        assert_eq!(linked.path, dup);
        assert_eq!(linked.mtime, Some(old));
        assert!(same_file(&keep, &dup), "one file now");
        assert_eq!(fs::read(&dup).unwrap(), b"identical content");
        // No temporary names left behind.
        let names: Vec<_> = fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        // A second time: already linked.
        assert!(link(&pair, &cancel).unwrap_err().to_string().contains("already"));

        unlink(&dup, linked.mtime).unwrap();
        assert!(!same_file(&keep, &dup), "its own copy again");
        assert_eq!(fs::read(&dup).unwrap(), b"identical content");
        assert_eq!(fs::metadata(&dup).unwrap().modified().unwrap(), old);
        // Writing one no longer changes the other.
        fs::write(&dup, b"changed").unwrap();
        assert_eq!(fs::read(&keep).unwrap(), b"identical content");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn undo_journal_entry_checks_then_copies_back() {
        use crate::undo::{run as undo, stat_fn, Entry, Item, Op};
        let d = tmp("undo");
        let (keep, dup) = (d.join("keep.bin"), d.join("dup.bin"));
        fs::write(&keep, [7u8; 4096]).unwrap();
        fs::write(&dup, [7u8; 4096]).unwrap();
        let cancel = AtomicBool::new(false);
        let pair = Pair {
            keep: keep.clone(),
            dup: dup.clone(),
            size: 4096,
        };
        let linked = link(&pair, &cancel).unwrap();
        let router = keel_vfs::Router::new();
        let now = VPath::local(&dup);
        let entry = Entry {
            id: 1,
            op: Op::Hardlink(vec![(now.clone(), linked.mtime)]),
            items: vec![Item {
                now: now.clone(),
                was: None,
                stamp: stat_fn(&router)(&now),
            }],
        };
        assert!(entry.items[0].stamp.is_some());
        assert_eq!(entry.done_text(), "Replaced 1 item with hard links");
        assert!(entry.preview().starts_with("Undo hard links"));
        assert!(entry.check(stat_fn(&router)).is_ok());
        undo(&entry, &router, &|_| {}, &cancel).unwrap();
        assert!(!same_file(&keep, &dup));
        assert_eq!(fs::read(&dup).unwrap(), [7u8; 4096]);
        // Changed since (its own copy now): a second undo is refused.
        assert!(entry.check(stat_fn(&router)).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn refuses_different_bytes_and_read_only_and_changed_sizes() {
        let d = tmp("refuse");
        let cancel = AtomicBool::new(false);
        let (keep, dup) = (d.join("k"), d.join("d"));
        fs::write(&keep, b"aaaa").unwrap();
        fs::write(&dup, b"aaab").unwrap();
        let pair = Pair {
            keep: keep.clone(),
            dup: dup.clone(),
            size: 4,
        };
        let e = link(&pair, &cancel).unwrap_err().to_string();
        assert!(e.contains("not the same bytes"), "{e}");
        assert_eq!(fs::read(&dup).unwrap(), b"aaab", "left alone");
        // A size other than the plan's.
        fs::write(&dup, b"aaaaa").unwrap();
        let e = link(&pair, &cancel).unwrap_err().to_string();
        assert!(e.contains("changed since the preview"), "{e}");
        // Read-only.
        fs::write(&dup, b"aaaa").unwrap();
        let mut perm = fs::metadata(&dup).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&dup, perm.clone()).unwrap();
        let e = link(&pair, &cancel).unwrap_err().to_string();
        assert!(e.contains("read-only"), "{e}");
        perm.set_readonly(false);
        fs::set_permissions(&dup, perm).unwrap();
        // The job skips and counts failures, links the rest.
        let other = d.join("o");
        fs::write(&other, b"aaaa").unwrap();
        let plan = DedupPlan {
            pairs: vec![
                Pair {
                    keep: keep.clone(),
                    dup: other.clone(),
                    size: 4,
                },
                Pair {
                    keep: keep.clone(),
                    dup: d.join("missing"),
                    size: 4,
                },
            ],
            skipped: Vec::new(),
        };
        let (linked, failed) = run(&plan, &|_| {}, &cancel);
        assert_eq!(linked.len(), 1);
        assert_eq!(failed.len(), 1);
        assert!(same_file(&keep, &other));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn copies_on_other_volumes_are_left_alone() {
        let d = tmp("vol");
        for n in ["a", "b", "c", "d"] {
            fs::write(d.join(n), b"x").unwrap();
        }
        // a on one volume, b c d on another: b keeps, c and d link to it.
        let paths: Vec<PathBuf> = ["a", "b", "c", "d"].iter().map(|n| d.join(n)).collect();
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let a = paths[0].clone();
        let plan = plan_by(&[group(1, &refs)], |p| {
            Some(if p == a { "vol1" } else { "vol2" }.to_owned())
        });
        let dups: Vec<&PathBuf> = plan.pairs.iter().map(|p| &p.dup).collect();
        assert_eq!(dups, [&paths[2], &paths[3]]);
        assert!(plan.pairs.iter().all(|p| p.keep == paths[1]));
        assert_eq!(plan.skipped, [(VPath::local(&paths[0]), Skip::OtherVolume)]);
        // A volume that cannot be told: never linked.
        let plan = plan_by(&[group(1, &refs)], |_| None);
        assert!(plan.pairs.is_empty());
        assert_eq!(plan.skipped.len(), 4);
        let _ = fs::remove_dir_all(&d);
    }
}
