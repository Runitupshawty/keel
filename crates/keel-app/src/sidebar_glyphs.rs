//! Phase 14: the sidebar's icons, drawn as thin strokes in one color (the accent), in the
//! spirit of a Finder sidebar. Keel's own drawings on a 16-unit grid; no artwork shipped.

use egui::{pos2, vec2, Color32, Painter, Pos2, Rect, Shape, Stroke, StrokeKind};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Glyph {
    Folder,
    Recents,
    Applications,
    Desktop,
    Documents,
    Downloads,
    Home,
    Drive,
    Optical,
    Network,
    Trash,
    Terminal,
    Archive,
    Cloud,
    Overview,
    Star,
    View,
    /// A collection: two stacked cards.
    Collection,
    Plus,
    Eject,
    /// A filled dot in this color (status dots, tag colors).
    Dot(Color32),
}

/// Draws `glyph` into `rect` (square) in `color`.
pub fn paint(p: &Painter, rect: Rect, glyph: Glyph, color: Color32) {
    let s = rect.width() / 16.0;
    let at = |x: f32, y: f32| -> Pos2 { rect.min + vec2(x, y) * s };
    let box_ = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(at(x0, y0), at(x1, y1));
    let stroke = Stroke::new(1.3 * s, color);
    let line = |pts: &[(f32, f32)]| {
        let pts = pts.iter().map(|&(x, y)| at(x, y)).collect();
        p.add(Shape::line(pts, stroke));
    };
    let closed = |pts: &[(f32, f32)]| {
        let pts = pts.iter().map(|&(x, y)| at(x, y)).collect();
        p.add(Shape::closed_line(pts, stroke));
    };
    let frame = |r: Rect, radius: f32| p.rect_stroke(r, radius * s, stroke, StrokeKind::Middle);
    let ring = |x: f32, y: f32, r: f32| p.circle_stroke(at(x, y), r * s, stroke);
    let fill = |x: f32, y: f32, r: f32| p.circle_filled(at(x, y), r * s, color);
    match glyph {
        Glyph::Folder => {
            closed(&[
                (1.5, 3.5),
                (6.0, 3.5),
                (7.5, 5.0),
                (14.5, 5.0),
                (14.5, 13.0),
                (1.5, 13.0),
            ]);
            line(&[(1.5, 7.0), (14.5, 7.0)]);
        }
        Glyph::Recents => {
            ring(8.0, 8.0, 6.2);
            line(&[(8.0, 4.5), (8.0, 8.0), (10.5, 9.5)]);
        }
        Glyph::Applications => {
            for (x, y) in [(2.5, 2.5), (9.0, 2.5), (2.5, 9.0), (9.0, 9.0)] {
                frame(box_(x, y, x + 4.5, y + 4.5), 1.2);
            }
        }
        Glyph::Desktop => {
            frame(box_(1.5, 2.5, 14.5, 11.0), 1.5);
            line(&[(8.0, 11.0), (8.0, 13.5)]);
            line(&[(5.0, 13.5), (11.0, 13.5)]);
        }
        Glyph::Documents => {
            closed(&[
                (3.5, 1.5),
                (10.0, 1.5),
                (12.5, 4.0),
                (12.5, 14.5),
                (3.5, 14.5),
            ]);
            line(&[(10.0, 1.5), (10.0, 4.0), (12.5, 4.0)]);
            line(&[(5.5, 7.5), (10.5, 7.5)]);
            line(&[(5.5, 10.0), (10.5, 10.0)]);
        }
        Glyph::Downloads => {
            ring(8.0, 8.0, 6.2);
            line(&[(8.0, 4.5), (8.0, 11.0)]);
            line(&[(5.5, 8.5), (8.0, 11.0), (10.5, 8.5)]);
        }
        Glyph::Home => {
            line(&[(1.8, 7.8), (8.0, 2.2), (14.2, 7.8)]);
            line(&[(3.5, 6.5), (3.5, 14.0), (12.5, 14.0), (12.5, 6.5)]);
            line(&[(6.5, 14.0), (6.5, 10.0), (9.5, 10.0), (9.5, 14.0)]);
        }
        Glyph::Drive => {
            frame(box_(1.5, 5.0, 14.5, 11.0), 2.0);
            fill(11.5, 8.0, 0.9);
        }
        Glyph::Optical => {
            ring(8.0, 8.0, 6.2);
            ring(8.0, 8.0, 1.8);
        }
        Glyph::Network => {
            ring(8.0, 8.0, 6.2);
            line(&[(1.8, 8.0), (14.2, 8.0)]);
            let pts: Vec<(f32, f32)> = (0..=24)
                .map(|i| {
                    let t = i as f32 / 24.0 * std::f32::consts::TAU;
                    (8.0 + 2.8 * t.cos(), 8.0 + 6.2 * t.sin())
                })
                .collect();
            line(&pts);
        }
        Glyph::Trash => {
            line(&[(2.5, 4.0), (13.5, 4.0)]);
            line(&[(6.0, 4.0), (6.5, 2.2), (9.5, 2.2), (10.0, 4.0)]);
            line(&[(3.8, 4.0), (4.8, 14.0), (11.2, 14.0), (12.2, 4.0)]);
            line(&[(6.7, 6.5), (6.9, 11.8)]);
            line(&[(9.3, 6.5), (9.1, 11.8)]);
        }
        Glyph::Terminal => {
            frame(box_(1.5, 2.5, 14.5, 13.5), 2.0);
            line(&[(4.0, 6.0), (6.5, 8.0), (4.0, 10.0)]);
            line(&[(8.0, 10.5), (11.5, 10.5)]);
        }
        Glyph::Archive => {
            frame(box_(1.5, 2.5, 14.5, 5.5), 1.0);
            frame(box_(2.5, 5.5, 13.5, 14.0), 1.0);
            line(&[(6.5, 8.5), (9.5, 8.5)]);
        }
        Glyph::Cloud => {
            fill(5.0, 9.8, 3.0);
            fill(9.2, 7.6, 4.0);
            fill(12.2, 10.3, 2.5);
            p.rect_filled(box_(5.0, 9.8, 12.2, 12.8), 0.0, color);
        }
        Glyph::Overview => {
            line(&[(3.5, 13.5), (3.5, 8.5)]);
            line(&[(8.0, 13.5), (8.0, 3.0)]);
            line(&[(12.5, 13.5), (12.5, 6.5)]);
        }
        Glyph::Star => {
            let pts: Vec<(f32, f32)> = (0..10)
                .map(|i| {
                    let r = if i % 2 == 0 { 6.6 } else { 2.8 };
                    let t = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
                    (8.0 + r * t.cos(), 8.6 + r * t.sin())
                })
                .collect();
            closed(&pts);
        }
        Glyph::View => {
            ring(7.0, 7.0, 4.5);
            line(&[(10.3, 10.3), (14.0, 14.0)]);
        }
        Glyph::Collection => {
            frame(box_(4.5, 1.5, 14.5, 10.5), 1.5);
            frame(box_(1.5, 5.5, 11.5, 14.5), 1.5);
        }
        Glyph::Plus => {
            line(&[(8.0, 3.0), (8.0, 13.0)]);
            line(&[(3.0, 8.0), (13.0, 8.0)]);
        }
        Glyph::Eject => {
            let tri = [at(8.0, 3.0), at(13.0, 9.0), at(3.0, 9.0)].to_vec();
            p.add(Shape::convex_polygon(tri, color, Stroke::NONE));
            p.rect_filled(box_(3.0, 11.0, 13.0, 13.0), 0.5 * s, color);
        }
        Glyph::Dot(c) => {
            p.circle_filled(rect.center(), 4.5 * s, c);
        }
    }
}

/// A square of side `size` centred vertically in `row`, starting at `left`.
pub fn slot(row: Rect, left: f32, size: f32) -> Rect {
    Rect::from_min_size(pos2(left, row.center().y - size / 2.0), vec2(size, size))
}
