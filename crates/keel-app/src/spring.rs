//! Spring-loaded folders (Phase 14 Task 67): a drag held over a folder (a row, a tile, a
//! path bar crumb) for [`DELAY`] opens it in its pane, so files can be dragged down a few
//! levels without letting go. Space while hovering opens it at once. Settings → General →
//! Spring-loaded folders turns it off.
//!
//! Each frame the folder under the drag reports itself with [`hovered`]; after the panes
//! are drawn [`tick`] runs the timer ([`Spring`]) and returns the folder to open.

use egui::{Context, Id};
use keel_vfs::VPath;
use std::time::{Duration, Instant};

/// How long a drag rests on a folder before it opens.
pub const DELAY: Duration = Duration::from_millis(600);

/// The timer: which folder (of which pane) the drag rests on, since when, and whether it
/// has opened already (once per rest).
#[derive(Clone, Debug, Default)]
pub struct Spring {
    over: Option<(usize, VPath)>,
    since: Option<Instant>,
    fired: bool,
}

impl Spring {
    /// One frame: `over` is the folder under the drag (None: no folder, or no drag), `now`
    /// the frame's time, `at_once` Space was pressed. Some: open this folder now.
    pub fn update(
        &mut self,
        over: Option<(usize, VPath)>,
        now: Instant,
        at_once: bool,
    ) -> Option<(usize, VPath)> {
        if over != self.over {
            self.over = over;
            self.since = self.over.as_ref().map(|_| now);
            self.fired = false;
        }
        let target = self.over.clone()?;
        let since = *self.since.get_or_insert(now);
        if self.fired {
            return None;
        }
        if at_once || now.saturating_duration_since(since) >= DELAY {
            self.fired = true;
            return Some(target);
        }
        None
    }

    /// Time left before the folder under the drag opens (None: nothing to wait for).
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        let since = self.since.filter(|_| self.over.is_some() && !self.fired)?;
        Some(DELAY.saturating_sub(now.saturating_duration_since(since)))
    }
}

fn over_id() -> Id {
    Id::new("keel-spring-over")
}

fn now_id() -> Id {
    Id::new("keel-spring-now")
}

fn state_id() -> Id {
    Id::new("keel-spring")
}

/// A folder of pane `pane` has a drag over it this frame.
pub fn hovered(ctx: &Context, pane: usize, path: &VPath) {
    ctx.data_mut(|d| d.insert_temp(over_id(), Some((pane, path.clone()))));
}

/// Space during a drag: the folder under it opens now.
pub fn space(ctx: &Context) {
    ctx.data_mut(|d| d.insert_temp(now_id(), true));
}

/// After the panes are drawn: runs the timer on this frame's folder and returns the one to
/// open (`enabled` false: the setting is off, nothing ever opens).
pub fn tick(ctx: &Context, enabled: bool) -> Option<(usize, VPath)> {
    let (over, at_once) = ctx.data_mut(|d| {
        let over = d.get_temp::<Option<(usize, VPath)>>(over_id()).flatten();
        let now = d.get_temp::<bool>(now_id()).unwrap_or(false);
        d.remove::<Option<(usize, VPath)>>(over_id());
        d.remove::<bool>(now_id());
        (over, now)
    });
    let dragging = egui::DragAndDrop::has_any_payload(ctx);
    let over = over.filter(|_| enabled && dragging);
    let mut spring = ctx.data(|d| d.get_temp::<Spring>(state_id()).unwrap_or_default());
    let now = Instant::now();
    let open = spring.update(over, now, at_once);
    if let Some(left) = spring.remaining(now) {
        ctx.request_repaint_after(left);
    }
    ctx.data_mut(|d| d.insert_temp(state_id(), spring));
    open
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> (usize, VPath) {
        (0, VPath::parse(&format!("mem://t/{name}")).unwrap())
    }

    #[test]
    fn a_folder_opens_after_the_delay_once_per_rest() {
        let t0 = Instant::now();
        let mut s = Spring::default();
        assert_eq!(s.update(Some(dir("a")), t0, false), None);
        assert_eq!(s.remaining(t0), Some(DELAY));
        let early = t0 + DELAY - Duration::from_millis(1);
        assert_eq!(s.update(Some(dir("a")), early, false), None);
        assert_eq!(s.update(Some(dir("a")), t0 + DELAY, false), Some(dir("a")));
        // Resting on: it does not open again.
        assert_eq!(s.update(Some(dir("a")), t0 + DELAY * 3, false), None);
        assert_eq!(s.remaining(t0 + DELAY * 3), None);
    }

    #[test]
    fn moving_to_another_folder_or_away_restarts_the_timer() {
        let t0 = Instant::now();
        let mut s = Spring::default();
        s.update(Some(dir("a")), t0, false);
        let t1 = t0 + Duration::from_millis(500);
        assert_eq!(s.update(Some(dir("b")), t1, false), None);
        // 600 ms after the first hover but only 100 ms on "b".
        assert_eq!(s.update(Some(dir("b")), t0 + DELAY, false), None);
        assert_eq!(s.update(Some(dir("b")), t1 + DELAY, false), Some(dir("b")));
        // Away and back: a new rest.
        let t2 = t1 + DELAY * 2;
        assert_eq!(s.update(None, t2, false), None);
        assert_eq!(s.remaining(t2), None);
        assert_eq!(s.update(Some(dir("b")), t2, false), None);
        assert_eq!(s.update(Some(dir("b")), t2 + DELAY, false), Some(dir("b")));
        // The same folder in the other pane is another folder.
        let other = (1, dir("b").1);
        assert_eq!(s.update(Some(other.clone()), t2 + DELAY, false), None);
    }

    #[test]
    fn space_opens_at_once_and_nothing_without_a_folder() {
        let t0 = Instant::now();
        let mut s = Spring::default();
        assert_eq!(s.update(None, t0, true), None);
        assert_eq!(s.update(Some(dir("a")), t0, true), Some(dir("a")));
        assert_eq!(s.update(Some(dir("a")), t0, true), None, "once per rest");
    }

    #[test]
    fn tick_needs_a_drag_and_the_setting() {
        let ctx = Context::default();
        let path = dir("a").1;
        let frame = |enabled: bool, drag: bool, space_down: bool| {
            let mut out = None;
            let _ = ctx.run(Default::default(), |ctx| {
                if drag {
                    egui::DragAndDrop::set_payload(ctx, 1u8);
                } else {
                    egui::DragAndDrop::clear_payload(ctx);
                }
                hovered(ctx, 0, &path);
                if space_down {
                    space(ctx);
                }
                out = tick(ctx, enabled);
            });
            out
        };
        assert_eq!(frame(true, false, true), None, "no drag");
        assert_eq!(frame(false, true, true), None, "setting off");
        assert_eq!(frame(true, true, true), Some((0, path.clone())));
    }
}
