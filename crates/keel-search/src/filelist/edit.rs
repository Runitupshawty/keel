//! The File List Editor's operations on a [`FileList`]: add files and folders (a folder
//! with everything below it), merge another list, remove duplicates, and **Update**, which
//! walks the listed folders again.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::bail;

use super::create::{create, entry_of};
use super::{CreateOptions, Entry, FileList, Limits};

/// Paths compare with `/` as `\`, without trailing separators and without regard to case.
fn key(path: &str) -> String {
    path.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Whether `path` (a [`key`]) is below the folder `dir` (a [`key`]).
fn below(path: &str, dir: &str) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'\\'
}

fn check(list: &FileList, limits: &Limits) -> anyhow::Result<()> {
    if list.entries.len() > limits.max_rows {
        bail!("more than {} entries", limits.max_rows);
    }
    Ok(())
}

/// Adds `other`'s entries after `list`'s. Property columns are joined by name (without
/// regard to case): a column only one list has is empty in the other's entries. More than
/// `limits.max_rows` entries in all is an error, and `list` is left as it was.
pub fn merge(list: &mut FileList, other: FileList, limits: &Limits) -> anyhow::Result<()> {
    if list.entries.len().saturating_add(other.entries.len()) > limits.max_rows {
        bail!("more than {} entries", limits.max_rows);
    }
    // Where each of `other`'s columns goes in `list`.
    let mut slots = Vec::with_capacity(other.columns.len());
    for c in &other.columns {
        let at = (list.columns.iter()).position(|have| have.eq_ignore_ascii_case(c));
        slots.push(at.unwrap_or_else(|| {
            list.columns.push(c.clone());
            list.columns.len() - 1
        }));
    }
    let width = list.columns.len();
    for e in &mut list.entries {
        e.extra.resize(width, String::new());
    }
    for mut e in other.entries {
        let mut extra = vec![String::new(); width];
        for (value, &slot) in e.extra.drain(..).zip(&slots) {
            extra[slot] = value;
        }
        e.extra = extra;
        list.entries.push(e);
    }
    Ok(())
}

/// Removes every entry whose path an earlier entry has (`/` and `\` alike, without regard to
/// case); returns how many went. The first one of each path stays, in place.
pub fn remove_duplicates(list: &mut FileList) -> usize {
    let mut seen = HashSet::with_capacity(list.entries.len());
    let before = list.entries.len();
    list.entries.retain(|e| seen.insert(key(&e.path)));
    before - list.entries.len()
}

/// Adds `paths` (dropped or picked): a file as itself, a folder with everything below it
/// (as [`create`] walks it). Paths it cannot read are skipped. Returns how many entries were
/// added; more than `limits.max_rows` in all is an error, and `list` is left as it was.
pub fn add_paths(list: &mut FileList, paths: &[PathBuf], limits: &Limits) -> anyhow::Result<usize> {
    let mut added = FileList::default();
    for path in paths {
        let Some(entry) = entry_of(path) else {
            continue;
        };
        let is_dir = entry.is_dir;
        added.entries.push(entry);
        if is_dir {
            let room = Limits {
                max_rows: limits.max_rows.saturating_sub(list.entries.len() + added.entries.len()),
                ..*limits
            };
            added.entries.extend(create(path, &CreateOptions::default(), &room)?.entries);
        }
        if list.entries.len() + added.entries.len() > limits.max_rows {
            bail!("more than {} entries", limits.max_rows);
        }
    }
    let n = added.entries.len();
    merge(list, added, limits)?;
    Ok(n)
}

/// The listed folders that no other listed folder holds: what [`update`] walks again.
pub fn roots(list: &FileList) -> Vec<String> {
    let mut dirs: Vec<(String, &str)> = (list.entries.iter())
        .filter(|e| e.is_dir)
        .map(|e| (key(&e.path), e.path.as_str()))
        .collect();
    // Shorter paths first: a folder's parents come before it.
    dirs.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));
    dirs.dedup_by(|a, b| a.0 == b.0);
    let mut out: Vec<(String, &str)> = Vec::new();
    for (k, path) in dirs {
        if !out.iter().any(|(r, _)| below(&k, r)) {
            out.push((k, path));
        }
    }
    out.into_iter().map(|(_, p)| p.to_owned()).collect()
}

/// What [`update`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Updated {
    /// Listed folders walked again.
    pub folders: usize,
    /// Listed folders and files that are not there now: kept as they were (a drive that is
    /// not connected must not empty the list).
    pub missing: usize,
}

/// **Update**: every listed folder no other listed folder holds ([`roots`]) is walked again,
/// so what is below it is what is there now (new files in, deleted ones out); every other
/// listed file is read again (size, dates, attributes). A folder or file that is not there
/// keeps its old entries. Property values stay with the paths that keep them. The order
/// follows the list's: each folder's walk takes the place of its first entry.
pub fn update(list: &FileList, limits: &Limits) -> anyhow::Result<(FileList, Updated)> {
    let mut stats = Updated::default();
    let roots: Vec<(String, String)> = (roots(list).into_iter())
        .map(|p| (key(&p), p))
        .collect();
    let extras: HashMap<String, &Vec<String>> = (list.entries.iter())
        .filter(|e| e.extra.iter().any(|v| !v.is_empty()))
        .map(|e| (key(&e.path), &e.extra))
        .collect();
    let width = list.columns.len();
    let mut out = FileList {
        columns: list.columns.clone(),
        entries: Vec::with_capacity(list.entries.len()),
    };
    // Per root: walked (Some(true)), missing (Some(false)), or not reached yet.
    let mut walked: HashMap<&str, bool> = HashMap::new();
    let fresh = |mut e: Entry| {
        e.extra = extras
            .get(&key(&e.path))
            .map_or_else(|| vec![String::new(); width], |v| (*v).clone());
        e
    };
    for e in &list.entries {
        let k = key(&e.path);
        let root = (roots.iter()).find(|(r, _)| *r == k || below(&k, r));
        match root {
            Some((rk, rpath)) => match walked.get(rk.as_str()).copied() {
                Some(true) => {}
                Some(false) => out.entries.push(e.clone()),
                None => {
                    let dir = Path::new(rpath);
                    let here = entry_of(dir).filter(|d| d.is_dir);
                    let ok = here.is_some();
                    walked.insert(rk.as_str(), ok);
                    match here {
                        Some(mut d) => {
                            stats.folders += 1;
                            // The folder's entry keeps the path as the list wrote it.
                            d.path.clone_from(rpath);
                            out.entries.push(fresh(d));
                            let room = Limits {
                                max_rows: limits.max_rows.saturating_sub(out.entries.len()),
                                ..*limits
                            };
                            let below_root = create(dir, &CreateOptions::default(), &room)?;
                            out.entries.extend(below_root.entries.into_iter().map(fresh));
                        }
                        None => {
                            stats.missing += 1;
                            out.entries.push(e.clone());
                        }
                    }
                }
            },
            None => match entry_of(Path::new(&e.path)) {
                Some(mut now) if now.is_dir == e.is_dir => {
                    now.path.clone_from(&e.path);
                    now.extra.clone_from(&e.extra);
                    now.extra.resize(width, String::new());
                    out.entries.push(now);
                }
                _ => {
                    stats.missing += 1;
                    out.entries.push(e.clone());
                }
            },
        }
        check(&out, limits)?;
    }
    Ok((out, stats))
}
