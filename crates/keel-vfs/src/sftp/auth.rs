use super::{conn::Client, RemoteAuth, RemoteHost};
use anyhow::{Context, Result};
use russh::{
    client::Handle,
    keys::{agent::client::AgentClient, PrivateKeyWithHashAlg},
};
use std::{path::PathBuf, sync::Arc};

/// Keyring service shared with settings. Account is `<host id>:password` or `:passphrase`.
pub const KEYRING_SERVICE: &str = "Keel SFTP";

/// Stores a password (`kind` "password") or key passphrase ("passphrase") for a host in the
/// OS keychain. Blocks on the keychain: call off the UI thread.
pub fn store_secret(host_id: &str, kind: &str, secret: &str) -> Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("{host_id}:{kind}"))?
        .set_password(secret)
        .context("could not write to the OS keychain")
}

/// Removes a host's stored password and passphrase, if any. Blocks: worker threads only.
pub fn forget_secrets(host_id: &str) {
    for kind in ["password", "passphrase"] {
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &format!("{host_id}:{kind}")) {
            let _ = entry.delete_credential();
        }
    }
}

/// `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`, whichever exist, in that order.
pub fn default_key_files() -> Vec<PathBuf> {
    let Some(ssh) = dirs::home_dir().map(|h| h.join(".ssh")) else {
        return Vec::new();
    };
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .into_iter()
        .map(|name| ssh.join(name))
        .filter(|p| p.is_file())
        .collect()
}

async fn secret(host: &RemoteHost, kind: &'static str) -> Result<String> {
    let account = format!("{}:{kind}", host.id);
    tokio::task::spawn_blocking(move || {
        keyring::Entry::new(KEYRING_SERVICE, &account)?.get_password()
    })
    .await
    .context("keychain worker failed")?
    .map_err(|_| anyhow::anyhow!("required credential unavailable in OS keychain"))
}

pub(super) async fn authenticate(session: &mut Handle<Client>, host: &RemoteHost) -> Result<()> {
    let success = match &host.auth {
        RemoteAuth::PasswordInKeyring => session
            .authenticate_password(&host.user, secret(host, "password").await?)
            .await?
            .success(),
        RemoteAuth::KeyFile {
            path,
            passphrase_in_keyring,
        } => {
            let passphrase = if *passphrase_in_keyring {
                Some(secret(host, "passphrase").await?)
            } else {
                None
            };
            // An empty path means "the default keys", tried in order like `ssh` does.
            let paths = if path.as_os_str().is_empty() {
                default_key_files()
            } else {
                vec![path.clone()]
            };
            anyhow::ensure!(
                !paths.is_empty(),
                "no SSH key found in ~/.ssh (id_ed25519, id_ecdsa, id_rsa)"
            );
            let hash = session.best_supported_rsa_hash().await?.flatten();
            let (mut loaded, mut success) = (0, false);
            for path in paths {
                let passphrase = passphrase.clone();
                let Ok(key) = tokio::task::spawn_blocking(move || {
                    russh::keys::load_secret_key(path, passphrase.as_deref())
                })
                .await
                .context("key worker failed")?
                else {
                    continue;
                };
                loaded += 1;
                let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
                if session
                    .authenticate_publickey(&host.user, key)
                    .await?
                    .success()
                {
                    success = true;
                    break;
                }
            }
            anyhow::ensure!(
                loaded > 0,
                "could not load the SSH private key (wrong passphrase or unsupported format)"
            );
            success
        }
        RemoteAuth::Agent => {
            #[cfg(windows)]
            let mut agent = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
                .await
                .context(
                    "SSH agent not reachable (start the OpenSSH Authentication Agent service, \
                     or use a key file instead)",
                )?;
            #[cfg(unix)]
            let mut agent = AgentClient::connect_env()
                .await
                .context("SSH agent not reachable (SSH_AUTH_SOCK unset or stale)")?;
            let hash = session.best_supported_rsa_hash().await?.flatten();
            let keys = agent.request_identities().await?;
            anyhow::ensure!(!keys.is_empty(), "SSH agent has no keys loaded");
            let mut success = false;
            for key in keys {
                if session
                    .authenticate_publickey_with(&host.user, key, hash, &mut agent)
                    .await?
                    .success()
                {
                    success = true;
                    break;
                }
            }
            success
        }
    };
    anyhow::ensure!(success, "SSH authentication rejected");
    Ok(())
}
