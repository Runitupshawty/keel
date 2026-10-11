//! Merge, flatten and organize: the moves a folder operation makes, generated from the disk
//! by the preview (`plan::preview`) and run by the plan's job one item at a time.

use crate::search::KindFilter;
use anyhow::{Context, Result};
use keel_vfs::{Kind, Provider, Router, VPath};
use serde::{Deserialize, Serialize};

/// A folder operation, as asked for (the preview generates its moves).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FolderOp {
    /// Everything in `folders` (but `into`, which may be one of them) moves into `into`;
    /// the folders left empty are removed.
    Merge { folders: Vec<VPath>, into: VPath },
    /// Every file in `folder`'s subfolders, at any depth, moves up into `folder`; the
    /// subfolders left empty are removed.
    Flatten { folder: VPath },
    /// `folder`'s files move into subfolders named by `by` (made when missing).
    Organize { folder: VPath, by: OrganizeBy },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrganizeBy {
    /// Images, Videos, Audio, Documents, Archives, Other (by extension).
    Type,
    /// `2024` (the modified date, local time).
    Year,
    /// `2024-03`.
    Month,
}

impl FolderOp {
    pub fn verb(&self) -> &'static str {
        match self {
            FolderOp::Merge { .. } => "merge",
            FolderOp::Flatten { .. } => "flatten",
            FolderOp::Organize { .. } => "organize",
        }
    }

    /// The folders it reads and changes.
    pub fn roots(&self) -> Vec<&VPath> {
        match self {
            FolderOp::Merge { folders, into } => folders.iter().chain([into]).collect(),
            FolderOp::Flatten { folder } | FolderOp::Organize { folder, .. } => vec![folder],
        }
    }
}

/// One generated move: `item` goes into the folder `into` (`files` files, `bytes` bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Planned {
    pub item: VPath,
    pub into: VPath,
    /// `item` is a folder (merge moves folders whole).
    pub dir: bool,
    pub files: u64,
    pub bytes: u64,
}

/// What the operation does on the disk as it is now: the moves (sorted by item, so the job
/// finds an item's folder by binary search) and the folders to remove when left empty
/// (deepest first). Links are never moved and never followed.
pub(crate) fn generate(router: &Router, what: &FolderOp) -> Result<(Vec<Planned>, Vec<VPath>)> {
    let provider = |p: &VPath| {
        router
            .provider_for(p)
            .with_context(|| format!("no provider for {}", p.display()))
    };
    let folder = |p: &VPath| -> Result<std::sync::Arc<dyn Provider>> {
        let pr = provider(p)?;
        let e = pr
            .stat(p)
            .with_context(|| format!("{} does not exist", p.display()))?;
        anyhow::ensure!(e.kind == Kind::Dir, "{} is not a folder", p.display());
        Ok(pr)
    };
    let (mut moves, mut prune) = (Vec::new(), Vec::new());
    // Links stay where they are, and so do the folders holding them.
    let mut links: Vec<VPath> = Vec::new();
    match what {
        FolderOp::Merge { folders, into } => {
            folder(into)?;
            let others: Vec<&VPath> = folders.iter().filter(|f| *f != into).collect();
            anyhow::ensure!(
                !others.is_empty(),
                "pick at least one folder to merge into another"
            );
            for f in &others {
                let pr = folder(f)?;
                for o in folders.iter().chain([into]).filter(|o| o != f) {
                    anyhow::ensure!(
                        crate::library::relative(f, o).is_none(),
                        "{} is inside {}: merge folders side by side",
                        o.display(),
                        f.display()
                    );
                }
                let (linked, entries): (Vec<_>, Vec<_>) =
                    pr.list(f)?.into_iter().partition(|e| e.is_link);
                links.extend(linked.into_iter().map(|e| e.path));
                for e in entries {
                    let (bytes, files) = match e.kind {
                        Kind::Dir => tree_size(&*pr, &e.path, 0),
                        _ => (e.size, 1),
                    };
                    moves.push(Planned {
                        dir: e.kind == Kind::Dir,
                        item: e.path,
                        into: into.clone(),
                        files,
                        bytes,
                    });
                }
                prune.push((*f).clone());
            }
        }
        FolderOp::Flatten { folder: root } => {
            let pr = folder(root)?;
            let mut dirs: Vec<VPath> = pr
                .list(root)?
                .into_iter()
                .filter(|e| e.kind == Kind::Dir && !e.is_link)
                .map(|e| e.path)
                .collect();
            while let Some(d) = dirs.pop() {
                for e in pr.list(&d)? {
                    if e.is_link {
                        links.push(e.path);
                        continue;
                    }
                    match e.kind {
                        Kind::Dir => dirs.push(e.path),
                        _ => moves.push(Planned {
                            item: e.path,
                            into: root.clone(),
                            dir: false,
                            files: 1,
                            bytes: e.size,
                        }),
                    }
                }
                prune.push(d);
            }
            // Deepest first: a folder goes after everything in it.
            prune.sort_by_key(|d| std::cmp::Reverse(d.path.matches('/').count()));
        }
        FolderOp::Organize { folder: root, by } => {
            let pr = folder(root)?;
            for e in pr.list(root)? {
                if e.is_link || e.kind != Kind::File {
                    continue;
                }
                let group = group_of(&e.ext, e.modified, *by);
                moves.push(Planned {
                    into: root.join(&group),
                    item: e.path,
                    dir: false,
                    files: 1,
                    bytes: e.size,
                });
            }
        }
    }
    prune.retain(|d| {
        !links
            .iter()
            .any(|l| crate::library::relative(d, l).is_some())
    });
    moves.sort_by(|a, b| key(&a.item).cmp(&key(&b.item)));
    Ok((moves, prune))
}

/// The order of generated moves (and the job's binary search over them).
pub(crate) fn key(p: &VPath) -> (&str, &str, &str) {
    (&p.scheme, &p.authority, &p.path)
}

/// The folder `item` moves into.
pub(crate) fn into_of<'a>(moves: &'a [(VPath, VPath)], item: &VPath) -> Option<&'a VPath> {
    moves
        .binary_search_by(|(i, _)| key(i).cmp(&key(item)))
        .ok()
        .map(|i| &moves[i].1)
}

/// The name `item` lands under when an earlier item of its plan takes its own.
pub(crate) fn renamed_to<'a>(renamed: &'a [(VPath, String)], item: &VPath) -> Option<&'a str> {
    renamed
        .binary_search_by(|(i, _)| key(i).cmp(&key(item)))
        .ok()
        .map(|i| renamed[i].1.as_str())
}

/// The subfolder a file goes into.
pub(crate) fn group_of(
    ext: &str,
    modified: Option<std::time::SystemTime>,
    by: OrganizeBy,
) -> String {
    use chrono::{DateTime, Datelike, Local};
    match by {
        OrganizeBy::Type => {
            let kinds = [
                (KindFilter::Image, "Images"),
                (KindFilter::Video, "Videos"),
                (KindFilter::Audio, "Audio"),
                (KindFilter::Document, "Documents"),
                (KindFilter::Archive, "Archives"),
            ];
            let ext = ext.to_ascii_lowercase();
            kinds
                .iter()
                .find(|(k, _)| k.extensions().contains(&ext.as_str()))
                .map_or("Other", |(_, name)| name)
                .to_owned()
        }
        OrganizeBy::Year | OrganizeBy::Month => match modified {
            None => "Undated".to_owned(),
            Some(t) => {
                let t: DateTime<Local> = t.into();
                match by {
                    OrganizeBy::Year => format!("{:04}", t.year()),
                    _ => format!("{:04}-{:02}", t.year(), t.month()),
                }
            }
        },
    }
}

/// Bytes and files under a folder, through its provider (links not followed).
pub(crate) fn tree_size(provider: &dyn Provider, dir: &VPath, depth: usize) -> (u64, u64) {
    let mut total = (0, 0);
    for e in provider.list(dir).unwrap_or_default() {
        let (bytes, files) = match e.kind {
            Kind::Dir if e.is_link => (0, 0),
            Kind::Dir if depth < 256 => tree_size(provider, &e.path, depth + 1),
            _ => (e.size, 1),
        };
        total = (total.0 + bytes, total.1 + files);
    }
    total
}

fn counted(n: usize, what: &str) -> String {
    match n {
        1 => format!("1 {what}"),
        n => format!("{n} {what}s"),
    }
}

/// A short description of `a` ("hidden on, read-only off, mode 755").
pub fn attributes_text(a: &keel_vfs::Attributes) -> String {
    let on = |b: bool| if b { "on" } else { "off" };
    let mut parts = Vec::new();
    if let Some(h) = a.hidden {
        parts.push(format!("hidden {}", on(h)));
    }
    if let Some(r) = a.readonly {
        parts.push(format!("read-only {}", on(r)));
    }
    if let Some(m) = a.mode {
        parts.push(format!("mode {m:03o}"));
    }
    parts.join(", ")
}

impl crate::Plan {
    /// The headline of a folder operation, attribute or link plan (None for the others,
    /// whose front ends word them their own way).
    pub fn headline(&self) -> Option<String> {
        use crate::{Action, Op};
        let n = |a: Action| self.changes.iter().filter(|c| c.action == a).count();
        let files: u64 = self.changes.iter().map(|c| c.files).sum();
        Some(match &self.op {
            Op::Arrange { what, moves, .. } => match what {
                FolderOp::Merge { folders, into } => format!(
                    "Merge {} into {}: move {} ({})",
                    counted(folders.iter().filter(|f| *f != into).count(), "folder"),
                    into.display(),
                    counted(n(Action::Move), "item"),
                    counted(files as usize, "file"),
                ),
                FolderOp::Flatten { folder } => format!(
                    "Flatten {}: move {} up, then remove the {} left empty",
                    folder.display(),
                    counted(n(Action::Move), "file"),
                    counted(n(Action::RemoveFolder), "subfolder"),
                ),
                FolderOp::Organize { folder, by } => {
                    let mut groups: Vec<&VPath> = moves.iter().map(|(_, d)| d).collect();
                    groups.sort_by(|a, b| key(a).cmp(&key(b)));
                    groups.dedup();
                    format!(
                        "Organize {} by {}: {} into {}",
                        folder.display(),
                        match by {
                            OrganizeBy::Type => "type",
                            OrganizeBy::Year => "year",
                            OrganizeBy::Month => "month",
                        },
                        counted(n(Action::Move), "file"),
                        counted(groups.len(), "folder"),
                    )
                }
            },
            Op::SetAttributes { paths, attrs } => format!(
                "Set the attributes of {}: {}",
                counted(paths.len(), "item"),
                attributes_text(attrs)
            ),
            Op::Link { target, link, kind } => format!(
                "Make {} {} to {}",
                match kind {
                    keel_vfs::LinkKind::Symlink => "a symbolic link",
                    keel_vfs::LinkKind::Junction => "a junction",
                    keel_vfs::LinkKind::Hardlink => "a hard link",
                },
                link.display(),
                target.display()
            ),
            _ => return None,
        })
    }
}

#[cfg(test)]
#[path = "arrange_tests.rs"]
mod integration;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_group_by_type_and_date() {
        use OrganizeBy::*;
        assert_eq!(group_of("JPG", None, Type), "Images");
        assert_eq!(group_of("pdf", None, Type), "Documents");
        assert_eq!(group_of("mkv", None, Type), "Videos");
        assert_eq!(group_of("flac", None, Type), "Audio");
        assert_eq!(group_of("7z", None, Type), "Archives");
        assert_eq!(group_of("", None, Type), "Other");
        assert_eq!(group_of("rs", None, Type), "Other");
        assert_eq!(group_of("txt", None, Year), "Undated");
        // Mid-June 2021 is 2021-06 in every time zone.
        let june = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_623_758_400);
        assert_eq!(group_of("txt", Some(june), Year), "2021");
        assert_eq!(group_of("txt", Some(june), Month), "2021-06");
    }

    #[test]
    fn an_items_folder_is_found_by_binary_search() {
        let v = |p: &str| VPath::local(p);
        let mut moves = vec![
            (v("/d/b.txt"), v("/d/B")),
            (v("/d/a.txt"), v("/d/A")),
            (v("/d/c.txt"), v("/d/C")),
        ];
        moves.sort_by(|a, b| key(&a.0).cmp(&key(&b.0)));
        assert_eq!(into_of(&moves, &v("/d/b.txt")), Some(&v("/d/B")));
        assert_eq!(into_of(&moves, &v("/d/c.txt")), Some(&v("/d/C")));
        assert_eq!(into_of(&moves, &v("/d/x.txt")), None);
    }
}
