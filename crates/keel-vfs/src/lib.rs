//! Virtual filesystem providers and shared file operation types for Keel.

pub mod path;
pub use path::VPath;
pub mod entry;
pub mod local;
pub mod provider;
pub mod router;
pub use entry::{Entry, Kind};
pub use local::{drives, watch, LocalProvider};
pub use provider::{Caps, Provider};
pub use router::Router;
pub mod ops;
pub use ops::{copy_local, move_local, plan_size, Conflict, Progress};

/// Returns this crate's package name.
pub fn crate_name() -> &'static str {
    "keel-vfs"
}

#[cfg(test)]
mod tests {
    use super::crate_name;

    #[test]
    fn reports_crate_name() {
        assert_eq!(crate_name(), "keel-vfs");
    }
}
