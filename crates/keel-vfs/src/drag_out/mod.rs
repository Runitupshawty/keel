//! Dragging files out of Keel into other apps (spec 2.5, Task 24).
//!
//! Windows: an OLE `DoDragDrop` with an `IDataObject` carrying `CF_HDROP` and
//! `Preferred DropEffect`, run on its own STA thread so the UI never blocks.
//! macOS and Linux: not supported yet (`SUPPORTED` is false); rows dragged out of the
//! window simply stay an in-app drag there.

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{dropfiles, run, start};

/// Whether this OS can drag files out to other apps.
pub const SUPPORTED: bool = cfg!(windows);

/// What the receiving app did with the files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropEffect {
    /// Cancelled, refused, or an optimized move the target reported as nothing.
    None,
    Copy,
    /// The target moved the files (it did the move; Keel only refreshes).
    Move,
}

/// Unsupported here: reports the error at once.
#[cfg(not(windows))]
pub fn start(
    _paths: Vec<std::path::PathBuf>,
    _allow_move: bool,
    done: impl FnOnce(anyhow::Result<DropEffect>) + Send + 'static,
) {
    done(Err(anyhow::anyhow!(
        "dragging files to other apps is not supported on this OS yet"
    )));
}
