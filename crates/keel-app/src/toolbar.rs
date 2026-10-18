//! The Finder style's unified toolbar (Task 65): in the title bar, Finder's items in
//! Finder's order, customizable per profile. Every item runs the same actions as the
//! menus, the palette and the keyboard; none is the only way to anything.

use crate::a11y::Named;
use crate::chrome::{self, Chrome};
use crate::design::{self, Style};
use crate::keys::Action;
use crate::pane::ViewMode;
use crate::state::AppState;
use crate::tab::SortKey;
use egui::{pos2, vec2, Align, Align2, Color32, FontId, Id, Rect, Response, Sense, Stroke, Ui};

/// One toolbar item; the order of `ALL` is Finder's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Item {
    Sidebar,
    Navigation,
    Title,
    /// Flexible space: pushes what follows to the right.
    Space,
    Views,
    Group,
    Share,
    Tags,
    Actions,
    Search,
}

impl Item {
    pub const ALL: [Item; 10] = [
        Item::Sidebar,
        Item::Navigation,
        Item::Title,
        Item::Space,
        Item::Views,
        Item::Group,
        Item::Share,
        Item::Tags,
        Item::Actions,
        Item::Search,
    ];

    /// The name in config.toml.
    pub fn key(self) -> &'static str {
        match self {
            Item::Sidebar => "sidebar",
            Item::Navigation => "navigation",
            Item::Title => "title",
            Item::Space => "space",
            Item::Views => "views",
            Item::Group => "group",
            Item::Share => "share",
            Item::Tags => "tags",
            Item::Actions => "actions",
            Item::Search => "search",
        }
    }

    pub fn from_key(key: &str) -> Option<Item> {
        Item::ALL.into_iter().find(|i| i.key() == key)
    }

    /// The label under the icon (Icon and Text) and in Customize Toolbar.
    pub fn label(self) -> &'static str {
        match self {
            Item::Sidebar => crate::i18n::tr("toolbar.item_sidebar"),
            Item::Navigation => crate::i18n::tr("toolbar.item_navigation"),
            Item::Title => crate::i18n::tr("toolbar.item_title"),
            Item::Space => crate::i18n::tr("toolbar.item_space"),
            Item::Views => crate::i18n::tr("toolbar.item_views"),
            Item::Group => crate::i18n::tr("toolbar.item_group"),
            Item::Share => crate::i18n::tr("toolbar.item_share"),
            Item::Tags => crate::i18n::tr("toolbar.item_tags"),
            Item::Actions => crate::i18n::tr("toolbar.item_actions"),
            Item::Search => crate::i18n::tr("toolbar.item_search"),
        }
    }

    /// Kept on the toolbar longest when the window is narrow (the others fold into »).
    fn pinned(self) -> bool {
        matches!(self, Item::Sidebar | Item::Navigation | Item::Title)
    }

    fn icon(self) -> Icon {
        match self {
            Item::Sidebar => Icon::Sidebar,
            Item::Navigation => Icon::Back,
            Item::Title => Icon::Folder,
            Item::Space => Icon::Space,
            Item::Views => Icon::Grid,
            Item::Group => Icon::Group,
            Item::Share => Icon::Share,
            Item::Tags => Icon::Tag,
            Item::Actions => Icon::More,
            Item::Search => Icon::Search,
        }
    }
}

/// The toolbar of a profile (config.toml `[toolbar]`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Left to right. Unknown names (from a later Keel) and repeats are dropped on load; an
    /// empty toolbar is allowed.
    #[serde(serialize_with = "save_items", deserialize_with = "load_items")]
    pub items: Vec<Item>,
    /// Icon and Text (else Icon Only).
    pub labels: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            items: Item::ALL.to_vec(),
            labels: false,
        }
    }
}

fn save_items<S: serde::Serializer>(items: &[Item], s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(items.iter().map(|i| i.key()))
}

fn load_items<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Item>, D::Error> {
    let keys: Vec<String> = serde::Deserialize::deserialize(d)?;
    let mut items = Vec::new();
    for item in keys.iter().filter_map(|k| Item::from_key(k)) {
        if !items.contains(&item) {
            items.push(item);
        }
    }
    Ok(items)
}

impl Config {
    /// Moves (or adds) `item` so it lands before the item now at `at` (`items.len()`: last).
    pub fn place(&mut self, item: Item, at: usize) {
        let at = at.min(self.items.len());
        let at = match self.items.iter().position(|i| *i == item) {
            Some(was) if was < at => at - 1,
            _ => at,
        };
        self.items.retain(|i| *i != item);
        self.items.insert(at.min(self.items.len()), item);
    }

    pub fn remove(&mut self, item: Item) {
        self.items.retain(|i| *i != item);
    }
}

/// Space between toolbar items.
const GAP: f32 = 6.0;
/// The » button that holds the items a narrow window has no room for.
const OVERFLOW: f32 = 28.0;
/// A toolbar control's height (both label modes).
const CONTROL: f32 = 28.0;
/// The narrowest the folder title gets before items fold away.
const MIN_TITLE: f32 = 48.0;

/// The toolbar laid out in a given width.
#[derive(Debug, PartialEq)]
pub struct Arranged {
    /// (item, x from the toolbar's left, width), left to right.
    pub placed: Vec<(Item, f32, f32)>,
    /// Items with no room, in toolbar order: in the » menu.
    pub hidden: Vec<Item>,
}

/// Lays out `items` (each with the width it wants) in `width` points. Items fold into the
/// » menu from the right, the sidebar button, back/forward and the folder title last; the
/// title then shrinks; flexible spaces share what is left over.
pub fn arrange(items: &[(Item, f32)], width: f32) -> Arranged {
    let total = |v: &[(Item, f32)], folded: bool| {
        let gaps = GAP * v.len().saturating_sub(1) as f32;
        let overflow = if folded { OVERFLOW + GAP } else { 0.0 };
        v.iter().map(|(_, w)| w).sum::<f32>() + gaps + overflow
    };
    let mut visible = items.to_vec();
    let mut hidden = Vec::new();
    while total(&visible, !hidden.is_empty()) > width {
        let Some(at) = visible
            .iter()
            .rposition(|(i, _)| !i.pinned() && *i != Item::Space)
        else {
            break;
        };
        hidden.insert(0, visible.remove(at).0);
    }
    // Spaces next to nothing, or in a folded toolbar, only cost gaps.
    if !hidden.is_empty() {
        visible.retain(|(i, _)| *i != Item::Space);
    }
    let over = total(&visible, !hidden.is_empty()) - width;
    if over > 0.0 {
        if let Some((_, w)) = visible.iter_mut().find(|(i, _)| *i == Item::Title) {
            *w = (*w - over).max(MIN_TITLE);
        }
    }
    let spaces = visible.iter().filter(|(i, _)| *i == Item::Space).count();
    let extra = (width - total(&visible, !hidden.is_empty())).max(0.0);
    let mut x = 0.0;
    let mut placed = Vec::new();
    for (item, w) in visible {
        let w = if item == Item::Space {
            w + extra / spaces as f32
        } else {
            w
        };
        placed.push((item, x, w));
        x += w + GAP;
    }
    Arranged { placed, hidden }
}

// ---------------------------------------------------------------------------------------
// Drawing

/// Original line icons on a 16-point grid with one stroke weight (no Apple artwork).
#[derive(Clone, Copy)]
enum Icon {
    Sidebar,
    Back,
    Forward,
    Folder,
    Space,
    Grid,
    List,
    Columns,
    Group,
    Share,
    Tag,
    More,
    Search,
    Chevron,
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn icon(painter: &egui::Painter, center: egui::Pos2, kind: Icon, color: Color32) {
    let s = Stroke::new(1.3_f32, color);
    let at = |x: f32, y: f32| center + vec2(x - 8.0, y - 8.0);
    let line = |a: (f32, f32), b: (f32, f32)| {
        painter.line_segment([at(a.0, a.1), at(b.0, b.1)], s);
    };
    let frame = |x: f32, y: f32, w: f32, h: f32, r: f32| {
        let rect = Rect::from_min_size(at(x, y), vec2(w, h));
        painter.rect_stroke(rect, r, s, egui::StrokeKind::Middle);
    };
    match kind {
        Icon::Sidebar => {
            frame(1.5, 2.5, 13.0, 11.0, 2.0);
            line((6.0, 2.5), (6.0, 13.5));
        }
        Icon::Back => {
            line((10.0, 3.0), (5.0, 8.0));
            line((5.0, 8.0), (10.0, 13.0));
        }
        Icon::Forward => {
            line((6.0, 3.0), (11.0, 8.0));
            line((11.0, 8.0), (6.0, 13.0));
        }
        Icon::Folder => {
            frame(1.5, 4.5, 13.0, 9.0, 1.5);
            line((1.5, 4.5), (3.0, 2.5));
            line((3.0, 2.5), (6.5, 2.5));
            line((6.5, 2.5), (7.5, 4.5));
        }
        Icon::Space => {
            line((1.0, 8.0), (15.0, 8.0));
            line((1.0, 5.0), (1.0, 11.0));
            line((15.0, 5.0), (15.0, 11.0));
        }
        Icon::Grid => {
            for x in [2.0, 9.0] {
                for y in [2.0, 9.0] {
                    frame(x, y, 5.0, 5.0, 1.0);
                }
            }
        }
        Icon::List => {
            for y in [3.5, 8.0, 12.5] {
                line((2.0, y), (3.5, y));
                line((6.0, y), (14.0, y));
            }
        }
        Icon::Columns => {
            frame(1.5, 2.5, 13.0, 11.0, 1.5);
            line((6.0, 2.5), (6.0, 13.5));
            line((10.5, 2.5), (10.5, 13.5));
        }
        Icon::Group => {
            frame(1.5, 2.0, 13.0, 5.0, 1.5);
            frame(1.5, 9.0, 13.0, 5.0, 1.5);
        }
        Icon::Share => {
            line((4.5, 6.5), (2.5, 6.5));
            line((2.5, 6.5), (2.5, 14.5));
            line((2.5, 14.5), (13.5, 14.5));
            line((13.5, 14.5), (13.5, 6.5));
            line((13.5, 6.5), (11.5, 6.5));
            line((8.0, 10.0), (8.0, 1.5));
            line((5.0, 4.5), (8.0, 1.5));
            line((8.0, 1.5), (11.0, 4.5));
        }
        Icon::Tag => {
            for (a, b) in [
                ((1.5, 2.0), (7.5, 2.0)),
                ((7.5, 2.0), (14.5, 9.0)),
                ((14.5, 9.0), (9.0, 14.5)),
                ((9.0, 14.5), (1.5, 7.0)),
                ((1.5, 7.0), (1.5, 2.0)),
            ] {
                line(a, b);
            }
            painter.circle_stroke(at(4.8, 5.3), 1.1, s);
        }
        Icon::More => {
            painter.circle_stroke(center, 6.5, s);
            for x in [5.0, 8.0, 11.0] {
                painter.circle_filled(at(x, 8.0), 0.9, color);
            }
        }
        Icon::Search => {
            painter.circle_stroke(at(6.5, 6.5), 4.5, s);
            line((9.8, 9.8), (14.0, 14.0));
        }
        Icon::Chevron => {
            line((5.0, 6.5), (8.0, 9.5));
            line((8.0, 9.5), (11.0, 6.5));
        }
        // The Windows caption glyphs: 10 points, thin.
        Icon::Minimize => line((3.0, 8.0), (13.0, 8.0)),
        Icon::Maximize => frame(3.0, 3.0, 10.0, 10.0, 1.0),
        Icon::Restore => {
            frame(3.0, 5.0, 8.0, 8.0, 1.0);
            line((5.0, 5.0), (5.0, 3.0));
            line((5.0, 3.0), (13.0, 3.0));
            line((13.0, 3.0), (13.0, 11.0));
            line((13.0, 11.0), (11.0, 11.0));
        }
        Icon::Close => {
            line((3.0, 3.0), (13.0, 13.0));
            line((13.0, 3.0), (3.0, 13.0));
        }
    }
}

/// An icon button (`selected`: the current view, a segment that is on).
fn button(ui: &mut Ui, name: &str, kind: Icon, enabled: bool, selected: bool) -> Response {
    let r = ui.add_enabled(enabled, egui::Button::new("").min_size(vec2(28.0, CONTROL)));
    r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, selected, name));
    let r = r.tip(name);
    if selected {
        let fill = ui.visuals().text_color().gamma_multiply(0.14);
        let radius = ui.visuals().widgets.inactive.corner_radius;
        ui.painter().rect_filled(r.rect, radius, fill);
    }
    let color = ui.style().interact(&r).fg_stroke.color;
    icon(ui.painter(), r.rect.center(), kind, color);
    r
}

/// An icon with a small chevron that opens a menu. `open` is set while it is open.
fn menu(
    ui: &mut Ui,
    name: &str,
    kind: Icon,
    open: &mut bool,
    add: impl FnOnce(&mut Ui),
) -> Response {
    let r = ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = CONTROL;
        ui.spacing_mut().button_padding.x = 18.0;
        ui.menu_button(" ", |ui| {
            ui.set_min_width(180.0);
            add(ui)
        })
    });
    let r = r.inner;
    *open |= r.inner.is_some();
    let response = r.response.tip(name);
    let color = ui.style().interact(&response).fg_stroke.color;
    let c = response.rect.center();
    icon(ui.painter(), c - vec2(5.0, 0.0), kind, color);
    icon(
        ui.painter(),
        c + vec2(12.0, 1.0),
        Icon::Chevron,
        color.gamma_multiply(0.7),
    );
    response
}

fn action(ui: &mut Ui, label: &str, a: Action, out: &mut Vec<Action>) {
    if ui.button(label).clicked() {
        out.push(a);
        ui.close_menu();
    }
}

/// The width an item asks for (the folder title: its name, up to a limit).
fn width(ui: &Ui, item: Item, s: &AppState, labels: bool, search_open: bool) -> f32 {
    let w: f32 = match item {
        Item::Space => 0.0,
        Item::Navigation => 2.0 * 28.0 + 2.0,
        Item::Title => {
            let title = s.panes[s.active].tab().title();
            let font = egui::TextStyle::Body.resolve(ui.style());
            let text = ui.fonts(|f| f.layout_no_wrap(title, font, Color32::WHITE).size().x);
            (text + 34.0).clamp(MIN_TITLE, 260.0)
        }
        Item::Views => 3.0 * 30.0 + 26.0,
        Item::Search if search_open => 200.0,
        Item::Group | Item::Share | Item::Tags | Item::Actions => 44.0,
        Item::Sidebar | Item::Search => 30.0,
    };
    if labels && item != Item::Space {
        let font = FontId::proportional(design::tokens(ui.ctx()).text.small);
        let text = ui.fonts(|f| {
            f.layout_no_wrap(item.label().into(), font, Color32::WHITE)
                .size()
                .x
        });
        w.max(text + 8.0)
    } else {
        w
    }
}

/// The views: Icons, List and Columns as a segmented control; Media and Sizes behind its
/// chevron (Gallery comes with the Finder views, Task 66).
const SEGMENTS: [(ViewMode, &str, Icon); 3] = [
    (ViewMode::Grid, "as Icons", Icon::Grid),
    (ViewMode::Details, "as List", Icon::List),
    (ViewMode::Columns, "as Columns", Icon::Columns),
];

/// The contents of `item`'s menu: in its own button, and in » when it has no room.
fn item_menu(ui: &mut Ui, item: Item, s: &mut AppState, out: &mut Vec<Action>) {
    let p = s.active;
    match item {
        Item::Sidebar => action(
            ui,
            crate::i18n::tr("toolbar.show_hide_sidebar"),
            Action::ToggleSidebar,
            out,
        ),
        Item::Navigation => {
            let tab = &s.panes[p].tabs[s.panes[p].active];
            let back = !tab.history.is_empty() || !tab.columns.cols.is_empty();
            let forward = !tab.future.is_empty();
            ui.add_enabled_ui(back, |ui| {
                action(ui, crate::i18n::tr("toolbar.back"), Action::Back, out)
            });
            ui.add_enabled_ui(forward, |ui| {
                action(ui, crate::i18n::tr("toolbar.forward"), Action::Forward, out)
            });
        }
        Item::Title => {
            let tab = s.panes[p].tab();
            if !tab.is_search() {
                let mut dir = tab.dir.parent();
                while let Some(d) = dir {
                    let name = match d.name() {
                        "" => d.display(),
                        name => name.to_owned(),
                    };
                    if ui.button(name).on_hover_text(d.display()).clicked() {
                        out.push(Action::Navigate(d.clone()));
                        ui.close_menu();
                    }
                    dir = d.parent();
                }
                ui.separator();
            }
            action(
                ui,
                crate::i18n::tr("toolbar.go_to_folder"),
                Action::FocusPath,
                out,
            );
        }
        Item::Views => {
            let view = s.panes[p].view;
            for (mode, label) in [
                (ViewMode::Grid, "as Icons"),
                (ViewMode::Details, "as List"),
                (ViewMode::Columns, "as Columns"),
                (ViewMode::Media, "as Media"),
                (ViewMode::Sizes, "as Sizes"),
                (
                    ViewMode::DiskUsage,
                    crate::i18n::tr("toolbar.as_disk_usage"),
                ),
            ] {
                if ui.selectable_label(view == mode, label).clicked() {
                    out.push(Action::View(mode));
                    ui.close_menu();
                }
            }
        }
        Item::Group => {
            ui.label(egui::RichText::new(crate::i18n::tr("toolbar.sort_by")).weak());
            let tab = s.panes[p].tab_mut();
            for (key, label) in [
                (SortKey::Name, crate::i18n::tr("toolbar.name")),
                (SortKey::Ext, crate::i18n::tr("toolbar.kind")),
                (SortKey::Modified, crate::i18n::tr("toolbar.date_modified")),
                (SortKey::Size, crate::i18n::tr("toolbar.size")),
            ] {
                if ui.selectable_label(tab.sort.0 == key, label).clicked() {
                    // Finder's defaults: names A to Z, the newest and the biggest first.
                    let ascending = matches!(key, SortKey::Name | SortKey::Ext);
                    tab.sort = (key, ascending);
                    ui.close_menu();
                }
            }
            if ui
                .selectable_label(!tab.sort.1, crate::i18n::tr("toolbar.descending"))
                .clicked()
            {
                tab.sort.1 = !tab.sort.1;
                ui.close_menu();
            }
            if s.panes[p].view == ViewMode::Media {
                ui.separator();
                ui.label(egui::RichText::new(crate::i18n::tr("toolbar.group_by")).weak());
                if ui
                    .selectable_label(!s.settings.media_dates, crate::i18n::tr("toolbar.none"))
                    .clicked()
                {
                    s.settings.media_dates = false;
                    ui.close_menu();
                }
                if ui
                    .selectable_label(
                        s.settings.media_dates,
                        crate::i18n::tr("toolbar.date_taken"),
                    )
                    .clicked()
                {
                    s.settings.media_dates = true;
                    ui.close_menu();
                }
            }
        }
        Item::Share => {
            let targets = s.panes[p].tab().targets();
            let some = !targets.is_empty();
            let link = targets
                .first()
                .filter(|e| crate::clouds::can_link(ui.ctx(), e))
                .map(|e| e.path.clone());
            ui.add_enabled_ui(some, |ui| {
                let send = Action::Devices(crate::devices::DevCmd::SendSelection);
                action(ui, crate::i18n::tr("toolbar.send_spacedrop"), send, out);
            });
            match link {
                Some(path) => action(
                    ui,
                    crate::i18n::tr("toolbar.copy_link"),
                    Action::CopyLink(path),
                    out,
                ),
                None => {
                    ui.add_enabled(
                        false,
                        egui::Button::new(crate::i18n::tr("toolbar.copy_link")),
                    )
                    .on_disabled_hover_text(crate::i18n::tr("toolbar.copy_link_disabled"));
                }
            }
            ui.add_enabled_ui(some, |ui| {
                action(
                    ui,
                    crate::i18n::tr("toolbar.copy_path"),
                    Action::CopyPath,
                    out,
                )
            });
        }
        Item::Tags => {
            let targets: Vec<_> = s.panes[p]
                .tab()
                .targets()
                .iter()
                .map(|e| e.path.clone())
                .collect();
            let lib = &s.library;
            ui.add_enabled_ui(!targets.is_empty(), |ui| {
                for tag in &lib.tags {
                    let on = targets
                        .iter()
                        .all(|t| lib.tagged.get(t).is_some_and(|tags| tags.contains(&tag.id)));
                    let color = tag
                        .color
                        .as_deref()
                        .and_then(crate::color_theme::parse_color)
                        .map(|[r, g, b, _]| Color32::from_rgb(r, g, b));
                    let r = ui.selectable_label(on, format!("     {}", tag.name));
                    if let Some(color) = color {
                        let dot = pos2(r.rect.left() + 12.0, r.rect.center().y);
                        ui.painter().circle_filled(dot, 5.0, color);
                    }
                    if r.clicked() {
                        let cmd = crate::library::LibCmd::SetTag {
                            tag: tag.id,
                            on: !on,
                        };
                        out.push(Action::Library(cmd));
                        ui.close_menu();
                    }
                }
                if !lib.tags.is_empty() {
                    ui.separator();
                }
                let picker = Action::Library(crate::library::LibCmd::TagPicker);
                action(ui, crate::i18n::tr("toolbar.tags_menu"), picker, out);
            });
        }
        Item::Actions => {
            crate::pane::context_menu(ui, s.panes[p].tab(), None, out);
            ui.separator();
            action(
                ui,
                crate::i18n::tr("toolbar.customize"),
                Action::CustomizeToolbar,
                out,
            );
        }
        Item::Search => action(
            ui,
            crate::i18n::tr("toolbar.search_menu"),
            Action::Search,
            out,
        ),
        Item::Space => {}
    }
}

fn search_id() -> Id {
    Id::new("keel-toolbar-search")
}

/// Draws `item` into `ui` (a child the width `arrange` gave it).
fn item_ui(ui: &mut Ui, item: Item, s: &mut AppState, out: &mut Vec<Action>, open: &mut bool) {
    let p = s.active;
    match item {
        Item::Sidebar => {
            let name = if s.settings.show_sidebar {
                crate::i18n::tr("toolbar.hide_sidebar")
            } else {
                crate::i18n::tr("toolbar.show_sidebar")
            };
            if button(ui, name, Icon::Sidebar, true, false).clicked() {
                out.push(Action::ToggleSidebar);
            }
        }
        Item::Navigation => {
            ui.spacing_mut().item_spacing.x = 2.0;
            let tab = &s.panes[p].tabs[s.panes[p].active];
            let back = !tab.history.is_empty() || !tab.columns.cols.is_empty();
            let forward = !tab.future.is_empty();
            if button(ui, crate::i18n::tr("toolbar.back"), Icon::Back, back, false).clicked() {
                out.push(Action::Back);
            }
            if button(
                ui,
                crate::i18n::tr("toolbar.forward"),
                Icon::Forward,
                forward,
                false,
            )
            .clicked()
            {
                out.push(Action::Forward);
            }
        }
        Item::Title => {
            let title = s.panes[p].tab().title();
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            ui.spacing_mut().interact_size.y = CONTROL;
            let text = egui::RichText::new(title).strong();
            let r = ui.menu_button(text, |ui| item_menu(ui, Item::Title, s, out));
            *open |= r.inner.is_some();
            r.response.tip(crate::i18n::tr("toolbar.folder_tip"));
        }
        Item::Views => {
            ui.spacing_mut().item_spacing.x = 0.0;
            let view = s.panes[p].view;
            for (mode, name, glyph) in SEGMENTS {
                let r = button(
                    ui,
                    &crate::i18n::tr("toolbar.view_mode").replace("{name}", name),
                    glyph,
                    true,
                    view == mode,
                );
                if r.clicked() {
                    out.push(Action::View(mode));
                }
            }
            let more = !SEGMENTS.iter().any(|(m, _, _)| *m == view);
            ui.scope(|ui| {
                ui.spacing_mut().interact_size = vec2(24.0, CONTROL);
                ui.spacing_mut().button_padding.x = 4.0;
                let r = ui.menu_button(" ", |ui| item_menu(ui, Item::Views, s, out));
                *open |= r.inner.is_some();
                let r = r.response.tip(crate::i18n::tr("toolbar.more_views_tip"));
                if more {
                    let fill = ui.visuals().text_color().gamma_multiply(0.14);
                    let radius = ui.visuals().widgets.inactive.corner_radius;
                    ui.painter().rect_filled(r.rect, radius, fill);
                }
                let color = ui.style().interact(&r).fg_stroke.color;
                icon(ui.painter(), r.rect.center(), Icon::Chevron, color);
            });
        }
        Item::Search => {
            let id = search_id();
            let field = id.with("field");
            let opening = id.with("opening");
            let expanded = ui.ctx().data(|d| d.get_temp::<bool>(id).unwrap_or(false));
            if !expanded {
                if button(
                    ui,
                    crate::i18n::tr("toolbar.search"),
                    Icon::Search,
                    true,
                    false,
                )
                .clicked()
                {
                    ui.ctx().data_mut(|d| {
                        d.insert_temp(id, true);
                        d.insert_temp(opening, true);
                    });
                }
                return;
            }
            let mut text = ui
                .ctx()
                .data(|d| d.get_temp::<String>(field).unwrap_or_default());
            let (glyph, _) = ui.allocate_exact_size(vec2(20.0, CONTROL), Sense::hover());
            icon(
                ui.painter(),
                glyph.center(),
                Icon::Search,
                ui.visuals().weak_text_color(),
            );
            let r = ui
                .add(
                    egui::TextEdit::singleline(&mut text)
                        .id(field)
                        .hint_text(crate::i18n::tr("toolbar.search_hint"))
                        .desired_width(ui.available_width()),
                )
                .named(crate::i18n::tr("toolbar.search"));
            // Focused on the frame it first shows (a focus request for a widget not
            // drawn yet would be dropped).
            let just_opened = ui
                .ctx()
                .data_mut(|d| d.remove_temp::<bool>(opening).unwrap_or(false));
            if just_opened {
                r.request_focus();
            }
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if r.lost_focus() && enter && !text.trim().is_empty() {
                out.push(Action::ToolbarSearch(text.trim().to_owned()));
            }
            // Enter, Esc or a click elsewhere: back to the magnifier.
            if !just_opened && !r.has_focus() {
                text.clear();
                ui.ctx().data_mut(|d| d.insert_temp(id, false));
            }
            ui.ctx().data_mut(|d| d.insert_temp(field, text));
        }
        Item::Space => {}
        Item::Group | Item::Share | Item::Tags | Item::Actions => {
            let name = match item {
                Item::Group => crate::i18n::tr("toolbar.group_and_sort"),
                Item::Share => crate::i18n::tr("toolbar.item_share"),
                Item::Tags => crate::i18n::tr("toolbar.item_tags"),
                _ => crate::i18n::tr("toolbar.item_actions"),
            };
            menu(ui, name, item.icon(), open, |ui| {
                item_menu(ui, item, s, out)
            });
        }
    }
}

/// The toolbar items currently configured (`pane::nav_bar` leaves out what it shows).
fn shown_id() -> Id {
    Id::new("keel-toolbar-shown")
}

/// The Finder toolbar shows `item` this frame (in place or in »).
pub fn shows(ctx: &egui::Context, item: Item) -> bool {
    ctx.data(|d| d.get_temp::<Vec<Item>>(shown_id()))
        .is_some_and(|items| items.contains(&item))
}

/// The toolbar at the top of the window (Finder style), and the title bar it stands in for.
pub fn panel(
    ctx: &egui::Context,
    s: &mut AppState,
    chrome: &mut Chrome,
    out: &mut Vec<(usize, Action)>,
) {
    if s.settings.style != Style::Finder {
        ctx.data_mut(|d| d.remove::<Vec<Item>>(shown_id()));
        if chrome.fullsize() {
            mac_title_bar(ctx);
        }
        chrome.publish(ctx, chrome::Layout::default());
        return;
    }
    let config = s.settings.toolbar.clone();
    ctx.data_mut(|d| d.insert_temp(shown_id(), config.items.clone()));
    let labels = config.labels;
    let height = if labels {
        design::finder::TOOLBAR_LABELS
    } else {
        design::finder::TOOLBAR
    };
    let (maximized, fullscreen) = ctx.input(|i| {
        let v = i.viewport();
        (v.maximized.unwrap_or(false), v.fullscreen.unwrap_or(false))
    });
    let mut layout = chrome::Layout {
        window: ctx.screen_rect(),
        maximized: maximized || fullscreen,
        ..Default::default()
    };
    let mut actions = Vec::new();
    let mut open = false;
    let frame = egui::Frame::new().fill(ctx.style().visuals.panel_fill);
    egui::TopBottomPanel::top("finder-toolbar")
        .exact_height(height)
        .frame(frame)
        .show(ctx, |ui| {
            let full = ui.max_rect();
            layout.title = full;
            // The empty toolbar: drags the window where the system does not (macOS), and
            // its menu where right-click is not the system menu (all but Windows).
            let bg = ui.interact(full, Id::new("toolbar-background"), crate::a11y::NO_TAB);
            if chrome.fullsize() {
                if bg.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if bg.double_clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
            }
            let menu = bg.context_menu(|ui| background_menu(ui, s, &mut actions));
            open |= menu.is_some();

            let (inset_left, inset_right) = chrome.insets();
            let area = Rect::from_x_y_ranges(
                full.left() + inset_left + 8.0..=full.right() - inset_right - 8.0,
                full.y_range(),
            );
            let top = if labels {
                full.top() + 5.0
            } else {
                full.center().y - CONTROL / 2.0
            };
            let search_open = ctx.data(|d| d.get_temp::<bool>(search_id()).unwrap_or(false));
            let wants: Vec<(Item, f32)> = config
                .items
                .iter()
                .map(|&i| (i, width(ui, i, s, labels, search_open)))
                .collect();
            let arranged = arrange(&wants, area.width().max(0.0));
            let small = FontId::proportional(design::tokens(ctx).text.small);
            // Toolbar buttons have no frame until hovered, as in Finder.
            let text = ui.visuals().text_color();
            let widgets = &mut ui.visuals_mut().widgets;
            for (w, fill) in [
                (&mut widgets.inactive, Color32::TRANSPARENT),
                (&mut widgets.hovered, text.gamma_multiply(0.08)),
                (&mut widgets.active, text.gamma_multiply(0.16)),
            ] {
                w.weak_bg_fill = fill;
                w.bg_stroke = Stroke::NONE;
            }
            for &(item, x, w) in &arranged.placed {
                if item == Item::Space {
                    continue;
                }
                let rect = Rect::from_min_size(pos2(area.left() + x, top), vec2(w, CONTROL));
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt(("toolbar", item.key()))
                        .max_rect(rect)
                        .layout(egui::Layout::left_to_right(Align::Center)),
                );
                child.set_clip_rect(rect.intersect(full));
                item_ui(&mut child, item, s, &mut actions, &mut open);
                layout.controls.push(child.min_rect());
                if labels {
                    ui.painter().text(
                        pos2(rect.center().x, rect.bottom() + 2.0),
                        Align2::CENTER_TOP,
                        item.label(),
                        small.clone(),
                        ui.visuals().weak_text_color(),
                    );
                }
            }
            if !arranged.hidden.is_empty() {
                let rect = Rect::from_min_size(
                    pos2(area.right() - OVERFLOW, top),
                    vec2(OVERFLOW, CONTROL),
                );
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt("toolbar-overflow")
                        .max_rect(rect)
                        .layout(egui::Layout::left_to_right(Align::Center)),
                );
                let r = child.menu_button("»", |ui| {
                    for &item in &arranged.hidden {
                        ui.menu_button(item.label(), |ui| item_menu(ui, item, s, &mut actions));
                    }
                });
                open |= r.inner.is_some();
                r.response.tip(crate::i18n::tr("toolbar.more_items_tip"));
                layout.controls.push(child.min_rect());
            }
            if chrome.caption_buttons() {
                caption_buttons(ui, full, chrome, maximized, &mut layout);
            }
        });
    let modal = ctx.memory(|m| !m.is_above_modal_layer(egui::LayerId::background()));
    layout.blocked = open || modal || ctx.is_context_menu_open();
    // Windows and popups drawn over the toolbar are Keel's, not the caption.
    ctx.memory(|m| {
        for layer in m.areas().visible_layer_ids() {
            if matches!(layer.order, egui::Order::Middle | egui::Order::Foreground) {
                if let Some(r) = m.area_rect(layer.id).filter(|r| r.intersects(layout.title)) {
                    layout.controls.push(r);
                }
            }
        }
    });
    chrome.publish(ctx, layout);
    out.extend(actions.into_iter().map(|a| (s.active, a)));
}

/// Right-click on the empty toolbar (Finder's menu).
fn background_menu(ui: &mut Ui, s: &mut AppState, out: &mut Vec<Action>) {
    let labels = &mut s.settings.toolbar.labels;
    if ui
        .radio_value(labels, true, crate::i18n::tr("toolbar.icon_and_text"))
        .clicked()
        || ui
            .radio_value(labels, false, crate::i18n::tr("toolbar.icon_only"))
            .clicked()
    {
        ui.close_menu();
    }
    ui.separator();
    action(
        ui,
        crate::i18n::tr("toolbar.customize"),
        Action::CustomizeToolbar,
        out,
    );
}

/// macOS, opened in the Finder style and switched to Classic: a bar to drag the window by,
/// so nothing sits under the traffic lights (the system title bar is back at the next start).
fn mac_title_bar(ctx: &egui::Context) {
    egui::TopBottomPanel::top("mac-title")
        .exact_height(chrome::MAC_TITLE)
        .show(ctx, |ui| {
            let r = ui.interact(ui.max_rect(), Id::new("mac-title"), crate::a11y::NO_TAB);
            if r.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if r.double_clicked() {
                let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!max));
            }
        });
}

/// Minimize, maximize (restore) and close, Windows style, at the toolbar's right end.
fn caption_buttons(
    ui: &mut Ui,
    full: Rect,
    chrome: &Chrome,
    maximized: bool,
    layout: &mut chrome::Layout,
) {
    let w = chrome::CAPTION_BUTTON;
    let text = ui.visuals().text_color();
    let buttons = [
        (
            crate::i18n::tr("toolbar.minimize"),
            Icon::Minimize,
            egui::ViewportCommand::Minimized(true),
        ),
        if maximized {
            (
                crate::i18n::tr("toolbar.restore"),
                Icon::Restore,
                egui::ViewportCommand::Maximized(false),
            )
        } else {
            (
                crate::i18n::tr("toolbar.maximize"),
                Icon::Maximize,
                egui::ViewportCommand::Maximized(true),
            )
        },
        (
            crate::i18n::tr("toolbar.close"),
            Icon::Close,
            egui::ViewportCommand::Close,
        ),
    ];
    for (i, (name, glyph, cmd)) in buttons.into_iter().enumerate() {
        let x = full.right() - (3 - i) as f32 * w;
        let rect = Rect::from_min_max(pos2(x, full.top()), pos2(x + w, full.bottom()));
        let r = ui
            .interact(rect, Id::new(("caption-button", i)), Sense::click())
            .tip(name);
        let close = i == 2;
        // The maximize button is the system's (snap layouts): its hover comes from there.
        let hovered = r.hovered() || r.has_focus() || (i == 1 && chrome.maximize_hovered());
        let pressed = r.is_pointer_button_down_on();
        if hovered || pressed {
            let fill = match (close, pressed) {
                (true, false) => Color32::from_rgb(0xc4, 0x2b, 0x1c),
                (true, true) => Color32::from_rgb(0xc8, 0x3c, 0x30),
                (false, false) => text.gamma_multiply(0.09),
                (false, true) => text.gamma_multiply(0.05),
            };
            ui.painter().rect_filled(rect, 0.0, fill);
        }
        let color = if close && hovered {
            Color32::WHITE
        } else {
            text
        };
        icon(ui.painter(), rect.center(), glyph, color);
        if r.clicked() {
            ui.ctx().send_viewport_cmd(cmd);
        }
        if i == 1 {
            layout.maximize = Some(rect);
        } else {
            layout.controls.push(rect);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Customize Toolbar

fn customize_id() -> Id {
    Id::new("keel-customize-toolbar")
}

pub fn open_customize(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(customize_id(), true));
}

/// The Customize Toolbar dialog is open.
#[cfg(test)]
pub fn customizing(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(customize_id()).unwrap_or(false))
}

#[cfg(test)]
pub fn close_customize(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(customize_id(), false));
}

/// The Customize Toolbar dialog, while open: drag items to reorder them, into the toolbar
/// list to add them and out of it to remove them (or use each row's buttons), Icon Only or
/// Icon and Text, Restore Default. Changes show at once and are saved with the profile.
pub fn customize(ctx: &egui::Context, config: &mut Config) {
    let id = customize_id();
    if !ctx.data(|d| d.get_temp::<bool>(id).unwrap_or(false)) {
        return;
    }
    let mut done = false;
    let modal = egui::Modal::new(id).show(ctx, |ui| {
        ui.set_width(440.0_f32.min(ctx.screen_rect().width() - 48.0));
        ui.heading(crate::i18n::tr("toolbar.customize_title"));
        ui.weak(crate::i18n::tr("toolbar.customize_hint"));
        ui.add_space(design::space::M);
        let mut place: Option<(Item, usize)> = None;
        let mut remove: Option<Item> = None;
        let max = (ctx.screen_rect().height() - 260.0).max(120.0);
        egui::ScrollArea::vertical().max_height(max).show(ui, |ui| {
            ui.strong(crate::i18n::tr("toolbar.in_toolbar"));
            let items = config.items.clone();
            for (i, &item) in items.iter().enumerate() {
                let row = ui.horizontal(|ui| {
                    ui.dnd_drag_source(id.with(("in", item.key())), item, |ui| {
                        chip(ui, item);
                    });
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let label = item.label();
                        if ui
                            .small_button(crate::i18n::tr("toolbar.remove"))
                            .named(
                                &crate::i18n::tr("toolbar.remove_item").replace("{label}", label),
                            )
                            .clicked()
                        {
                            remove = Some(item);
                        }
                        let later = ui.add_enabled(i + 1 < items.len(), egui::Button::new("⏷"));
                        if later
                            .tip(&crate::i18n::tr("toolbar.move_right").replace("{label}", label))
                            .clicked()
                        {
                            place = Some((item, i + 2));
                        }
                        let earlier = ui.add_enabled(i > 0, egui::Button::new("⏶"));
                        if earlier
                            .tip(&crate::i18n::tr("toolbar.move_left").replace("{label}", label))
                            .clicked()
                        {
                            place = Some((item, i - 1));
                        }
                    });
                });
                if let Some(dropped) = row.response.dnd_release_payload::<Item>() {
                    place = Some((*dropped, i));
                }
            }
            let end = ui.add(egui::Label::new(
                egui::RichText::new(crate::i18n::tr("toolbar.drop_to_add_end")).weak(),
            ));
            if let Some(dropped) = end.dnd_release_payload::<Item>() {
                place = Some((*dropped, items.len()));
            }
            ui.add_space(design::space::M);
            let available = ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.strong(crate::i18n::tr("toolbar.available"));
                let missing: Vec<Item> = Item::ALL
                    .into_iter()
                    .filter(|i| !items.contains(i))
                    .collect();
                if missing.is_empty() {
                    ui.weak(crate::i18n::tr("toolbar.all_in_toolbar"));
                }
                for item in missing {
                    ui.horizontal(|ui| {
                        ui.dnd_drag_source(id.with(("out", item.key())), item, |ui| {
                            chip(ui, item);
                        });
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            let name = crate::i18n::tr("toolbar.add_item")
                                .replace("{label}", item.label());
                            if ui
                                .small_button(crate::i18n::tr("toolbar.add"))
                                .named(&name)
                                .clicked()
                            {
                                place = Some((item, items.len()));
                            }
                        });
                    });
                }
            });
            if let Some(dropped) = available.response.dnd_release_payload::<Item>() {
                remove = Some(*dropped);
            }
        });
        if let Some(item) = remove {
            config.remove(item);
        } else if let Some((item, at)) = place {
            config.place(item, at);
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(crate::i18n::tr("toolbar.show"));
            ui.radio_value(
                &mut config.labels,
                false,
                crate::i18n::tr("toolbar.icon_only"),
            );
            ui.radio_value(
                &mut config.labels,
                true,
                crate::i18n::tr("toolbar.icon_and_text"),
            );
        });
        ui.add_space(design::space::M);
        ui.horizontal(|ui| {
            if ui
                .button(crate::i18n::tr("toolbar.restore_default"))
                .clicked()
            {
                *config = Config::default();
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.button(crate::i18n::tr("toolbar.done")).clicked() {
                    done = true;
                }
            });
        });
    });
    if done || modal.should_close() {
        ctx.data_mut(|d| d.insert_temp(id, false));
    }
}

/// An item in Customize Toolbar: its icon and name.
fn chip(ui: &mut Ui, item: Item) {
    let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
    icon(
        ui.painter(),
        rect.center(),
        item.icon(),
        ui.visuals().text_color(),
    );
    ui.label(item.label());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(widths: &[(Item, f32)]) -> Vec<Item> {
        widths.iter().map(|(i, _)| *i).collect()
    }

    /// Finder's widths, roughly: 30 + 58 + 120 + 0 + 116 + 4 * 44 + 30 = 530, plus 9 gaps.
    fn finder() -> Vec<(Item, f32)> {
        Item::ALL
            .into_iter()
            .map(|i| {
                let w = match i {
                    Item::Space => 0.0,
                    Item::Navigation => 58.0,
                    Item::Title => 120.0,
                    Item::Views => 116.0,
                    Item::Sidebar | Item::Search => 30.0,
                    _ => 44.0,
                };
                (i, w)
            })
            .collect()
    }

    #[test]
    fn a_wide_toolbar_shows_everything_and_the_space_pushes_right() {
        let a = arrange(&finder(), 1000.0);
        assert!(a.hidden.is_empty());
        assert_eq!(a.placed.iter().map(|p| p.0).collect::<Vec<_>>(), Item::ALL);
        let (item, x, w) = *a.placed.last().unwrap();
        assert_eq!(item, Item::Search);
        assert!(
            (x + w - 1000.0).abs() < 0.01,
            "search ends at the right edge"
        );
        let space = a.placed.iter().find(|p| p.0 == Item::Space).unwrap();
        assert!((space.2 - (1000.0 - 530.0 - 9.0 * GAP)).abs() < 0.01);
        // Left to right without overlaps.
        for pair in a.placed.windows(2) {
            assert!(pair[0].1 + pair[0].2 + GAP <= pair[1].1 + 0.01);
        }
    }

    #[test]
    fn a_narrow_toolbar_folds_from_the_right_and_keeps_the_location() {
        let a = arrange(&finder(), 400.0);
        assert_eq!(
            a.hidden,
            [
                Item::Group,
                Item::Share,
                Item::Tags,
                Item::Actions,
                Item::Search
            ]
        );
        assert_eq!(
            a.placed.iter().map(|p| p.0).collect::<Vec<_>>(),
            [Item::Sidebar, Item::Navigation, Item::Title, Item::Views]
        );
        let (_, x, w) = *a.placed.last().unwrap();
        assert!(x + w + GAP + OVERFLOW <= 400.01, "room for »");
        // Narrower still: only the pinned items, then the title shrinks.
        let a = arrange(&finder(), 200.0);
        assert_eq!(
            items(&a.placed.iter().map(|p| (p.0, p.2)).collect::<Vec<_>>()),
            [Item::Sidebar, Item::Navigation, Item::Title]
        );
        let title = a.placed[2].2;
        assert!((MIN_TITLE..120.0).contains(&title), "title {title}");
        assert_eq!(a.hidden.len(), 6);
    }

    #[test]
    fn no_space_means_left_aligned_and_an_empty_toolbar_is_fine() {
        let mut w = finder();
        w.retain(|(i, _)| *i != Item::Space);
        let a = arrange(&w, 1000.0);
        assert_eq!(a.placed[0].1, 0.0);
        let (_, x, w) = *a.placed.last().unwrap();
        assert!(x + w < 700.0);
        assert_eq!(
            arrange(&[], 500.0),
            Arranged {
                placed: vec![],
                hidden: vec![]
            }
        );
    }

    #[test]
    fn add_remove_reorder_and_restore() {
        let mut c = Config::default();
        c.remove(Item::Share);
        assert!(!c.items.contains(&Item::Share));
        c.place(Item::Share, 0);
        assert_eq!(c.items[0], Item::Share);
        c.place(Item::Share, c.items.len());
        assert_eq!(c.items.last(), Some(&Item::Share));
        // Down one (the row's ⏷: before the item two below it), then up one.
        c.place(Item::Sidebar, 2);
        assert_eq!(&c.items[..2], [Item::Navigation, Item::Sidebar]);
        c.place(Item::Sidebar, 0);
        assert_eq!(c.items[0], Item::Sidebar);
        assert_eq!(c.items.len(), Item::ALL.len());
        assert_eq!(Config::default().items, Item::ALL);
    }

    #[test]
    fn unknown_and_repeated_items_are_dropped_on_load() {
        let c: Config =
            toml::from_str("items = ['search', 'hologram', 'search', 'title']\nlabels = true")
                .unwrap();
        assert_eq!(c.items, [Item::Search, Item::Title]);
        assert!(c.labels);
        let text = toml::to_string(&c).unwrap();
        assert!(text.contains("items = [\"search\", \"title\"]"), "{text}");
    }

    /// Each profile keeps its own toolbar; an old config.toml gets the default one.
    #[test]
    fn toolbar_settings_persist_per_profile() {
        let _lock = crate::settings::TEST_ENV.lock();
        let _env = crate::settings::EnvRestore::save(&["KEEL_CONFIG_DIR", "KEEL_DATA_DIR"]);
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("KEEL_CONFIG_DIR", tmp.path());
        std::env::set_var("KEEL_DATA_DIR", tmp.path().join("data"));
        let mut a = crate::settings::Settings {
            toolbar: Config {
                items: vec![Item::Search, Item::Title, Item::Actions],
                labels: true,
            },
            show_sidebar: false,
            ..Default::default()
        };
        let b = crate::settings::Settings::default();
        let (path_a, path_b) = (
            crate::profiles::config_path("work"),
            crate::profiles::config_path("home"),
        );
        a.save_to(&path_a).unwrap();
        b.save_to(&path_b).unwrap();
        let (loaded, notice) = crate::settings::Settings::load_from(&path_a);
        assert_eq!(notice, None);
        assert_eq!(
            (loaded.toolbar, loaded.show_sidebar),
            (a.toolbar.clone(), false)
        );
        let other = crate::settings::Settings::load_from(&path_b).0;
        assert_eq!(
            (other.toolbar, other.show_sidebar),
            (Config::default(), true)
        );
        let old: crate::settings::Settings = toml::from_str("theme = 'dark'").unwrap();
        assert_eq!(old.toolbar, Config::default());
        // An empty toolbar stays empty.
        a.toolbar.items.clear();
        a.save_to(&path_a).unwrap();
        let loaded = crate::settings::Settings::load_from(&path_a).0;
        assert!(loaded.toolbar.items.is_empty());
    }
}
