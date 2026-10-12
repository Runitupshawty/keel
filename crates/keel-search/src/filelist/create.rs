//! Making a file list from a folder (Everything's Tools → Create file list, and
//! `keel filelist create`).

use std::path::Path;

use anyhow::{bail, Context};
use regex::{Regex, RegexBuilder};

use super::time::filetime_from_system;
use super::{Entry, FileList, Limits, ATTR_DIRECTORY};
#[cfg(not(windows))]
use super::{ATTR_HIDDEN, ATTR_READONLY};

/// Folders deeper than this below the root are not walked.
const MAX_DEPTH: usize = 512;

/// What [`create`] lists. Wildcards (`*`, `?`) match a name without regard to case; one
/// with a `/` or `\` matches the path below the root instead (`src/*`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateOptions {
    /// Only files matching one of these (none: every file).
    pub include_files: Vec<String>,
    /// No file matching one of these.
    pub exclude_files: Vec<String>,
    /// Only what is in (or is) a folder matching one of these (none: everything). Other
    /// folders are still walked to find matching ones below them.
    pub include_folders: Vec<String>,
    /// Folders matching one of these are left out with everything in them.
    pub exclude_folders: Vec<String>,
    /// Only the root's own files and folders.
    pub no_subfolders: bool,
    /// List no folders (their files still are).
    pub no_folders: bool,
    /// List no files.
    pub no_files: bool,
}

/// `*.mp3;*.flac` (Everything's form; commas work too) as separate wildcards.
pub fn split_wildcards(text: &str) -> Vec<String> {
    text.split([';', ','])
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

struct Wildcards {
    names: Vec<Regex>,
    paths: Vec<Regex>,
}

impl Wildcards {
    fn new(patterns: &[String]) -> anyhow::Result<Self> {
        let mut w = Self {
            names: Vec::new(),
            paths: Vec::new(),
        };
        for p in patterns {
            let norm = p.replace('\\', "/");
            let re = RegexBuilder::new(&crate::walkindex::glob(norm.trim_start_matches('/')))
                .case_insensitive(true)
                .build()
                .with_context(|| format!("wildcard {p:?}"))?;
            match norm.contains('/') {
                true => w.paths.push(re),
                false => w.names.push(re),
            }
        }
        Ok(w)
    }

    fn is_empty(&self) -> bool {
        self.names.is_empty() && self.paths.is_empty()
    }

    /// `rel`: the path below the root with `/` separators.
    fn matches(&self, name: &str, rel: &str) -> bool {
        self.names.iter().any(|re| re.is_match(name))
            || self.paths.iter().any(|re| re.is_match(rel))
    }
}

/// Walks `root` (links are listed, never followed) into a file list: entries in path order,
/// with size, dates and attributes. Folders it cannot read are skipped. More entries than
/// `limits.max_rows` is an error.
pub fn create(root: &Path, opts: &CreateOptions, limits: &Limits) -> anyhow::Result<FileList> {
    let include_files = Wildcards::new(&opts.include_files)?;
    let exclude_files = Wildcards::new(&opts.exclude_files)?;
    let include_folders = Wildcards::new(&opts.include_folders)?;
    let exclude_folders = Wildcards::new(&opts.exclude_folders)?;
    let meta = std::fs::metadata(root).with_context(|| format!("{}", root.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a folder", root.display());
    }
    let mut list = FileList::default();
    let first = level(root, String::new(), 0, include_folders.is_empty())
        .with_context(|| format!("{}", root.display()))?;
    // The folders being walked, outermost first: a folder's entries come right after it.
    let mut stack = vec![first];
    while let Some(top) = stack.last_mut() {
        let Some(child) = top.children.next() else {
            stack.pop();
            continue;
        };
        let (depth, in_scope) = (top.depth, top.in_scope);
        let name = child.file_name().to_string_lossy().into_owned();
        let rel = match top.rel.as_str() {
            "" => name.clone(),
            rel => format!("{rel}/{name}"),
        };
        let Ok(md) = child.metadata() else { continue };
        let link = md.file_type().is_symlink();
        // A link counts as what it points at, but is never walked into.
        let target = if link {
            std::fs::metadata(child.path()).ok()
        } else {
            None
        };
        let md = target.as_ref().unwrap_or(&md);
        if md.is_dir() {
            if exclude_folders.matches(&name, &rel) {
                continue;
            }
            let scope = in_scope || include_folders.matches(&name, &rel);
            if scope && !opts.no_folders {
                push(&mut list, entry(&child.path(), &name, md, true), limits)?;
            }
            if !link && !opts.no_subfolders && depth + 1 < MAX_DEPTH {
                // An unreadable folder is skipped.
                if let Ok(next) = level(&child.path(), rel, depth + 1, scope) {
                    stack.push(next);
                }
            }
        } else {
            let wanted = in_scope
                && !opts.no_files
                && (include_files.is_empty() || include_files.matches(&name, &rel))
                && !exclude_files.matches(&name, &rel);
            if wanted {
                push(&mut list, entry(&child.path(), &name, md, false), limits)?;
            }
        }
    }
    Ok(list)
}

/// A folder being walked: its remaining children in name order.
struct Level {
    children: std::vec::IntoIter<std::fs::DirEntry>,
    /// The folder's path below the root, `/`-separated.
    rel: String,
    depth: usize,
    /// Inside a folder the include-folder wildcards matched (or there are none).
    in_scope: bool,
}

fn level(dir: &Path, rel: String, depth: usize, in_scope: bool) -> std::io::Result<Level> {
    let mut children: Vec<_> = std::fs::read_dir(dir)?.flatten().collect();
    children.sort_by_key(|c| c.file_name());
    Ok(Level {
        children: children.into_iter(),
        rel,
        depth,
        in_scope,
    })
}

fn push(list: &mut FileList, entry: Entry, limits: &Limits) -> anyhow::Result<()> {
    if list.entries.len() >= limits.max_rows {
        bail!("more than {} entries", limits.max_rows);
    }
    list.entries.push(entry);
    Ok(())
}

fn entry(path: &Path, name: &str, md: &std::fs::Metadata, is_dir: bool) -> Entry {
    #[cfg(windows)]
    let attributes = {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes()
    };
    #[cfg(not(windows))]
    let attributes = {
        let mut a = 0;
        if md.permissions().readonly() {
            a |= ATTR_READONLY;
        }
        if name.starts_with('.') {
            a |= ATTR_HIDDEN;
        }
        a
    };
    #[cfg(windows)]
    let _ = name;
    Entry {
        path: path.to_string_lossy().into_owned(),
        is_dir,
        size: (!is_dir).then_some(md.len()),
        modified: md.modified().ok().and_then(filetime_from_system),
        created: md.created().ok().and_then(filetime_from_system),
        attributes: Some(if is_dir {
            attributes | ATTR_DIRECTORY
        } else {
            attributes & !ATTR_DIRECTORY
        }),
        extra: Vec::new(),
    }
}
