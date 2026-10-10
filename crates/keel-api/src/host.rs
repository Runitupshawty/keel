//! Opening what the API runs against: the profile's library, its SFTP hosts and cloud
//! accounts (and keel-net when it is on), for `keel-daemon` and for the CLI when no
//! daemon runs.

use crate::config::HostConfig;
use crate::Ctx;
use anyhow::Context;
use std::sync::Arc;
use std::time::Duration;

/// How long closing waits for jobs to reach a checkpoint.
pub const CLOSE_WAIT: Duration = Duration::from_secs(10);

/// Room for files extracted from archives (`<data dir>/archives`).
const ARCHIVE_CACHE_BYTES: u64 = 2 << 30;

pub struct Host {
    pub ctx: Arc<Ctx>,
    /// Runs the node (when keel-net is on).
    rt: Option<tokio::runtime::Runtime>,
}

/// keel-net settings for [`Host::open`]: where its identity lives and how it connects.
pub struct NetSetup {
    pub secrets: Arc<dyn keel_vfs::cloud::SecretStore>,
    pub options: keel_net::NodeOptions,
}

impl NetSetup {
    /// The OS keychain and public discovery/relays.
    pub fn system() -> Self {
        Self {
            secrets: Arc::new(keel_vfs::cloud::KeyringStore),
            options: keel_net::NodeOptions::default(),
        }
    }
}

/// A router for `cfg`: archives extracted under the data folder (never the app's own
/// cache), the profile's SFTP hosts (`[[remotes]]`; a host key not trusted yet is refused,
/// with no one to ask) and cloud accounts (`[[clouds]]`, credentials from the OS keychain).
fn router(cfg: &HostConfig) -> keel_vfs::Router {
    let cache = keel_vfs::archive::cache::MaterialiseCache::new(
        cfg.data_dir.join("archives"),
        ARCHIVE_CACHE_BYTES,
    );
    let router = keel_vfs::Router::with_archive_cache(Arc::new(cache));
    for host in &cfg.remotes {
        router.register_remote(host.clone());
    }
    if !cfg.clouds.is_empty() {
        let secrets: Arc<dyn keel_vfs::cloud::SecretStore> =
            Arc::new(keel_vfs::cloud::KeyringStore);
        for account in &cfg.clouds {
            if let Err(e) = router.register_cloud(account, secrets.clone()) {
                tracing::warn!("cloud account {}: {e:#}", account.id);
            }
        }
    }
    router
}

impl Host {
    /// Opens `cfg.library` under `cfg.data_dir` (creating it on first use). keel-net is
    /// opened with `net` when given and `cfg.net` is on (else later with `open_net`).
    /// `resume`: restart the jobs an earlier session left (a long-lived host does; a
    /// one-shot CLI call does not).
    pub fn open(cfg: &HostConfig, net: Option<NetSetup>, resume: bool) -> anyhow::Result<Host> {
        let lib = keel_core::Library::open(&cfg.data_dir, &cfg.library).with_context(|| {
            format!(
                "opening library {} (a running Keel window holds it; close it or use keel-daemon)",
                cfg.library
            )
        })?;
        let router = Arc::new(router(cfg));
        lib.set_router(router.clone());
        let lib = Arc::new(lib);
        let mut host = Host {
            ctx: Arc::new(Ctx::new(lib.clone(), router)),
            rt: None,
        };
        if let Some(net) = net {
            host.open_net(cfg, net)?;
        }
        if resume {
            lib.jobs().resume_all()?;
        }
        Ok(host)
    }

    /// Brings keel-net online when `cfg.net` is on and it is not yet (devices and shares
    /// need it; nothing else does).
    pub fn open_net(&mut self, cfg: &HostConfig, net: NetSetup) -> anyhow::Result<()> {
        if !cfg.net || self.ctx.node.is_some() {
            return Ok(());
        }
        let ctx = Arc::get_mut(&mut self.ctx).context("keel-net: the host is in use")?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("keel-net")
            .build()?;
        let node = runtime
            .block_on(keel_net::Node::open_with_options(
                net.secrets,
                &cfg.data_dir,
                Arc::new(crate::net::NoSources),
                net.options,
            ))
            .context("opening keel-net")?;
        ctx.node = Some(node);
        ctx.rt = Some(runtime.handle().clone());
        self.rt = Some(runtime);
        Ok(())
    }

    pub fn with_utc_offset(mut self, secs: i64) -> Self {
        if let Some(ctx) = Arc::get_mut(&mut self.ctx) {
            ctx.utc_offset = secs;
        }
        self.ctx.lib.set_utc_offset(secs);
        self
    }

    /// Closes the node and the library (jobs stop at their next checkpoint and resume on
    /// the next open). False when a job was still busy after `CLOSE_WAIT`.
    pub fn close(&self) -> bool {
        if let (Some(node), Some(rt)) = (&self.ctx.node, &self.rt) {
            rt.block_on(node.close());
        }
        self.ctx.lib.close(CLOSE_WAIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_router_has_the_profiles_remotes_and_keeps_archives_in_the_data_folder() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles/work");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(
            profile.join("config.toml"),
            "[[remotes]]\nid = \"nas\"\nlabel = \"NAS\"\nhost = \"example.invalid\"\nport = 22\n\
             user = \"me\"\nauth = \"Agent\"\nbookmarks = []\n",
        )
        .unwrap();
        let cfg = HostConfig::read("work", dir.path().into(), dir.path().join("data"));
        assert_eq!(cfg.remotes.len(), 1, "{cfg:?}");
        let router = router(&cfg);
        let nas = keel_vfs::VPath::parse("sftp://nas/home").unwrap();
        assert!(router.provider_for(&nas).is_some());
        let other = keel_vfs::VPath::parse("sftp://other/home").unwrap();
        assert!(router.provider_for(&other).is_none());
    }
}
