//! A synthetic index (structure of arrays with parent links and folder aggregates, like
//! Keel's own) answering tree and set functions. The small run checks the answers against
//! a direct count; the 1,000,000-entry run is the Phase 15 timing target and is ignored by
//! default (it is meant for an optimized build):
//!
//! `cargo test -p keel-query --release --test scale -- --ignored`

use std::collections::HashMap;
use std::time::{Duration, Instant};

use keel_query::{parse, ChildCounts, DateProp, NodeId, Options, Props, Tree};

/// Folders `C:\dNNNNN`, each with files `fNNNNN.bin`; every tenth folder has more than
/// 1,000 files.
struct Index {
    paths: Vec<String>,
    name_at: Vec<usize>,
    parent: Vec<Option<u32>>,
    dir: Vec<bool>,
    size: Vec<u64>,
    /// Children of folder `i`: `first_child[i]..first_child[i] + counts`.
    first_child: Vec<u32>,
    files: Vec<u64>,
    folders: Vec<u64>,
    below: Vec<ChildCounts>,
}

impl Index {
    fn build(entries: usize) -> Self {
        let mut ix = Index {
            paths: vec!["C:\\".to_string()],
            name_at: vec![0],
            parent: vec![None],
            dir: vec![true],
            size: vec![0],
            first_child: vec![1],
            files: vec![0],
            folders: vec![0],
            below: vec![ChildCounts::default()],
        };
        let mut folder = 0u64;
        while ix.paths.len() < entries {
            let files = if folder.is_multiple_of(10) {
                1_200
            } else {
                950
            };
            let files = files.min(entries - ix.paths.len() - 1);
            let d = ix.paths.len();
            let path = format!("C:\\d{folder:05}");
            ix.name_at.push(3);
            ix.paths.push(path);
            ix.parent.push(Some(0));
            ix.dir.push(true);
            ix.size.push(0);
            ix.first_child.push(d as u32 + 1);
            ix.files.push(files as u64);
            ix.folders.push(0);
            ix.below.push(ChildCounts {
                files: files as u64,
                folders: 0,
            });
            for f in 0..files {
                let path = format!("C:\\d{folder:05}\\f{f:05}.bin");
                ix.name_at.push(10);
                ix.paths.push(path);
                ix.parent.push(Some(d as u32));
                ix.dir.push(false);
                // About 300,000 distinct sizes: most files have duplicates.
                ix.size
                    .push((folder * 7_919 + f as u64 * 104_729) % 300_007 * 4_096);
                ix.first_child.push(0);
                ix.files.push(0);
                ix.folders.push(0);
                ix.below.push(ChildCounts::default());
            }
            ix.folders[0] += 1;
            ix.below[0].folders += 1;
            ix.below[0].files += files as u64;
            folder += 1;
        }
        ix
    }

    fn row(&self, i: usize) -> Row<'_> {
        Row { ix: self, i }
    }

    fn len(&self) -> usize {
        self.paths.len()
    }
}

struct Row<'a> {
    ix: &'a Index,
    i: usize,
}

impl Props for Row<'_> {
    fn name(&self) -> &str {
        let p = &self.ix.paths[self.i];
        if self.i == 0 {
            "C:"
        } else {
            &p[self.ix.name_at[self.i]..]
        }
    }
    fn path(&self) -> &str {
        &self.ix.paths[self.i]
    }
    fn is_dir(&self) -> bool {
        self.ix.dir[self.i]
    }
    fn size(&self) -> Option<u64> {
        (!self.ix.dir[self.i]).then_some(self.ix.size[self.i])
    }
    fn allocated(&self) -> Option<u64> {
        self.size()
    }
    fn date(&self, _: DateProp) -> Option<i64> {
        None
    }
    fn attributes(&self) -> Option<u32> {
        Some(if self.is_dir() { 0x10 } else { 0x20 })
    }
    fn depth(&self) -> u32 {
        let mut d = 0;
        let mut cur = self.ix.parent[self.i];
        while let Some(p) = cur {
            d += 1;
            cur = self.ix.parent[p as usize];
        }
        d
    }
    fn child_counts(&self) -> Option<ChildCounts> {
        self.is_dir().then_some(ChildCounts {
            files: self.ix.files[self.i],
            folders: self.ix.folders[self.i],
        })
    }
    fn file_id(&self) -> Option<u64> {
        Some(self.i as u64)
    }
}

impl Tree for Index {
    fn node_of(&self, item: &dyn Props) -> Option<NodeId> {
        item.file_id()
    }
    fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.parent[node as usize].map(NodeId::from)
    }
    fn children(&self, node: NodeId, f: &mut dyn FnMut(NodeId) -> bool) {
        let i = node as usize;
        if node == 0 {
            // The root's children are the folders, spread through the arrays.
            for (j, p) in self.parent.iter().enumerate() {
                if *p == Some(0) && !f(j as NodeId) {
                    return;
                }
            }
            return;
        }
        let first = self.first_child[i] as usize;
        let n = (self.files[i] + self.folders[i]) as usize;
        for c in first..first + n {
            if !f(c as NodeId) {
                return;
            }
        }
    }
    fn with_props(&self, node: NodeId, f: &mut dyn FnMut(&dyn Props)) {
        f(&self.row(node as usize));
    }
    fn descendant_counts(&self, node: NodeId) -> Option<ChildCounts> {
        Some(self.below[node as usize])
    }
}

/// Runs `query` over the whole index: the positions of the result.
fn run(ix: &Index, query: &str) -> Vec<u64> {
    let q = parse(query, &Options::default()).unwrap();
    let mut pass = q.set_pass();
    let mut out = Vec::new();
    for i in 0..ix.len() {
        let row = ix.row(i);
        if !q.matches_in(&row, Some(ix)) {
            continue;
        }
        match &mut pass {
            Some(p) => {
                if !p.push(i as u64, &row, Some(ix)).unwrap() {
                    break;
                }
            }
            None => out.push(i as u64),
        }
    }
    match pass {
        Some(p) => p.finish(),
        None => out,
    }
}

fn expected_dupes(ix: &Index) -> usize {
    let mut by_size: HashMap<u64, usize> = HashMap::new();
    for i in 0..ix.len() {
        if !ix.dir[i] {
            *by_size.entry(ix.size[i]).or_default() += 1;
        }
    }
    by_size.values().filter(|&&n| n > 1).sum()
}

fn check(ix: &Index) {
    let big = (0..ix.len())
        .filter(|&i| ix.dir[i] && ix.files[i] + ix.folders[i] > 1_000)
        .count();
    assert!(big > 0);
    assert_eq!(run(ix, "child-count:>1000").len(), big);
    let deep = (0..ix.len())
        .filter(|&i| ix.dir[i] && ix.below[i].files + ix.below[i].folders > 1_000)
        .count();
    assert_eq!(run(ix, "descendant-count:>1000").len(), deep);
    let dupes = run(ix, "dupe:size");
    assert_eq!(dupes.len(), expected_dupes(ix));
    // Groups are adjacent and in size order.
    let sizes: Vec<u64> = dupes.iter().map(|&i| ix.size[i as usize]).collect();
    assert!(sizes.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(run(ix, "count:10").len(), 10);
    let with_eight = (1..ix.len())
        .filter(|&i| ix.dir[i] && ix.files[i] > 7)
        .count();
    assert_eq!(run(ix, "child:f00007.bin").len(), with_eight);
}

#[test]
fn small_synthetic_index() {
    check(&Index::build(20_000));
}

#[test]
#[ignore = "timing target: run with --release -- --ignored"]
fn million_entries() {
    let ix = Index::build(1_000_000);
    assert_eq!(ix.len(), 1_000_000);
    let timed = |query: &str, limit: Duration| {
        let start = Instant::now();
        let n = run(&ix, query).len();
        let took = start.elapsed();
        println!("{query}: {n} results in {took:?}");
        assert!(took < limit, "{query} took {took:?}, over {limit:?}");
    };
    timed("dupe:size", Duration::from_secs(2));
    timed("child-count:>1000", Duration::from_secs(1));
    timed("descendant-count:>1000", Duration::from_secs(1));
    check(&ix);
}
