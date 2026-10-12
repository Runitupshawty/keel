//! Left sidebar (Phase 14, Finder style): **Favorites** (user-arranged: drag folders in,
//! drag rows to reorder, right-click to remove), **Locations** (drives with an eject button
//! for removable ones, the trash, WSL (Task 51), open archives, remote hosts (Phase 3),
//! cloud accounts (Phase 4), paired devices), **Library** and **Tags** (Task 29). Each
//! section's header hides it (Hide / Show on hover); the layout is kept per profile in
//! `[sidebar]` (`sidebar_prefs`).

use crate::keys::Action;
use crate::library::{LibCmd, RECENTS_QUERY};
use crate::pane::DragPayload;
use crate::sidebar_glyphs::{self as glyphs, Glyph};
use crate::sidebar_prefs::{folders_in, Favorite, Place, Section, SidebarCmd, SidebarSettings};
use crate::state::{AppState, Msg};
use crate::theme::Theme;
use egui::{pos2, vec2, Align, Align2, Id, Layout, Rect, Sense, Shape, Stroke, UiBuilder};
use humansize::{format_size, DECIMAL};
use keel_vfs::{DriveKind, VPath};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// `(name, label, free bytes, total bytes)` as returned by `keel_vfs::drives()`.
pub type Drive = (String, String, u64, u64);

pub const DRIVES_REFRESH: Duration = Duration::from_secs(30);
/// A drive list that has not answered after this long (a dead network volume) is shown
/// as not responding; no new request starts until it answers.
pub const DRIVES_TIMEOUT: Duration = Duration::from_secs(20);

/// The sidebar's sizes (px), until the design tokens of `design.rs` take their place.
pub mod metrics {
    /// A row (the pill's height).
    pub const ROW_H: f32 = 24.0;
    /// A section header.
    pub const HEADER_H: f32 = 22.0;
    /// The selection pill's corner radius.
    pub const PILL_RADIUS: f32 = 5.0;
    /// Inside the pill, left and right.
    pub const PAD_X: f32 = 6.0;
    /// Icon size.
    pub const GLYPH: f32 = 16.0;
    /// Between the icon and the text.
    pub const GAP: f32 = 6.0;
    /// Above each section header.
    pub const SECTION_GAP: f32 = 8.0;
    /// Remote bookmarks under their host.
    pub const INDENT: f32 = 14.0;
}
use metrics::*;

/// A Favorites row being dragged (its index in `SidebarSettings::favorites`).
struct FavDrag(usize, String);

pub struct Sidebar {
    /// Favorites' places on this computer (resolved once; tests put fixtures here).
    pub places: HashMap<Place, VPath>,
    pub drives: Vec<Drive>,
    /// Each drive's kind (by name), answered after the drive list.
    pub kinds: HashMap<String, DriveKind>,
    /// When the last `drives()` request was sent (None = never).
    pub drives_requested: Option<Instant>,
    /// A `drives()` request is running (since then).
    pub drives_pending: Option<Instant>,
    /// The running request passed `DRIVES_TIMEOUT`.
    pub drives_stuck: bool,
    /// Installed WSL distributions, the default first (refreshed with the drives); none
    /// are shown while empty.
    pub wsl: Vec<String>,
    /// The distribution being opened (`Action::OpenWsl`), shown as starting.
    pub wsl_starting: Option<String>,
    /// Opens a distribution (`wsl::open`; tests stub it). Runs on a worker.
    pub wsl_open: fn(&str) -> anyhow::Result<VPath>,
    /// Tests that stub `wsl` set this so the 30 s drives refresh does not replace the list.
    pub wsl_frozen: bool,
    /// Configured SFTP hosts (`sidebar_remotes`), rebuilt each frame by the state.
    pub remotes: Vec<crate::sidebar_remotes::RemoteRow>,
    /// Configured cloud accounts, rebuilt each frame by the state.
    pub clouds: Vec<crate::sidebar_remotes::CloudRow>,
    /// Paired devices (None while devices are not running), rebuilt each frame.
    pub devices: Option<Vec<crate::devices::DeviceRow>>,
    /// Why devices are not running.
    pub devices_note: Option<String>,
}

impl Default for Sidebar {
    fn default() -> Self {
        let places = (Place::ALL.into_iter())
            .filter_map(|p| Some((p, p.resolve()?)))
            .collect();
        Self {
            places,
            drives: Vec::new(),
            kinds: HashMap::new(),
            drives_requested: None,
            drives_pending: None,
            drives_stuck: false,
            wsl: Vec::new(),
            wsl_starting: None,
            wsl_open: crate::wsl::open,
            wsl_frozen: false,
            remotes: Vec::new(),
            clouds: Vec::new(),
            devices: None,
            devices_note: None,
        }
    }
}

/// The icon of a place.
fn place_glyph(p: Place) -> Glyph {
    match p {
        Place::Recents => Glyph::Recents,
        Place::Applications => Glyph::Applications,
        Place::Desktop => Glyph::Desktop,
        Place::Documents => Glyph::Documents,
        Place::Downloads => Glyph::Downloads,
        Place::Home => Glyph::Home,
    }
}

impl Sidebar {
    /// The drive whose mount point holds `dir` (whole path components: `/media/usb` is
    /// not the drive of `/media/usb2`), the deepest one when mounts nest.
    pub fn drive_of(&self, dir: &VPath) -> Option<&Drive> {
        drive_in(&self.drives, dir)
    }

    /// The drives' kinds arrived (kept while a harness froze the drive list).
    pub fn set_kinds(&mut self, kinds: Vec<(String, DriveKind)>) {
        let frozen = (self.drives_requested).is_some_and(|at| at > Instant::now());
        if !frozen {
            self.kinds = kinds.into_iter().collect();
        }
    }

    pub fn place(&self, p: Place) -> Option<VPath> {
        self.places.get(&p).cloned()
    }

    /// Draws the sidebar; clicks become `Navigate`, middle-clicks `NewTabAt`, layout changes
    /// `Action::Sidebar`. `archives`: the archive each (pane, tab) is browsing; a click
    /// shows that tab.
    #[allow(clippy::too_many_arguments)]
    pub fn ui(
        &self,
        ui: &mut egui::Ui,
        theme: &Theme,
        prefs: &SidebarSettings,
        current: &VPath,
        archives: &[(usize, usize, VPath)],
        library: &crate::library::LibraryUi,
        out: &mut Vec<Action>,
    ) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.style_mut().interaction.selectable_labels = false;
            ui.spacing_mut().item_spacing.y = 1.0;
            let fav_menu = |ui: &mut egui::Ui, out: &mut Vec<Action>| {
                menu_item(ui, "Add current folder", out, || {
                    SidebarCmd::PinFolder(current.clone())
                });
                menu_item(ui, "Restore default favorites", out, || {
                    SidebarCmd::ResetFavorites
                });
            };
            if header(ui, Section::Favorites, prefs, theme, out, fav_menu) {
                self.favorites(ui, prefs, current, out);
            }
            ui.add_space(SECTION_GAP);
            if header(ui, Section::Locations, prefs, theme, out, add_menu) {
                self.locations(ui, current, archives, out);
            }
            ui.add_space(SECTION_GAP);
            if header(ui, Section::Library, prefs, theme, out, |_, _| ()) {
                crate::library_ui::sidebar(ui, library, current, out);
            }
            if library.is_open() && !library.tags.is_empty() {
                ui.add_space(SECTION_GAP);
                if header(ui, Section::Tags, prefs, theme, out, |_, _| ()) {
                    crate::library_ui::sidebar_tags(ui, library, out);
                }
            }
            ui.add_space(SECTION_GAP);
        });
    }

    /// Favorites: places and pinned folders in the user's order. Rows drag to reorder;
    /// folders dragged in from a pane are pinned between rows, or copied / moved into a
    /// folder when dropped on its middle.
    fn favorites(
        &self,
        ui: &mut egui::Ui,
        prefs: &SidebarSettings,
        current: &VPath,
        out: &mut Vec<Action>,
    ) {
        let places = |p: Place| self.place(p);
        let len = prefs.favorites.len();
        // (index, row, the folder it opens)
        let mut rows: Vec<(usize, Rect, Option<VPath>)> = Vec::new();
        for (i, fav) in prefs.favorites.iter().enumerate() {
            let target = fav.target(&places);
            let recents = matches!(
                fav,
                Favorite::Place {
                    place: Place::Recents
                }
            );
            if target.is_none() && !recents {
                continue; // a place this computer does not have
            }
            let glyph = match fav {
                Favorite::Place { place } => place_glyph(*place),
                Favorite::Folder { .. } => Glyph::Folder,
            };
            let label = fav.label();
            let selected = target.as_ref() == Some(current);
            let id = Id::new(("sidebar-favorite", i));
            let r = row(ui, id, glyph, &label, selected, Sense::click_and_drag(), |_| ());
            let r = match &target {
                Some(t) => r.on_hover_text(t.display()),
                None => r.on_hover_text("Files you opened lately (the library's Recents)"),
            };
            if r.clicked() {
                out.push(match &target {
                    Some(t) => Action::Navigate(t.clone()),
                    None => Action::Library(LibCmd::Query(RECENTS_QUERY.into())),
                });
            } else if r.middle_clicked() {
                if let Some(t) = &target {
                    out.push(Action::NewTabAt(t.clone()));
                }
            }
            r.dnd_set_drag_payload(FavDrag(i, label.clone()));
            r.context_menu(|ui| {
                if let Some(t) = &target {
                    if ui.button("Open in new tab").clicked() {
                        out.push(Action::NewTabAt(t.clone()));
                        ui.close_menu();
                    }
                }
                if i > 0 {
                    menu_item(ui, "Move up", out, || SidebarCmd::Move {
                        from: i,
                        to: i - 1,
                    });
                }
                if i + 1 < len {
                    menu_item(ui, "Move down", out, || SidebarCmd::Move {
                        from: i,
                        to: i + 2,
                    });
                }
                menu_item(ui, "Remove from Sidebar", out, || SidebarCmd::Unpin(i));
            });
            rows.push((i, r.rect, target));
        }
        if rows.is_empty() {
            let (_, rect) = ui.allocate_space(vec2(ui.available_width(), ROW_H));
            ui.painter().text(
                pos2(rect.left() + PAD_X, rect.center().y),
                Align2::LEFT_CENTER,
                "Drag folders here",
                egui::TextStyle::Body.resolve(ui.style()),
                ui.visuals().weak_text_color(),
            );
            // An empty list still takes drops: the placeholder is the zone.
            rows.push((len, rect, None));
        }
        drop_zone(ui, &rows, len, out);
    }

    /// Locations: drives, the trash, WSL, open archives, hosts, clouds and devices.
    fn locations(
        &self,
        ui: &mut egui::Ui,
        current: &VPath,
        archives: &[(usize, usize, VPath)],
        out: &mut Vec<Action>,
    ) {
        if self.drives_stuck {
            ui.colored_label(ui.visuals().warn_fg_color, "Drives are not responding")
                .on_hover_text("A drive (often a disconnected network drive) is not answering");
        } else if self.drives.is_empty() {
            ui.weak("Loading…");
        }
        for (name, label, free, total) in &self.drives {
            let text = if label.is_empty() || cfg!(not(windows)) {
                name.clone()
            } else {
                format!("{label} ({name})")
            };
            let path = VPath::local(drive_root(name));
            let kind = self.kinds.get(name).copied().unwrap_or(DriveKind::Fixed);
            let glyph = match kind {
                DriveKind::Network => Glyph::Network,
                DriveKind::Optical => Glyph::Optical,
                DriveKind::Fixed | DriveKind::Removable => Glyph::Drive,
            };
            let ejectable = ejectable(name, kind);
            let id = Id::new(("sidebar-drive", name));
            let r = row(ui, id, glyph, &text, &path == current, Sense::click(), |ui| {
                if ejectable && eject_button(ui, &text).clicked() {
                    out.push(Action::Sidebar(SidebarCmd::Eject(name.clone())));
                }
            });
            let mut tip = path.display();
            if *total > 0 {
                let used = 1.0 - *free as f32 / *total as f32;
                usage_line(ui, r.rect, used, ejectable);
                tip = format!(
                    "{tip}\n{} free of {}",
                    format_size(*free, DECIMAL),
                    format_size(*total, DECIMAL)
                );
            }
            let r = r.on_hover_text(tip);
            clicked(&r, &path, out);
        }
        if keel_vfs::trashbin::SUPPORTED {
            let root = keel_vfs::trashbin::root();
            let here = current.scheme == root.scheme;
            let label = keel_vfs::trashbin::label();
            let id = Id::new("sidebar-trash");
            let r = row(ui, id, Glyph::Trash, label, here, Sense::click(), |_| ());
            clicked(&r, &root, out);
        }
        for name in &self.wsl {
            let root = VPath::local(format!(r"\\wsl.localhost\{name}"));
            let starting = self.wsl_starting.as_ref() == Some(name);
            let id = Id::new(("sidebar-wsl", name));
            let here = current.path.starts_with(&root.path);
            let r = row(ui, id, Glyph::Terminal, name, here, Sense::click(), |ui| {
                ui.weak("WSL");
                if starting {
                    ui.weak("starting…");
                    ui.spinner();
                }
            })
            .on_hover_text(root.display());
            if r.clicked() {
                out.push(Action::OpenWsl(name.clone()));
            }
        }
        for (pane, tab, archive) in archives {
            let id = Id::new(("sidebar-archive", pane, tab));
            let r = row(ui, id, Glyph::Archive, archive.name(), false, Sense::click(), |_| ())
                .on_hover_text(archive.display());
            if r.clicked() {
                out.push(Action::FocusTab {
                    pane: *pane,
                    tab: *tab,
                });
            }
        }
        crate::sidebar_remotes::ui(ui, &self.remotes, current, out);
        crate::sidebar_remotes::cloud_ui(ui, &self.clouds, current, out);
        if let Some(devices) = &self.devices {
            let accent = ui.visuals().hyperlink_color;
            let note = self.devices_note.as_deref();
            crate::devices::sidebar(ui, Some(devices.as_slice()), note, current, accent, out);
        }
        if self.remotes.is_empty() && self.clouds.is_empty() {
            ui.scope(|ui| {
                ui.visuals_mut().button_frame = false;
                ui.menu_button("Add server or cloud…", |ui| add_menu(ui, out));
            });
        }
    }
}

/// The pin / reorder drop zone over `rows` (`len`: the Favorites count): an insertion line
/// between rows, a frame on a folder a pane's items would be dropped into.
fn drop_zone(
    ui: &mut egui::Ui,
    rows: &[(usize, Rect, Option<VPath>)],
    len: usize,
    out: &mut Vec<Action>,
) {
    let ctx = ui.ctx().clone();
    let moving = egui::DragAndDrop::payload::<FavDrag>(&ctx);
    let files = egui::DragAndDrop::payload::<DragPayload>(&ctx);
    if moving.is_none() && files.is_none() {
        return;
    }
    if let Some(m) = &moving {
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        egui::show_tooltip_at_pointer(
            &ctx,
            egui::LayerId::new(egui::Order::Tooltip, Id::new("keel-favorite-drag")),
            Id::new("keel-favorite-drag-tip"),
            |ui| ui.label(m.1.as_str()),
        );
    }
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return;
    };
    let zone = Rect::from_min_max(first.1.min, last.1.max).expand2(vec2(0.0, ROW_H / 2.0));
    let (pos, released) = ui.input(|i| (i.pointer.hover_pos(), i.pointer.any_released()));
    let Some(pos) = pos.filter(|p| zone.contains(*p)) else {
        return;
    };
    let accent = ui.visuals().hyperlink_color;
    // A pane's items over the middle half of a folder row go into that folder.
    let into = files.as_ref().and_then(|drag| {
        rows.iter().find_map(|(_, rect, target)| {
            let band = rect.shrink2(vec2(0.0, rect.height() / 4.0));
            let t = target.as_ref().filter(|t| !drag.paths.contains(*t))?;
            band.contains(pos).then(|| (*rect, t.clone()))
        })
    });
    if let Some((rect, target)) = into {
        let stroke = Stroke::new(2.0, accent);
        ui.painter()
            .rect_stroke(rect, PILL_RADIUS, stroke, egui::StrokeKind::Inside);
        if released {
            if let Some(drag) = egui::DragAndDrop::take_payload::<DragPayload>(&ctx) {
                out.push(drag.action(target));
            }
        }
        return;
    }
    let gap = gap_at(rows, pos.y, len);
    let y = (rows.iter().find(|(i, ..)| *i >= gap))
        .map_or(last.1.bottom(), |(_, rect, _)| rect.top());
    let line = [pos2(zone.left() + PAD_X, y), pos2(zone.right() - PAD_X, y)];
    ui.painter().line_segment(line, Stroke::new(2.0, accent));
    ui.painter().circle_stroke(line[0], 3.0, Stroke::new(1.5, accent));
    if !released {
        return;
    }
    if let Some(m) = egui::DragAndDrop::take_payload::<FavDrag>(&ctx) {
        out.push(Action::Sidebar(SidebarCmd::Move { from: m.0, to: gap }));
    } else if let Some(drag) = egui::DragAndDrop::take_payload::<DragPayload>(&ctx) {
        out.push(Action::Sidebar(SidebarCmd::Pin {
            pane: drag.pane,
            paths: drag.paths.clone(),
            at: Some(gap),
        }));
    }
}

/// The gap (a Favorites index, `len` = the end) a drop at height `y` goes into: before the
/// first shown row whose middle is below `y`.
pub fn gap_at(rows: &[(usize, Rect, Option<VPath>)], y: f32, len: usize) -> usize {
    (rows.iter())
        .find(|(_, rect, _)| y < rect.center().y)
        .map_or(len, |(i, ..)| *i)
}

/// A section header: its title, and Hide / Show while hovered or focused (a click
/// toggles). Right-click: Hide / Show and `menu`. Returns whether the section is shown.
fn header(
    ui: &mut egui::Ui,
    section: Section,
    prefs: &SidebarSettings,
    theme: &Theme,
    out: &mut Vec<Action>,
    menu: impl FnOnce(&mut egui::Ui, &mut Vec<Action>),
) -> bool {
    let open = !prefs.is_hidden(section);
    let title = section.title();
    let (_, rect) = ui.allocate_space(vec2(ui.available_width(), HEADER_H));
    let r = ui.interact(rect, Id::new(("sidebar-header", title)), Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, title));
    ui.ctx()
        .accesskit_node_builder(r.id, |b| b.set_expanded(open));
    let font = egui::TextStyle::Small.resolve(ui.style());
    let y = rect.center().y;
    let painter = ui.painter();
    painter.text(
        pos2(rect.left() + PAD_X, y),
        Align2::LEFT_CENTER,
        title,
        font.clone(),
        theme.muted(),
    );
    if r.hovered() || r.has_focus() {
        let word = if open { "Hide" } else { "Show" };
        let at = pos2(rect.right() - PAD_X, y);
        painter.text(at, Align2::RIGHT_CENTER, word, font, theme.muted());
    }
    let toggle = Action::Sidebar(SidebarCmd::Toggle(section));
    if r.clicked() {
        out.push(toggle.clone());
    }
    r.context_menu(|ui| {
        if ui.button(if open { "Hide" } else { "Show" }).clicked() {
            out.push(toggle);
            ui.close_menu();
        }
        menu(ui, out);
    });
    open
}

/// A context-menu entry that sends `cmd`.
fn menu_item(
    ui: &mut egui::Ui,
    text: &str,
    out: &mut Vec<Action>,
    cmd: impl FnOnce() -> SidebarCmd,
) {
    if ui.button(text).clicked() {
        out.push(Action::Sidebar(cmd()));
        ui.close_menu();
    }
}

/// Add an SFTP / FTP host or a cloud account (Settings → Remotes / Cloud).
fn add_menu(ui: &mut egui::Ui, out: &mut Vec<Action>) {
    if ui.button("Add SFTP or FTP host…").clicked() {
        out.push(Action::Remote {
            host: String::new(),
            cmd: crate::remotes::RemoteCmd::Add,
        });
        ui.close_menu();
    }
    if ui.button("Add cloud account…").clicked() {
        out.push(Action::Cloud {
            id: String::new(),
            cmd: crate::clouds::CloudCmd::Add,
        });
        ui.close_menu();
    }
}

/// One sidebar row: `glyph` in the accent, `text` cut to the width, a rounded pill when
/// `selected` (a fainter one on hover), and `trailing` widgets at the right end (badges,
/// the eject button). Screen readers hear `text` and whether it is selected.
pub(crate) fn row(
    ui: &mut egui::Ui,
    id: Id,
    glyph: Glyph,
    text: &str,
    selected: bool,
    sense: Sense,
    trailing: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let (_, rect) = ui.allocate_space(vec2(ui.available_width(), ROW_H));
    let r = ui.interact(rect, id, sense);
    r.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, text)
    });
    let pill = ui.painter().add(Shape::Noop);
    let inner = rect.shrink2(vec2(PAD_X, 0.0));
    let mut tail = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::right_to_left(Align::Center)),
    );
    trailing(&mut tail);
    let used = tail.min_rect();
    let right = if used.width() > 0.5 {
        used.left() - GAP
    } else {
        inner.right()
    };
    let visuals = ui.visuals().clone();
    let icon = glyphs::slot(rect, inner.left(), GLYPH);
    glyphs::paint(ui.painter(), icon, glyph, visuals.hyperlink_color);
    let left = icon.right() + GAP;
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        (right - left).max(0.0),
        egui::TextStyle::Body,
    );
    let color = if selected {
        visuals.strong_text_color()
    } else {
        visuals.text_color()
    };
    let at = pos2(left, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(at, galley, color);
    let fill = if selected {
        visuals.selection.bg_fill
    } else if ui.rect_contains_pointer(rect) {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter()
        .set(pill, Shape::rect_filled(rect, PILL_RADIUS, fill));
    r
}

/// A thin bar along the bottom of a drive row: how full it is (red past 90%).
fn usage_line(ui: &egui::Ui, rect: Rect, used: f32, eject: bool) {
    let left = rect.left() + PAD_X + GLYPH + GAP;
    let right = rect.right() - PAD_X - if eject { GLYPH + GAP } else { 0.0 };
    if right <= left {
        return;
    }
    let y = rect.bottom() - 2.5;
    let v = ui.visuals();
    let fill = if used > 0.9 {
        v.error_fg_color
    } else {
        v.hyperlink_color
    };
    let track = v.widgets.noninteractive.bg_stroke.color;
    let painter = ui.painter();
    painter.line_segment([pos2(left, y), pos2(right, y)], Stroke::new(2.0, track));
    let end = left + (right - left) * used.clamp(0.0, 1.0);
    painter.line_segment([pos2(left, y), pos2(end, y)], Stroke::new(2.0, fill));
}

/// The eject button of drive `what`.
fn eject_button(ui: &mut egui::Ui, what: &str) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(GLYPH, GLYPH), Sense::click());
    let v = ui.visuals();
    let color = if r.hovered() {
        v.strong_text_color()
    } else {
        v.text_color()
    };
    glyphs::paint(ui.painter(), rect.shrink(2.0), Glyph::Eject, color);
    let name = format!("Eject {what}");
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
    r.on_hover_text(name)
}

fn clicked(r: &egui::Response, path: &VPath, out: &mut Vec<Action>) {
    if r.clicked() {
        out.push(Action::Navigate(path.clone()));
    } else if r.middle_clicked() {
        out.push(Action::NewTabAt(path.clone()));
    }
}

/// A drive's root folder: `E:\` for `E:`, else its mount point.
fn drive_root(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}\\")
    } else {
        name.to_owned()
    }
}

/// Removable and optical drives get an eject button (never the system root).
pub fn ejectable(name: &str, kind: DriveKind) -> bool {
    matches!(kind, DriveKind::Removable | DriveKind::Optical) && name != "/"
}

/// Each drive's kind, on a worker (a dead mapped drive can block).
pub fn kinds_of(names: &[String]) -> Vec<(String, DriveKind)> {
    (names.iter())
        .map(|n| {
            let root = drive_root(n);
            (n.clone(), keel_vfs::drive_kind(std::path::Path::new(&root)))
        })
        .collect()
}

/// Ejects drive `name` on a worker; a failure is an error toast. The drive list is read
/// again either way.
fn spawn_eject(name: String, tx: crossbeam_channel::Sender<Msg>, ctx: egui::Context) -> bool {
    crate::worker::spawn("keel-eject", move || {
        if let Err(e) = eject(&name) {
            let text = format!("Could not eject {name}: {e:#}");
            crate::worker::send(&tx, &ctx, Msg::Toast(text));
        }
        let drives = keel_vfs::drives();
        let names: Vec<String> = drives.iter().map(|d| d.0.clone()).collect();
        crate::worker::send(&tx, &ctx, Msg::Drives(drives));
        crate::worker::send(&tx, &ctx, Msg::DriveKinds(kinds_of(&names)));
    })
}

/// Windows: the shell's Eject verb (what Explorer's Eject runs), then waits for the drive
/// to leave the list.
#[cfg(windows)]
fn eject(name: &str) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let b = name.as_bytes();
    anyhow::ensure!(
        b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':',
        "not a drive letter"
    );
    let script = format!(
        "(New-Object -ComObject Shell.Application).Namespace(17).ParseName('{name}')\
         .InvokeVerb('Eject'); Start-Sleep -Seconds 2"
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    for _ in 0..20 {
        let there = (keel_vfs::drives().iter()).any(|d| d.0.eq_ignore_ascii_case(name));
        if !there {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    anyhow::bail!("it is still in use")
}

/// macOS: `diskutil eject`; Linux: `gio mount --eject`, else `umount`.
#[cfg(not(windows))]
fn eject(mount: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        mount.starts_with('/') && mount != "/",
        "not a removable drive"
    );
    let tries: Vec<(&str, Vec<&str>)> = if cfg!(target_os = "macos") {
        vec![("diskutil", vec!["eject"])]
    } else {
        vec![("gio", vec!["mount", "--eject"]), ("umount", Vec::new())]
    };
    let mut last = anyhow::anyhow!("no eject command found");
    for (cmd, args) in tries {
        match std::process::Command::new(cmd)
            .args(args)
            .arg(mount)
            .output()
        {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => last = anyhow::anyhow!("{}", String::from_utf8_lossy(&out.stderr).trim()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => last = e.into(),
        }
    }
    Err(last)
}

impl AppState {
    /// `Action::Sidebar`: layout changes land in the profile's settings (saved like any
    /// setting); Eject runs on a worker.
    pub(crate) fn sidebar_cmd(&mut self, p: usize, cmd: SidebarCmd) {
        match cmd {
            SidebarCmd::Pin { pane, paths, at } => {
                let entries = self.panes.get(pane).map(|pane| &pane.tab().entries);
                let dirs = entries.map_or_else(Vec::new, |e| folders_in(e, &paths));
                self.pin_all(dirs, at);
            }
            SidebarCmd::PinSelection => {
                let tab = self.tab(p);
                let picked: Vec<VPath> = (tab.entries.iter())
                    .filter(|e| tab.selected.contains(&e.name))
                    .map(|e| e.path.clone())
                    .collect();
                let dirs = match picked.is_empty() {
                    true => vec![tab.dir.clone()],
                    false => folders_in(&tab.entries, &picked),
                };
                self.pin_all(dirs, None);
            }
            SidebarCmd::PinFolder(dir) => self.pin_all(vec![dir], None),
            SidebarCmd::Unpin(i) => {
                self.settings.sidebar.unpin(i);
            }
            SidebarCmd::Move { from, to } => {
                self.settings.sidebar.move_to(from, to);
            }
            SidebarCmd::Toggle(section) => self.settings.sidebar.toggle(section),
            SidebarCmd::ResetFavorites => {
                self.settings.sidebar.favorites = crate::sidebar_prefs::seeds();
            }
            SidebarCmd::Eject(name) => {
                self.toasts.info(format!("Ejecting {name}…"));
                if !spawn_eject(name, self.tx.clone(), self.ctx.clone()) {
                    self.toasts.error("Could not start ejecting");
                }
            }
        }
    }

    /// Pins `dirs` in order before row `at` (the end when None).
    fn pin_all(&mut self, dirs: Vec<VPath>, at: Option<usize>) {
        if dirs.is_empty() {
            return self.toasts.error("Only folders can be added to the sidebar");
        }
        let sidebar = &self.sidebar;
        let places = |p: Place| sidebar.place(p);
        let mut at = at;
        for dir in dirs {
            let row = self.settings.sidebar.pin(dir, at, &places);
            at = at.map(|_| row + 1);
        }
    }
}

/// The drive of `drives` a local folder is on (the longest mount point holding it).
pub fn drive_in<'a>(drives: &'a [Drive], dir: &VPath) -> Option<&'a Drive> {
    let fold = |s: String| if cfg!(windows) { s.to_lowercase() } else { s };
    let local = fold(dir.to_local_path()?.to_string_lossy().into_owned());
    let local = std::path::Path::new(&local);
    drives
        .iter()
        .filter(|(name, ..)| local.starts_with(fold(name.clone())))
        .max_by_key(|(name, ..)| name.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::Kind;
    use std::sync::Arc;

    #[test]
    fn drive_of_matches_whole_path_components() {
        let mut s = Sidebar::default();
        let drive = |n: &str| (n.to_owned(), String::new(), 1, 2);
        if cfg!(windows) {
            s.drives = vec![drive("C:"), drive("D:")];
            let of = |p: &str| s.drive_of(&VPath::local(p)).map(|d| d.0.clone());
            assert_eq!(of(r"c:\Users\x").as_deref(), Some("C:"));
            assert_eq!(of(r"D:\").as_deref(), Some("D:"));
            assert_eq!(of(r"E:\x"), None);
        } else {
            s.drives = vec![drive("/"), drive("/media/usb"), drive("/media/usb2")];
            let of = |p: &str| s.drive_of(&VPath::local(p)).map(|d| d.0.clone());
            assert_eq!(of("/media/usb2/photos").as_deref(), Some("/media/usb2"));
            assert_eq!(of("/media/usb/x").as_deref(), Some("/media/usb"));
            assert_eq!(of("/media/usbx").as_deref(), Some("/"));
        }
        let remote = VPath::parse("sftp://host/media/usb").unwrap();
        assert_eq!(s.drive_of(&remote), None);
    }

    #[test]
    fn only_removable_and_optical_drives_eject() {
        assert!(ejectable("E:", DriveKind::Removable));
        assert!(ejectable("/media/usb", DriveKind::Optical));
        assert!(!ejectable("C:", DriveKind::Fixed));
        assert!(!ejectable("Z:", DriveKind::Network));
        assert!(!ejectable("/", DriveKind::Removable));
    }

    #[test]
    fn drops_land_in_the_gap_under_the_pointer() {
        let at = |i: usize, top: f32| {
            let rect = Rect::from_min_size(pos2(0.0, top), vec2(100.0, ROW_H));
            (i, rect, None)
        };
        // Row 1 is a place this computer lacks (not drawn).
        let rows = [at(0, 0.0), at(2, 24.0), at(3, 48.0)];
        assert_eq!(gap_at(&rows, -5.0, 4), 0);
        assert_eq!(gap_at(&rows, 13.0, 4), 2, "below row 0's middle");
        assert_eq!(gap_at(&rows, 40.0, 4), 3);
        assert_eq!(gap_at(&rows, 70.0, 4), 4, "the end");
        assert_eq!(gap_at(&[], 0.0, 0), 0);
    }

    fn state(dir: &std::path::Path) -> AppState {
        let mut s = AppState::new(
            egui::Context::default(),
            Arc::new(keel_vfs::Router::new()),
            VPath::local(dir),
        );
        s.sidebar.places = [(Place::Home, VPath::local(dir))].into();
        s.settings.sidebar.favorites = vec![Favorite::Place { place: Place::Home }];
        s
    }

    /// The sidebar's commands change the profile's `[sidebar]` (saved like every setting):
    /// pin from a drop (folders only, at the gap), Add to Sidebar, reorder, remove, hide.
    #[test]
    fn sidebar_commands_change_the_profile() {
        let tmp = std::env::temp_dir().join(format!("keel-sidebar-cmds-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut s = state(&tmp);
        let dir = VPath::local(&tmp);
        let entries = vec![
            crate::tab::test_entry(&dir, "Work", Kind::Dir, 0),
            crate::tab::test_entry(&dir, "Photos", Kind::Dir, 0),
            crate::tab::test_entry(&dir, "notes.md", Kind::File, 3),
        ];
        s.tab_mut(0).entries = entries.clone();
        let labels = |s: &AppState| -> Vec<String> {
            (s.settings.sidebar.favorites.iter())
                .map(Favorite::label)
                .collect()
        };
        let cmd = |s: &mut AppState, c: SidebarCmd| s.run(0, Action::Sidebar(c));
        // A drop before row 0: the folders in order, the file left out.
        let paths = entries.iter().map(|e| e.path.clone()).collect();
        cmd(
            &mut s,
            SidebarCmd::Pin {
                pane: 0,
                paths,
                at: Some(0),
            },
        );
        assert_eq!(labels(&s), ["Work", "Photos", "Home"]);
        // Only a file: nothing pinned, a toast says why.
        let file = vec![entries[2].path.clone()];
        cmd(
            &mut s,
            SidebarCmd::Pin {
                pane: 0,
                paths: file,
                at: None,
            },
        );
        assert_eq!(labels(&s).len(), 3);
        assert!(s.toasts.list.iter().any(|t| t.text.contains("Only folders")));
        // Home is already there (as a place): Add to Sidebar adds nothing.
        cmd(&mut s, SidebarCmd::PinSelection);
        assert_eq!(labels(&s), ["Work", "Photos", "Home"]);
        cmd(&mut s, SidebarCmd::Move { from: 0, to: 3 });
        assert_eq!(labels(&s), ["Photos", "Home", "Work"]);
        cmd(&mut s, SidebarCmd::Unpin(1));
        assert_eq!(labels(&s), ["Photos", "Work"]);
        cmd(&mut s, SidebarCmd::Toggle(Section::Locations));
        assert!(s.settings.sidebar.is_hidden(Section::Locations));
        cmd(&mut s, SidebarCmd::ResetFavorites);
        assert_eq!(s.settings.sidebar.favorites, crate::sidebar_prefs::seeds());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The drive kinds a harness froze with the drive list stay as they are.
    #[test]
    fn frozen_drive_kinds_stay() {
        let mut s = Sidebar::default();
        s.set_kinds(vec![("E:".into(), DriveKind::Removable)]);
        assert_eq!(s.kinds.get("E:"), Some(&DriveKind::Removable));
        s.drives_requested = Some(Instant::now() + Duration::from_secs(60));
        s.set_kinds(Vec::new());
        assert_eq!(s.kinds.len(), 1);
    }
}
