//! Font files (TrueType, OpenType and collections of them): a sample sentence drawn in the
//! font with `ab_glyph` (egui's rasterizer), and the family and style from its name table
//! (`ttf-parser`, which `ab_glyph` already parses fonts with). A collection shows its first
//! font. woff and woff2 are not read (woff2 needs a brotli decoder).

use crate::{Preview, Rgba};
use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};

const FONT_EXTENSIONS: &[&str] = &["ttf", "otf", "ttc", "otc"];
pub const SAMPLE: &str = "The quick brown fox jumps over the lazy dog 0123456789";
/// Pixel sizes of the sample lines at a 480 px wide preview (scaled with the width).
const SIZES: [f32; 4] = [14.0, 20.0, 32.0, 48.0];
const PAD: f32 = 12.0;
/// The sample's width range and its height cap (px).
const MIN_W: u32 = 240;
const MAX_W: u32 = 1600;
const MAX_H: u32 = 2400;
const INK: [u8; 3] = [24, 24, 24];

pub(crate) fn accepts(ext: &str) -> bool {
    FONT_EXTENSIONS.contains(&ext)
}

pub(crate) fn render(req: &crate::Request) -> Preview {
    match std::fs::read(&req.bytes_path) {
        Ok(data) => sample(&data, req.max_px).unwrap_or_else(Preview::Error),
        Err(e) => Preview::Error(e.to_string()),
    }
}

/// The preview of a font file's bytes, the sample `max_px` wide (within 240..=1600).
pub(crate) fn sample(data: &[u8], max_px: u32) -> Result<Preview, String> {
    let face = ttf_parser::Face::parse(data, 0).map_err(|e| format!("not a font ({e})"))?;
    let font = FontRef::try_from_slice_and_index(data, 0).map_err(|e| e.to_string())?;
    let name = |ids: &[u16]| {
        ids.iter().find_map(|id| {
            let names = face.names();
            let of_id = || names.into_iter().filter(|n| n.name_id == *id);
            of_id().find_map(|n| n.to_string()).or_else(|| {
                // Old fonts may only have Macintosh (Roman) names: keep their ASCII.
                of_id()
                    .find(|n| n.platform_id == ttf_parser::PlatformId::Macintosh)
                    .map(|n| n.name.iter().map(|&b| b as char).collect::<String>())
                    .filter(|s| !s.is_empty() && s.is_ascii())
            })
        })
    };
    use ttf_parser::name_id::{FAMILY, SUBFAMILY, TYPOGRAPHIC_FAMILY, TYPOGRAPHIC_SUBFAMILY};
    let family = name(&[TYPOGRAPHIC_FAMILY, FAMILY]).unwrap_or_else(|| "Unnamed font".into());
    let style = name(&[TYPOGRAPHIC_SUBFAMILY, SUBFAMILY]).unwrap_or_default();
    let mut info = vec![
        if face.tables().cff.is_some() || face.tables().cff2.is_some() {
            "OpenType (CFF)".to_owned()
        } else {
            "TrueType".to_owned()
        },
        format!("{} glyphs", face.number_of_glyphs()),
    ];
    let faces = ttf_parser::fonts_in_collection(data).unwrap_or(1);
    if faces > 1 {
        info.push(format!("first of {faces} fonts"));
    }
    Ok(Preview::Font {
        sample: draw(&font, max_px.clamp(MIN_W, MAX_W))?,
        name: format!("{family} {style}").trim().to_owned(),
        info: info.join(" · "),
    })
}

/// SAMPLE at each of SIZES (scaled to `width`), word-wrapped, dark on white.
fn draw(font: &FontRef, width: u32) -> Result<Rgba, String> {
    let k = (width as f32 / 480.0).clamp(0.6, 2.5);
    let room = width as f32 - 2.0 * PAD;
    // (size, lines) for every size; the height follows from them.
    let mut blocks = Vec::new();
    for px in SIZES.map(|s| s * k) {
        let scaled = font.as_scaled(PxScale::from(px));
        let line_h = scaled.height() + scaled.line_gap();
        if !scaled.height().is_finite() || scaled.height() <= 0.0 {
            return Err("the font has no line height".into());
        }
        let line_h = if line_h.is_finite() {
            line_h.clamp(px, 3.0 * px)
        } else {
            1.3 * px
        };
        blocks.push((px, line_h, wrap(&scaled, room)));
    }
    let height: f32 = blocks
        .iter()
        .map(|(px, line_h, lines)| line_h * lines.len() as f32 + px * 0.5)
        .sum::<f32>()
        + 2.0 * PAD;
    let h = (height.ceil() as u32).clamp(1, MAX_H);
    let mut data = vec![255u8; width as usize * h as usize * 4];
    let mut top = PAD;
    for (px, line_h, lines) in blocks {
        let scaled = font.as_scaled(PxScale::from(px));
        for line in lines {
            let baseline = top + scaled.ascent().clamp(0.0, line_h);
            let mut x = PAD;
            let mut prev = None;
            for c in line.chars() {
                let id = scaled.glyph_id(c);
                if let Some(p) = prev {
                    x += finite(scaled.kern(p, id));
                }
                prev = Some(id);
                let glyph = id.with_scale_and_position(px, point(x, baseline));
                x += finite(scaled.h_advance(id));
                let Some(outlined) = font.outline_glyph(glyph) else {
                    continue;
                };
                let b = outlined.px_bounds();
                // The rasterizer allocates the glyph's box: a font that claims a huge or
                // broken one is skipped, never drawn.
                let sane = |v: f32| v.is_finite() && v.abs() < 8.0 * px + width as f32;
                if ![b.min.x, b.min.y, b.max.x, b.max.y].into_iter().all(sane)
                    || b.width() > 4.0 * px
                    || b.height() > 4.0 * px
                {
                    continue;
                }
                outlined.draw(|gx, gy, cover| {
                    let (x, y) = (
                        b.min.x as i64 + i64::from(gx),
                        b.min.y as i64 + i64::from(gy),
                    );
                    if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(h) {
                        return;
                    }
                    let i = (y as usize * width as usize + x as usize) * 4;
                    let a = cover.clamp(0.0, 1.0);
                    for (c, ink) in data[i..i + 3].iter_mut().zip(INK) {
                        *c = (f32::from(*c) * (1.0 - a) + f32::from(ink) * a) as u8;
                    }
                });
            }
            top += line_h;
        }
        top += px * 0.5;
    }
    Ok(Rgba { w: width, h, data })
}

fn finite(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// SAMPLE broken into lines at most `room` px wide (a word wider than that has a line).
fn wrap(scaled: &ab_glyph::PxScaleFont<&FontRef<'_>>, room: f32) -> Vec<String> {
    let width = |s: &str| {
        let (mut w, mut prev) = (0.0, None);
        for c in s.chars() {
            let id = scaled.glyph_id(c);
            if let Some(p) = prev {
                w += finite(scaled.kern(p, id));
            }
            w += finite(scaled.h_advance(id));
            prev = Some(id);
        }
        w
    };
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in SAMPLE.split(' ') {
        let longer = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if width(&longer) > room && !line.is_empty() {
            lines.push(std::mem::replace(&mut line, word.to_owned()));
        } else {
            line = longer;
        }
    }
    lines.push(line);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// egui's bundled Ubuntu Light (its license is in THIRD_PARTY.md, with egui's fonts).
    #[test]
    fn a_sample_in_the_font_with_its_names() {
        let Ok(Preview::Font { sample, name, info }) =
            super::sample(epaint_default_fonts::UBUNTU_LIGHT, 480)
        else {
            panic!("expected a font preview");
        };
        assert_eq!(name, "Ubuntu Light");
        assert!(info.starts_with("TrueType · "), "{info}");
        assert_eq!(sample.w, 480);
        assert!(sample.h > 100 && sample.h <= MAX_H, "{}", sample.h);
        assert_eq!(sample.data.len(), 480 * sample.h as usize * 4);
        let inked = sample
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] < 128)
            .count();
        assert!(inked > 1000, "the sample is drawn: {inked} dark pixels");
        // Narrow and wide requests stay within the width range.
        for (asked, w) in [(10, MIN_W), (100_000, MAX_W)] {
            let Ok(Preview::Font { sample, .. }) =
                super::sample(epaint_default_fonts::HACK_REGULAR, asked)
            else {
                panic!("expected a font preview");
            };
            assert_eq!(sample.w, w);
        }
    }

    /// The `font` fuzz seed (`scripts/make-test-font.py`): a square for every letter.
    #[test]
    fn the_synthetic_test_font() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fuzz/corpus/font/keel-test.ttf"
        );
        let data = std::fs::read(path).unwrap();
        let Ok(Preview::Font { sample, name, info }) = super::sample(&data, 300) else {
            panic!("expected a font preview");
        };
        assert_eq!(
            (name.as_str(), info.as_str()),
            ("Keel Test Regular", "TrueType · 3 glyphs")
        );
        let inked = sample
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] < 128)
            .count();
        assert!(inked > 1000, "{inked}");
    }

    #[test]
    fn non_fonts_are_errors() {
        for data in [&b""[..], b"not a font at all", &[0, 1, 0, 0, 0, 9, 0, 0]] {
            assert!(super::sample(data, 480).is_err());
        }
    }
}
