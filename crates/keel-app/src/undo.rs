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
    /// Merge, flatten or organize: the items go back; the folders in `removed` (emptied
    /// and removed by it) are made again first, those in `made` removed after when empty.
    /// `merged` items went into a folder that was there and stay (the toast says so).
    Arrange {
        what: Arranged,
        made: Vec<VPath>,
        removed: Vec<VPath>,
        #[serde(default)]
        merged: usize,
    },
    /// Attributes set: each item gets these back.
    Attributes(Vec<(VPath, keel_vfs::Attributes)>),
    /// A link made (`now`): removed again (a hard link goes to the trash).
    Link,
    /// Task 85: duplicates replaced with hard links: each path gets its own copy of the
    /// bytes again, with its modified time from before.
    Hardlink(Vec<(VPath, Option<std::time::SystemTime>)>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arranged {
    Merge,
    Flatten,
    Organize,
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
    /// Bumped by each profile switch: an operation started before it is not recorded.
    generation: u64,
}

/// Why an entry cannot be undone now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub why: String,
    /// It may pass (a place taken, a folder that cannot be reached): the entry stays.
    pub transient: bool,
}

impl Refusal {
    fn lasting(why: String) -> Self {
        Self {
            why,
            transient: false,
        }
    }
    fn transient(why: String) -> Self {
        Self {
            why,
            transient: true,
        }
    }
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

    /// Profile switch: everything goes, and what operations still running report later
    /// (tagged with the old [`generation`](Self::generation)) is not recorded.
    pub fn clear(&mut self) {
        *self = Self {
            next: self.next,
            generation: self.generation + 1,
            ..Self::default()
        };
    }

    /// What operations started now are tagged with (`Msg::Undoable`).
    pub fn generation(&self) -> u64 {
        self.generation
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
            Op::Arrange { what, .. } => match what {
                Arranged::Merge => "merge",
                Arranged::Flatten => "flatten",
                Arranged::Organize => "organize",
            },
            Op::Attributes(_) => "attributes",
            Op::Link => "link",
            Op::Hardlink(_) => "hard links",
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
            Op::Arrange { what, merged, .. } => {
                let done = match what {
                    Arranged::Merge => {
                        format!("Merged {n} {}", Self::place(self.nows(), "into", ""))
                    }
                    Arranged::Flatten => format!("Flattened: {n} moved up"),
                    Arranged::Organize => format!("Organized {n} into folders"),
                };
                match merged {
                    0 => done,
                    m => format!("{}; {}", done.trim_end(), Self::merged_note(*m)),
                }
            }
            Op::Attributes(_) => format!("Changed the attributes of {n}"),
            Op::Link => format!("Made the link {}", self.items[0].now.name()),
            Op::Hardlink(_) => format!("Replaced {n} with hard links"),
        }
        .trim_end()
        .to_owned()
    }

    /// Items that merged into a folder that was there: Undo cannot take them out alone.
    fn merged_note(merged: usize) -> String {
        let (n, it) = match merged {
            1 => ("1 item".to_owned(), "it"),
            m => (format!("{m} items"), "them"),
        };
        format!("{n} merged into a folder already there, so Undo leaves {it}")
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
            Op::Arrange { merged, .. } => {
                let back = format!(
                    "Undo {}: move {n} back {}?",
                    self.noun(),
                    Self::place(self.wases(), "to", "to where they were")
                );
                match merged {
                    0 => back,
                    m => format!("{back} ({})", Self::merged_note(*m)),
                }
            }
            Op::Attributes(_) => format!("Undo attributes: set {n} back as they were?"),
            Op::Link => format!("Undo link: remove {}?", self.items[0].now.name()),
            Op::Hardlink(_) => format!("Undo hard links: give {n} their own copy again?"),
        }
    }

    /// Why this cannot be undone now, or Ok. `stat` gives a path's stamp, None when it
    /// is missing (items in the trash: Some when they are still there). An item that
    /// changed, is gone or left the trash refuses for good; a taken place or a folder that
    /// cannot be reached may pass (`transient`).
    pub fn check(&self, stat: impl Fn(&VPath) -> Option<Stamp>) -> Result<(), Refusal> {
        if self.items.is_empty() {
            return Err(Refusal::lasting("nothing left to undo".into()));
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
                    return Err(Refusal::lasting(format!(
                        "{shown} is no longer in the {}",
                        keel_vfs::trashbin::label()
                    )))
                }
                None => {
                    // Its folder gone too: an unplugged drive or a share offline.
                    return Err(match it.now.parent().filter(|d| stat(d).is_none()) {
                        Some(dir) => {
                            Refusal::transient(format!("{} can't be reached", dir.display()))
                        }
                        None => Refusal::lasting(format!("{shown} is gone")),
                    });
                }
                Some(now) if it.stamp.is_some_and(|s| s != now) => {
                    return Err(Refusal::lasting(format!("{shown} changed since")))
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
                return Err(Refusal::transient(format!("{} is taken", was.display())));
            }
            // Into its old folder, which must still be there (the Recycle Bin makes it, an
            // undone merge or flatten makes again the folders it removed).
            let into_folder = match &self.op {
                Op::Move => true,
                Op::Trash => it.now.scheme == "file",
                Op::Arrange { removed, .. } => !removed.iter().any(|r| {
                    was.path
                        .strip_prefix(r.path.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
                }),
                _ => false,
            };
            if into_folder {
                if let Some(dir) = was.parent().filter(|d| stat(d).is_none()) {
                    return Err(Refusal::transient(format!("{} is gone", dir.display())));
                }
            }
        }
        Ok(())
    }
}

// ---- recording (workers: these touch the disk) ------------------------------------------

/// A path's stamp through its provider, with the file's identity for local paths; None when
/// it is missing. Trash items are looked up in one listing of the Recycle Bin / Trash per
/// call of `stat_fn`.
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
        let stamp = Stamp::new(e.size, e.modified);
        Some(match p.to_local_path() {
            Some(local) => stamp.with_file_id(&local),
            None => stamp,
        })
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
/// the trash, from the trash listing or what the trash call returned (`kept`, macOS; with
/// its stamp from `stat`, so a later file of the same name there is not taken for it). Err
/// when fewer were found than deleted: the delete cannot be undone whole.
pub fn trashed(
    deleted: &[(VPath, Option<std::path::PathBuf>)],
    since: std::time::SystemTime,
    stat: impl Fn(&VPath) -> Option<Stamp>,
) -> Result<Vec<Item>, String> {
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
    let items: Vec<Item> = deleted
        .iter()
        .zip(found)
        .filter_map(|((was, kept), found)| {
            let (now, stamp) = match (found, kept) {
                (Some(now), _) => (now, None),
                (None, Some(kept)) => {
                    let now = VPath::local(kept);
                    let stamp = stat(&now)?;
                    (now, Some(stamp))
                }
                (None, None) => return None,
            };
            Some(Item {
                now,
                was: Some(was.clone()),
                stamp,
            })
        })
        .collect();
    if items.len() < deleted.len() {
        let what = match deleted {
            [(one, _)] => one.name().to_owned(),
            _ => items_of(deleted.len() - items.len(), deleted.len()),
        };
        return Err(format!(
            "couldn't find {what} in the {}",
            keel_vfs::trashbin::label()
        ));
    }
    Ok(items)
}

/// "2 of the 5 items".
fn items_of(missing: usize, all: usize) -> String {
    format!("{missing} of the {}", items(all))
}

/// A library operation started from its preview, kept until its job ends.
#[derive(Clone, Debug)]
pub struct Planned {
    pub op: keel_core::Op,
    /// The targets that existed before (shown paths): a Skip left or merged them.
    pub existed: Vec<String>,
    pub since: std::time::SystemTime,
    /// The journal's generation when it started.
    pub generation: u64,
    /// A folder operation's moves (item, where it goes) as previewed, in order.
    pub moves: Vec<(VPath, VPath)>,
    /// The folders it makes (organize) and removes when emptied (merge, flatten).
    pub made: Vec<VPath>,
    pub prune: Vec<VPath>,
    /// Read just before it ran: the targets of `moves` that existed (an item that merged
    /// into one stays), and the attributes an attribute plan changes.
    pub taken: Vec<VPath>,
    pub before: Vec<(VPath, keel_vfs::Attributes)>,
}

impl Planned {
    /// Reads, on a worker just before the plan runs, what undo needs to know about the disk
    /// as it was: the targets of a folder operation's moves that exist, an attribute
    /// plan's attributes.
    pub fn read_before(&mut self, stat: impl Fn(&VPath) -> Option<Stamp>) {
        self.taken = (self.moves.iter())
            .map(|(_, to)| to)
            .filter(|to| stat(to).is_some())
            .cloned()
            .collect();
        if let keel_core::Op::SetAttributes { paths, .. } = &self.op {
            self.before = (paths.iter())
                .filter_map(|p| {
                    let a = keel_vfs::fsattr::attributes(&p.to_local_path()?).ok()?;
                    Some((p.clone(), a))
                })
                .collect();
        }
    }

    /// What the operation did, as the disk shows it now that its job ended (the part that
    /// was done, when it failed); or why it cannot be undone.
    pub fn done(&self, stat: impl Fn(&VPath) -> Option<Stamp>) -> Result<(Op, Vec<Item>), String> {
        use keel_core::Op as L;
        match &self.op {
            L::Copy {
                src,
                dst_dir,
                on_conflict,
            }
            | L::Move {
                src,
                dst_dir,
                on_conflict,
            } => {
                let mv = matches!(self.op, L::Move { .. });
                let t = crate::jobs::Transfer {
                    src: src.clone(),
                    dst: dst_dir.clone(),
                    mv,
                    extract: None,
                };
                if let Some(why) = transfer_refusal(&t, (*on_conflict).into()) {
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
            L::Arrange {
                what: keel_core::FolderOp::RemoveEmpty { .. },
                ..
            } => {
                // The empty branches went to the trash: they come back from there.
                if self.prune.iter().any(|p| p.to_local_path().is_none()) {
                    return Err(crate::state::PERMANENT.into());
                }
                if !keel_vfs::trashbin::KNOWS_ORIGIN {
                    return Err("removed folders on macOS (restore them from the Trash)".into());
                }
                let deleted: Vec<_> = (self.prune.iter())
                    .filter(|p| stat(p).is_none())
                    .map(|p| (p.clone(), None))
                    .collect();
                Ok((Op::Trash, trashed(&deleted, self.since, stat)?))
            }
            L::Arrange {
                what, on_conflict, ..
            } => {
                let paths = self.moves.iter().flat_map(|(a, b)| [a, b]);
                if paths.clone().any(|p| p.to_local_path().is_none()) {
                    return Err("folder operations on remote hosts and cloud accounts".into());
                }
                if *on_conflict == keel_core::OnConflict::Overwrite {
                    return Err("files were replaced by Overwrite".into());
                }
                // An item gone into a folder that was there (Skip), or into one another item
                // of the plan landed on too, cannot go back alone (what is in that folder now
                // is both): its target is left out of the undo, and the toast says so.
                let mut seen = HashSet::new();
                let shared: HashSet<&VPath> = (self.moves.iter())
                    .map(|(_, to)| to)
                    .filter(|to| !seen.insert(*to))
                    .collect();
                let (mut items, mut merged) = (Vec::new(), 0);
                for (from, to) in &self.moves {
                    if stat(from).is_some() {
                        continue;
                    }
                    if shared.contains(to) || self.taken.contains(to) {
                        // Keep both put it under another name (not known here).
                        merged += usize::from(*on_conflict == keel_core::OnConflict::Skip);
                        continue;
                    }
                    if let Some(stamp) = stat(to) {
                        items.push(Item {
                            now: to.clone(),
                            was: Some(from.clone()),
                            stamp: Some(stamp),
                        });
                    }
                }
                let what = match what {
                    keel_core::FolderOp::Merge { .. } => Arranged::Merge,
                    keel_core::FolderOp::Flatten { .. } => Arranged::Flatten,
                    keel_core::FolderOp::Organize { .. } => Arranged::Organize,
                    keel_core::FolderOp::RemoveEmpty { .. } => unreachable!("matched above"),
                };
                let removed = (self.prune.iter())
                    .filter(|d| stat(d).is_none())
                    .cloned()
                    .collect();
                if items.is_empty() && merged > 0 {
                    return Err(Entry::merged_note(merged));
                }
                let op = Op::Arrange {
                    what,
                    made: self.made.clone(),
                    removed,
                    merged,
                };
                Ok((op, items))
            }
            L::SetAttributes { paths, .. } => {
                if self.before.len() < paths.len() {
                    return Err("the attributes before could not be read".into());
                }
                let items = (self.before.iter())
                    .map(|(p, _)| Item {
                        now: p.clone(),
                        was: None,
                        stamp: None,
                    })
                    .collect();
                Ok((Op::Attributes(self.before.clone()), items))
            }
            L::Link { link, .. } => {
                let items = match stat(link) {
                    Some(_) => vec![Item {
                        now: link.clone(),
                        was: None,
                        // A junction's folder changes with what is in it: not compared.
                        stamp: None,
                    }],
                    None => Vec::new(),
                };
                Ok((Op::Link, items))
            }
            L::Delete { paths } => {
                if paths.iter().any(|p| p.to_local_path().is_none()) {
                    return Err(crate::state::PERMANENT.into());
                }
                if !keel_vfs::trashbin::KNOWS_ORIGIN {
                    return Err("library deletes on macOS (restore them from the Trash)".into());
                }
                let deleted: Vec<_> = paths.iter().map(|p| (p.clone(), None)).collect();
                Ok((Op::Trash, trashed(&deleted, self.since, stat)?))
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
        .map_err(|r| anyhow::anyhow!("Can't undo: {}", r.why))?;
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
                move_back(it, router, cancel)?;
                p.done_items += 1;
            }
        }
        Op::Arrange { made, removed, .. } => {
            // Shallowest first: a folder before the ones inside it.
            let mut again: Vec<&VPath> = removed.iter().collect();
            again.sort_by_key(|d| d.path.matches('/').count());
            for d in again {
                let local = d
                    .to_local_path()
                    .context("folder operations undo locally")?;
                std::fs::create_dir_all(&local).with_context(|| d.display())?;
            }
            for it in &e.items {
                step(&mut p, &it.now)?;
                move_back(it, router, cancel)?;
                p.done_items += 1;
            }
            for d in made {
                // Only when empty: whatever came in since stays.
                if let Some(local) = d.to_local_path() {
                    let _ = std::fs::remove_dir(local);
                }
            }
        }
        Op::Attributes(before) => {
            for (path, attrs) in before {
                step(&mut p, path)?;
                let local = path.to_local_path().context("attributes are local")?;
                keel_vfs::fsattr::set_attributes(&local, attrs)?;
                p.done_items += 1;
            }
        }
        Op::Link => {
            for it in &e.items {
                step(&mut p, &it.now)?;
                let local = it.now.to_local_path().context("links are local")?;
                // A symbolic link or junction holds nothing: it goes, never what it points
                // to. A hard link is the file under a second name: to the trash.
                if std::fs::symlink_metadata(&local)?.file_type().is_symlink() {
                    keel_vfs::local::delete_permanently(&it.now)?;
                } else {
                    keel_vfs::local::trash(&it.now)?;
                }
                p.done_items += 1;
            }
        }
        Op::Hardlink(before) => {
            for (path, mtime) in before {
                step(&mut p, path)?;
                let local = path.to_local_path().context("hard links are local")?;
                crate::hardlink_dedup::unlink(&local, *mtime)?;
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

/// Moves a moved item back where it was (never over anything that took its place).
fn move_back(it: &Item, router: &Router, cancel: &AtomicBool) -> anyhow::Result<()> {
    let was = it.was.as_ref().context("a move records where items were")?;
    let dir = was.parent().context("moved from a folder")?;
    if it.now.name() == was.name() {
        // A no-replace rename first: on one volume it is atomic and never lands
        // in (or merges into) something that appeared at the old place since.
        let back = provider(router, was)?;
        if back.rename_noreplace(&it.now, was).is_err() {
            anyhow::ensure!(
                stat_fn(router)(was).is_none(),
                "Can't undo: {} is taken",
                was.display()
            );
            // Across volumes. Skip never overwrites a file, but would merge into
            // a folder made at the old place since the check just above.
            // ponytail: check-then-transfer window; a staged name and a
            // no-replace rename if it ever matters.
            keel_vfs::ops::transfer(
                std::slice::from_ref(&it.now),
                &dir,
                true,
                Conflict::Skip,
                &|_| {},
                cancel,
                router,
            )?;
        }
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
    Ok(())
}

#[cfg(test)]
#[path = "undo_tests.rs"]
mod tests;
