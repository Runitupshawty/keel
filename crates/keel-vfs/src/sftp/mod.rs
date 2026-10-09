//! SFTP remotes: host config, connection pool, authentication and known_hosts trust.
pub mod auth;
pub mod conn;
pub mod hostkeys;
pub use conn::ConnPool;
use crossbeam_channel::Sender;
pub use hostkeys::{add_known_host, known_hosts_check, HostKeyVerdict};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RemoteHost {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: RemoteAuth,
    pub home: Option<String>,
    pub bookmarks: Vec<(String, String)>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RemoteAuth {
    Agent,
    KeyFile {
        path: PathBuf,
        passphrase_in_keyring: bool,
    },
    PasswordInKeyring,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConnStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed,
}
#[derive(Clone, Debug)]
pub enum RemoteEvent {
    Status {
        host_id: String,
        status: ConnStatus,
        detail: String,
    },
    HostKeyPrompt {
        host_id: String,
        fingerprint: String,
        reply: Sender<bool>,
    },
}
