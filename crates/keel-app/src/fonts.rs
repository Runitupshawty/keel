//! The fonts of the window and of the web client (keel-web includes this file by path).

/// egui's built-in fonts, with Hack (already built in, the monospace font) as the last
/// fallback of proportional text too: Ubuntu Light has no arrows ("Settings \u{2192}
/// Library", "Copy A \u{2192} B", "\u{2190} Back") and no "\u{22ef}" or "\u{25cf}".
pub fn fonts() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .push("Hack".into());
    fonts
}
