//! Task 66: view settings remembered per folder, in the profile's config.toml (`[views]`):
//! "Always open in <view>", the Icon view's options (icon size, grid spacing, label
//! position, arrangement) and the icon positions of folders arranged by hand. Folders are
//! keyed like folder styles (`folder_style::key`: Windows paths in either case).

use crate::pane::ViewMode;
use keel_vfs::VPath;
use std::collections::BTreeMap;

pub const MIN_ICON: f32 = 16.0;
pub const MAX_ICON: f32 = 512.0;
pub const MAX_SPACING: f32 = 64.0;
/// Folders remembered at most; the least recently changed one is forgotten first.
pub const MAX_FOLDERS: usize = 1000;

/// Where an icon's name goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelPos {
    #[default]
    Bottom,
    Right,
}

/// How the Icon view places items.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arrange {
    /// In the folder's sort order (the tab's Sort by); icons cannot be moved by hand.
    #[default]
    SortBy,
    /// Moved by hand; a dropped icon lands on the nearest free grid cell.
    SnapToGrid,
    /// Moved by hand anywhere.
    Free,
}

impl Arrange {
    pub const ALL: [Arrange; 3] = [Self::SortBy, Self::SnapToGrid, Self::Free];

    pub fn label(self) -> String {
        match self {
            Self::SortBy => crate::i18n::tr("view_grid.sort_by_folder"),
            Self::SnapToGrid => crate::i18n::tr("view_grid.snap_to_grid"),
            Self::Free => crate::i18n::tr("view_grid.free"),
        }
        .to_owned()
    }

    /// Icons keep the positions they are dragged to.
    pub fn by_hand(self) -> bool {
        self != Self::SortBy
    }
}

/// The Icon view's options.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct IconOptions {
    /// Icon (and thumbnail) box in points, 16 to 512.
    pub icon_size: f32,
    /// Extra room around each grid cell in points, 0 to 64.
    pub spacing: f32,
    pub label: LabelPos,
    pub arrange: Arrange,
}

impl Default for IconOptions {
    fn default() -> Self {
        Self {
            icon_size: 96.0,
            spacing: 6.0,
            label: LabelPos::Bottom,
            arrange: Arrange::SortBy,
        }
    }
}

impl IconOptions {
    /// Sizes inside their ranges (a hand-edited config.toml can hold anything).
    pub fn clamped(mut self) -> Self {
        let ok = |v: f32, lo: f32, hi: f32, default: f32| {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                default
            }
        };
        let d = Self::default();
        self.icon_size = ok(self.icon_size, MIN_ICON, MAX_ICON, d.icon_size);
        self.spacing = ok(self.spacing, 0.0, MAX_SPACING, d.spacing);
        self
    }
}

/// What is remembered of one folder.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FolderView {
    /// "Always open in": the view the folder opens in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<ViewMode>,
    /// The folder's own Icon view options (else the defaults).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<IconOptions>,
    /// Icon positions by entry name, in grid cells (x = column, y = row; fractions in free
    /// arrangement), so they follow the icon size.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub positions: BTreeMap<String, [f32; 2]>,
    /// When it last changed (`ViewPrefs::clock`), for forgetting the oldest.
    pub used: u64,
}

impl FolderView {
    fn is_empty(&self) -> bool {
        self.view.is_none() && self.icons.is_none() && self.positions.is_empty()
    }
}

/// `[views]` in config.toml.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ViewPrefs {
    /// Icon view options of folders without their own ("Use as defaults").
    pub icons: IconOptions,
    /// By `folder_style::key` of the folder.
    pub folders: BTreeMap<String, FolderView>,
    /// Bumped on every change of a folder's entry.
    pub clock: u64,
}

impl ViewPrefs {
    pub fn folder(&self, dir: &VPath) -> Option<&FolderView> {
        self.folders.get(&crate::folder_style::key(dir))
    }

    /// The folder's entry for changing (created when missing). Past `MAX_FOLDERS` the
    /// least recently changed other folder is forgotten.
    pub fn folder_mut(&mut self, dir: &VPath) -> &mut FolderView {
        let key = crate::folder_style::key(dir);
        self.clock += 1;
        if !self.folders.contains_key(&key) && self.folders.len() >= MAX_FOLDERS {
            // ponytail: O(n) over at most 1,000 folders, only when a new folder is added.
            if let Some(oldest) = (self.folders.iter())
                .min_by_key(|(_, f)| f.used)
                .map(|(k, _)| k.clone())
            {
                self.folders.remove(&oldest);
            }
        }
        let clock = self.clock;
        let f = self.folders.entry(key).or_default();
        f.used = clock;
        f
    }

    /// Drops the folder's entry when nothing is left in it.
    fn tidy(&mut self, dir: &VPath) {
        let key = crate::folder_style::key(dir);
        if self.folders.get(&key).is_some_and(FolderView::is_empty) {
            self.folders.remove(&key);
        }
    }

    /// The folder's "Always open in" view.
    pub fn always_open(&self, dir: &VPath) -> Option<ViewMode> {
        self.folder(dir).and_then(|f| f.view)
    }

    pub fn set_always_open(&mut self, dir: &VPath, view: Option<ViewMode>) {
        if view.is_none() && self.folder(dir).is_none() {
            return;
        }
        self.folder_mut(dir).view = view;
        self.tidy(dir);
    }

    /// The Icon view options in effect for the folder.
    pub fn icons_for(&self, dir: &VPath) -> IconOptions {
        (self.folder(dir).and_then(|f| f.icons))
            .unwrap_or(self.icons)
            .clamped()
    }

    /// Sets the folder's own Icon view options (Finder: options are per folder).
    pub fn set_icons(&mut self, dir: &VPath, icons: IconOptions) {
        if self.icons_for(dir) != icons {
            self.folder_mut(dir).icons = Some(icons.clamped());
        }
    }

    /// "Use as defaults": the folder's options become everyone's, the folder follows them.
    pub fn use_as_defaults(&mut self, dir: &VPath) {
        self.icons = self.icons_for(dir);
        if self.folder(dir).is_some_and(|f| f.icons.is_some()) {
            self.folder_mut(dir).icons = None;
            self.tidy(dir);
        }
    }

    pub fn positions(&self, dir: &VPath) -> Option<&BTreeMap<String, [f32; 2]>> {
        self.folder(dir)
            .map(|f| &f.positions)
            .filter(|p| !p.is_empty())
    }

    /// Replaces the folder's icon positions (an empty map forgets them).
    pub fn set_positions(&mut self, dir: &VPath, positions: BTreeMap<String, [f32; 2]>) {
        if self.folder(dir).map(|f| &f.positions) == Some(&positions)
            || (positions.is_empty() && self.folder(dir).is_none())
        {
            return;
        }
        self.folder_mut(dir).positions = positions;
        self.tidy(dir);
    }
}

/// A view's name as the view buttons show it.
pub fn view_name(view: ViewMode) -> &'static str {
    match view {
        ViewMode::Details => "Details",
        ViewMode::Grid => "Grid",
        ViewMode::Columns => "Columns",
        ViewMode::Media => "Media",
        ViewMode::Sizes => "Sizes",
        ViewMode::Gallery => "Gallery",
        ViewMode::DiskUsage => "Disk usage",
    }
}

/// The view a pane switches to when it shows a new folder: the folder's "Always open in"
/// view (the view it had is kept in `before`), else the view it had before entering a
/// remembered folder, else `current`.
pub fn view_on_enter(
    remembered: Option<ViewMode>,
    current: ViewMode,
    before: &mut Option<ViewMode>,
) -> ViewMode {
    match remembered {
        Some(view) => {
            before.get_or_insert(current);
            view
        }
        None => before.take().unwrap_or(current),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(p: &str) -> VPath {
        VPath::parse(&format!("mem://t{p}")).unwrap()
    }

    #[test]
    fn always_open_in_switches_and_restores_the_view() {
        let mut before = None;
        // A plain folder keeps the pane's view.
        assert_eq!(
            view_on_enter(None, ViewMode::Details, &mut before),
            ViewMode::Details
        );
        // A gallery folder switches, remembering Details.
        let v = view_on_enter(Some(ViewMode::Gallery), ViewMode::Details, &mut before);
        assert_eq!(v, ViewMode::Gallery);
        assert_eq!(before, Some(ViewMode::Details));
        // Another remembered folder: still remembers the first view.
        let v = view_on_enter(Some(ViewMode::Grid), v, &mut before);
        assert_eq!(v, ViewMode::Grid);
        assert_eq!(before, Some(ViewMode::Details));
        // Back to a plain folder: the view from before.
        assert_eq!(view_on_enter(None, v, &mut before), ViewMode::Details);
        assert_eq!(before, None);
    }

    #[test]
    fn folder_memory_round_trips_through_config_toml() {
        let mut s = crate::settings::Settings::default();
        let a = dir("/photos");
        s.views.set_always_open(&a, Some(ViewMode::Gallery));
        let opts = IconOptions {
            icon_size: 200.0,
            spacing: 20.0,
            label: LabelPos::Right,
            arrange: Arrange::Free,
        };
        s.views.set_icons(&a, opts);
        let positions: BTreeMap<String, [f32; 2]> = [
            ("a b.png".to_owned(), [0.5, 2.0]),
            ("c\"d.txt".to_owned(), [3.0, 0.0]),
        ]
        .into();
        s.views.set_positions(&a, positions.clone());
        let text = toml::to_string_pretty(&s).unwrap();
        let (back, dropped) = crate::settings::parse_lenient(&text).unwrap();
        assert!(dropped.is_empty());
        assert_eq!(back.views, s.views);
        assert_eq!(back.views.always_open(&a), Some(ViewMode::Gallery));
        assert_eq!(back.views.icons_for(&a), opts);
        assert_eq!(back.views.positions(&a), Some(&positions));
        // Other folders use the defaults.
        assert_eq!(back.views.icons_for(&dir("/x")), IconOptions::default());
    }

    #[test]
    fn empty_entries_are_dropped_and_old_ones_forgotten() {
        let mut v = ViewPrefs::default();
        let a = dir("/a");
        v.set_always_open(&a, Some(ViewMode::Grid));
        v.set_always_open(&a, None);
        assert!(v.folders.is_empty(), "nothing left: forgotten");
        // Clearing what was never set adds nothing.
        v.set_positions(&a, BTreeMap::new());
        assert!(v.folders.is_empty());
        for i in 0..MAX_FOLDERS + 5 {
            v.set_always_open(&dir(&format!("/f{i}")), Some(ViewMode::Grid));
        }
        assert_eq!(v.folders.len(), MAX_FOLDERS);
        assert!(v.folder(&dir("/f0")).is_none(), "oldest forgotten");
        assert!(v.folder(&dir(&format!("/f{}", MAX_FOLDERS + 4))).is_some());
    }

    #[test]
    fn use_as_defaults_moves_the_folder_options_to_everyone() {
        let mut v = ViewPrefs::default();
        let a = dir("/a");
        let big = IconOptions {
            icon_size: 300.0,
            ..Default::default()
        };
        v.set_icons(&a, big);
        assert_eq!(v.icons_for(&dir("/b")), IconOptions::default());
        v.use_as_defaults(&a);
        assert_eq!(v.icons_for(&dir("/b")), big);
        assert!(v.folders.is_empty(), "the folder follows the defaults now");
    }

    #[test]
    fn out_of_range_options_are_clamped() {
        let wild = IconOptions {
            icon_size: 5000.0,
            spacing: f32::NAN,
            ..Default::default()
        };
        let c = wild.clamped();
        assert_eq!(c.icon_size, MAX_ICON);
        assert_eq!(c.spacing, IconOptions::default().spacing);
        let tiny = IconOptions {
            icon_size: 1.0,
            spacing: -3.0,
            ..Default::default()
        };
        assert_eq!(tiny.clamped().icon_size, MIN_ICON);
        assert_eq!(tiny.clamped().spacing, 0.0);
    }
}
