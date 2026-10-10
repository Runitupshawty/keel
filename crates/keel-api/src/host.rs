//! Opening what the API runs against: the profile's library (and keel-net when it is on),
//! for `keel-daemon` and for the CLI when no daemon runs.

use crate::config::HostConfig;
use crate::Ctx;
use anyhow::Context;
use std::sync::Arc;
use std::time::Duration;

/// How long closing waits for jobs to reach a checkpoint.
pub const CLOSE_WAIT: Duration = Duration::from_secs(10);

pub struct Host {
    pub ctx: Arc<Ctx>,
    /// Runs the node (when keel-net is on).
    rt: Option<tokio::runtime::Runtime>,
}

/// keel-net settings for [`open`]: where its identity lives and how it connects.
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

impl Host {
    /// Opens `cfg.library` under `cfg.data_dir` (creating it on first use). `net` is used
    /// when `cfg.net` is on. `resume`: restart the jobs an earlier session left (a
    /// long-lived host does; a one-shot CLI call does not).
    pub fn open(cfg: &HostConfig, net: Option<NetSetup>, resume: bool) -> anyhow::Result<Host> {
        let lib = keel_core::Library::open(&cfg.data_dir, &cfg.library).with_context(|| {
            format!(
                "opening library {} (a running Keel window holds it; close it or use keel-daemon)",
                cfg.library
            )
        })?;
        let router = Arc::new(keel_vfs::Router::new());
        lib.set_router(router.clone());
        let lib = Arc::new(lib);
        let mut ctx = Ctx::new(lib.clone(), router);
        let mut rt = None;
        if let (true, Some(net)) = (cfg.net, net) {
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
            ctx = ctx.with_net(node, runtime.handle().clone());
            rt = Some(runtime);
        }
        if resume {
            lib.jobs().resume_all()?;
        }
        Ok(Host {
            ctx: Arc::new(ctx),
            rt,
        })
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
