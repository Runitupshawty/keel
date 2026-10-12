//! Treemap colors and shading: the options and style presets, WinDirStat's 13-color
//! extension palette, cushion shading per pixel (van Wijk and van de Wetering, with
//! WinDirStat's brightness and color normalization) into a `ColorImage`, and the flat mode
//! as one `egui::Mesh`.

use super::layout::{Algorithm, Layout, Params};
use super::tree::{Tree, SPECIAL};
use egui::{Color32, ColorImage, Mesh, Pos2, Rect};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

/// WinDirStat's default extension colors, assigned by size rank and repeated after 13.
pub const PALETTE: [[u8; 3]; 13] = [
    [0, 0, 255],
    [255, 0, 0],
    [0, 255, 0],
    [0, 255, 255],
    [255, 0, 255],
    [255, 255, 0],
    [150, 150, 255],
    [255, 150, 150],
    [150, 255, 150],
    [150, 255, 255],
    [255, 150, 255],
    [255, 255, 150],
    [255, 255, 255],
];

/// Pseudo items (unknown folder contents, files of a folder, free space).
const SPECIAL_COLOR: [u8; 3] = [160, 160, 160];
/// Palette colors are brought to this mean brightness before shading.
const PALETTE_BRIGHTNESS: f64 = 0.6;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub algorithm: Algorithm,
    /// Cushion shading; off draws flat tiles.
    pub cushion: bool,
    /// 0..=1
    pub brightness: f32,
    /// Cushion height, 0..=1.
    pub height: f32,
    /// Each level's cushion height relative to its parent's, 0..=1.
    pub scale: f32,
    /// Light source direction in x and y, -2..=2 each (up and to the left is (-1, -1)).
    pub light: [f32; 2],
    /// Ambient light, 0..=1.
    pub ambient: f32,
    pub grid: bool,
    pub grid_color: [u8; 3],
    /// Frames around folders.
    pub frames: bool,
    pub frame_color: [u8; 3],
    /// Folders get a frame only when both sides are at least this many points.
    pub frame_min: f32,
    /// Extension colors by size rank (editable).
    pub palette: Vec<[u8; 3]>,
    /// Outline every file of the selected file's extension.
    pub highlight_ext: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Preset {
    WinDirStat,
    KDirStat,
    SequoiaView,
    Flat,
}

impl Preset {
    pub const ALL: [Preset; 4] = [
        Preset::WinDirStat,
        Preset::KDirStat,
        Preset::SequoiaView,
        Preset::Flat,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Preset::WinDirStat => "WinDirStat",
            Preset::KDirStat => "KDirStat",
            Preset::SequoiaView => "SequoiaView",
            Preset::Flat => "Flat",
        }
    }

    pub fn options(self) -> Options {
        let base = Options {
            algorithm: Algorithm::Squarified,
            cushion: true,
            brightness: 0.88,
            height: 0.38,
            scale: 0.91,
            light: [-1.0, -1.0],
            ambient: 0.13,
            grid: false,
            grid_color: [0, 0, 0],
            frames: false,
            frame_color: [64, 64, 64],
            frame_min: 40.0,
            palette: PALETTE.to_vec(),
            highlight_ext: true,
        };
        match self {
            Preset::WinDirStat => base,
            Preset::KDirStat => Options {
                algorithm: Algorithm::Strip,
                grid: true,
                grid_color: [96, 96, 96],
                ..base
            },
            Preset::SequoiaView => Options {
                brightness: 0.85,
                height: 0.40,
                scale: 0.90,
                ambient: 0.15,
                ..base
            },
            Preset::Flat => Options {
                cushion: false,
                grid: true,
                grid_color: [32, 32, 32],
                frames: true,
                ..base
            },
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Preset::WinDirStat.options()
    }
}

impl Options {
    /// The preset these options are, if unchanged from one.
    pub fn preset(&self) -> Option<Preset> {
        Preset::ALL.into_iter().find(|p| p.options() == *self)
    }

    /// Layout parameters in physical pixels (`ppp` pixels per point).
    pub fn params(&self, ppp: f32) -> Params {
        let ppp = f64::from(ppp.max(0.1));
        Params {
            algorithm: self.algorithm,
            min_px: 1.0,
            height: if self.cushion {
                f64::from(self.height.clamp(0.0, 1.0))
            } else {
                0.0
            },
            scale: f64::from(self.scale.clamp(0.0, 1.0)),
            frame_px: if self.frames {
                (2.0 * ppp).round()
            } else {
                0.0
            },
            frame_min: f64::from(self.frame_min.max(0.0)) * ppp,
            ..Params::default()
        }
    }

    /// The palette color of extension rank `rank` (cycling).
    pub fn rank_color(&self, rank: u32) -> [u8; 3] {
        if rank == u32::MAX || self.palette.is_empty() {
            return SPECIAL_COLOR;
        }
        self.palette[rank as usize % self.palette.len()]
    }

    /// Each color key's base color (0..=255 per channel, brought to the palette brightness)
    /// for the tree shown from `root`.
    pub fn key_colors(&self, tree: &Tree, root: u32) -> Vec<[f64; 3]> {
        let ranks = tree.key_ranks(root);
        (0..tree.keys.len())
            .map(|k| {
                let c = if tree.keys[k].starts_with(SPECIAL) {
                    SPECIAL_COLOR
                } else {
                    self.rank_color(ranks[k])
                };
                make_bright(c, PALETTE_BRIGHTNESS)
            })
            .collect()
    }

    /// The flat (unshaded) display color of a base color.
    pub fn flat_color(&self, base: [f64; 3]) -> Color32 {
        let l = Light::of(self);
        l.shade(base, 0.0, 0.0)
    }
}

/// `c` scaled to the mean brightness `b` (WinDirStat's MakeBrightColor).
fn make_bright(c: [u8; 3], b: f64) -> [f64; 3] {
    let d = c.map(|v| f64::from(v) / 255.0);
    let s = d[0] + d[1] + d[2];
    if s <= 0.0 {
        return [b * 255.0; 3];
    }
    let f = 3.0 * b / s;
    let [r, g, bl] = normalize(d.map(|v| v * f * 255.0));
    [r, g, bl]
}

/// Moves a channel's overflow above 255 into the other two (WinDirStat's NormalizeColor).
fn normalize(mut c: [f64; 3]) -> [f64; 3] {
    fn spill(c: &mut [f64; 3], first: usize, second: usize, third: usize) {
        let h = (c[first] - 255.0) / 2.0;
        c[first] = 255.0;
        c[second] += h;
        c[third] += h;
        if c[second] > 255.0 {
            c[third] += c[second] - 255.0;
            c[second] = 255.0;
        } else if c[third] > 255.0 {
            c[second] += c[third] - 255.0;
            c[third] = 255.0;
        }
    }
    if c[0] > 255.0 {
        spill(&mut c, 0, 1, 2);
    } else if c[1] > 255.0 {
        spill(&mut c, 1, 2, 0);
    } else if c[2] > 255.0 {
        spill(&mut c, 2, 0, 1);
    }
    c.map(|v| v.clamp(0.0, 255.0))
}

fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// The light and brightness for shading.
#[derive(Clone, Copy)]
struct Light {
    lx: f64,
    ly: f64,
    lz: f64,
    ambient: f64,
    direct: f64,
    factor: f64,
}

impl Light {
    fn of(o: &Options) -> Self {
        let (lx, ly, lz) = (
            f64::from(o.light[0].clamp(-4.0, 4.0)),
            f64::from(o.light[1].clamp(-4.0, 4.0)),
            10.0,
        );
        let len = (lx * lx + ly * ly + lz * lz).sqrt();
        let ambient = f64::from(o.ambient.clamp(0.0, 1.0));
        Self {
            lx: lx / len,
            ly: ly / len,
            lz: lz / len,
            ambient,
            direct: 1.0 - ambient,
            factor: f64::from(o.brightness.clamp(0.0, 1.0)) / PALETTE_BRIGHTNESS,
        }
    }

    /// The color of a surface point with normal slope (nx, ny).
    fn shade(&self, base: [f64; 3], nx: f64, ny: f64) -> Color32 {
        let cosa =
            ((nx * self.lx + ny * self.ly + self.lz) / (nx * nx + ny * ny + 1.0).sqrt()).min(1.0);
        let pixel = (self.direct * cosa).max(0.0) + self.ambient;
        let f = self.factor * pixel;
        let [r, g, b] = normalize(base.map(|v| v * f));
        Color32::from_rgb(r as u8, g as u8, b as u8)
    }
}

/// Pixel indices whose centers fall in `[a, b)`, clamped to `0..max`.
fn span(a: f64, b: f64, max: usize) -> (usize, usize) {
    let s = (a - 0.5).ceil().max(0.0) as usize;
    let e = ((b - 0.5).ceil().max(0.0) as usize).min(max);
    (s.min(e), e)
}

/// Shades `lay` (laid out in pixels from (0, 0)) into an image of `size` pixels: cushions
/// (or flat tiles), the grid and the folder frames. None when `cancel` was set meanwhile.
pub fn render(
    tree: &Tree,
    lay: &Layout,
    colors: &[[f64; 3]],
    o: &Options,
    size: [usize; 2],
    cancel: Option<&AtomicBool>,
) -> Option<ColorImage> {
    let [w, h] = size;
    let light = Light::of(o);
    let grid = rgb(o.grid_color);
    let frame = rgb(o.frame_color);
    let mut px = vec![if o.grid { grid } else { Color32::BLACK }; w * h];
    let cushion = o.cushion;
    for (i, t) in lay.tiles.iter().enumerate() {
        if i % 1024 == 1023 && cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return None;
        }
        let (x0, x1) = span(t.rect.x0, t.rect.x1, w);
        let (y0, y1) = span(t.rect.y0, t.rect.y1, h);
        if t.framed {
            // The band between the folder's edge and its children's area.
            let inner = t.rect.shrink(lay.frame_px);
            let (ix0, ix1) = span(inner.x0, inner.x1, w);
            let (iy0, iy1) = span(inner.y0, inner.y1, h);
            for y in y0..y1 {
                let row = &mut px[y * w..(y + 1) * w];
                if y < iy0 || y >= iy1 {
                    row[x0..x1].fill(frame);
                } else {
                    row[x0..ix0.clamp(x0, x1)].fill(frame);
                    row[ix1.clamp(x0, x1)..x1].fill(frame);
                }
            }
        }
        if !t.is_leaf() {
            continue;
        }
        let base = colors[tree.node(t.node).key as usize];
        // The grid takes each tile's last column and row, when the tile is big enough.
        let (gx, gy) = if o.grid {
            (
                if x1 - x0 > 2 { x1 - 1 } else { x1 },
                if y1 - y0 > 2 { y1 - 1 } else { y1 },
            )
        } else {
            (x1, y1)
        };
        let flat = (!cushion).then(|| light.shade(base, 0.0, 0.0));
        let s = t.surface;
        for y in y0..gy {
            let row = &mut px[y * w..(y + 1) * w];
            if let Some(c) = flat {
                row[x0..gx].fill(c);
                continue;
            }
            let fy = y as f64 + 0.5;
            let ny = -(2.0 * s[1] * fy + s[3]);
            for (x, p) in row.iter_mut().enumerate().take(gx).skip(x0) {
                let fx = x as f64 + 0.5;
                let nx = -(2.0 * s[0] * fx + s[2]);
                *p = light.shade(base, nx, ny);
            }
        }
    }
    Some(ColorImage {
        size: [w, h],
        pixels: px,
    })
}

/// The flat mode: every leaf as one quad of a single mesh, in points from (0, 0) (the
/// layout is in pixels; `ppp` pixels per point). The grid is the background showing
/// between tiles; folder frames are their folder's quad showing around the children.
pub fn flat_mesh(tree: &Tree, lay: &Layout, colors: &[[f64; 3]], o: &Options, ppp: f32) -> Mesh {
    let light = Light::of(o);
    let k = 1.0 / f64::from(ppp.max(0.1));
    let rect = |r: &super::layout::R, inset: f64| {
        Rect::from_min_max(
            Pos2::new((r.x0 * k) as f32, (r.y0 * k) as f32),
            Pos2::new(
                ((r.x1 - inset).max(r.x0) * k) as f32,
                ((r.y1 - inset).max(r.y0) * k) as f32,
            ),
        )
    };
    let mut mesh = Mesh::default();
    let quads = lay.tiles.iter().filter(|t| t.is_leaf() || t.framed).count() + 1;
    mesh.reserve_vertices(quads * 4);
    mesh.reserve_triangles(quads * 2);
    let back = if o.grid {
        rgb(o.grid_color)
    } else {
        Color32::BLACK
    };
    mesh.add_colored_rect(rect(&lay.bounds, 0.0), back);
    let frame = rgb(o.frame_color);
    for t in &lay.tiles {
        if t.framed {
            mesh.add_colored_rect(rect(&t.rect, 0.0), frame);
        }
        if !t.is_leaf() {
            continue;
        }
        let inset = if o.grid && t.rect.w() > 2.0 && t.rect.h() > 2.0 {
            1.0
        } else {
            0.0
        };
        let base = colors[tree.node(t.node).key as usize];
        mesh.add_colored_rect(rect(&t.rect, inset), light.shade(base, 0.0, 0.0));
    }
    mesh
}
