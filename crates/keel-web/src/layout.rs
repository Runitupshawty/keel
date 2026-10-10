//! Which layout the client shows (tested natively): below `PHONE_BELOW` logical pixels of
//! width a phone layout (one pane, a bottom bar with Browse / Search / Library / Devices,
//! the preview as a full-screen sheet), the desktop layout otherwise. Going back to the
//! desktop layout needs `HYSTERESIS` more, so a width near the breakpoint (a rotating
//! phone, a window being resized) does not flip back and forth.

/// Narrower than this (logical pixels) is a phone.
pub const PHONE_BELOW: f32 = 700.0;
/// A phone layout turns into the desktop one only at `PHONE_BELOW + HYSTERESIS`.
pub const HYSTERESIS: f32 = 40.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Desktop,
    Phone,
}

impl Layout {
    /// The layout to start with at `width`.
    pub fn for_width(width: f32) -> Layout {
        if width < PHONE_BELOW {
            Layout::Phone
        } else {
            Layout::Desktop
        }
    }

    /// The layout after the width changed to `width`.
    pub fn next(self, width: f32) -> Layout {
        match self {
            Layout::Desktop if width < PHONE_BELOW => Layout::Phone,
            Layout::Phone if width >= PHONE_BELOW + HYSTERESIS => Layout::Desktop,
            same => same,
        }
    }
}

/// The phone layout's bottom bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Browse,
    Search,
    Library,
    Devices,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Browse, Tab::Search, Tab::Library, Tab::Devices];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Browse => "Browse",
            Tab::Search => "Search",
            Tab::Library => "Library",
            Tab::Devices => "Devices",
        }
    }
}

/// What is on screen: the layout, the phone tab, and whether the preview sheet covers it.
#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub layout: Layout,
    pub tab: Tab,
    /// Phone only: the preview as a full-screen sheet.
    pub sheet: bool,
}

impl Screen {
    pub fn new(width: f32) -> Self {
        Self {
            layout: Layout::for_width(width),
            tab: Tab::Browse,
            sheet: false,
        }
    }

    pub fn phone(&self) -> bool {
        self.layout == Layout::Phone
    }

    /// Every frame: true when the layout changed. The desktop shows the preview in its
    /// side panel, so the sheet closes.
    pub fn resize(&mut self, width: f32) -> bool {
        let next = self.layout.next(width);
        let changed = next != self.layout;
        self.layout = next;
        if !self.phone() {
            self.sheet = false;
        }
        changed
    }

    /// A file was opened: on a phone the preview sheet covers the list.
    pub fn open_preview(&mut self) {
        self.sheet = self.phone();
    }

    /// Back (the sheet's button, Escape): closes the sheet; false when there was none.
    pub fn back(&mut self) -> bool {
        std::mem::take(&mut self.sheet)
    }

    pub fn show(&mut self, tab: Tab) {
        self.tab = tab;
        self.sheet = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoint_with_hysteresis() {
        assert_eq!(Layout::for_width(360.0), Layout::Phone);
        assert_eq!(Layout::for_width(699.0), Layout::Phone);
        assert_eq!(Layout::for_width(700.0), Layout::Desktop);
        assert_eq!(Layout::for_width(1280.0), Layout::Desktop);
        let mut l = Layout::Desktop;
        for (w, want) in [
            (900.0, Layout::Desktop),
            (699.0, Layout::Phone),
            (705.0, Layout::Phone),
            (739.0, Layout::Phone),
            (740.0, Layout::Desktop),
            (720.0, Layout::Desktop),
            (650.0, Layout::Phone),
        ] {
            l = l.next(w);
            assert_eq!(l, want, "at {w}");
        }
    }

    #[test]
    fn the_sheet_is_phone_only_and_closes_on_back_tab_and_widening() {
        let mut s = Screen::new(1024.0);
        s.open_preview();
        assert!(!s.sheet, "the desktop previews in its side panel");
        assert!(!s.back());
        assert!(s.resize(390.0));
        assert!(!s.resize(395.0), "no change");
        s.open_preview();
        assert!(s.sheet);
        assert!(s.back());
        assert!(!s.sheet && !s.back());
        s.open_preview();
        s.show(Tab::Devices);
        assert_eq!((s.tab, s.sheet), (Tab::Devices, false));
        s.open_preview();
        assert!(s.resize(1024.0));
        assert_eq!((s.layout, s.sheet), (Layout::Desktop, false));
        assert_eq!(
            Tab::ALL.map(Tab::label),
            ["Browse", "Search", "Library", "Devices"]
        );
    }
}
