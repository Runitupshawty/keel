//! Accessibility: names for controls drawn as symbols, file rows for screen readers and
//! live announcements. egui builds the AccessKit tree; these only fill in what its
//! widgets cannot know.

use egui::accesskit::{Live, Role};
use egui::{Context, Id, Rect, Response, Ui};

fn bounds(r: Rect) -> egui::accesskit::Rect {
    egui::accesskit::Rect {
        x0: r.min.x.into(),
        y0: r.min.y.into(),
        x1: r.max.x.into(),
        y1: r.max.y.into(),
    }
}

/// Accessible names for responses; `tip` also shows `name` as the tooltip.
pub trait Named: Sized {
    fn named(self, name: &str) -> Self;
    fn tip(self, name: &str) -> Self;
}

impl Named for Response {
    fn named(self, name: &str) -> Self {
        self.ctx.accesskit_node_builder(self.id, |b| {
            // A bare `interact` area (the path bar's empty part) has no role yet.
            if b.role() == Role::Unknown {
                b.set_role(Role::Button);
            }
            b.set_label(name);
        });
        self
    }

    fn tip(self, name: &str) -> Self {
        self.named(name).on_hover_text(name)
    }
}

/// A file row or tile: `label` (name, size, modified) and its selection for screen
/// readers.
pub fn item(ui: &Ui, r: &Response, label: &str, selected: bool) {
    ui.ctx().accesskit_node_builder(r.id, |b| {
        b.set_role(Role::ListItem);
        b.set_label(label);
        b.set_selected(selected);
        b.set_bounds(bounds(r.rect));
        b.add_action(egui::accesskit::Action::Click);
    });
}

/// A live region: screen readers announce `text` whenever it changes (the folder and its
/// item count after navigation, toasts). Drawn by the caller; this is the tree node.
pub fn live(ctx: &Context, id: Id, rect: Rect, text: &str, assertive: bool) {
    ctx.accesskit_node_builder(id, |b| {
        b.set_role(Role::Status);
        b.set_label(text);
        b.set_live(if assertive {
            Live::Assertive
        } else {
            Live::Polite
        });
        b.set_bounds(bounds(rect));
    });
}

/// Marks `r` (a label) as a live region.
pub fn live_label(r: &Response, assertive: bool) {
    r.ctx.accesskit_node_builder(r.id, |b| {
        b.set_live(if assertive {
            Live::Assertive
        } else {
            Live::Polite
        })
    });
}

/// egui makes some pointer-only widgets focusable: a scroll area's drag surface, panel
/// and table column resize handles. They have no name and no role (`Role::Unknown`):
/// hidden from screen readers. Call at the end of a frame.
pub fn hide_dead_stops(ctx: &Context) {
    let ids: Vec<Id> = ctx.viewport(|v| {
        (v.this_pass.widgets.layers())
            .flat_map(|(_, ws)| ws.iter())
            .filter(|w| w.enabled && w.sense.is_focusable())
            .map(|w| w.id)
            .collect()
    });
    for id in ids {
        ctx.accesskit_node_builder(id, |b| {
            if b.role() == Role::Unknown && b.label().is_none() {
                b.set_hidden();
            }
        });
    }
}

/// The focused widget always has a node in the tree: AccessKit refuses (panics on) an
/// update whose focus is not in it, and egui keeps a focus requested this frame for a
/// widget that was not drawn (a row scrolled away, a column the cursor left). Call at the
/// end of a frame.
pub fn focus_has_node(ctx: &Context) {
    if let Some(id) = ctx.memory(|m| m.focused()) {
        ctx.accesskit_node_builder(id, |_| ());
    }
}
