//! Task 76: Settings → Advanced (every setting of the profile's config.toml with its path,
//! type, value and default; a search box, "Show only modified", an inline editor that
//! checks the value, reset per row), Import settings… / Export settings… (a TOML file of
//! the profile's settings) and Reset all settings (asks first, keeps a dated backup).
//!
//! Setting paths are key lists (`["search", "delay_ms"]`), never dotted literals, so the
//! i18n key check does not take them for interface text.

use crate::a11y::Named;
use crate::i18n::tr;
use crate::settings::Settings;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use toml::{Table, Value};

/// What the Advanced page and its palette entries do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoCmd {
    /// Open Settings → Advanced.
    Advanced,
    /// Pick a file and import it.
    Import,
    /// The Export settings dialog.
    Export,
    /// The Reset all settings question.
    ResetAll,
}

/// Accounts: remote hosts, cloud accounts and the devices this one trusts. Export leaves
/// them out unless "Include accounts" is ticked. Their passwords, passphrases, tokens and
/// keys are never in config.toml (the OS keychain holds them), so no export has them.
const ACCOUNTS: &[&[&str]] = &[
    &["remotes"],
    &["clouds"],
    &["devices", "auto_accept"],
    &["devices", "sync"],
];

/// This computer's paths and state: export leaves them out unless "Include this
/// computer's paths" is ticked.
const MACHINE: &[&[&str]] = &[
    &["terminal_shell"],
    &["open_with"],
    &["omissions", "permanent"],
    &["sidebar", "favorites"],
    &["views", "folders"],
    &["library", "name"],
    &["devices", "inbox"],
    &["devices", "label"],
    &["search_index"],
    &["updates", "last_check"],
    &["updates", "asked"],
    &["updates", "skip"],
];

/// Key names never exported at any depth, ticked or not (none should be in config.toml;
/// this keeps one that slipped in from leaving the machine).
const SECRET_KEYS: &[&str] = &[
    "password",
    "passphrase",
    "secret",
    "token",
    "access_key",
    "secret_key",
    "account_key",
    "api_key",
    "client_secret",
];

/// Largest settings file imported.
pub const MAX_IMPORT: u64 = 4 * 1024 * 1024;

/// Ranges the Settings window's sliders keep to; the editor refuses values outside them.
const RANGES: &[(&[&str], f64, f64)] = &[
    (
        &["ui_scale"],
        crate::settings::MIN_SCALE as f64,
        crate::settings::MAX_SCALE as f64,
    ),
    (
        &["max_preview_mb"],
        1.0,
        (keel_preview::MAX_PREVIEW_BYTES / (1024 * 1024)) as f64,
    ),
    (&["hover_preview_ms"], 200.0, 2000.0),
    (&["devices", "sync_secs"], 10.0, 3600.0),
    (&["sidebar_width"], 0.0, 4000.0),
    (&["preview_width"], 0.0, 4000.0),
    (&["terminal_height"], 0.0, 4000.0),
];

fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    (SECRET_KEYS.iter()).any(|s| k == *s || k.ends_with(&format!("_{s}")))
}

/// The settings as a TOML table (as config.toml has them).
pub fn to_table(s: &Settings) -> Result<Table, String> {
    match Value::try_from(s) {
        Ok(Value::Table(t)) => Ok(t),
        Ok(_) => Err("settings are not a table".into()),
        Err(e) => Err(e.to_string()),
    }
}

fn get_path<'a>(t: &'a Table, keys: &[impl AsRef<str>]) -> Option<&'a Value> {
    let (last, parents) = keys.split_last()?;
    let mut t = t;
    for k in parents {
        t = t.get(k.as_ref())?.as_table()?;
    }
    t.get(last.as_ref())
}

/// Sets (Some) or removes (None) the value at `keys`, making the tables on the way.
fn set_path(t: &mut Table, keys: &[impl AsRef<str>], value: Option<Value>) {
    let Some((last, parents)) = keys.split_last() else {
        return;
    };
    let mut t = t;
    for k in parents {
        let entry = (t.entry(k.as_ref().to_owned())).or_insert_with(|| Value::Table(Table::new()));
        if !entry.is_table() {
            *entry = Value::Table(Table::new());
        }
        let Value::Table(next) = entry else {
            return;
        };
        t = next;
    }
    match value {
        Some(v) => {
            t.insert(last.as_ref().to_owned(), v);
        }
        None => {
            t.remove(last.as_ref());
        }
    }
}

fn scrub_secrets(t: &mut Table) {
    t.retain(|k, _| !is_secret_key(k));
    for (_, v) in t.iter_mut() {
        scrub_value(v);
    }
}

fn scrub_value(v: &mut Value) {
    match v {
        Value::Table(t) => scrub_secrets(t),
        Value::Array(items) => items.iter_mut().for_each(scrub_value),
        _ => {}
    }
}

/// What Export settings… writes besides the portable settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportOptions {
    /// Remote hosts, cloud accounts and trusted devices.
    pub accounts: bool,
    /// This computer's paths: the shell, Open with apps, Favorites, folder views, the
    /// index's folders, the library's name.
    pub machine: bool,
}

/// The exported file: the profile's settings without accounts and this computer's paths
/// (unless ticked), and never a secret.
pub fn export_text(s: &Settings, o: ExportOptions, date: &str) -> Result<String, String> {
    let mut t = to_table(s)?;
    if !o.accounts {
        ACCOUNTS.iter().for_each(|&k| set_path(&mut t, k, None));
    }
    if !o.machine {
        MACHINE.iter().for_each(|&k| set_path(&mut t, k, None));
    }
    scrub_secrets(&mut t);
    let body = toml::to_string_pretty(&t).map_err(|e| e.to_string())?;
    let mut left_out = Vec::new();
    if !o.accounts {
        left_out.push("remote hosts, cloud accounts and trusted devices");
    }
    if !o.machine {
        left_out.push("this computer's paths");
    }
    let left_out = match left_out.is_empty() {
        true => String::new(),
        false => format!("# Left out: {}.\n", left_out.join("; ")),
    };
    Ok(format!(
        "# Keel settings, exported {date}. Passwords, keys and tokens are never in this file.\n\
         {left_out}# Import them in Settings → Advanced → Import settings….\n\n{body}"
    ))
}

/// `incoming` over `into`: a table whose default is a non-empty table (a group of
/// settings) is merged key by key; anything else (a value, a list, a map such as
/// `[keymap]`) is replaced.
fn merge(into: &mut Table, incoming: Table, defaults: Option<&Table>) {
    for (k, v) in incoming {
        let d = defaults.and_then(|d| d.get(&k)).and_then(Value::as_table);
        match (into.get_mut(&k), v, d) {
            (Some(Value::Table(a)), Value::Table(b), Some(d)) if !d.is_empty() => {
                merge(a, b, Some(d));
            }
            (_, v, _) => {
                into.insert(k, v);
            }
        }
    }
}

/// The settings after importing `text` over `current`: what the file has replaces the
/// current value, what it lacks (accounts, this computer's paths) stays. Also the parts
/// skipped (unknown settings, invalid entries).
pub fn import_text(current: &Settings, text: &str) -> Result<(Settings, Vec<String>), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let incoming: Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let defaults = to_table(&Settings::default())?;
    let mut merged = to_table(current)?;
    let mut skipped = Vec::new();
    let mut known = Table::new();
    for (k, v) in incoming {
        if defaults.contains_key(&k) {
            known.insert(k, v);
        } else {
            skipped.push(k);
        }
    }
    merge(&mut merged, known, Some(&defaults));
    let text = toml::to_string(&merged).map_err(|e| e.to_string())?;
    let (s, dropped) = crate::settings::parse_lenient(&text)?;
    skipped.extend(dropped);
    Ok((s, skipped))
}

/// The defaults, keeping what Reset all leaves alone: the interface language, the
/// accounts (remote hosts, cloud accounts, trusted devices) and the library opened.
pub fn reset_all(current: &Settings) -> Settings {
    let mut s = Settings::default();
    s.language.clone_from(&current.language);
    s.remotes.clone_from(&current.remotes);
    s.clouds.clone_from(&current.clouds);
    s.devices
        .auto_accept
        .clone_from(&current.devices.auto_accept);
    s.devices.sync.clone_from(&current.devices.sync);
    s.library.name.clone_from(&current.library.name);
    s
}

/// Writes `s` to `<config>.bak-<date>` (`-2`, `-3`, … when that exists) and returns the
/// path.
pub fn backup(config: &Path, s: &Settings, date: &str) -> Result<PathBuf, String> {
    let name =
        (config.file_name()).map_or("config.toml".into(), |n| n.to_string_lossy().into_owned());
    let text = toml::to_string_pretty(s).map_err(|e| e.to_string())?;
    for n in 1..1000 {
        let file = match n {
            1 => format!("{name}.bak-{date}"),
            n => format!("{name}.bak-{date}-{n}"),
        };
        let path = config.with_file_name(file);
        if !path.exists() {
            crate::settings::write_atomic(&path, text.as_bytes()).map_err(|e| format!("{e:#}"))?;
            return Ok(path);
        }
    }
    Err(format!("{name}.bak-{date}: too many backups"))
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// A setting's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Integer,
    Float,
    Text,
    List,
    Table,
    Date,
}

impl Kind {
    fn of(v: &Value) -> Self {
        match v {
            Value::Boolean(_) => Self::Bool,
            Value::Integer(_) => Self::Integer,
            Value::Float(_) => Self::Float,
            Value::String(_) => Self::Text,
            Value::Array(_) => Self::List,
            Value::Table(_) => Self::Table,
            Value::Datetime(_) => Self::Date,
        }
    }

    pub fn label(self) -> &'static str {
        tr(match self {
            Self::Bool => "settings_io.kind_bool",
            Self::Integer => "settings_io.kind_integer",
            Self::Float => "settings_io.kind_number",
            Self::Text => "settings_io.kind_text",
            Self::List => "settings_io.kind_list",
            Self::Table => "settings_io.kind_table",
            Self::Date => "settings_io.kind_date",
        })
    }
}

/// One row of the Advanced page.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub keys: Vec<String>,
    pub kind: Kind,
    /// None: not set (the default applies).
    pub value: Option<Value>,
    /// None: no default (a value only some configs have).
    pub default: Option<Value>,
}

impl Row {
    pub fn path(&self) -> String {
        self.keys.join(".")
    }

    pub fn modified(&self) -> bool {
        match (&self.value, &self.default) {
            (Some(a), Some(b)) => !same(a, b),
            (None, None) => false,
            _ => true,
        }
    }
}

/// Every setting of `s`: one row per value, list and map, inside the groups of settings
/// (tables whose default is a non-empty table), sorted by path.
pub fn rows(s: &Settings) -> Vec<Row> {
    let (Ok(cur), Ok(def)) = (to_table(s), to_table(&Settings::default())) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(&mut out, &mut Vec::new(), Some(&cur), Some(&def));
    out
}

fn walk(out: &mut Vec<Row>, keys: &mut Vec<String>, cur: Option<&Table>, def: Option<&Table>) {
    let names: BTreeSet<&String> = (cur.into_iter().flat_map(|t| t.keys()))
        .chain(def.into_iter().flat_map(|t| t.keys()))
        .collect();
    for name in names {
        let c = cur.and_then(|t| t.get(name));
        let d = def.and_then(|t| t.get(name));
        keys.push(name.clone());
        match (c, d) {
            (None | Some(Value::Table(_)), Some(Value::Table(dt))) if !dt.is_empty() => {
                walk(out, keys, c.and_then(Value::as_table), Some(dt));
            }
            _ => {
                if let Some(v) = c.or(d) {
                    out.push(Row {
                        keys: keys.clone(),
                        kind: Kind::of(v),
                        value: c.cloned(),
                        default: d.cloned(),
                    });
                }
            }
        }
        keys.pop();
    }
}

/// Equal, with floats compared at f32 precision (most float settings are f32).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => {
            x == y || (x - y).abs() <= 1e-6 * x.abs().max(y.abs()).max(1.0)
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Table(x), Value::Table(y)) => {
            x.len() == y.len() && (x.iter()).all(|(k, a)| y.get(k).is_some_and(|b| same(a, b)))
        }
        _ => a == b,
    }
}

/// A value as the page shows and edits it: text as it is, anything else as TOML.
pub fn shown(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The typed text as a value of the row's type.
fn parse_value(row: &Row, text: &str) -> Result<Value, String> {
    if row.kind == Kind::Text {
        return Ok(Value::String(text.to_owned()));
    }
    let bad = || tr("settings_io.not_toml").to_owned();
    let mut t: Table = toml::from_str(&format!("v = {}", text.trim())).map_err(|_| bad())?;
    let v = t.remove("v").filter(|_| t.is_empty()).ok_or_else(bad)?;
    Ok(match (row.kind, v) {
        (Kind::Float, Value::Integer(i)) => Value::Float(i as f64),
        (_, v) => v,
    })
}

/// `s` with the value at `keys` set (None: removed), if Keel takes it as it is.
pub fn set(s: &Settings, keys: &[String], value: Option<Value>) -> Result<Settings, String> {
    let mut t = to_table(s)?;
    set_path(&mut t, keys, value.clone());
    let new: Settings = (Value::Table(t).try_into())
        .map_err(|e: toml::de::Error| tr("settings_io.refused").replace("{why}", e.message()))?;
    if let Some(v) = &value {
        let back = to_table(&new)
            .ok()
            .and_then(|t| get_path(&t, keys).cloned());
        if !back.is_some_and(|b| same(&b, v)) {
            return Err(tr("settings_io.not_kept").to_owned());
        }
    }
    Ok(new)
}

/// `s` with `row` set to the typed `text`, or why not: not TOML, another type, out of
/// range, an unknown language, a key Keel does not take, or a value Keel would not keep.
pub fn edit(s: &Settings, row: &Row, text: &str) -> Result<Settings, String> {
    let v = parse_value(row, text)?;
    if Kind::of(&v) != row.kind {
        return Err(tr("settings_io.expected").replace("{kind}", row.kind.label()));
    }
    if let Value::Float(f) = v {
        if !f.is_finite() {
            return Err(tr("settings_io.not_finite").to_owned());
        }
    }
    let n = match v {
        Value::Integer(i) => Some(i as f64),
        Value::Float(f) => Some(f),
        _ => None,
    };
    if let (Some(n), Some((_, lo, hi))) = (n, RANGES.iter().find(|(k, ..)| *k == row.keys)) {
        if n < *lo || n > *hi {
            return Err((tr("settings_io.range"))
                .replace("{min}", &lo.to_string())
                .replace("{max}", &hi.to_string()));
        }
    }
    if row.keys == ["language"] {
        let code = v.as_str().unwrap_or_default();
        if !crate::i18n::languages().any(|(c, _)| c == code) {
            let codes: Vec<_> = crate::i18n::languages().map(|(c, _)| c).collect();
            return Err(tr("settings_io.languages").replace("{codes}", &codes.join(", ")));
        }
    }
    let new = set(s, &row.keys, Some(v))?;
    if row.keys.first().is_some_and(|k| k == "keymap") {
        let notes = new.keymap.clone().sanitize();
        if !notes.is_empty() {
            return Err(tr("settings_io.refused").replace("{why}", &notes.join("; ")));
        }
    }
    Ok(new)
}

/// `s` with `row` back at its default (removed when it has none).
pub fn reset_row(s: &Settings, row: &Row) -> Result<Settings, String> {
    set(s, &row.keys, row.default.clone())
}

/// Longest value shown in a cell.
const CELL_CHARS: usize = 44;

fn cell(text: &str) -> String {
    let text = text.replace('\n', " ");
    match text.char_indices().nth(CELL_CHARS) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text,
    }
}

/// Settings → Advanced's state (kept while Settings is closed).
#[derive(Default)]
pub struct AdvancedUi {
    pub search: String,
    pub modified_only: bool,
    /// The row being edited and the text typed so far.
    editing: Option<(Vec<String>, String)>,
    /// Why the last edit of a row was refused.
    error: Option<(Vec<String>, String)>,
    /// The last import, export or reset (text, is an error).
    notice: Option<(String, bool)>,
    /// A palette command for the page to run when it shows.
    pub pending: Option<IoCmd>,
    /// The Export settings dialog is open, with these ticks.
    export: Option<ExportOptions>,
    /// The Reset all settings question is open.
    confirm_reset: bool,
}

impl AdvancedUi {
    /// A value is being typed (Esc ends that, not the Settings window).
    pub fn editing(&self) -> bool {
        self.editing.is_some()
    }

    fn import(&mut self, s: &mut Settings) {
        let Some(path) = crate::platform::file_dialog(tr("settings_io.import"), |d| {
            d.add_filter("TOML", &["toml"])
                .add_filter(tr("settings_io.all_files"), &["*"])
                .pick_file()
        }) else {
            return;
        };
        self.notice = Some(match import_file(&path, s, &Settings::path(), &today()) {
            Ok(text) => (text, false),
            Err(e) => (e, true),
        });
    }

    fn export(&mut self, s: &Settings, o: ExportOptions) {
        let Some(path) = crate::platform::file_dialog(tr("settings_io.export"), |d| {
            d.set_file_name("keel-settings.toml")
                .add_filter("TOML", &["toml"])
                .save_file()
        }) else {
            return;
        };
        self.notice = Some(match export_file(&path, s, o, &today()) {
            Ok(()) => (
                tr("settings_io.exported").replace("{path}", &path.display().to_string()),
                false,
            ),
            Err(e) => (e, true),
        });
    }

    /// Settings → Advanced. Returns true when the theme changed (an import or a reset).
    pub fn page(&mut self, ui: &mut egui::Ui, s: &mut Settings) -> bool {
        let theme = s.theme.clone();
        match self.pending.take() {
            Some(IoCmd::Import) => self.import(s),
            Some(IoCmd::Export) => self.export = Some(ExportOptions::default()),
            Some(IoCmd::ResetAll) => self.confirm_reset = true,
            Some(IoCmd::Advanced) | None => {}
        }
        ui.horizontal(|ui| {
            if ui.button(tr("settings_io.import")).clicked() {
                self.import(s);
            }
            if ui.button(tr("settings_io.export")).clicked() {
                self.export = Some(ExportOptions::default());
            }
            if ui.button(tr("settings_io.reset_all")).clicked() {
                self.confirm_reset = true;
            }
        });
        if let Some((text, error)) = &self.notice {
            if *error {
                ui.colored_label(ui.visuals().error_fg_color, text);
            } else {
                ui.label(text);
            }
        }
        ui.separator();
        let rows = rows(s);
        let needle = self.search.trim().to_lowercase();
        let shown: Vec<&Row> = (rows.iter())
            .filter(|r| !self.modified_only || r.modified())
            .filter(|r| {
                needle.is_empty()
                    || r.path().to_lowercase().contains(&needle)
                    || (r.value.as_ref()).is_some_and(|v| shown(v).to_lowercase().contains(&needle))
            })
            .collect();
        ui.horizontal(|ui| {
            let _ = ui
                .add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text(tr("settings_io.search"))
                        .desired_width(220.0),
                )
                .named(tr("settings_io.search"));
            ui.checkbox(&mut self.modified_only, tr("settings_io.modified_only"));
            let modified = rows.iter().filter(|r| r.modified()).count();
            ui.weak(
                (tr("settings_io.count"))
                    .replace("{n}", &shown.len().to_string())
                    .replace("{total}", &rows.len().to_string())
                    .replace("{modified}", &modified.to_string()),
            );
        });
        let mut change: Option<Result<Settings, String>> = None;
        let mut changed_row: Option<Vec<String>> = None;
        egui::ScrollArea::vertical()
            .max_height(380.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("settings-advanced")
                    .num_columns(5)
                    .striped(true)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        for h in [
                            "settings_io.setting",
                            "settings_io.type",
                            "settings_io.value",
                            "settings_io.default",
                        ] {
                            ui.strong(tr(h));
                        }
                        ui.label("");
                        ui.end_row();
                        for row in &shown {
                            if let Some(c) = self.row(ui, s, row) {
                                change = Some(c);
                                changed_row = Some(row.keys.clone());
                            }
                            ui.end_row();
                        }
                    });
                if shown.is_empty() {
                    ui.weak(tr("settings_io.none"));
                }
            });
        match (change, changed_row) {
            (Some(Ok(new)), _) => {
                *s = new;
                self.error = None;
                self.editing = None;
            }
            (Some(Err(why)), Some(keys)) => self.error = Some((keys, why)),
            _ => {}
        }
        self.export_dialog(ui.ctx(), s);
        self.reset_dialog(ui.ctx(), s);
        s.theme != theme
    }

    /// One row; Some when it was edited or reset (the new settings, or why not).
    fn row(
        &mut self,
        ui: &mut egui::Ui,
        s: &Settings,
        row: &Row,
    ) -> Option<Result<Settings, String>> {
        let path = row.path();
        let mut out = None;
        if row.modified() {
            ui.strong(&path);
        } else {
            ui.label(&path);
        }
        ui.weak(row.kind.label());
        ui.vertical(|ui| {
            let editing = (self.editing.as_ref()).is_some_and(|(k, _)| *k == row.keys);
            let (mut stop, mut submit) = (false, None::<String>);
            match row.kind {
                Kind::Bool => {
                    let mut on = row.value.as_ref().and_then(Value::as_bool).unwrap_or(false);
                    if ui.checkbox(&mut on, "").named(&path).changed() {
                        out = Some(set(s, &row.keys, Some(Value::Boolean(on))));
                    }
                }
                _ if editing => {
                    if let Some((_, text)) = self.editing.as_mut() {
                        let r = ui.add(
                            egui::TextEdit::singleline(text)
                                .desired_width(260.0)
                                .font(egui::TextStyle::Monospace),
                        );
                        let r = r.named(&path);
                        if !r.has_focus() && !r.lost_focus() {
                            r.request_focus();
                        }
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            stop = true;
                        } else if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            submit = Some(text.clone());
                        } else if r.lost_focus() {
                            stop = true;
                        }
                    }
                }
                _ => {
                    let text = (row.value.as_ref())
                        .map_or_else(|| tr("settings_io.not_set").to_owned(), shown);
                    let button = egui::Button::new(egui::RichText::new(cell(&text)).monospace())
                        .frame(false);
                    let r = (ui.add(button).named(&path))
                        .on_hover_text(tr("settings_io.click_to_edit"));
                    if r.clicked() {
                        let text = row.value.as_ref().map(shown).unwrap_or_default();
                        self.editing = Some((row.keys.clone(), text));
                        self.error = None;
                    }
                }
            }
            if stop {
                self.editing = None;
                self.error = None;
            }
            if let Some(text) = submit {
                out = Some(edit(s, row, &text));
            }
            if let Some((_, why)) = (self.error.as_ref()).filter(|(k, _)| *k == row.keys) {
                ui.colored_label(ui.visuals().error_fg_color, why);
            }
        });
        let default = row.default.as_ref().map_or_else(|| "—".to_owned(), shown);
        let _ = ui.weak(cell(&default)).on_hover_text(&default);
        let reset = ui
            .add_enabled(row.modified(), egui::Button::new(tr("settings_io.reset")))
            .named(&tr("settings_io.reset_named").replace("{path}", &path))
            .on_hover_text(tr("settings_io.reset_tip"));
        if reset.clicked() {
            out = Some(reset_row(s, row));
        }
        out
    }

    fn export_dialog(&mut self, ctx: &egui::Context, s: &Settings) {
        let Some(mut o) = self.export else { return };
        let (mut go, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("keel-settings-export")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading(tr("settings_io.export_title"));
            ui.checkbox(&mut o.accounts, tr("settings_io.include_accounts"));
            ui.checkbox(&mut o.machine, tr("settings_io.include_machine"));
            ui.weak(tr("settings_io.no_secrets"));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                go = ui.button(tr("settings_io.export")).clicked();
                cancel = ui.button(tr("dialogs.cancel")).clicked();
            });
        });
        self.export = (!go && !cancel && !modal.should_close()).then_some(o);
        if go {
            self.export(s, o);
        }
    }

    fn reset_dialog(&mut self, ctx: &egui::Context, s: &mut Settings) {
        if !self.confirm_reset {
            return;
        }
        let (mut go, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::new("keel-settings-reset")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading(tr("settings_io.reset_title"));
            ui.label(tr("settings_io.reset_body"));
            ui.weak(
                tr("settings_io.reset_backup")
                    .replace("{file}", &format!("config.toml.bak-{}", today())),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                go = ui.button(tr("settings_io.reset_all_now")).clicked();
                cancel = ui.button(tr("dialogs.cancel")).clicked();
            });
        });
        self.confirm_reset = !go && !cancel && !modal.should_close();
        if go {
            self.notice = Some(match reset_file(s, &Settings::path(), &today()) {
                Ok(text) => (text, false),
                Err(e) => (e, true),
            });
            self.editing = None;
            self.error = None;
        }
    }
}

/// Writes the export of `s` to `path`.
pub fn export_file(path: &Path, s: &Settings, o: ExportOptions, date: &str) -> Result<(), String> {
    let text = export_text(s, o, date)?;
    crate::settings::write_atomic(path, text.as_bytes())
        .map_err(|e| (tr("settings_io.export_failed")).replace("{why}", &format!("{e:#}")))
}

/// Imports `path` into `s` after backing up `s` next to `config`; the notice to show.
pub fn import_file(
    path: &Path,
    s: &mut Settings,
    config: &Path,
    date: &str,
) -> Result<String, String> {
    let failed = |why: String| {
        (tr("settings_io.import_failed"))
            .replace("{path}", &path.display().to_string())
            .replace("{why}", &why)
    };
    let len = std::fs::metadata(path)
        .map_err(|e| failed(e.to_string()))?
        .len();
    if len > MAX_IMPORT {
        return Err(failed(tr("settings_io.too_large").to_owned()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| failed(e.to_string()))?;
    let (new, skipped) = import_text(s, &text).map_err(failed)?;
    let saved = backup(config, s, date).map_err(failed)?;
    *s = new;
    let mut notice = (tr("settings_io.imported"))
        .replace("{path}", &path.display().to_string())
        .replace("{backup}", &saved.display().to_string());
    if !skipped.is_empty() {
        notice.push(' ');
        notice.push_str(&tr("settings_io.skipped").replace("{list}", &skipped.join(", ")));
    }
    Ok(notice)
}

/// Backs up `s` next to `config`, then resets it (`reset_all`); the notice to show.
pub fn reset_file(s: &mut Settings, config: &Path, date: &str) -> Result<String, String> {
    let saved = backup(config, s, date)
        .map_err(|why| tr("settings_io.reset_failed").replace("{why}", &why))?;
    *s = reset_all(s);
    Ok(tr("settings_io.was_reset").replace("{backup}", &saved.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn row(s: &Settings, keys: &[&str]) -> Row {
        (rows(s).into_iter())
            .find(|r| r.keys == keys)
            .unwrap_or_else(|| panic!("no row {keys:?}"))
    }

    fn remote() -> keel_vfs::RemoteHost {
        keel_vfs::RemoteHost {
            id: "nas".into(),
            label: "NAS".into(),
            host: "nas.example".into(),
            port: 22,
            user: "someone".into(),
            auth: keel_vfs::RemoteAuth::Agent,
            home: None,
            bookmarks: Vec::new(),
            use_ssh_config: true,
            protocol: Default::default(),
        }
    }

    fn custom() -> Settings {
        let mut s = Settings {
            theme: "light".into(),
            show_hidden: true,
            ui_scale: 1.3,
            hover_preview_ms: 800,
            terminal_shell: "/opt/shells/fish".into(),
            ..Settings::default()
        };
        s.search.delay_ms = 300;
        s.devices.sync = vec!["device-1".into()];
        s.devices.inbox = "/data/inbox".into();
        s.library.name = "library-7f3".into();
        s.open_with.remember("txt", "/opt/apps/editor");
        s.remotes.push(remote());
        s
    }

    #[test]
    fn export_import_round_trip() {
        let s = custom();
        let all = ExportOptions {
            accounts: true,
            machine: true,
        };
        let text = export_text(&s, all, "2026-10-17").unwrap();
        assert!(text.starts_with("# Keel settings, exported 2026-10-17."));
        let (back, skipped) = import_text(&Settings::default(), &text).unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(back, s);
        // Through files, with a backup of what was there.
        let dir = temp("settings-io-round");
        let file = dir.join("keel-settings.toml");
        export_file(&file, &s, all, "2026-10-17").unwrap();
        let mut target = Settings::default();
        let config = dir.join("config.toml");
        let notice = import_file(&file, &mut target, &config, "2026-10-17").unwrap();
        assert_eq!(target, s);
        assert!(notice.contains("config.toml.bak-2026-10-17"), "{notice}");
        let saved = std::fs::read_to_string(dir.join("config.toml.bak-2026-10-17")).unwrap();
        assert_eq!(
            toml::from_str::<Settings>(&saved).unwrap(),
            Settings::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_leaves_out_accounts_and_paths_unless_ticked() {
        let s = custom();
        let text = export_text(&s, ExportOptions::default(), "2026-10-17").unwrap();
        for absent in [
            "nas.example",
            "someone",
            "device-1",
            "/opt/shells/fish",
            "/data/inbox",
            "library-7f3",
            "/opt/apps/editor",
        ] {
            assert!(!text.contains(absent), "{absent} in:\n{text}");
        }
        assert!(text.contains("theme = \"light\""), "{text}");
        let t: Table = toml::from_str(&text).unwrap();
        assert!(!t.contains_key("remotes") && !t.contains_key("clouds"));
        assert!(!t.contains_key("search_index"));
        // Importing it keeps this computer's accounts and paths.
        let mut here = Settings {
            terminal_shell: "/bin/zsh".into(),
            ..Settings::default()
        };
        here.remotes.push(remote());
        let (back, _) = import_text(&here, &text).unwrap();
        assert_eq!(back.terminal_shell, "/bin/zsh");
        assert_eq!(back.remotes, here.remotes);
        assert_eq!(back.theme, "light");
        assert_eq!(back.search.delay_ms, 300);
        // Ticked: in.
        let accounts = ExportOptions {
            accounts: true,
            machine: false,
        };
        let text = export_text(&s, accounts, "2026-10-17").unwrap();
        assert!(text.contains("nas.example") && text.contains("device-1"));
        assert!(!text.contains("/opt/shells/fish"));
    }

    #[test]
    fn secrets_are_never_exported() {
        let s = custom();
        let mut t = to_table(&s).unwrap();
        // A secret that slipped into the file (a hand edit, an older version).
        set_path(
            &mut t,
            &["clouds"],
            Some(toml::Value::Array(vec![{
                let mut c = Table::new();
                c.insert("id".into(), "box".into());
                c.insert("refresh_token".into(), "TOKEN-1".into());
                c.insert("password".into(), "PASS-1".into());
                Value::Table(c)
            }])),
        );
        set_path(&mut t, &["devices", "api_key"], Some("KEY-1".into()));
        let mut scrubbed = t.clone();
        scrub_secrets(&mut scrubbed);
        let text = toml::to_string(&scrubbed).unwrap();
        for secret in ["TOKEN-1", "PASS-1", "KEY-1"] {
            assert!(!text.contains(secret), "{secret}");
        }
        assert!(text.contains("box"), "the rest of the entry stays");
        // Through export_text, every option ticked.
        let all = ExportOptions {
            accounts: true,
            machine: true,
        };
        let text = export_text(&s, all, "2026-10-17").unwrap();
        for key in ["password", "passphrase", "token", "secret"] {
            assert!(!text.contains(&format!("{key} =")), "{key}:\n{text}");
        }
        assert!(is_secret_key("client_secret") && is_secret_key("Access_Key"));
        assert!(!is_secret_key("keymap") && !is_secret_key("tokens_column"));
    }

    #[test]
    fn reset_keeps_a_backup() {
        let dir = temp("settings-io-reset");
        let config = dir.join("config.toml");
        let mut s = custom();
        s.language = "es".into();
        let before = s.clone();
        let notice = reset_file(&mut s, &config, "2026-10-17").unwrap();
        let bak = dir.join("config.toml.bak-2026-10-17");
        assert!(notice.contains(&bak.display().to_string()), "{notice}");
        let saved: Settings = toml::from_str(&std::fs::read_to_string(&bak).unwrap()).unwrap();
        assert_eq!(saved, before);
        // Defaults, but the language, accounts and library stay.
        assert_eq!(s.theme, Settings::default().theme);
        assert!(!s.show_hidden);
        assert_eq!(s.terminal_shell, "");
        assert_eq!(s.language, "es");
        assert_eq!(s.remotes, before.remotes);
        assert_eq!(s.devices.sync, before.devices.sync);
        assert_eq!(s.library.name, "library-7f3");
        // A second reset the same day keeps the first backup.
        reset_file(&mut s, &config, "2026-10-17").unwrap();
        assert!(dir.join("config.toml.bak-2026-10-17-2").exists());
        let first: Settings = toml::from_str(&std::fs::read_to_string(&bak).unwrap()).unwrap();
        assert_eq!(first, before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rows_list_every_setting_with_its_default() {
        let s = custom();
        let all = rows(&s);
        assert!(all.len() > 60, "{}", all.len());
        let paths: Vec<String> = all.iter().map(Row::path).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), paths.len(), "one row per path");
        let theme = row(&s, &["theme"]);
        assert_eq!((theme.kind, theme.modified()), (Kind::Text, true));
        assert_eq!(theme.default, Some(Value::String("dark".into())));
        let delay = row(&s, &["search", "delay_ms"]);
        assert_eq!((delay.kind, delay.modified()), (Kind::Integer, true));
        assert_eq!(row(&s, &["ui_scale"]).kind, Kind::Float);
        assert!(!row(&s, &["dual"]).modified());
        assert_eq!(row(&s, &["keymap"]).kind, Kind::Table);
        assert_eq!(row(&s, &["remotes"]).kind, Kind::List);
        let modified: Vec<String> = (all.iter())
            .filter(|r| r.modified())
            .map(Row::path)
            .collect();
        assert!(modified.contains(&"show_hidden".to_owned()), "{modified:?}");
        assert!(!rows(&Settings::default())
            .iter()
            .any(|r| r.modified() && r.keys != ["library", "name"]));
    }

    #[test]
    fn the_editor_checks_values() {
        let s = Settings::default();
        let refused = |keys: &[&str], text: &str| {
            let r = row(&s, keys);
            assert!(edit(&s, &r, text).is_err(), "{keys:?} = {text} was taken");
        };
        refused(&["search", "delay_ms"], "fast");
        refused(&["search", "delay_ms"], "-5");
        refused(&["search", "delay_ms"], "\"300\"");
        refused(&["search", "delay_ms"], "1\nshow_hidden = true");
        refused(&["ui_scale"], "9");
        refused(&["ui_scale"], "nan");
        refused(&["hover_preview_ms"], "50");
        refused(&["language"], "xx");
        refused(&["style"], "Fancy");
        refused(&["keymap"], "{ \"no.such.command\" = [\"F9\"] }");
        refused(&["column_widths"], "[\"wide\"]");
        // Taken.
        let r = row(&s, &["search", "delay_ms"]);
        assert_eq!(edit(&s, &r, " 250 ").unwrap().search.delay_ms, 250);
        let r = row(&s, &["ui_scale"]);
        assert!((edit(&s, &r, "2").unwrap().ui_scale - 2.0).abs() < 1e-6);
        assert!((edit(&s, &r, "1.3").unwrap().ui_scale - 1.3).abs() < 1e-6);
        let r = row(&s, &["language"]);
        assert_eq!(edit(&s, &r, "es").unwrap().language, "es");
        let r = row(&s, &["theme"]);
        assert_eq!(edit(&s, &r, "light").unwrap().theme, "light");
        let r = row(&s, &["column_widths"]);
        assert_eq!(
            edit(&s, &r, "[120, 80.5]").unwrap().column_widths,
            [120.0, 80.5]
        );
        // Reset per row.
        let changed = edit(&s, &row(&s, &["search", "delay_ms"]), "250").unwrap();
        let r = row(&changed, &["search", "delay_ms"]);
        assert!(r.modified());
        assert_eq!(reset_row(&changed, &r).unwrap(), s);
    }

    /// The page: search, the editor (a refused value says why, a good one is kept),
    /// Show only modified and Reset on the row.
    #[test]
    fn the_page_searches_edits_and_resets() {
        use egui_kittest::{kittest::Queryable, Harness};
        struct St {
            page: AdvancedUi,
            s: Settings,
        }
        let mut h = Harness::builder()
            .with_size(egui::vec2(1100.0, 700.0))
            .build_ui_state(
                |ui, st: &mut St| {
                    st.page.page(ui, &mut st.s);
                },
                St {
                    page: AdvancedUi::default(),
                    s: Settings::default(),
                },
            );
        h.run_steps(2);
        h.state_mut().page.search = "delay_ms".into();
        h.run_steps(2);
        let keys: Vec<String> = vec!["search".into(), "delay_ms".into()];
        h.state_mut().page.editing = Some((keys.clone(), "fast".into()));
        h.run_steps(2);
        h.press_key(egui::Key::Enter);
        h.run_steps(2);
        let _ = h.get_by_label(tr("settings_io.not_toml"));
        assert_eq!(
            h.state().s.search.delay_ms,
            Settings::default().search.delay_ms
        );
        h.state_mut().page.editing = Some((keys, "250".into()));
        h.run_steps(2);
        h.press_key(egui::Key::Enter);
        h.run_steps(2);
        assert_eq!(h.state().s.search.delay_ms, 250);
        assert!(h.state().page.editing.is_none());
        h.state_mut().page.search.clear();
        h.get_by_label(tr("settings_io.modified_only")).click();
        h.run_steps(2);
        assert!(h.state().page.modified_only);
        h.get_by_label("Reset search.delay_ms").click();
        h.run_steps(2);
        assert_eq!(h.state().s, Settings::default());
        let _ = h.get_by_label(tr("settings_io.none"));
    }

    #[test]
    fn import_reports_what_it_skips_and_refuses_broken_files() {
        let s = Settings::default();
        assert!(import_text(&s, "theme = ").is_err());
        assert!(import_text(&s, "show_hidden = \"yes\"").is_err());
        let (back, skipped) =
            import_text(&s, "\u{feff}theme = 'light'\nno_such_setting = 1\n").unwrap();
        assert_eq!(back.theme, "light");
        assert_eq!(skipped, ["no_such_setting"]);
        // A map is replaced, not merged.
        let mut here = s.clone();
        here.keymap
            .0
            .insert("view.refresh".into(), vec!["F9".into()]);
        let (back, _) = import_text(&here, "[keymap]\n").unwrap();
        assert!(back.keymap.0.is_empty());
        // Too large.
        let dir = temp("settings-io-large");
        let file = dir.join("big.toml");
        std::fs::write(&file, vec![b'#'; MAX_IMPORT as usize + 1]).unwrap();
        let mut target = Settings::default();
        assert!(import_file(&file, &mut target, &dir.join("config.toml"), "2026-10-17").is_err());
        assert!(
            !dir.join("config.toml.bak-2026-10-17").exists(),
            "no backup for a refused import"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
