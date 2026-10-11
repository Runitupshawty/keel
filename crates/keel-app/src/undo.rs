//! Undo for file operations (Ctrl+Z). Each window keeps a journal of its last [`KEEP`]
//! finished operations, in memory: what each one left where ([`Item`]), recorded when its
//! job ends (the part that was done, when it stopped partway). Undoing one first checks
//! that every item is still as the operation left it and that its old place is free, and
//! does nothing but say why otherwise; it never overwrites and never picks a new name.
//! Operations that cannot be undone (permanent deletes, Overwrite, archives, other devices,
//! remote copies and moves) are not recorded: the next Ctrl+Z says so once
//! ([`Journal::refuse`]), the one after goes on to the entry before.

use crate::jobs::items;
use anyhow::Context as _;
use keel_vfs::ops::{Landed, Stamp};
use keel_vfs::{Conflict, Progress, Router, VPath};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

/// Entries kept per window.
pub const KEEP: usize = 20;

/// What an operation was.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Renames as they ran, bulk rename's temporary names included.
    Rename(Vec<(VPath, VPath)>),
    Move,
    Copy,
    /// New folder (true) or file.
    Create(bool),
    /// Delete to the Recycle Bin or Trash.
    Trash,
}

/// One item an operation left behind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// Where the operation left it: the new name, the moved item, the copy, the new item;
    /// for a delete, the item in the trash (a `trash://` path, or on macOS the file there).
    pub now: VPath,
    /// Where it was before (renames, moves, deletes); None for copies and new items.
    pub was: Option<VPath>,
    /// `now` as the operation left it; None: not compared (items in the trash).
    pub stamp: Option<Stamp>,
}

/// A finished operation that can be undone (serializable, so the journal could be kept
/// across restarts later).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub op: Op,
    pub items: Vec<Item>,
}

#[derive(Default)]
pub struct Journal {
    entries: VecDeque<Entry>,
    next: u64,
    /// Why the newest operation cannot be undone, for the next Ctrl+Z to say.
    refused: Option<String>,
}

impl Journal {
    /// Records a finished operation (nothing when it left nothing to undo).
    pub fn record(&mut self, op: Op, items: Vec<Item>) -> Option<&Entry> {
        if items.is_empty() {
            return None;
        }
        self.next += 1;
        self.refused = None;
        self.entries.push_back(Entry {
            id: self.next,
            op,
            items,
        });
        while self.entries.len() > KEEP {
            self.entries.pop_front();
        }
        self.entries.back()
    }

    /// The newest operation cannot be undone, because of `why` ("deleted permanently").
    pub fn refuse(&mut self, why: impl Into<String>) {
        self.refused = Some(why.into());
    }

    /// What Ctrl+Z works on: the newest entry, or (once) the toast saying why the newest
    /// operation cannot be undone, or that there is nothing to undo.
    pub fn next(&mut self) -> Result<u64, String> {
        if let Some(why) = self.refused.take() {
            return Err(format!("Can't undo: {why}"));
        }
        self.entries
            .back()
            .map(|e| e.id)
            .ok_or_else(|| "Nothing to undo".to_owned())
    }

    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn take(&mut self, id: u64) -> Option<Entry> {
        let i = self.entries.iter().position(|e| e.id == id)?;
        self.entries.remove(i)
    }

    /// Profile switch.
    pub fn clear(&mut self) {
        *self = Self {
            next: self.next,
            ..Self::default()
        };
    }

    /// The palette's "Undo <what>" for the newest entry (plain "Undo" while the newest
    /// operation is one that cannot be undone).
    pub fn label(&self) -> Option<String> {
        if self.refused.is_some() {
            return Some("Undo".into());
        }
        self.entries.back().map(|e| format!("Undo {}", e.noun()))
    }
}

/// The folder all of `paths` are in, when they share one.
fn common_parent<'a>(mut paths: impl Iterator<Item = &'a VPath>) -> Option<VPath> {
    let first = paths.next()?.parent()?;
    paths
        .all(|p| p.parent().as_ref() == Some(&first))
        .then_some(first)
}

impl Entry {
    pub fn noun(&self) -> &'static str {
        match self.op {
            Op::Rename(_) => "rename",
            Op::Move => "move",
            Op::Copy => "copy",
            Op::Create(true) => "new folder",
            Op::Create(false) => "new file",
            Op::Trash => "delete",
        }
    }

    fn nows(&self) -> impl Iterator<Item = &VPath> {
        self.items.iter().map(|i| &i.now)
    }

    fn wases(&self) -> impl Iterator<Item = &VPath> {
        self.items.iter().filter_map(|i| i.was.as_ref())
    }

    /// "to <folder>" / "back to <folder>" for where `paths` are, or `many` when they are
    /// in several folders.
    fn place<'a>(paths: impl Iterator<Item = &'a VPath>, word: &str, many: &str) -> String {
        match common_parent(paths) {
            Some(dir) => format!("{word} {}", dir.display()),
            None => many.to_owned(),
        }
    }

    /// The toast after the operation finished (with its Undo button).
    pub fn done_text(&self) -> String {
        let n = items(self.items.len());
        match &self.op {
            Op::Rename(_) => match self.items.as_slice() {
                [one] => format!(
                    "Renamed {} to {}",
                    one.was.as_ref().map_or("", |w| w.name()),
                    one.now.name()
                ),
                _ => format!("Renamed {n}"),
            },
            Op::Move => format!("Moved {n} {}", Self::place(self.nows(), "to", "")),
            Op::Copy => format!("Copied {n} {}", Self::place(self.nows(), "to", "")),
            Op::Create(_) => format!("Created {}", self.items[0].now.name()),
            Op::Trash => format!("Moved {n} to the trash"),
        }
        .trim_end()
        .to_owned()
    }

    /// The preview an undo that moves or deletes asks to confirm.
    pub fn preview(&self) -> String {
        let n = items(self.items.len());
        match &self.op {
            Op::Rename(_) => "Undo rename".to_owned(),
            Op::Move => format!(
                "Undo move: {n} back {}?",
                Self::place(self.wases(), "to", "to where they were")
            ),
            Op::Copy => format!(
                "Undo copy: move the {} {} to the trash?",
                if self.items.len() == 1 {
                    "copy".to_owned()
                } else {
                    format!("{} copies", self.items.len())
                },
                Self::place(self.nows(), "in", "")
            )
            .replace("  ", " "),
            Op::Create(_) => format!(
                "Undo {}: move {} to the trash?",
                self.noun(),
                self.items[0].now.name()
            ),
            Op::Trash => format!(
                "Undo delete: restore {n} {}?",
                Self::place(self.wases(), "to", "to where they were")
            ),
        }
    }

    /// Why this cannot be undone now, or Ok. `stat` gives a path's stamp, None when it
    /// is missing (items in the trash: Some when they are still there).
    pub fn check(&self, stat: impl Fn(&VPath) -> Option<Stamp>) -> Result<(), String> {
        if self.items.is_empty() {
            return Err("nothing left to undo".into());
        }
        let nows: HashSet<&VPath> = self.nows().collect();
        for it in &self.items {
            let name = it.now.name();
            let shown = match (&self.op, &it.was) {
                (Op::Trash, Some(was)) => was.name(),
                _ => name,
            };
            match stat(&it.now) {
                None if self.op == Op::Trash => {
                    return Err(format!(
                        "{shown} is no longer in the {}",
                        keel_vfs::trashbin::label()
                    ))
                }
                None => return Err(format!("{shown} is gone")),
                Some(now) if it.stamp.is_some_and(|s| s != now) => {
                    return Err(format!("{shown} changed since"))
                }
                Some(_) => {}
            }
            let Some(was) = &it.was else { continue };
            // A swap or a case-only rename frees its own old place.
            let vacated = nows.contains(was)
                || (cfg!(any(windows, target_os = "macos"))
                    && was.scheme == it.now.scheme
                    && was.path.eq_ignore_ascii_case(&it.now.path));
            if !vacated && stat(was).is_some() {
                return Err(format!("{} is taken", was.display()));
            }
            // Into its old folder, which must still be there (the Recycle Bin makes it).
            let into_folder = match self.op {
                Op::Move => true,
                Op::Trash => it.now.scheme == "file",
                _ => false,
            };
            if into_folder {
                if let Some(dir) = was.parent().filter(|d| stat(d).is_none()) {
                    return Err(format!("{} is gone", dir.display()));
                }
            }
        }
        Ok(())
    }
}

// ---- recording (workers: these touch the disk) ------------------------------------------

/// A path's stamp through its provider; None when it is missing. Trash items are looked up
/// in one listing of the Recycle Bin / Trash per call of `stat_fn`.
pub fn stat_fn(router: &Router) -> impl Fn(&VPath) -> Option<Stamp> + '_ {
    let bin = std::cell::OnceCell::<HashSet<VPath>>::new();
    move |p: &VPath| {
        if p.scheme == keel_vfs::trashbin::SCHEME {
            let listed = bin.get_or_init(|| {
                let root = keel_vfs::trashbin::root();
                let list = keel_vfs::Provider::list(&keel_vfs::TrashProvider, &root);
                list.unwrap_or_default()
                    .into_iter()
                    .map(|e| e.path)
                    .collect()
            });
            return listed.contains(p).then_some(Stamp::new(0, None));
        }
        let e = router.provider_for(p)?.stat(p).ok()?;
        Some(Stamp::new(e.size, e.modified))
    }
}

/// What a finished transfer can undo: the targets a copy made, or the sources a move took
/// away completely, as they are now.
pub fn landed(landed: &[Landed], mv: bool, stat: impl Fn(&VPath) -> Option<Stamp>) -> Vec<Item> {
    landed
        .iter()
        .filter(|l| l.new && (!mv || l.complete))
        .filter(|l| !mv || stat(&l.source).is_none())
        .filter_map(|l| {
            Some(Item {
                stamp: Some(stat(&l.target)?),
                now: l.target.clone(),
                was: mv.then(|| l.source.clone()),
            })
        })
        .collect()
}

/// The items of renames that ran (`done`, in order): each final name and the name it had
/// before the first step, as they are now. A step back to the old name cancels out.
pub fn renamed(done: &[(VPath, VPath)], stat: impl Fn(&VPath) -> Option<Stamp>) -> Vec<Item> {
    let mut origin: Vec<(VPath, VPath)> = Vec::new();
    for (a, b) in done {
        let first = match origin.iter().position(|(now, _)| now == a) {
            Some(i) => origin.remove(i).1,
            None => a.clone(),
        };
        origin.push((b.clone(), first));
    }
    origin
        .into_iter()
        .filter(|(now, was)| now != was)
        .filter_map(|(now, was)| {
            Some(Item {
                stamp: Some(stat(&now)?),
                now,
                was: Some(was),
            })
        })
        .collect()
}

/// One new item.
pub fn created(path: &VPath, stat: impl Fn(&VPath) -> Option<Stamp>) -> Vec<Item> {
    stat(path)
        .map(|stamp| Item {
            now: path.clone(),
            was: None,
            stamp: Some(stamp),
        })
        .into_iter()
        .collect()
}

/// Items `deleted` (local paths moved to the trash at or after `since`): where each is in
/// the trash, from what the trash call returned (`kept`, macOS) or the trash listing.
pub fn trashed(
    deleted: &[(VPath, Option<std::path::PathBuf>)],
    since: std::time::SystemTime,
) -> Vec<Item> {
    let originals: Vec<std::path::PathBuf> = deleted
        .iter()
        .filter_map(|(p, _)| p.to_local_path())
        .collect();
    let found = match keel_vfs::trashbin::find_trashed(&originals, since) {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!("finding deleted items in the trash: {e:#}");
            vec![None; originals.len()]
        }
    };
    deleted
        .iter()
        .zip(found)
        .filter_map(|((was, kept), found)| {
            let now = found.or_else(|| kept.as_ref().map(VPath::local))?;
            Some(Item {
                now,
                was: Some(was.clone()),
                stamp: None,
            })
        })
        .collect()
}

/// A library operation started from its preview, kept until its job ends.
#[derive(Clone, Debug)]
pub struct Planned {
    pub op: keel_core::Op,
    /// The targets that existed before (shown paths): a Skip left or merged them.
    pub existed: Vec<String>,
    pub since: std::time::SystemTime,
}

impl Planned {
    /// What the operation did, as the disk shows it now that its job ended (the part that
    /// was done, when it failed); or why it cannot be undone.
    pub fn done(&self, stat: impl Fn(&VPath) -> Option<Stamp>) -> Result<(Op, Vec<Item>), String> {
        use keel_core::Op as L;
        match &self.op {
            L::Copy { src, dst_dir, .. } | L::Move { src, dst_dir, .. } => {
                let mv = matches!(self.op, L::Move { .. });
                let t = crate::jobs::Transfer {
                    src: src.clone(),
                    dst: dst_dir.clone(),
                    mv,
                    extract: None,
                };
                if let Some(why) = transfer_refusal(&t, Conflict::Skip) {
                    return Err(why.into());
                }
                let reached: Vec<Landed> = src
                    .iter()
                    .map(|s| (s, dst_dir.join(s.name())))
                    .filter(|(_, t)| !self.existed.contains(&t.display()))
                    .map(|(s, t)| Landed {
                        source: s.clone(),
                        new: true,
                        complete: stat(&t).is_some(),
                        target: t,
                    })
                    .collect();
                let op = if mv { Op::Move } else { Op::Copy };
                Ok((op, landed(&reached, mv, stat)))
            }
            L::Rename { path, new_name } => {
                let dir = path.parent().ok_or("a rename at the root")?;
                if let Some(why) = place_refusal(&dir, true) {
                    return Err(why.into());
                }
                let steps = vec![(path.clone(), dir.join(new_name))];
                Ok((Op::Rename(steps.clone()), renamed(&steps, stat)))
            }
            L::Delete { paths } => {
                if paths.iter().any(|p| p.to_local_path().is_none()) {
                    return Err(crate::state::PERMANENT.into());
                }
                if !keel_vfs::trashbin::KNOWS_ORIGIN {
                    return Err("library deletes on macOS (restore them from the Trash)".into());
                }
                let deleted: Vec<_> = paths.iter().map(|p| (p.clone(), None)).collect();
                Ok((Op::Trash, trashed(&deleted, self.since)))
            }
        }
    }
}

/// Why a transfer cannot be undone (None: it can).
pub fn transfer_refusal(t: &crate::jobs::Transfer, conflict: Conflict) -> Option<&'static str> {
    if t.extract.is_some() {
        return Some("extracting from an archive");
    }
    let mut paths = t.src.iter().chain([&t.dst]);
    if paths.clone().any(|p| p.split_archive().is_some()) {
        return Some("changes inside archives");
    }
    if paths.clone().any(|p| p.scheme == "node") {
        return Some("files on another device");
    }
    if paths.any(|p| p.to_local_path().is_none()) {
        return Some("copies and moves on remote hosts and cloud accounts");
    }
    (conflict == Conflict::Overwrite).then_some("files were replaced by Overwrite")
}

/// Why a rename or a new item in `dir` cannot be undone (None: it can). New items need
/// a trash to go back to, so only local folders.
pub fn place_refusal(dir: &VPath, rename: bool) -> Option<&'static str> {
    if dir.split_archive().is_some() {
        Some("changes inside archives")
    } else if dir.scheme == "node" {
        Some("files on another device")
    } else if !rename && dir.to_local_path().is_none() {
        Some("new items on remote hosts and cloud accounts")
    } else {
        None
    }
}

// ---- undoing (a job) -----------------------------------------------------------------

fn provider(router: &Router, p: &VPath) -> anyhow::Result<std::sync::Arc<dyn keel_vfs::Provider>> {
    router
        .provider_for(p)
        .ok_or_else(|| anyhow::anyhow!("no provider for {}", p.display()))
}

/// Undoes `e`: checks it again, then reverses it. Stops at the first failure.
pub fn run(
    e: &Entry,
    router: &Router,
    report: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    e.check(stat_fn(router))
        .map_err(|why| anyhow::anyhow!("Can't undo: {why}"))?;
    let mut p = Progress {
        total_items: e.items.len(),
        ..Progress::default()
    };
    let step = |p: &mut Progress, path: &VPath| -> anyhow::Result<()> {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
        p.current = path.display();
        report(p.clone());
        Ok(())
    };
    match &e.op {
        Op::Rename(steps) => {
            let back = crate::bulk_rename::undo_steps(steps);
            let out =
                crate::bulk_rename::run_steps(&back, |a, b| provider(router, a)?.rename(a, b));
            if !out.failed.is_empty() {
                anyhow::bail!(crate::bulk_rename::summary(&out, back.len(), "Renamed back").0);
            }
        }
        Op::Move => {
            for it in &e.items {
                step(&mut p, &it.now)?;
                let was = it.was.as_ref().context("a move records where items were")?;
                let dir = was.parent().context("moved from a folder")?;
                if it.now.name() == was.name() {
                    // Skip: a clash that appeared since the check is never overwritten.
                    keel_vfs::ops::transfer(
                        std::slice::from_ref(&it.now),
                        &dir,
                        true,
                        Conflict::Skip,
                        &|_| {},
                        cancel,
                        router,
                    )?;
                } else {
                    // Moved under a new name (Keep both): the same volume renames back.
                    provider(router, was)?.rename(&it.now, was)?;
                }
                let stat = stat_fn(router);
                anyhow::ensure!(
                    stat(was).is_some() && stat(&it.now).is_none(),
                    "{} was left in {}",
                    it.now.name(),
                    it.now.parent().map(|d| d.display()).unwrap_or_default()
                );
                p.done_items += 1;
            }
        }
        Op::Copy | Op::Create(_) => {
            for it in &e.items {
                step(&mut p, &it.now)?;
                // Local only (`place_refusal`, `transfer_refusal`): to the trash.
                keel_vfs::local::trash(&it.now)?;
                p.done_items += 1;
            }
        }
        Op::Trash => {
            let (bin, kept): (Vec<&Item>, Vec<&Item>) = e
                .items
                .iter()
                .partition(|i| i.now.scheme == keel_vfs::trashbin::SCHEME);
            if !bin.is_empty() {
                // Restores nothing on Windows when an original path is taken.
                let paths: Vec<VPath> = bin.iter().map(|i| i.now.clone()).collect();
                keel_vfs::TrashProvider.restore_paths(&paths)?;
                p.done_items += bin.len();
            }
            for it in kept {
                step(&mut p, &it.now)?;
                let was = it
                    .was
                    .as_ref()
                    .context("a delete records where items were")?;
                // macOS: the file in the Trash goes back, never over anything.
                provider(router, was)?.rename(&it.now, was)?;
                p.done_items += 1;
            }
        }
    }
    report(p);
    Ok(())
}

#[cfg(test)]
#[path = "undo_tests.rs"]
mod tests;
