//! Touch gestures as plain arithmetic (tested natively): pull to refresh, swipes between
//! items, pinch zoom in the viewer and pinch to resize the media grid's tiles.

/// How far (logical pixels) a list must be pulled down from its top to refresh.
pub const PULL_REFRESH: f32 = 70.0;
/// Shortest horizontal drag that counts as a swipe.
pub const SWIPE_MIN: f32 = 60.0;
/// Viewer zoom range.
pub const ZOOM: (f32, f32) = (1.0, 8.0);
/// Media grid tile sizes.
pub const TILE: (f32, f32) = (64.0, 320.0);

/// Pull to refresh on a list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pull {
    dist: f32,
}

impl Pull {
    /// One frame: `at_top` the list is scrolled to its top, `down` a finger (or button) is
    /// down on it, `dy` how far it moved this frame. True once, when a pull of at least
    /// `PULL_REFRESH` is let go.
    pub fn update(&mut self, at_top: bool, down: bool, dy: f32) -> bool {
        if down {
            if at_top || self.dist > 0.0 {
                self.dist = (self.dist + dy).max(0.0);
            }
            return false;
        }
        std::mem::take(&mut self.dist) >= PULL_REFRESH
    }

    /// [`Pull::update`] where pulling applies: only the phone layout refreshes this way (on
    /// a desktop a drag over a list selects or scrolls, it never reloads).
    pub fn update_on(&mut self, phone: bool, at_top: bool, down: bool, dy: f32) -> bool {
        if !phone {
            self.dist = 0.0;
            return false;
        }
        self.update(at_top, down, dy)
    }

    /// 0..=1, for the "release to refresh" hint.
    pub fn progress(&self) -> f32 {
        (self.dist / PULL_REFRESH).min(1.0)
    }
}

/// A finished drag as a swipe between items: 1 for the next one (dragged left), -1 for
/// the previous one, 0 for no swipe (too short, or more vertical than horizontal).
pub fn swipe(dx: f32, dy: f32) -> i32 {
    if dx.abs() < SWIPE_MIN || dx.abs() < 2.0 * dy.abs() {
        0
    } else if dx < 0.0 {
        1
    } else {
        -1
    }
}

/// The viewer's zoom after a pinch by `factor`.
pub fn zoom(current: f32, factor: f32) -> f32 {
    (current * factor).clamp(ZOOM.0, ZOOM.1)
}

/// The grid's tile size after a pinch by `factor`.
pub fn tile(current: f32, factor: f32) -> f32 {
    (current * factor).clamp(TILE.0, TILE.1)
}

/// The item `step` away from `at` among `count`, staying in range.
pub fn step(at: usize, step: i32, count: usize) -> usize {
    let last = count.saturating_sub(1) as i64;
    (at as i64 + i64::from(step)).clamp(0, last) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_to_refresh_fires_once_past_the_threshold_from_the_top() {
        let mut p = Pull::default();
        assert!(!p.update(false, true, 100.0), "not at the top: a scroll");
        assert!(!p.update(false, false, 0.0));
        for _ in 0..4 {
            assert!(!p.update(true, true, 20.0));
        }
        assert_eq!(p.progress(), 1.0);
        assert!(p.update(true, false, 0.0), "released past the threshold");
        assert!(!p.update(true, false, 0.0), "once");
        assert!(!p.update(true, true, 30.0));
        assert!(!p.update(false, true, -20.0), "pushed back up");
        assert!(!p.update(true, false, 0.0), "short pull");
    }

    #[test]
    fn only_the_phone_layout_pulls_to_refresh() {
        let mut p = Pull::default();
        for _ in 0..5 {
            assert!(!p.update_on(false, true, true, 30.0));
        }
        assert_eq!(p.progress(), 0.0, "a desktop drag builds no pull");
        assert!(!p.update_on(false, true, false, 0.0), "never refreshes");
        for _ in 0..5 {
            assert!(!p.update_on(true, true, true, 30.0));
        }
        assert!(p.update_on(true, true, false, 0.0), "the phone does");
    }

    #[test]
    fn swipes_pinches_and_steps() {
        assert_eq!(swipe(-120.0, 10.0), 1);
        assert_eq!(swipe(120.0, -10.0), -1);
        assert_eq!(swipe(30.0, 0.0), 0, "too short");
        assert_eq!(swipe(100.0, 80.0), 0, "mostly vertical");
        assert_eq!(zoom(1.0, 2.0), 2.0);
        assert_eq!(zoom(1.0, 0.5), 1.0);
        assert_eq!(zoom(6.0, 2.0), 8.0);
        assert_eq!(tile(128.0, 1.5), 192.0);
        assert_eq!(tile(80.0, 0.5), 64.0);
        assert_eq!(step(0, -1, 5), 0);
        assert_eq!(step(3, 1, 5), 4);
        assert_eq!(step(4, 1, 5), 4);
        assert_eq!(step(0, 1, 0), 0);
    }
}
