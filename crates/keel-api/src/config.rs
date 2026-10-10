//! What a host (daemon or in-process CLI) reads from Keel's settings: the library name,
//! whether keel-net is on and the SFTP hosts and cloud accounts, from
//! `<config dir>/profiles/<profile>/config.toml` (`[library] name`, `[devices] enabled`,
//! `[[remotes]]`, `[[clouds]]`); the daemon's socket name and WebSocket token.

use std::path::PathBuf;

pub const DEFAULT_PROFILE: &str = "default";
pub const DEFAULT_LIBRARY: &str = "james";

/// What [`valid_profile`] accepts (keel-app's profile rule).
pub const PROFILE_RULE: &str = "use letters, digits, '.', '_' or '-' (at most 64)";

/// Profile names become folder names: ASCII letters, digits, `.`, `_`, `-`; not `.`/`..`,
/// no trailing dot and no Windows device name (`con`, `nul`, `com1`, …). The same rule as
/// keel-app's `profiles::valid_name`.
pub fn valid_profile(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_lowercase();
    let device = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.as_bytes()[3].is_ascii_digit());
    !name.is_empty()
        && name.len() <= 64
        && !name.ends_with('.')
        && !device
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

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
    /// `[[remotes]]`: SFTP hosts (secrets stay in the OS keychain).
    pub remotes: Vec<keel_vfs::RemoteHost>,
    /// `[[clouds]]`: cloud accounts (non-secret fields only).
    pub clouds: Vec<keel_vfs::CloudAccount>,
}

/// The entries of the array `key` that parse as `T` (a broken one is skipped).
fn entries<T: serde::de::DeserializeOwned>(table: &toml::Table, key: &str) -> Vec<T> {
    let items = table.get(key).and_then(toml::Value::as_array);
    items
        .into_iter()
        .flatten()
        .filter_map(|v| match v.clone().try_into() {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("config.toml [[{key}]]: {e}");
                None
            }
        })
        .collect()
}

impl HostConfig {
    /// From the environment (`KEEL_CONFIG_DIR`, `KEEL_DATA_DIR`) and the profile's
    /// config.toml; a missing or unreadable file means the defaults. An invalid profile
    /// name (`valid_profile`) is refused.
    pub fn load(profile: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            valid_profile(profile),
            "profile \"{profile}\": {PROFILE_RULE}"
        );
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
        // One switch with the app: `[devices] enabled`, counted once it is the user's
        // (`explicit`, which the Settings switch writes; a value saved while Devices
        // defaulted on is not). `[net] enabled` (0.8 previews) is read when it is not.
        let explicit = get("devices", "explicit").and_then(|v| v.as_bool()) == Some(true);
        let net = get("devices", "enabled")
            .and_then(|v| v.as_bool())
            .filter(|_| explicit)
            .or_else(|| get("net", "enabled").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        Self {
            profile: profile.to_owned(),
            config_dir,
            data_dir,
            library,
            net,
            remotes: entries(&table, "remotes"),
            clouds: entries(&table, "clouds"),
        }
    }

    /// The daemon's socket name: per user, profile and data folder, salted from the
    /// config folder (`socket::salted_name`).
    pub fn socket_name(&self) -> String {
        crate::socket::salted_name(
            "keel-daemon",
            &format!("{}\0{}", self.profile, self.data_dir.display()),
            &self.config_dir,
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
    fn profile_names_are_checked() {
        for bad in [
            "../x", "a/b", "a\\b", "..", ".", "", "x.", "con", "COM1.txt", "a b",
        ] {
            assert!(!valid_profile(bad), "{bad:?}");
            assert!(HostConfig::load(bad).is_err(), "{bad:?}");
        }
        for good in ["default", "work", "my.profile-2_x", "console"] {
            assert!(valid_profile(good), "{good:?}");
        }
        assert!(!valid_profile(&"x".repeat(65)));
    }

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
        // `[devices] enabled` (the app's switch) wins once it is explicit.
        std::fs::write(
            p.join("config.toml"),
            "[devices]\nenabled = false\nexplicit = true\n[net]\nenabled = true\n",
        )
        .unwrap();
        let off = HostConfig::read("work", dir.path().into(), dir.path().join("data"));
        assert!(!off.net);
        std::fs::write(
            p.join("config.toml"),
            "[devices]\nenabled = true\nexplicit = true\n",
        )
        .unwrap();
        assert!(HostConfig::read("work", dir.path().into(), dir.path().join("data")).net);
        // Saved while Devices defaulted on (not explicit): off.
        std::fs::write(p.join("config.toml"), "[devices]\nenabled = true\n").unwrap();
        assert!(!HostConfig::read("work", dir.path().into(), dir.path().join("data")).net);
        let other = HostConfig::read("work", dir.path().into(), dir.path().join("other"));
        assert_ne!(cfg.socket_name(), other.socket_name());
        assert!(cfg.socket_name().starts_with("keel-daemon-"));
    }
}
