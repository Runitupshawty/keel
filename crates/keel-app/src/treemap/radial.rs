//! The sunburst and the flame graph: the same partition of the tree (each node a span of
//! its parent's span, by weight, one ring or row per level) drawn as rings around the zoom
//! root or as rows below it.

use super::tree::Tree;
use egui::{Color32, Mesh, Pos2, Rect, Shape, Stroke};
use std::f32::consts::{FRAC_PI_2, TAU};

/// One node's span: `f0..f1` of the root's (0..1), on level `depth` below the root.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seg {
    pub node: u32,
    pub depth: u16,
    pub f0: f64,
    pub f1: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Partition {
    /// By level, each level in span order; the root first.
    pub segs: Vec<Seg>,
    /// Levels drawn (the root's included).
    pub levels: u16,
}

/// Spans of the tree below `root`, `max_levels` deep, leaving out spans under `min_frac`.
pub fn partition(tree: &Tree, root: u32, max_levels: u16, min_frac: f64) -> Partition {
    let mut segs = vec![Seg {
        node: root,
        depth: 0,
        f0: 0.0,
        f1: 1.0,
    }];
    let mut levels = 1;
    let mut i = 0;
    while i < segs.len() {
        let s = segs[i];
        i += 1;
        if s.depth + 1 >= max_levels {
            continue;
        }
        let n = tree.node(s.node);
        let total = n.total as f64;
        if total <= 0.0 {
            continue;
        }
        let mut f = s.f0;
        for &c in &n.children {
            let w = tree.node(c).total as f64;
            if w <= 0.0 {
                continue;
            }
            let f1 = f + (s.f1 - s.f0) * w / total;
            if f1 - f >= min_frac {
                segs.push(Seg {
                    node: c,
                    depth: s.depth + 1,
                    f0: f,
                    f1,
                });
                levels = levels.max(s.depth + 2);
            }
            f = f1;
        }
    }
    Partition { segs, levels }
}

impl Partition {
    /// The span at level `depth` holding `f`.
    pub fn hit(&self, depth: u16, f: f64) -> Option<usize> {
        self.segs
            .iter()
            .position(|s| s.depth == depth && f >= s.f0 && f < s.f1)
    }

    pub fn seg_of(&self, node: u32) -> Option<usize> {
        self.segs.iter().position(|s| s.node == node)
    }

    /// The span before (`-1`) or after (`+1`) `i` on its level.
    pub fn sibling(&self, i: usize, step: i32) -> Option<usize> {
        let d = self.segs[i].depth;
        if step < 0 {
            (0..i).rev().find(|&j| self.segs[j].depth == d)
        } else {
            (i + 1..self.segs.len()).find(|&j| self.segs[j].depth == d)
        }
    }

    /// The first span one level down inside span `i`.
    pub fn first_child(&self, i: usize) -> Option<usize> {
        let s = self.segs[i];
        self.segs
            .iter()
            .position(|c| c.depth == s.depth + 1 && c.f0 >= s.f0 && c.f0 < s.f1)
    }
}

/// Sunburst geometry in `rect`: center and ring width.
pub fn sunburst_geometry(rect: Rect, levels: u16) -> (Pos2, f32) {
    let radius = (rect.width().min(rect.height()) / 2.0 - 4.0).max(8.0);
    (rect.center(), radius / f32::from(levels.max(1)))
}

/// The sunburst level and span fraction under `p`.
pub fn sunburst_at(center: Pos2, ring: f32, p: Pos2) -> (u16, f64) {
    let d = p - center;
    let depth = (d.length() / ring).floor().clamp(0.0, f32::from(u16::MAX)) as u16;
    let angle = d.y.atan2(d.x) + FRAC_PI_2;
    (depth, f64::from((angle / TAU).rem_euclid(1.0)))
}

/// The outline of span `s` as a closed polygon (the root as a circle).
pub fn sector_outline(center: Pos2, ring: f32, s: &Seg) -> Vec<Pos2> {
    let (r0, r1) = (ring * f32::from(s.depth), ring * f32::from(s.depth + 1));
    let steps = arc_steps(s, r1);
    let at = |f: f64, r: f32| {
        let a = f as f32 * TAU - FRAC_PI_2;
        center + egui::vec2(a.cos(), a.sin()) * r
    };
    let mut pts: Vec<Pos2> = (0..=steps)
        .map(|k| at(s.f0 + (s.f1 - s.f0) * k as f64 / steps as f64, r1))
        .collect();
    if r0 > 0.0 {
        pts.extend(
            (0..=steps)
                .rev()
                .map(|k| at(s.f0 + (s.f1 - s.f0) * k as f64 / steps as f64, r0)),
        );
    } else if s.f1 - s.f0 < 1.0 {
        pts.push(center);
    }
    pts
}

fn arc_steps(s: &Seg, r: f32) -> usize {
    (((s.f1 - s.f0) as f32 * TAU * r / 6.0).ceil() as usize).clamp(1, 256)
}

/// The rings as one mesh, colored by `color`, with separators in `line`.
pub fn sunburst_shapes(
    p: &Partition,
    center: Pos2,
    ring: f32,
    color: impl Fn(&Seg) -> Color32,
    line: Color32,
) -> Vec<Shape> {
    let mut mesh = Mesh::default();
    let mut lines = Vec::new();
    let at = |f: f64, r: f32| {
        let a = f as f32 * TAU - FRAC_PI_2;
        center + egui::vec2(a.cos(), a.sin()) * r
    };
    for s in &p.segs {
        let (r0, r1) = (ring * f32::from(s.depth), ring * f32::from(s.depth + 1));
        let c = color(s);
        let steps = arc_steps(s, r1);
        let base = mesh.vertices.len() as u32;
        for k in 0..=steps {
            let f = s.f0 + (s.f1 - s.f0) * k as f64 / steps as f64;
            mesh.colored_vertex(at(f, r0), c);
            mesh.colored_vertex(at(f, r1), c);
        }
        for k in 0..steps as u32 {
            let v = base + 2 * k;
            mesh.add_triangle(v, v + 1, v + 2);
            mesh.add_triangle(v + 1, v + 3, v + 2);
        }
        if s.depth > 0 && (s.f1 - s.f0) as f32 * TAU * r1 > 3.0 {
            lines.push(Shape::line_segment(
                [at(s.f0, r0), at(s.f0, r1)],
                Stroke::new(1.0_f32, line),
            ));
        }
    }
    let mut shapes = vec![Shape::mesh(mesh)];
    for d in 1..p.levels {
        shapes.push(Shape::circle_stroke(
            center,
            ring * f32::from(d),
            Stroke::new(1.0_f32, line),
        ));
    }
    shapes.extend(lines);
    shapes
}

/// Flame graph row height in points.
pub const ROW: f32 = 20.0;

/// The rectangle of span `s` in a flame graph filling `rect` (root on top).
pub fn flame_rect(rect: Rect, s: &Seg) -> Rect {
    let w = rect.width() as f64;
    let x0 = rect.left() + (s.f0 * w) as f32;
    let x1 = rect.left() + (s.f1 * w) as f32;
    let y0 = rect.top() + ROW * f32::from(s.depth);
    Rect::from_min_max(
        Pos2::new(x0, y0),
        Pos2::new((x1 - 1.0).max(x0), y0 + ROW - 1.0),
    )
}

/// The flame graph level and span fraction under `p`.
pub fn flame_at(rect: Rect, p: Pos2) -> (u16, f64) {
    let depth = ((p.y - rect.top()) / ROW)
        .floor()
        .clamp(0.0, f32::from(u16::MAX)) as u16;
    let f = f64::from((p.x - rect.left()) / rect.width().max(1.0));
    (depth, f)
}
