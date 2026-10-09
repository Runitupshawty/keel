//! File clipboard: the system one (so Explorer / Finder / Nautilus paste works) with an
//! in-app copy as fallback. Windows: CF_HDROP via keel-vfs; macOS: NSPasteboard
//! `public.file-url` and Linux: `text/uri-list`, both via arboard.

use std::path::PathBuf;

#[cfg(windows)]
mod sys {
    use super::*;
    pub fn write_files(paths: &[PathBuf], cut: bool) -> anyhow::Result<()> {
        keel_vfs::clipboard::write_files(paths, cut)
    }
    pub fn read_files() -> Option<(Vec<PathBuf>, bool)> {
        keel_vfs::clipboard::read_files()
            .map_err(|e| tracing::warn!("read clipboard: {e:#}"))
            .ok()
            .flatten()
    }
    pub fn sequence() -> Option<u32> {
        Some(keel_vfs::clipboard::sequence())
    }
}

#[cfg(not(windows))]
mod sys {
    use super::*;
    // ponytail: arboard has no custom MIME types, so no `x-special/gnome-copied-files` cut
    // verb on Linux (and Finder has no cut); a cut made here is honoured in-app only.
    pub fn write_files(paths: &[PathBuf], _cut: bool) -> anyhow::Result<()> {
        let mut cb = arboard::Clipboard::new()?;
        if paths.is_empty() {
            cb.clear()?;
        } else {
            cb.set().file_list(paths)?;
        }
        Ok(())
    }
    pub fn read_files() -> Option<(Vec<PathBuf>, bool)> {
        let paths = arboard::Clipboard::new().ok()?.get().file_list().ok()?;
        (!paths.is_empty()).then_some((paths, false))
    }
    pub fn sequence() -> Option<u32> {
        None
    }
}

pub use sys::{read_files, write_files};

/// The in-app clipboard. `stamp` is the system clipboard sequence right after our write
/// (Windows), to tell whether another app has written since.
#[derive(Clone, Debug, Default)]
pub struct Clipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
    stamp: Option<u32>,
}

impl Clipboard {
    /// Copy/cut: in-app always; system best effort. Empty `paths` clears both.
    pub fn set(&mut self, paths: Vec<PathBuf>, cut: bool) {
        if let Err(e) = write_files(&paths, cut) {
            tracing::warn!("system clipboard: {e:#}; in-app only");
        }
        self.stamp = sys::sequence();
        self.paths = paths;
        self.cut = cut;
    }

    /// What Ctrl+V pastes. Reads the system clipboard, which may wait on its owner:
    /// call off the UI thread.
    pub fn resolve(&self) -> Option<(Vec<PathBuf>, bool)> {
        choose(self, read_files(), sys::sequence())
    }
}

/// The system clipboard wins when it changed after our copy (Windows: sequence number;
/// elsewhere: it holds other files). Otherwise the in-app copy, which remembers a cut.
fn choose(
    app: &Clipboard,
    system: Option<(Vec<PathBuf>, bool)>,
    seq: Option<u32>,
) -> Option<(Vec<PathBuf>, bool)> {
    let changed = match (seq, app.stamp) {
        (Some(now), Some(ours)) => now != ours,
        _ => system.as_ref().is_some_and(|(p, _)| *p != app.paths),
    };
    if changed || app.paths.is_empty() {
        system
    } else {
        Some((app.paths.clone(), app.cut))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_wins_only_when_changed() {
        let a = vec![PathBuf::from("/a")];
        let b = vec![PathBuf::from("/b")];
        let app = Clipboard {
            paths: a.clone(),
            cut: true,
            stamp: Some(7),
        };
        // Same sequence: ours, cut kept even though the system copy cannot say so.
        assert_eq!(
            choose(&app, Some((a.clone(), false)), Some(7)),
            Some((a.clone(), true))
        );
        // Another app wrote (even text only): system.
        assert_eq!(
            choose(&app, Some((b.clone(), true)), Some(8)),
            Some((b.clone(), true))
        );
        assert_eq!(choose(&app, None, Some(8)), None);
        // No sequence numbers: other files on the system clipboard win, nothing falls back.
        let app = Clipboard { stamp: None, ..app };
        assert_eq!(
            choose(&app, Some((b.clone(), false)), None),
            Some((b, false))
        );
        assert_eq!(choose(&app, None, None), Some((a.clone(), true)));
        assert_eq!(
            choose(&app, Some((a.clone(), false)), None),
            Some((a, true))
        );
    }

    /// Overwrites the real system clipboard, so it only runs on request:
    /// `cargo test -p keel-app -- --ignored round_trip`. Skipped where there is no display.
    #[test]
    #[ignore]
    fn round_trip_two_paths() {
        if cfg!(target_os = "linux")
            && std::env::var_os("DISPLAY").is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_none()
        {
            return;
        }
        let dir = std::env::temp_dir().join("keel-clip-test");
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<PathBuf> = ["one file.txt", "two.txt"]
            .iter()
            .map(|n| {
                let p = dir.join(n);
                std::fs::write(&p, n).unwrap();
                // macOS writes canonical file URLs.
                std::fs::canonicalize(&p).map_or(p, |c| {
                    PathBuf::from(c.to_string_lossy().trim_start_matches(r"\?\"))
                })
            })
            .collect();
        if let Err(e) = write_files(&paths, true) {
            eprintln!("no system clipboard here: {e:#}");
            return;
        }
        let (got, cut) = read_files().expect("files on the clipboard");
        assert_eq!(got, paths);
        assert_eq!(cut, cfg!(windows), "only Windows carries the cut flag");
        write_files(&[], false).unwrap();
        assert_eq!(read_files(), None);
    }
}
