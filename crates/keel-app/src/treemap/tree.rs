//! The weighted tree every chart draws: nodes with an id, a parent, a weight, a color key and
//! a label. Built by an adapter (the Sizes view's folder totals, a disk usage scan) and then
//! frozen with [`Tree::finish`], which totals the weights bottom up and orders children
//! biggest first.

use std::collections::HashMap;

/// The caller's id of a node (a path hash, a scan's node number); clicks return it.
pub type NodeId = u64;

/// Color keys starting with this character are pseudo items (folders whose content is not
/// known, the files of a folder, free and unknown space): drawn in a neutral color, never
/// ranked in the extension palette.
pub const SPECIAL: char = '/';

/// Any tree a scan keeps can be shown by implementing this: nodes by index, each with the
/// index of its parent (None for the root or roots).
// The Disk Usage window's scan (Task 81) converts through this; the Sizes view builds a
// `Tree` directly.
#[cfg_attr(not(test), allow(dead_code))]
pub trait WeightedTree {
    fn len(&self) -> usize;
    fn id(&self, i: usize) -> NodeId;
    fn parent(&self, i: usize) -> Option<usize>;
    /// The node's own weight; a parent's total is at least the sum of its children.
    fn weight(&self, i: usize) -> u64;
    /// Extension or other key the color comes from (`""` for none).
    fn color_key(&self, i: usize) -> &str;
    fn label(&self, i: usize) -> &str;
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    pub parent: Option<u32>,
    /// Biggest first once finished (stable: equal weights keep the order they were added in).
    pub children: Vec<u32>,
    /// Given weight until [`Tree::finish`], then the total including the children.
    pub total: u64,
    /// Index into [`Tree::keys`].
    pub key: u32,
    pub label: String,
    /// Added by `finish` for the part of a parent's weight its children do not cover.
    pub synthetic: bool,
    pub depth: u16,
}

#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub keys: Vec<String>,
    key_index: HashMap<String, u32>,
    by_id: HashMap<NodeId, u32>,
}

impl Tree {
    /// A tree with only its root (index 0).
    pub fn new(root_id: NodeId, root_label: &str) -> Self {
        let mut t = Self::default();
        t.push(None, root_id, 0, "", root_label, false);
        t
    }

    fn key(&mut self, key: &str) -> u32 {
        if let Some(k) = self.key_index.get(key) {
            return *k;
        }
        let k = self.keys.len() as u32;
        self.keys.push(key.to_owned());
        self.key_index.insert(key.to_owned(), k);
        k
    }

    fn push(
        &mut self,
        parent: Option<u32>,
        id: NodeId,
        weight: u64,
        key: &str,
        label: &str,
        synthetic: bool,
    ) -> u32 {
        let i = self.nodes.len() as u32;
        let key = self.key(key);
        let depth = parent.map_or(0, |p| self.nodes[p as usize].depth.saturating_add(1));
        self.nodes.push(Node {
            id,
            parent,
            children: Vec::new(),
            total: weight,
            key,
            label: label.to_owned(),
            synthetic,
            depth,
        });
        if let Some(p) = parent {
            self.nodes[p as usize].children.push(i);
        }
        if !synthetic {
            self.by_id.entry(id).or_insert(i);
        }
        i
    }

    /// Adds a node under `parent` (an index this tree returned) and returns its index.
    pub fn add(&mut self, parent: u32, id: NodeId, weight: u64, key: &str, label: &str) -> u32 {
        self.push(Some(parent), id, weight, key, label, false)
    }

    /// Copies any [`WeightedTree`]; several roots get a common root labelled `root_label`.
    /// Parents may come after their children; a node whose parent chain loops is dropped.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn from_source(src: &impl WeightedTree, root_label: &str) -> Self {
        let n = src.len();
        let roots: Vec<usize> = (0..n).filter(|&i| src.parent(i).is_none()).collect();
        let mut kids: Vec<Vec<usize>> = vec![Vec::new(); n];
        for i in 0..n {
            if let Some(p) = src.parent(i).filter(|&p| p < n && p != i) {
                kids[p].push(i);
            }
        }
        let mut t;
        let mut stack: Vec<(usize, u32)> = Vec::new();
        if roots.len() == 1 {
            let r = roots[0];
            t = Self::new(src.id(r), src.label(r));
            t.nodes[0].total = src.weight(r);
            let key = t.key(src.color_key(r));
            t.nodes[0].key = key;
            stack.extend(kids[r].iter().rev().map(|&c| (c, 0)));
        } else {
            t = Self::new(u64::MAX, root_label);
            stack.extend(roots.iter().rev().map(|&c| (c, 0)));
        }
        let mut seen = vec![false; n];
        while let Some((i, parent)) = stack.pop() {
            if std::mem::replace(&mut seen[i], true) {
                continue;
            }
            let me = t.add(
                parent,
                src.id(i),
                src.weight(i),
                src.color_key(i),
                src.label(i),
            );
            stack.extend(kids[i].iter().rev().map(|&c| (c, me)));
        }
        t.finish();
        t
    }

    /// Totals the weights bottom up (a parent's total is the larger of its own weight and
    /// its children's sum; the difference becomes a synthetic child labelled "Other") and
    /// sorts children biggest first. Call once, after the last `add`.
    pub fn finish(&mut self) {
        // Children always have higher indices than their parents (`add` needs the parent).
        for i in (0..self.nodes.len()).rev() {
            if self.nodes[i].children.is_empty() {
                continue;
            }
            let sum: u64 = (self.nodes[i].children.iter())
                .map(|&c| self.nodes[c as usize].total)
                .fold(0u64, u64::saturating_add);
            let own = self.nodes[i].total;
            if own > sum && sum > 0 {
                let label = format!("Other in {}", self.nodes[i].label);
                let id = self.nodes[i].id;
                let c = self.push(Some(i as u32), id, own - sum, "/other", &label, true);
                debug_assert!(c as usize > i);
            }
            self.nodes[i].total = own.max(sum);
        }
        let totals: Vec<u64> = self.nodes.iter().map(|n| n.total).collect();
        for n in &mut self.nodes {
            n.children
                .sort_by(|a, b| totals[*b as usize].cmp(&totals[*a as usize]));
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node(&self, i: u32) -> &Node {
        &self.nodes[i as usize]
    }

    /// The node with the caller's `id` (not a synthetic one).
    pub fn find(&self, id: NodeId) -> Option<u32> {
        self.by_id.get(&id).copied()
    }

    pub fn key_of(&self, i: u32) -> &str {
        &self.keys[self.nodes[i as usize].key as usize]
    }

    pub fn is_leaf(&self, i: u32) -> bool {
        self.nodes[i as usize].children.is_empty()
    }

    /// Whether `a` is `b` or one of its ancestors.
    pub fn contains(&self, a: u32, b: u32) -> bool {
        let mut at = Some(b);
        while let Some(n) = at {
            if n == a {
                return true;
            }
            at = self.nodes[n as usize].parent;
        }
        false
    }

    /// The child of `ancestor` on the way down to `node` (None when `node` is not below it).
    pub fn child_toward(&self, ancestor: u32, node: u32) -> Option<u32> {
        let mut at = node;
        while let Some(p) = self.nodes[at as usize].parent {
            if p == ancestor {
                return Some(at);
            }
            at = p;
        }
        None
    }

    /// Labels from the root (left out) down to `node`, joined with " › ".
    pub fn path(&self, node: u32) -> String {
        let mut parts = Vec::new();
        let mut at = Some(node);
        while let Some(n) = at {
            let p = self.nodes[n as usize].parent;
            if p.is_some() || parts.is_empty() {
                parts.push(self.nodes[n as usize].label.as_str());
            }
            at = p;
        }
        parts.reverse();
        parts.join(" › ")
    }

    /// Ranks of the color keys by the bytes of the leaves below `root` carrying them,
    /// biggest first (special keys rank `u32::MAX`; keys absent below `root` follow).
    pub fn key_ranks(&self, root: u32) -> Vec<u32> {
        let mut bytes = vec![0u64; self.keys.len()];
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            let node = &self.nodes[n as usize];
            if node.children.is_empty() {
                bytes[node.key as usize] = bytes[node.key as usize].saturating_add(node.total);
            }
            stack.extend(node.children.iter().copied());
        }
        let mut order: Vec<usize> = (0..self.keys.len())
            .filter(|&k| !self.keys[k].starts_with(SPECIAL))
            .collect();
        order.sort_by(|a, b| {
            bytes[*b]
                .cmp(&bytes[*a])
                .then(self.keys[*a].cmp(&self.keys[*b]))
        });
        let mut ranks = vec![u32::MAX; self.keys.len()];
        for (rank, k) in order.into_iter().enumerate() {
            ranks[k] = rank as u32;
        }
        ranks
    }
}
