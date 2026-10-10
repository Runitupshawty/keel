//! File clipboard: the system one (so Explorer / Finder / Nautilus paste works) with an
//! in-app copy as fallback. Windows: CF_HDROP via keel-vfs; Linux with an X display (or
//! XWayland): `x-special/gnome-copied-files`, `text/uri-list` and the KDE cut flag via
//! `x11_clipboard`; macOS (and Linux without a display): NSPasteboard `public.file-url` or
//! `text/uri-list` via arboard.

use crate::state::Msg;
use crossbeam_channel::Sender;
use keel_vfs::VPath;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    pub fn write_files(paths: &[PathBuf], cut: bool) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        if crate::x11_clipboard::available() {
            return crate::x11_clipboard::write(paths, cut);
        }
        // Finder has no cut on the pasteboard: a cut made here is honoured in-app only.
        let _ = cut;
        let mut cb = arboard::Clipboard::new()?;
        if paths.is_empty() {
            cb.clear()?;
        } else {
            cb.set().file_list(paths)?;
        }
        Ok(())
    }
    pub fn read_files() -> Option<(Vec<PathBuf>, bool)> {
        #[cfg(target_os = "linux")]
        if crate::x11_clipboard::available() {
            return crate::x11_clipboard::read()
                .map_err(|e| tracing::warn!("read clipboard: {e:#}"))
                .ok()
                .flatten();
        }
        let paths = arboard::Clipboard::new().ok()?.get().file_list().ok()?;
        (!paths.is_empty()).then_some((paths, false))
    }
    pub fn sequence() -> Option<u32> {
        None
    }
}

pub use sys::{read_files, write_files};

/// Tests that use the real system clipboard take this, so they do not overwrite each other.
#[cfg(test)]
pub(crate) static SYSTEM_CLIPBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where our last system clipboard write stands.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Stamp {
    /// Queued on the writer thread.
    Writing,
    /// Written (or failed); the system clipboard sequence right after (Windows).
    #[default]
    Unknown,
    Done(u32),
}

type WriteFn = fn(&[PathBuf], bool) -> anyhow::Result<()>;
type WriteJob = (Vec<PathBuf>, bool, Arc<Mutex<Stamp>>);

/// The in-app clipboard. `stamp` tells whether another app has written the system
/// clipboard since our write. System writes run on one writer thread (another app may
/// hold the clipboard open for a while; Windows retries), in order, with a toast when
/// one fails.
#[derive(Clone, Debug, Default)]
pub struct Clipboard {
    pub paths: Vec<PathBuf>,
    /// Remote (or in-archive) entries: in-app only, the system clipboard is cleared.
    pub remote: Vec<VPath>,
    pub cut: bool,
    stamp: Arc<Mutex<Stamp>>,
    /// None (tests): writes happen inline.
    writer: Option<Sender<WriteJob>>,
}

/// Text of the toast for a failed system clipboard write.
pub const WRITE_FAILED: &str = "Could not put the files on the system clipboard";

impl Clipboard {
    /// A clipboard whose system writes run on a worker thread.
    pub fn with_writer(tx: Sender<Msg>, ctx: egui::Context) -> Self {
        Self::with_writer_using(tx, ctx, write_files)
    }

    fn with_writer_using(tx: Sender<Msg>, ctx: egui::Context, write: WriteFn) -> Self {
        let (jobs, rx) = crossbeam_channel::unbounded::<WriteJob>();
        crate::worker::spawn("keel-clipboard", move || {
            while let Ok(first) = rx.recv() {
                // Only the newest write matters; older stamps are settled unwritten.
                let mut job = first;
                for newer in rx.try_iter() {
                    *job.2.lock() = Stamp::Unknown;
                    job = newer;
                }
                let (paths, cut, stamp) = job;
                if let Err(e) = write(&paths, cut) {
                    tracing::warn!("system clipboard: {e:#}; in-app only");
                    let text = format!("{WRITE_FAILED} ({e:#}); paste works inside Keel only");
                    crate::worker::send(&tx, &ctx, Msg::Toast(text));
                }
                *stamp.lock() = sys::sequence().map_or(Stamp::Unknown, Stamp::Done);
            }
        });
        Self {
            writer: Some(jobs),
            ..Self::default()
        }
    }

    /// Copy/cut: in-app always; system best effort. Empty `paths` clears both.
    pub fn set(&mut self, paths: Vec<PathBuf>, cut: bool) {
        self.stamp = Arc::new(Mutex::new(Stamp::Writing));
        let job = (paths.clone(), cut, self.stamp.clone());
        if !self.writer.as_ref().is_some_and(|w| w.send(job).is_ok()) {
            if let Err(e) = write_files(&paths, cut) {
                tracing::warn!("system clipboard: {e:#}; in-app only");
            }
            *self.stamp.lock() = sys::sequence().map_or(Stamp::Unknown, Stamp::Done);
        }
        self.paths = paths;
        self.remote.clear();
        self.cut = cut;
    }

    /// Our write's sequence number; waits (up to 2 s) for a queued write. Workers only.
    fn settled_stamp(&self) -> Option<u32> {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            match *self.stamp.lock() {
                Stamp::Done(s) => return Some(s),
                Stamp::Unknown => return None,
                Stamp::Writing if Instant::now() >= until => return None,
                Stamp::Writing => {}
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Copy/cut of any entries: local ones as above, otherwise in-app only.
    pub fn set_paths(&mut self, paths: Vec<VPath>, cut: bool) {
        match paths.iter().map(VPath::to_local_path).collect() {
            Some(local) => self.set(local, cut),
            None => {
                self.set(Vec::new(), cut);
                self.remote = paths;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.remote.is_empty()
    }

    /// Another app wrote the system clipboard after our last write (Windows only). A
    /// write still queued is ours.
    pub fn changed_outside(&self) -> bool {
        let ours = match *self.stamp.lock() {
            Stamp::Writing => return false,
            Stamp::Done(s) => Some(s),
            Stamp::Unknown => None,
        };
        sys::sequence().is_some_and(|s| Some(s) != ours)
    }

    /// What Ctrl+V pastes. Reads the system clipboard, which may wait on its owner:
    /// call off the UI thread.
    pub fn resolve(&self) -> Option<(Vec<VPath>, bool)> {
        let stamp = self.settled_stamp();
        let (system, seq) = (read_files(), sys::sequence());
        if !self.remote.is_empty() {
            // Ours unless another app wrote the clipboard since (we left it empty).
            let changed = match (seq, stamp) {
                (Some(now), Some(ours)) => now != ours,
                _ => system.is_some(),
            };
            if !changed {
                return Some((self.remote.clone(), self.cut));
            }
        }
        choose(self, stamp, system, seq)
            .map(|(p, cut)| (p.into_iter().map(VPath::local).collect(), cut))
    }
}

/// Same files, compared canonically (macOS hands back `/private/var/...` for `/var/...`,
/// and resolved symlinks); a path that cannot be resolved compares as written.
fn same_files(a: &[PathBuf], b: &[PathBuf]) -> bool {
    let canon = |p: &PathBuf| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y || canon(x) == canon(y))
}

/// The system clipboard wins when it changed after our copy (Windows: sequence number;
/// elsewhere: it holds other files). Otherwise the in-app copy, which remembers a cut.
fn choose(
    app: &Clipboard,
    stamp: Option<u32>,
    system: Option<(Vec<PathBuf>, bool)>,
    seq: Option<u32>,
) -> Option<(Vec<PathBuf>, bool)> {
    let changed = match (seq, stamp) {
        (Some(now), Some(ours)) => now != ours,
        _ => system
            .as_ref()
            .is_some_and(|(p, _)| !same_files(p, &app.paths)),
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
            ..Clipboard::default()
        };
        // Same sequence: ours, cut kept even though the system copy cannot say so.
        assert_eq!(
            choose(&app, Some(7), Some((a.clone(), false)), Some(7)),
            Some((a.clone(), true))
        );
        // Another app wrote (even text only): system.
        assert_eq!(
            choose(&app, Some(7), Some((b.clone(), true)), Some(8)),
            Some((b.clone(), true))
        );
        assert_eq!(choose(&app, Some(7), None, Some(8)), None);
        // No sequence numbers: other files on the system clipboard win, nothing falls back.
        assert_eq!(
            choose(&app, None, Some((b.clone(), false)), None),
            Some((b, false))
        );
        assert_eq!(choose(&app, None, None, None), Some((a.clone(), true)));
        assert_eq!(
            choose(&app, None, Some((a.clone(), false)), None),
            Some((a, true))
        );
    }

    /// Polish backlog (macOS): the system hands back canonical paths; still our cut.
    #[test]
    fn system_paths_compare_canonically() {
        let dir = std::env::temp_dir().join(format!("keel-clip-canon-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        let ours = vec![dir.join("sub").join("..").join("f.txt")];
        let system = vec![std::fs::canonicalize(dir.join("f.txt")).unwrap()];
        let app = Clipboard {
            paths: ours.clone(),
            cut: true,
            ..Clipboard::default()
        };
        assert_eq!(
            choose(&app, None, Some((system, false)), None),
            Some((ours, true))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Polish backlog: system writes run on a worker; a failure toasts, and a queued
    /// write never looks like another app's.
    #[test]
    fn failed_background_write_toasts_and_keeps_the_in_app_copy() {
        fn fail(_: &[PathBuf], _: bool) -> anyhow::Result<()> {
            std::thread::sleep(Duration::from_millis(100));
            anyhow::bail!("clipboard is busy")
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut clip = Clipboard::with_writer_using(tx, egui::Context::default(), fail);
        clip.set(vec![PathBuf::from("/a")], true);
        assert!(!clip.changed_outside(), "our write is still queued");
        match rx.recv_timeout(Duration::from_secs(5)).expect("toast") {
            Msg::Toast(text) => assert!(text.starts_with(WRITE_FAILED), "{text}"),
            _ => panic!("unexpected message"),
        }
        assert_eq!((clip.paths.len(), clip.cut), (1, true));
        let until = Instant::now() + Duration::from_secs(2);
        while *clip.stamp.lock() == Stamp::Writing && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(*clip.stamp.lock(), Stamp::Writing);
    }

    /// Overwrites the real system clipboard, so it only runs on request:
    /// `cargo test -p keel-app -- --ignored round_trip`. Skipped where there is no display.
    #[test]
    #[ignore]
    fn round_trip_two_paths() {
        let _only = SYSTEM_CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
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
        // The user's own file clipboard comes back afterwards.
        let saved = read_files();
        if let Err(e) = write_files(&paths, true) {
            eprintln!("no system clipboard here: {e:#}");
            return;
        }
        let (got, cut) = read_files().expect("files on the clipboard");
        write_files(&[], false).unwrap();
        let cleared = read_files();
        if let Some((paths, cut)) = saved {
            let _ = write_files(&paths, cut);
        }
        assert_eq!(got, paths);
        assert_eq!(cut, !cfg!(target_os = "macos"), "macOS has no cut flag");
        assert_eq!(cleared, None);
    }
}
