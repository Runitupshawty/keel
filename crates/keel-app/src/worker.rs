//! Background work. Every worker sends a `Msg` and then calls `ctx.request_repaint()`;
//! nothing here runs on the UI thread.

use crate::preview_panel::PreviewKey;
use crate::state::Msg;
use crossbeam_channel::{Sender, TrySendError};
use keel_vfs::{Entry, Router, VPath};
use std::sync::Arc;

fn send(tx: &Sender<Msg>, ctx: &egui::Context, msg: Msg) {
    let _ = tx.send(msg);
    ctx.request_repaint();
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        tracing::error!("spawn {name}: {e}");
    }
}

pub fn spawn_list(
    router: Arc<Router>,
    dir: VPath,
    pane: usize,
    tab: usize,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    spawn("keel-list", move || {
        let result = match router.provider_for(&dir) {
            Some(p) => p.list(&dir),
            None => Err(anyhow::anyhow!("no provider for {}", dir.display())),
        };
        send(
            &tx,
            &ctx,
            Msg::Listed {
                pane,
                tab,
                dir,
                result,
            },
        );
    });
}

pub fn spawn_drives(tx: Sender<Msg>, ctx: egui::Context) {
    spawn("keel-drives", move || {
        let drives = keel_vfs::drives();
        send(&tx, &ctx, Msg::Drives(drives));
    });
}

/// Materialises `path` (may be remote) and runs `f` on the local copy; errors become toasts.
pub fn spawn_local(
    router: Arc<Router>,
    path: VPath,
    tx: Sender<Msg>,
    ctx: egui::Context,
    f: impl FnOnce(&std::path::Path) -> std::io::Result<()> + Send + 'static,
) {
    spawn("keel-launch", move || {
        let result = router
            .provider_for(&path)
            .ok_or_else(|| anyhow::anyhow!("no provider for {}", path.display()))
            .and_then(|p| p.local_copy(&path))
            .and_then(|local| Ok(f(&local)?));
        if let Err(e) = result {
            send(&tx, &ctx, Msg::Toast(format!("{}: {e:#}", path.display())));
        }
    });
}

/// Four thumbnail threads behind a small bounded queue. When the queue is full the
/// request is refused and the grid asks again next frame, so fast scrolling never
/// piles up stale work.
pub struct ThumbPool {
    jobs: Sender<(PreviewKey, Entry)>,
}

pub const THUMB_PX: u32 = 96;

impl ThumbPool {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context, router: Arc<Router>) -> Self {
        let (jobs, rx) = crossbeam_channel::bounded::<(PreviewKey, Entry)>(16);
        for _ in 0..4 {
            let (rx, tx, ctx, router) = (rx.clone(), tx.clone(), ctx.clone(), router.clone());
            spawn("keel-thumb", move || {
                while let Ok((key, entry)) = rx.recv() {
                    let preview = match router
                        .provider_for(&entry.path)
                        .ok_or_else(|| anyhow::anyhow!("no provider"))
                        .and_then(|p| p.local_copy(&entry.path))
                    {
                        Ok(bytes_path) => keel_preview::preview(&keel_preview::Request {
                            entry,
                            bytes_path,
                            page: 0,
                            max_px: THUMB_PX,
                        }),
                        Err(e) => keel_preview::Preview::Error(e.to_string()),
                    };
                    send(&tx, &ctx, Msg::Thumb { key, preview });
                }
            });
        }
        Self { jobs }
    }

    /// False when the queue is full; ask again later.
    pub fn request(&self, key: PreviewKey, entry: Entry) -> bool {
        !matches!(self.jobs.try_send((key, entry)), Err(TrySendError::Full(_)))
    }
}
