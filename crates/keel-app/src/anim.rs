//! Motion: 120 ms ease-out-cubic transitions for the pane split, panels, the cursor row's
//! highlight and toasts. The "Reduce motion" setting sets egui's animation time to 0;
//! every transition here (and egui's own panel animations) reads it, so all of them
//! become instant. Lists and scroll positions never animate.

use egui::{style::ScrollAnimation, Context, Id};

pub const DURATION: f32 = 0.12;

pub fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

/// Applies the setting to egui's styles (every frame; only writes on a change). Scrolling
/// to a row is never animated, motion or not.
pub fn apply(ctx: &Context, reduce: bool) {
    let time = if reduce { 0.0 } else { DURATION };
    let style = ctx.style();
    if style.animation_time != time || style.scroll_animation != ScrollAnimation::none() {
        ctx.all_styles_mut(|s| {
            s.animation_time = time;
            s.scroll_animation = ScrollAnimation::none();
        });
    }
}

/// The transition length now: 0 with reduce motion.
pub fn duration(ctx: &Context) -> f32 {
    ctx.style().animation_time
}

/// `from` -> `to` after `elapsed` seconds of a `duration` transition.
pub fn at(from: f32, to: f32, elapsed: f32, duration: f32) -> f32 {
    if duration <= 0.0 || elapsed >= duration {
        to
    } else {
        from + (to - from) * ease_out_cubic(elapsed / duration)
    }
}

fn now(ctx: &Context) -> f64 {
    ctx.input(|i| i.time)
}

/// `target`, reached by an eased transition from the value shown when it changed.
pub fn value(ctx: &Context, id: Id, target: f32) -> f32 {
    let now = now(ctx);
    let dur = duration(ctx);
    let saved = ctx.data(|d| d.get_temp::<(f32, f32, f64)>(id));
    let (mut from, mut to, mut start) = saved.unwrap_or((target, target, now));
    if saved.is_none() || to != target {
        from = at(from, to, (now - start) as f32, dur);
        to = target;
        start = now;
        ctx.data_mut(|d| d.insert_temp(id, (from, to, start)));
    }
    let v = at(from, to, (now - start) as f32, dur);
    if v != to {
        ctx.request_repaint();
    }
    v
}

/// Jumps to `v` (a drag follows the pointer, it does not ease).
pub fn snap(ctx: &Context, id: Id, v: f32) {
    let now = now(ctx);
    ctx.data_mut(|d| d.insert_temp(id, (v, v, now)));
}

/// 0 -> 1 since `key` last changed (1 when it never did): the highlight of a row that
/// just got the cursor fades in.
pub fn fade_in(ctx: &Context, id: Id, key: Option<&str>) -> f32 {
    let now = now(ctx);
    let hash = Id::new(key).value();
    let start = match ctx.data(|d| d.get_temp::<(u64, f64)>(id)) {
        Some((h, start)) if h == hash => start,
        Some(_) => {
            ctx.data_mut(|d| d.insert_temp(id, (hash, now)));
            now
        }
        None => {
            ctx.data_mut(|d| d.insert_temp(id, (hash, f64::MIN)));
            f64::MIN
        }
    };
    let t = at(0.0, 1.0, (now - start) as f32, duration(ctx));
    if t < 1.0 {
        ctx.request_repaint();
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_cubic_starts_fast_and_lands_exactly() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(2.0), 1.0, "clamped");
        assert!((ease_out_cubic(0.5) - 0.875).abs() < 1e-6);
        let mut last = 0.0;
        for i in 1..=20 {
            let v = ease_out_cubic(i as f32 / 20.0);
            assert!(
                v > last && v >= i as f32 / 20.0,
                "monotonic, ahead of linear"
            );
            last = v;
        }
        assert_eq!(at(10.0, 20.0, 0.06, DURATION), 10.0 + 10.0 * 0.875);
        assert_eq!(at(10.0, 20.0, 0.5, DURATION), 20.0);
    }

    #[test]
    fn reduce_motion_returns_instant_values() {
        let ctx = Context::default();
        let id = Id::new("t");
        apply(&ctx, false);
        assert_eq!(duration(&ctx), DURATION);
        assert_eq!(value(&ctx, id, 0.5), 0.5, "first sight: no transition");
        // Time does not advance outside a frame: a change is still at its start.
        assert_eq!(value(&ctx, id, 1.0), 0.5);
        assert_eq!(fade_in(&ctx, id.with(1), Some("a")), 1.0, "first sight");
        assert_eq!(fade_in(&ctx, id.with(1), Some("b")), 0.0);
        apply(&ctx, true);
        assert_eq!(duration(&ctx), 0.0);
        assert_eq!(value(&ctx, id, 0.25), 0.25);
        assert_eq!(fade_in(&ctx, id.with(1), Some("c")), 1.0);
        assert_eq!(at(0.0, 7.0, 0.0, duration(&ctx)), 7.0);
        assert_eq!(ctx.style().scroll_animation, ScrollAnimation::none());
    }
}
