use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use ssh_key::known_hosts::{Entry, HostPatterns, KnownHosts, Marker};
pub use ssh_key::PublicKey;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyVerdict {
    Known,
    Unknown(String),
    Mismatch,
}

/// Tests point known_hosts at a temp file (hold `TEST_LOCK` while it is set).
#[cfg(test)]
pub(super) static TEST_FILE: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);
#[cfg(test)]
pub(super) static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn location() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_FILE.lock().clone() {
        return Ok(path);
    }
    Ok(dirs::home_dir()
        .context("home directory unavailable")?
        .join(".ssh/known_hosts"))
}

/// `Mismatch` when the host has a known key of the same algorithm that differs, or the key
/// is `@revoked`, or the file exists but cannot be read (fails closed). A host whose known
/// keys are all of other algorithms is `Unknown` here; the connection itself only offers
/// the known algorithms (see `known_key_types`), so a server cannot downgrade into a prompt.
pub fn known_hosts_check(host: &str, port: u16, key: &PublicKey) -> HostKeyVerdict {
    location()
        .map(|p| check_at(&p, host, port, key))
        .unwrap_or(HostKeyVerdict::Mismatch)
}

/// Key types (`ssh-ed25519`, `ssh-rsa`, `ecdsa-sha2-nistp256`, ...) of the plain (not
/// `@cert-authority` / `@revoked`) known_hosts lines for `host:port`; empty if none.
pub fn known_key_types(host: &str, port: u16) -> Vec<String> {
    let Ok(input) = location().and_then(|p| Ok(fs::read_to_string(p)?)) else {
        return Vec::new();
    };
    let mut types = Vec::new();
    for entry in applicable(&input, host, port) {
        let kind = entry.public_key().algorithm().as_str().to_owned();
        if entry.marker().is_none() && !types.contains(&kind) {
            types.push(kind);
        }
    }
    types
}

fn glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut i, mut j, mut star, mut retry) = (0, 0, None, 0);
    while j < t.len() {
        if i < p.len() && (p[i] == b'?' || p[i].eq_ignore_ascii_case(&t[j])) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            i += 1;
            retry = j;
        } else if let Some(s) = star {
            retry += 1;
            j = retry;
            i = s + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}

/// Parsed lines whose host patterns match `host:port`.
fn applicable<'a>(input: &'a str, host: &str, port: u16) -> impl Iterator<Item = Entry> + 'a {
    let name = if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    };
    // Like OpenSSH: a line we cannot parse (unknown key type, typo) is skipped. It can
    // never make an unknown key trusted; at worst it turns a mismatch into a prompt.
    KnownHosts::new(input)
        .flatten()
        .filter(move |entry| match entry.host_patterns() {
            HostPatterns::Patterns(patterns) => {
                !patterns
                    .iter()
                    .any(|p| p.strip_prefix('!').is_some_and(|p| glob(p, &name)))
                    && patterns
                        .iter()
                        .any(|p| !p.starts_with('!') && glob(p, &name))
            }
            HostPatterns::HashedName { salt, hash } => Hmac::<sha1::Sha1>::new_from_slice(salt)
                .map(|mac| mac.chain_update(name.as_bytes()).verify_slice(hash).is_ok())
                .unwrap_or(false),
        })
}

fn check_at(path: &Path, host: &str, port: u16, key: &PublicKey) -> HostKeyVerdict {
    let unknown = || HostKeyVerdict::Unknown(key.fingerprint(ssh_key::HashAlg::Sha256).to_string());
    let input = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return unknown(),
        Err(_) => return HostKeyVerdict::Mismatch,
    };
    let (mut seen, mut matched) = (false, false);
    for entry in applicable(&input, host, port) {
        // A known key of another algorithm is not a mismatch here (the handshake only
        // offers known algorithms, see `known_key_types`).
        if entry.public_key().algorithm() != key.algorithm() {
            continue;
        }
        let same = entry.public_key().key_data() == key.key_data();
        match entry.marker() {
            Some(Marker::Revoked) if same => return HostKeyVerdict::Mismatch,
            // @cert-authority keys sign host certificates; they are not this host's key.
            Some(_) => {}
            None => {
                seen = true;
                matched |= same;
            }
        }
    }
    if matched {
        HostKeyVerdict::Known
    } else if seen {
        HostKeyVerdict::Mismatch
    } else {
        unknown()
    }
}

pub fn add_known_host(host: &str, port: u16, key: &PublicKey) -> Result<()> {
    add_at(&location()?, host, port, key)
}

fn add_at(path: &Path, host: &str, port: u16, key: &PublicKey) -> Result<()> {
    static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let _guard = LOCK.lock();
    anyhow::ensure!(
        !host.is_empty()
            && !host
                .chars()
                .any(|c| c.is_whitespace() || ",*?!|".contains(c)),
        "invalid host"
    );
    match check_at(path, host, port, key) {
        HostKeyVerdict::Known => return Ok(()),
        HostKeyVerdict::Mismatch => {
            anyhow::bail!("known_hosts mismatch; refusing to replace trusted key")
        }
        HostKeyVerdict::Unknown(_) => {}
    }
    fs::create_dir_all(path.parent().context("missing trust directory")?)?;
    let mut options = fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let name = if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    };
    // Leading newline also handles an existing file without a final newline.
    writeln!(file, "\n{name} {}", key.to_openssh()?)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(byte: u8) -> PublicKey {
        PublicKey::new(ssh_key::public::Ed25519PublicKey([byte; 32]).into(), "")
    }
    #[test]
    fn trust_roundtrip_mismatch_port_and_revocation() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(".ssh/known_hosts");
        let host = "test-remote";
        assert!(matches!(
            check_at(&file, host, 22, &key(1)),
            HostKeyVerdict::Unknown(_)
        ));
        add_at(&file, host, 22, &key(1)).unwrap();
        assert_eq!(check_at(&file, host, 22, &key(1)), HostKeyVerdict::Known);
        assert_eq!(check_at(&file, host, 22, &key(2)), HostKeyVerdict::Mismatch);
        assert!(add_at(&file, host, 22, &key(2)).is_err());
        assert!(matches!(
            check_at(&file, host, 2222, &key(1)),
            HostKeyVerdict::Unknown(_)
        ));
        fs::write(
            &file,
            format!(
                "{host} {}\n@revoked {host} {}\n",
                key(1).to_openssh().unwrap(),
                key(1).to_openssh().unwrap()
            ),
        )
        .unwrap();
        assert_eq!(check_at(&file, host, 22, &key(1)), HostKeyVerdict::Mismatch);
        fs::write(
            &file,
            format!(
                "malformed
{host} {}
",
                key(1).to_openssh().unwrap()
            ),
        )
        .unwrap();
        assert_eq!(check_at(&file, host, 22, &key(1)), HostKeyVerdict::Known);
        let rsa = PublicKey::new(
            ssh_key::public::RsaPublicKey {
                e: ssh_key::Mpint::from_positive_bytes(&[1, 0, 1]).unwrap(),
                n: ssh_key::Mpint::from_positive_bytes(&[0xc5; 256]).unwrap(),
            }
            .into(),
            "",
        );
        fs::write(
            &file,
            format!(
                "{host} {}
",
                rsa.to_openssh().unwrap()
            ),
        )
        .unwrap();
        assert!(matches!(
            check_at(&file, host, 22, &key(1)),
            HostKeyVerdict::Unknown(_)
        ));
        add_at(&file, host, 22, &key(1)).unwrap();
        assert_eq!(check_at(&file, host, 22, &key(1)), HostKeyVerdict::Known);
        assert_eq!(check_at(&file, host, 22, &rsa), HostKeyVerdict::Known);
    }
    /// The public API against a temp HOME (`dirs` reads `$HOME` on Unix only; Windows
    /// covers the same code through `check_at`/`add_at` above).
    /// m24: @cert-authority / @revoked lines for other keys are no mismatch.
    #[test]
    fn marker_lines_do_not_count_as_known_keys() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("known_hosts");
        let host = "marked-host";
        fs::write(
            &file,
            format!(
                "@cert-authority {host} {}\n@revoked {host} {}\n",
                key(2).to_openssh().unwrap(),
                key(3).to_openssh().unwrap()
            ),
        )
        .unwrap();
        assert!(matches!(
            check_at(&file, host, 22, &key(1)),
            HostKeyVerdict::Unknown(_)
        ));
        assert_eq!(check_at(&file, host, 22, &key(3)), HostKeyVerdict::Mismatch);
        add_at(&file, host, 22, &key(1)).unwrap();
        assert_eq!(check_at(&file, host, 22, &key(1)), HostKeyVerdict::Known);
    }
    /// M12: the key types a connection may negotiate come from plain lines only.
    #[test]
    fn known_key_types_ignore_markers_and_other_hosts() {
        let _guard = TEST_LOCK.lock();
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("known_hosts");
        fs::write(
            &file,
            format!(
                "[typed]:2222 {}\n@cert-authority [typed]:2222 {}\nother {}\n",
                key(1).to_openssh().unwrap(),
                key(2).to_openssh().unwrap(),
                key(3).to_openssh().unwrap()
            ),
        )
        .unwrap();
        *TEST_FILE.lock() = Some(file);
        let types = known_key_types("typed", 2222);
        let none = known_key_types("typed", 22);
        *TEST_FILE.lock() = None;
        assert_eq!(types, ["ssh-ed25519"]);
        assert!(none.is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn tofu_with_temp_home() {
        let _guard = TEST_LOCK.lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        assert!(matches!(
            known_hosts_check("tofu-host", 2222, &key(7)),
            HostKeyVerdict::Unknown(_)
        ));
        add_known_host("tofu-host", 2222, &key(7)).unwrap();
        assert_eq!(
            known_hosts_check("tofu-host", 2222, &key(7)),
            HostKeyVerdict::Known
        );
        assert_eq!(
            known_hosts_check("tofu-host", 2222, &key(8)),
            HostKeyVerdict::Mismatch
        );
        let text = fs::read_to_string(home.path().join(".ssh/known_hosts")).unwrap();
        assert!(text.contains("[tofu-host]:2222 ssh-ed25519 "));
    }
}
