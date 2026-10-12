//! Finder tag colors (Task 67): the colored dots after file names in the Details, Grid and
//! Columns views, the context menu's row of color dots that toggles tags on the selection,
//! and on macOS the native Finder tags (`com.apple.metadata:_kMDItemUserTags`) of listed
//! local files: read on a worker after each listing, merged with the library's tags for
//! the dots, and written through when Keel sets or clears a tag on a local file.

use crate::a11y::Named;
use crate::keys::Action;
use crate::library::{real_of, LibCmd, LibraryUi};
use crate::state::Msg;
use crossbeam_channel::Sender;
use egui::{vec2, Color32, Sense, Stroke};
use keel_core::finder_tags::same_name;
use keel_core::{FinderColor, FinderTag, SourceSummary, Tag, TagId, FAVORITES};
use keel_vfs::{Entry, VPath};
use std::collections::HashMap;
use std::sync::Arc;

/// Dots drawn after a name; more colors show the last three, as Finder does.
pub const MAX_DOTS: usize = 3;
const RADIUS: f32 = 4.0;
/// Overlapping dots step this far.
const STEP: f32 = 5.0;
/// Most entries of one listing whose native tags are read (macOS).
#[cfg(target_os = "macos")]
const MAX_NATIVE_READS: usize = 20_000;
/// The native tag cache is dropped when it grows past this many paths (macOS).
pub const MAX_NATIVE: usize = 200_000;

/// Native tags by local path (macOS; empty elsewhere).
pub type Native = Arc<HashMap<VPath, Vec<FinderTag>>>;

/// One dot: its color and the tag it stands for.
#[derive(Clone, Debug, PartialEq)]
pub struct Dot {
    pub color: Color32,
    pub name: String,
}

/// A stored tag color (`#rrggbb`) as a color; None for no color or anything else.
pub fn color32(hex: &str) -> Option<Color32> {
    keel_core::valid_color(hex).then(|| {
        let b = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or_default();
        Color32::from_rgb(b(1), b(3), b(5))
    })
}

/// The dots of an item: its library tags with a color, in the order they were set
/// (Favorites never has one), then its native tags whose names the library does not have;
/// one dot per color.
pub fn dots(tags: &[Tag], on: &[TagId], native: &[FinderTag]) -> Vec<Dot> {
    let mut out: Vec<Dot> = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    let mut push = |out: &mut Vec<Dot>, color: Option<Color32>, name: &str| {
        if let Some(color) = color.filter(|c| !out.iter().any(|d| d.color == *c)) {
            out.push(Dot {
                color,
                name: name.to_owned(),
            });
        }
    };
    for id in on.iter().filter(|id| **id != FAVORITES) {
        let Some(t) = tags.iter().find(|t| t.id == *id) else {
            continue;
        };
        names.push(&t.name);
        push(&mut out, t.color.as_deref().and_then(color32), &t.name);
    }
    for t in native {
        if !names.iter().any(|n| same_name(n, &t.name)) {
            push(&mut out, t.color.hex().and_then(color32), &t.name);
        }
    }
    out
}

/// "Tags: Red, Work" (what the dots say to a screen reader and on hover).
pub fn describe(dots: &[Dot]) -> String {
    let names: Vec<&str> = dots.iter().map(|d| d.name.as_str()).collect();
    format!("Tags: {}", names.join(", "))
}

/// Room the dots take after a name (0 for none).
pub fn width(n: usize) -> f32 {
    match n.min(MAX_DOTS) {
        0 => 0.0,
        n => 2.0 * RADIUS + STEP * (n - 1) as f32 + 4.0,
    }
}

/// Paints the last `MAX_DOTS` dots from `left`, centered on `y`, each ringed in `ring` (the
/// background) so overlapping dots stay apart.
pub fn paint(painter: &egui::Painter, left: f32, y: f32, dots: &[Dot], ring: Color32) {
    let shown = &dots[dots.len().saturating_sub(MAX_DOTS)..];
    for (i, d) in shown.iter().enumerate() {
        let c = egui::pos2(left + 2.0 + RADIUS + STEP * i as f32, y);
        painter.circle(c, RADIUS, d.color, Stroke::new(1.0, ring));
    }
}

/// Allocates room for the dots in a layout (after a name) and paints them there, with
/// their tag names on hover and for screen readers.
pub fn show(ui: &mut egui::Ui, dots: &[Dot]) {
    if dots.is_empty() {
        return;
    }
    let h = ui.spacing().interact_size.y.min(16.0);
    let (rect, r) = ui.allocate_exact_size(vec2(width(dots.len()), h), Sense::hover());
    let ring = ui.visuals().extreme_bg_color;
    paint(ui.painter(), rect.left(), rect.center().y, dots, ring);
    r.tip(&describe(dots));
}

impl LibraryUi {
    /// Task 67: the tag dots of the item at `p` (real or `library://`).
    pub fn dots(&self, p: &VPath) -> Vec<Dot> {
        if self.tagged.is_empty() && self.native.is_empty() {
            return Vec::new();
        }
        let real = match self.is_open() {
            true => real_of(&self.sources, p),
            false => None,
        };
        let key = real.as_ref().unwrap_or(p);
        let on = self.tagged.get(key).map_or(&[][..], |t| t.as_slice());
        let native = self.native.get(key).map_or(&[][..], |t| t.as_slice());
        dots(&self.tags, on, native)
    }

    /// Native tags read or written on a worker (macOS).
    pub fn native_read(&mut self, read: Vec<(VPath, Vec<FinderTag>)>) {
        if self.native.len() > MAX_NATIVE {
            self.native = Native::default();
        }
        let native = Arc::make_mut(&mut self.native);
        for (path, tags) in read {
            match tags.is_empty() {
                true => native.remove(&path),
                false => native.insert(path, tags),
            };
        }
    }
}

/// The library tag a color dot toggles: one whose color is nearest that Finder color,
/// preferring the one named like the color ("Red").
pub fn tag_for(tags: &[Tag], color: FinderColor) -> Option<&Tag> {
    tags.iter()
        .filter(|t| t.id != FAVORITES && t.color.is_some())
        .filter(|t| FinderColor::nearest(t.color.as_deref()) == color)
        .min_by_key(|t| !same_name(&t.name, color.name()))
}

/// What the context menu needs of the library, published every frame.
#[derive(Clone, Default)]
struct Shared {
    open: bool,
    sources: Vec<SourceSummary>,
    tags: Vec<Tag>,
    tagged: Arc<HashMap<VPath, Vec<TagId>>>,
    native: Native,
}

fn shared_id() -> egui::Id {
    egui::Id::new("keel-tag-dots")
}

pub fn publish(ctx: &egui::Context, lib: &LibraryUi) {
    let shared = Arc::new(Shared {
        open: lib.is_open() && !lib.lost,
        sources: lib.sources.clone(),
        tags: lib.tags.clone(),
        tagged: lib.tagged.clone(),
        native: lib.native.clone(),
    });
    ctx.data_mut(|d| d.insert_temp(shared_id(), shared));
}

/// Whether the item at `path` carries the tag the `color` dot toggles: the library's tag
/// of that color, or a native tag named after the color.
fn carries(
    open: bool,
    sources: &[SourceSummary],
    tags: &[Tag],
    tagged: &HashMap<VPath, Vec<TagId>>,
    native: &HashMap<VPath, Vec<FinderTag>>,
    path: &VPath,
    color: FinderColor,
) -> bool {
    let real = match open {
        true => real_of(sources, path),
        false => None,
    };
    let key = real.as_ref().unwrap_or(path);
    let in_library = (tag_for(tags, color).or_else(|| named(tags, color)))
        .is_some_and(|t| tagged.get(key).is_some_and(|on| on.contains(&t.id)));
    in_library
        || native
            .get(key)
            .is_some_and(|n| n.iter().any(|t| same_name(&t.name, color.name())))
}

/// The library tag named after a Finder color ("Red"), whatever its color.
pub fn named(tags: &[Tag], color: FinderColor) -> Option<&Tag> {
    tags.iter().find(|t| same_name(&t.name, color.name()))
}

impl LibraryUi {
    /// Task 67: whether the item at `p` carries the tag of the `color` dot.
    pub fn carries_color(&self, p: &VPath, color: FinderColor) -> bool {
        let open = self.is_open();
        carries(open, &self.sources, &self.tags, &self.tagged, &self.native, p, color)
    }
}

/// A row of the seven Finder color dots; a ring marks the ones `on` says are set. Returns
/// the color clicked.
pub fn color_row(ui: &mut egui::Ui, on: impl Fn(FinderColor) -> bool) -> Option<FinderColor> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for color in FinderColor::colored() {
            let set = on(color);
            let (rect, r) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
            let fill = color.hex().and_then(color32).unwrap_or(Color32::GRAY);
            let radius = if r.hovered() || r.has_focus() { 7.0 } else { 6.0 };
            ui.painter().circle_filled(rect.center(), radius, fill);
            if set {
                let ring = Stroke::new(1.5, ui.visuals().text_color());
                ui.painter().circle_stroke(rect.center(), 8.0, ring);
            }
            let verb = if set { "Remove the tag" } else { "Tag" };
            if r.tip(&format!("{verb} {}", color.name())).clicked() {
                clicked = Some(color);
            }
        }
    });
    clicked
}

/// The context menu's row of color dots: a click toggles that color's tag on the
/// selection (a ring marks a color every selected item carries); then "Tags…".
pub fn menu_row(ui: &mut egui::Ui, targets: &[&Entry], out: &mut Vec<Action>) {
    let s: Arc<Shared> = ui
        .ctx()
        .data(|d| d.get_temp(shared_id()))
        .unwrap_or_default();
    let on = |color| {
        !targets.is_empty()
            && targets.iter().all(|e| {
                carries(s.open, &s.sources, &s.tags, &s.tagged, &s.native, &e.path, color)
            })
    };
    if let Some(color) = color_row(ui, on) {
        out.push(Action::Library(LibCmd::ToggleColor { color, paths: None }));
        ui.close_menu();
    }
    if ui.button("Tags…").clicked() {
        out.push(Action::Library(LibCmd::TagPicker));
        ui.close_menu();
    }
}

/// After a listing (macOS): reads the native tags of the local entries on a worker.
pub fn listed(tx: &Sender<Msg>, ctx: &egui::Context, entries: &[Entry]) {
    #[cfg(target_os = "macos")]
    {
        let paths: Vec<(VPath, std::path::PathBuf)> = (entries.iter())
            .filter_map(|e| Some((e.path.clone(), e.path.to_local_path()?)))
            .take(MAX_NATIVE_READS)
            .collect();
        if paths.is_empty() {
            return;
        }
        let (tx, ctx) = (tx.clone(), ctx.clone());
        crate::worker::spawn("keel-finder-tags", move || {
            let read: Vec<(VPath, Vec<FinderTag>)> = (paths.into_iter())
                .map(|(p, local)| {
                    let tags = keel_core::finder_tags::read_native(&local).unwrap_or_default();
                    (p, tags)
                })
                .collect();
            let msg = crate::library::LibMsg::Native(read);
            crate::worker::send(&tx, &ctx, Msg::Library(msg));
        });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (tx, ctx, entries);
}

/// Writes a tag change through to the native tags of the local files among `paths`
/// (macOS; elsewhere tags live in the library only).
pub fn write_through(
    tx: &Sender<Msg>,
    ctx: &egui::Context,
    paths: Vec<VPath>,
    name: String,
    color: FinderColor,
    on: bool,
) {
    #[cfg(target_os = "macos")]
    {
        let local: Vec<(VPath, std::path::PathBuf)> = (paths.into_iter())
            .filter_map(|p| Some((p.clone(), p.to_local_path()?)))
            .collect();
        if local.is_empty() {
            return;
        }
        let (tx, ctx) = (tx.clone(), ctx.clone());
        crate::worker::spawn("keel-finder-tags", move || {
            use keel_core::finder_tags::{read_native, toggled, write_native};
            let mut done = Vec::new();
            let mut failed = None;
            for (p, l) in local {
                let result = read_native(&l).and_then(|tags| {
                    let tags = toggled(&tags, &name, color, on);
                    write_native(&l, &tags).map(|()| tags)
                });
                match result {
                    Ok(tags) => done.push((p, tags)),
                    Err(e) => failed = Some(format!("{}: {e}", l.display())),
                }
            }
            let msg = crate::library::LibMsg::Native(done);
            crate::worker::send(&tx, &ctx, Msg::Library(msg));
            if let Some(e) = failed {
                crate::worker::send(&tx, &ctx, Msg::Toast(format!("Finder tags: {e}")));
            }
        });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (tx, ctx, paths, name, color, on);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(id: TagId, name: &str, color: Option<&str>) -> Tag {
        Tag {
            id,
            name: name.into(),
            color: color.map(str::to_owned),
            parent: None,
        }
    }

    #[test]
    fn dots_follow_tag_order_skip_colorless_and_merge_native() {
        let tags = vec![
            tag(FAVORITES, "Favorites", Some("#ffcc00")),
            tag(2, "Work", Some("#ff3b30")),
            tag(3, "plain", None),
            tag(4, "Later", Some("#007aff")),
            tag(5, "Also red", Some("#FF3B30")),
            tag(6, "odd", Some("blue")),
        ];
        let red = Color32::from_rgb(0xff, 0x3b, 0x30);
        let blue = Color32::from_rgb(0x00, 0x7a, 0xff);
        let green = Color32::from_rgb(0x34, 0xc7, 0x59);
        let d = dots(&tags, &[4, FAVORITES, 3, 2, 5, 6, 99], &[]);
        let colors: Vec<Color32> = d.iter().map(|d| d.color).collect();
        // Favorites, colorless, unknown and malformed colors give no dot; one per color.
        assert_eq!(colors, [blue, red]);
        assert_eq!(describe(&d), "Tags: Later, Work");
        // Native tags the library does not have add their Finder color.
        let native = vec![
            FinderTag::new("work", FinderColor::Green),
            FinderTag::new("Finder", FinderColor::Green),
            FinderTag::new("none", FinderColor::None),
        ];
        let d = dots(&tags, &[2], &native);
        assert_eq!(
            d,
            [
                Dot {
                    color: red,
                    name: "Work".into()
                },
                Dot {
                    color: green,
                    name: "Finder".into()
                }
            ]
        );
        assert!(dots(&tags, &[], &[]).is_empty());
    }

    #[test]
    fn dot_room_and_the_tag_a_color_toggles() {
        assert_eq!(width(0), 0.0);
        assert!(width(1) > 0.0 && width(2) > width(1));
        assert_eq!(width(3), width(7), "at most three are drawn");
        let tags = vec![
            tag(2, "Urgent", Some("#e5484d")),
            tag(3, "Red", Some("#ff3b30")),
            tag(4, "Sky", Some("#3b82f6")),
            tag(5, "plain", None),
        ];
        assert_eq!(tag_for(&tags, FinderColor::Red).map(|t| t.id), Some(3));
        assert_eq!(tag_for(&tags, FinderColor::Blue).map(|t| t.id), Some(4));
        assert_eq!(tag_for(&tags, FinderColor::Green), None);
        assert_eq!(tag_for(&tags[..1], FinderColor::Red).map(|t| t.id), Some(2));
        assert_eq!(color32("#00FF00"), Some(Color32::from_rgb(0, 255, 0)));
        assert_eq!(color32("green"), None);
    }

    #[test]
    fn native_cache_takes_reads_and_drops_empty_ones() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let router = Arc::new(keel_vfs::Router::new());
        let mut l = LibraryUi::new(&router, tx, egui::Context::default());
        let a = VPath::local(std::env::temp_dir().join("a.txt"));
        l.native_read(vec![(a.clone(), vec![FinderTag::new("Red", FinderColor::Red)])]);
        assert_eq!(l.dots(&a).len(), 1);
        l.native_read(vec![(a.clone(), Vec::new())]);
        assert!(l.native.is_empty());
        assert!(l.dots(&a).is_empty());
    }
}
