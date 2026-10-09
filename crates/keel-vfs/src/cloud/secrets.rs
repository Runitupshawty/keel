//! Where cloud tokens and keys live: the OS keychain in the app, memory in tests. Config
//! files only ever hold non-secret account metadata (`CloudAccount`).
use anyhow::{Context, Result};
use std::collections::HashMap;

/// Keychain service for every cloud secret. Accounts are `<cloud id>/<field>`.
pub const KEYRING_SERVICE: &str = "Keel Cloud";

/// Secret fields stored per account (`<cloud id>/<field>`). OAuth tokens are one JSON
/// entry, `tokens`; the three token fields here are what earlier builds wrote.
pub const FIELDS: [&str; 6] = [
    "access_token",
    "refresh_token",
    "expires_at",
    "client_secret",
    "access_key_id",
    "secret_access_key",
];

/// Keys are `<cloud id>/<field>`. Implementations block (keychain IPC): worker threads only.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    /// Removing a missing key is not an error.
    fn delete(&self, key: &str) -> Result<()>;
}

/// Removes every secret of an account (sign-out / account removal). Best effort per field.
pub fn forget_account(store: &dyn SecretStore, id: &str) {
    for field in FIELDS.into_iter().chain(["tokens"]) {
        let _ = store.delete(&format!("{id}/{field}"));
    }
}

/// The OS keychain (Credential Manager / Keychain / Secret Service).
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyringStore;
impl SecretStore for KeyringStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        match keyring::Entry::new(KEYRING_SERVICE, key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => anyhow::bail!("could not read from the OS keychain"),
        }
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        keyring::Entry::new(KEYRING_SERVICE, key)?
            .set_password(value)
            .context("could not write to the OS keychain")
    }
    fn delete(&self, key: &str) -> Result<()> {
        match keyring::Entry::new(KEYRING_SERVICE, key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => anyhow::bail!("could not delete from the OS keychain"),
        }
    }
}

/// In-memory store for tests and for callers that hold secrets only for one session.
#[derive(Default)]
pub struct MemoryStore(parking_lot::Mutex<HashMap<String, String>>);
impl MemoryStore {
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.0.lock().keys().cloned().collect();
        keys.sort();
        keys
    }
}
impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.0.lock().get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.0.lock().insert(key.into(), value.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().remove(key);
        Ok(())
    }
}
