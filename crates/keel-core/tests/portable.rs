//! Portable mode end to end: every folder resolver follows the variables
//! `keel_core::portable` sets. Its own test binary, so changing the environment here races
//! no other test.

use keel_core::portable::{apply_dirs, Dirs, DEFAULT_FOLDER};

#[test]
fn resolvers_follow_the_portable_folders() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(dir.path().join(DEFAULT_FOLDER));
    for var in ["KEEL_CONFIG_DIR", "KEEL_DATA_DIR", "KEEL_INDEX_DIR"] {
        std::env::remove_var(var);
    }
    apply_dirs(&dirs, |_| false, |var, path| std::env::set_var(var, path));
    // Libraries and crash reports.
    assert_eq!(keel_core::data_dir(), Some(dirs.data.clone()));
    // Archive cache, remote downloads, media cache.
    let cache = keel_vfs::cache_dir();
    assert!(cache.starts_with(&dirs.data), "{}", cache.display());
    // The NTFS and folder-walk indexes.
    assert_eq!(keel_search::walk_index_dir(), Some(dirs.index.clone()));
    // Settings, session, window state and profiles are under KEEL_CONFIG_DIR (keel-app's
    // and keel-api's `config_dir`; tested there).
    assert!(dirs.config.is_dir());
}
