use super::{conn::Client, Endpoint, RemoteAuth};
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

async fn secret(host_id: &str, kind: &'static str) -> Result<String> {
    let account = format!("{host_id}:{kind}");
    tokio::task::spawn_blocking(move || {
        keyring::Entry::new(KEYRING_SERVICE, &account)?.get_password()
    })
    .await
    .context("keychain worker failed")?
    .map_err(|_| anyhow::anyhow!("required credential unavailable in OS keychain"))
}

/// Signs in to one hop. `host_id` names the remote's keychain entries.
pub(super) async fn authenticate(
    session: &mut Handle<Client>,
    host_id: &str,
    host: &Endpoint,
) -> Result<()> {
    let success = match &host.auth {
        RemoteAuth::PasswordInKeyring => session
            .authenticate_password(&host.user, secret(host_id, "password").await?)
            .await?
            .success(),
        RemoteAuth::KeyFile {
            path,
            passphrase_in_keyring,
        } => {
            // A password remote's jump host: the agent's keys first, then key files, like ssh.
            let agent = match host.agent_first {
                true => with_agent(session, host).await,
                false => Ok(false),
            };
            match agent {
                Ok(true) => true,
                agent => with_key_files(session, host_id, host, path, *passphrase_in_keyring)
                    .await
                    .map_err(|e| match agent {
                        Err(a) => anyhow::anyhow!("{e:#} (SSH agent: {a:#})"),
                        Ok(_) => e,
                    })?,
            }
        }
        RemoteAuth::Agent => with_agent(session, host).await?,
    };
    anyhow::ensure!(success, "SSH authentication rejected");
    Ok(())
}

/// Offers `path`, else (empty) the config's IdentityFile entries that exist, else the
/// default keys, in order like `ssh` does. Whether one was accepted.
async fn with_key_files(
    session: &mut Handle<Client>,
    host_id: &str,
    host: &Endpoint,
    path: &std::path::Path,
    passphrase_in_keyring: bool,
) -> Result<bool> {
    let passphrase = if passphrase_in_keyring {
        Some(secret(host_id, "passphrase").await?)
    } else {
        None
    };
    let configured: Vec<PathBuf> = host
        .identity_files
        .iter()
        .filter(|p| p.is_file())
        .cloned()
        .collect();
    let paths = if !path.as_os_str().is_empty() {
        vec![path.to_owned()]
    } else if !configured.is_empty() {
        configured
    } else {
        default_key_files()
    };
    anyhow::ensure!(
        !paths.is_empty(),
        "no SSH key found (IdentityFile, or id_ed25519, id_ecdsa, id_rsa in ~/.ssh)"
    );
    let hash = session.best_supported_rsa_hash().await?.flatten();
    let mut loaded = 0;
    for path in paths {
        let passphrase = passphrase.clone();
        let Ok(key) = tokio::task::spawn_blocking(move || -> Result<_> {
            // Bounded: a key file is a few KiB, never a pipe or a huge file.
            let text = super::ssh_config::read_small(&path)?.context("no such key file")?;
            Ok(russh::keys::decode_secret_key(
                &text,
                passphrase.as_deref(),
            )?)
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
            return Ok(true);
        }
    }
    anyhow::ensure!(
        loaded > 0,
        "could not load the SSH private key (wrong passphrase or unsupported format)"
    );
    Ok(false)
}

/// Offers the SSH agent's keys (with IdentitiesOnly, only those of an IdentityFile). Whether
/// one was accepted.
async fn with_agent(session: &mut Handle<Client>, host: &Endpoint) -> Result<bool> {
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
    let mut keys = agent.request_identities().await?;
    anyhow::ensure!(!keys.is_empty(), "SSH agent has no keys loaded");
    // IdentitiesOnly: only the agent keys whose IdentityFile has a `.pub` beside it.
    if host.identities_only && !host.identity_files.is_empty() {
        let allowed: Vec<_> = host
            .identity_files
            .iter()
            .filter_map(|p| {
                let mut pub_file = p.clone().into_os_string();
                pub_file.push(".pub");
                let text = super::ssh_config::read_small(pub_file.as_ref()).ok()??;
                // `type base64 comment`, or the base64 alone.
                let base64 = text.split_whitespace().take(2).last()?;
                russh::keys::parse_public_key_base64(base64).ok()
            })
            .collect();
        keys.retain(|k| {
            allowed
                .iter()
                .any(|a| a.key_data() == k.public_key().key_data())
        });
        anyhow::ensure!(
            !keys.is_empty(),
            "IdentitiesOnly: no SSH agent key matches an IdentityFile (its .pub file)"
        );
    }
    for key in keys {
        let key = key.public_key().into_owned();
        if session
            .authenticate_publickey_with(&host.user, key, hash, &mut agent)
            .await?
            .success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}
