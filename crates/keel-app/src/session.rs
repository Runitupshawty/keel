//! Open tabs, restored on the next start: `<cache dir>/session.json` for the default
//! profile, `<cache dir>/profiles/<name>/session.json` for the others (`profiles`).

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

    /// The current profile's session file.
    pub fn path() -> Option<PathBuf> {
        crate::profiles::session_path(&crate::profiles::current())
    }

    /// The saved session (None when there is none) and, when session.json could not be
    /// read, a notice for a toast (the file is kept as `session.json.bad`).
    pub fn load() -> (Option<Session>, Option<String>) {
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => (None, None),
        }
    }

    pub fn load_from(path: &Path) -> (Option<Session>, Option<String>) {
        match crate::settings::read_config(path, |t| {
            serde_json::from_str(t).map_err(|e| e.to_string())
        }) {
            Ok(session) => (session, None),
            Err(notice) => (None, Some(notice)),
        }
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
        assert_eq!(
            Session::load_from(&file),
            (None, None),
            "no file, no notice"
        );
        saved.save_to(&file).unwrap();

        let (loaded, notice) = Session::load_from(&file);
        let mut loaded = loaded.expect("session loads");
        assert_eq!((&loaded, notice), (&saved, None));
        loaded.repair(&home);
        // A missing folder is not checked here (no disk access before the window opens).
        assert_eq!(loaded.panes, vec![vec![home.clone(), gone], vec![home]]);
        assert_eq!(loaded.active, 1);
        assert_eq!(loaded.active_tab, [1, 0], "out-of-range tab index clamped");

        // A BOM is tolerated; a broken file is set aside and reported.
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, format!("\u{feff}{text}")).unwrap();
        assert_eq!(Session::load_from(&file).0, Some(saved));
        std::fs::write(&file, "{ not json").unwrap();
        let (none, notice) = Session::load_from(&file);
        assert!(none.is_none(), "broken file = no session");
        assert!(notice.unwrap().contains("session.json.bad"));
        assert!(!file.exists());
        assert_eq!(
            std::fs::read_to_string(tmp.join("session.json.bad")).unwrap(),
            "{ not json"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
