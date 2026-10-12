//! Phase 14: the sidebar's per-profile layout (`[sidebar]` in the profile's `config.toml`):
//! the user-arranged Favorites and which sections are hidden. Drawing is in `sidebar.rs`.

use keel_vfs::VPath;
use serde::{Deserialize, Deserializer, Serialize};

/// A well-known place, resolved on this computer when the sidebar is drawn (so a profile
/// copied to another computer points at that computer's folders).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Place {
    /// Files opened lately (the library's `is:recent`).
    Recents,
    /// Applications (macOS) or Programs (Windows); none on Linux.
    Applications,
    Desktop,
    Documents,
    Downloads,
    Home,
}

impl Place {
    pub const ALL: [Place; 6] = [
        Place::Recents,
        Place::Applications,
        Place::Desktop,
        Place::Documents,
        Place::Downloads,
        Place::Home,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Place::Recents => "Recents",
            Place::Applications if cfg!(windows) => "Programs",
            Place::Applications => "Applications",
            Place::Desktop => "Desktop",
            Place::Documents => "Documents",
            Place::Downloads => "Downloads",
            Place::Home => "Home",
        }
    }

    /// The folder on this computer; None for Recents (a library query) and for a place this
    /// computer does not have.
    pub fn resolve(self) -> Option<VPath> {
        let user = || directories::UserDirs::new();
        let path = match self {
            Place::Recents => None,
            Place::Applications => applications(),
            Place::Desktop => user().and_then(|u| u.desktop_dir().map(|d| d.to_owned())),
            Place::Documents => user().and_then(|u| u.document_dir().map(|d| d.to_owned())),
            Place::Downloads => user().and_then(|u| u.download_dir().map(|d| d.to_owned())),
            Place::Home => directories::BaseDirs::new().map(|b| b.home_dir().to_owned()),
        };
        path.map(VPath::local)
    }
}

fn applications() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        let dir = std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into());
        Some(dir.into())
    } else if cfg!(target_os = "macos") {
        Some("/Applications".into())
    } else {
        None
    }
}

/// One Favorites row.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Favorite {
    Place {
        place: Place,
    },
    /// A folder the user pinned; `name` empty = the folder's own name.
    Folder {
        folder: VPath,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        name: String,
    },
}

impl Favorite {
    pub fn folder(path: VPath) -> Self {
        Favorite::Folder {
            folder: path,
            name: String::new(),
        }
    }

    /// The row's text.
    pub fn label(&self) -> String {
        match self {
            Favorite::Place { place } => place.label().to_owned(),
            Favorite::Folder { folder, name } if name.is_empty() => match folder.name() {
                "" => folder.display(),
                n => n.to_owned(),
            },
            Favorite::Folder { name, .. } => name.clone(),
        }
    }

    /// The folder it opens, with places resolved by `places`.
    pub fn target(&self, places: &impl Fn(Place) -> Option<VPath>) -> Option<VPath> {
        match self {
            Favorite::Place { place } => places(*place),
            Favorite::Folder { folder, .. } => Some(folder.clone()),
        }
    }
}

/// The sidebar's sections, in order.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Section {
    Favorites,
    Locations,
    Library,
    Tags,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::Favorites => "Favorites",
            Section::Locations => "Locations",
            Section::Library => "Library",
            Section::Tags => "Tags",
        }
    }
}

/// `[sidebar]`: the profile's Favorites (in their order) and its hidden sections. Entries a
/// newer Keel wrote that this one does not know are skipped, never the whole file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SidebarSettings {
    #[serde(deserialize_with = "lenient")]
    pub favorites: Vec<Favorite>,
    /// Sections collapsed with their header's Hide.
    #[serde(deserialize_with = "lenient")]
    pub hidden: Vec<Section>,
}

impl Default for SidebarSettings {
    fn default() -> Self {
        Self {
            favorites: seeds(),
            hidden: Vec::new(),
        }
    }
}

/// A new profile's Favorites: Recents, Applications/Programs, Desktop, Documents,
/// Downloads and Home.
pub fn seeds() -> Vec<Favorite> {
    Place::ALL
        .into_iter()
        .map(|place| Favorite::Place { place })
        .collect()
}

/// The items of an array that parse; the others are left out (with a log line).
fn lenient<'de, D: Deserializer<'de>, T: serde::de::DeserializeOwned>(
    d: D,
) -> Result<Vec<T>, D::Error> {
    let items = Vec::<toml::Value>::deserialize(d)?;
    Ok(items
        .into_iter()
        .filter_map(|item| match item.try_into() {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("config.toml [sidebar]: skipped an entry: {e}");
                None
            }
        })
        .collect())
}

impl SidebarSettings {
    /// Where `path` already is in Favorites (a pinned folder, or a place resolving to it).
    pub fn position(
        &self,
        path: &VPath,
        places: &impl Fn(Place) -> Option<VPath>,
    ) -> Option<usize> {
        (self.favorites.iter()).position(|f| f.target(places).as_ref() == Some(path))
    }

    /// Pins `path` before row `at` (at the end when None or past it). A folder already
    /// there moves to `at` instead of being added twice. Returns its row.
    pub fn pin(
        &mut self,
        path: VPath,
        at: Option<usize>,
        places: &impl Fn(Place) -> Option<VPath>,
    ) -> usize {
        let len = self.favorites.len();
        match self.position(&path, places) {
            Some(i) => match at {
                Some(at) => self.move_to(i, at),
                None => i,
            },
            None => {
                let at = at.unwrap_or(len).min(len);
                self.favorites.insert(at, Favorite::folder(path));
                at
            }
        }
    }

    /// Removes row `i` (Remove from Sidebar); false when there is no such row.
    pub fn unpin(&mut self, i: usize) -> bool {
        if i < self.favorites.len() {
            self.favorites.remove(i);
            true
        } else {
            false
        }
    }

    /// Moves row `from` into the gap before row `to` (`to` = the row count: the end), as a
    /// drag between rows does. Returns the row it ends up at.
    pub fn move_to(&mut self, from: usize, to: usize) -> usize {
        let len = self.favorites.len();
        if from >= len {
            return from;
        }
        let to = to.min(len);
        let at = if to > from { to - 1 } else { to };
        let item = self.favorites.remove(from);
        self.favorites.insert(at, item);
        at
    }

    pub fn is_hidden(&self, s: Section) -> bool {
        self.hidden.contains(&s)
    }

    /// The header's Hide / Show.
    pub fn toggle(&mut self, s: Section) {
        if self.is_hidden(s) {
            self.hidden.retain(|h| *h != s);
        } else {
            self.hidden.push(s);
        }
    }
}

/// What the sidebar asks the state to do (`Action::Sidebar`).
#[derive(Clone, Debug, PartialEq)]
pub enum SidebarCmd {
    /// Items dragged out of pane `pane` and dropped between Favorites rows: its folders are
    /// pinned before row `at` (the end when None); files are left out.
    Pin {
        pane: usize,
        paths: Vec<VPath>,
        at: Option<usize>,
    },
    /// Add to Sidebar: the active pane's selected folders, or its folder when nothing is
    /// selected.
    PinSelection,
    /// Pins this folder at the end.
    PinFolder(VPath),
    /// Remove from Sidebar.
    Unpin(usize),
    /// A Favorites row dragged into the gap before row `to`.
    Move { from: usize, to: usize },
    /// A section header's Hide / Show.
    Toggle(Section),
    /// Favorites back to the seeds.
    ResetFavorites,
    /// Eject the removable drive `name` (`"E:"`, or its mount point).
    Eject(String),
}

/// The folders among `paths`, judged by `entries` (the listing they were dragged from):
/// files and unknown items are left out.
pub fn folders_in(entries: &[keel_vfs::Entry], paths: &[VPath]) -> Vec<VPath> {
    paths
        .iter()
        .filter(|p| (entries.iter()).any(|e| &e.path == *p && e.kind == keel_vfs::Kind::Dir))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "keel-sidebar-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Places resolve to folders under `root` (never this computer's real ones).
    fn places(root: &std::path::Path) -> impl Fn(Place) -> Option<VPath> + '_ {
        move |p: Place| match p {
            Place::Recents | Place::Applications => None,
            _ => Some(VPath::local(root.join(p.label()))),
        }
    }

    fn labels(s: &SidebarSettings) -> Vec<String> {
        s.favorites.iter().map(Favorite::label).collect()
    }

    #[test]
    fn a_new_profile_is_seeded_in_finder_order() {
        let s = SidebarSettings::default();
        let mut want = vec!["Recents", Place::Applications.label()];
        want.extend(["Desktop", "Documents", "Downloads", "Home"]);
        assert_eq!(labels(&s), want);
        assert!(s.hidden.is_empty());
    }

    #[test]
    fn pin_unpin_and_reorder() {
        let root = temp("ops");
        let places = places(&root);
        let mut s = SidebarSettings {
            favorites: vec![
                Favorite::Place {
                    place: Place::Recents,
                },
                Favorite::Place {
                    place: Place::Documents,
                },
            ],
            hidden: Vec::new(),
        };
        let work = VPath::local(root.join("Work"));
        let photos = VPath::local(root.join("Photos"));
        assert_eq!(s.pin(work.clone(), None, &places), 2, "at the end");
        assert_eq!(s.pin(photos.clone(), Some(1), &places), 1, "before row 1");
        assert_eq!(labels(&s), ["Recents", "Photos", "Documents", "Work"]);
        // Pinning again moves instead of adding a second row; a place counts too.
        assert_eq!(s.pin(work.clone(), Some(0), &places), 0);
        assert_eq!(labels(&s), ["Work", "Recents", "Photos", "Documents"]);
        let docs = VPath::local(root.join("Documents"));
        assert_eq!(s.pin(docs, None, &places), 3, "already there");
        assert_eq!(s.favorites.len(), 4);
        // Dragging down: into the gap before row `to`.
        assert_eq!(s.move_to(0, 3), 2);
        assert_eq!(labels(&s), ["Recents", "Photos", "Work", "Documents"]);
        assert_eq!(s.move_to(3, 0), 0);
        assert_eq!(labels(&s), ["Documents", "Recents", "Photos", "Work"]);
        assert_eq!(s.move_to(1, 99), 3, "past the end: last");
        assert_eq!(labels(&s), ["Documents", "Photos", "Work", "Recents"]);
        assert_eq!(s.move_to(9, 0), 9, "no such row");
        assert!(s.unpin(1));
        assert!(!s.unpin(9));
        assert_eq!(labels(&s), ["Documents", "Work", "Recents"]);
        assert_eq!(s.position(&photos, &places), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hide_and_show_sections() {
        let mut s = SidebarSettings::default();
        s.toggle(Section::Tags);
        s.toggle(Section::Locations);
        assert!(s.is_hidden(Section::Tags) && s.is_hidden(Section::Locations));
        s.toggle(Section::Tags);
        assert!(!s.is_hidden(Section::Tags));
        assert_eq!(s.hidden, [Section::Locations]);
    }

    /// Pins, removals, the order and hidden sections survive a save and a load of the
    /// profile's config.toml; an empty list stays empty (not seeded again).
    #[test]
    fn the_layout_persists_in_the_profile() {
        let root = temp("persist");
        let path = root.join("config.toml");
        let places = places(&root);
        let mut settings = Settings::default();
        let s = &mut settings.sidebar;
        s.unpin(1);
        s.pin(VPath::local(root.join("Work")), Some(0), &places);
        s.pin(VPath::parse("sftp://nas/srv/media").unwrap(), None, &places);
        s.favorites.push(Favorite::Folder {
            folder: VPath::local(root.join("Photos")),
            name: "Holiday".into(),
        });
        s.move_to(0, 3);
        s.toggle(Section::Library);
        settings.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[sidebar]") || text.contains("[[sidebar.favorites]]"));
        let load = || crate::settings::parse_lenient(&std::fs::read_to_string(&path).unwrap());
        let (loaded, dropped) = load().unwrap();
        assert!(dropped.is_empty());
        assert_eq!(loaded.sidebar, settings.sidebar);
        assert_eq!(
            labels(&loaded.sidebar)[..4],
            ["Recents", "Desktop", "Work", "Documents"]
        );
        assert!(labels(&loaded.sidebar).contains(&"Holiday".to_owned()));
        assert!(loaded.sidebar.is_hidden(Section::Library));
        // Everything removed: stays empty.
        settings.sidebar.favorites.clear();
        settings.save_to(&path).unwrap();
        assert!(load().unwrap().0.sidebar.favorites.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An entry this Keel does not know (a newer place, a broken folder) is skipped; the
    /// rest of the sidebar and of the profile still load.
    #[test]
    fn unknown_entries_are_skipped() {
        let text = "theme = \"light\"\n[sidebar]\nhidden = [\"tags\", \"widgets\"]\n\
                    [[sidebar.favorites]]\nplace = \"home\"\n\
                    [[sidebar.favorites]]\nplace = \"airdrop\"\n\
                    [[sidebar.favorites]]\nfolder = 3\n\
                    [[sidebar.favorites]]\nplace = \"desktop\"\n";
        let (s, dropped) = crate::settings::parse_lenient(text).unwrap();
        assert!(dropped.is_empty());
        assert_eq!(s.theme, "light");
        assert_eq!(labels(&s.sidebar), ["Home", "Desktop"]);
        assert_eq!(s.sidebar.hidden, [Section::Tags]);
        // No [sidebar] at all: the seeds.
        let (s, _) = crate::settings::parse_lenient("theme = \"light\"\n").unwrap();
        assert_eq!(s.sidebar, SidebarSettings::default());
    }

    #[test]
    fn only_folders_are_pinned_from_a_drop() {
        let dir = VPath::parse("mem://t/").unwrap();
        let a = crate::tab::test_entry(&dir, "a", keel_vfs::Kind::Dir, 0);
        let f = crate::tab::test_entry(&dir, "f.txt", keel_vfs::Kind::File, 5);
        let elsewhere = VPath::parse("mem://t/zz").unwrap();
        let picked = folders_in(
            &[a.clone(), f.clone()],
            &[f.path.clone(), a.path.clone(), elsewhere],
        );
        assert_eq!(picked, [a.path]);
    }
}
