//! Color themes: TOML files mapped onto `egui::Visuals`. Built-ins (`assets/themes`), the
//! user's `<config dir>/themes/*.toml` (any name; a built-in's name overrides it) and the
//! VS Code color themes installed there (`color_theme`).

use crate::settings::config_dir;
use egui::Color32;

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: String,
    pub dark: bool,
    pub bg: [u8; 3],
    pub panel: [u8; 3],
    pub text: [u8; 3],
    pub accent: [u8; 3],
    pub selection: [u8; 3],
    pub muted: [u8; 3],
    /// Warning and error text (absent: Keel's defaults for a dark or light theme).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn: Option<[u8; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<[u8; 3]>,
    /// Text or muted colors were moved toward legibility (a mapped VS Code theme).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adjusted: bool,
}

pub(crate) const BUILTIN: &[(&str, &str)] = &[
    ("dark", include_str!("../../../assets/themes/dark.toml")),
    ("light", include_str!("../../../assets/themes/light.toml")),
    // --- Task 47 ---
    (
        "midnight",
        include_str!("../../../assets/themes/midnight.toml"),
    ),
    ("paper", include_str!("../../../assets/themes/paper.toml")),
    ("dusk", include_str!("../../../assets/themes/dusk.toml")),
    (
        "high-contrast",
        include_str!("../../../assets/themes/high-contrast.toml"),
    ),
];

pub fn rgb([r, g, b]: [u8; 3]) -> Color32 {
    Color32::from_rgb(r, g, b)
}

impl Theme {
    /// User file `<config dir>/themes/<name>.toml` wins over the built-in; unknown or
    /// broken themes fall back to built-in dark.
    pub fn load(name: &str) -> Theme {
        let user = config_dir()
            .map(|d| d.join("themes").join(format!("{name}.toml")))
            .and_then(|p| std::fs::read_to_string(p).ok());
        let builtin = BUILTIN.iter().find(|(n, _)| *n == name).map(|(_, s)| *s);
        user.as_deref()
            .into_iter()
            .chain(builtin)
            .find_map(|s| {
                toml::from_str(s)
                    .map_err(|e| tracing::warn!("theme {name}: {e}"))
                    .ok()
            })
            .unwrap_or_else(|| toml::from_str(BUILTIN[0].1).expect("built-in dark theme parses"))
    }

    pub fn muted(&self) -> Color32 {
        rgb(self.muted)
    }

    pub fn accent(&self) -> Color32 {
        rgb(self.accent)
    }

    pub fn apply(&self, ctx: &egui::Context) {
        let (theme, mut v) = if self.dark {
            (egui::Theme::Dark, egui::Visuals::dark())
        } else {
            (egui::Theme::Light, egui::Visuals::light())
        };
        let (bg, panel, text) = (rgb(self.bg), rgb(self.panel), rgb(self.text));
        v.panel_fill = panel;
        v.window_fill = panel;
        v.extreme_bg_color = bg;
        v.faint_bg_color = panel.lerp_to_gamma(text, 0.04);
        v.override_text_color = Some(text);
        v.selection.bg_fill = rgb(self.selection);
        v.hyperlink_color = self.accent();
        v.widgets.noninteractive.bg_fill = panel;
        // Contrast (WCAG 4.5:1 on panel, background and stripes; the theme tests check it).
        // Weak text (`ui.weak`, disabled widgets) is the muted colour: egui mixes the text
        // colour half and half with this fill.
        let (muted, mix) = (self.muted(), |t: u8, m: u8| {
            (2 * m as i32 - t as i32).clamp(0, 255) as u8
        });
        v.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(
            mix(text.r(), muted.r()),
            mix(text.g(), muted.g()),
            mix(text.b(), muted.b()),
        );
        let (warn, error) = if self.dark {
            ([255, 143, 0], [255, 107, 107])
        } else {
            ([155, 80, 0], [190, 0, 0])
        };
        v.warn_fg_color = rgb(self.warn.unwrap_or(warn));
        v.error_fg_color = rgb(self.error.unwrap_or(error));
        ctx.set_theme(if self.dark {
            egui::ThemePreference::Dark
        } else {
            egui::ThemePreference::Light
        });
        // Set for the active theme so an OS light/dark switch cannot undo it.
        ctx.set_visuals_of(theme, v);
        crate::icons::set_light(!self.dark);
    }
}

/// Every theme the app can switch to, read once at startup (the user's theme files are
/// config reads): switching themes later never touches the disk on the UI thread.
pub struct Themes(Vec<(String, Theme)>);

impl Themes {
    /// The built-ins, then every `<config dir>/themes/*.toml` by file name.
    pub fn load() -> Self {
        let mut names: Vec<String> = BUILTIN.iter().map(|(n, _)| (*n).to_owned()).collect();
        let dir = config_dir().map(|d| d.join("themes"));
        let mut user: Vec<String> = dir
            .and_then(|d| std::fs::read_dir(d).ok())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                name.strip_suffix(".toml").map(str::to_owned)
            })
            .filter(|n| !n.is_empty() && !names.contains(n))
            .collect();
        user.sort_by_key(|n| n.to_lowercase());
        names.extend(user);
        Self(
            names
                .into_iter()
                .map(|n| {
                    let theme = Theme::load(&n);
                    (n, theme)
                })
                .collect(),
        )
    }

    /// (settings name, label) of every theme, for the picker.
    pub fn list(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .map(|(n, t)| {
                let label = if t.adjusted {
                    format!("{} (adjusted for contrast)", t.name)
                } else {
                    t.name.clone()
                };
                (n.clone(), label)
            })
            .collect()
    }

    /// The theme loaded under `name` (the settings value), else the first (dark).
    pub fn get(&self, name: &str) -> Theme {
        let found = self.0.iter().find(|(n, _)| n == name);
        found.unwrap_or(&self.0[0]).1.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{Theme, Themes};

    /// WCAG 2.x relative luminance of an sRGB colour.
    fn luminance(c: egui::Color32) -> f64 {
        let lin = |v: u8| {
            let s = v as f64 / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    /// WCAG contrast ratio of two colours (1 to 21).
    fn ratio(a: egui::Color32, b: egui::Color32) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Every non-ASCII character in a string or char literal of the non-test code under
    /// `src` (test modules, `*_tests.rs` and the screenshot harness are skipped), with a
    /// file it is in.
    fn ui_symbols(src: std::path::PathBuf) -> std::collections::BTreeMap<char, String> {
        let mut found = std::collections::BTreeMap::new();
        let mut dirs = vec![src];
        while let Some(dir) = dirs.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = e.path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy();
                if !name.ends_with(".rs") || name.ends_with("_tests.rs") || name == "screenshots.rs"
                {
                    continue;
                }
                let text = std::fs::read_to_string(&path)
                    .unwrap()
                    .replace("\r\n", "\n");
                let code = text.split("\n#[cfg(test)]\nmod tests").next().unwrap();
                let s: Vec<char> = code.chars().collect();
                let mut add = |c: char| {
                    found.entry(c).or_insert_with(|| path.display().to_string());
                };
                let mut i = 0;
                while i < s.len() {
                    match s[i] {
                        '/' if s.get(i + 1) == Some(&'/') => {
                            while i < s.len() && s[i] != '\n' {
                                i += 1;
                            }
                        }
                        '"' => {
                            i += 1;
                            while i < s.len() && s[i] != '"' {
                                if !s[i].is_ascii() {
                                    add(s[i]);
                                }
                                i += if s[i] == '\\' { 2 } else { 1 };
                            }
                        }
                        // Char literals ('"' must not open a string); lifetimes pass.
                        '\'' if s.get(i + 1) == Some(&'\\') => {
                            i += 3;
                            while i < s.len() && s[i] != '\'' {
                                i += 1;
                            }
                        }
                        '\'' if s.get(i + 2) == Some(&'\'') => {
                            if !s[i + 1].is_ascii() {
                                add(s[i + 1]);
                            }
                            i += 2;
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
        }
        found
    }

    /// Found making the screenshots: the arrow drew as a box. Every symbol in the strings
    /// of every crate has a glyph in the proportional and the monospace fonts: errors and
    /// notes from the other crates reach the window too, and the web client (same fonts)
    /// shows the daemon's.
    #[test]
    fn every_ui_symbol_has_a_glyph() {
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::fonts());
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut scanned = 0;
        for e in std::fs::read_dir(crates).unwrap().flatten() {
            let src = e.path().join("src");
            if !src.is_dir() {
                continue;
            }
            scanned += 1;
            let symbols = ui_symbols(src);
            if e.file_name() == "keel-app" {
                for c in ['\u{2192}', '\u{2026}', '\u{d7}', '\u{2022}', '\u{22ef}'] {
                    assert!(
                        symbols.contains_key(&c),
                        "{c} not found: the scan is broken"
                    );
                }
            }
            for (c, file) in symbols {
                for font in [
                    egui::FontId::proportional(14.0),
                    egui::FontId::monospace(14.0),
                ] {
                    assert!(
                        ctx.fonts(|f| f.has_glyph(&font, c)),
                        "{c:?} (U+{:04X}, {file}) has no glyph in {font:?}",
                        c as u32
                    );
                }
            }
        }
        assert!(scanned > 5, "only {scanned} crates found");
    }

    #[test]
    fn contrast_ratio_matches_the_wcag_examples() {
        use egui::Color32;
        assert!((ratio(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 1e-9);
        assert!((ratio(Color32::WHITE, Color32::WHITE) - 1.0).abs() < 1e-9);
        // #767676 on white is the classic 4.54:1.
        assert!((ratio(Color32::from_gray(0x76), Color32::WHITE) - 4.54).abs() < 0.01);
    }

    /// Every built-in theme: text, secondary (muted, weak) text, text on the selection,
    /// links / accents, warnings and errors reach 4.5:1 on the backgrounds they are drawn
    /// on; the focus ring 3:1 (a UI component).
    #[test]
    fn builtin_themes_meet_wcag_contrast() {
        let mut failures = Vec::new();
        for name in super::BUILTIN.iter().map(|(n, _)| *n) {
            let theme = Theme::load(name);
            let ctx = egui::Context::default();
            theme.apply(&ctx);
            let v = ctx.style().visuals.clone();
            let text = v.text_color();
            let backs = [
                ("panel", v.panel_fill),
                ("bg", v.extreme_bg_color),
                ("stripe", v.faint_bg_color),
            ];
            let mut check = |what: &str, fg, on: &str, bg, min: f64| {
                let r = ratio(fg, bg);
                if r < min {
                    failures.push(format!(
                        "{name}: {what} on {on} {r:.2} < {min} ({fg:?} on {bg:?})"
                    ));
                }
            };
            for (on, bg) in backs {
                check("text", text, on, bg, 4.5);
                check("muted", theme.muted(), on, bg, 4.5);
                check("weak text", v.weak_text_color(), on, bg, 4.5);
                check("accent", theme.accent(), on, bg, 4.5);
                check("warning", v.warn_fg_color, on, bg, 4.5);
                check("error", v.error_fg_color, on, bg, 4.5);
                check("focus ring", crate::a11y::ring_color(&v), on, bg, 3.0);
            }
            let sel = v.selection.bg_fill;
            check("text", text, "selection", sel, 4.5);
            // Selected rows draw their secondary text in the text colour (views).
            check(
                "focus ring",
                crate::a11y::ring_color(&v),
                "selection",
                sel,
                3.0,
            );
        }
        assert!(
            failures.is_empty(),
            "{}",
            failures.join(
                "
"
            )
        );
    }

    #[test]
    fn themes_are_cached_once() {
        let themes = Themes::load();

        assert!(!themes.get("light").dark);
        assert!(themes.get("dark").dark);
        assert_eq!(themes.get("unknown").name, "dark");
    }

    /// Task 47: any `<config>/themes/*.toml` is listed by its file name, with its own label;
    /// a broken file falls back to dark.
    #[test]
    fn user_theme_files_are_listed_by_name() {
        let _env = crate::settings::TEST_ENV.lock();
        let was = std::env::var_os("KEEL_CONFIG_DIR");
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("KEEL_CONFIG_DIR", dir.path());
        let themes_dir = dir.path().join("themes");
        std::fs::create_dir(&themes_dir).unwrap();
        let mut ocean = Theme::load("midnight");
        ocean.name = "Ocean".into();
        ocean.adjusted = true;
        ocean.bg = [1, 2, 3];
        std::fs::write(
            themes_dir.join("ocean.toml"),
            toml::to_string(&ocean).unwrap(),
        )
        .unwrap();
        std::fs::write(themes_dir.join("broken.toml"), "name = [").unwrap();
        let themes = Themes::load();
        match was {
            Some(v) => std::env::set_var("KEEL_CONFIG_DIR", v),
            None => std::env::remove_var("KEEL_CONFIG_DIR"),
        }
        let list = themes.list();
        assert_eq!(list.len(), super::BUILTIN.len() + 2, "{list:?}");
        assert!(list.contains(&("ocean".into(), "Ocean (adjusted for contrast)".into())));
        assert_eq!(themes.get("ocean"), ocean);
        assert_eq!(themes.get("broken").name, "dark");
        assert_eq!(themes.get("high-contrast").name, "High contrast");
    }

    #[test]
    fn builtin_themes_load() {
        assert!(Theme::load("dark").dark);
        assert!(!Theme::load("light").dark);
        assert_eq!(Theme::load("no-such-theme").name, "dark");
    }
}
