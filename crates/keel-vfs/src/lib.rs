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
pub mod trashbin;
// --- Task 33 ---
pub mod volume;
pub use volume::{volume_info, VolumeInfo, VolumeType};
// --- end Task 33 ---
#[cfg(feature = "cloud")]
pub use cloud::{
    CloudAccount, CloudError, CloudKind, CloudProvider, S3Config, SecretStore, WebDavConfig,
};
pub use entry::{Entry, Kind};
pub use local::{drives, is_fixed_disk, long, user_mount, watch, LocalProvider};
pub use provider::{Caps, Provider, Quota, RemoveKind, ShareLink};
pub use router::Router;
pub use sftp::{ConnStatus, RemoteAuth, RemoteEvent, RemoteHost, SftpProvider};
pub use trashbin::TrashProvider;
pub mod ops;
#[cfg(not(windows))]
mod ops_unix;
#[cfg(not(windows))]
use ops_unix as sys;
#[cfg(windows)]
mod ops_windows;
#[cfg(feature = "zip")]
pub use ops::{add_to_archive, add_to_zip};
pub use ops::{copy_local, extract, extract_under, move_local, plan_size, Conflict, Progress};
#[cfg(windows)]
use ops_windows as sys;

/// Returns this crate's package name.
pub fn crate_name() -> &'static str {
    "keel-vfs"
}

/// The root of every cache and temp folder Keel keeps (archive extracts, RAR temp,
/// remote downloads, the media cache): `<KEEL_DATA_DIR>/cache`, else
/// `<KEEL_CONFIG_DIR>/cache`, else (unit tests and any test binary) a temp folder per
/// process, else `%LOCALAPPDATA%\Keel`, `~/Library/Caches/Keel`, `~/.cache/keel`.
pub fn cache_dir() -> std::path::PathBuf {
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    };
    if let Some(dir) = var("KEEL_DATA_DIR").or_else(|| var("KEEL_CONFIG_DIR")) {
        return dir.join("cache");
    }
    if cfg!(test) || in_test_binary() {
        return std::env::temp_dir().join(format!("keel-test-cache-{}", std::process::id()));
    }
    let app = if cfg!(target_os = "linux") {
        "keel"
    } else {
        "Keel"
    };
    directories::BaseDirs::new()
        .map(|d| d.cache_dir().join(app))
        .unwrap_or_else(|| std::env::temp_dir().join(app))
}

/// Cargo puts test binaries (any crate's) in `target/<profile>/deps`.
fn in_test_binary() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.file_name().map(|d| d == "deps"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::crate_name;

    #[test]
    fn reports_crate_name() {
        assert_eq!(crate_name(), "keel-vfs");
    }

    /// Every folder (with its modified time) a few levels into Keel's real per-user
    /// folders; None when there are none (nothing to protect on this machine).
    pub(crate) fn real_dirs() -> Option<Vec<(std::path::PathBuf, std::time::SystemTime)>> {
        let base = directories::BaseDirs::new()?;
        let app = if cfg!(target_os = "linux") {
            "keel"
        } else {
            "Keel"
        };
        let roots: Vec<_> = [base.data_local_dir(), base.config_dir(), base.cache_dir()]
            .iter()
            .map(|d| d.join(app))
            .filter(|d| d.is_dir())
            .collect();
        fn walk(
            dir: &std::path::Path,
            depth: u32,
            out: &mut Vec<(std::path::PathBuf, std::time::SystemTime)>,
        ) {
            if let Ok(m) = std::fs::metadata(dir).and_then(|m| m.modified()) {
                out.push((dir.to_owned(), m));
            }
            if depth == 0 {
                return;
            }
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    walk(&e.path(), depth - 1, out);
                }
            }
        }
        if roots.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for r in &roots {
            walk(r, 3, &mut out);
        }
        out.sort();
        out.dedup();
        Some(out)
    }

    /// The archive cache, RAR temp and remote downloads land under `cache_dir`, never in
    /// the user's real `%LOCALAPPDATA%\Keel` / `%APPDATA%\Keel` (their folders' times are
    /// unchanged around it).
    #[test]
    fn caches_and_temp_folders_stay_out_of_the_real_folders() {
        use crate::Provider;
        let before = real_dirs();
        let cache = super::cache_dir();
        assert!(
            !cache.starts_with(
                directories::BaseDirs::new()
                    .unwrap()
                    .data_local_dir()
                    .join("Keel")
            ),
            "{}",
            cache.display()
        );
        assert_eq!(
            crate::archive::cache::default_root(),
            cache.join("archives")
        );
        drop(crate::archive::cache::MaterialiseCache::default());
        #[cfg(feature = "rar")]
        {
            let rar = crate::archive::rar::temp_root().unwrap();
            assert!(rar.path().starts_with(&cache), "{}", rar.path().display());
        }
        #[cfg(feature = "cloud")]
        {
            let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
            let account = crate::CloudAccount {
                id: "dirs".into(),
                label: "Dirs".into(),
                kind: crate::CloudKind::S3,
                root: None,
                client_id_override: None,
                s3: None,
                webdav: None,
            };
            let cloud =
                crate::CloudProvider::with_operator(account, op, crossbeam_channel::unbounded().0)
                    .unwrap();
            let p = crate::VPath::parse("cloud://dirs/a.txt").unwrap();
            let mut w = cloud.write(&p).unwrap();
            std::io::Write::write_all(&mut w, b"a").unwrap();
            std::io::Write::flush(&mut w).unwrap();
            drop(w);
            let copy = cloud.local_copy(&p).unwrap();
            assert!(copy.starts_with(cache.join("remote")), "{}", copy.display());
        }
        if let Some(before) = before {
            assert_eq!(real_dirs(), Some(before), "the real Keel folders changed");
        }
    }
}
