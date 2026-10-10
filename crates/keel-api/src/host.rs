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
    /// `KEEL_NET_SECRET=memory` keeps the identity in memory instead (tests, trials).
    pub fn system() -> Self {
        let secrets: Arc<dyn keel_vfs::cloud::SecretStore> =
            if std::env::var("KEEL_NET_SECRET").as_deref() == Ok("memory") {
                Arc::new(keel_vfs::cloud::MemoryStore::default())
            } else {
                Arc::new(keel_vfs::cloud::KeyringStore)
            };
        Self {
            secrets,
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
        // The library holds the index, plans and keel-net's state: owner-only when created
        // here (else it inherits the drive's permissions).
        crate::private::create_dir_all(&cfg.data_dir)
            .with_context(|| format!("creating {}", cfg.data_dir.display()))?;
        let lib = keel_core::Library::open(&cfg.data_dir, &cfg.library).with_context(|| {
            format!(
                "opening library {} (a running Keel window holds it; close it or use keel-daemon)",
                cfg.library
            )
        })?;
        let router = Arc::new(router(cfg));
        lib.set_router(router.clone());
        lib.set_remote_poll(std::time::Duration::from_secs(cfg.remote_poll_secs));
        let lib = Arc::new(lib);
        let mut ctx = Ctx::new(lib.clone(), router);
        ctx.config_dir = Some(cfg.config_dir.clone());
        ctx.data_dir = Some(cfg.data_dir.clone());
        let mut host = Host {
            ctx: Arc::new(ctx),
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
    /// need it; nothing else does). The node serves the library's sources to the devices
    /// granted them (keel-net's `LibraryHandler`, as the window does), takes Spacedrops
    /// through [`crate::net::Drops`], and `node://<device>/` paths browse paired devices.
    pub fn open_net(&mut self, cfg: &HostConfig, net: NetSetup) -> anyhow::Result<()> {
        if !cfg.net || self.ctx.node.is_some() {
            return Ok(());
        }
        let ctx = Arc::get_mut(&mut self.ctx).context("keel-net: the host is in use")?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("keel-net")
            .build()?;
        let drops = Arc::new(
            crate::net::Drops::new(cfg.inbox_dir(), cfg.auto_accept.clone()).with_relay(cfg.relay),
        );
        let handler = Arc::new(keel_net::LibraryHandler::new(ctx.lib.clone()));
        // `devices.settings_set` moves the inbox while the node runs.
        let bind = {
            let (handler, offers) = (handler.clone(), Arc::downgrade(&drops));
            move |inbox: std::path::PathBuf| {
                let offers = offers.clone();
                handler.on_drop(inbox, move |offer| {
                    if let Some(drops) = offers.upgrade() {
                        drops.offer(offer);
                    }
                });
            }
        };
        bind(cfg.inbox_dir());
        drops.on_rebind(bind);
        // `[devices] relay = false` turns the public relays off (never on: the caller's
        // options may have none).
        let options = match cfg.relay {
            true => net.options,
            false => net.options.with_relay(false),
        };
        let node = runtime
            .block_on(keel_net::Node::open_with_options(
                net.secrets,
                &cfg.data_dir,
                handler,
                options,
            ))
            .context("opening keel-net")?;
        ctx.router.register(Arc::new(keel_net::NodeProvider::new(
            node.clone(),
            runtime.handle().clone(),
        )));
        // Spacedrop jobs an earlier session left resume with the library's jobs.
        keel_net::spacedrop::register(&ctx.lib);
        ctx.node = Some(node);
        ctx.drops = Some(drops);
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

    /// Serves `mounts.*` (keel-daemon): mounts live until [`Host::close`]. Remote writes
    /// spool in `<data dir>/mount-spool`.
    pub fn with_mounts(mut self, data_dir: &std::path::Path) -> Self {
        let mounts = Arc::new(keel_mount::Mounts::new(data_dir.join("mount-spool")));
        if let Some(ctx) = Arc::get_mut(&mut self.ctx) {
            ctx.mounts = Some(mounts);
        }
        self
    }

    /// Unmounts, then closes the node and the library (jobs stop at their next checkpoint
    /// and resume on the next open). False when a job was still busy after `CLOSE_WAIT`.
    pub fn close(&self) -> bool {
        if let Some(m) = &self.ctx.mounts {
            m.unmount_all();
        }
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

    #[test]
    fn the_data_folder_is_created_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("new").join("data");
        let cfg = HostConfig::read("work", dir.path().join("config"), data.clone());
        let host = Host::open(&cfg, None, false).unwrap();
        assert!(crate::private::is_private_dir(&data).unwrap());
        assert_eq!(
            host.ctx.config_dir.as_deref(),
            Some(cfg.config_dir.as_path())
        );
        assert!(host.close());
    }
}
