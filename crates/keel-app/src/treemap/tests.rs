use super::layout::{self, Algorithm, Dir, Layout, Params, R};
use super::radial;
use super::shade::{self, Options, Preset};
use super::tree::{NodeId, Tree, WeightedTree};
use super::widget::{self, TreemapView};
use std::time::Instant;

/// A deterministic generator (no rand dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn weight(&mut self) -> u64 {
        // Spread over four orders of magnitude, like file sizes.
        let exp = self.next() % 4;
        1 + self.next() % (10u64.pow(exp as u32 + 1))
    }
}

fn flat_tree(weights: &[u64]) -> Tree {
    let mut t = Tree::new(0, "root");
    for (i, w) in weights.iter().enumerate() {
        t.add(0, i as NodeId + 1, *w, "bin", &format!("f{i}"));
    }
    t.finish();
    t
}

/// Folders of folders of files, `fanout` wide, `depth` deep.
fn nested_tree(rng: &mut Lcg, fanout: usize, depth: usize) -> Tree {
    let mut t = Tree::new(0, "root");
    let mut next_id = 1;
    let mut level = vec![0u32];
    for d in 0..depth {
        let mut below = Vec::new();
        for &p in &level {
            for i in 0..fanout {
                let leaf = d + 1 == depth;
                let w = if leaf { rng.weight() } else { 0 };
                let ext = ["txt", "jpg", "mp4", "rs"][i % 4];
                below.push(t.add(p, next_id, w, ext, &format!("n{next_id}")));
                next_id += 1;
            }
        }
        level = below;
    }
    t.finish();
    t
}

fn exact(alg: Algorithm) -> Params {
    Params {
        algorithm: alg,
        min_px: 0.0,
        height: 0.38,
        scale: 0.91,
        frame_px: 0.0,
        ..Params::default()
    }
}

/// Every folder's children: areas proportional to their weights (within 0.5 %), no two
/// overlapping, all inside the folder; and the children cover the folder.
fn check_invariants(tree: &Tree, lay: &Layout, what: &str) {
    for (i, t) in lay.tiles.iter().enumerate() {
        let mut kids = Vec::new();
        let mut c = t.first_child;
        while c != layout::NONE {
            kids.push(&lay.tiles[c as usize]);
            c = lay.tiles[c as usize].next_sibling;
        }
        if kids.is_empty() {
            continue;
        }
        let total = tree.node(t.node).total as f64;
        let area = t.rect.area();
        let mut covered = 0.0;
        for k in &kids {
            let want = tree.node(k.node).total as f64 / total;
            let got = k.rect.area() / area;
            assert!(
                (got - want).abs() <= 0.005 * want,
                "{what}: tile {i} child area {got} vs weight share {want}"
            );
            let eps = 1e-6 * (t.rect.w() + t.rect.h());
            assert!(
                k.rect.x0 >= t.rect.x0 - eps
                    && k.rect.y0 >= t.rect.y0 - eps
                    && k.rect.x1 <= t.rect.x1 + eps
                    && k.rect.y1 <= t.rect.y1 + eps,
                "{what}: child {:?} outside {:?}",
                k.rect,
                t.rect
            );
            covered += k.rect.area();
        }
        assert!(
            (covered - area).abs() <= 1e-6 * area,
            "{what}: children cover {covered} of {area}"
        );
        for a in 0..kids.len() {
            for b in a + 1..kids.len() {
                let o = kids[a].rect.overlap(&kids[b].rect);
                assert!(
                    o <= 1e-9 * area,
                    "{what}: {:?} and {:?} overlap by {o}",
                    kids[a].rect,
                    kids[b].rect
                );
            }
        }
    }
}

#[test]
fn every_layout_keeps_areas_proportional_without_overlap_inside_bounds() {
    let mut rng = Lcg(7);
    let weights: Vec<u64> = (0..600).map(|_| rng.weight()).collect();
    let flat = flat_tree(&weights);
    let nested = nested_tree(&mut rng, 6, 4);
    for alg in Algorithm::ALL {
        for (name, tree) in [("flat", &flat), ("nested", &nested)] {
            for bounds in [
                R::new(0.0, 0.0, 1000.0, 700.0),
                R::new(10.0, 20.0, 310.0, 1220.0),
                R::new(0.0, 0.0, 2000.0, 90.0),
            ] {
                let lay = layout::layout(tree, 0, bounds, &exact(alg), None).unwrap();
                assert_eq!(lay.tiles[0].rect, bounds);
                let leaves = lay.leaves().count();
                let want = (0..tree.len() as u32)
                    .filter(|&n| tree.is_leaf(n) && tree.node(n).total > 0)
                    .count();
                assert_eq!(leaves, want, "{alg:?} {name}: every file laid out");
                check_invariants(tree, &lay, &format!("{alg:?} {name} {bounds:?}"));
            }
        }
    }
}

#[test]
fn squarified_tiles_are_closer_to_squares_than_strips() {
    let mut rng = Lcg(11);
    let weights: Vec<u64> = (0..400).map(|_| rng.weight()).collect();
    let tree = flat_tree(&weights);
    let bounds = R::new(0.0, 0.0, 1200.0, 800.0);
    let mean_aspect = |alg| {
        let lay = layout::layout(&tree, 0, bounds, &exact(alg), None).unwrap();
        let (sum, n) = lay.leaves().fold((0.0, 0), |(s, n), t| {
            let (w, h) = (t.rect.w(), t.rect.h());
            (s + w.max(h) / w.min(h), n + 1)
        });
        sum / f64::from(n)
    };
    let sq = mean_aspect(Algorithm::Squarified);
    assert!(sq < 3.0, "squarified mean aspect {sq}");
    assert!(sq <= mean_aspect(Algorithm::Hilbert));
}

/// Wikipedia's Hilbert curve index to cell (x right, y up), n a power of two.
fn d2xy(n: u32, d: u32) -> (u32, u32) {
    let (mut x, mut y, mut t, mut s) = (0, 0, d, 1);
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (x, y)
}

/// Equal tiles of a square in layout order, as grid cells (x right, y up).
fn cells(alg: Algorithm, side: u32) -> Vec<(u32, u32)> {
    let n = side * side;
    let tree = flat_tree(&vec![1; n as usize]);
    let size = f64::from(side) * 10.0;
    let lay = layout::layout(&tree, 0, R::new(0.0, 0.0, size, size), &exact(alg), None).unwrap();
    // Children were laid out in tree order (all equal: insertion order).
    let mut out = Vec::new();
    let mut c = lay.tiles[0].first_child;
    while c != layout::NONE {
        let (cx, cy) = lay.tiles[c as usize].rect.center();
        out.push(((cx / 10.0) as u32, side - 1 - (cy / 10.0) as u32));
        c = lay.tiles[c as usize].next_sibling;
    }
    assert_eq!(out.len(), n as usize);
    out
}

fn adjacent(a: (u32, u32), b: (u32, u32)) -> bool {
    a.0.abs_diff(b.0) + a.1.abs_diff(b.1) == 1
}

#[test]
fn hilbert_layout_follows_the_hilbert_curve() {
    for side in [2, 4, 8] {
        let got = cells(Algorithm::Hilbert, side);
        let want: Vec<_> = (0..side * side).map(|d| d2xy(side, d)).collect();
        assert_eq!(got, want, "{side}×{side}");
    }
}

#[test]
fn moore_layout_is_a_closed_loop_of_hilbert_curves() {
    let got = cells(Algorithm::Moore, 4);
    let want = vec![
        (1, 0),
        (0, 0),
        (0, 1),
        (1, 1),
        (1, 2),
        (0, 2),
        (0, 3),
        (1, 3),
        (2, 3),
        (3, 3),
        (3, 2),
        (2, 2),
        (2, 1),
        (3, 1),
        (3, 0),
        (2, 0),
    ];
    assert_eq!(got, want);
    for side in [2, 4, 8, 16] {
        let got = cells(Algorithm::Moore, side);
        let mut seen = got.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), got.len(), "every cell once");
        for w in got.windows(2) {
            assert!(adjacent(w[0], w[1]), "{side}: {:?} then {:?}", w[0], w[1]);
        }
        assert!(
            adjacent(got[0], got[got.len() - 1]),
            "{side}: the loop closes"
        );
    }
}

#[test]
fn hit_testing_finds_the_deepest_tile_and_neighbors() {
    let mut rng = Lcg(3);
    let tree = nested_tree(&mut rng, 5, 3);
    let bounds = R::new(0.0, 0.0, 800.0, 600.0);
    for alg in Algorithm::ALL {
        let lay = layout::layout(&tree, 0, bounds, &exact(alg), None).unwrap();
        for (i, t) in lay.tiles.iter().enumerate() {
            if !t.is_leaf() || t.rect.w() < 1e-6 || t.rect.h() < 1e-6 {
                continue;
            }
            let (x, y) = t.rect.center();
            assert_eq!(lay.hit(x, y), Some(i as u32), "{alg:?}");
        }
        assert_eq!(lay.hit(-1.0, 5.0), None);
        assert_eq!(lay.hit(800.0, 5.0), None, "right edge is outside");
        // The tile of a node, and a node's tile back.
        let leaf = lay.leaves().next().unwrap();
        assert_eq!(
            lay.tile_of(leaf.node).map(|t| lay.tiles[t as usize].node),
            Some(leaf.node)
        );
    }
    // Four equal files on a Hilbert square: bottom left, top left, top right, bottom right.
    let tree = flat_tree(&[1, 1, 1, 1]);
    let lay = layout::layout(
        &tree,
        0,
        R::new(0.0, 0.0, 100.0, 100.0),
        &exact(Algorithm::Hilbert),
        None,
    )
    .unwrap();
    let tile = |label: &str| {
        (lay.tiles
            .iter()
            .position(|t| tree.node(t.node).label == label))
        .unwrap() as u32
    };
    let (bl, tl, tr, br) = (tile("f0"), tile("f1"), tile("f2"), tile("f3"));
    assert_eq!(lay.neighbor(bl, Dir::Up), Some(tl));
    assert_eq!(lay.neighbor(bl, Dir::Right), Some(br));
    assert_eq!(lay.neighbor(tl, Dir::Right), Some(tr));
    assert_eq!(lay.neighbor(tr, Dir::Down), Some(br));
    assert_eq!(lay.neighbor(tl, Dir::Left), None);
    assert_eq!(lay.neighbor(tl, Dir::Up), None);
}

#[test]
fn neighbors_stay_on_the_selected_level() {
    // Two folders side by side, each with two files.
    let mut t = Tree::new(0, "root");
    let a = t.add(0, 1, 0, "/folder", "a");
    let b = t.add(0, 2, 0, "/folder", "b");
    for (p, base) in [(a, 10), (b, 20)] {
        t.add(p, base + 1, 5, "txt", "x");
        t.add(p, base + 2, 5, "txt", "y");
    }
    t.finish();
    let lay = layout::layout(
        &t,
        0,
        R::new(0.0, 0.0, 200.0, 100.0),
        &exact(Algorithm::Squarified),
        None,
    )
    .unwrap();
    let ta = lay.tile_of(a).unwrap();
    let tb = lay.tile_of(b).unwrap();
    let right = lay.neighbor(ta, Dir::Right).or(lay.neighbor(ta, Dir::Down));
    assert_eq!(right, Some(tb), "folder to folder, not into its files");
}

#[test]
fn tree_totals_synthetic_rest_ranks_and_paths() {
    let mut t = Tree::new(100, "root");
    let docs = t.add(0, 1, 50, "/folder", "docs");
    t.add(docs, 2, 30, "pdf", "a.pdf");
    t.add(0, 3, 70, "mp4", "film.mp4");
    t.add(0, 4, 0, "txt", "empty.txt");
    t.finish();
    assert_eq!(t.node(0).total, 120);
    assert_eq!(t.node(docs).total, 50);
    let rest = t.node(docs).children[1];
    assert!(t.node(rest).synthetic);
    assert_eq!(t.node(rest).total, 20);
    assert_eq!(t.node(rest).label, "Other in docs");
    // Biggest first.
    assert_eq!(t.node(0).children[0], t.find(3).unwrap());
    let ranks = t.key_ranks(0);
    let rank = |k: &str| ranks[t.keys.iter().position(|x| x == k).unwrap()];
    assert_eq!(rank("mp4"), 0);
    assert_eq!(rank("pdf"), 1);
    assert_eq!(rank("/other"), u32::MAX, "special keys are not ranked");
    let pdf = t.find(2).unwrap();
    assert_eq!(t.path(pdf), "docs › a.pdf");
    assert!(t.contains(docs, pdf));
    assert_eq!(t.child_toward(0, pdf), Some(docs));
}

/// A scan tree in the shape another module keeps: parents after children, two roots.
struct Scan(Vec<(NodeId, Option<usize>, u64, &'static str, &'static str)>);

impl WeightedTree for Scan {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn id(&self, i: usize) -> NodeId {
        self.0[i].0
    }
    fn parent(&self, i: usize) -> Option<usize> {
        self.0[i].1
    }
    fn weight(&self, i: usize) -> u64 {
        self.0[i].2
    }
    fn color_key(&self, i: usize) -> &str {
        self.0[i].3
    }
    fn label(&self, i: usize) -> &str {
        self.0[i].4
    }
}

#[test]
fn any_weighted_tree_converts() {
    let scan = Scan(vec![
        (10, Some(2), 4, "jpg", "a.jpg"),
        (11, Some(2), 6, "jpg", "b.jpg"),
        (12, None, 0, "/folder", "C:"),
        (13, None, 5, "/free", "Free space"),
    ]);
    let t = Tree::from_source(&scan, "All drives");
    assert_eq!(t.node(0).label, "All drives");
    assert_eq!(t.node(0).total, 15);
    let c = t.find(12).unwrap();
    assert_eq!(t.node(c).total, 10);
    assert_eq!(t.node(t.find(11).unwrap()).parent, Some(c));
}

#[test]
fn presets_round_trip_and_default_is_squarified_cushions() {
    for p in Preset::ALL {
        assert_eq!(p.options().preset(), Some(p), "{}", p.name());
    }
    let d = Options::default();
    assert_eq!(d.algorithm, Algorithm::Squarified);
    assert!(d.cushion);
    assert_eq!(d.palette.len(), 13);
    assert_eq!(Preset::KDirStat.options().algorithm, Algorithm::Strip);
    assert!(!Preset::Flat.options().cushion);
    let mut custom = d.clone();
    custom.brightness = 0.5;
    assert_eq!(custom.preset(), None);
    // The palette cycles by rank.
    assert_eq!(d.rank_color(13), d.rank_color(0));
    assert_ne!(d.rank_color(1), d.rank_color(0));
}

#[test]
fn cushions_are_lit_from_the_light_source_and_frames_and_grid_are_drawn() {
    let tree = flat_tree(&[1]);
    let mut o = Options::default();
    let (lay, img, mesh) = widget::render(&tree, 0, [100, 100], 1.0, &o, None).unwrap();
    assert!(mesh.is_none());
    let img = img.unwrap();
    assert_eq!(img.size, [100, 100]);
    let lum = |x: usize, y: usize| {
        let c = img.pixels[y * 100 + x];
        u32::from(c.r()) + u32::from(c.g()) + u32::from(c.b())
    };
    // Light from the top left: that slope is brighter than the bottom right one.
    assert!(
        lum(25, 25) > lum(75, 75),
        "{} vs {}",
        lum(25, 25),
        lum(75, 75)
    );
    assert_eq!(lay.leaves().count(), 1);

    // Flat: one quad per leaf plus the background.
    o.cushion = false;
    let tree = flat_tree(&[3, 2, 1]);
    let (_, img, mesh) = widget::render(&tree, 0, [90, 60], 1.0, &o, None).unwrap();
    assert!(img.is_none());
    assert_eq!(mesh.unwrap().vertices.len(), 4 * (3 + 1));

    // Frames: a folder's band in the frame color, its children inside.
    let mut t = Tree::new(0, "root");
    let f = t.add(0, 1, 0, "/folder", "f");
    t.add(f, 2, 1, "txt", "a");
    t.finish();
    let mut o = Options {
        frames: true,
        frame_min: 10.0,
        frame_color: [1, 2, 3],
        grid: true,
        grid_color: [9, 9, 9],
        ..Options::default()
    };
    let (_, img, _) = widget::render(&t, 0, [80, 80], 1.0, &o, None).unwrap();
    let img = img.unwrap();
    assert_eq!(img.pixels[0], egui::Color32::from_rgb(1, 2, 3), "frame");
    assert_eq!(img.pixels[79 * 80 - 4], egui::Color32::from_rgb(1, 2, 3));
    // The grid line along the file's last row, inside the frames.
    let inner_last_row = 80 - 4 - 1;
    assert_eq!(
        img.pixels[inner_last_row * 80 + 40],
        egui::Color32::from_rgb(9, 9, 9),
        "grid"
    );
    o.frames = false;
    let (lay, _, _) = widget::render(&t, 0, [80, 80], 1.0, &o, None).unwrap();
    assert!(lay.tiles.iter().all(|t| !t.framed));
}

#[test]
fn colors_follow_extension_size_rank() {
    let mut t = Tree::new(0, "root");
    t.add(0, 1, 100, "mp4", "big");
    t.add(0, 2, 10, "txt", "small");
    t.add(0, 3, 50, "/folder", "unknown");
    t.finish();
    let o = Options::default();
    let colors = o.key_colors(&t, 0);
    let key = |k: &str| t.keys.iter().position(|x| x == k).unwrap();
    // mp4 is the biggest type: the palette's first color (blue), txt the second (red).
    let mp4 = colors[key("mp4")];
    let txt = colors[key("txt")];
    assert!(mp4[2] > mp4[0] && mp4[2] > mp4[1], "{mp4:?}");
    assert!(txt[0] > txt[1] && txt[0] > txt[2], "{txt:?}");
    let gray = colors[key("/folder")];
    assert!((gray[0] - gray[1]).abs() < 1.0 && (gray[1] - gray[2]).abs() < 1.0);
}

#[test]
fn the_view_selects_zooms_and_steps_through_the_treemap() {
    let mut t = Tree::new(0, "root");
    let a = t.add(0, 1, 0, "/folder", "a");
    t.add(a, 2, 60, "txt", "a1");
    t.add(a, 3, 40, "txt", "a2");
    t.add(0, 4, 50, "jpg", "b");
    t.finish();
    let mut v = TreemapView::new();
    v.set_tree(t);
    let o = Options::default();
    v.render_now([300, 200], &o);
    // The file under a point is what a click picks.
    let a1 = v.tree().find(2).unwrap();
    let n = v.node_at(10.0, 10.0).unwrap();
    assert!(v.tree().contains(v.tree().find(1).unwrap(), n) || n == v.tree().find(4).unwrap());
    // Arrow keys from nothing select the first child of the zoom root, then move.
    assert!(v.step(Dir::Right) || v.step(Dir::Left) || v.step(Dir::Down));
    assert!(v.selected.is_some());
    // Zoom into the folder (as a double-click on its file does), and out again.
    assert!(v.zoom_to(v.tree().find(1).unwrap()));
    assert!(!v.zoom_to(a1), "files are not zoomed into");
    v.render_now([300, 200], &o);
    assert!(v.select(2));
    assert!(v.step(Dir::Right) || v.step(Dir::Down) || v.step(Dir::Left) || v.step(Dir::Up));
    assert_eq!(v.selected_id(), Some(3), "the other file of the folder");
    assert!(v.zoom_out());
    assert!(!v.zoom_out(), "already at the top");
    // A new tree keeps the zoom and selection by id.
    assert!(v.zoom_to(v.tree().find(1).unwrap()));
    let mut t2 = Tree::new(0, "root");
    let a = t2.add(0, 1, 0, "/folder", "a");
    t2.add(a, 3, 40, "txt", "a2");
    t2.finish();
    v.set_tree(t2);
    assert_eq!(v.tree().node(v.root).id, 1);
    assert_eq!(v.selected_id(), Some(3));
}

#[test]
fn sunburst_and_flame_partitions_split_spans_by_weight() {
    let mut t = Tree::new(0, "root");
    let a = t.add(0, 1, 0, "/folder", "a");
    t.add(a, 2, 30, "txt", "a1");
    t.add(a, 3, 10, "txt", "a2");
    t.add(0, 4, 60, "jpg", "b");
    t.add(0, 5, 0, "jpg", "nothing");
    t.finish();
    let p = radial::partition(&t, 0, 8, 0.0);
    assert_eq!(p.levels, 3);
    assert_eq!(p.segs.len(), 5, "the empty file is left out");
    let seg = |id| p.segs[p.seg_of(t.find(id).unwrap()).unwrap()];
    let (sb, sa) = (seg(4), seg(1));
    assert!((sb.f1 - sb.f0 - 0.6).abs() < 1e-12);
    assert!((sa.f1 - sa.f0 - 0.4).abs() < 1e-12);
    let a1 = seg(2);
    assert!((a1.f1 - a1.f0 - 0.3).abs() < 1e-12);
    assert!(a1.f0 >= sa.f0 && a1.f1 <= sa.f1 + 1e-12);
    // Hits by level and fraction; siblings and children for the arrow keys.
    assert_eq!(
        p.hit(1, 0.1).map(|i| p.segs[i].node),
        Some(t.find(4).unwrap())
    );
    assert_eq!(
        p.hit(2, 0.65).map(|i| p.segs[i].node),
        Some(t.find(2).unwrap())
    );
    assert_eq!(p.hit(2, 0.1), None, "b has no children");
    let ib = p.seg_of(t.find(4).unwrap()).unwrap();
    assert_eq!(p.sibling(ib, 1), p.seg_of(a));
    assert_eq!(p.sibling(ib, -1), None);
    assert_eq!(
        p.first_child(p.seg_of(a).unwrap()),
        p.seg_of(t.find(2).unwrap())
    );
    // Small spans are left out; levels are capped.
    assert_eq!(radial::partition(&t, 0, 2, 0.0).levels, 2);
    assert_eq!(radial::partition(&t, 0, 8, 0.35).segs.len(), 3);
    // Geometry: the sunburst's top is fraction 0, clockwise; the flame graph's root row on top.
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 200.0));
    let (c, ring) = radial::sunburst_geometry(rect, 3);
    let (d, f) = radial::sunburst_at(c, ring, c + egui::vec2(0.1, -1.5 * ring));
    assert_eq!(d, 1);
    assert!(f < 0.01, "{f}");
    let (d, f) = radial::sunburst_at(c, ring, c + egui::vec2(1.5 * ring, 0.0));
    assert!((f - 0.25).abs() < 0.01 && d == 1);
    let (d, f) = radial::flame_at(rect, egui::Pos2::new(150.0, radial::ROW * 1.5));
    assert_eq!(d, 1);
    assert!((f - 0.75).abs() < 1e-6);
}

/// The plan's budget: 200,000 tiles under 200 ms for the first render, hit tests far under
/// a frame. `cargo test -p keel-app --release treemap::tests::render_benchmark -- --ignored --nocapture`
#[test]
#[ignore = "benchmark: run in release with --ignored"]
fn render_benchmark() {
    let mut rng = Lcg(42);
    // 500 folders × 400 files = 200,000 files.
    let mut t = Tree::new(0, "root");
    let exts = [
        "txt", "jpg", "png", "mp4", "rs", "pdf", "zip", "exe", "dll", "doc",
    ];
    let mut id = 1;
    for f in 0..500 {
        let folder = t.add(0, id, 0, "/folder", &format!("folder{f}"));
        id += 1;
        for i in 0..400 {
            t.add(folder, id, rng.weight(), exts[i % exts.len()], "file");
            id += 1;
        }
    }
    t.finish();
    for alg in Algorithm::ALL {
        for cushion in [true, false] {
            let o = Options {
                algorithm: alg,
                cushion,
                ..Options::default()
            };
            let started = Instant::now();
            let (lay, _, _) = widget::render(&t, 0, [1920, 1080], 1.0, &o, None).unwrap();
            let took = started.elapsed();
            let started = Instant::now();
            let mut hits = 0;
            for k in 0..10_000u32 {
                let (x, y) = (f64::from(k % 1920), f64::from((k * 7) % 1080));
                hits += usize::from(lay.hit(x, y).is_some());
            }
            let hit_took = started.elapsed() / 10_000;
            println!(
                "{alg:?} cushion={cushion}: {} tiles in {took:?}, a hit test {hit_took:?} ({hits})",
                lay.tiles.len()
            );
            if !cfg!(debug_assertions) {
                assert!(took.as_millis() < 200, "{alg:?}: {took:?}");
                assert!(hit_took.as_micros() < 100, "{alg:?}: {hit_took:?}");
            }
        }
    }
    let _ = shade::PALETTE;
}
