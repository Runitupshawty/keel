//! The filter chip row under the library search box (Task 57): Kind, Type, Tag, Source,
//! Size, Modified and Favorites. A chip edits the query text (`chips::toggle`) and lights
//! when the text has its term (`chips::is_on`), so clicking and typing agree; hovering a
//! chip shows its term. Every chip is a Tab stop with a name of its own, and Space (or
//! Enter) toggles the focused one.
//!
//! Only egui and `crate::chips`: keel-web shares this file by path.

use crate::chips::{self, Chip, Group};

/// Tags offered at most (more are typed: `tag:name`).
const MAX_TAGS: usize = 12;
/// Extensions suggested at most.
const MAX_TYPES: usize = 6;

/// What the row offers besides its fixed chips.
pub struct Offer<'a> {
    pub tags: &'a [String],
    pub sources: &'a [String],
    /// The index's most common extensions, most first.
    pub types: &'a [String],
}

/// Draws the row; true when a chip changed `query`.
pub fn chip_row(ui: &mut egui::Ui, query: &mut String, offer: &Offer) -> bool {
    let before = query.clone();
    ui.horizontal_wrapped(|ui| {
        group(ui, query, Group::Kind, Chip::kinds());
        let types = with_typed(query, Group::Type, offer.types, MAX_TYPES);
        let types = types.iter().filter_map(|e| Chip::ext(e)).collect();
        group(ui, query, Group::Type, types);
        ext_field(ui, query);
        ui.add_space(8.0);
        let tags = with_typed(query, Group::Tag, offer.tags, MAX_TAGS);
        group(
            ui,
            query,
            Group::Tag,
            tags.iter().filter_map(|t| Chip::tag(t)).collect(),
        );
        let sources = with_typed(query, Group::Source, offer.sources, usize::MAX);
        let sources = sources.iter().filter_map(|s| Chip::source(s)).collect();
        group(ui, query, Group::Source, sources);
        group(ui, query, Group::Size, Chip::sizes());
        group(ui, query, Group::Modified, Chip::modified());
        let fav = Chip::favorites();
        if chip(ui, query, &fav) {
            *query = chips::toggle(query, &fav);
        }
    });
    *query != before
}

/// Up to `max` offered values, then the ones the query has that are not among them (a tag
/// or extension typed by hand gets its chip too).
fn with_typed(query: &str, group: Group, offered: &[String], max: usize) -> Vec<String> {
    let mut out: Vec<String> = offered.iter().take(max).cloned().collect();
    for v in chips::values(query, group) {
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&v)) {
            out.push(v);
        }
    }
    out
}

fn group(ui: &mut egui::Ui, query: &mut String, group: Group, row: Vec<Chip>) {
    if row.is_empty() && group != Group::Type {
        return;
    }
    ui.weak(group.label());
    for c in &row {
        if chip(ui, query, c) {
            *query = chips::toggle(query, c);
        }
    }
    if group != Group::Type {
        ui.add_space(8.0);
    }
}

/// One chip; true when it was clicked (or Space/Enter pressed on it).
fn chip(ui: &mut egui::Ui, query: &str, c: &Chip) -> bool {
    let on = chips::is_on(query, c);
    let chip = egui::Button::new(&c.label).selected(on).corner_radius(8.0);
    let r = ui.add(chip).on_hover_text(&c.term);
    let name = c.name();
    r.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, &name)
    });
    r.clicked()
}

/// Another extension: Enter adds `ext:<typed>`.
fn ext_field(ui: &mut egui::Ui, query: &mut String) {
    let id = ui.id().with("keel-chip-ext");
    let mut text: String = ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
    let r = ui.add(
        egui::TextEdit::singleline(&mut text)
            .desired_width(56.0)
            .hint_text("ext"),
    );
    r.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Type: another extension")
    });
    let r = r.on_hover_text("Type an extension and press Enter: ext:<extension>");
    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        if let Some(c) = Chip::ext(&text).filter(|c| !chips::is_on(query, c)) {
            *query = chips::toggle(query, &c);
        }
        text.clear();
    }
    ui.data_mut(|d| d.insert_temp(id, text));
}
