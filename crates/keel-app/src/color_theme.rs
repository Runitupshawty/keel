//! VS Code color themes (Task 47): a Marketplace package's `contributes.themes` mapped onto
//! Keel's six colors and written as Keel theme files, `<config>/themes/<id>~<n>.toml`, so
//! the theme picker lists them beside the built-ins and the user's own files. Only JSON is
//! read (with comments and trailing commas, `include` followed at most `MAX_INCLUDES`
//! levels inside the package); nothing runs and nothing is fetched beyond the package.
//! Downloads, the license dialog and the size caps are the icon themes' (`icon_theme`).

use crate::icon_theme::{self, split_id};
use crate::theme::{ratio, rgb, Theme};
use crate::toast::Toasts;
use anyhow::{ensure, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use egui::Color32;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// `include` levels followed below a theme file.
const MAX_INCLUDES: usize = 4;
/// Themes taken from one package.
const MAX_THEMES: usize = 64;
/// Text, muted text and the accent reach this contrast on the background and the panels.
const MIN_CONTRAST: f64 = 4.5;

/// `text` with `//` and `/* */` comments and trailing commas removed (outside strings), as
/// VS Code's JSON with comments allows.
pub fn strip_jsonc(text: &str) -> String {
    let s: Vec<char> = text.trim_start_matches('\u{feff}').chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            '"' => {
                out.push('"');
                i += 1;
                while i < s.len() && s[i] != '"' {
                    if s[i] == '\\' && i + 1 < s.len() {
                        out.push(s[i]);
                        i += 1;
                    }
                    out.push(s[i]);
                    i += 1;
                }
                if i < s.len() {
                    out.push('"');
                }
            }
            '/' if s.get(i + 1) == Some(&'/') => {
                while i < s.len() && s[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if s.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < s.len() && !(s[i] == '*' && s.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
                continue;
            }
            ',' => {
                // A comma followed (after blanks and comments, already gone or still to
                // come) by a closing bracket is dropped.
                let mut j = i + 1;
                loop {
                    while j < s.len() && s[j].is_whitespace() {
                        j += 1;
                    }
                    if s.get(j) == Some(&'/') && s.get(j + 1) == Some(&'/') {
                        while j < s.len() && s[j] != '\n' {
                            j += 1;
                        }
                    } else if s.get(j) == Some(&'/') && s.get(j + 1) == Some(&'*') {
                        j += 2;
                        while j < s.len() && !(s[j] == '*' && s.get(j + 1) == Some(&'/')) {
                            j += 1;
                        }
                        j += 2;
                    } else {
                        break;
                    }
                }
                if !matches!(s.get(j), Some('}' | ']')) {
                    out.push(',');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`.
pub fn parse_color(s: &str) -> Option<[u8; 4]> {
    let hex = s.trim().strip_prefix('#')?;
    if !hex.is_ascii() {
        return None;
    }
    let digit = |i: usize, n: usize| u8::from_str_radix(&hex[i..i + n], 16).ok();
    let short = |i: usize| digit(i, 1).map(|v| v * 17);
    match hex.len() {
        3 => Some([short(0)?, short(1)?, short(2)?, 255]),
        4 => Some([short(0)?, short(1)?, short(2)?, short(3)?]),
        6 => Some([digit(0, 2)?, digit(2, 2)?, digit(4, 2)?, 255]),
        8 => Some([digit(0, 2)?, digit(2, 2)?, digit(4, 2)?, digit(6, 2)?]),
        _ => None,
    }
}

/// `c` over `under` (alpha flattened).
fn flatten([r, g, b, a]: [u8; 4], under: [u8; 3]) -> [u8; 3] {
    let mix = |c: u8, u: u8| ((c as u32 * a as u32 + u as u32 * (255 - a as u32)) / 255) as u8;
    [mix(r, under[0]), mix(g, under[1]), mix(b, under[2])]
}

/// The `colors` of the theme file at `path` in the package (`read` gives a file's bytes),
/// over those of the files it includes (followed at most `MAX_INCLUDES` levels; a cycle is
/// refused).
pub fn theme_colors(
    read: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
    path: &str,
) -> Result<HashMap<String, String>> {
    fn walk(
        read: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
        path: &str,
        chain: &mut Vec<String>,
    ) -> Result<HashMap<String, String>> {
        ensure!(!chain.contains(&path.to_owned()), "{path} includes itself");
        ensure!(
            chain.len() <= MAX_INCLUDES,
            "{path}: includes nest more than {MAX_INCLUDES} deep"
        );
        let text = String::from_utf8_lossy(&read(path)?).into_owned();
        let json: serde_json::Value =
            serde_json::from_str(&strip_jsonc(&text)).with_context(|| path.to_owned())?;
        let mut colors = match json["include"].as_str() {
            Some(rel) => {
                let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
                let parent = icon_theme::zip_join(dir, rel)
                    .with_context(|| format!("{path}: include {rel} is outside the package"))?;
                chain.push(path.to_owned());
                let up = walk(read, &parent, chain)?;
                chain.pop();
                up
            }
            None => HashMap::new(),
        };
        if let Some(own) = json["colors"].as_object() {
            for (k, v) in own {
                if let Some(v) = v.as_str() {
                    colors.insert(k.clone(), v.to_owned());
                }
            }
        }
        Ok(colors)
    }
    walk(read, path, &mut Vec::new())
}

/// Keel's colors from a VS Code theme's `colors` (`light`: `uiTheme` is `vs`): the first key
/// present per color, else the built-in dark or light one; alpha flattened against the
/// background. Text, muted text and the accent (links) under 4.5:1 on the background or
/// panels are moved toward legibility (`Theme::adjusted`).
pub fn map(name: &str, colors: &HashMap<String, String>, light: bool) -> Theme {
    let base = Theme::load(if light { "light" } else { "dark" });
    let get = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| colors.get(*k).and_then(|c| parse_color(c)))
    };
    let backdrop = if light { [255; 3] } else { [0; 3] };
    let bg = get(&["editor.background"]).map_or(base.bg, |c| flatten(c, backdrop));
    let pick = |keys: &[&str], fallback: [u8; 3]| get(keys).map_or(fallback, |c| flatten(c, bg));
    let mut theme = Theme {
        name: name.to_owned(),
        dark: !light,
        bg,
        panel: pick(
            &[
                "sideBarSectionHeader.background",
                "sideBar.background",
                "panel.background",
            ],
            base.panel,
        ),
        text: pick(&["foreground", "editor.foreground"], base.text),
        accent: pick(
            &["focusBorder", "button.background", "textLink.foreground"],
            base.accent,
        ),
        selection: pick(&["list.activeSelectionBackground"], base.selection),
        muted: pick(&["descriptionForeground", "disabledForeground"], base.muted),
        warn: None,
        error: None,
        adjusted: false,
    };
    let (bg, panel) = (rgb(theme.bg), rgb(theme.panel));
    for c in [&mut theme.text, &mut theme.muted, &mut theme.accent] {
        let fixed = legible(rgb(*c), &[bg, panel]);
        if fixed != rgb(*c) {
            *c = [fixed.r(), fixed.g(), fixed.b()];
            theme.adjusted = true;
        }
    }
    theme
}

/// `c`, or `c` moved step by step toward white or black (whichever contrasts more with the
/// backgrounds) until it reaches `MIN_CONTRAST` on each of `backs`.
fn legible(c: Color32, backs: &[Color32]) -> Color32 {
    let worst = |c: Color32| backs.iter().map(|b| ratio(c, *b)).fold(f64::MAX, f64::min);
    if worst(c) >= MIN_CONTRAST {
        return c;
    }
    let toward = [Color32::WHITE, Color32::BLACK]
        .into_iter()
        .max_by(|a, b| worst(*a).total_cmp(&worst(*b)))
        .expect("two");
    (1..=20)
        .map(|step| c.lerp_to_gamma(toward, step as f32 / 20.0))
        .find(|m| worst(*m) >= MIN_CONTRAST)
        .unwrap_or(toward)
}

/// A downloaded color theme package, waiting for the license to be accepted.
pub struct ColorVsix {
    pub id: String,
    pub label: String,
    pub license: String,
    /// Each theme the package contributes, mapped.
    pub themes: Vec<Theme>,
}

/// Reads `contributes.themes` and the license of a package; every theme is mapped here, so
/// a broken one is refused before the license is shown.
pub fn inspect(id: &str, bytes: Vec<u8>) -> Result<ColorVsix> {
    let pkg = icon_theme::Package::open(id, bytes)?;
    let list = pkg
        .json
        .pointer("/contributes/themes")
        .and_then(|t| t.as_array())
        .filter(|t| !t.is_empty())
        .context("this extension contributes no color theme")?
        .clone();
    let mut themes = Vec::new();
    for t in list.iter().take(MAX_THEMES) {
        let path = t["path"].as_str().context("a color theme has no path")?;
        let entry = icon_theme::zip_join("extension", path).context("bad color theme path")?;
        let colors = theme_colors(&mut |p| pkg.entry(p), &entry)?;
        let name = t["label"]
            .as_str()
            .filter(|l| !l.starts_with('%'))
            .or_else(|| t["id"].as_str())
            .unwrap_or(&pkg.label);
        themes.push(map(name, &colors, t["uiTheme"].as_str() == Some("vs")));
    }
    Ok(ColorVsix {
        id: id.to_owned(),
        label: pkg.label.clone(),
        license: pkg.license.clone(),
        themes,
    })
}

/// The theme files of package `id` in `dir`.
fn files_of(dir: &Path, id: &str) -> Vec<PathBuf> {
    let prefix = format!("{id}~");
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".toml"))
        })
        .collect()
}

/// Writes the package's themes to `<dir>/<id>~<n>.toml` (replacing an older install) and
/// returns their theme names (file stems), in package order.
pub fn install(v: &ColorVsix, dir: &Path) -> Result<Vec<String>> {
    ensure!(split_id(&v.id).is_some(), "invalid extension id {}", v.id);
    std::fs::create_dir_all(dir)?;
    for old in files_of(dir, &v.id) {
        std::fs::remove_file(old)?;
    }
    let mut names = Vec::new();
    for (n, theme) in v.themes.iter().enumerate() {
        let stem = format!("{}~{n}", v.id);
        let text = format!(
            "# {} from the VS Code Marketplace ({}), mapped by Keel.\n{}",
            theme.name,
            v.id,
            toml::to_string(theme)?
        );
        crate::settings::write_atomic(&dir.join(format!("{stem}.toml")), text.as_bytes())?;
        names.push(stem);
    }
    Ok(names)
}

/// Removes an installed package's themes.
pub fn remove(dir: &Path, id: &str) -> Result<()> {
    ensure!(split_id(id).is_some(), "invalid extension id {id}");
    let files = files_of(dir, id);
    ensure!(!files.is_empty(), "{id} is not installed");
    for f in files {
        std::fs::remove_file(f)?;
    }
    Ok(())
}

/// Installed packages in `dir`: (id, label of its first theme), sorted by id.
pub fn installed(dir: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some((id, _)) = name.strip_suffix(".toml").and_then(|s| s.split_once('~')) else {
            continue;
        };
        if split_id(id).is_none() || out.iter().any(|(i, _)| i == id) {
            continue;
        }
        let label = std::fs::read_to_string(e.path())
            .ok()
            .and_then(|t| toml::from_str::<Theme>(&t).ok())
            .map_or_else(|| id.to_owned(), |t| t.name);
        out.push((id.to_owned(), label));
    }
    out.sort();
    out
}

fn themes_dir() -> PathBuf {
    crate::settings::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("themes")
}

enum Done {
    Progress(u64, Option<u64>),
    Downloaded(Result<Box<ColorVsix>, String>),
    /// The themes now on disk, and the new themes' names (or why not).
    Installed(Result<(String, Vec<String>), String>, crate::theme::Themes),
    Removed(Result<String, String>, crate::theme::Themes),
}

/// Settings → General's color theme installer.
pub struct ColorThemes {
    pub install_id: String,
    pub installed: Vec<(String, String)>,
    download: Option<(String, u64, Option<u64>)>,
    license: Option<Box<ColorVsix>>,
    busy: bool,
    tx: Sender<Done>,
    rx: Receiver<Done>,
    ctx: egui::Context,
}

impl ColorThemes {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            install_id: String::new(),
            installed: installed(&themes_dir()),
            download: None,
            license: None,
            busy: false,
            tx,
            rx,
            ctx,
        }
    }

    fn spawn(&self, job: impl FnOnce(&Sender<Done>) -> Done + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        crate::worker::spawn("keel-color-theme", move || {
            let done = job(&tx);
            let _ = tx.send(done);
            ctx.request_repaint();
        });
    }

    /// Per frame: worker answers. Returns the themes to use when they changed on disk (an
    /// install selects its first theme in `settings`).
    pub fn tick(
        &mut self,
        settings: &mut crate::settings::Settings,
        toasts: &mut Toasts,
    ) -> Option<crate::theme::Themes> {
        let mut out = None;
        while let Ok(done) = self.rx.try_recv() {
            match done {
                Done::Progress(n, total) => {
                    if let Some(d) = &mut self.download {
                        (d.1, d.2) = (n, total);
                    }
                }
                Done::Downloaded(result) => {
                    self.download = None;
                    match result {
                        Ok(v) => self.license = Some(v),
                        Err(e) => toasts.error(e),
                    }
                }
                Done::Installed(result, themes) => {
                    self.busy = false;
                    match result {
                        Ok((id, names)) => {
                            toasts.info(format!("Color theme {id} installed"));
                            if let Some(first) = names.first() {
                                settings.theme = first.clone();
                            }
                        }
                        Err(e) => toasts.error(e),
                    }
                    self.installed = installed(&themes_dir());
                    out = Some(themes);
                }
                Done::Removed(result, themes) => {
                    self.busy = false;
                    match result {
                        Ok(id) => {
                            if settings.theme.starts_with(&format!("{id}~")) {
                                settings.theme = "dark".into();
                            }
                            toasts.info(format!("Color theme {id} removed"));
                        }
                        Err(e) => toasts.error(e),
                    }
                    self.installed = installed(&themes_dir());
                    out = Some(themes);
                }
            }
        }
        out
    }

    /// The install row and the installed packages (Settings → General).
    pub fn settings_rows(&mut self, ui: &mut egui::Ui, current: &str) {
        ui.label("Color themes");
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.install_id)
                        .hint_text("publisher.name")
                        .desired_width(180.0),
                );
                let id = self.install_id.trim().to_owned();
                let ok = split_id(&id).is_some()
                    && self.download.is_none()
                    && self.license.is_none()
                    && !self.busy;
                if ui
                    .add_enabled(ok, egui::Button::new("Install from VS Code Marketplace…"))
                    .on_hover_text("A color theme extension id: publisher.name")
                    .clicked()
                {
                    self.start_download(id);
                }
            });
            if let Some((id, n, total)) = &self.download {
                ui.weak(format!("{id}: {:.1} MB", *n as f32 / (1 << 20) as f32));
                let _ = total;
            }
            let mut remove_id = None;
            for (id, label) in &self.installed {
                ui.horizontal(|ui| {
                    ui.label(label).on_hover_text(id);
                    let in_use = current.starts_with(&format!("{id}~"));
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Remove").small())
                        .on_hover_text(if in_use {
                            "In use: Keel switches to dark"
                        } else {
                            "Delete its theme files"
                        })
                        .clicked()
                    {
                        remove_id = Some(id.clone());
                    }
                });
            }
            if let Some(id) = remove_id {
                self.busy = true;
                self.spawn(move |_| {
                    let result = remove(&themes_dir(), &id).map(|()| id);
                    Done::Removed(
                        result.map_err(|e| format!("{e:#}")),
                        crate::theme::Themes::load(),
                    )
                });
            }
        });
    }

    fn start_download(&mut self, id: String) {
        self.download = Some((id.clone(), 0, None));
        self.spawn(move |tx| {
            let result = icon_theme::download(&id, &mut |n, total| {
                let _ = tx.send(Done::Progress(n, total));
            })
            .and_then(|bytes| inspect(&id, bytes))
            .map(Box::new);
            Done::Downloaded(result.map_err(|e| format!("{e:#}")))
        });
    }

    /// The license dialog of a downloaded package.
    pub fn license_modal(&mut self, ctx: &egui::Context) {
        let Some(v) = &self.license else { return };
        match icon_theme::license_dialog(ctx, "keel-color-license", &v.label, &v.id, &v.license) {
            Some(true) => {
                let v = self.license.take().expect("shown above");
                self.busy = true;
                self.spawn(move |_| {
                    let result = install(&v, &themes_dir()).map(|names| (v.id.clone(), names));
                    Done::Installed(
                        result.map_err(|e| format!("{e:#}")),
                        crate::theme::Themes::load(),
                    )
                });
            }
            Some(false) => self.license = None,
            None => {}
        }
    }
}

#[cfg(test)]
impl ColorThemes {
    /// A downloaded package, as if from the Marketplace: its license dialog shows next.
    pub fn offer(&mut self, v: ColorVsix) {
        self.license = Some(Box::new(v));
    }
}

#[cfg(test)]
#[path = "color_theme_tests.rs"]
mod tests;
