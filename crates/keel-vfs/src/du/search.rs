//! The **Search** tab: items of a scan by name, size, type, extension and owner.

use super::analysis::SizeRange;
use super::{NodeId, NodeKind, Tree};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ItemType {
    #[default]
    Any,
    File,
    /// Folders and scanned roots.
    Folder,
    /// Links that were not followed.
    Link,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// A substring of the name, or a regular expression when `regex` is on; "" matches all.
    pub name: String,
    pub regex: bool,
    pub case_sensitive: bool,
    /// A folder's size is its subtree's.
    pub size: SizeRange,
    pub kind: ItemType,
    /// Extensions without the dot (any case); empty: any. Only files have one.
    pub extensions: Vec<String>,
    /// A substring (any case) of the owner's name or SID / user id; "" matches all. Needs
    /// the owners (see [`super::permissions::Report::owners`]); items without one are
    /// counted in [`Results::owners_unknown`] and left out.
    pub owner: String,
}

impl Query {
    /// "jpg, .PNG mp4" → `["jpg", "png", "mp4"]`.
    pub fn parse_extensions(text: &str) -> Vec<String> {
        (text.split([',', ';', ' ']))
            .map(|e| e.trim().trim_start_matches('.').to_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Results {
    /// Matches in scan order.
    pub nodes: Vec<NodeId>,
    pub files: u64,
    pub folders: u64,
    pub links: u64,
    /// Bytes of the matching files (a matching folder adds nothing, so nothing counts
    /// twice; hard-linked names count once).
    pub bytes: u64,
    pub allocated: u64,
    /// Items left out because their owner is not known.
    pub owners_unknown: u64,
}

/// Runs `query` over `tree`. `owners` maps items to owner names (empty when the owners
/// were not loaded). Fails only on a regular expression that does not compile.
pub fn search(
    tree: &Tree,
    query: &Query,
    owners: &HashMap<NodeId, String>,
) -> anyhow::Result<Results> {
    let regex = if query.regex {
        Some(
            regex::RegexBuilder::new(&query.name)
                .case_insensitive(!query.case_sensitive)
                .size_limit(1 << 20)
                .build()?,
        )
    } else {
        None
    };
    let needle = if query.case_sensitive {
        query.name.clone()
    } else {
        query.name.to_lowercase()
    };
    let owner = query.owner.trim().to_lowercase();
    let exts: HashSet<&str> = query.extensions.iter().map(String::as_str).collect();
    let mut out = Results::default();
    let mut linked = HashSet::new();
    for (id, n) in tree.nodes() {
        let kind_ok = match query.kind {
            ItemType::Any => matches!(
                n.kind,
                NodeKind::File | NodeKind::Dir | NodeKind::Root | NodeKind::Link
            ),
            ItemType::File => n.kind == NodeKind::File,
            ItemType::Folder => matches!(n.kind, NodeKind::Dir | NodeKind::Root),
            ItemType::Link => n.kind == NodeKind::Link,
        };
        if !kind_ok || !query.size.contains(n.size) {
            continue;
        }
        if !exts.is_empty() && (n.kind != NodeKind::File || !exts.contains(tree.ext(id))) {
            continue;
        }
        let name_ok = match &regex {
            Some(r) => r.is_match(&n.name),
            None if needle.is_empty() => true,
            None if query.case_sensitive => n.name.contains(&needle),
            None => n.name.to_lowercase().contains(&needle),
        };
        if !name_ok {
            continue;
        }
        if !owner.is_empty() {
            match owners.get(&id) {
                Some(o) if o.to_lowercase().contains(&owner) => {}
                Some(_) => continue,
                None => {
                    out.owners_unknown += 1;
                    continue;
                }
            }
        }
        out.nodes.push(id);
        match n.kind {
            NodeKind::File => {
                out.files += 1;
                // The first matching name of a hard-linked file counts.
                let counted = !(tree.options.count_hard_links_once
                    && tree.link_id(id).is_some_and(|k| !linked.insert(k)));
                if counted {
                    out.bytes = out.bytes.saturating_add(n.size);
                    out.allocated = out.allocated.saturating_add(n.allocated);
                }
            }
            NodeKind::Link => out.links += 1,
            _ => out.folders += 1,
        }
    }
    Ok(out)
}
