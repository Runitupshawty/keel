//! Accessibility: names for controls drawn as symbols, the file list's keyboard focus
//! (the cursor row holds it, so a screen reader reads each row the cursor moves to),
//! live announcements, and Tab leaving the sidebar in one press. egui builds the
//! AccessKit tree; these only fill in what its widgets cannot know.

use egui::accesskit::{Live, Node, Role};
use egui::{Context, Id, Key, LayerId, Rect, Response, Sense, Ui};

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

/// Clicks and drags but no Tab stop: file rows and tiles (the cursor row is the list's one
/// Tab stop, see `item`) and drag handles.
pub const NO_TAB: Sense = Sense::CLICK.union(Sense::DRAG);

/// The id of the widget that held the file list's keyboard focus last.
fn list_focus_id() -> Id {
    Id::new("keel-list-focus")
}

/// The file list has the keys: nothing has the keyboard focus, or the list's cursor row
/// has it. Otherwise a focused button, checkbox or sidebar item keeps Enter, Space and the
/// arrow keys for itself (Esc gives the focus back to the list).
pub fn list_has_keys(ctx: &Context) -> bool {
    let focused = ctx.memory(|m| m.focused());
    focused.is_none() || focused == ctx.data(|d| d.get_temp::<Id>(list_focus_id()))
}

/// A file row or tile: `label` (name, size, modified) and its selection for screen
/// readers. The cursor row of the pane with the keys (`cursor`) is the list's Tab stop and
/// takes the keyboard focus whenever nothing else holds it (`focus_ring` shows it).
pub fn item(ui: &Ui, r: &Response, label: &str, selected: bool, cursor: bool) {
    let ctx = ui.ctx();
    not_a_stop(r);
    let fill = |b: &mut Node| {
        b.set_role(Role::ListItem);
        b.set_label(label);
        b.set_selected(selected);
        b.set_bounds(bounds(r.rect));
    };
    ctx.accesskit_node_builder(r.id, |b| {
        fill(b);
        b.add_action(egui::accesskit::Action::Click);
    });
    if !cursor {
        return;
    }
    let f = ui.interact(r.rect, r.id.with("keel-kb"), Sense::FOCUSABLE);
    ctx.accesskit_node_builder(f.id, fill);
    let last = ctx.data(|d| d.get_temp::<Id>(list_focus_id()));
    let focused = ctx.memory(|m| m.focused());
    let tab = ctx.input(|i| i.key_pressed(Key::Tab));
    let free = ctx.memory(|m| m.allows_interaction(ui.layer_id()));
    // Not in the frame Tab leaves the list (egui has just cleared the focus for that).
    if free && focused != Some(f.id) && (focused.is_none() && !tab || focused == last) {
        f.request_focus();
    }
    ctx.data_mut(|d| d.insert_temp(list_focus_id(), f.id));
    if ctx.memory(|m| m.has_focus(f.id)) {
        // The arrow keys move the cursor (keys.rs), not egui's focus.
        let filter = egui::EventFilter {
            horizontal_arrows: true,
            vertical_arrows: true,
            ..Default::default()
        };
        ctx.memory_mut(|m| m.set_focus_lock_filter(f.id, filter));
    }
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

/// The background layer's widget count: `group_start` / `group_end` around a region
/// (the sidebar) record which widgets it made this frame.
fn widget_count(ctx: &Context) -> usize {
    ctx.viewport(|v| v.this_pass.widgets.get_layer(LayerId::background()).count())
}

pub fn group_start(ctx: &Context, name: &'static str) {
    let n = widget_count(ctx);
    ctx.data_mut(|d| d.insert_temp(Id::new(("keel-group-start", name)), n));
}

pub fn group_end(ctx: &Context, name: &'static str) {
    let end = widget_count(ctx);
    let start = ctx
        .data(|d| d.get_temp::<usize>(Id::new(("keel-group-start", name))))
        .unwrap_or(end);
    ctx.data_mut(|d| d.insert_temp(Id::new(("keel-group", name)), (start, end)));
}

/// Tab / Shift+Tab with the focus inside group `name` leave the group in one press: the
/// focus goes to its last (first) Tab stop, from where egui moves it on. Arrow keys move
/// inside the group (egui). Call at the start of a frame, before the group is drawn.
pub fn tab_out(ctx: &Context, name: &'static str) {
    let Some(focused) = ctx.memory(|m| m.focused()) else {
        return;
    };
    let (tab, back) = ctx.input(|i| (i.key_pressed(Key::Tab), i.modifiers.shift));
    let range = ctx.data(|d| d.get_temp::<(usize, usize)>(Id::new(("keel-group", name))));
    let Some((start, end)) = range.filter(|_| tab) else {
        return;
    };
    let stops: Vec<Id> = ctx.viewport(|v| {
        (v.prev_pass.widgets.get_layer(LayerId::background()))
            .skip(start)
            .take(end.saturating_sub(start))
            .filter(|w| w.enabled && w.sense.is_focusable())
            .map(|w| w.id)
            .collect()
    });
    if !stops.contains(&focused) {
        return;
    }
    // Not a dead stop: `skip_dead_stop` would move the focus off it before Tab does.
    let dead = ctx
        .data(|d| d.get_temp::<Vec<Id>>(dead_id()))
        .unwrap_or_default();
    let stops: Vec<Id> = stops.into_iter().filter(|id| !dead.contains(id)).collect();
    let edge = if back { stops.first() } else { stops.last() };
    if let Some(&id) = edge {
        ctx.memory_mut(|m| m.request_focus(id));
    }
}

fn dead_id() -> Id {
    Id::new("keel-dead-stops")
}

/// `r` is no Tab stop, though egui makes it one: `Response::context_menu` adds a focusable
/// click sense, so every file row (and the empty space under them) would be a stop.
/// Skipped like the dead stops.
pub fn not_a_stop(r: &Response) {
    let pass = r.ctx.cumulative_pass_nr();
    r.ctx.data_mut(|d| {
        let (at, ids) = d.get_temp_mut_or_default::<(u64, Vec<Id>)>(Id::new("keel-not-stops"));
        if *at != pass {
            *at = pass;
            ids.clear();
        }
        ids.push(r.id);
    });
}

/// egui makes some pointer-only widgets Tab stops: a scroll area's drag surface, panel and
/// table column resize handles. They have no name and no role (`Role::Unknown`): hidden
/// from screen readers here and remembered for `skip_dead_stop`. Call at the end of a
/// frame (the tree is built every frame, `App::new` turns it on).
pub fn hide_dead_stops(ctx: &Context) {
    let ids: Vec<Id> = ctx.viewport(|v| {
        (v.this_pass.widgets.layers())
            .flat_map(|(_, ws)| ws.iter())
            .filter(|w| w.enabled && w.sense.is_focusable())
            .map(|w| w.id)
            .collect()
    });
    let dead: Vec<Id> = (ids.into_iter())
        .filter(|&id| {
            ctx.accesskit_node_builder(id, |b| {
                let dead = b.role() == Role::Unknown && b.label().is_none();
                if dead {
                    b.set_hidden();
                }
                dead
            })
            .unwrap_or(false)
        })
        .collect();
    let pass = ctx.cumulative_pass_nr();
    let rows = ctx.data(|d| d.get_temp::<(u64, Vec<Id>)>(Id::new("keel-not-stops")));
    let rows = rows.filter(|(at, _)| *at == pass).map(|(_, ids)| ids);
    let dead: Vec<Id> = dead.into_iter().chain(rows.into_iter().flatten()).collect();
    ctx.data_mut(|d| d.insert_temp(dead_id(), dead));
}

/// Tab (or Shift+Tab) landed on a dead stop last frame: the focus moves on to the next
/// (previous) real one in that layer. Call at the start of a frame.
pub fn skip_dead_stop(ctx: &Context) {
    let back_id = Id::new("keel-tab-back");
    let (tab, shift) = ctx.input(|i| (i.key_pressed(Key::Tab), i.modifiers.shift));
    let back = ctx.data(|d| d.get_temp::<bool>(back_id)).unwrap_or(false);
    if tab {
        ctx.data_mut(|d| d.insert_temp(back_id, shift));
    }
    let dead = ctx
        .data(|d| d.get_temp::<Vec<Id>>(dead_id()))
        .unwrap_or_default();
    let Some(focused) = ctx.memory(|m| m.focused()).filter(|f| dead.contains(f)) else {
        return;
    };
    let next = ctx.viewport(|v| {
        let layer = v.prev_pass.widgets.get(focused)?.layer_id;
        let stops: Vec<Id> = (v.prev_pass.widgets.get_layer(layer))
            .filter(|w| w.enabled && w.sense.is_focusable())
            .map(|w| w.id)
            .collect();
        let at = stops.iter().position(|&id| id == focused)?;
        let n = stops.len();
        (1..n)
            .map(|k| if back { (at + n - k) % n } else { (at + k) % n })
            .map(|i| stops[i])
            .find(|id| !dead.contains(id))
    });
    if let Some(id) = next {
        ctx.memory_mut(|m| m.request_focus(id));
    }
}

/// A dialog's first focus: `r` (its primary action or first field) while nothing has the
/// focus (on open; not in the frame Tab is moving it). Enter or Space then presses it.
pub fn first_focus(r: &Response) {
    let none = r.ctx.memory(|m| m.focused().is_none());
    if none && !r.ctx.input(|i| i.key_pressed(Key::Tab)) {
        r.request_focus();
    }
}

/// Esc closes the window titled `title` (an `egui::Window`, which has no Esc of its own)
/// when it is the frontmost one and no popup or modal is above it. Call before showing it.
pub fn esc_closes(ctx: &Context, title: &str, open: &mut bool) {
    esc_closes_id(ctx, Id::new(title), open);
}

/// `esc_closes` for a window with its own id (`egui::Window::id`).
pub fn esc_closes_id(ctx: &Context, id: Id, open: &mut bool) {
    // Raw: the list may have consumed Esc already (it clears the filter too).
    let esc = ctx.input(|i| {
        (i.raw.events.iter()).any(|e| {
            matches!(
                e,
                egui::Event::Key {
                    key: Key::Escape,
                    pressed: true,
                    ..
                }
            )
        })
    });
    if !*open || !esc {
        return;
    }
    let layer = LayerId::new(egui::Order::Middle, id);
    let front = ctx.memory(|m| {
        let top = (m.layer_ids())
            .filter(|l| l.order == egui::Order::Middle && m.areas().is_visible(l))
            .last();
        top == Some(layer) && !m.any_popup_open() && m.is_above_modal_layer(layer)
    });
    if front {
        *open = false;
    }
}

/// Draws a ring around the focused widget (frameless buttons, sidebar items and file rows
/// show no focus of their own) while the keyboard is in use: from a key press until the
/// next pointer press, like a browser's `:focus-visible`. Text boxes draw their own. Call
/// at the end of a frame.
pub fn focus_ring(ctx: &Context) {
    let id = Id::new("keel-keyboard-mode");
    let mut keyboard = ctx.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    ctx.input(|i| {
        for e in &i.raw.events {
            match e {
                egui::Event::Key { pressed: true, .. } => keyboard = true,
                egui::Event::PointerButton { pressed: true, .. } => keyboard = false,
                _ => {}
            }
        }
    });
    ctx.data_mut(|d| d.insert_temp(id, keyboard));
    let Some(focused) = ctx.memory(|m| m.focused()).filter(|_| keyboard) else {
        return;
    };
    if egui::TextEdit::load_state(ctx, focused).is_some() {
        return;
    }
    let Some(w) = ctx.viewport(|v| v.this_pass.widgets.get(focused).copied()) else {
        return;
    };
    if w.interact_rect.area() <= 0.0 {
        return;
    }
    let stroke = egui::Stroke::new(2.0_f32, ring_color(&ctx.style().visuals));
    ctx.layer_painter(w.layer_id).rect_stroke(
        w.interact_rect,
        2.0,
        stroke,
        egui::StrokeKind::Inside,
    );
}

/// The focus ring's colour (checked against the panel colour in the theme tests).
pub fn ring_color(v: &egui::Visuals) -> egui::Color32 {
    v.selection.stroke.color
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
