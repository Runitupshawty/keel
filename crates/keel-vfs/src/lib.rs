//! Virtual filesystem providers and shared file operation types for Keel.

pub mod archive;
#[cfg(windows)]
pub mod clipboard;
#[cfg(feature = "cloud")]
pub mod cloud;
// --- Task 24 ---
#[cfg(windows)]
pub mod desktop;
pub mod drag_out;
// --- end Task 24 ---
pub mod library;
pub mod path;
/// Per-user named pipe security (single instance, Windows).
#[cfg(windows)]
pub mod pipe;
pub use path::VPath;
pub mod entry;
pub mod local;
pub mod provider;
pub mod router;
pub mod sftp;
/// Explorer Properties sheet (Windows).
#[cfg(windows)]
pub mod shell;
// --- Task 33 ---
pub mod volume;
pub use volume::{volume_info, VolumeInfo, VolumeType};
// --- end Task 33 ---
#[cfg(feature = "cloud")]
pub use cloud::{CloudAccount, CloudError, CloudKind, CloudProvider, S3Config, SecretStore};
pub use entry::{Entry, Kind};
pub use local::{drives, is_fixed_disk, long, user_mount, watch, LocalProvider};
pub use provider::{Caps, Provider, RemoveKind};
pub use router::Router;
pub use sftp::{ConnStatus, RemoteAuth, RemoteEvent, RemoteHost, SftpProvider};
pub mod ops;
#[cfg(not(windows))]
mod ops_unix;
#[cfg(not(windows))]
use ops_unix as sys;
#[cfg(windows)]
mod ops_windows;
#[cfg(feature = "zip")]
pub use ops::add_to_zip;
pub use ops::{copy_local, extract, extract_under, move_local, plan_size, Conflict, Progress};
#[cfg(windows)]
use ops_windows as sys;

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
