//! Dragging files out of Keel into other apps (spec 2.5, Task 24; macOS and Linux Task 55).
//!
//! Windows: an OLE `DoDragDrop` with an `IDataObject` carrying `CF_HDROP` and
//! `Preferred DropEffect`, run on its own STA thread so the UI never blocks.
//! macOS: an AppKit dragging session on Keel's window with one file URL per item.
//! Linux: XDND on X11 (or XWayland) from a thread with its own X connection; a Wayland
//! window reports that it cannot at once.
//!
//! Keel never moves or deletes a file here: a target that moves the files does so itself,
//! and its "move" only counts once the sources are really gone (`confirmed`).

#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod win;
#[cfg(any(target_os = "linux", test))]
pub mod x11;

#[cfg(target_os = "macos")]
use mac as imp;
#[cfg(windows)]
use win as imp;
#[cfg(windows)]
pub use win::{dropfiles, run};
#[cfg(target_os = "linux")]
use x11 as imp;

use std::path::{Path, PathBuf};

/// Whether this session can drag files out to other apps: Windows, macOS, and Linux on X11
/// or XWayland (a native Wayland window cannot, so a row drag stays inside Keel).
pub fn supported() -> bool {
    cfg!(any(windows, target_os = "macos")) || (cfg!(target_os = "linux") && !wayland())
}

/// winit opens a native Wayland window exactly when one of these is set (and not empty).
pub(crate) fn wayland() -> bool {
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    set("WAYLAND_DISPLAY") || set("WAYLAND_SOCKET")
}

/// What the receiving app did with the files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropEffect {
    /// Cancelled, refused, or an optimized move the target reported as nothing.
    None,
    Copy,
    /// The target moved the files (it did the move; Keel only refreshes).
    Move,
}

/// Starts an OS drag of local `paths` from the UI thread (call it there, while the
/// primary button that started the drag is held) and returns at once; `done` runs, on
/// the drag thread (Windows, Linux) or the main thread (macOS), with what the target did.
/// `allow_move` lets the target move the files (with the platform's move modifier held).
pub fn start(
    paths: Vec<PathBuf>,
    allow_move: bool,
    done: impl FnOnce(anyhow::Result<DropEffect>) + Send + 'static,
) {
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    {
        let sources = paths.clone();
        imp::start(paths, allow_move, move |r| {
            done(r.map(|effect| confirmed(effect, &sources)))
        });
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (paths, allow_move);
        done(Err(anyhow::anyhow!(
            "dragging files to other apps is not supported on this OS yet"
        )));
    }
}

/// A target's move is believed only when every source is gone: with any one still there,
/// it was not a move (and Keel never removes them itself).
pub fn confirmed(effect: DropEffect, sources: &[PathBuf]) -> DropEffect {
    let there = |p: &PathBuf| Path::symlink_metadata(p).is_ok();
    if effect == DropEffect::Move && sources.iter().any(there) {
        DropEffect::None
    } else {
        effect
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_counts_only_once_the_sources_are_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let both = [a.clone(), b.clone()];
        assert_eq!(confirmed(DropEffect::Move, &both), DropEffect::None);
        assert_eq!(confirmed(DropEffect::Copy, &both), DropEffect::Copy);
        std::fs::remove_file(&a).unwrap();
        assert_eq!(
            confirmed(DropEffect::Move, &both),
            DropEffect::None,
            "one source is still there"
        );
        assert!(b.exists(), "nothing is removed by the check");
        std::fs::remove_file(&b).unwrap();
        assert_eq!(confirmed(DropEffect::Move, &both), DropEffect::Move);
    }
}
