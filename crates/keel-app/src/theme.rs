//! Color themes: TOML files mapped onto `egui::Visuals`.

use crate::settings::config_dir;
use egui::Color32;

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
        crate::icons::set_light(!self.dark);
    }
}

/// Every theme the app can switch to, read once at startup (the user's theme files are
/// config reads): switching themes later never touches the disk on the UI thread.
pub struct Themes(Vec<(String, Theme)>);

impl Themes {
    /// The built-ins plus `current` (a user theme named in the settings).
    pub fn load(current: &str) -> Self {
        let mut names: Vec<&str> = BUILTIN.iter().map(|(n, _)| *n).collect();
        if !names.contains(&current) {
            names.push(current);
        }
        Self(
            names
                .into_iter()
                .map(|n| (n.to_owned(), Theme::load(n)))
                .collect(),
        )
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

    #[test]
    fn themes_are_cached_once() {
        let themes = Themes::load("no-such-theme");
        assert!(!themes.get("light").dark);
        assert!(themes.get("dark").dark);
        assert_eq!(themes.get("unknown").name, "dark");
    }

    #[test]
    fn builtin_themes_load() {
        assert!(Theme::load("dark").dark);
        assert!(!Theme::load("light").dark);
        assert_eq!(Theme::load("no-such-theme").name, "dark");
    }
}
