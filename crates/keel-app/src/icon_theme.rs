//! VS Code icon themes (spec 2.6): installed from the VS Code Marketplace into
//! `<config dir>/icons/<extension id>/` (a normalized `theme.json` plus the icon files),
//! listed and selected in Settings → Icons, and switched at runtime (`icons::set_theme`).
//! Downloads, unzips and theme loads run on workers.

use crate::settings::{config_dir, Settings};
use crate::toast::Toasts;
use anyhow::{ensure, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use egui::ImageSource;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `Settings::icon_theme` for the icons embedded in the binary.
pub const BUILTIN: &str = "default";
/// A .vsix larger than this is refused.
const MAX_DOWNLOAD: u64 = 200 << 20;
/// One file inside the .vsix (theme JSON, an icon, the license).
const MAX_ENTRY: u64 = 16 << 20;
/// All icons of a theme together.
const MAX_THEME_BYTES: u64 = 64 << 20;
/// Icons in a theme.
const MAX_ICONS: usize = 10_000;
/// License text shown in the dialog.
const MAX_LICENSE: usize = 64 << 10;

/// Which icon (an `iconDefinitions` id) a name gets. Keys are lowercase once loaded.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Associations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_expanded: Option<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub file_extensions: HashMap<String, String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub file_names: HashMap<String, String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub folder_names: HashMap<String, String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub folder_names_expanded: HashMap<String, String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub language_ids: HashMap<String, String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct IconDef {
    /// svg or png, relative to the theme file. Font glyph icons (`fontCharacter`) are not
    /// supported and fall through to the next rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_path: Option<String>,
}

/// A VS Code icon theme file (`contributes.iconThemes[].path`); also the stored form, with
/// `label` added and every `iconPath` rewritten to `icons/<n>.<ext>`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ThemeJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub icon_definitions: HashMap<String, IconDef>,
    #[serde(flatten)]
    pub base: Associations,
    /// Overrides for light color themes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light: Option<Associations>,
}

pub fn parse(text: &str) -> Result<ThemeJson> {
    serde_json::from_str(text.trim_start_matches('\u{feff}')).context("icon theme JSON")
}

/// VS Code language id of a file, for `languageIds` (the common ones).
fn language_id(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map_or(name, |(_, e)| e);
    Some(match ext {
        "rs" => "rust",
        "py" | "pyw" | "pyi" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "json" => "json",
        "jsonc" => "jsonc",
        "md" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "fs" | "fsx" => "fsharp",
        "vb" => "vb",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "rb" => "ruby",
        "php" => "php",
        "pl" | "pm" => "perl",
        "lua" => "lua",
        "r" => "r",
        "dart" => "dart",
        "scala" => "scala",
        "groovy" | "gradle" => "groovy",
        "hs" => "haskell",
        "clj" | "cljs" => "clojure",
        "sh" | "bash" | "zsh" => "shellscript",
        "ps1" | "psm1" | "psd1" => "powershell",
        "bat" | "cmd" => "bat",
        "xml" | "xaml" | "csproj" | "props" => "xml",
        "yml" | "yaml" => "yaml",
        "toml" => "toml",
        "ini" | "cfg" => "ini",
        "sql" => "sql",
        "vue" => "vue",
        "svelte" => "svelte",
        "tex" => "latex",
        "txt" => "plaintext",
        "dockerfile" => "dockerfile",
        "makefile" | "mk" => "makefile",
        "diff" | "patch" => "diff",
        "log" => "log",
        _ => return None,
    })
}

impl Associations {
    fn lowercased(self) -> Self {
        let lower = |m: HashMap<String, String>| -> HashMap<String, String> {
            m.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect()
        };
        Self {
            file_extensions: lower(self.file_extensions),
            file_names: lower(self.file_names),
            folder_names: lower(self.folder_names),
            folder_names_expanded: lower(self.folder_names_expanded),
            language_ids: self.language_ids,
            ..self
        }
    }

    /// Only the entries whose icon exists, so a missing one falls through to the next rule.
    fn known<T>(mut self, icons: &HashMap<String, T>) -> Self {
        let has = |id: &String| icons.contains_key(id);
        for map in [
            &mut self.file_extensions,
            &mut self.file_names,
            &mut self.folder_names,
            &mut self.folder_names_expanded,
            &mut self.language_ids,
        ] {
            map.retain(|_, id| has(id));
        }
        for one in [&mut self.file, &mut self.folder, &mut self.folder_expanded] {
            *one = one.take().filter(has);
        }
        self
    }

    /// `self` with `over`'s entries on top (the `light` section over the base).
    fn overlay(&self, over: Associations) -> Self {
        let mut out = self.clone();
        out.file = over.file.or(out.file);
        out.folder = over.folder.or(out.folder);
        out.folder_expanded = over.folder_expanded.or(out.folder_expanded);
        out.file_extensions.extend(over.file_extensions);
        out.file_names.extend(over.file_names);
        out.folder_names.extend(over.folder_names);
        out.folder_names_expanded.extend(over.folder_names_expanded);
        out.language_ids.extend(over.language_ids);
        out
    }

    /// fileNames, then fileExtensions (longest first: `d.ts` before `ts`), then
    /// languageIds, then the default file icon.
    pub fn file_icon(&self, name: &str) -> Option<&str> {
        let name = name.to_lowercase();
        let by_ext = || {
            name.match_indices('.')
                .find_map(|(i, _)| self.file_extensions.get(&name[i + 1..]))
        };
        let by_language = || language_id(&name).and_then(|l| self.language_ids.get(l));
        self.file_names
            .get(&name)
            .or_else(by_ext)
            .or_else(by_language)
            .or(self.file.as_ref())
            .map(String::as_str)
    }

    /// folderNames (folderNamesExpanded when open), then the default folder icon.
    pub fn folder_icon(&self, name: Option<&str>, open: bool) -> Option<&str> {
        let name = name.map(str::to_lowercase);
        let (names, default) = match open {
            true => (
                &self.folder_names_expanded,
                self.folder_expanded.as_ref().or(self.folder.as_ref()),
            ),
            false => (&self.folder_names, self.folder.as_ref()),
        };
        name.and_then(|n| names.get(&n))
            .or(default)
            .map(String::as_str)
    }
}

/// An installed theme in memory: every icon's bytes, ready for egui's loaders.
pub struct Loaded {
    icons: HashMap<String, ImageSource<'static>>,
    dark: Associations,
    light: Associations,
}

impl Loaded {
    fn assoc(&self, light: bool) -> &Associations {
        match light {
            true => &self.light,
            false => &self.dark,
        }
    }

    pub fn file(&self, name: &str, light: bool) -> Option<ImageSource<'static>> {
        let id = self.assoc(light).file_icon(name)?;
        self.icons.get(id).cloned()
    }

    pub fn folder(
        &self,
        name: Option<&str>,
        open: bool,
        light: bool,
    ) -> Option<ImageSource<'static>> {
        let id = self.assoc(light).folder_icon(name, open)?;
        self.icons.get(id).cloned()
    }
}

/// Why an icon theme's SVG is refused, or None when it is self-contained. Themes come from
/// the Marketplace, and egui_extras renders SVGs with usvg's default options, which load an
/// `<image href>` (or `<feImage>`) from disk or a UNC path: a leaked NTLM hash, a frozen UI,
/// local files shown. Refused: `<image>`, `<script>`, `<foreignObject>`, entity
/// declarations (they could hide any of these, or expand without end), an `href` that is not
/// `#id` or `data:`, CSS `@import`, `url()` to anything but `#id`, CSS escapes in styles,
/// and anything that is not well-formed UTF-8 XML (usvg would also inflate gzip).
pub fn svg_unsafe(bytes: &[u8]) -> Option<&'static str> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Some("not UTF-8 text");
    };
    let raw = text.to_ascii_lowercase();
    for (needle, why) in [
        ("<image", "an <image>"),
        ("<script", "a <script>"),
        ("<foreignobject", "a <foreignObject>"),
        ("<!entity", "an entity declaration"),
        ("@import", "a CSS @import"),
    ] {
        if raw.contains(needle) {
            return Some(why);
        }
    }
    // As usvg parses it.
    let opts = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let Ok(doc) = roxmltree::Document::parse_with_options(text, opts) else {
        return Some("not well-formed XML");
    };
    let in_style = |n: roxmltree::Node| n.parent().is_some_and(|p| p.has_tag_name("style"));
    for node in doc.descendants() {
        if node.is_text() {
            let text = node.text().unwrap_or_default();
            if let Some(why) = css_unsafe(text, in_style(node)) {
                return Some(why);
            }
            continue;
        }
        let tag = node.tag_name().name().to_ascii_lowercase();
        if matches!(tag.as_str(), "image" | "script" | "foreignobject") {
            return Some("a forbidden element");
        }
        for attr in node.attributes() {
            let value = attr.value().trim_start();
            if attr.name().eq_ignore_ascii_case("href")
                && !value.starts_with('#')
                && !value.to_ascii_lowercase().starts_with("data:")
            {
                return Some("an external href");
            }
            if let Some(why) = css_unsafe(value, attr.name() == "style") {
                return Some(why);
            }
        }
    }
    None
}

/// `@import`, a `url()` to anything but `#id`, and (in a stylesheet) escapes, which could
/// spell either.
fn css_unsafe(text: &str, stylesheet: bool) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("@import") {
        return Some("a CSS @import");
    }
    if stylesheet && text.contains('\\') {
        return Some("a CSS escape");
    }
    for (i, _) in lower.match_indices("url(") {
        let target = lower[i + 4..].trim_start().trim_start_matches(['"', '\'']);
        if !target.starts_with('#') {
            return Some("a url() outside the file");
        }
    }
    None
}

/// "svg" or "png" for an icon path; other formats are skipped.
fn icon_ext(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    [".svg", ".png"]
        .into_iter()
        .find(|e| lower.ends_with(e))
        .map(|e| &e[1..])
}

/// Reads an installed theme (`<dir>/theme.json` + `icons/`). The icon URIs carry
/// `generation`, so a switch never shows textures cached from the previous theme.
pub fn load(dir: &Path, generation: u64) -> Result<Loaded> {
    let json = parse(&std::fs::read_to_string(dir.join("theme.json"))?)?;
    let mut icons = HashMap::new();
    let mut files: HashMap<String, Arc<[u8]>> = HashMap::new();
    for (id, def) in &json.icon_definitions {
        // Stored themes only point into their own icons/ folder.
        let Some(rel) = def
            .icon_path
            .as_deref()
            .filter(|r| r.starts_with("icons/") && !r.contains("..") && !r.contains('\\'))
        else {
            continue;
        };
        if icon_ext(rel).is_none() {
            continue;
        }
        let bytes = match files.get(rel) {
            Some(b) => b.clone(),
            None => match std::fs::read(dir.join(rel)) {
                // Themes installed before the checks existed are checked here too.
                Ok(b) if icon_ext(rel) == Some("svg") && svg_unsafe(&b).is_some() => {
                    let why = svg_unsafe(&b).unwrap_or_default();
                    tracing::warn!("icon theme {}: {rel} skipped: {why}", dir.display());
                    continue;
                }
                Ok(b) => files.entry(rel.to_owned()).or_insert(b.into()).clone(),
                Err(e) => {
                    tracing::warn!("icon theme {}: {rel}: {e}", dir.display());
                    continue;
                }
            },
        };
        let uri = format!("bytes://icon-theme/{generation}/{rel}");
        let bytes = egui::load::Bytes::Shared(bytes);
        icons.insert(
            id.clone(),
            ImageSource::Bytes {
                uri: uri.into(),
                bytes,
            },
        );
    }
    let dark = json.base.lowercased().known(&icons);
    let light = match json.light {
        Some(l) => dark.overlay(l.lowercased().known(&icons)),
        None => dark.clone(),
    };
    Ok(Loaded { icons, dark, light })
}

/// `publisher.name` (e.g. `PKief.material-icon-theme`), else None.
pub fn split_id(id: &str) -> Option<(&str, &str)> {
    let (publisher, name) = id.split_once('.')?;
    let part = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    };
    (part(publisher) && part(name)).then_some((publisher, name))
}

pub fn marketplace_url(publisher: &str, name: &str) -> String {
    format!(
        "https://marketplace.visualstudio.com/_apis/public/gallery/publishers/{publisher}/vsextensions/{name}/latest/vspackage"
    )
}

/// The Marketplace may send the .vsix gzip-wrapped (with or without saying so).
pub fn gunzip_if_needed(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes);
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..])
        .take(MAX_DOWNLOAD + 1)
        .read_to_end(&mut out)
        .context("gzip")?;
    ensure!(out.len() as u64 <= MAX_DOWNLOAD, "the package is too large");
    Ok(out)
}

/// `rel` (a theme's `./../icons/x.svg`) resolved against the zip folder `base`; None when it
/// climbs out of the archive.
fn zip_join(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in rel.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

type Zip<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

fn read_entry(zip: &mut Zip, name: &str) -> Result<Vec<u8>> {
    let entry = zip
        .by_name(name)
        .with_context(|| format!("{name} not in the package"))?;
    let mut out = Vec::new();
    entry.take(MAX_ENTRY + 1).read_to_end(&mut out)?;
    ensure!(out.len() as u64 <= MAX_ENTRY, "{name} is too large");
    Ok(out)
}

/// A downloaded extension package, waiting for the license to be accepted.
pub struct Vsix {
    pub id: String,
    pub label: String,
    /// The package's LICENSE file, or a note when it has none.
    pub license: String,
    /// `contributes.iconThemes[0].path`.
    theme_path: String,
    bytes: Arc<Vec<u8>>,
}

/// `text` cut to at most `max` bytes (on a character boundary), with a note when cut.
fn truncated(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n\n[… cut here: the full text is in the package]");
    }
    text
}

/// Reads `extension/package.json` and the license of a .vsix (gzip-wrapped or not).
pub fn inspect(id: &str, bytes: Vec<u8>) -> Result<Vsix> {
    let bytes = gunzip_if_needed(bytes)?;
    let mut zip = zip::ZipArchive::new(Cursor::new(&bytes[..])).context("not a .vsix package")?;
    let pkg: serde_json::Value =
        serde_json::from_slice(&read_entry(&mut zip, "extension/package.json")?)
            .context("extension/package.json")?;
    let theme = pkg
        .pointer("/contributes/iconThemes/0")
        .context("this extension contributes no icon theme")?;
    let theme_path = theme["path"]
        .as_str()
        .context("the icon theme has no path")?
        .to_owned();
    // Labels may be `%placeholders%` for package.nls.json.
    let label = [&theme["label"], &pkg["displayName"]]
        .into_iter()
        .filter_map(|v| v.as_str())
        .find(|l| !l.starts_with('%'))
        .unwrap_or(id)
        .to_owned();
    let license_file = zip
        .file_names()
        .filter(|n| {
            n.strip_prefix("extension/")
                .is_some_and(|f| !f.contains('/') && f.to_ascii_lowercase().starts_with("license"))
        })
        .min_by_key(|n| n.len())
        .map(str::to_owned);
    let license = match license_file.map(|f| read_entry(&mut zip, &f)) {
        Some(Ok(text)) => String::from_utf8_lossy(&text).into_owned(),
        _ => match pkg["license"].as_str() {
            Some(l) => format!(
                "The package declares the license \"{l}\" but includes no license text. \
                 See the extension's Marketplace page for its terms."
            ),
            None => "The package includes no license text. See the extension's Marketplace \
                     page for its terms."
                .into(),
        },
    };
    drop(zip);
    Ok(Vsix {
        id: id.to_owned(),
        label,
        license: truncated(license, MAX_LICENSE),
        theme_path,
        bytes: Arc::new(bytes),
    })
}

/// Unpacks the icon theme of `v` into `<root>/<id>/`: a normalized theme.json (label added,
/// icon paths `icons/<n>.svg|png`) and only the icon files it uses, without the SVGs
/// `svg_unsafe` refuses. Returns how many icon files were refused. At most `MAX_ICONS`
/// icons, `MAX_THEME_BYTES` in all. Replaces an older install; a failure leaves the old one
/// alone.
pub fn install(v: &Vsix, root: &Path) -> Result<usize> {
    install_capped(v, root, MAX_THEME_BYTES, MAX_ICONS)
}

fn install_capped(v: &Vsix, root: &Path, max_bytes: u64, max_icons: usize) -> Result<usize> {
    ensure!(split_id(&v.id).is_some(), "invalid extension id {}", v.id);
    let mut zip = zip::ZipArchive::new(Cursor::new(&v.bytes[..]))?;
    let theme_entry = zip_join("extension", &v.theme_path).context("bad icon theme path")?;
    let text = read_entry(&mut zip, &theme_entry)?;
    let mut json = parse(&String::from_utf8_lossy(&text))?;
    let theme_dir = theme_entry
        .rsplit_once('/')
        .map_or("", |(d, _)| d)
        .to_owned();
    let tmp = root.join(format!(".{}.partial", v.id));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("icons"))?;
    let written = (|| -> Result<usize> {
        let mut stored: HashMap<String, String> = HashMap::new();
        let mut refused = std::collections::HashSet::new();
        let mut total = 0;
        for def in json.icon_definitions.values_mut() {
            let src = def.icon_path.take().and_then(|rel| {
                let ext = icon_ext(&rel)?;
                Some((zip_join(&theme_dir, &rel)?, ext))
            });
            let Some((src, ext)) = src else { continue };
            if !stored.contains_key(&src) && !refused.contains(&src) {
                let Ok(bytes) = read_entry(&mut zip, &src) else {
                    continue;
                };
                if let Some(why) = (ext == "svg").then(|| svg_unsafe(&bytes)).flatten() {
                    tracing::warn!("icon theme {}: {src} refused: {why}", v.id);
                    refused.insert(src);
                    continue;
                }
                ensure!(
                    stored.len() < max_icons,
                    "the theme has more than {max_icons} icons"
                );
                total += bytes.len() as u64;
                ensure!(
                    total <= max_bytes,
                    "the theme's icons take more than {} MB",
                    max_bytes >> 20
                );
                let name = format!("icons/{}.{ext}", stored.len());
                std::fs::write(tmp.join(&name), bytes)?;
                stored.insert(src.clone(), name);
            }
            def.icon_path = stored.get(&src).cloned();
        }
        json.icon_definitions.retain(|_, d| d.icon_path.is_some());
        ensure!(
            !json.icon_definitions.is_empty(),
            "the theme has no svg or png icons"
        );
        json.label = Some(v.label.clone());
        std::fs::write(tmp.join("theme.json"), serde_json::to_vec_pretty(&json)?)?;
        let dest = root.join(&v.id);
        if dest.exists() {
            std::fs::remove_dir_all(&dest).context("replace the installed theme")?;
        }
        std::fs::rename(&tmp, &dest)?;
        Ok(refused.len())
    })();
    if written.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    written
}

/// Installed themes under `root`: (id, label), sorted by label.
pub fn list_installed(root: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let id = e
                .file_name()
                .into_string()
                .ok()
                .filter(|n| split_id(n).is_some())?;
            let text = std::fs::read_to_string(e.path().join("theme.json")).ok()?;
            let value: serde_json::Value = serde_json::from_str(&text).ok()?;
            let label = value["label"].as_str().unwrap_or(&id).to_owned();
            Some((id, label))
        })
        .collect();
    out.sort_by_key(|(_, label)| label.to_lowercase());
    out
}

/// Deletes an installed theme (downloaded files Keel manages, not user documents).
pub fn remove(root: &Path, id: &str) -> Result<()> {
    ensure!(split_id(id).is_some(), "invalid theme id {id}");
    std::fs::remove_dir_all(root.join(id)).with_context(|| format!("remove {id}"))
}

fn root() -> PathBuf {
    config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("icons")
}

enum Done {
    Listed(Vec<(String, String)>),
    Progress(u64, Option<u64>),
    Downloaded(Result<Box<Vsix>, String>),
    /// (id, icons refused) or why not.
    Installed(Result<(String, usize), String>),
    Loaded(String, Result<Arc<Loaded>, String>),
    Removed(Result<String, String>),
}

/// Settings → Icons: installed themes, the Marketplace installer and the active theme.
pub struct IconThemes {
    pub installed: Vec<(String, String)>,
    /// The theme last asked to load (`Settings::icon_theme` when it took effect).
    applied: Option<String>,
    /// Bumped per load: part of every icon URI.
    generation: u64,
    install_id: String,
    /// (id, bytes so far, total) while downloading.
    download: Option<(String, u64, Option<u64>)>,
    /// A downloaded package waiting for "I accept".
    license: Option<Box<Vsix>>,
    /// Installing or removing.
    busy: bool,
    tx: Sender<Done>,
    rx: Receiver<Done>,
    ctx: egui::Context,
}

impl IconThemes {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let me = Self {
            installed: Vec::new(),
            applied: None,
            generation: 0,
            install_id: String::new(),
            download: None,
            license: None,
            busy: false,
            tx,
            rx,
            ctx,
        };
        me.relist();
        me
    }

    fn spawn(&self, job: impl FnOnce(&Sender<Done>) -> Done + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("keel-icon-theme".into())
            .spawn(move || {
                let done = job(&tx);
                let _ = tx.send(done);
                ctx.request_repaint();
            });
        if let Err(e) = spawned {
            tracing::error!("spawn keel-icon-theme: {e}");
        }
    }

    fn relist(&self) {
        self.spawn(|_| Done::Listed(list_installed(&root())));
    }

    /// Per frame: worker answers, and loads the theme named in `settings` when it changed
    /// (Select, a profile switch, startup).
    pub fn tick(&mut self, settings: &mut Settings, toasts: &mut Toasts) {
        while let Ok(done) = self.rx.try_recv() {
            match done {
                Done::Listed(list) => self.installed = list,
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
                Done::Installed(result) => {
                    self.busy = false;
                    match result {
                        Ok((id, refused)) => {
                            match refused {
                                0 => toasts.info(format!("Icon theme {id} installed")),
                                n => toasts.info(format!(
                                    "Icon theme {id} installed without {n} icon(s) that could \
                                     load files from disk or the network"
                                )),
                            }
                            settings.icon_theme = id;
                            self.applied = None; // reload even when reinstalled
                            self.relist();
                        }
                        Err(e) => toasts.error(e),
                    }
                }
                Done::Loaded(id, result) => {
                    if self.applied.as_ref() != Some(&id) {
                        continue; // superseded
                    }
                    match result {
                        Ok(theme) => self.activate(Some(theme)),
                        // For this run only: the setting stays, so a theme that failed for
                        // a passing reason (a locked file) is back next time.
                        Err(e) => {
                            toasts.error(format!("Icon theme {id}: {e}; using the built-in icons"));
                            self.activate(None);
                        }
                    }
                }
                Done::Removed(result) => {
                    self.busy = false;
                    match result {
                        Ok(id) => {
                            if settings.icon_theme == id {
                                settings.icon_theme = BUILTIN.into();
                            }
                            toasts.info(format!("Icon theme {id} removed"));
                            self.relist();
                        }
                        Err(e) => toasts.error(e),
                    }
                }
            }
        }
        if self.applied.as_deref() != Some(settings.icon_theme.as_str()) {
            let id = settings.icon_theme.clone();
            self.applied = Some(id.clone());
            if id == BUILTIN {
                self.activate(None);
            } else {
                self.generation += 1;
                let generation = self.generation;
                self.spawn(move |_| {
                    let result = match split_id(&id) {
                        Some(_) => load(&root().join(&id), generation).map(Arc::new),
                        None => Err(anyhow::anyhow!("not an installed theme")),
                    };
                    Done::Loaded(id, result.map_err(|e| format!("{e:#}")))
                });
            }
        }
    }

    fn activate(&self, theme: Option<Arc<Loaded>>) {
        crate::icons::set_theme(theme);
        // Drop the decoded textures of the previous theme.
        self.ctx.forget_all_images();
        self.ctx.request_repaint();
    }

    fn start_download(&mut self, id: String) {
        let Some((publisher, name)) = split_id(&id) else {
            return;
        };
        let url = marketplace_url(publisher, name);
        self.download = Some((id.clone(), 0, None));
        self.spawn(move |tx| {
            let mut last = std::time::Instant::now();
            let mut progress = |n: u64, total: Option<u64>| {
                if last.elapsed() >= std::time::Duration::from_millis(100) {
                    last = std::time::Instant::now();
                    let _ = tx.send(Done::Progress(n, total));
                }
            };
            let result = keel_vfs::cloud::https_get(&url, MAX_DOWNLOAD, &mut progress)
                .with_context(|| format!("download {id}"))
                .and_then(|bytes| inspect(&id, bytes))
                .map(Box::new);
            Done::Downloaded(result.map_err(|e| format!("{e:#}")))
        });
    }

    pub fn settings_page(&mut self, ui: &mut egui::Ui, s: &mut Settings) {
        ui.label("Icon theme");
        let builtin = (
            BUILTIN.to_owned(),
            "Built-in (Material Icon Theme subset)".to_owned(),
        );
        let mut remove_id = None;
        for (id, label) in std::iter::once(&builtin).chain(&self.installed) {
            ui.horizontal(|ui| {
                ui.radio_value(&mut s.icon_theme, id.clone(), label)
                    .on_hover_text(id);
                if id != BUILTIN
                    && ui
                        .add_enabled(!self.busy, egui::Button::new("Remove").small())
                        .clicked()
                {
                    remove_id = Some(id.clone());
                }
            });
        }
        if let Some(id) = remove_id {
            self.busy = true;
            self.spawn(move |_| {
                Done::Removed(
                    remove(&root(), &id)
                        .map(|()| id)
                        .map_err(|e| format!("{e:#}")),
                )
            });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.weak("Preview");
            if self.applied.as_deref() != Some(s.icon_theme.as_str()) {
                ui.spinner();
            }
            let size = egui::vec2(20.0, 20.0);
            let folders = [
                ("folder", crate::icons::folder()),
                ("open folder", crate::icons::folder_open()),
            ];
            let files = [
                "main.rs",
                "index.ts",
                "package.json",
                "README.md",
                "photo.png",
                "notes.txt",
            ]
            .map(|n| (n, crate::icons::file_icon(n)));
            for (name, src) in folders.into_iter().chain(files) {
                ui.add(egui::Image::new(src).fit_to_exact_size(size))
                    .on_hover_text(name);
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.install_id)
                    .hint_text("PKief.material-icon-theme")
                    .desired_width(220.0),
            );
            let id = self.install_id.trim().to_owned();
            let ok = split_id(&id).is_some()
                && self.download.is_none()
                && self.license.is_none()
                && !self.busy;
            if ui
                .add_enabled(ok, egui::Button::new("Install from VS Code Marketplace…"))
                .on_hover_text("Extension id: publisher.name")
                .clicked()
            {
                self.start_download(id);
            }
        });
        if let Some((id, n, total)) = &self.download {
            let mb = |b: u64| b as f32 / (1 << 20) as f32;
            let text = match total {
                Some(t) => format!("{id}: {:.1} of {:.1} MB", mb(*n), mb(*t)),
                None => format!("{id}: {:.1} MB", mb(*n)),
            };
            let fraction = total.map_or(0.0, |t| *n as f32 / t.max(1) as f32);
            ui.add(
                egui::ProgressBar::new(fraction)
                    .text(text)
                    .animate(total.is_none()),
            );
        }
        if self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Working…");
            });
        }
        ui.weak("Themes are downloaded from the VS Code Marketplace under their own licenses.");
    }

    /// The license dialog of a downloaded package (shown whatever page is open).
    pub fn license_modal(&mut self, ctx: &egui::Context) {
        let Some(v) = &self.license else { return };
        let mut answer = None;
        let modal = egui::Modal::new(egui::Id::new("keel-icon-license")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading(format!("License: {}", v.label));
            ui.label(format!(
                "{} from the VS Code Marketplace. It is installed for you only; Keel does \
                 not redistribute it.",
                v.id
            ));
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(&v.license).monospace()).wrap());
                });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("I accept").clicked() {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        if modal.should_close() && answer.is_none() {
            answer = Some(false);
        }
        match answer {
            Some(true) => {
                let v = self.license.take().expect("shown above");
                self.busy = true;
                self.spawn(move |_| {
                    let id = v.id.clone();
                    Done::Installed(
                        install(&v, &root())
                            .map(|refused| (id, refused))
                            .map_err(|e| format!("{e:#}")),
                    )
                });
            }
            Some(false) => self.license = None,
            None => {}
        }
    }
}

#[cfg(test)]
impl IconThemes {
    /// A downloaded package, as if from the Marketplace: its license dialog shows next.
    pub fn offer(&mut self, v: Vsix) {
        self.license = Some(Box::new(v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const FIXTURE: &str = include_str!("../tests/fixtures/icon-theme.json");

    #[test]
    fn parses_the_fixture() {
        let t = parse(&format!("\u{feff}{FIXTURE}")).unwrap();
        assert_eq!(t.icon_definitions.len(), 14);
        assert_eq!(
            t.icon_definitions["glyph"].icon_path, None,
            "font icons skipped"
        );
        assert_eq!(t.base.file.as_deref(), Some("_file"));
        assert_eq!(t.base.folder_expanded.as_deref(), Some("_folder_open"));
        assert_eq!(t.base.file_extensions["d.ts"], "dts");
        assert_eq!(t.base.language_ids["rust"], "rust");
        assert_eq!(t.light.as_ref().unwrap().file_extensions["ts"], "ts_light");
        assert!(parse("{ nope").is_err());
        // Stored form round-trips.
        let back = parse(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn mapping_precedence() {
        let t = parse(FIXTURE).unwrap();
        let dark = t.base.clone().lowercased();
        let light = dark.overlay(t.light.unwrap().lowercased());
        let f = |a: &Associations, n: &str| a.file_icon(n).unwrap().to_owned();
        // fileNames beat fileExtensions, case-insensitively.
        assert_eq!(f(&dark, "package.json"), "npm");
        assert_eq!(f(&dark, "PACKAGE.JSON"), "npm");
        assert_eq!(f(&dark, "data.json"), "json", "theme key JSON matches json");
        assert_eq!(f(&dark, "cargo.toml"), "rust");
        // Longest extension first.
        assert_eq!(f(&dark, "index.d.ts"), "dts");
        assert_eq!(f(&dark, "index.ts"), "ts", "extension beats languageIds");
        assert_eq!(f(&dark, "INDEX.TS"), "ts");
        // languageIds when no name or extension matches.
        assert_eq!(f(&dark, "main.rs"), "rust");
        assert_eq!(f(&dark, "worker.mts"), "lang_ts");
        assert_eq!(f(&dark, "notes.unknown"), "_file");
        assert_eq!(f(&dark, "README"), "_file");
        // The light variant overrides only what it names.
        assert_eq!(f(&light, "index.ts"), "ts_light");
        assert_eq!(f(&light, "index.d.ts"), "dts");
        // Folders: by name, expanded variant, defaults.
        assert_eq!(dark.folder_icon(Some("SRC"), false), Some("src"));
        assert_eq!(dark.folder_icon(Some("src"), true), Some("src_open"));
        assert_eq!(dark.folder_icon(Some("docs"), false), Some("_folder"));
        assert_eq!(dark.folder_icon(Some("docs"), true), Some("_folder_open"));
        assert_eq!(dark.folder_icon(None, true), Some("_folder_open"));
        assert_eq!(Associations::default().file_icon("a.rs"), None);
    }

    #[test]
    fn ids_and_paths() {
        assert_eq!(
            split_id("PKief.material-icon-theme"),
            Some(("PKief", "material-icon-theme"))
        );
        assert_eq!(
            split_id("vscode-icons-team.vscode-icons"),
            Some(("vscode-icons-team", "vscode-icons"))
        );
        for bad in [
            "", "nodot", ".x", "x.", "a/b.c", "a.b/../c", "a.b c", "a.b.c",
        ] {
            assert_eq!(split_id(bad), None, "{bad:?}");
        }
        assert_eq!(
            marketplace_url("PKief", "material-icon-theme"),
            "https://marketplace.visualstudio.com/_apis/public/gallery/publishers/PKief/vsextensions/material-icon-theme/latest/vspackage"
        );
        assert_eq!(
            zip_join("extension/dist", "./../icons/a.svg").as_deref(),
            Some("extension/icons/a.svg")
        );
        assert_eq!(
            zip_join("extension", "./dist/t.json").as_deref(),
            Some("extension/dist/t.json")
        );
        assert_eq!(zip_join("extension", "..\\..\\..\\x.svg"), None);
        assert_eq!(icon_ext("a/B.SVG"), Some("svg"));
        assert_eq!(icon_ext("a.png"), Some("png"));
        assert_eq!(icon_ext("a.woff"), None);
    }

    /// A .vsix like the Marketplace's: the theme in `extension/dist/`, icons beside it.
    fn vsix(license: bool) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        let mut add = |name: &str, body: &[u8]| {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body).unwrap();
        };
        add("extension.vsixmanifest", b"<PackageManifest/>");
        add(
            "extension/package.json",
            br#"{"name":"t","displayName":"%displayName%","license":"MIT",
                "contributes":{"iconThemes":[{"id":"t","label":"Test Icons","path":"./dist/theme.json"}]}}"#,
        );
        if license {
            add("extension/LICENSE.md", b"MIT License\n\nCopyright (c) test");
        }
        add(
            "extension/dist/theme.json",
            FIXTURE.replace("./icons/", "./../icons/").as_bytes(),
        );
        for name in [
            "file",
            "folder",
            "folder-open",
            "typescript",
            "typescript-light",
            "typescript-def",
            "typescript-lang",
            "json",
            "npm",
            "folder-src",
        ] {
            add(
                &format!("extension/icons/{name}.svg"),
                format!("<svg id='{name}'/>").as_bytes(),
            );
        }
        add("extension/icons/rust.png", b"\x89PNG fake");
        // folder-src-open.svg is missing: that definition is dropped.
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn vsix_layout_install_load_remove() {
        let root = std::env::temp_dir().join(format!("keel-icon-themes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let raw = vsix(true);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        let gz = gz.finish().unwrap();
        assert_eq!(gunzip_if_needed(gz.clone()).unwrap(), raw);

        // Plain and gzip-wrapped packages read the same.
        let v = inspect("test.icons", gz).unwrap();
        assert_eq!(v.label, "Test Icons");
        assert!(v.license.starts_with("MIT License"));
        let plain = inspect("test.icons", vsix(false)).unwrap();
        assert!(plain.license.contains("\"MIT\""), "{}", plain.license);
        assert!(inspect("test.icons", b"not a zip".to_vec()).is_err());

        install(&v, &root).unwrap();
        let dir = root.join("test.icons");
        assert!(!root.join(".test.icons.partial").exists());
        let stored = parse(&std::fs::read_to_string(dir.join("theme.json")).unwrap()).unwrap();
        assert_eq!(stored.label.as_deref(), Some("Test Icons"));
        assert!(!stored.icon_definitions.contains_key("glyph"));
        assert!(
            !stored.icon_definitions.contains_key("escape"),
            "path out of the zip"
        );
        assert!(
            !stored.icon_definitions.contains_key("src_open"),
            "file missing"
        );
        for def in stored.icon_definitions.values() {
            let p = def.icon_path.as_deref().unwrap();
            assert!(p.starts_with("icons/") && dir.join(p).is_file(), "{p}");
        }
        assert_eq!(
            list_installed(&root),
            [("test.icons".to_owned(), "Test Icons".to_owned())]
        );

        let t = load(&dir, 7).unwrap();
        let bytes_of = |src: Option<ImageSource<'static>>| match src {
            Some(ImageSource::Bytes { uri, bytes }) => {
                assert!(uri.starts_with("bytes://icon-theme/7/icons/"), "{uri}");
                String::from_utf8_lossy(&bytes).into_owned()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            bytes_of(t.file("x.d.ts", false)),
            "<svg id='typescript-def'/>"
        );
        assert_eq!(
            bytes_of(t.file("x.ts", true)),
            "<svg id='typescript-light'/>"
        );
        assert_eq!(bytes_of(t.file("main.rs", false)), "\u{fffd}PNG fake");
        assert_eq!(
            bytes_of(t.folder(Some("src"), false, false)),
            "<svg id='folder-src'/>"
        );
        // No expanded src icon was stored: the default expanded folder instead.
        assert_eq!(
            bytes_of(t.folder(Some("src"), true, false)),
            "<svg id='folder-open'/>"
        );
        assert_eq!(bytes_of(t.file("evil.txt", false)), "<svg id='file'/>");
        assert_eq!(
            bytes_of(t.folder(Some("docs"), true, false)),
            "<svg id='folder-open'/>"
        );

        // Reinstalling replaces; removing deletes.
        install(&v, &root).unwrap();
        assert_eq!(list_installed(&root).len(), 1);
        remove(&root, "test.icons").unwrap();
        assert!(list_installed(&root).is_empty());
        assert!(remove(&root, "../x").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unsafe_svgs_are_refused() {
        let gz = {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(b"<svg/>").unwrap();
            gz.finish().unwrap()
        };
        let bad: &[(&[u8], &str)] = &[
            (br#"<svg><image href="C:\x.png"/></svg>"#, "<image>"),
            (br#"<svg><IMAGE href="x.png"/></svg>"#, "<image>, any case"),
            (br#"<svg><script>alert(1)</script></svg>"#, "<script>"),
            (br#"<svg><foreignObject/></svg>"#, "<foreignObject>"),
            (br#"<svg><use href="file:///etc/passwd"/></svg>"#, "file href"),
            (
                br#"<svg xmlns:xlink="http://www.w3.org/1999/xlink"><use xlink:href="\\host\share\x.svg"/></svg>"#,
                "UNC xlink:href",
            ),
            (br#"<svg><filter><feImage href="//host/x.png"/></filter></svg>"#, "feImage href"),
            (br#"<svg><a href="https://example.com"/></svg>"#, "web href"),
            (br#"<svg><style>@import url(x.css);</style></svg>"#, "@import"),
            (br#"<svg><style>rect { fill: url(x.svg#a) }</style></svg>"#, "url() in a stylesheet"),
            (br#"<svg><rect fill="url(http://x/#a)"/></svg>"#, "url() attribute"),
            (br#"<svg><rect style="fill: URL( 'file:x' )"/></svg>"#, "url() in style"),
            (br#"<svg><style>rect { fill: u\72l(x) }</style></svg>"#, "CSS escape"),
            (
                br#"<!DOCTYPE svg [<!ENTITY i "&#60;image href='x.png'/&#62;">]><svg>&i;</svg>"#,
                "entity",
            ),
            (&gz, "gzip"),
            (b"<svg><rect></svg>", "not well-formed"),
            (b"<svg>\xff</svg>", "not UTF-8"),
        ];
        for (svg, what) in bad {
            assert!(svg_unsafe(svg).is_some(), "{what} should be refused");
        }
        let good: &[&[u8]] = &[
            b"<svg id='x'/>",
            br##"<svg xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="g"/></defs>
                <use href="#a"/><rect fill="url(#g)" style="fill: url( '#g' )"/></svg>"##,
            br#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "x.dtd"><svg/>"#,
            br#"<svg><use href="data:image/png;base64,AAAA"/><style>rect{fill:#fff}</style></svg>"#,
        ];
        for svg in good {
            assert_eq!(svg_unsafe(svg), None, "{}", String::from_utf8_lossy(svg));
        }
    }

    /// A theme package whose icons are `icons` (name, body), all used by one definition each.
    fn vsix_with(icons: &[(&str, &[u8])]) -> Vsix {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        let defs: Vec<String> = icons
            .iter()
            .map(|(n, _)| format!(r#""{n}": {{"iconPath": "./icons/{n}"}}"#))
            .collect();
        let theme = format!(
            r#"{{"iconDefinitions": {{{}}}, "file": "{}"}}"#,
            defs.join(","),
            icons[0].0
        );
        zip.start_file("extension/package.json", opts).unwrap();
        zip.write_all(br#"{"contributes":{"iconThemes":[{"label":"T","path":"./theme.json"}]}}"#)
            .unwrap();
        zip.start_file("extension/theme.json", opts).unwrap();
        zip.write_all(theme.as_bytes()).unwrap();
        for (name, body) in icons {
            zip.start_file(format!("extension/icons/{name}"), opts)
                .unwrap();
            zip.write_all(body).unwrap();
        }
        inspect("test.caps", zip.finish().unwrap().into_inner()).unwrap()
    }

    #[test]
    fn install_refuses_unsafe_icons_and_load_skips_them() {
        let root = std::env::temp_dir().join(format!("keel-icon-unsafe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let v = vsix_with(&[
            ("ok.svg", b"<svg id='ok'/>"),
            (
                "bad.svg",
                br#"<svg><image href="\\host\share\x.png"/></svg>"#,
            ),
            ("bad2.svg", br#"<svg><style>@import "x.css";</style></svg>"#),
            ("raw.png", b"\x89PNG <image href='x'>"),
        ]);
        assert_eq!(install(&v, &root).unwrap(), 2, "two SVGs refused");
        let dir = root.join("test.caps");
        let stored = parse(&std::fs::read_to_string(dir.join("theme.json")).unwrap()).unwrap();
        let mut ids: Vec<_> = stored.icon_definitions.keys().cloned().collect();
        ids.sort();
        assert_eq!(ids, ["ok.svg", "raw.png"], "PNGs are not SVG-checked");
        // An install from before the check: load skips the unsafe file.
        let path = dir.join(
            stored.icon_definitions["ok.svg"]
                .icon_path
                .as_ref()
                .unwrap(),
        );
        std::fs::write(&path, br#"<svg><image href="C:\secret.png"/></svg>"#).unwrap();
        let t = load(&dir, 1).unwrap();
        assert!(t.icons.contains_key("raw.png"));
        assert!(!t.icons.contains_key("ok.svg"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_caps_icon_count_and_bytes() {
        let root = std::env::temp_dir().join(format!("keel-icon-caps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let v = vsix_with(&[
            ("a.svg", b"<svg id='a'/>"),
            ("b.svg", b"<svg id='b'/>"),
            ("c.svg", b"<svg id='c'/>"),
        ]);
        let err = install_capped(&v, &root, MAX_THEME_BYTES, 2).unwrap_err();
        assert!(format!("{err:#}").contains("more than 2 icons"), "{err:#}");
        let err = install_capped(&v, &root, 30, MAX_ICONS).unwrap_err();
        assert!(format!("{err:#}").contains("MB"), "{err:#}");
        assert!(!root.join("test.caps").exists(), "nothing left behind");
        assert!(!root.join(".test.caps.partial").exists());
        // At the limits it installs.
        assert_eq!(install_capped(&v, &root, 39, 3).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn license_text_is_capped() {
        assert_eq!(truncated("MIT".into(), 10), "MIT");
        let long = "ä".repeat(40_000); // 80,000 bytes
        let cut = truncated(long, MAX_LICENSE);
        let (text, note) = cut.split_once("\n\n[").unwrap();
        assert!(text.len() <= MAX_LICENSE && text.len() > MAX_LICENSE - 2);
        assert!(text.chars().all(|c| c == 'ä'));
        assert!(note.contains("cut"));
    }

    /// A theme that fails to load falls back to the built-in icons for this run only.
    #[test]
    fn failed_load_keeps_the_setting() {
        let _env = crate::settings::TEST_ENV.lock();
        let cfg = std::env::temp_dir().join(format!("keel-icon-fail-{}", std::process::id()));
        let before = std::env::var_os("KEEL_CONFIG_DIR");
        std::env::set_var("KEEL_CONFIG_DIR", &cfg);
        let mut themes = IconThemes::new(egui::Context::default());
        let mut settings = Settings {
            icon_theme: "missing.theme".into(),
            ..Settings::default()
        };
        let mut toasts = Toasts::default();
        for _ in 0..500 {
            themes.tick(&mut settings, &mut toasts);
            if !toasts.list.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        match before {
            Some(dir) => std::env::set_var("KEEL_CONFIG_DIR", dir),
            None => std::env::remove_var("KEEL_CONFIG_DIR"),
        }
        assert!(toasts
            .list
            .iter()
            .any(|t| t.text.contains("built-in icons")));
        assert_eq!(settings.icon_theme, "missing.theme");
        assert_eq!(
            themes.applied.as_deref(),
            Some("missing.theme"),
            "not retried"
        );
        assert!(!crate::icons::has_theme());
        let _ = std::fs::remove_dir_all(&cfg);
    }

    /// Live, with network and a GPU: `KEEL_CONFIG_DIR=<empty temp dir> KEEL_SHOT=<png>
    /// cargo test -p keel-app -- --ignored marketplace_live`. Installs
    /// PKief.material-icon-theme through the Settings → Icons page (button, license dialog),
    /// renders the window, then switches to a fresh profile and back.
    #[test]
    #[ignore]
    fn marketplace_live() {
        use crate::app::{App, Boot};
        use egui_kittest::kittest::Queryable;
        let Some(cfg) = std::env::var_os("KEEL_CONFIG_DIR") else {
            return;
        };
        let _env = crate::settings::TEST_ENV.lock();
        let start = keel_vfs::VPath::local(env!("CARGO_MANIFEST_DIR"));
        let boot = Boot {
            saved: Some(None),
            ..Boot::at(start)
        };
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .wgpu()
            .build_eframe(|cc| App::new(cc, boot));
        let wait = |h: &mut egui_kittest::Harness<App>, what: &str, done: &dyn Fn(&App) -> bool| {
            for _ in 0..3000 {
                h.step();
                if done(h.state()) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("timed out: {what}");
        };
        {
            let s = &mut h.state_mut().state;
            s.settings_open = true;
            s.remotes.page = crate::settings::Page::Icons;
            s.icon_themes.install_id = "PKief.material-icon-theme".into();
        }
        h.run_steps(3);
        h.get_by_label("Install from VS Code Marketplace…").click();
        wait(&mut h, "license", &|a| {
            a.state.icon_themes.license.is_some()
        });
        h.run_steps(2);
        h.get_by_label("I accept").click();
        let id = "PKief.material-icon-theme";
        wait(&mut h, "install", &|a| {
            a.state.settings.icon_theme == id
                && a.state.icon_themes.applied.as_deref() == Some(id)
                && crate::icons::has_theme()
        });
        h.run_steps(6);
        let rs = crate::tab::test_entry(
            &keel_vfs::VPath::local(env!("CARGO_MANIFEST_DIR")),
            "main.rs",
            keel_vfs::Kind::File,
            1,
        );
        match crate::icons::icon_for(&rs) {
            ImageSource::Bytes { uri, .. } => {
                assert!(uri.starts_with("bytes://icon-theme/"), "{uri}")
            }
            other => panic!("{other:?}"),
        }
        if let Some(shot) = std::env::var_os("KEEL_SHOT") {
            h.render().unwrap().save(shot).unwrap();
        }
        assert!(std::path::Path::new(&cfg)
            .join("icons")
            .join(id)
            .join("theme.json")
            .is_file());

        // A fresh profile has the built-in icons; switching back restores the theme.
        h.state_mut()
            .state
            .run(0, crate::keys::Action::SwitchProfile("live2".into()));
        wait(&mut h, "switch", &|a| {
            crate::profiles::current() == "live2"
                && a.state.icon_themes.applied.as_deref() == Some(BUILTIN)
        });
        assert!(!crate::icons::has_theme());
        h.run_steps(4);
        if let Some(shot) = std::env::var_os("KEEL_SHOT2") {
            h.render().unwrap().save(shot).unwrap();
        }
        h.state_mut()
            .state
            .run(0, crate::keys::Action::SwitchProfile("default".into()));
        wait(&mut h, "switch back", &|a| {
            crate::profiles::current() == "default"
                && a.state.settings.icon_theme == id
                && crate::icons::has_theme()
        });
        crate::profiles::set_current("default");
    }
}
