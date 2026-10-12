//! What tree functions ask of an index: [`Tree`]. An item's own properties come from
//! [`Props`]; `child:`, `descendant:`, `sibling:`, `exists:`, `parentsize:` and the
//! descendant counts look at other items through the parent links of the index the item came
//! from. Without a tree (or when the tree does not know the item) they are unknown.

use crate::props::{ChildCounts, Props};

/// An item of a [`Tree`]: an index row, an arena slot. Only the tree gives it meaning.
pub type NodeId = u64;

/// Parent links of an index, for the tree functions. Every method is a read; scans over
/// children and descendants are bounded by [`Options::tree_scan_limit`](crate::Options).
pub trait Tree {
    /// The node of `item`, an item of this tree being evaluated.
    fn node_of(&self, item: &dyn Props) -> Option<NodeId>;
    /// The parent folder of a node; `None` for a root.
    fn parent(&self, node: NodeId) -> Option<NodeId>;
    /// Calls `f` with each direct child of a folder until `f` returns `false`.
    fn children(&self, node: NodeId, f: &mut dyn FnMut(NodeId) -> bool);
    /// Calls `f` once with the node's properties.
    fn with_props(&self, node: NodeId, f: &mut dyn FnMut(&dyn Props));
    /// The node at a full path (`\` separators, as [`normalize`](crate::normalize) gives;
    /// any case), for `exists:` with an absolute path. `None`: not indexed.
    fn find(&self, _path: &str) -> Option<NodeId> {
        None
    }
    /// Files and folders anywhere below a folder, when the index keeps folder aggregates;
    /// `None` counts them with a bounded scan.
    fn descendant_counts(&self, _node: NodeId) -> Option<ChildCounts> {
        None
    }
}
