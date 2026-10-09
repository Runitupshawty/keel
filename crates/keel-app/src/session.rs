//! Open tabs, restored on the next start (`<cache dir>/session.json`).

use crate::state::AppState;
use keel_vfs::VPath;
use std::path::{Path, PathBuf};

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct Session {
    /// Folder of every tab, per pane (a search tab is saved as the folder it started from).
    pub panes: Vec<Vec<VPath>>,
    /// The active pane.
    pub active: usize,
    /// The active tab of each pane.
    pub active_tab: [usize; 2],
}

impl Session {
    /// One tab on `dir` in both panes.
    pub fn single(dir: VPath) -> Self {
        Self {
            panes: vec![vec![dir.clone()], vec![dir]],
            active: 0,
            active_tab: [0, 0],
        }
    }

    pub fn of(state: &AppState) -> Self {
        Self {
            panes: state
                .panes
                .iter()
                .map(|p| p.tabs.iter().map(|t| t.dir.clone()).collect())
                .collect(),
            active: state.active,
            active_tab: [state.panes[0].active, state.panes[1].active],
        }
    }

    pub fn path() -> Option<PathBuf> {
        crate::settings::cache_dir().map(|d| d.join("session.json"))
    }

    pub fn load() -> Option<Session> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &Path) -> Option<Session> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text)
            .map_err(|e| tracing::warn!("{}: {e}", path.display()))
            .ok()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no cache folder"))?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        crate::settings::write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    /// Makes a loaded session usable: exactly two panes with at least one tab, indices in
    /// range. Never touches the disk: a saved folder that is gone is found by the worker
    /// that lists it (`AppState::listed`), so an offline share keeps its tabs.
    pub fn repair(&mut self, home: &VPath) {
        self.panes.resize_with(2, Vec::new);
        self.panes.truncate(2);
        for (p, tabs) in self.panes.iter_mut().enumerate() {
            if tabs.is_empty() {
                tabs.push(home.clone());
            }
            self.active_tab[p] = self.active_tab[p].min(tabs.len() - 1);
        }
        self.active = self.active.min(1);
    }
}

#[cfg(test)]
mod tests {
    use super::Session;
    use keel_vfs::VPath;

    #[test]
    fn load_repair_and_broken_file() {
        let tmp = std::env::temp_dir().join(format!("keel-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let home = VPath::local(&tmp);
        let gone = VPath::local(tmp.join("unplugged-usb"));
        let saved = Session {
            panes: vec![vec![home.clone(), gone.clone()], vec![]],
            active: 1,
            active_tab: [1, 7],
        };
        let file = tmp.join("session.json");
        saved.save_to(&file).unwrap();

        let mut loaded = Session::load_from(&file).expect("session loads");
        assert_eq!(loaded, saved);
        loaded.repair(&home);
        // A missing folder is not checked here (no disk access before the window opens).
        assert_eq!(loaded.panes, vec![vec![home.clone(), gone], vec![home]]);
        assert_eq!(loaded.active, 1);
        assert_eq!(loaded.active_tab, [1, 0], "out-of-range tab index clamped");

        std::fs::write(&file, "{ not json").unwrap();
        assert!(
            Session::load_from(&file).is_none(),
            "broken file = no session"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
