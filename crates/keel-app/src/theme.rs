//! Color themes: TOML files mapped onto `egui::Visuals`.

use egui::Color32;
use std::path::PathBuf;

#[derive(serde::Deserialize, Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub dark: bool,
    pub bg: [u8; 3],
    pub panel: [u8; 3],
    pub text: [u8; 3],
    pub accent: [u8; 3],
    pub selection: [u8; 3],
    pub muted: [u8; 3],
}

const BUILTIN: &[(&str, &str)] = &[
    ("dark", include_str!("../../../assets/themes/dark.toml")),
    ("light", include_str!("../../../assets/themes/light.toml")),
];

/// `%APPDATA%\Keel`, `~/Library/Application Support/Keel`, `~/.config/keel` (spec 2.9).
pub fn config_dir() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    Some(base.config_dir().join(if cfg!(target_os = "linux") {
        "keel"
    } else {
        "Keel"
    }))
}

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
        ctx.set_theme(if self.dark {
            egui::ThemePreference::Dark
        } else {
            egui::ThemePreference::Light
        });
        // Set for the active theme so an OS light/dark switch cannot undo it.
        ctx.set_visuals_of(theme, v);
    }
}

#[cfg(test)]
mod tests {
    use super::Theme;

    #[test]
    fn builtin_themes_load() {
        assert!(Theme::load("dark").dark);
        assert!(!Theme::load("light").dark);
        assert_eq!(Theme::load("no-such-theme").name, "dark");
    }
}
