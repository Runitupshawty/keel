//! Get Info (Task 67; the Properties dialog before it): a Finder-style info window per item
//! or selection, with collapsible sections: General (kind, size, where, dates, tags,
//! locked), More Info (the media metadata the library keeps, a drive's space), Name &
//! Extension, Comments (kept in the library), Open with, Preview and Sharing & Permissions
//! (Windows access list, Unix owner and mode). Several windows can be open at once;
//! Alt+Ctrl+I (Option+Cmd+I) opens one more that follows the active pane's selection.
//! Facts are gathered on a worker (`gather`, `gather_extras`); tags are drawn live from the
//! library. Files, folders, drives and several items at once (a summary) all have one.

use crate::a11y::Named;
use crate::keys::Action;
use crate::library::LibCmd;
use crate::state::{AppState, Msg};
use crate::view_details::size_text_of;
use crate::worker;
use egui::{CollapsingHeader, RichText};
use keel_core::{FinderColor, FinderTag, SourceSummary};
use keel_vfs::sharing::Sharing;
use keel_vfs::{Entry, Kind, Router, Space, VPath};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// A folder walk stops after this many items (the window says the totals are partial).
const MAX_ITEMS: u64 = 2_000_000;
/// The following window asks again once the selection has stayed this long.
pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// After a rename or a lock change the window reads the item again this much later.
const REREAD: Duration = Duration::from_millis(400);

pub const COMMENTS_OFF: &str = "Turn the library on (Settings → Library) to keep comments";
pub const COMMENTS_OUTSIDE: &str = "Add the folder's drive to the library to keep comments";

#[derive(Clone, Debug, Default)]
pub struct Props {
    pub name: String,
    pub location: String,
    pub kind: String,
    pub bytes: u64,
    pub files: u64,
    pub folders: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub hidden: bool,
    pub read_only: bool,
    pub link: bool,
    /// Totals stopped early (too many items, a remote folder that is not walked, a drive).
    pub partial: bool,
    /// One item: as the file system lists it (Preview, Name & Extension, Open with).
    pub entry: Option<Entry>,
    /// A drive (a file system's root): its capacity and free space.
    pub drive: Option<Space>,
    /// One local item: owner and access (an error says why it could not be read).
    pub sharing: Option<Result<Sharing, String>>,
    /// One photo or video: the metadata the library keeps for it.
    pub media: Vec<(String, String)>,
    /// One item: its comment (`Err`: why there can be none, shown with the box disabled).
    pub comment: Option<Result<Option<String>, String>>,
    /// One local file: the applications that open it, `(name, app)`.
    pub apps: Vec<(String, String)>,
    /// One local item on macOS: its Finder tags.
    pub native: Vec<FinderTag>,
}

/// The sections of an info window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    General,
    MoreInfo,
    NameExt,
    Comments,
    OpenWith,
    Preview,
    Sharing,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::MoreInfo => "More Info",
            Section::NameExt => "Name & Extension",
            Section::Comments => "Comments",
            Section::OpenWith => "Open with",
            Section::Preview => "Preview",
            Section::Sharing => "Sharing & Permissions",
        }
    }

    /// Open when the window opens (as in Finder).
    fn open_at_first(self) -> bool {
        matches!(
            self,
            Section::General | Section::NameExt | Section::Comments | Section::Preview
        )
    }
}

/// The sections shown for `p` (`n` items): several items only get General.
pub fn sections(p: &Props, n: usize) -> Vec<Section> {
    if n != 1 {
        return vec![Section::General];
    }
    let file = p.entry.as_ref().is_some_and(|e| e.kind != Kind::Dir);
    let local = p
        .entry
        .as_ref()
        .is_some_and(|e| e.path.to_local_path().is_some());
    let mut out = vec![Section::General, Section::MoreInfo];
    if p.drive.is_none() {
        out.push(Section::NameExt);
    }
    out.push(Section::Comments);
    if file && local {
        out.push(Section::OpenWith);
    }
    out.push(Section::Preview);
    if p.sharing.is_some() {
        out.push(Section::Sharing);
    }
    out
}

/// General's rows (`n` items): what it is, how big, where and when.
pub fn general_rows(p: &Props, n: usize) -> Vec<(&'static str, String)> {
    let mut rows = vec![("Kind", p.kind.clone())];
    let more = if p.partial { "at least " } else { "" };
    match p.drive {
        Some(space) => {
            rows.push(("Capacity", size_text_of(space.total)));
            rows.push(("Available", size_text_of(space.free)));
            rows.push(("Used", size_text_of(space.total.saturating_sub(space.free))));
        }
        None => rows.push((
            "Size",
            format!("{more}{} ({} bytes)", size_text_of(p.bytes), p.bytes),
        )),
    }
    if p.drive.is_none() && (p.folders > 0 || p.kind == "Folder" || n > 1) {
        rows.push((
            "Contains",
            format!("{more}{}", contains(p.files, p.folders)),
        ));
    }
    rows.push(("Where", p.location.clone()));
    if n == 1 {
        rows.push(("Created", date(p.created)));
        rows.push(("Modified", date(p.modified)));
        let attrs: Vec<&str> = [(p.hidden, "hidden"), (p.link, "link")]
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
            .collect();
        if !attrs.is_empty() {
            rows.push(("Attributes", attrs.join(", ")));
        }
    }
    rows
}

/// A local path that is the root of its file system (a drive, a volume, a share).
pub fn is_drive(path: &VPath) -> bool {
    path.to_local_path()
        .is_some_and(|l| l.parent().is_none() && l.is_absolute())
}

/// Gathers the facts about `paths` (one item, or a summary of several). Walks local
/// folders without following links; never walks a drive. Blocks: workers only.
pub fn gather(router: &Router, paths: &[VPath]) -> anyhow::Result<Props> {
    let first = paths
        .first()
        .ok_or_else(|| anyhow::anyhow!("nothing selected"))?;
    let mut p = Props {
        location: first.parent().map(|d| d.display()).unwrap_or_default(),
        ..Props::default()
    };
    let started = Instant::now();
    for path in paths {
        if is_drive(path) {
            let local = path.to_local_path().unwrap_or_default();
            let meta = std::fs::metadata(&local)?;
            p.modified = meta.modified().ok();
            p.created = meta.created().ok();
            p.folders += 1;
            p.partial = true;
            if paths.len() == 1 {
                p.drive = router.provider_for(path).and_then(|pr| pr.space(path));
            }
            continue;
        }
        match path.to_local_path() {
            Some(local) => local_props(&local, &mut p)?,
            None => {
                let provider = router
                    .provider_for(path)
                    .ok_or_else(|| anyhow::anyhow!("no provider for {}", path.display()))?;
                let e = provider.stat(path)?;
                if e.kind == Kind::Dir {
                    p.folders += 1;
                    p.partial = true;
                } else {
                    p.files += 1;
                    p.bytes += e.size;
                }
                p.modified = e.modified;
                p.hidden = e.hidden;
                p.link = e.is_link;
            }
        }
    }
    tracing::debug!("info on {} items in {:?}", paths.len(), started.elapsed());
    if let [one] = paths {
        p.entry = (router.provider_for(one)).and_then(|pr| pr.stat(one).ok());
        p.name = match one.name() {
            "" => one.display(),
            name => name.to_owned(),
        };
        p.kind = if p.drive.is_some() || is_drive(one) {
            "Drive".into()
        } else if p.folders > 0 && !p.link {
            "Folder".into()
        } else if p.link {
            "Link".into()
        } else {
            match one.name().rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => format!("{} file", ext.to_uppercase()),
                _ => "File".into(),
            }
        };
        // The item itself is not one of its contents.
        if p.kind == "Folder" || p.kind == "Drive" {
            p.folders -= 1;
        }
    } else {
        p.name = format!("{} items", paths.len());
        p.kind = "Several items".into();
        p.modified = None;
        p.created = None;
    }
    Ok(p)
}

fn local_props(root: &std::path::Path, p: &mut Props) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(root)?;
    p.modified = meta.modified().ok();
    p.created = meta.created().ok();
    p.read_only = meta.permissions().readonly();
    p.link = meta.file_type().is_symlink();
    p.hidden = is_hidden(root, &meta);
    let mut stack = vec![(root.to_path_buf(), meta)];
    while let Some((path, meta)) = stack.pop() {
        if meta.is_dir() && !meta.file_type().is_symlink() {
            p.folders += 1;
            // Unreadable folders count, their contents do not.
            let Ok(children) = std::fs::read_dir(&path) else {
                continue;
            };
            for child in children.flatten() {
                if p.files + p.folders >= MAX_ITEMS {
                    p.partial = true;
                    return Ok(());
                }
                if let Ok(m) = child.metadata() {
                    stack.push((child.path(), m));
                }
            }
        } else {
            p.files += 1;
            p.bytes += meta.len();
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_hidden(_: &std::path::Path, meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x2 != 0
}

#[cfg(not(windows))]
fn is_hidden(path: &std::path::Path, _: &std::fs::Metadata) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

/// What the library part of the window needs (cloned for the worker).
#[derive(Clone)]
pub struct Library {
    /// None: the library is off (or the daemon is gone).
    pub backend: Option<crate::backend::LibraryBackend>,
    pub sources: Vec<SourceSummary>,
}

/// The facts beyond the file system's for one item: access, media metadata, comment,
/// applications and (macOS) Finder tags. Blocks: workers only.
pub fn gather_extras(
    p: &mut Props,
    path: &VPath,
    lib: &Library,
    media: Option<&crate::media::Shared>,
) {
    let local = path.to_local_path();
    if let Some(l) = &local {
        p.sharing = Some(keel_vfs::sharing::sharing(l).map_err(|e| e.to_string()));
    }
    let real = crate::library::real_of(&lib.sources, path).unwrap_or_else(|| path.clone());
    let in_source = crate::library::source_of(&lib.sources, &real).is_some();
    p.comment = Some(match &lib.backend {
        None => Err(COMMENTS_OFF.to_owned()),
        Some(_) if !in_source => Err(COMMENTS_OUTSIDE.to_owned()),
        Some(b) => b.comment(&lib.sources, path).map_err(|e| format!("{e:#}")),
    });
    let Some(entry) = p.entry.clone() else {
        return;
    };
    if let Some(sh) = media.filter(|_| crate::media::media_type(&entry).is_some()) {
        let req = crate::media::Req { entry, real };
        if let Some(m) = sh.meta(&req, false) {
            p.media = (crate::media_viewer::meta_rows(&m).into_iter())
                .map(|(k, v)| (k.to_owned(), v))
                .collect();
        }
    }
    if let Some(l) = &local {
        if p.kind != "Folder" && p.drive.is_none() {
            p.apps = crate::platform::apps_for(l).unwrap_or_default();
        }
        #[cfg(target_os = "macos")]
        {
            p.native = keel_core::finder_tags::read_native(l).unwrap_or_default();
        }
    }
}

fn date(t: Option<SystemTime>) -> String {
    t.map(|t| {
        chrono::DateTime::<chrono::Local>::from(t)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    })
    .unwrap_or_else(|| "—".into())
}

/// "2 files, 1 folder": the Contains row.
fn contains(files: u64, folders: u64) -> String {
    let n = |n: u64, one: &str| match n {
        1 => format!("1 {one}"),
        n => format!("{n} {one}s"),
    };
    format!("{}, {}", n(files, "file"), n(folders, "folder"))
}

/// `name` split for Name & Extension: the extension (with its dot) when there is one.
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// One info window.
pub struct InfoWindow {
    pub id: u64,
    pub paths: Vec<VPath>,
    /// Follows the active pane's selection (Alt+Ctrl+I).
    pub follow: bool,
    /// None while the worker is reading.
    pub info: Option<Result<Props, String>>,
    /// The request the shown answer is for (older answers lose).
    req: u64,
    /// Read again at this time (the following window's debounce, after a rename).
    due: Option<Instant>,
    /// Name & Extension: the name being edited, with or without its extension.
    pub name: String,
    pub hide_ext: bool,
    /// Comments: the text in the box and the text last read or saved.
    pub comment: String,
    saved_comment: String,
    /// Open with: the application picked.
    app: usize,
    open: bool,
}

impl InfoWindow {
    fn new(id: u64, paths: Vec<VPath>, follow: bool) -> Self {
        Self {
            id,
            paths,
            follow,
            info: None,
            req: 0,
            due: None,
            name: String::new(),
            hide_ext: false,
            comment: String::new(),
            saved_comment: String::new(),
            app: 0,
            open: true,
        }
    }

    pub fn title(&self) -> String {
        let name = match &self.info {
            Some(Ok(p)) => p.name.clone(),
            _ => match self.paths.as_slice() {
                [one] => one.name().to_owned(),
                many => format!("{} items", many.len()),
            },
        };
        format!("{name} Info")
    }

    /// The full name the Name field stands for.
    fn full_name(&self, ext: &str) -> String {
        match self.hide_ext {
            true => format!("{}{ext}", self.name.trim()),
            false => self.name.trim().to_owned(),
        }
    }
}

/// The open info windows.
#[derive(Default)]
pub struct InfoWindows {
    pub windows: Vec<InfoWindow>,
    next: u64,
}

impl AppState {
    /// Get Info: a new window for the selection (else the folder shown); one already
    /// showing exactly these items comes to the front. `follow`: the window that follows
    /// the selection (opened, or closed when it is open).
    pub(crate) fn open_info(&mut self, p: usize, follow: bool) {
        if follow {
            if let Some(i) = self.info.windows.iter().position(|w| w.follow) {
                self.info.windows.remove(i);
                return;
            }
        }
        let mut paths = self.target_paths(p);
        if paths.is_empty() && !self.tab(p).is_search() {
            paths.push(self.tab(p).dir.clone());
        }
        if paths.is_empty() {
            return;
        }
        if !follow {
            if let Some(w) = self
                .info
                .windows
                .iter()
                .find(|w| !w.follow && w.paths == paths)
            {
                let layer = egui::LayerId::new(egui::Order::Middle, window_id(w.id));
                self.ctx.move_to_top(layer);
                return;
            }
        }
        self.info.next += 1;
        let w = InfoWindow::new(self.info.next, paths, follow);
        self.info.windows.push(w);
        let i = self.info.windows.len() - 1;
        self.gather_info(i);
    }

    /// Reads window `i`'s items on a worker (answered as `Msg::Properties`).
    fn gather_info(&mut self, i: usize) {
        let w = &mut self.info.windows[i];
        w.req += 1;
        w.due = None;
        let (id, req, paths) = (w.id, w.req, w.paths.clone());
        let l = &self.library;
        let lib = Library {
            backend: l.backend.clone().filter(|_| !l.lost),
            sources: l.sources.clone(),
        };
        let media = self.media.sh.clone();
        let (router, tx, ctx) = (self.router.clone(), self.tx.clone(), self.ctx.clone());
        worker::spawn("keel-info", move || {
            let mut info = gather(&router, &paths);
            if let (Ok(p), [one]) = (&mut info, paths.as_slice()) {
                gather_extras(p, one, &lib, Some(&*media));
            }
            let info = Box::new(info.map_err(|e| format!("{e:#}")));
            worker::send(&tx, &ctx, Msg::Properties { id, req, info });
        });
    }

    /// A worker's answer for window `id`, kept when it is for its latest request.
    pub(crate) fn info_answer(&mut self, id: u64, req: u64, info: Result<Props, String>) {
        let Some(w) = self.info.windows.iter_mut().find(|w| w.id == id) else {
            return;
        };
        if w.req != req {
            return;
        }
        if let Ok(p) = &info {
            let (stem, _) = split_ext(&p.name);
            w.name = match w.hide_ext {
                true => stem.to_owned(),
                false => p.name.clone(),
            };
            let comment = p.comment.clone().and_then(Result::ok).flatten();
            w.comment = comment.clone().unwrap_or_default();
            w.saved_comment = w.comment.clone();
            w.app = 0;
        }
        w.info = Some(info);
    }

    /// The following window takes the active pane's selection once it has settled; the
    /// others read again when due (after a rename or a lock change).
    fn update_info(&mut self) {
        let selection = || {
            let p = self.active;
            let mut paths = self.target_paths(p);
            if paths.is_empty() && !self.tab(p).is_search() {
                paths.push(self.tab(p).dir.clone());
            }
            paths
        };
        if self.info.windows.iter().any(|w| w.follow) {
            let now = selection();
            let w = self.info.windows.iter_mut().find(|w| w.follow);
            if let Some(w) = w.filter(|w| w.paths != now && !now.is_empty()) {
                w.paths = now;
                w.due = Some(Instant::now() + DEBOUNCE);
            }
        }
        let now = Instant::now();
        let due: Vec<usize> = (0..self.info.windows.len())
            .filter(|&i| self.info.windows[i].due.is_some_and(|d| d <= now))
            .collect();
        for i in due {
            self.gather_info(i);
        }
        if let Some(next) = self.info.windows.iter().filter_map(|w| w.due).min() {
            self.ctx
                .request_repaint_after(next.saturating_duration_since(now));
        }
    }

    /// Locked (read-only) on or off for one local item, then read again.
    pub(crate) fn set_locked(&mut self, path: PathBuf, on: bool) {
        let set = keel_vfs::fsattr::Attributes {
            readonly: Some(on),
            ..Default::default()
        };
        if let Err(e) = keel_vfs::fsattr::set_attributes(&path, &set) {
            return self.toasts.error(format!("Locked: {e:#}"));
        }
        for w in &mut self.info.windows {
            if w.paths
                .iter()
                .any(|p| p.to_local_path().as_ref() == Some(&path))
            {
                w.due = Some(Instant::now());
            }
        }
    }
}

fn window_id(id: u64) -> egui::Id {
    egui::Id::new(("keel-info", id))
}

/// Draws every info window; their answers go to `out`.
pub fn windows(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    s.update_info();
    let mut wins = std::mem::take(&mut s.info.windows);
    for (n, w) in wins.iter_mut().enumerate() {
        let id = window_id(w.id);
        let mut open = w.open;
        crate::a11y::esc_closes_id(ctx, id, &mut open);
        // Toward the top right, as Finder places them; each one a step down and left.
        let step = 24.0 * (n % 8) as f32;
        let at = egui::pos2(ctx.screen_rect().right() - 360.0 - step, 60.0 + step);
        egui::Window::new(w.title())
            .id(id)
            .open(&mut open)
            .default_pos(at)
            .default_width(330.0)
            .resizable(true)
            .collapsible(true)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ctx.screen_rect().height() * 0.8)
                    .show(ui, |ui| body(ui, s, w, out));
            });
        w.open = open;
    }
    wins.retain(|w| w.open);
    // Windows opened while these were drawn come after them.
    wins.append(&mut s.info.windows);
    s.info.windows = wins;
}

fn body(ui: &mut egui::Ui, s: &mut AppState, w: &mut InfoWindow, out: &mut Vec<Action>) {
    if w.follow {
        ui.weak("Follows the selection");
    }
    let p = match &w.info {
        None => {
            ui.horizontal(|ui| {
                crate::anim::spinner(ui);
                ui.label("Reading…");
            });
            return;
        }
        Some(Err(e)) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
            return;
        }
        Some(Ok(p)) => p.clone(),
    };
    let n = w.paths.len();
    header(ui, s, &p, n);
    for section in sections(&p, n) {
        CollapsingHeader::new(section.title())
            .id_salt((w.id, section.title()))
            .default_open(section.open_at_first())
            .show(ui, |ui| match section {
                Section::General => general(ui, s, w, &p, out),
                Section::MoreInfo => more_info(ui, &p),
                Section::NameExt => name_ext(ui, w, &p, out),
                Section::Comments => comments(ui, w, &p, out),
                Section::OpenWith => open_with(ui, w, &p, out),
                Section::Preview => preview(ui, s, &p),
                Section::Sharing => sharing(ui, &p, out),
            });
    }
}

/// The icon, name and size at the top.
fn header(ui: &mut egui::Ui, s: &mut AppState, p: &Props, n: usize) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(32.0, 32.0), egui::Sense::hover());
        match &p.entry {
            Some(e) => {
                let style = s.library.style_of(&e.path);
                crate::folder_style::paint_entry(ui, rect, e, style, true);
            }
            None => {
                let folder =
                    egui::Image::new(crate::icons::folder()).fit_to_exact_size(rect.size());
                folder.paint_at(ui, rect);
            }
        }
        ui.vertical(|ui| {
            ui.add(egui::Label::new(RichText::new(&p.name).strong()).truncate());
            let size = match p.drive {
                Some(d) => format!("{} available", size_text_of(d.free)),
                None if p.partial => format!("at least {}", size_text_of(p.bytes)),
                None => size_text_of(p.bytes),
            };
            ui.weak(size);
            if n == 1 {
                ui.weak(format!("Modified: {}", date(p.modified)));
            }
        });
    });
    ui.add_space(4.0);
}

fn grid(ui: &mut egui::Ui, id: (u64, &str), rows: &[(&str, String)]) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            for (k, v) in rows {
                ui.weak(*k);
                ui.add(egui::Label::new(v.as_str()).wrap());
                ui.end_row();
            }
        });
}

fn general(ui: &mut egui::Ui, s: &AppState, w: &InfoWindow, p: &Props, out: &mut Vec<Action>) {
    grid(ui, (w.id, "general"), &general_rows(p, w.paths.len()));
    ui.add_space(4.0);
    // Tags: the dots and names the items carry (library and, on macOS, Finder), a color
    // row that toggles them, Tags… for the rest.
    let l = &s.library;
    if let [one] = w.paths.as_slice() {
        let mut dots = l.dots(one);
        for t in &p.native {
            let known = dots
                .iter()
                .any(|d| keel_core::finder_tags::same_name(&d.name, &t.name));
            if let (false, Some(color)) = (known, t.color.hex().and_then(crate::tag_dots::color32))
            {
                dots.push(crate::tag_dots::Dot {
                    color,
                    name: t.name.clone(),
                });
            }
        }
        let real = crate::library::real_of(&l.sources, one).unwrap_or_else(|| one.clone());
        let on = l.tagged.get(&real).map_or(&[][..], |t| t.as_slice());
        let mut names: Vec<String> = (l.tags.iter())
            .filter(|t| on.contains(&t.id))
            .map(|t| t.name.clone())
            .collect();
        for t in &p.native {
            if !names
                .iter()
                .any(|n| keel_core::finder_tags::same_name(n, &t.name))
            {
                names.push(t.name.clone());
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.weak("Tags");
            crate::tag_dots::show(ui, &dots);
            ui.label(if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            });
        });
    }
    let carries = |color: FinderColor| {
        w.paths.iter().all(|x| {
            let native = match w.paths.len() {
                1 => &p.native[..],
                _ => &[][..],
            };
            l.carries_color(x, color)
                || native
                    .iter()
                    .any(|t| keel_core::finder_tags::same_name(&t.name, color.name()))
        })
    };
    if let Some(color) = crate::tag_dots::color_row(ui, carries) {
        out.push(Action::Library(LibCmd::ToggleColor {
            color,
            paths: Some(w.paths.clone()),
        }));
    }
    if l.is_open() && ui.small_button("Tags…").clicked() {
        out.push(Action::Library(LibCmd::TagPickerFor(w.paths.clone())));
    }
    // Locked: read-only for one local item.
    let local = w.paths.first().and_then(|x| x.to_local_path());
    if let ([_], Some(local)) = (w.paths.as_slice(), local) {
        if p.drive.is_none() {
            let mut locked = p.read_only;
            let r = ui.checkbox(&mut locked, "Locked (read-only)");
            if r.changed() {
                out.push(Action::SetLocked(local, locked));
            }
        }
    }
}

fn more_info(ui: &mut egui::Ui, p: &Props) {
    let mut rows: Vec<(&str, String)> = p
        .media
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    if let Some(e) = &p.entry {
        if !e.ext.is_empty() && e.kind != Kind::Dir {
            rows.push(("Extension", format!(".{}", e.ext)));
        }
    }
    if rows.is_empty() {
        ui.weak("No more information");
    } else {
        egui::Grid::new(("more", p.name.as_str()))
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                for (k, v) in &rows {
                    ui.weak(*k);
                    ui.add(egui::Label::new(v.as_str()).wrap());
                    ui.end_row();
                }
            });
    }
}

fn name_ext(ui: &mut egui::Ui, w: &mut InfoWindow, p: &Props, out: &mut Vec<Action>) {
    let Some(path) = w.paths.first().cloned() else {
        return;
    };
    let folder = p.entry.as_ref().is_some_and(|e| e.kind == Kind::Dir);
    let ext = match folder {
        true => "",
        false => split_ext(&p.name).1,
    };
    let r = ui.add(
        egui::TextEdit::singleline(&mut w.name)
            .desired_width(f32::INFINITY)
            .hint_text("Name"),
    );
    let r = r.named("Name");
    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if !folder && !ext.is_empty() {
        let mut hide = w.hide_ext;
        if ui.checkbox(&mut hide, "Hide extension").changed() {
            // The field keeps what was typed, with or without the extension.
            let typed = w.full_name(ext);
            w.hide_ext = hide;
            w.name = match hide {
                true => split_ext(&typed).0.to_owned(),
                false => typed,
            };
        }
    }
    let to = w.full_name(ext);
    let changed = !to.is_empty() && to != p.name;
    let rename = ui.add_enabled(changed, egui::Button::new("Rename"));
    if changed && (rename.clicked() || enter) {
        out.push(Action::RenameTo {
            from: path.clone(),
            to: to.clone(),
        });
        if let Some(dir) = path.parent() {
            w.paths = vec![dir.join(&to)];
            w.due = Some(Instant::now() + REREAD);
        }
    }
}

fn comments(ui: &mut egui::Ui, w: &mut InfoWindow, p: &Props, out: &mut Vec<Action>) {
    match &p.comment {
        Some(Ok(_)) => {
            let r = ui.add(
                egui::TextEdit::multiline(&mut w.comment)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text("Add a comment"),
            );
            let r = r.named("Comments");
            let changed = w.comment != w.saved_comment;
            let save = ui.add_enabled(changed, egui::Button::new("Save comment"));
            if changed && (save.clicked() || r.lost_focus()) {
                if let Some(path) = w.paths.first() {
                    out.push(Action::Library(LibCmd::SetComment {
                        path: path.clone(),
                        text: w.comment.clone(),
                    }));
                    w.saved_comment = w.comment.clone();
                }
            }
        }
        Some(Err(why)) => {
            let mut empty = String::new();
            let off = egui::TextEdit::multiline(&mut empty)
                .desired_rows(2)
                .desired_width(f32::INFINITY);
            ui.add_enabled(false, off).named("Comments");
            ui.add(egui::Label::new(RichText::new(why).small().weak()).wrap());
        }
        None => {}
    }
}

fn open_with(ui: &mut egui::Ui, w: &mut InfoWindow, p: &Props, out: &mut Vec<Action>) {
    let Some(local) = w.paths.first().and_then(|x| x.to_local_path()) else {
        return;
    };
    if p.apps.is_empty() {
        // Windows: Explorer's own chooser below.
        if !cfg!(windows) {
            ui.weak("No applications found for this file");
        }
    } else {
        w.app = w.app.min(p.apps.len() - 1);
        egui::ComboBox::from_id_salt((w.id, "open-with"))
            .selected_text(p.apps[w.app].0.as_str())
            .width(ui.available_width().min(260.0))
            .show_ui(ui, |ui| {
                for (i, (name, _)) in p.apps.iter().enumerate() {
                    ui.selectable_value(&mut w.app, i, name.as_str());
                }
            });
    }
    ui.horizontal_wrapped(|ui| {
        let app = p.apps.get(w.app).map(|(_, app)| app.clone());
        if let Some(app) = app {
            if ui.button("Open").clicked() {
                out.push(Action::OpenWithApp {
                    paths: vec![local.clone()],
                    app: app.clone(),
                    remember: false,
                });
            }
            let ext = p.entry.as_ref().map(|e| e.ext.as_str()).unwrap_or_default();
            if !ext.is_empty() {
                let all = ui.button(format!("Use for all .{ext} files"));
                if all
                    .tip("Keel opens these files with this application")
                    .clicked()
                {
                    out.push(Action::OpenWithApp {
                        paths: vec![local.clone()],
                        app,
                        remember: true,
                    });
                }
            }
        }
        #[cfg(windows)]
        if ui.button("Choose app…").clicked() {
            out.push(Action::OpenWithSystem(local.clone()));
        }
        if ui.button("Other…").clicked() {
            out.push(Action::OpenWithBrowse {
                paths: vec![local.clone()],
                remember: false,
            });
        }
    });
}

fn preview(ui: &mut egui::Ui, s: &mut AppState, p: &Props) {
    let Some(e) = &p.entry else {
        ui.weak("No preview");
        return;
    };
    let side = ui.available_width().clamp(64.0, 240.0);
    match s.thumbs.get(e) {
        Some((tex, size)) => {
            let size = size / ui.ctx().pixels_per_point();
            let scale = (side / size.x).min(side / size.y).min(1.0);
            ui.add(egui::Image::new((tex, size * scale)));
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(96.0, 96.0), egui::Sense::hover());
            let style = s.library.style_of(&e.path);
            crate::folder_style::paint_entry(ui, rect, e, style, true);
        }
    }
}

fn sharing(ui: &mut egui::Ui, p: &Props, out: &mut Vec<Action>) {
    let Some(sharing) = &p.sharing else { return };
    let s = match sharing {
        Ok(s) => s,
        Err(e) => {
            ui.weak(format!("Not readable: {e}"));
            return;
        }
    };
    if let Some(owner) = &s.owner {
        ui.horizontal(|ui| {
            ui.weak("Owner");
            ui.label(owner);
        });
    }
    if let Some(mode) = s.mode {
        let dir = p.entry.as_ref().is_some_and(|e| e.kind == Kind::Dir);
        ui.horizontal(|ui| {
            ui.weak("Mode");
            ui.monospace(format!(
                "{} ({mode:o})",
                keel_vfs::sharing::mode_text(mode, dir)
            ));
        });
    }
    egui::Grid::new(("sharing", p.name.as_str()))
        .num_columns(2)
        .striped(true)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            ui.strong("Name");
            ui.strong("Privilege");
            ui.end_row();
            for a in &s.entries {
                ui.label(a.who.as_str());
                let mut text = match a.deny {
                    true => format!("Deny: {}", a.access),
                    false => a.access.clone(),
                };
                if a.inherited {
                    text.push_str(" (inherited)");
                }
                ui.label(text);
                ui.end_row();
            }
        });
    #[cfg(windows)]
    if let Some(local) = p.entry.as_ref().and_then(|e| e.path.to_local_path()) {
        if ui.button("Windows Security…").clicked() {
            out.push(Action::ShellProperties(local));
        }
    }
    #[cfg(not(windows))]
    let _ = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-info-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn contains_row_counts_in_the_singular_too() {
        assert_eq!(contains(2, 1), "2 files, 1 folder");
        assert_eq!(contains(1, 0), "1 file, 0 folders");
        assert_eq!(contains(0, 3), "0 files, 3 folders");
    }

    #[test]
    fn names_split_at_their_extension() {
        assert_eq!(split_ext("report.final.pdf"), ("report.final", ".pdf"));
        assert_eq!(split_ext(".bashrc"), (".bashrc", ""));
        assert_eq!(split_ext("README"), ("README", ""));
        assert_eq!(split_ext("trailing."), ("trailing.", ""));
    }

    /// The model for a file, a folder, several items and a drive: totals, kind, the
    /// sections shown and General's rows.
    #[test]
    fn info_model_for_file_folder_several_and_drive() {
        let dir = scratch("model");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("one.txt"), "12345").unwrap();
        std::fs::write(dir.join("a/two.txt"), "123").unwrap();
        std::fs::write(dir.join("a/b/three.bin"), "1").unwrap();
        let router = Router::new();
        // A folder.
        let folder = gather(&router, &[VPath::local(&dir)]).unwrap();
        assert_eq!(
            (
                folder.kind.as_str(),
                folder.files,
                folder.folders,
                folder.bytes
            ),
            ("Folder", 3, 2, 9)
        );
        assert!(!folder.partial);
        assert_eq!(
            folder.entry.as_ref().map(|e| e.kind.clone()),
            Some(Kind::Dir)
        );
        let s = sections(&folder, 1);
        assert!(s.contains(&Section::NameExt) && s.contains(&Section::Comments));
        assert!(!s.contains(&Section::OpenWith), "folders have no Open with");
        let rows = general_rows(&folder, 1);
        let row =
            |rows: &[(&str, String)], k: &str| rows.iter().find(|r| r.0 == k).map(|r| r.1.clone());
        assert_eq!(
            row(&rows, "Contains").as_deref(),
            Some("3 files, 2 folders")
        );
        assert!(row(&rows, "Created").is_some() && row(&rows, "Modified").is_some());
        // A file, with its extras (no library: comments say how to get them).
        let one = VPath::local(dir.join("one.txt"));
        let mut file = gather(&router, std::slice::from_ref(&one)).unwrap();
        assert_eq!(
            (file.kind.as_str(), file.bytes, file.files),
            ("TXT file", 5, 1)
        );
        let lib = Library {
            backend: None,
            sources: Vec::new(),
        };
        gather_extras(&mut file, &one, &lib, None);
        assert_eq!(file.comment, Some(Err(COMMENTS_OFF.to_owned())));
        let sharing = file.sharing.clone().unwrap().unwrap();
        assert!(sharing.owner.is_some() && !sharing.entries.is_empty());
        let s = sections(&file, 1);
        assert_eq!(
            s,
            [
                Section::General,
                Section::MoreInfo,
                Section::NameExt,
                Section::Comments,
                Section::OpenWith,
                Section::Preview,
                Section::Sharing
            ]
        );
        assert_eq!(row(&general_rows(&file, 1), "Contains"), None);
        // Several items: a summary with General only.
        let two = [one.clone(), VPath::local(dir.join("a"))];
        let several = gather(&router, &two).unwrap();
        assert_eq!(
            (
                several.name.as_str(),
                several.files,
                several.folders,
                several.bytes
            ),
            ("2 items", 3, 2, 9)
        );
        assert!(several.entry.is_none() && several.modified.is_none());
        assert_eq!(sections(&several, 2), [Section::General]);
        let rows = general_rows(&several, 2);
        assert_eq!(row(&rows, "Kind").as_deref(), Some("Several items"));
        assert_eq!(row(&rows, "Created"), None);
        // A drive: never walked; capacity instead of size; no Name & Extension.
        let root = dir.ancestors().last().unwrap().to_path_buf();
        let drive = VPath::local(&root);
        assert!(is_drive(&drive) && !is_drive(&one));
        let d = gather(&router, std::slice::from_ref(&drive)).unwrap();
        assert_eq!(d.kind, "Drive");
        assert!(d.drive.is_some_and(|s| s.total > 0), "{:?}", d.drive);
        assert_eq!((d.files, d.folders), (0, 0));
        assert!(!sections(&d, 1).contains(&Section::NameExt));
        let rows = general_rows(&d, 1);
        assert!(row(&rows, "Capacity").is_some() && row(&rows, "Size").is_none());
        assert!(gather(&router, &[VPath::local(dir.join("missing"))]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_draw_every_state() {
        let ctx = egui::Context::default();
        let mut w = InfoWindow::new(1, vec![VPath::local(std::env::temp_dir())], false);
        assert!(w.title().ends_with(" Info"));
        w.hide_ext = true;
        w.name = "report".into();
        assert_eq!(w.full_name(".pdf"), "report.pdf");
        w.hide_ext = false;
        w.name = " notes.txt ".into();
        assert_eq!(w.full_name(".txt"), "notes.txt");
        let file = Props {
            name: "x.txt".into(),
            kind: "TXT file".into(),
            comment: Some(Err(COMMENTS_OUTSIDE.into())),
            apps: vec![("Editor".into(), "editor".into())],
            sharing: Some(Ok(Sharing {
                owner: Some("me".into()),
                mode: Some(0o644),
                entries: keel_vfs::sharing::unix_entries("me", "staff", 0o644),
                ..Sharing::default()
            })),
            media: vec![("Dimensions".into(), "4 x 3".into())],
            ..Props::default()
        };
        for p in [file, Props::default()] {
            let mut out = Vec::new();
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    more_info(ui, &p);
                    name_ext(ui, &mut w, &p, &mut out);
                    comments(ui, &mut w, &p, &mut out);
                    open_with(ui, &mut w, &p, &mut out);
                    sharing(ui, &p, &mut out);
                });
            });
            assert!(out.is_empty(), "nothing is done without a click");
        }
    }
}
