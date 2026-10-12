//! The **Largest files** tab: the biggest files of a scan.

use super::analysis::{counted_file, FileRow, SizeRange};
use super::{NodeId, Tree};
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// The `limit` biggest files whose size is in `range`, biggest first (then by path). One
/// pass keeping `limit` ids; paths are made only for the rows returned. A further name of
/// a hard-linked file counted elsewhere is left out.
pub fn largest(tree: &Tree, limit: usize, range: SizeRange) -> Vec<FileRow> {
    if limit == 0 {
        return Vec::new();
    }
    // A min-heap of (size, -id): the smallest kept is popped first; among equal sizes the
    // higher id goes first, so ties keep scan order.
    let mut heap: BinaryHeap<Reverse<(u64, Reverse<NodeId>)>> = BinaryHeap::new();
    for (id, n) in tree.nodes() {
        if !range.contains(n.size) || !counted_file(tree, id) {
            continue;
        }
        if heap.len() < limit {
            heap.push(Reverse((n.size, Reverse(id))));
        } else if let Some(Reverse(low)) = heap.peek() {
            if (n.size, Reverse(id)) > *low {
                heap.pop();
                heap.push(Reverse((n.size, Reverse(id))));
            }
        }
    }
    let mut rows: Vec<FileRow> = (heap.into_iter())
        .filter_map(|Reverse((_, Reverse(id)))| FileRow::from_tree(tree, id))
        .collect();
    rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    rows
}
