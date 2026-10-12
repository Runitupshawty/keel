//! Treemap layouts: squarified (Bruls, Huizing and van Wijk), the simple strip layout of
//! KDirStat and WinDirStat, and the Hilbert and Moore space-filling layouts (after Tak and
//! Cockburn), each nested folder by folder. The cushion surface of every tile (van Wijk and
//! van de Wetering) is accumulated here too, so shading only has to evaluate it per pixel.
//! Coordinates are `f64` pixels; hit-testing walks the tile tree.

use super::tree::Tree;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

/// No tile (end of a sibling chain, no children, no parent).
pub const NONE: u32 = u32::MAX;

/// A rectangle, `x0 <= x1`, `y0 <= y1`, y downward.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct R {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl R {
    pub const fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self { x0, y0, x1, y1 }
    }
    pub fn w(&self) -> f64 {
        self.x1 - self.x0
    }
    pub fn h(&self) -> f64 {
        self.y1 - self.y0
    }
    pub fn area(&self) -> f64 {
        self.w().max(0.0) * self.h().max(0.0)
    }
    /// Half-open: a point on a shared edge belongs to one tile only.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
    pub fn center(&self) -> (f64, f64) {
        ((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0)
    }
    pub fn shrink(&self, d: f64) -> R {
        R::new(self.x0 + d, self.y0 + d, self.x1 - d, self.y1 - d)
    }
    /// The area two rectangles share.
    #[cfg(test)]
    pub fn overlap(&self, o: &R) -> f64 {
        let w = self.x1.min(o.x1) - self.x0.max(o.x0);
        let h = self.y1.min(o.y1) - self.y0.max(o.y0);
        if w > 0.0 && h > 0.0 {
            w * h
        } else {
            0.0
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Algorithm {
    /// Tiles as close to squares as possible (the default).
    #[default]
    Squarified,
    /// KDirStat's and WinDirStat's rows, biggest first.
    Strip,
    /// Children follow a Hilbert curve, so neighbors in size order stay neighbors on screen.
    Hilbert,
    /// The Moore curve: a closed Hilbert loop.
    Moore,
}

impl Algorithm {
    pub const ALL: [Algorithm; 4] = [
        Algorithm::Squarified,
        Algorithm::Strip,
        Algorithm::Hilbert,
        Algorithm::Moore,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Squarified => "Squarified",
            Algorithm::Strip => "Strips (KDirStat)",
            Algorithm::Hilbert => "Hilbert",
            Algorithm::Moore => "Moore",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub algorithm: Algorithm,
    /// A folder narrower or lower than this many pixels is drawn as one tile.
    pub min_px: f64,
    /// Cushion height of the outermost level (0: flat surfaces).
    pub height: f64,
    /// Each level's cushion height relative to its parent's.
    pub scale: f64,
    /// Folder frame width; 0 draws no frames.
    pub frame_px: f64,
    /// Folders get a frame only when both sides are at least this many pixels.
    pub frame_min: f64,
    /// Levels below the root drawn at most.
    pub max_depth: u16,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Squarified,
            min_px: 1.0,
            height: 0.38,
            scale: 0.91,
            frame_px: 0.0,
            frame_min: 40.0,
            max_depth: 512,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Tile {
    /// Tree node index.
    pub node: u32,
    pub rect: R,
    /// Levels below the layout's root.
    pub depth: u16,
    pub parent: u32,
    pub first_child: u32,
    pub next_sibling: u32,
    /// Cushion surface coefficients: z = s0·x² + s1·y² + s2·x + s3·y (+ c).
    pub surface: [f64; 4],
    /// Its children sit inside a frame of `Params::frame_px`.
    pub framed: bool,
}

impl Tile {
    pub fn is_leaf(&self) -> bool {
        self.first_child == NONE
    }
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// Pre-order: a tile's children follow it (tile 0 is the root).
    pub tiles: Vec<Tile>,
    pub bounds: R,
    /// The tree node laid out at the top (the zoom).
    pub root: u32,
    /// The folder frame width it was laid out with.
    pub frame_px: f64,
    node_tile: Vec<u32>,
}

/// Arrow-key directions for [`Layout::neighbor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Lays out `tree` below `root` in `bounds`. None when `cancel` was set meanwhile.
pub fn layout(
    tree: &Tree,
    root: u32,
    bounds: R,
    p: &Params,
    cancel: Option<&AtomicBool>,
) -> Option<Layout> {
    let mut tiles: Vec<Tile> = Vec::new();
    let mut last_child: Vec<u32> = Vec::new();
    let mut node_tile = vec![NONE; tree.len()];
    // (node, rect, depth, parent tile, inherited surface, cushion height)
    let mut stack: Vec<(u32, R, u16, u32, [f64; 4], f64)> =
        vec![(root, bounds, 0, NONE, [0.0; 4], p.height)];
    let mut items: Vec<(u32, f64)> = Vec::new();
    let mut placed: Vec<(u32, R)> = Vec::new();
    while let Some((node, rect, depth, parent, surface, h)) = stack.pop() {
        if tiles.len() % 4096 == 4095 && cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return None;
        }
        let idx = tiles.len() as u32;
        let mut s = surface;
        if h > 0.0 {
            add_ridge(&rect, &mut s, h);
        }
        tiles.push(Tile {
            node,
            rect,
            depth,
            parent,
            first_child: NONE,
            next_sibling: NONE,
            surface: s,
            framed: false,
        });
        last_child.push(NONE);
        node_tile[node as usize] = idx;
        if parent != NONE {
            let prev = last_child[parent as usize];
            if prev == NONE {
                tiles[parent as usize].first_child = idx;
            } else {
                tiles[prev as usize].next_sibling = idx;
            }
            last_child[parent as usize] = idx;
        }
        let n = tree.node(node);
        if n.children.is_empty()
            || depth >= p.max_depth
            || rect.w() < p.min_px
            || rect.h() < p.min_px
        {
            continue;
        }
        let mut inner = rect;
        if p.frame_px > 0.0 && rect.w() >= p.frame_min && rect.h() >= p.frame_min {
            inner = rect.shrink(p.frame_px);
            tiles[idx as usize].framed = true;
        }
        if inner.w() <= 0.0 || inner.h() <= 0.0 {
            continue;
        }
        items.clear();
        items.extend(
            (n.children.iter())
                .map(|&c| (c, tree.node(c).total as f64))
                .filter(|(_, w)| *w > 0.0),
        );
        if items.is_empty() {
            tiles[idx as usize].framed = false;
            continue;
        }
        placed.clear();
        arrange(p.algorithm, &items, inner, &mut placed);
        for &(c, r) in placed.iter().rev() {
            stack.push((c, r, depth + 1, idx, s, h * p.scale));
        }
    }
    Some(Layout {
        tiles,
        bounds,
        root,
        frame_px: p.frame_px,
        node_tile,
    })
}

/// Places `items` (biggest first, positive weights) in `rect`, in item order.
pub fn arrange(alg: Algorithm, items: &[(u32, f64)], rect: R, out: &mut Vec<(u32, R)>) {
    match alg {
        Algorithm::Squarified => squarify(items, rect, out),
        Algorithm::Strip => strips(items, rect, out),
        Algorithm::Hilbert => curve(items, rect, Sym::I, false, out),
        Algorithm::Moore => curve(items, rect, Sym::I, true, out),
    }
}

/// van Wijk and van de Wetering: a parabolic ridge of height `h` over `r` in x and in y.
fn add_ridge(r: &R, s: &mut [f64; 4], h: f64) {
    let h4 = 4.0 * h;
    if r.w() > 0.0 {
        let wf = h4 / r.w();
        s[2] += wf * (r.x1 + r.x0);
        s[0] -= wf;
    }
    if r.h() > 0.0 {
        let hf = h4 / r.h();
        s[3] += hf * (r.y1 + r.y0);
        s[1] -= hf;
    }
}

fn sum(items: &[(u32, f64)]) -> f64 {
    items.iter().map(|i| i.1).sum()
}

/// The worst aspect ratio of a row of areas laid along a side of length `side`.
fn worst(max_area: f64, min_area: f64, row_area: f64, side: f64) -> f64 {
    let (s2, w2) = (row_area * row_area, side * side);
    (w2 * max_area / s2).max(s2 / (w2 * min_area))
}

fn squarify(items: &[(u32, f64)], rect: R, out: &mut Vec<(u32, R)>) {
    let mut remaining = sum(items);
    let mut rest = rect;
    let mut i = 0;
    while i < items.len() && remaining > 0.0 {
        let wide = rest.w() >= rest.h();
        let (long, short) = if wide {
            (rest.w(), rest.h())
        } else {
            (rest.h(), rest.w())
        };
        let per = rest.area() / remaining;
        let (mut j, mut row, mut best) = (i, 0.0, f64::INFINITY);
        while j < items.len() {
            let next = row + items[j].1;
            let ratio = worst(items[i].1 * per, items[j].1 * per, next * per, short);
            if j > i && ratio > best {
                break;
            }
            (best, row, j) = (ratio, next, j + 1);
        }
        let thick = if j == items.len() {
            long
        } else {
            row / remaining * long
        };
        let mut off = 0.0;
        for (k, it) in items[i..j].iter().enumerate() {
            let len = if i + k + 1 == j {
                short - off
            } else {
                it.1 / row * short
            };
            let r = if wide {
                R::new(rest.x0, rest.y0 + off, rest.x0 + thick, rest.y0 + off + len)
            } else {
                R::new(rest.x0 + off, rest.y0, rest.x0 + off + len, rest.y0 + thick)
            };
            out.push((it.0, r));
            off += len;
        }
        if wide {
            rest.x0 += thick;
        } else {
            rest.y0 += thick;
        }
        remaining -= row;
        i = j;
    }
}

/// KDirStat's row rule: a row takes children while its newest (smallest) child stays at
/// least this wide relative to the row's thickness.
const MIN_PROPORTION: f64 = 0.4;

fn strips(items: &[(u32, f64)], rect: R, out: &mut Vec<(u32, R)>) {
    let total = sum(items);
    // Rows run along the longer side and stack along the shorter one.
    let along_x = rect.w() >= rect.h();
    let (long, short) = if along_x {
        (rect.w(), rect.h())
    } else {
        (rect.h(), rect.w())
    };
    let (mut i, mut used) = (0, 0.0);
    while i < items.len() {
        let (mut j, mut row) = (i, 0.0);
        while j < items.len() {
            let next = row + items[j].1;
            let thick = next / total * short;
            let len = items[j].1 / next * long;
            if j > i && len / thick < MIN_PROPORTION {
                break;
            }
            (row, j) = (next, j + 1);
        }
        let thick = if j == items.len() {
            short - used
        } else {
            row / total * short
        };
        let mut off = 0.0;
        for (k, it) in items[i..j].iter().enumerate() {
            let len = if i + k + 1 == j {
                long - off
            } else {
                it.1 / row * long
            };
            let r = if along_x {
                R::new(
                    rect.x0 + off,
                    rect.y0 + used,
                    rect.x0 + off + len,
                    rect.y0 + used + thick,
                )
            } else {
                R::new(
                    rect.x0 + used,
                    rect.y0 + off,
                    rect.x0 + used + thick,
                    rect.y0 + off + len,
                )
            };
            out.push((it.0, r));
            off += len;
        }
        used += thick;
        i = j;
    }
}

/// One of the eight symmetries of the unit square, as a signed permutation matrix: local
/// curve coordinates (u right, v up; the canonical curve enters at (0,0) and leaves at
/// (1,0)) to the rectangle's normalized coordinates (x right, y up).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sym([[i8; 2]; 2]);

impl Sym {
    const I: Sym = Sym([[1, 0], [0, 1]]);
    /// (u, v) -> (v, u)
    const T: Sym = Sym([[0, 1], [1, 0]]);
    /// (u, v) -> (1 - v, 1 - u)
    const AT: Sym = Sym([[0, -1], [-1, 0]]);
    /// (u, v) -> (1 - v, u): enters at bottom right, leaves at top right.
    const UP: Sym = Sym([[0, -1], [1, 0]]);
    /// (u, v) -> (v, 1 - u): enters at top left, leaves at bottom left.
    const DOWN: Sym = Sym([[0, 1], [-1, 0]]);

    /// `self` after `inner`.
    fn then(self, inner: Sym) -> Sym {
        let (a, b) = (self.0, inner.0);
        let mut m = [[0i8; 2]; 2];
        for (r, row) in m.iter_mut().enumerate() {
            for (c, v) in row.iter_mut().enumerate() {
                *v = a[r][0] * b[0][c] + a[r][1] * b[1][c];
            }
        }
        Sym(m)
    }

    fn apply(self, u: f64, v: f64) -> (f64, f64) {
        let row = |r: [i8; 2]| {
            let off = if r[0] < 0 || r[1] < 0 { 1.0 } else { 0.0 };
            f64::from(r[0]) * u + f64::from(r[1]) * v + off
        };
        (row(self.0[0]), row(self.0[1]))
    }

    /// Local u runs along screen x.
    fn u_along_x(self) -> bool {
        self.0[0][0] != 0
    }

    /// The part of `rect` that local `[u0, u1] × [v0, v1]` maps to.
    fn part(self, rect: &R, u0: f64, v0: f64, u1: f64, v1: f64) -> R {
        let (xa, ya) = self.apply(u0, v0);
        let (xb, yb) = self.apply(u1, v1);
        let (w, h) = (rect.w(), rect.h());
        R::new(
            rect.x0 + xa.min(xb) * w,
            rect.y1 - ya.max(yb) * h,
            rect.x0 + xa.max(xb) * w,
            rect.y1 - ya.min(yb) * h,
        )
    }
}

/// The index in `range` whose prefix sum is closest to `target`.
fn cut(prefix: &[f64], lo: usize, hi: usize, target: f64) -> usize {
    (lo..=hi)
        .min_by(|&a, &b| {
            (prefix[a] - target)
                .abs()
                .total_cmp(&(prefix[b] - target).abs())
        })
        .unwrap_or(lo)
}

fn curve(items: &[(u32, f64)], rect: R, sym: Sym, moore: bool, out: &mut Vec<(u32, R)>) {
    let n = items.len();
    if n == 0 {
        return;
    }
    if n == 1 {
        out.push((items[0].0, rect));
        return;
    }
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0.0);
    for it in items {
        prefix.push(prefix.last().copied().unwrap_or(0.0) + it.1);
    }
    let total = prefix[n];
    let (lw, lh) = if sym.u_along_x() {
        (rect.w(), rect.h())
    } else {
        (rect.h(), rect.w())
    };
    let frac = |a: f64, b: f64| if b > 0.0 { a / b } else { 0.5 };
    if n >= 4 && lw < 2.0 * lh && lh < 2.0 * lw {
        // Four quadrants in curve order: bottom left, top left, top right, bottom right.
        let a = cut(&prefix, 1, n - 3, total / 4.0);
        let b = cut(&prefix, a + 1, n - 2, total / 2.0);
        let c = cut(&prefix, b + 1, n - 1, total * 3.0 / 4.0);
        let groups = [&items[..a], &items[a..b], &items[b..c], &items[c..]];
        let w = [
            prefix[a],
            prefix[b] - prefix[a],
            prefix[c] - prefix[b],
            total - prefix[c],
        ];
        let us = frac(w[0] + w[1], total);
        let vl = frac(w[0], w[0] + w[1]);
        let vr = frac(w[3], w[2] + w[3]);
        let parts = [
            (0.0, 0.0, us, vl),
            (0.0, vl, us, 1.0),
            (us, vr, 1.0, 1.0),
            (us, 0.0, 1.0, vr),
        ];
        let turns = if moore {
            [Sym::UP, Sym::UP, Sym::DOWN, Sym::DOWN]
        } else {
            [Sym::T, Sym::I, Sym::I, Sym::AT]
        };
        for ((group, (u0, v0, u1, v1)), turn) in groups.into_iter().zip(parts).zip(turns) {
            let r = sym.part(&rect, u0, v0, u1, v1);
            curve(group, r, sym.then(turn), false, out);
        }
        return;
    }
    let a = cut(&prefix, 1, n - 1, total / 2.0);
    let s = frac(prefix[a], total);
    if lh >= 2.0 * lw {
        // Tall: the first half below (climbing), the second above.
        let r0 = sym.part(&rect, 0.0, 0.0, 1.0, s);
        let r1 = sym.part(&rect, 0.0, s, 1.0, 1.0);
        curve(&items[..a], r0, sym.then(Sym::T), false, out);
        curve(&items[a..], r1, sym, false, out);
    } else {
        let r0 = sym.part(&rect, 0.0, 0.0, s, 1.0);
        let r1 = sym.part(&rect, s, 0.0, 1.0, 1.0);
        curve(&items[..a], r0, sym, false, out);
        curve(&items[a..], r1, sym, false, out);
    }
}

impl Layout {
    /// The deepest tile at (x, y).
    pub fn hit(&self, x: f64, y: f64) -> Option<u32> {
        let root = self.tiles.first()?;
        if !root.rect.contains(x, y) {
            return None;
        }
        let mut t = 0u32;
        'down: loop {
            let mut c = self.tiles[t as usize].first_child;
            while c != NONE {
                if self.tiles[c as usize].rect.contains(x, y) {
                    t = c;
                    continue 'down;
                }
                c = self.tiles[c as usize].next_sibling;
            }
            return Some(t);
        }
    }

    /// The tile of a tree node, if it was laid out (not inside a too-small folder).
    pub fn tile_of(&self, node: u32) -> Option<u32> {
        self.node_tile
            .get(node as usize)
            .copied()
            .filter(|&t| t != NONE)
    }

    /// `t` or its ancestor at `depth`.
    pub fn at_depth(&self, mut t: u32, depth: u16) -> u32 {
        while self.tiles[t as usize].depth > depth {
            t = self.tiles[t as usize].parent;
        }
        t
    }

    /// The tile next to `t` in `dir`, at `t`'s level (or above it where the tree is
    /// shallower there): the one across the middle of that edge.
    pub fn neighbor(&self, t: u32, dir: Dir) -> Option<u32> {
        let tile = self.tiles.get(t as usize)?;
        let r = tile.rect;
        let (cx, cy) = r.center();
        let eps = 1e-3;
        let (x, y) = match dir {
            Dir::Left => (r.x0 - eps, cy),
            Dir::Right => (r.x1 + eps, cy),
            Dir::Up => (cx, r.y0 - eps),
            Dir::Down => (cx, r.y1 + eps),
        };
        let hit = self.hit(x, y)?;
        let n = self.at_depth(hit, tile.depth);
        (n != t).then_some(n)
    }

    /// Leaf tiles (what is painted).
    pub fn leaves(&self) -> impl Iterator<Item = &Tile> {
        self.tiles.iter().filter(|t| t.is_leaf())
    }
}
