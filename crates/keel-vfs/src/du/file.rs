//! Scan files: a scan saved as versioned JSON (gzip-compressed unless asked otherwise),
//! loaded again without walking. Loading checks every structural rule a walk guarantees
//! (parents before children, contiguous siblings, kinds in their places, indexes in
//! range) and sums the folder totals again rather than trusting them.
//!
//! ```json
//! {"format": "keel-du-scan", "version": 1, "started": 1760000000, "elapsed_ms": 1234,
//!  "complete": true, "unreadable": 0, "excluded": 0, "options": {...},
//!  "roots": [{"node": 1, "path": "C:\\", "total": 512110190592, "free": 1234}],
//!  "exts": ["", "txt"], "links": ["5a1f0000000000000000000000000c41"],
//!  "nodes": [[name, parent, kind, flags, attributes, ext, link, size, allocated, modified], ...]}
//! ```
//!
//! `kind`: 0 top, 1 root, 2 folder, 3 file, 4 link, 5 free, 6 unknown. `modified` is unix
//! nanoseconds (`i64::MIN` unknown). A folder's `size` and `allocated` are not used (0).

use super::{flag, Node, NodeId, NodeKind, RootInfo, ScanInfo, ScanOptions, Space, Tree, NONE};
use anyhow::{Context, Result};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The `format` of a scan file.
pub const FORMAT: &str = "keel-du-scan";
/// The newest scan file version this Keel writes and reads.
pub const VERSION: u32 = 1;
/// At most this many bytes of JSON are read (after decompression).
const MAX_JSON: u64 = 16 << 30;

#[derive(Serialize)]
struct DocOut<'a> {
    format: &'static str,
    version: u32,
    started: i64,
    elapsed_ms: u64,
    complete: bool,
    unreadable: u64,
    excluded: u64,
    options: &'a ScanOptions,
    roots: Vec<DocRoot>,
    exts: &'a [Box<str>],
    links: Vec<String>,
    nodes: NodesOut<'a>,
}

struct NodesOut<'a> {
    tree: &'a Tree,
    order: &'a [NodeId],
    remap: &'a [NodeId],
}

impl Serialize for NodesOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.order.len()))?;
        for &id in self.order {
            let n = self.tree.node(id);
            let parent = match n.parent {
                NONE => NONE,
                p => self.remap[p as usize],
            };
            let (size, allocated) = match n.is_container() {
                true => (0, 0),
                false => (n.size, n.allocated),
            };
            seq.serialize_element(&(
                &*n.name,
                parent,
                n.kind as u8,
                n.flags,
                n.attributes,
                n.ext,
                n.link,
                size,
                allocated,
                n.modified,
            ))?;
        }
        seq.end()
    }
}

#[derive(Serialize, Deserialize)]
struct DocRoot {
    node: NodeId,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    free: Option<u64>,
}

#[derive(Deserialize)]
struct Header {
    format: String,
    version: u32,
}

#[derive(Deserialize)]
struct DocIn {
    format: String,
    version: u32,
    #[serde(default)]
    started: i64,
    #[serde(default)]
    elapsed_ms: u64,
    #[serde(default)]
    complete: bool,
    #[serde(default)]
    unreadable: u64,
    #[serde(default)]
    excluded: u64,
    #[serde(default)]
    options: ScanOptions,
    roots: Vec<DocRoot>,
    exts: Vec<String>,
    #[serde(default)]
    links: Vec<String>,
    nodes: Vec<(String, u32, u8, u16, u32, u32, u32, u64, u64, i64)>,
}

/// Writes `tree` to `w` (gzip when `compress`). Dead items are left out.
pub fn save_to(tree: &Tree, w: impl Write, compress: bool) -> Result<()> {
    let (order, remap) = tree.live_order();
    let doc = DocOut {
        format: FORMAT,
        version: VERSION,
        started: tree.info.started,
        elapsed_ms: tree.info.elapsed.as_millis() as u64,
        complete: tree.info.complete,
        unreadable: tree.info.unreadable,
        excluded: tree.info.excluded,
        options: &tree.options,
        roots: (tree.roots.iter())
            .map(|r| DocRoot {
                node: remap[r.node as usize],
                path: r.path.to_string_lossy().into_owned(),
                total: r.space.map(|s| s.total),
                free: r.space.map(|s| s.free),
            })
            .collect(),
        exts: &tree.exts,
        links: tree.links.iter().map(|l| format!("{l:x}")).collect(),
        nodes: NodesOut {
            tree,
            order: &order,
            remap: &remap,
        },
    };
    if compress {
        let mut gz = flate2::write::GzEncoder::new(w, flate2::Compression::new(6));
        serde_json::to_writer(&mut gz, &doc)?;
        gz.finish()?.flush()?;
    } else {
        let mut w = w;
        serde_json::to_writer(&mut w, &doc)?;
        w.flush()?;
    }
    Ok(())
}

/// Saves `tree` to `path` (gzip-compressed unless `compress` is false), replacing it in
/// one step.
pub fn save(tree: &Tree, path: &Path, compress: bool) -> Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("cannot write in {}", dir.display()))?;
    save_to(tree, BufWriter::new(tmp.as_file()), compress)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Reads a scan saved by [`save`] (compressed or not).
pub fn load(path: &Path) -> Result<Tree> {
    let f = std::fs::File::open(path).with_context(|| path.display().to_string())?;
    load_from(f).or_else(|e| {
        // A newer Keel's file says so rather than failing on its layout.
        if let Ok(h) = read_header(path) {
            anyhow::ensure!(h.format == FORMAT, "{} is not a Keel scan file", path.display());
            anyhow::ensure!(
                h.version <= VERSION,
                "{} is a version {} scan file, newer than this Keel reads ({VERSION})",
                path.display(),
                h.version
            );
        }
        Err(e).with_context(|| path.display().to_string())
    })
}

fn read_header(path: &Path) -> Result<Header> {
    let f = std::fs::File::open(path)?;
    Ok(serde_json::from_reader(decoder(f)?)?)
}

/// gzip or plain JSON, by the first bytes; at most [`MAX_JSON`] bytes of JSON.
fn decoder<'r>(r: impl Read + 'r) -> Result<Box<dyn Read + 'r>> {
    let mut r = BufReader::new(r);
    let gz = r.fill_buf()?.starts_with(&[0x1f, 0x8b]);
    Ok(match gz {
        true => Box::new(BufReader::new(flate2::read::MultiGzDecoder::new(r)).take(MAX_JSON)),
        false => Box::new(r.take(MAX_JSON)),
    })
}

/// Reads a scan from `r` (compressed or not).
pub fn load_from(r: impl Read) -> Result<Tree> {
    let doc: DocIn = serde_json::from_reader(decoder(r)?).context("not a Keel scan file")?;
    build(doc)
}

fn build(doc: DocIn) -> Result<Tree> {
    anyhow::ensure!(doc.format == FORMAT, "not a Keel scan file");
    anyhow::ensure!(
        doc.version <= VERSION,
        "a version {} scan file, newer than this Keel reads ({VERSION})",
        doc.version
    );
    anyhow::ensure!(doc.version >= 1, "scan file version 0");
    anyhow::ensure!(
        doc.exts.first().is_some_and(|e| e.is_empty()),
        "the first extension must be empty"
    );
    let links = (doc.links.iter())
        .map(|l| u128::from_str_radix(l, 16).with_context(|| format!("hard link id {l:?}")))
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(!doc.nodes.is_empty(), "the scan has no items");
    anyhow::ensure!(doc.nodes.len() < NONE as usize, "too many items");
    let mut nodes: Vec<Node> = Vec::with_capacity(doc.nodes.len());
    for (i, (name, parent, kind, flags, attributes, ext, link, size, allocated, modified)) in
        doc.nodes.into_iter().enumerate()
    {
        let kind = NodeKind::from_u8(kind).with_context(|| format!("item {i}: kind {kind}"))?;
        anyhow::ensure!(kind != NodeKind::Dead, "item {i}: removed item");
        anyhow::ensure!((ext as usize) < doc.exts.len(), "item {i}: extension {ext}");
        anyhow::ensure!((link as usize) <= links.len(), "item {i}: hard link {link}");
        let mut n = Node::new(kind, name);
        n.flags = flags & flag::ALL;
        n.attributes = attributes;
        n.ext = ext;
        n.link = link;
        n.modified = modified;
        if !n.is_container() {
            n.size = size;
            n.allocated = allocated;
        }
        if i == 0 {
            anyhow::ensure!(
                kind == NodeKind::Top && parent == NONE,
                "the first item must be the top"
            );
        } else {
            anyhow::ensure!(kind != NodeKind::Top, "item {i}: a second top");
            anyhow::ensure!((parent as usize) < i, "item {i}: parent {parent} after it");
            let p = &mut nodes[parent as usize];
            anyhow::ensure!(p.is_container(), "item {i}: parent {parent} is not a folder");
            anyhow::ensure!(
                (kind == NodeKind::Root) == (p.kind == NodeKind::Top),
                "item {i}: roots and only roots are below the top"
            );
            anyhow::ensure!(
                !n.is_pseudo() || p.kind == NodeKind::Root,
                "item {i}: free and unknown space belong to a root"
            );
            if p.child_count == 0 {
                p.first_child = i as NodeId;
            } else {
                anyhow::ensure!(
                    p.first_child as usize + p.child_count as usize == i,
                    "item {i}: not next to its siblings"
                );
            }
            p.child_count += 1;
            n.parent = parent;
        }
        nodes.push(n);
    }
    let mut roots = Vec::with_capacity(doc.roots.len());
    for r in doc.roots {
        let ok = nodes
            .get(r.node as usize)
            .is_some_and(|n| n.kind == NodeKind::Root);
        anyhow::ensure!(ok, "root {} is not a root item", r.node);
        anyhow::ensure!(
            !roots.iter().any(|x: &RootInfo| x.node == r.node),
            "root {} twice",
            r.node
        );
        roots.push(RootInfo {
            node: r.node,
            path: PathBuf::from(r.path),
            space: match (r.total, r.free) {
                (Some(total), Some(free)) => Some(Space { total, free }),
                _ => None,
            },
        });
    }
    let root_items = nodes.iter().filter(|n| n.kind == NodeKind::Root).count();
    anyhow::ensure!(root_items == roots.len(), "a root item without its path");
    let mut tree = Tree {
        nodes,
        exts: doc.exts.into_iter().map(String::into_boxed_str).collect(),
        links,
        roots,
        options: doc.options,
        info: ScanInfo {
            started: doc.started,
            elapsed: Duration::from_millis(doc.elapsed_ms),
            complete: doc.complete,
            unreadable: doc.unreadable,
            excluded: doc.excluded,
        },
    };
    tree.recompute();
    Ok(tree)
}
