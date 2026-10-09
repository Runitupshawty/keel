//! PTY sessions and shell discovery. Call discovery and session I/O off the UI thread.
mod grid;
mod session;
mod shells;
pub use session::Session;
pub use shells::{available_shells, wslpath, Shell, ShellKind};
