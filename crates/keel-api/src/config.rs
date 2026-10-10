//! What a host (daemon or in-process CLI) reads from Keel's settings: the library name
//! and whether keel-net is on, from `<config dir>/profiles/<profile>/config.toml`
//! (`[library] name`, `[net] enabled`); the daemon's socket name and WebSocket token.

use std::path::PathBuf;

pub const DEFAULT_PROFILE: &str = "default";
pub const DEFAULT_LIBRARY: &str = "james";

/// `KEEL_CONFIG_DIR`, else `%APPDATA%\Keel`, `~/Library/Application Support/Keel`,
/// `~/.config/keel` (as keel-app).
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("KEEL_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(dir.into());
    }
    let base = directories::BaseDirs::new()?;
    let app = if cfg!(target_os = "linux") {
        "keel"
    } else {
        "Keel"
    };
    Some(base.config_dir().join(app))
}

/// What the host needs from one profile's settings.
#[derive(Clone, Debug, PartialEq)]
pub struct HostConfig {
    pub profile: String,
    pub config_dir: PathBuf,
    /// Libraries live under `<data_dir>/library/<name>/`.
    pub data_dir: PathBuf,
    pub library: String,
    pub net: bool,
}

impl HostConfig {
    /// From the environment (`KEEL_CONFIG_DIR`, `KEEL_DATA_DIR`) and the profile's
    /// config.toml; a missing or unreadable file means the defaults.
    pub fn load(profile: &str) -> anyhow::Result<Self> {
        let config_dir = config_dir().ok_or_else(|| anyhow::anyhow!("no configuration folder"))?;
        let data_dir = keel_core::data_dir().ok_or_else(|| anyhow::anyhow!("no data folder"))?;
        Ok(Self::read(profile, config_dir, data_dir))
    }

    pub fn read(profile: &str, config_dir: PathBuf, data_dir: PathBuf) -> Self {
        let path = config_dir
            .join("profiles")
            .join(profile)
            .join("config.toml");
        let table: toml::Table = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();
        let get = |section: &str, key: &str| table.get(section).and_then(|s| s.get(key)).cloned();
        let library = get("library", "name")
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_LIBRARY.to_owned());
        let net = get("net", "enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Self {
            profile: profile.to_owned(),
            config_dir,
            data_dir,
            library,
            net,
        }
    }

    /// The daemon's socket name: per user, profile and data folder.
    pub fn socket_name(&self) -> String {
        crate::socket::name(
            "keel-daemon",
            &format!("{}\0{}", self.profile, self.data_dir.display()),
        )
    }

    /// The WebSocket bearer token file.
    pub fn token_path(&self) -> PathBuf {
        self.config_dir.join("daemon.token")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_library_and_net() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = HostConfig::read("work", dir.path().into(), dir.path().join("data"));
        assert_eq!((cfg.library.as_str(), cfg.net), (DEFAULT_LIBRARY, false));
        let p = dir.path().join("profiles/work");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(
            p.join("config.toml"),
            "theme = \"dark\"\n[library]\nname = \"lab\"\n[net]\nenabled = true\n",
        )
        .unwrap();
        let cfg = HostConfig::read("work", dir.path().into(), dir.path().join("data"));
        assert_eq!((cfg.library.as_str(), cfg.net), ("lab", true));
        let other = HostConfig::read("work", dir.path().into(), dir.path().join("other"));
        assert_ne!(cfg.socket_name(), other.socket_name());
        assert!(cfg.socket_name().starts_with("keel-daemon-"));
    }
}
