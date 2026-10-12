//! Window chrome of the Finder style (Task 65): the toolbar lives in the title bar.
//!
//! - **macOS:** a full-size content view under a transparent title bar; the system's
//!   traffic lights stay where they are and the toolbar starts right of them. This is set
//!   when the window opens: a style change takes effect there at the next start.
//! - **Windows:** no system title bar. Keel draws minimize, maximize and close in the
//!   Windows style, and `chrome_windows` answers the system's hit tests from [`Layout`]:
//!   empty toolbar space drags the window (double-click maximizes, right-click opens the
//!   system menu), the maximize button is the system's so Windows 11 shows its snap
//!   layouts on hover, and the top edge and corners resize (the other edges too).
//! - **Linux and the BSDs:** the window manager's decorations stay (tiling window managers
//!   keep working); the toolbar sits under them.
//!
//! The Classic style keeps the system title bar everywhere.

use crate::design::Style;
use egui::{Pos2, Rect};

#[cfg(windows)]
#[path = "chrome_windows.rs"]
mod native;

/// The smallest window in the Classic style (as before Task 65).
const MIN_CLASSIC: [f32; 2] = [640.0, 400.0];
/// The toolbar folds into its overflow menu, so the Finder style allows a narrower window.
const MIN_FINDER: [f32; 2] = [480.0, 360.0];
/// Room left of the toolbar for the traffic lights (macOS, full-size content view).
const TRAFFIC_LIGHTS: f32 = 78.0;
/// A title bar to drag by, when the window opened in the Finder style on macOS and was
/// switched to Classic since (the content view still reaches under the traffic lights).
pub const MAC_TITLE: f32 = 28.0;
/// One Windows caption button (minimize, maximize, close), as wide as the system's.
pub const CAPTION_BUTTON: f32 = 46.0;
/// The band along the window edges that resizes it (points).
pub const EDGE: f32 = 5.0;

/// The window as `main` opens it: what the style asks of the window manager.
pub fn viewport(builder: egui::ViewportBuilder, style: Style) -> egui::ViewportBuilder {
    if style == Style::Classic {
        return builder.with_min_inner_size(MIN_CLASSIC);
    }
    let builder = builder.with_min_inner_size(MIN_FINDER);
    if cfg!(windows) {
        builder.with_decorations(false)
    } else if cfg!(target_os = "macos") {
        builder
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false)
    } else {
        builder
    }
}

/// What a point of the window is to the window manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Keel's: a control, the content, a menu.
    Client,
    /// Drags the window; a double-click maximizes or restores it.
    Caption,
    /// The maximize button (Windows: the snap layouts flyout on hover).
    Maximize,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// The title bar as last drawn, in points from the window's top left corner.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// The whole window.
    pub window: Rect,
    /// The toolbar: its empty space is the caption.
    pub title: Rect,
    /// Toolbar controls and anything drawn over the toolbar (menus, windows): never caption.
    pub controls: Vec<Rect>,
    pub maximize: Option<Rect>,
    /// Maximized or full screen: no edges to resize by.
    pub maximized: bool,
    /// A menu or a modal dialog is open: a click anywhere must reach Keel (to close it).
    pub blocked: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            window: Rect::NOTHING,
            title: Rect::NOTHING,
            controls: Vec::new(),
            maximize: None,
            maximized: false,
            blocked: false,
        }
    }
}

impl Layout {
    /// What the window manager should do at `p`. Edges come first (a resize band is never
    /// covered by a control), then the maximize button, controls and the caption.
    pub fn hit(&self, p: Pos2) -> Hit {
        if !self.window.contains(p) {
            return Hit::Client;
        }
        if !self.maximized {
            let w = self.window;
            // Corners reach a little further along the edges, as the system's do.
            let corner = EDGE * 3.0;
            let left = p.x < w.left() + EDGE;
            let right = p.x >= w.right() - EDGE;
            let top = p.y < w.top() + EDGE;
            let bottom = p.y >= w.bottom() - EDGE;
            let near_left = p.x < w.left() + corner;
            let near_right = p.x >= w.right() - corner;
            let near_top = p.y < w.top() + corner;
            let near_bottom = p.y >= w.bottom() - corner;
            if (top && near_left) || (left && near_top) {
                return Hit::TopLeft;
            }
            if (top && near_right) || (right && near_top) {
                return Hit::TopRight;
            }
            if (bottom && near_left) || (left && near_bottom) {
                return Hit::BottomLeft;
            }
            if (bottom && near_right) || (right && near_bottom) {
                return Hit::BottomRight;
            }
            if left {
                return Hit::Left;
            }
            if right {
                return Hit::Right;
            }
            if top {
                return Hit::Top;
            }
            if bottom {
                return Hit::Bottom;
            }
        }
        if self.blocked {
            return Hit::Client;
        }
        if self.maximize.is_some_and(|r| r.contains(p)) {
            return Hit::Maximize;
        }
        if self.controls.iter().any(|r| r.contains(p)) {
            return Hit::Client;
        }
        if self.title.contains(p) {
            Hit::Caption
        } else {
            Hit::Client
        }
    }
}

/// The window's chrome for the style shown: insets for the system's buttons, Keel's own
/// caption buttons on Windows, and the hit testing behind them.
pub struct Chrome {
    /// Opened by `main` (false in tests: no native window, no insets).
    native: bool,
    /// The style the window was opened in.
    opened: Style,
    style: Style,
    #[cfg(windows)]
    windows: Option<native::Native>,
}

impl Chrome {
    /// `native`: the window was opened with [`viewport`] for `style`.
    pub fn new(cc: &eframe::CreationContext<'_>, style: Style, native: bool) -> Self {
        #[cfg(windows)]
        let windows = if native {
            native::Native::new(cc)
        } else {
            None
        };
        #[cfg(windows)]
        if native && style == Style::Finder && windows.is_none() {
            // No hit testing: the system title bar is the only way to move the window.
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Decorations(true));
        }
        #[cfg(not(windows))]
        let _ = cc;
        Self {
            native,
            opened: style,
            style,
            #[cfg(windows)]
            windows,
        }
    }

    /// Every frame: follows a style change (Windows swaps the title bar at once; macOS keeps
    /// the window it opened with).
    pub fn sync(&mut self, ctx: &egui::Context, style: Style) {
        if style != self.style {
            self.style = style;
            if self.native {
                let min = if style == Style::Finder {
                    MIN_FINDER
                } else {
                    MIN_CLASSIC
                };
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(min.into()));
                #[cfg(windows)]
                if self.windows.is_some() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(
                        style == Style::Classic,
                    ));
                }
            }
        }
        #[cfg(windows)]
        if let Some(windows) = &mut self.windows {
            windows.sync(style == Style::Finder, ctx.style().visuals.dark_mode);
        }
    }

    /// Keel draws minimize, maximize and close (Windows, Finder style).
    pub fn caption_buttons(&self) -> bool {
        #[cfg(windows)]
        {
            self.style == Style::Finder && self.windows.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// The window's content reaches under a transparent title bar (macOS, opened in the
    /// Finder style).
    pub fn fullsize(&self) -> bool {
        self.native && cfg!(target_os = "macos") && self.opened == Style::Finder
    }

    /// Points kept free at the left and right of the toolbar for the window's buttons.
    pub fn insets(&self) -> (f32, f32) {
        let left = if self.fullsize() { TRAFFIC_LIGHTS } else { 0.0 };
        let right = if self.caption_buttons() {
            3.0 * CAPTION_BUTTON
        } else {
            0.0
        };
        (left, right)
    }

    /// The maximize button is under the pointer (Windows: the system tracks it, as a
    /// nonclient area, and egui never sees the pointer there).
    pub fn maximize_hovered(&self) -> bool {
        #[cfg(windows)]
        {
            self.windows.as_ref().is_some_and(|w| w.hovered())
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// The title bar as drawn this frame, for the system's hit tests (Windows).
    pub fn publish(&mut self, ctx: &egui::Context, layout: Layout) {
        #[cfg(windows)]
        if let Some(windows) = &mut self.windows {
            windows.publish(layout, ctx.pixels_per_point());
        }
        #[cfg(not(windows))]
        let _ = (ctx, layout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2};

    /// An 800x600 window with a 38-point toolbar: a control at the left, Keel's caption
    /// buttons at the right.
    fn layout() -> Layout {
        Layout {
            window: Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0)),
            title: Rect::from_min_size(Pos2::ZERO, vec2(800.0, 38.0)),
            controls: vec![
                Rect::from_min_size(pos2(10.0, 5.0), vec2(200.0, 28.0)),
                Rect::from_min_size(pos2(662.0, 0.0), vec2(46.0, 38.0)),
                Rect::from_min_size(pos2(754.0, 0.0), vec2(46.0, 38.0)),
            ],
            maximize: Some(Rect::from_min_size(pos2(708.0, 0.0), vec2(46.0, 38.0))),
            ..Default::default()
        }
    }

    #[test]
    fn the_title_drags_and_buttons_do_not() {
        let l = layout();
        assert_eq!(l.hit(pos2(400.0, 20.0)), Hit::Caption, "empty toolbar");
        assert_eq!(l.hit(pos2(100.0, 20.0)), Hit::Client, "a toolbar control");
        assert_eq!(l.hit(pos2(685.0, 20.0)), Hit::Client, "minimize");
        assert_eq!(l.hit(pos2(730.0, 20.0)), Hit::Maximize, "maximize");
        assert_eq!(l.hit(pos2(777.0, 20.0)), Hit::Client, "close");
        assert_eq!(l.hit(pos2(400.0, 100.0)), Hit::Client, "content");
        assert_eq!(l.hit(pos2(-1.0, 20.0)), Hit::Client, "outside");
    }

    #[test]
    fn every_edge_and_corner_resizes() {
        let l = layout();
        for (x, y, hit) in [
            (1.0, 1.0, Hit::TopLeft),
            (799.0, 1.0, Hit::TopRight),
            (1.0, 599.0, Hit::BottomLeft),
            (799.0, 599.0, Hit::BottomRight),
            // Corners reach along the edges.
            (12.0, 1.0, Hit::TopLeft),
            (1.0, 12.0, Hit::TopLeft),
            (788.0, 598.0, Hit::BottomRight),
            (400.0, 1.0, Hit::Top),
            (400.0, 599.0, Hit::Bottom),
            (1.0, 300.0, Hit::Left),
            (799.0, 300.0, Hit::Right),
        ] {
            assert_eq!(l.hit(pos2(x, y)), hit, "at {x},{y}");
        }
        // Above the caption buttons the top edge still resizes; the buttons start below it.
        assert_eq!(l.hit(pos2(777.0, 2.0)), Hit::Top);
        assert_eq!(l.hit(pos2(730.0, 2.0)), Hit::Top);
        assert_eq!(l.hit(pos2(790.0, 2.0)), Hit::TopRight);
        assert_eq!(l.hit(pos2(730.0, 6.0)), Hit::Maximize);
    }

    #[test]
    fn maximized_windows_have_no_edges() {
        let mut l = layout();
        l.maximized = true;
        assert_eq!(l.hit(pos2(400.0, 1.0)), Hit::Caption);
        assert_eq!(l.hit(pos2(730.0, 1.0)), Hit::Maximize);
        assert_eq!(l.hit(pos2(1.0, 300.0)), Hit::Client);
        assert_eq!(l.hit(pos2(799.0, 599.0)), Hit::Client);
    }

    #[test]
    fn an_open_menu_takes_every_click_but_edges_still_resize() {
        let mut l = layout();
        l.blocked = true;
        assert_eq!(l.hit(pos2(400.0, 20.0)), Hit::Client);
        assert_eq!(l.hit(pos2(730.0, 20.0)), Hit::Client);
        assert_eq!(l.hit(pos2(1.0, 300.0)), Hit::Left);
    }

    #[test]
    fn a_window_over_the_toolbar_is_not_caption() {
        let mut l = layout();
        l.controls
            .push(Rect::from_min_size(pos2(300.0, 20.0), vec2(200.0, 300.0)));
        assert_eq!(l.hit(pos2(400.0, 30.0)), Hit::Client);
        assert_eq!(l.hit(pos2(250.0, 30.0)), Hit::Caption);
    }

    #[test]
    fn classic_and_linux_keep_the_system_title_bar() {
        let b = viewport(egui::ViewportBuilder::default(), Style::Classic);
        assert_eq!(b.decorations, None);
        assert_eq!(b.fullsize_content_view, None);
        assert_eq!(b.min_inner_size, Some(egui::Vec2::from(MIN_CLASSIC)));
        let f = viewport(egui::ViewportBuilder::default(), Style::Finder);
        if cfg!(windows) {
            assert_eq!(f.decorations, Some(false));
        } else if cfg!(target_os = "macos") {
            assert_eq!(f.fullsize_content_view, Some(true));
            assert_eq!(f.titlebar_shown, Some(false));
        } else {
            assert_eq!((f.decorations, f.fullsize_content_view), (None, None));
        }
    }
}
