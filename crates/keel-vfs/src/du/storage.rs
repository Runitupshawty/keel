//! The **Storage analytics** tab: the scan's files bucketed into user-defined storage tiers
//! by how long ago they last changed, with an estimated monthly cost per tier, per
//! extension and per top folder.

use super::analysis::counted_file;
use super::{NodeId, NodeKind, Tree, NONE, UNKNOWN_TIME};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const DAY_NS: i64 = 86_400 * 1_000_000_000;

/// A storage tier: files changed less than `younger_than_days` ago (the last tier: all
/// older ones), priced per decimal GB (1,000,000,000 bytes) per month in any currency.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tier {
    pub name: String,
    /// The tier's upper age bound, exclusive; None for the last tier only.
    pub younger_than_days: Option<u32>,
    pub cost_per_gb: f64,
}

impl Tier {
    /// Hot (under 30 days), warm (under a year), cold; no prices.
    pub fn defaults() -> Vec<Tier> {
        let tier = |name: &str, days, cost_per_gb| Tier {
            name: name.into(),
            younger_than_days: days,
            cost_per_gb,
        };
        vec![
            tier("Hot", Some(30), 0.0),
            tier("Warm", Some(365), 0.0),
            tier("Cold", None, 0.0),
        ]
    }

    /// Checks a tier list: at least one tier, increasing bounds, only the last unbounded,
    /// finite non-negative prices.
    pub fn check(tiers: &[Tier]) -> Result<(), String> {
        let Some(last) = tiers.last() else {
            return Err("Add at least one storage tier".into());
        };
        if last.younger_than_days.is_some() {
            return Err("The last tier must take all older files".into());
        }
        let mut below = 0;
        for t in tiers {
            if !(t.cost_per_gb.is_finite() && t.cost_per_gb >= 0.0) {
                return Err(format!(
                    "{}: the price must be a number of 0 or more",
                    t.name
                ));
            }
            if let Some(days) = t.younger_than_days {
                if days <= below {
                    return Err(format!(
                        "{}: the ages must increase from tier to tier",
                        t.name
                    ));
                }
                below = days;
            } else if !std::ptr::eq(t, last) {
                return Err(format!(
                    "{}: only the last tier can take all older files",
                    t.name
                ));
            }
        }
        Ok(())
    }
}

/// Files, bytes and the estimated monthly cost of a group of files.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Total {
    pub files: u64,
    pub bytes: u64,
    pub monthly_cost: f64,
    /// Bytes per tier (same order as the tiers); the undated bucket is not in it.
    pub tier_bytes: Vec<u64>,
}

impl Total {
    fn new(tiers: usize) -> Self {
        Total {
            tier_bytes: vec![0; tiers],
            ..Total::default()
        }
    }

    fn add(&mut self, bytes: u64, tier: Option<usize>, rate: f64) {
        self.files += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        self.monthly_cost += bytes as f64 / 1e9 * rate;
        if let Some(slot) = tier.and_then(|t| self.tier_bytes.get_mut(t)) {
            *slot = slot.saturating_add(bytes);
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Analytics {
    /// One per tier.
    pub tiers: Vec<Total>,
    /// Files whose modification time is not known: in no tier, not priced.
    pub undated: Total,
    pub total: Total,
    /// By extension ("" for none), biggest first.
    pub extensions: Vec<(String, Total)>,
    /// By top folder (a root's child; files right in a root are under the root's path),
    /// biggest first.
    pub folders: Vec<(String, Total)>,
}

/// Buckets the counted files of `tree` (hard links once) into `tiers` by their age at
/// `now_ns` (unix nanoseconds; a file changed in the future is age 0).
pub fn analyze(tree: &Tree, tiers: &[Tier], now_ns: i64) -> Result<Analytics, String> {
    Tier::check(tiers)?;
    let n = tiers.len();
    let mut out = Analytics {
        tiers: vec![Total::new(n); n],
        undated: Total::new(n),
        total: Total::new(n),
        extensions: Vec::new(),
        folders: Vec::new(),
    };
    // Each item's top folder (a root's child), parents first: the arena keeps parents
    // before children.
    let mut top = vec![NONE; tree.len()];
    let mut by_ext: HashMap<u32, Total> = HashMap::new();
    let mut by_top: HashMap<NodeId, Total> = HashMap::new();
    for (id, node) in tree.nodes() {
        let parent = tree.get(node.parent);
        top[id as usize] = match parent.map(|p| p.kind) {
            Some(NodeKind::Root) => id,
            Some(_) => top[node.parent as usize],
            None => NONE,
        };
        if !counted_file(tree, id) {
            continue;
        }
        let tier = (node.modified != UNKNOWN_TIME).then(|| {
            let days = now_ns.saturating_sub(node.modified).max(0) / DAY_NS;
            (tiers.iter())
                .position(|t| t.younger_than_days.is_none_or(|d| days < i64::from(d)))
                .unwrap_or(n - 1)
        });
        let rate = tier.map_or(0.0, |t| tiers[t].cost_per_gb);
        match tier {
            Some(t) => out.tiers[t].add(node.size, tier, rate),
            None => out.undated.add(node.size, None, 0.0),
        }
        out.total.add(node.size, tier, rate);
        (by_ext.entry(node.ext).or_insert_with(|| Total::new(n))).add(node.size, tier, rate);
        // A file right in a root is its own "top"; group those under the root.
        let key = match top[id as usize] {
            t if t == id || t == NONE => node.parent,
            t => t,
        };
        (by_top.entry(key).or_insert_with(|| Total::new(n))).add(node.size, tier, rate);
    }
    let biggest = |v: &mut Vec<(String, Total)>| {
        v.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then_with(|| a.0.cmp(&b.0)));
    };
    out.extensions = (by_ext.into_iter())
        .map(|(e, t)| (tree.exts().get(e as usize).map_or("", |e| e).to_owned(), t))
        .collect();
    biggest(&mut out.extensions);
    out.folders = (by_top.into_iter())
        .map(|(id, t)| {
            let name = match tree.get(id) {
                Some(n) if n.kind == NodeKind::Root => tree
                    .root_of(id)
                    .map_or_else(|| n.name.to_string(), |r| r.path.display().to_string()),
                _ => (tree.path(id)).map_or_else(String::new, |p| p.display().to_string()),
            };
            (name, t)
        })
        .collect();
    biggest(&mut out.folders);
    Ok(out)
}
