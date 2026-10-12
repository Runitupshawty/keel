//! Task 66: the Finder-style list view's options and helpers. Columns chosen from the
//! header's menu, relative dates, Kind as words, middle truncation of long names, Group by
//! (Details and Grid), creation times read off the UI thread, and the View Options panel
//! (Ctrl+J). Disclosure triangles live in `Tab` (`tab::Tree`); their child listings come
//! back through the usual listing worker (`fill_trees`).

use crate::pane::{Pane, ViewCx, ViewMode};
use crate::tab::{Listing, Tab};
use chrono::{DateTime, Datelike, Local, NaiveDate};
use keel_vfs::{Entry, Kind, VPath};
use parking_lot::Mutex;
use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::SystemTime;
use unicode_segmentation::UnicodeSegmentation;

/// Height of a group header row (Details) or line (Grid).
pub const GROUP_H: f32 = 26.0;

/// Optional columns of the list view (Name is always shown, first). Tags is the library
/// setting `tags_column`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ListColumn {
    /// The extension (the column search, the Recycle Bin and Sizes use for something else).
    Ext,
    Kind,
    Size,
    Modified,
    Created,
    /// When Keel last opened the file (the library's recents; in-process library only).
    Opened,
}

impl ListColumn {
    pub const ALL: [ListColumn; 6] = [
        ListColumn::Ext,
        ListColumn::Kind,
        ListColumn::Size,
        ListColumn::Modified,
        ListColumn::Created,
        ListColumn::Opened,
    ];

    /// The name in the column menu and the View Options panel.
    pub fn label(self) -> &'static str {
        match self {
            ListColumn::Ext => "Extension",
            ListColumn::Kind => "Kind",
            ListColumn::Size => "Size",
            ListColumn::Modified => "Date Modified",
            ListColumn::Created => "Date Created",
            ListColumn::Opened => "Date Last Opened",
        }
    }
}

/// What rows are grouped by, in Details and Grid; independent of the sort.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum GroupBy {
    #[default]
    None,
    Kind,
    Modified,
    Created,
    Size,
    Tags,
}

impl GroupBy {
    pub const ALL: [GroupBy; 6] = [
        GroupBy::None,
        GroupBy::Kind,
        GroupBy::Modified,
        GroupBy::Created,
        GroupBy::Size,
        GroupBy::Tags,
    ];

    pub fn label(self) -> &'static str {
        match self {
            GroupBy::None => "None",
            GroupBy::Kind => "Kind",
            GroupBy::Modified => "Date Modified",
            GroupBy::Created => "Date Created",
            GroupBy::Size => "Size",
            GroupBy::Tags => "Tags",
        }
    }
}

/// `[list]` in the profile: the list view's columns, dates and grouping.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ListOptions {
    /// Optional columns shown (any order; drawn in `ListColumn::ALL` order).
    pub columns: Vec<ListColumn>,
    /// "Today at 14:05", "Yesterday at 09:12" instead of the full date.
    pub relative_dates: bool,
    pub group_by: GroupBy,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self {
            columns: vec![ListColumn::Ext, ListColumn::Size, ListColumn::Modified],
            relative_dates: false,
            group_by: GroupBy::None,
        }
    }
}

impl ListOptions {
    pub fn shows(&self, c: ListColumn) -> bool {
        self.columns.contains(&c)
    }

    pub fn set(&mut self, c: ListColumn, on: bool) {
        self.columns.retain(|x| *x != c);
        if on {
            self.columns.push(c);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------------------

/// `text` shortened in the middle with an ellipsis to fit `max` (in the units of `width`,
/// which measures one grapheme cluster): "My Long Fi…me.pdf". Never splits a grapheme
/// cluster (accents, emoji, flags). Unchanged when it fits.
pub fn truncate_middle<'a>(text: &'a str, max: f32, width: impl Fn(&str) -> f32) -> Cow<'a, str> {
    let graphemes: Vec<(&str, f32)> = text.graphemes(true).map(|g| (g, width(g))).collect();
    let total: f32 = graphemes.iter().map(|g| g.1).sum();
    if total <= max {
        return Cow::Borrowed(text);
    }
    let budget = max - width("…");
    let (mut head, mut tail) = (0usize, graphemes.len());
    let (mut head_w, mut tail_w) = (0.0f32, 0.0f32);
    // Take from whichever end is shorter so far, so both ends stay readable.
    while head < tail {
        let take_head = head_w <= tail_w;
        let w = if take_head {
            graphemes[head].1
        } else {
            graphemes[tail - 1].1
        };
        if head_w + tail_w + w > budget {
            // The other end may still have room for a narrower grapheme.
            let other = if take_head {
                graphemes[tail - 1].1
            } else {
                graphemes[head].1
            };
            if head + 1 < tail && head_w + tail_w + other <= budget {
                if take_head {
                    tail -= 1;
                    tail_w += other;
                } else {
                    head += 1;
                    head_w += other;
                }
                continue;
            }
            break;
        }
        if take_head {
            head += 1;
            head_w += w;
        } else {
            tail -= 1;
            tail_w += w;
        }
    }
    let mut out = String::with_capacity(text.len());
    graphemes[..head].iter().for_each(|g| out.push_str(g.0));
    out.push('…');
    graphemes[tail..].iter().for_each(|g| out.push_str(g.0));
    Cow::Owned(out)
}

/// `text` middle-truncated to `max` points in `font`.
pub fn fit_middle<'a>(ui: &egui::Ui, text: &'a str, font: &egui::FontId, max: f32) -> Cow<'a, str> {
    ui.fonts(|f| {
        truncate_middle(text, max, |g| {
            g.chars().map(|c| f.glyph_width(font, c)).sum::<f32>()
        })
    })
}

/// A file's kind in words, as the Kind column and Group by Kind show it.
pub fn kind_text(e: &Entry) -> Cow<'static, str> {
    if e.kind == Kind::Dir {
        return Cow::Borrowed("Folder");
    }
    let ext = if e.ext.is_empty() {
        // Search hits and library rows may come without `ext`.
        match e.path.name().rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
            _ => String::new(),
        }
    } else {
        e.ext.to_lowercase()
    };
    kind_of_ext(&ext)
}

/// The kind of a file with extension `ext` (lowercase, no dot).
pub fn kind_of_ext(ext: &str) -> Cow<'static, str> {
    let word = match ext {
        "" => "Document",
        "pdf" => "PDF document",
        "doc" | "docx" => "Word document",
        "xls" | "xlsx" => "Excel spreadsheet",
        "ppt" | "pptx" => "PowerPoint presentation",
        "odt" => "OpenDocument text",
        "ods" => "OpenDocument spreadsheet",
        "odp" => "OpenDocument presentation",
        "rtf" => "Rich text document",
        "txt" | "text" | "log" => "Plain text",
        "md" | "markdown" => "Markdown document",
        "csv" | "tsv" => "CSV document",
        "json" => "JSON document",
        "xml" => "XML document",
        "html" | "htm" => "HTML document",
        "css" => "CSS style sheet",
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => "Configuration file",
        "js" | "mjs" | "cjs" => "JavaScript source",
        "ts" | "tsx" => "TypeScript source",
        "rs" => "Rust source",
        "py" => "Python script",
        "c" => "C source",
        "h" | "hpp" => "C header",
        "cpp" | "cc" | "cxx" => "C++ source",
        "cs" => "C# source",
        "java" => "Java source",
        "go" => "Go source",
        "rb" => "Ruby script",
        "php" => "PHP script",
        "swift" => "Swift source",
        "kt" => "Kotlin source",
        "sh" | "bash" | "zsh" => "Shell script",
        "ps1" => "PowerShell script",
        "bat" | "cmd" => "Batch file",
        "sql" => "SQL script",
        "jpg" | "jpeg" => "JPEG image",
        "png" => "PNG image",
        "gif" => "GIF image",
        "webp" => "WebP image",
        "bmp" => "BMP image",
        "svg" => "SVG image",
        "tif" | "tiff" => "TIFF image",
        "heic" | "heif" => "HEIF image",
        "avif" => "AVIF image",
        "ico" => "Icon",
        "psd" => "Photoshop document",
        "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" => "Raw image",
        "mp3" => "MP3 audio",
        "flac" => "FLAC audio",
        "wav" => "WAV audio",
        "m4a" | "aac" => "AAC audio",
        "ogg" | "opus" => "Ogg audio",
        "wma" => "Windows Media audio",
        "mp4" | "m4v" => "MPEG-4 movie",
        "mov" => "QuickTime movie",
        "mkv" => "Matroska video",
        "avi" => "AVI video",
        "webm" => "WebM video",
        "wmv" => "Windows Media video",
        "zip" => "ZIP archive",
        "7z" => "7-Zip archive",
        "rar" => "RAR archive",
        "tar" => "Tar archive",
        "gz" | "tgz" => "Gzip archive",
        "bz2" | "tbz2" => "Bzip2 archive",
        "xz" | "txz" => "XZ archive",
        "zst" => "Zstandard archive",
        "exe" | "app" | "com" => "Application",
        "msi" => "Windows Installer package",
        "dll" | "so" | "dylib" => "Library",
        "dmg" | "iso" | "img" | "vhd" | "vhdx" => "Disk image",
        "lnk" => "Shortcut",
        "url" | "webloc" => "Internet shortcut",
        "ttf" | "otf" | "woff" | "woff2" => "Font",
        "eml" | "msg" => "Email message",
        "ics" => "Calendar event",
        "vcf" => "Contact card",
        "epub" => "EPUB book",
        "torrent" => "Torrent file",
        other => return Cow::Owned(format!("{} file", other.to_uppercase())),
    };
    Cow::Borrowed(word)
}

/// A date for the date columns: "2026-03-12 14:05", or with `relative` "Today at 14:05",
/// "Yesterday at 09:12" for the last two days (`now` is the local time).
pub fn date_label(t: Option<SystemTime>, relative: bool, now: DateTime<Local>) -> String {
    let Some(t) = t else {
        return String::new();
    };
    let d = DateTime::<Local>::from(t);
    if relative {
        match (now.date_naive() - d.date_naive()).num_days() {
            0 => return format!("Today at {}", d.format("%H:%M")),
            1 => return format!("Yesterday at {}", d.format("%H:%M")),
            _ => {}
        }
    }
    d.format("%Y-%m-%d %H:%M").to_string()
}

// ---------------------------------------------------------------------------------------
// Group by
// ---------------------------------------------------------------------------------------

/// Rows grouped: each top-level entry's group (index into `labels`, which are in display
/// order). Folders expanded inline stay in their parent's group.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Groups {
    pub rank: Vec<u32>,
    pub labels: Vec<String>,
}

impl Groups {
    /// Groups from one `(order, label)` key per entry: groups sort by order, then label.
    pub fn from_keys(keys: Vec<(i64, Cow<'_, str>)>) -> Self {
        let mut uniq: Vec<(i64, Cow<'_, str>)> = keys.clone();
        uniq.sort();
        uniq.dedup();
        let rank = keys
            .iter()
            .map(|k| uniq.binary_search(k).unwrap_or(0) as u32)
            .collect();
        let labels = uniq.into_iter().map(|(_, l)| l.into_owned()).collect();
        Self { rank, labels }
    }
}

/// What a group key reads besides the entry itself.
pub struct GroupInputs<'a> {
    pub today: NaiveDate,
    pub created: &'a HashMap<String, SystemTime>,
    /// Tag names of an entry, in the library's tag order.
    pub tags: &'a dyn Fn(&Entry) -> Option<String>,
    /// Bytes of an entry (a folder's known total), None when unknown.
    pub bytes: &'a dyn Fn(&Entry) -> Option<u64>,
}

/// Finder's date groups, newest first: Today, Yesterday, Previous 7 Days, Previous 30
/// Days, the months of this year, then years.
pub fn date_group(t: Option<SystemTime>, today: NaiveDate) -> (i64, Cow<'static, str>) {
    let Some(t) = t else {
        return (i64::MAX, Cow::Borrowed("Unknown date"));
    };
    let d = DateTime::<Local>::from(t).date_naive();
    let days = (today - d).num_days();
    match days {
        i64::MIN..=-1 => (-1, Cow::Borrowed("Future")),
        0 => (0, Cow::Borrowed("Today")),
        1 => (1, Cow::Borrowed("Yesterday")),
        2..=7 => (2, Cow::Borrowed("Previous 7 Days")),
        8..=30 => (3, Cow::Borrowed("Previous 30 Days")),
        _ if d.year() == today.year() => (
            100 - i64::from(d.month()),
            Cow::Owned(d.format("%B").to_string()),
        ),
        _ => (
            1000 + i64::from(today.year() - d.year()),
            Cow::Owned(d.year().to_string()),
        ),
    }
}

/// Size groups, biggest first; folders without a known total last.
pub fn size_group(bytes: Option<u64>) -> (i64, Cow<'static, str>) {
    const KB: u64 = 1000;
    const MB: u64 = 1000 * KB;
    const GB: u64 = 1000 * MB;
    let Some(b) = bytes else {
        return (10, Cow::Borrowed("Size unknown"));
    };
    let (order, label) = match b {
        _ if b >= GB => (0, "Larger than 1 GB"),
        _ if b >= 100 * MB => (1, "100 MB to 1 GB"),
        _ if b >= 10 * MB => (2, "10 MB to 100 MB"),
        _ if b >= MB => (3, "1 MB to 10 MB"),
        _ if b >= 100 * KB => (4, "100 KB to 1 MB"),
        _ if b > 0 => (5, "Less than 100 KB"),
        _ => (6, "Zero bytes"),
    };
    (order, Cow::Borrowed(label))
}

/// The group key of `e` for `by` (`by` is never `None` here).
pub fn group_key(by: GroupBy, e: &Entry, inp: &GroupInputs<'_>) -> (i64, Cow<'static, str>) {
    match by {
        GroupBy::None => (0, Cow::Borrowed("")),
        GroupBy::Kind => (i64::from(e.kind != Kind::Dir), kind_text(e)),
        GroupBy::Modified => date_group(e.modified, inp.today),
        GroupBy::Created => date_group(inp.created.get(&e.name).copied(), inp.today),
        GroupBy::Size => size_group((inp.bytes)(e)),
        GroupBy::Tags => match (inp.tags)(e) {
            Some(t) => (0, Cow::Owned(t)),
            None => (1, Cow::Borrowed("No tags")),
        },
    }
}

/// Groups of `entries` for `by`.
pub fn groups_of(by: GroupBy, entries: &[Entry], inp: &GroupInputs<'_>) -> Groups {
    Groups::from_keys(entries.iter().map(|e| group_key(by, e, inp)).collect())
}

// ---------------------------------------------------------------------------------------
// Per-tab metadata read off the UI thread (creation times)
// ---------------------------------------------------------------------------------------

type Slot = Arc<Mutex<Option<HashMap<String, SystemTime>>>>;

/// Creation times of a tab's entries (local fixed disks only: elsewhere each one is a
/// round trip), read by a worker once per listing when a column or the grouping needs them.
#[derive(Default)]
pub struct Meta {
    pub created: HashMap<String, SystemTime>,
    /// Bumped when `created` changes.
    pub created_gen: u64,
    /// The tab generation `created` was read for, and the worker reading it.
    done: Option<u64>,
    job: Option<(u64, Slot)>,
}

/// When Keel last opened files, by real path: the library's recents, read by a worker
/// when the Date Last Opened column shows (again every `OPENED_EVERY`).
#[derive(Default)]
pub struct Opened {
    pub by_path: Arc<HashMap<VPath, SystemTime>>,
    read: Option<std::time::Instant>,
    job: Option<OpenedSlot>,
}

type OpenedSlot = Arc<Mutex<Option<HashMap<VPath, SystemTime>>>>;

/// How long the Date Last Opened column trusts what it read.
pub const OPENED_EVERY: std::time::Duration = std::time::Duration::from_secs(20);

impl Opened {
    /// Reads the library's opens again when they are older than `OPENED_EVERY` (only with
    /// the library open in this window: an attached daemon does not serve them).
    pub fn want(&mut self, lib: &crate::library::LibraryUi, ctx: &egui::Context) {
        if let Some(slot) = &self.job {
            let found = slot.lock().take();
            if let Some(found) = found {
                self.by_path = Arc::new(found);
                self.job = None;
            }
            return;
        }
        if self.read.is_some_and(|t| t.elapsed() < OPENED_EVERY) {
            return;
        }
        self.read = Some(std::time::Instant::now());
        ctx.request_repaint_after(OPENED_EVERY);
        let Some(library) = lib.lib.clone() else {
            self.by_path = Default::default();
            return;
        };
        let slot: OpenedSlot = Default::default();
        let out = slot.clone();
        let ctx = ctx.clone();
        let started = crate::worker::spawn("keel-opened", move || {
            let found = match library.last_opened() {
                Ok(list) => list
                    .into_iter()
                    .map(|(p, ts)| {
                        let t = std::time::UNIX_EPOCH
                            + std::time::Duration::from_secs(ts.max(0) as u64);
                        (p, t)
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!("last opened: {e:#}");
                    HashMap::new()
                }
            };
            *out.lock() = Some(found);
            ctx.request_repaint();
        });
        if started {
            self.job = Some(slot);
        }
    }
}

/// When `e` was last opened, from `Opened::by_path`.
pub fn opened_of(
    by_path: &HashMap<VPath, SystemTime>,
    lib: &crate::library::LibraryUi,
    e: &Entry,
) -> Option<SystemTime> {
    if by_path.is_empty() {
        return None;
    }
    let real = crate::library::real_of(&lib.sources, &e.path)?;
    by_path.get(&real).copied()
}

fn opened_id() -> egui::Id {
    egui::Id::new("keel-last-opened")
}

/// Runs `f` on the window's Date Last Opened state (kept in egui's memory: one for all
/// panes).
pub fn with_opened<R>(ctx: &egui::Context, f: impl FnOnce(&mut Opened) -> R) -> R {
    let shared: Arc<Mutex<Opened>> = ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<Arc<Mutex<Opened>>>(opened_id())
            .clone()
    });
    let mut opened = shared.lock();
    f(&mut opened)
}

/// Fills `tab.meta.created` for the current listing (starts or polls the worker).
pub fn want_created(tab: &mut Tab, ctx: &egui::Context) {
    let key = tab.generation();
    if tab.meta.done == Some(key) {
        return;
    }
    if let Some((k, slot)) = &tab.meta.job {
        if *k == key {
            let found = slot.lock().take();
            if let Some(found) = found {
                tab.meta.created = found;
                tab.meta.created_gen += 1;
                tab.meta.done = Some(key);
                tab.meta.job = None;
            }
            return;
        }
    }
    tab.meta.job = None;
    if !crate::sizes::walks_on_its_own(&tab.dir) {
        tab.meta.done = Some(key);
        return;
    }
    let paths: Vec<(String, std::path::PathBuf)> = (tab.entries().iter())
        .filter_map(|e| Some((e.name.clone(), e.path.to_local_path()?)))
        .collect();
    let slot: Slot = Default::default();
    let out = slot.clone();
    let ctx = ctx.clone();
    let started = crate::worker::spawn("keel-created", move || {
        let found = paths
            .into_iter()
            .filter_map(|(name, p)| Some((name, std::fs::metadata(p).ok()?.created().ok()?)))
            .collect();
        *out.lock() = Some(found);
        ctx.request_repaint();
    });
    if started {
        tab.meta.job = Some((key, slot));
    } else {
        tab.meta.done = Some(key);
    }
}

// ---------------------------------------------------------------------------------------
// Each frame, before a view draws
// ---------------------------------------------------------------------------------------

/// Sets what the tab's rows depend on in this view: disclosure triangles (List view of a
/// folder), the grouping (Details and Grid) and the creation times the columns or the
/// grouping read.
pub fn prepare(ctx: &egui::Context, tab: &mut Tab, view: ViewMode, search: bool, cx: &ViewCx) {
    tab.disclosure = view == ViewMode::Details && !search && !tab.is_trash();
    let by = match view {
        ViewMode::Details | ViewMode::Grid => cx.list.group_by,
        _ => GroupBy::None,
    };
    let created_col = view == ViewMode::Details && cx.list.shows(ListColumn::Created);
    if by == GroupBy::Created || created_col {
        want_created(tab, ctx);
    }
    if view == ViewMode::Details && cx.list.shows(ListColumn::Opened) {
        with_opened(ctx, |o| o.want(cx.library, ctx));
    }
    if by == GroupBy::None {
        tab.set_groups(None, None);
        return;
    }
    let today = Local::now().date_naive();
    let lib = cx.library;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (by, tab.generation(), today).hash(&mut h);
    match by {
        GroupBy::Created => tab.meta.created_gen.hash(&mut h),
        GroupBy::Size => (lib.sizes_gen, cx.sizes.gen).hash(&mut h),
        GroupBy::Tags => {
            // Order-independent signature of every tag assignment.
            let sig = lib.tagged.iter().fold(0u64, |acc, (p, t)| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                (p, t).hash(&mut h);
                acc ^ h.finish()
            });
            (sig, lib.tags.len()).hash(&mut h);
        }
        _ => {}
    }
    let key = h.finish();
    if tab.group_key() == Some(key) {
        return;
    }
    let tags = |e: &Entry| -> Option<String> {
        let real = crate::library::real_of(&lib.sources, &e.path)?;
        let on = lib.tagged.get(&real)?;
        lib.tags
            .iter()
            .find(|t| on.contains(&t.id))
            .map(|t| t.name.clone())
    };
    let bytes = |e: &Entry| crate::sizes::bytes_of(lib, cx.sizes, e);
    let inp = GroupInputs {
        today,
        created: &tab.meta.created,
        tags: &tags,
        bytes: &bytes,
    };
    let groups = groups_of(by, &tab.entries()[..tab.top_len()], &inp);
    tab.set_groups(Some(key), Some(groups));
}

/// A listing of `dir` arrived: folders waiting for it in any tab's tree take it.
pub fn fill_trees(panes: &mut [Pane; 2], dir: &VPath, result: &anyhow::Result<Listing>) {
    for tab in panes.iter_mut().flat_map(|p| p.tabs.iter_mut()) {
        tab.fill_child(dir, result);
    }
}

// ---------------------------------------------------------------------------------------
// Menus and the View Options panel
// ---------------------------------------------------------------------------------------

fn options_id() -> egui::Id {
    egui::Id::new("keel-view-options")
}

/// Ctrl+J: shows or hides the View Options panel.
pub fn toggle_options(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        let open = d.get_temp_mut_or_default::<bool>(options_id());
        *open = !*open;
    });
}

pub fn options_open(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(options_id()).unwrap_or(false))
}

/// The column checkboxes, Group by and relative dates: the header's right-click menu
/// and the View Options panel share them.
pub fn options_ui(ui: &mut egui::Ui, list: &mut ListOptions, tags: Option<&mut bool>, menu: bool) {
    ui.label(egui::RichText::new("Show columns").strong());
    let mut name = true;
    ui.add_enabled(false, egui::Checkbox::new(&mut name, "Name"));
    for c in ListColumn::ALL {
        let mut on = list.shows(c);
        if ui.checkbox(&mut on, c.label()).changed() {
            list.set(c, on);
        }
    }
    if let Some(tags) = tags {
        ui.checkbox(tags, "Tags");
    }
    ui.separator();
    ui.checkbox(&mut list.relative_dates, "Use relative dates");
    ui.separator();
    if menu {
        // A combo box inside a context menu closes the menu: a submenu instead.
        ui.menu_button(format!("Group by: {}", list.group_by.label()), |ui| {
            for g in GroupBy::ALL {
                if ui.radio_value(&mut list.group_by, g, g.label()).clicked() {
                    ui.close_menu();
                }
            }
        });
        return;
    }
    ui.horizontal(|ui| {
        ui.label("Group by");
        egui::ComboBox::from_id_salt("keel-group-by")
            .selected_text(list.group_by.label())
            .show_ui(ui, |ui| {
                for g in GroupBy::ALL {
                    ui.selectable_value(&mut list.group_by, g, g.label());
                }
            });
    });
}

/// The list header's right-click menu.
pub fn header_menu(ui: &mut egui::Ui, list: &mut ListOptions, tags: Option<&mut bool>) {
    options_ui(ui, list, tags, true);
    ui.separator();
    let text = format!("View Options… ({})", crate::keys::shortcut_label("Ctrl+J"));
    if ui.button(text).clicked() {
        toggle_options(ui.ctx());
        ui.memory_mut(|m| m.close_popup());
    }
}

/// The View Options panel (Ctrl+J), while it is open.
pub fn options_window(ctx: &egui::Context, list: &mut ListOptions, tags: Option<&mut bool>) {
    let mut open = options_open(ctx);
    if !open {
        return;
    }
    egui::Window::new("View Options")
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(220.0)
        .show(ctx, |ui| {
            options_ui(ui, list, tags, false);
            ui.weak("Columns apply to the Details view, Group by to Details and Grid.");
        });
    if !open {
        ctx.data_mut(|d| d.insert_temp(options_id(), false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::VPath;
    use std::time::Duration;

    fn chars(text: &str, max: usize) -> String {
        truncate_middle(text, max as f32, |_| 1.0).into_owned()
    }

    #[test]
    fn middle_truncation_keeps_both_ends() {
        assert_eq!(chars("short.pdf", 20), "short.pdf");
        assert_eq!(chars("My Long File Name.pdf", 11), "My Lo…e.pdf");
        assert_eq!(chars("My Long File Name.pdf", 11).chars().count(), 11);
        assert_eq!(chars("abcdef", 1), "…");
        assert_eq!(chars("abcdef", 0), "…");
        assert!(matches!(
            truncate_middle("fits", 4.0, |_| 1.0),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn middle_truncation_never_splits_a_grapheme() {
        // "é" as e + combining acute, a family emoji (ZWJ sequence) and a flag.
        let text = "e\u{301}e\u{301}e\u{301}👨‍👩‍👧🇨🇦🇨🇦e\u{301}e\u{301}e\u{301}";
        let out = chars(text, 5);
        assert_eq!(out.graphemes(true).count(), 5);
        assert!(out.starts_with("e\u{301}e\u{301}"));
        assert!(out.ends_with("e\u{301}e\u{301}"));
        for g in out.graphemes(true) {
            assert!(
                g == "…" || text.contains(g),
                "{g:?} is a whole grapheme of the text"
            );
        }
        // Wide graphemes: each CJK one counts 2.
        let wide = |g: &str| if g.is_ascii() { 1.0 } else { 2.0 };
        let out = truncate_middle("日本語のファイル名.txt", 9.0, wide);
        let w: f32 = out
            .graphemes(true)
            .map(|g| if g == "…" { 1.0 } else { wide(g) })
            .sum();
        assert!(w <= 9.0, "{out} is {w} wide");
        assert!(out.ends_with(".txt") || out.ends_with("txt"), "{out}");
    }

    fn entry(name: &str, kind: Kind) -> Entry {
        crate::tab::test_entry(&VPath::parse("mem://t/").unwrap(), name, kind, 1)
    }

    #[test]
    fn kinds_are_words() {
        assert_eq!(kind_text(&entry("a.pdf", Kind::File)), "PDF document");
        assert_eq!(kind_text(&entry("A.JPG", Kind::File)), "JPEG image");
        assert_eq!(kind_text(&entry("data", Kind::Dir)), "Folder");
        assert_eq!(kind_text(&entry("x.qqq", Kind::File)), "QQQ file");
        assert_eq!(kind_text(&entry("README", Kind::File)), "Document");
        let mut e = entry("movie.mkv", Kind::File);
        e.ext.clear();
        assert_eq!(kind_text(&e), "Matroska video", "falls back to the name");
    }

    #[test]
    fn relative_dates() {
        let now = Local::now();
        let at = |d: chrono::Duration| Some(SystemTime::from(now - d));
        let today = date_label(at(chrono::Duration::zero()), true, now);
        assert!(today.starts_with("Today at "), "{today}");
        let yesterday = date_label(at(chrono::Duration::days(1)), true, now);
        assert!(yesterday.starts_with("Yesterday at "), "{yesterday}");
        let old = date_label(at(chrono::Duration::days(40)), true, now);
        assert_eq!(old.len(), "2026-03-12 14:05".len(), "{old}");
        let plain = date_label(at(chrono::Duration::zero()), false, now);
        assert!(!plain.starts_with("Today"));
        assert_eq!(date_label(None, true, now), "");
    }

    #[test]
    fn date_groups_are_newest_first() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        let at = |y, m, d| {
            let t = NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_local_timezone(Local)
                .unwrap();
            Some(SystemTime::from(t))
        };
        let g = |t| date_group(t, today);
        assert_eq!(g(at(2026, 10, 12)).1, "Today");
        assert_eq!(g(at(2026, 10, 11)).1, "Yesterday");
        assert_eq!(g(at(2026, 10, 8)).1, "Previous 7 Days");
        assert_eq!(g(at(2026, 9, 20)).1, "Previous 30 Days");
        assert_eq!(g(at(2026, 3, 1)).1, "March");
        assert_eq!(g(at(2024, 3, 1)).1, "2024");
        assert_eq!(g(None).1, "Unknown date");
        let order: Vec<i64> = [
            at(2026, 10, 12),
            at(2026, 10, 11),
            at(2026, 10, 8),
            at(2026, 9, 20),
            at(2026, 3, 1),
            at(2026, 1, 1),
            at(2025, 6, 1),
            at(2024, 3, 1),
            None,
        ]
        .into_iter()
        .map(|t| g(t).0)
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    }

    #[test]
    fn groups_sort_folders_first_then_by_label() {
        let entries = vec![
            entry("b.pdf", Kind::File),
            entry("a.png", Kind::File),
            entry("zz", Kind::Dir),
            entry("c.pdf", Kind::File),
        ];
        let created = HashMap::new();
        let inp = GroupInputs {
            today: NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
            created: &created,
            tags: &|_| None,
            bytes: &|e| Some(e.size),
        };
        let g = groups_of(GroupBy::Kind, &entries, &inp);
        assert_eq!(g.labels, ["Folder", "PDF document", "PNG image"]);
        assert_eq!(g.rank, [1, 2, 0, 1]);
        let g = groups_of(GroupBy::Size, &entries, &inp);
        assert_eq!(g.labels, ["Less than 100 KB"]);
        let g = groups_of(GroupBy::Tags, &entries, &inp);
        assert_eq!(g.labels, ["No tags"]);
    }

    #[test]
    fn size_groups_biggest_first() {
        let order: Vec<i64> = [
            Some(2_000_000_000),
            Some(5_000_000),
            Some(10),
            Some(0),
            None,
        ]
        .into_iter()
        .map(|b| size_group(b).0)
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    }

    #[test]
    fn options_default_to_the_classic_columns_and_round_trip() {
        let o = ListOptions::default();
        assert!(o.shows(ListColumn::Ext) && o.shows(ListColumn::Size));
        assert!(!o.shows(ListColumn::Kind) && !o.relative_dates);
        let mut o2 = o.clone();
        o2.set(ListColumn::Kind, true);
        o2.set(ListColumn::Ext, false);
        o2.group_by = GroupBy::Modified;
        o2.relative_dates = true;
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Wrap {
            list: ListOptions,
        }
        let text = toml::to_string(&Wrap { list: o2.clone() }).unwrap();
        let back: Wrap = toml::from_str(&text).unwrap();
        assert_eq!(back.list, o2);
        let empty: Wrap = toml::from_str("[list]\n").unwrap();
        assert_eq!(empty.list, o);
    }

    /// Pictures of the list view with expanded folders and Group by, and the grouped grid,
    /// for the design review (needs a GPU), in `KEEL_LIST_SHOTS` (default: the temp
    /// folder's `keel-list-shots`): `cargo test -p keel-app --bin keel list_view_screenshots
    /// -- --ignored`.
    #[test]
    #[ignore]
    fn list_view_screenshots() {
        use crate::app::{App, Boot};
        use crate::screenshots::{listed, wait, write};
        let out = std::env::var_os("KEEL_LIST_SHOTS")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("keel-list-shots"));
        std::fs::create_dir_all(&out).unwrap();
        let _env = crate::settings::TEST_ENV.lock();
        let _restore = crate::settings::EnvRestore::save(&[
            "KEEL_CONFIG_DIR",
            "KEEL_DATA_DIR",
            "KEEL_NET_SECRET",
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Work");
        let ago = |days: u64, hours: u64| {
            SystemTime::now() - Duration::from_secs(days * 86_400 + hours * 3_600)
        };
        for (name, days) in [
            ("Projects/keel/notes.md", 0),
            ("Projects/keel/plan.pdf", 1),
            (
                "Projects/keel/Quarterly budget review for the planning committee final draft.xlsx",
                3,
            ),
            ("Projects/site/index.html", 10),
            ("Photos/beach.jpg", 40),
            ("Photos/hills.png", 41),
            ("Invoices/2026-09.pdf", 20),
            ("readme.txt", 0),
            (
                "Budget 2026 - department summary with a rather long name.xlsx",
                2,
            ),
            ("photo.jpg", 60),
            ("notes.md", 1),
            ("report.pdf", 400),
            ("archive.zip", 5),
        ] {
            write(&root.join(name), "x", ago(days, 2));
        }
        let keel = tmp.path().join(".keel");
        std::env::set_var("KEEL_CONFIG_DIR", keel.join("config"));
        std::env::set_var("KEEL_DATA_DIR", keel.join("data"));
        std::env::set_var("KEEL_NET_SECRET", "memory");
        let mut settings = crate::settings::Settings::default();
        settings.list.set(ListColumn::Kind, true);
        settings.list.relative_dates = true;
        settings.list.group_by = GroupBy::Kind;
        settings.dual = false;
        let start = VPath::local(&root);
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1280.0, 480.0))
            .wgpu()
            .build_eframe(move |cc| {
                let boot = Boot {
                    settings,
                    ..Boot::at(start.clone())
                };
                App::new(cc, boot)
            });
        assert!(wait(&mut h, 30, &listed), "listed");
        let shot = |h: &mut egui_kittest::Harness<App>, name: &str| {
            h.run_steps(4);
            let img = image::DynamicImage::ImageRgba8(h.render().unwrap()).into_rgb8();
            img.save(out.join(format!("{name}.png"))).unwrap();
        };
        let expand = |h: &mut egui_kittest::Harness<App>, name: &str| {
            h.state_mut().state.tab_mut(0).expand(name, false);
            let done = wait(h, 30, &|s| {
                s.tab(0).node_state(name) == Some(&crate::tab::NodeState::Listed)
            });
            assert!(done, "{name} listed");
        };
        expand(&mut h, "Projects");
        expand(&mut h, "Projects/keel");
        let tab = h.state_mut().state.tab_mut(0);
        tab.click("Projects/keel/plan.pdf", false, false);
        shot(&mut h, "list-grouped-tree");
        // Scrolled: the top group's header stays in view.
        let last = h.state().state.tab(0).visible_cached().len() - 1;
        h.state_mut().state.tab_mut(0).scroll_to = Some(last);
        shot(&mut h, "list-sticky");
        h.state_mut().state.settings.list.group_by = GroupBy::None;
        shot(&mut h, "list-tree");
        h.state_mut().state.settings.list.group_by = GroupBy::Modified;
        shot(&mut h, "list-by-date");
        h.state_mut().state.settings.list.group_by = GroupBy::Kind;
        h.state_mut().state.panes[0].view = ViewMode::Grid;
        shot(&mut h, "grid-grouped");
        crate::list_view::toggle_options(&h.state().state.ctx);
        h.state_mut().state.panes[0].view = ViewMode::Details;
        shot(&mut h, "view-options");
    }

    #[test]
    fn creation_times_are_read_off_the_ui_thread() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let path = VPath::local(dir.path());
        let mut tab = Tab::new(path.clone());
        tab.set_entries(vec![crate::tab::test_entry(&path, "a.txt", Kind::File, 1)]);
        if !crate::sizes::walks_on_its_own(&path) {
            return; // A temp dir on a network or removable drive: not read, by design.
        }
        let ctx = egui::Context::default();
        let wait = std::time::Instant::now();
        while tab.meta.created_gen == 0 && wait.elapsed() < Duration::from_secs(5) {
            want_created(&mut tab, &ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        // Not every file system records a creation time.
        if std::fs::metadata(dir.path().join("a.txt"))
            .and_then(|m| m.created())
            .is_ok()
        {
            assert!(tab.meta.created.contains_key("a.txt"));
        }
    }
}
