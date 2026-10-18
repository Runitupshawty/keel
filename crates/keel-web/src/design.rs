//! Finder design tokens for the web client. Mirrors crates/keel-app/src/design.rs
//! (`FINDER` tokens and `Palette::finder`); keep the two in step.

use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, TextStyle, Visuals};

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// macOS system blue, the default accent.
pub const ACCENT: Color32 = hex(0x007aff);

/// (window, sidebar, content, alternating_row, separator, label, secondary_label)
pub fn colors(dark: bool) -> [Color32; 7] {
    if dark {
        [
            hex(0x323232),
            hex(0x282828),
            hex(0x1e1e1e),
            hex(0x262626),
            hex(0x3d3d3d),
            hex(0xe5e5e5),
            hex(0xa5a5aa),
        ]
    } else {
        [
            hex(0xececec),
            hex(0xe6e6e6),
            hex(0xffffff),
            hex(0xf4f5f5),
            hex(0xd5d5d5),
            hex(0x1f1f1f),
            hex(0x646469),
        ]
    }
}

/// Finder spacing, type scale and radii onto `base`, colors for its light/dark mode.
pub fn finder_style(base: &egui::Style) -> egui::Style {
    let mut s = base.clone();
    s.spacing.item_spacing = egui::vec2(8.0, 4.0);
    s.spacing.button_padding = egui::vec2(8.0, 3.0);
    s.spacing.interact_size.y = 20.0;
    s.spacing.window_margin = Margin::same(12);
    s.spacing.menu_margin = Margin::same(6);
    s.text_styles.insert(
        TextStyle::Small,
        FontId::new(11.0, FontFamily::Proportional),
    );
    s.text_styles
        .insert(TextStyle::Body, FontId::new(13.0, FontFamily::Proportional));
    s.text_styles.insert(
        TextStyle::Button,
        FontId::new(13.0, FontFamily::Proportional),
    );
    s.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(12.0, FontFamily::Monospace),
    );
    s.text_styles.insert(
        TextStyle::Heading,
        FontId::new(15.0, FontFamily::Proportional),
    );
    finder_visuals(&mut s.visuals);
    s
}

/// Finder colors and radii onto `v`, keeping its light/dark mode.
pub fn finder_visuals(v: &mut Visuals) {
    let [window, sidebar, content, alt, sep, label, secondary] = colors(v.dark_mode);
    // egui's visuals have no sidebar fill or weak-text field (weak text is derived from
    // the label color); the web app paints its own sidebar and secondary text.
    let _ = (sidebar, secondary);
    v.panel_fill = window;
    v.window_fill = content;
    v.extreme_bg_color = content;
    v.faint_bg_color = alt;
    v.widgets.noninteractive.bg_stroke.color = sep;
    v.widgets.noninteractive.fg_stroke.color = label;
    v.override_text_color = None;
    v.selection.bg_fill = content.lerp_to_gamma(ACCENT, if v.dark_mode { 0.4 } else { 0.22 });
    v.selection.stroke.color = ACCENT;
    v.hyperlink_color = ACCENT;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.active,
        &mut v.widgets.open,
        &mut v.widgets.hovered,
    ] {
        w.corner_radius = CornerRadius::same(6);
    }
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_colors() {
        assert_eq!(colors(false)[2], Color32::WHITE);
        assert_eq!(colors(true)[0], Color32::from_rgb(0x32, 0x32, 0x32));
    }

    #[test]
    fn finder_style_tokens() {
        let s = finder_style(&egui::Style::default());
        assert_eq!(s.spacing.interact_size.y, 20.0);
        assert_eq!(s.text_styles[&TextStyle::Body].size, 13.0);
        assert_eq!(s.visuals.window_corner_radius, CornerRadius::same(10));
    }
}
